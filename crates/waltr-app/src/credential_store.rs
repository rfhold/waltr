//! Platform secure-store adapter for the native refresh token.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
#[cfg(not(any(target_os = "android", target_os = "linux")))]
use waltr_client::NonPersistentNativeRefreshTokenStore;
use waltr_client::{NativeRefreshTokenStore, NativeRefreshTokenStoreError};

#[cfg(any(target_os = "android", target_os = "linux"))]
use keyring_core::{Entry, Error, api::CredentialStoreApi as _};

const SERVICE: &str = "dev.waltr.app.native-access";
const USER: &str = "refresh-token";

pub(crate) fn refresh_token_store() -> Arc<dyn NativeRefreshTokenStore> {
    #[cfg(any(target_os = "android", target_os = "linux"))]
    {
        return Arc::new(PlatformRefreshTokenStore::default());
    }
    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    Arc::new(NonPersistentNativeRefreshTokenStore)
}

#[cfg(any(target_os = "android", target_os = "linux"))]
#[derive(Default)]
struct PlatformRefreshTokenStore {
    entry: Arc<Mutex<Option<Entry>>>,
}

#[cfg(any(target_os = "android", target_os = "linux"))]
impl PlatformRefreshTokenStore {
    fn with_entry<T>(
        entry: &Mutex<Option<Entry>>,
        operation: impl FnOnce(&Entry) -> keyring_core::Result<T>,
    ) -> keyring_core::Result<T> {
        let mut entry = entry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if entry.is_none() {
            *entry = Some(build_entry()?);
        }
        operation(entry.as_ref().expect("secure-store entry initialized"))
    }
}

#[cfg(target_os = "linux")]
fn build_entry() -> keyring_core::Result<Entry> {
    zbus_secret_service_keyring_store::Store::new()?.build(SERVICE, USER, None)
}

#[cfg(target_os = "android")]
fn build_entry() -> keyring_core::Result<Entry> {
    let configuration = std::collections::HashMap::from([
        ("name", "waltr-refresh"),
        ("filename", "keyring-waltr-refresh"),
    ]);
    android_native_keyring_store::Store::new_with_configuration(&configuration)?
        .build(SERVICE, USER, None)
}

#[cfg(any(target_os = "android", target_os = "linux"))]
fn load_error(error: Error) -> NativeRefreshTokenStoreError {
    match error {
        Error::BadEncoding(_) | Error::BadDataFormat(_, _) | Error::BadStoreFormat(_) => {
            NativeRefreshTokenStoreError::Malformed
        }
        _ => NativeRefreshTokenStoreError::Unavailable,
    }
}

#[cfg(any(target_os = "android", target_os = "linux"))]
#[async_trait]
impl NativeRefreshTokenStore for PlatformRefreshTokenStore {
    async fn load(&self) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
        let entry = Arc::clone(&self.entry);
        let result =
            tokio::task::spawn_blocking(move || Self::with_entry(&entry, Entry::get_secret))
                .await
                .map_err(|_| NativeRefreshTokenStoreError::Unavailable)?;
        match result {
            Ok(secret) => Ok(Some(secret)),
            Err(Error::NoEntry) => Ok(None),
            Err(error) => Err(load_error(error)),
        }
    }

    async fn store(&self, token: &[u8]) -> Result<(), NativeRefreshTokenStoreError> {
        let entry = Arc::clone(&self.entry);
        let token = token.to_vec();
        tokio::task::spawn_blocking(move || {
            Self::with_entry(&entry, |entry| entry.set_secret(&token))
        })
        .await
        .map_err(|_| NativeRefreshTokenStoreError::Unavailable)?
        .map_err(|_| NativeRefreshTokenStoreError::Unavailable)
    }

    async fn delete(&self) -> Result<(), NativeRefreshTokenStoreError> {
        let entry = Arc::clone(&self.entry);
        let result =
            tokio::task::spawn_blocking(move || Self::with_entry(&entry, Entry::delete_credential))
                .await
                .map_err(|_| NativeRefreshTokenStoreError::Unavailable)?;
        match result {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(_) => Err(NativeRefreshTokenStoreError::Unavailable),
        }
    }
}
