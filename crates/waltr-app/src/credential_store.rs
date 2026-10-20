//! Native-only opaque refresh-token persistence. Never expose this module through IPC.

use std::{
    collections::HashMap,
    fmt,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use waltr_client::{NativeRefreshTokenStore, NativeRefreshTokenStoreError};
use sha2::{Digest as _, Sha256};
use url::Url;
use zeroize::Zeroizing;

use NativeRefreshTokenStoreError::{Malformed, Unavailable};

const MAX_TOKEN_BYTES: usize = 32 * 1024;
const MAX_FIELD_BYTES: usize = 4096;
const MAX_SCOPES: usize = 128;
const MAX_SCOPE_BYTES: usize = 1024;

/// Current verified discovery and configuration used to bind a credential.
#[derive(Clone, Copy)]
pub struct CredentialIdentity<'a> {
    /// Stable configured server identifier, also used for server-wide deletion.
    pub server_id: &'a str,
    /// Waltr API endpoint; trailing slashes are normalized away.
    pub endpoint: &'a str,
    /// Exact issuer from current verified discovery, not cached metadata.
    pub issuer: &'a str,
    /// Public native OIDC client identifier.
    pub client_id: &'a str,
    /// Effective requested scopes, including `openid` and discovered defaults.
    pub scopes: &'a [String],
    /// Optional provider-specific audience, bound exactly when present.
    pub audience: Option<&'a str>,
}

impl fmt::Debug for CredentialIdentity<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialIdentity([REDACTED])")
    }
}

/// A cloneable identity-bound store. Construction performs no secure-store I/O.
#[derive(Clone)]
pub struct CredentialStore {
    #[cfg_attr(not(any(target_os = "macos", target_os = "android")), allow(dead_code))]
    server: String,
    #[cfg_attr(not(any(target_os = "macos", target_os = "android")), allow(dead_code))]
    account: String,
    fence: Arc<ServerFence>,
    generation: u64,
}

#[derive(Default)]
struct ServerFence {
    generation: AtomicU64,
    io: Mutex<()>,
}

type FenceRegistry = Mutex<HashMap<String, Weak<ServerFence>>>;
static FENCES: OnceLock<FenceRegistry> = OnceLock::new();

fn server_fence(server: &str) -> Result<Arc<ServerFence>, NativeRefreshTokenStoreError> {
    let mut registry = FENCES
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|_| Unavailable)?;
    registry.retain(|_, fence| fence.strong_count() != 0);
    if let Some(fence) = registry.get(server).and_then(Weak::upgrade) {
        return Ok(fence);
    }
    let fence = Arc::new(ServerFence::default());
    registry.insert(server.to_owned(), Arc::downgrade(&fence));
    Ok(fence)
}

impl fmt::Debug for CredentialStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialStore([REDACTED])")
    }
}

impl CredentialStore {
    /// Bind to current verified discovery, not cached issuer metadata.
    ///
    /// `scopes` must be the effective requested scopes (including `openid` and
    /// discovered defaults). Endpoint trailing slashes are insignificant; issuer
    /// path/trailing slash, client ID, and audience are exact identity components.
    pub fn new(
        identity: CredentialIdentity<'_>,
    ) -> Result<Arc<Self>, NativeRefreshTokenStoreError> {
        let CredentialIdentity {
            server_id,
            endpoint,
            issuer,
            client_id,
            scopes,
            audience,
        } = identity;
        let server = server_namespace(server_id)?;
        let endpoint = normalized_url(endpoint, true)?;
        let issuer = normalized_url(issuer, false)?;
        check_field(client_id)?;
        if scopes.is_empty() || scopes.len() > MAX_SCOPES {
            return Err(Malformed);
        }
        let mut scopes: Vec<&str> = scopes.iter().map(String::as_str).collect();
        if scopes.iter().any(|scope| {
            scope.is_empty()
                || scope.len() > MAX_SCOPE_BYTES
                || !scope
                    .bytes()
                    .all(|byte| matches!(byte, 0x21 | 0x23..=0x5b | 0x5d..=0x7e))
        }) {
            return Err(Malformed);
        }
        scopes.sort_unstable();
        scopes.dedup();
        if let Some(audience) = audience {
            check_field(audience)?;
        }
        let mut identity = Sha256::new();
        for field in ["waltr-refresh-v1", &server, &endpoint, &issuer, client_id] {
            hash_field(&mut identity, field.as_bytes());
        }
        hash_field(&mut identity, &(scopes.len() as u64).to_be_bytes());
        for scope in scopes {
            hash_field(&mut identity, scope.as_bytes());
        }
        hash_field(&mut identity, &[u8::from(audience.is_some())]);
        if let Some(audience) = audience {
            hash_field(&mut identity, audience.as_bytes());
        }
        let fence = server_fence(&server)?;
        let generation = fence.generation.load(Ordering::SeqCst);
        Ok(Arc::new(Self {
            server,
            account: hex_digest(identity),
            fence,
            generation,
        }))
    }

    /// Remove every identity for a server, including inactive or obsolete issuers.
    /// No discovery or durable registry metadata is required. Serializes with
    /// in-flight operations and invalidates existing handles, so detached writes
    /// cannot recreate a removed credential. Construct a fresh handle to sign in
    /// again. Still stop the server's auth tasks before removing its configuration.
    pub async fn delete_server(server_id: &str) -> Result<(), NativeRefreshTokenStoreError> {
        let server = server_namespace(server_id)?;
        let fence = server_fence(&server)?;
        blocking(move || {
            let _guard = fence.io.lock().map_err(|_| Unavailable)?;
            fence.generation.fetch_add(1, Ordering::SeqCst);
            platform::delete_server(&server)
        })
        .await
    }

    fn with_current<T>(
        &self,
        operation: impl FnOnce() -> Result<T, NativeRefreshTokenStoreError>,
    ) -> Result<T, NativeRefreshTokenStoreError> {
        let _guard = self.fence.io.lock().map_err(|_| Unavailable)?;
        if self.generation != self.fence.generation.load(Ordering::SeqCst) {
            return Err(Unavailable);
        }
        operation()
    }
}

#[async_trait]
impl NativeRefreshTokenStore for CredentialStore {
    async fn load(&self) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
        let store = self.clone();
        let token = blocking(move || store.with_current(|| platform::load(&store))).await?;
        if let Some(token) = &token {
            check_token(token)?;
        }
        Ok(token)
    }

    async fn store(&self, token: &[u8]) -> Result<(), NativeRefreshTokenStoreError> {
        check_token(token)?;
        let store = self.clone();
        let token = Zeroizing::new(token.to_vec());
        blocking(move || store.with_current(|| platform::store(&store, &token))).await
    }

    async fn delete(&self) -> Result<(), NativeRefreshTokenStoreError> {
        let store = self.clone();
        blocking(move || store.with_current(|| platform::delete(&store))).await
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, NativeRefreshTokenStoreError> + Send + 'static,
) -> Result<T, NativeRefreshTokenStoreError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| Unavailable)?
}

fn check_token(token: &[u8]) -> Result<(), NativeRefreshTokenStoreError> {
    if token.is_empty() || token.len() > MAX_TOKEN_BYTES {
        Err(Malformed)
    } else {
        Ok(())
    }
}

fn check_field(value: &str) -> Result<(), NativeRefreshTokenStoreError> {
    if value.is_empty() || value.len() > MAX_FIELD_BYTES || value.chars().any(char::is_control) {
        Err(Malformed)
    } else {
        Ok(())
    }
}

fn hash_field(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn server_namespace(server_id: &str) -> Result<String, NativeRefreshTokenStoreError> {
    check_field(server_id)?;
    let mut hash = Sha256::new();
    hash_field(&mut hash, b"dev.waltr.app.refresh.server.v1");
    hash_field(&mut hash, server_id.as_bytes());
    Ok(hex_digest(hash))
}

fn hex_digest(hash: Sha256) -> String {
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn normalized_url(value: &str, endpoint: bool) -> Result<String, NativeRefreshTokenStoreError> {
    check_field(value)?;
    let mut url = Url::parse(value).map_err(|_| Malformed)?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Malformed);
    }
    if endpoint {
        let path = url.path().trim_end_matches('/').to_owned();
        url.set_path(if path.is_empty() { "/" } else { &path });
    }
    Ok(if endpoint {
        url.into()
    } else {
        value.to_owned()
    })
}

#[cfg(target_os = "macos")]
mod platform {
    use super::*;
    use security_framework::{
        item::{ItemClass, ItemSearchOptions},
        passwords::{delete_generic_password, get_generic_password, set_generic_password},
    };

    const NOT_FOUND: i32 = -25300;

    fn service(server: &str) -> String {
        format!("dev.waltr.app.refresh.v1.{server}")
    }

    pub(super) fn load(
        store: &CredentialStore,
    ) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
        match get_generic_password(&service(&store.server), &store.account) {
            Ok(token) => Ok(Some(token)),
            Err(error) if error.code() == NOT_FOUND => Ok(None),
            Err(_) => Err(Unavailable),
        }
    }

    pub(super) fn store(
        store: &CredentialStore,
        token: &[u8],
    ) -> Result<(), NativeRefreshTokenStoreError> {
        set_generic_password(&service(&store.server), &store.account, token)
            .map_err(|_| Unavailable)
    }

    pub(super) fn delete(store: &CredentialStore) -> Result<(), NativeRefreshTokenStoreError> {
        deletion(delete_generic_password(
            &service(&store.server),
            &store.account,
        ))
    }

    pub(super) fn delete_server(server: &str) -> Result<(), NativeRefreshTokenStoreError> {
        deletion(
            ItemSearchOptions::new()
                .class(ItemClass::generic_password())
                .service(&service(server))
                .delete(),
        )
    }

    fn deletion(
        result: Result<(), security_framework::base::Error>,
    ) -> Result<(), NativeRefreshTokenStoreError> {
        match result {
            Ok(()) => Ok(()),
            Err(error) if error.code() == NOT_FOUND => Ok(()),
            Err(_) => Err(Unavailable),
        }
    }
}

/// Install the application-context Kotlin store once, from an Activity JNI hook.
/// The object must be returned by `SecureTokenStore.initialize(context)`.
#[cfg(target_os = "android")]
pub fn initialize_android(
    env: &jni::Env<'_>,
    store: &jni::objects::JObject<'_>,
) -> Result<(), NativeRefreshTokenStoreError> {
    platform::initialize(env, store)
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use jni::{
        JavaVM,
        objects::{Global, JByteArray, JObject, JValue},
    };
    use std::sync::OnceLock;

    static STORE: OnceLock<Global<JObject<'static>>> = OnceLock::new();

    pub(super) fn initialize(
        env: &jni::Env<'_>,
        store: &JObject<'_>,
    ) -> Result<(), NativeRefreshTokenStoreError> {
        if store.is_null() {
            return Err(Unavailable);
        }
        if STORE.get().is_none() {
            let global = env.new_global_ref(store).map_err(|_| Unavailable)?;
            // Activity recreation reuses the same application-context singleton.
            drop(STORE.set(global));
        }
        Ok(())
    }

    fn execute(
        operation: i32,
        server: &str,
        account: &str,
        token: &[u8],
    ) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
        let store = STORE.get().ok_or(Unavailable)?;
        let vm = JavaVM::singleton().map_err(|_| Unavailable)?;
        let response: Result<Vec<u8>, jni::errors::Error> = vm.attach_current_thread(|env| {
            let server = env.new_string(server)?;
            let account = env.new_string(account)?;
            let token = env.byte_array_from_slice(token)?;
            let response = env
                .call_method(
                    store,
                    jni::jni_str!("execute"),
                    jni::jni_sig!("(ILjava/lang/String;Ljava/lang/String;[B)[B"),
                    &[
                        JValue::Int(operation),
                        JValue::Object(server.as_ref()),
                        JValue::Object(account.as_ref()),
                        JValue::Object(token.as_ref()),
                    ],
                )?
                .l()?;
            let response = env.cast_local::<JByteArray>(response)?;
            let length = response.len(env)?;
            if !(1..=(MAX_TOKEN_BYTES + 1)).contains(&length) {
                return Ok(vec![3]);
            }
            let bytes = env.convert_byte_array(&response)?;
            let zeros = vec![0_i8; length];
            response.set_region(env, 0, &zeros)?;
            Ok(bytes)
        });
        let response = Zeroizing::new(response.map_err(|_| Unavailable)?);
        match response.as_slice() {
            [0] => Ok(None),
            [1, token @ ..] if operation == 0 => {
                check_token(token)?;
                Ok(Some(token.to_vec()))
            }
            [3] => Err(Malformed),
            _ => Err(Unavailable),
        }
    }

    pub(super) fn load(
        store: &CredentialStore,
    ) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
        execute(0, &store.server, &store.account, &[])
    }

    pub(super) fn store(
        store: &CredentialStore,
        token: &[u8],
    ) -> Result<(), NativeRefreshTokenStoreError> {
        execute(1, &store.server, &store.account, token).map(|_| ())
    }

    pub(super) fn delete(store: &CredentialStore) -> Result<(), NativeRefreshTokenStoreError> {
        execute(2, &store.server, &store.account, &[]).map(|_| ())
    }

    pub(super) fn delete_server(server: &str) -> Result<(), NativeRefreshTokenStoreError> {
        execute(3, server, "", &[]).map(|_| ())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "android")))]
mod platform {
    use super::*;

    pub(super) fn load(
        _: &CredentialStore,
    ) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
        Err(Unavailable)
    }
    pub(super) fn store(_: &CredentialStore, _: &[u8]) -> Result<(), NativeRefreshTokenStoreError> {
        Err(Unavailable)
    }
    pub(super) fn delete(_: &CredentialStore) -> Result<(), NativeRefreshTokenStoreError> {
        Err(Unavailable)
    }
    pub(super) fn delete_server(_: &str) -> Result<(), NativeRefreshTokenStoreError> {
        Err(Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(scopes: &[String]) -> CredentialIdentity<'_> {
        CredentialIdentity {
            server_id: "server",
            endpoint: "https://example.com",
            issuer: "https://id.example/",
            client_id: "client",
            scopes,
            audience: None,
        }
    }

    #[test]
    fn canonical_endpoint_and_scope_set() {
        let a = CredentialStore::new(CredentialIdentity {
            endpoint: "https://EXAMPLE.com:443/api/",
            ..identity(&["b".to_owned(), "a".to_owned()])
        })
        .unwrap();
        let b = CredentialStore::new(CredentialIdentity {
            endpoint: "https://example.com/api",
            ..identity(&["a".to_owned(), "b".to_owned(), "a".to_owned()])
        })
        .unwrap();
        assert_eq!(a.account, b.account);
    }

    #[test]
    fn every_identity_component_is_bound() {
        let scopes = ["openid".to_owned()];
        let input = identity(&scopes);
        let base = CredentialStore::new(input).unwrap();
        for other in [
            CredentialIdentity {
                server_id: "other",
                ..input
            },
            CredentialIdentity {
                endpoint: "https://other.example",
                ..input
            },
            CredentialIdentity {
                issuer: "https://id.example/changed",
                ..input
            },
            CredentialIdentity {
                client_id: "other",
                ..input
            },
            CredentialIdentity {
                scopes: &["openid".to_owned(), "extra".to_owned()],
                ..input
            },
            CredentialIdentity {
                audience: Some("audience"),
                ..input
            },
        ] {
            assert_ne!(base.account, CredentialStore::new(other).unwrap().account);
        }
        assert_eq!(format!("{base:?}"), "CredentialStore([REDACTED])");
        assert_eq!(format!("{input:?}"), "CredentialIdentity([REDACTED])");
    }

    #[test]
    fn server_removal_fences_old_handles_without_discovery() {
        let scopes = ["openid".to_owned()];
        let input = CredentialIdentity {
            server_id: "fence-test",
            ..identity(&scopes)
        };
        let old = CredentialStore::new(input).unwrap();
        let other_identity = CredentialStore::new(CredentialIdentity {
            endpoint: "https://other.example",
            ..input
        })
        .unwrap();
        assert!(Arc::ptr_eq(&old.fence, &other_identity.fence));
        old.fence.generation.fetch_add(1, Ordering::SeqCst);
        assert_eq!(old.with_current(|| Ok(())), Err(Unavailable));
        assert_eq!(other_identity.with_current(|| Ok(())), Err(Unavailable));
        let fresh = CredentialStore::new(input).unwrap();
        assert_eq!(fresh.with_current(|| Ok(())), Ok(()));
        assert_eq!(old.server, server_namespace("fence-test").unwrap());
    }

    #[test]
    fn token_bounds_preserve_opaque_bytes() {
        assert_eq!(check_token(&[]), Err(Malformed));
        assert!(check_token(&[0, 255]).is_ok());
        assert!(check_token(&vec![1; MAX_TOKEN_BYTES]).is_ok());
        assert_eq!(check_token(&vec![1; MAX_TOKEN_BYTES + 1]), Err(Malformed));
    }

    #[test]
    fn invalid_identity_is_rejected() {
        for url in [
            "http://remote.example",
            "https://u:p@example.com",
            "https://example.com/?q=1",
            "https://example.com/#fragment",
        ] {
            assert_eq!(normalized_url(url, true), Err(Malformed));
        }
        assert_eq!(server_namespace(""), Err(Malformed));
        assert_eq!(
            check_field(&"x".repeat(MAX_FIELD_BYTES + 1)),
            Err(Malformed)
        );
        assert!(CredentialStore::new(identity(&["bad scope".to_owned()])).is_err());
    }
}
