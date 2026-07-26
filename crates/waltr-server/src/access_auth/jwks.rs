use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};

use jsonwebtoken::{
    DecodingKey,
    jwk::{AlgorithmParameters, Jwk, JwkSet, KeyOperations, PublicKeyUse},
};
use reqwest::header::CACHE_CONTROL;
use tokio::sync::Mutex;
use url::Url;

use super::oidc::{AccessAuthHttpClient, OidcError, read_bounded_body};

const DEFAULT_CACHE_AGE: Duration = Duration::from_mins(5);
const MIN_CACHE_AGE: Duration = Duration::from_secs(30);
const MAX_CACHE_AGE: Duration = Duration::from_hours(6);
const UNKNOWN_KID_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const MAX_KEYS: usize = 64;
const JWKS_RESPONSE_LIMIT: usize = 1024 * 1024;

/// A bounded, rotating cache of issuer signing keys.
#[derive(Debug, Clone)]
pub(super) struct JwksCache {
    inner: Arc<CacheInner>,
}

#[derive(Debug)]
struct CacheInner {
    client: AccessAuthHttpClient,
    uri: Url,
    algorithms: HashSet<String>,
    state: Mutex<CacheState>,
    refresh: Mutex<()>,
}

#[derive(Debug)]
struct CacheState {
    keys: HashMap<String, Jwk>,
    expires_at: Instant,
    last_unknown_kid_refresh: Option<Instant>,
}

impl JwksCache {
    /// Fetch a nonempty initial key set and create a rotating cache.
    pub(super) async fn initialize(
        client: AccessAuthHttpClient,
        uri: Url,
        algorithms: HashSet<String>,
    ) -> Result<Self, OidcError> {
        let (keys, max_age) = fetch_keys(&client, &uri, &algorithms).await?;
        Ok(Self {
            inner: Arc::new(CacheInner {
                client,
                uri,
                algorithms,
                state: Mutex::new(CacheState {
                    keys,
                    expires_at: Instant::now() + max_age,
                    last_unknown_kid_refresh: None,
                }),
                refresh: Mutex::new(()),
            }),
        })
    }

    /// Resolve exactly one key by `kid`, refreshing on expiry or an unknown ID.
    ///
    /// Unknown IDs never select a fallback key and their refresh path is rate limited.
    pub(super) async fn key(&self, kid: &str) -> Result<Jwk, OidcError> {
        if kid.is_empty() {
            return Err(OidcError::InvalidToken("JWT kid is missing"));
        }
        let now = Instant::now();
        let refresh_for_unknown = {
            let mut state = self.inner.state.lock().await;
            if let Some(key) = state.keys.get(kid) {
                if now < state.expires_at {
                    return Ok(key.clone());
                }
                false
            } else {
                if state
                    .last_unknown_kid_refresh
                    .is_some_and(|last| now.duration_since(last) < UNKNOWN_KID_REFRESH_INTERVAL)
                {
                    return Err(OidcError::UnknownKeyId);
                }
                state.last_unknown_kid_refresh = Some(now);
                true
            }
        };

        let _refresh = self.inner.refresh.lock().await;
        {
            let state = self.inner.state.lock().await;
            if let Some(key) = state.keys.get(kid)
                && (refresh_for_unknown || Instant::now() < state.expires_at)
            {
                return Ok(key.clone());
            }
        }

        let (keys, max_age) =
            fetch_keys(&self.inner.client, &self.inner.uri, &self.inner.algorithms).await?;
        let key = keys.get(kid).cloned();
        let mut state = self.inner.state.lock().await;
        state.keys = keys;
        state.expires_at = Instant::now() + max_age;
        drop(state);
        key.ok_or(OidcError::UnknownKeyId)
    }

    #[cfg(test)]
    pub(super) fn fixture(keys: JwkSet) -> Result<Self, OidcError> {
        let algorithms = test_algorithms();
        let keys = validate_keys(keys, &algorithms)?;
        Ok(Self {
            inner: Arc::new(CacheInner {
                client: AccessAuthHttpClient::build(reqwest::Client::builder())
                    .expect("fixture client"),
                uri: Url::parse("http://127.0.0.1/jwks").expect("fixture URL"),
                algorithms,
                state: Mutex::new(CacheState {
                    keys,
                    expires_at: Instant::now() + DEFAULT_CACHE_AGE,
                    last_unknown_kid_refresh: Some(Instant::now()),
                }),
                refresh: Mutex::new(()),
            }),
        })
    }

    #[cfg(test)]
    pub(super) async fn replace_fixture(&self, keys: JwkSet) -> Result<(), OidcError> {
        let keys = validate_keys(keys, &self.inner.algorithms)?;
        let mut state = self.inner.state.lock().await;
        state.keys = keys;
        state.expires_at = Instant::now() + DEFAULT_CACHE_AGE;
        drop(state);
        Ok(())
    }
}

async fn fetch_keys(
    client: &AccessAuthHttpClient,
    uri: &Url,
    algorithms: &HashSet<String>,
) -> Result<(HashMap<String, Jwk>, Duration), OidcError> {
    let response = client
        .inner()
        .get(uri.clone())
        .send()
        .await
        .map_err(|_| OidcError::Provider("JWKS request failed"))?;
    if !response.status().is_success() {
        return Err(OidcError::Provider("JWKS endpoint returned an error"));
    }
    let max_age = response
        .headers()
        .get(CACHE_CONTROL)
        .and_then(|value| value.to_str().ok())
        .and_then(cache_max_age)
        .unwrap_or(DEFAULT_CACHE_AGE)
        .clamp(MIN_CACHE_AGE, MAX_CACHE_AGE);
    let body = read_bounded_body(
        response,
        JWKS_RESPONSE_LIMIT,
        "JWKS response read failed",
        "JWKS response exceeds the size limit",
    )
    .await?;
    let set = serde_json::from_slice::<JwkSet>(&body)
        .map_err(|_| OidcError::Provider("JWKS response was invalid"))?;
    Ok((validate_keys(set, algorithms)?, max_age))
}

fn validate_keys(
    set: JwkSet,
    algorithms: &HashSet<String>,
) -> Result<HashMap<String, Jwk>, OidcError> {
    if set.keys.is_empty() || set.keys.len() > MAX_KEYS {
        return Err(OidcError::Provider(
            "JWKS must contain between 1 and 64 keys",
        ));
    }
    let mut keys = HashMap::with_capacity(set.keys.len());
    for key in set.keys {
        let Some(kid) = key
            .common
            .key_id
            .as_ref()
            .filter(|kid| !kid.is_empty())
            .cloned()
        else {
            continue;
        };
        if key
            .common
            .public_key_use
            .as_ref()
            .is_some_and(|usage| usage != &PublicKeyUse::Signature)
            || key
                .common
                .key_operations
                .as_ref()
                .is_some_and(|operations| !operations.contains(&KeyOperations::Verify))
            || matches!(key.algorithm, AlgorithmParameters::OctetKey(_))
            || !key_supports_any(&key, algorithms)
            || DecodingKey::from_jwk(&key).is_err()
        {
            continue;
        }
        if keys.contains_key(&kid) {
            return Err(OidcError::Provider(
                "JWKS contains duplicate usable kid values",
            ));
        }
        keys.insert(kid, key);
    }
    if keys.is_empty() {
        return Err(OidcError::Provider(
            "JWKS contains no usable allowed signing key",
        ));
    }
    Ok(keys)
}

fn key_supports_any(key: &Jwk, algorithms: &HashSet<String>) -> bool {
    key.common.key_algorithm.map_or_else(
        || match key.algorithm {
            AlgorithmParameters::RSA(_) => algorithms
                .iter()
                .any(|name| name.starts_with("RS") || name.starts_with("PS")),
            AlgorithmParameters::EllipticCurve(_) => {
                algorithms.iter().any(|name| name.starts_with("ES"))
            }
            AlgorithmParameters::OctetKeyPair(_) => algorithms.contains("EdDSA"),
            AlgorithmParameters::OctetKey(_) => false,
        },
        |algorithm| algorithms.contains(&algorithm.to_string()),
    )
}

#[cfg(test)]
fn test_algorithms() -> HashSet<String> {
    [
        "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256", "ES384", "EdDSA",
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

fn cache_max_age(value: &str) -> Option<Duration> {
    if value.split(',').map(str::trim).any(|directive| {
        directive.eq_ignore_ascii_case("no-cache") || directive.eq_ignore_ascii_case("no-store")
    }) {
        return Some(Duration::ZERO);
    }
    value.split(',').map(str::trim).find_map(|directive| {
        let (name, seconds) = directive.split_once('=')?;
        name.eq_ignore_ascii_case("max-age")
            .then(|| seconds.trim_matches('"').parse::<u64>().ok())
            .flatten()
            .map(Duration::from_secs)
    })
}

#[cfg(test)]
#[allow(
    clippy::await_holding_lock,
    clippy::duration_suboptimal_units,
    clippy::unchecked_time_subtraction,
    reason = "key-cache tests exercise lock and expiry boundaries"
)]
mod tests {
    use super::*;
    use std::{
        convert::Infallible,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use axum::{
        Json, Router,
        body::{Body, Bytes},
        extract::State,
        http::{StatusCode, header},
        response::{IntoResponse as _, Response},
        routing::get,
    };
    use futures_util::stream;
    use serde_json::json;

    fn rsa_set(kid: &str) -> JwkSet {
        serde_json::from_value(json!({"keys": [{
            "kty": "RSA", "kid": kid, "use": "sig", "alg": "RS256",
            "n": "sXchDaQebHnPiGvyDOAT4saGEUetSyo1R7FuZ7Q7we6NwN9oT1Zr4F8GdKjKB7Wj2Y4lRtoHqY4rGfQ8bRk4Pw",
            "e": "AQAB"
        }]})).unwrap()
    }

    fn http_client() -> AccessAuthHttpClient {
        AccessAuthHttpClient::build(reqwest::Client::builder()).unwrap()
    }

    #[tokio::test]
    async fn fixture_never_falls_back_for_unknown_kid() {
        let cache = JwksCache::fixture(rsa_set("known")).unwrap();
        assert!(matches!(
            cache.key("missing").await,
            Err(OidcError::UnknownKeyId)
        ));
        assert_eq!(
            cache.key("known").await.unwrap().common.key_id.as_deref(),
            Some("known")
        );
    }

    #[tokio::test]
    async fn fixture_supports_key_rotation_without_fallback() {
        let cache = JwksCache::fixture(rsa_set("old")).unwrap();
        assert!(cache.key("old").await.is_ok());
        cache.replace_fixture(rsa_set("new")).await.unwrap();
        assert!(cache.key("new").await.is_ok());
        assert!(matches!(
            cache.key("old").await,
            Err(OidcError::UnknownKeyId)
        ));
    }

    #[tokio::test]
    async fn unknown_kid_refreshes_once_and_observes_rotation() {
        let keys = Arc::new(Mutex::new(rsa_set("old")));
        let app = Router::new()
            .route(
                "/jwks",
                get(|State(keys): State<Arc<Mutex<JwkSet>>>| async move {
                    Json(keys.lock().await.clone())
                }),
            )
            .with_state(Arc::clone(&keys));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let cache = JwksCache::initialize(
            http_client(),
            Url::parse(&format!("http://{address}/jwks")).unwrap(),
            test_algorithms(),
        )
        .await
        .unwrap();

        *keys.lock().await = rsa_set("new");
        assert_eq!(
            cache.key("new").await.unwrap().common.key_id.as_deref(),
            Some("new")
        );
        assert!(matches!(
            cache.key("attacker-controlled").await,
            Err(OidcError::UnknownKeyId)
        ));
        server.abort();
    }

    #[tokio::test]
    async fn unknown_kid_and_expiry_refreshes_never_follow_jwks_redirects() {
        let target_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&target_hits);
        let target = Router::new().fallback(move || {
            hits.fetch_add(1, Ordering::SeqCst);
            async { StatusCode::OK }
        });
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_address = target_listener.local_addr().unwrap();
        let target_server =
            tokio::spawn(async move { axum::serve(target_listener, target).await.unwrap() });
        let location = format!("http://{target_address}/stolen");

        for refresh in ["unknown", "expired"] {
            let requests = Arc::new(AtomicUsize::new(0));
            let state = (Arc::clone(&requests), location.clone());
            let source = Router::new()
                .route(
                    "/jwks",
                    get(
                        |State((requests, location)): State<(Arc<AtomicUsize>, String)>| async move {
                            if requests.fetch_add(1, Ordering::SeqCst) == 0 {
                                Json(rsa_set("known")).into_response()
                            } else {
                                (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, location)])
                                    .into_response()
                            }
                        },
                    ),
                )
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, source).await.unwrap() });
            let cache = JwksCache::initialize(
                http_client(),
                Url::parse(&format!("http://{address}/jwks")).unwrap(),
                test_algorithms(),
            )
            .await
            .unwrap();
            let result = if refresh == "unknown" {
                cache.key("unknown").await
            } else {
                cache.inner.state.lock().await.expires_at = Instant::now() - Duration::from_secs(1);
                cache.key("known").await
            };
            assert_eq!(
                result,
                Err(OidcError::Provider("JWKS endpoint returned an error")),
                "{refresh} refresh"
            );
            assert_eq!(requests.load(Ordering::SeqCst), 2);
            assert_eq!(target_hits.load(Ordering::SeqCst), 0);
            server.abort();
        }
        target_server.abort();
    }

    #[tokio::test]
    async fn jwks_stream_accepts_normal_body_and_rejects_oversized_body() {
        let normal = serde_json::to_vec(&rsa_set("normal")).unwrap();
        let router = Router::new()
            .route(
                "/normal",
                get(move || {
                    let normal = normal.clone();
                    async move {
                        Response::new(Body::from_stream(stream::iter([Ok::<_, Infallible>(
                            Bytes::from(normal),
                        )])))
                    }
                }),
            )
            .route(
                "/large",
                get(|| async {
                    Response::new(Body::from_stream(stream::iter([
                        Ok::<_, Infallible>(Bytes::from(vec![b' '; JWKS_RESPONSE_LIMIT])),
                        Ok::<_, Infallible>(Bytes::from_static(b"x")),
                    ])))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });

        let normal = JwksCache::initialize(
            http_client(),
            Url::parse(&format!("http://{address}/normal")).unwrap(),
            test_algorithms(),
        )
        .await
        .unwrap();
        assert!(normal.key("normal").await.is_ok());

        let error = JwksCache::initialize(
            http_client(),
            Url::parse(&format!("http://{address}/large")).unwrap(),
            test_algorithms(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            error,
            OidcError::Provider("JWKS response exceeds the size limit")
        );
        server.abort();
    }

    #[test]
    fn cache_age_is_parsed_and_bounded() {
        assert_eq!(
            cache_max_age("public, max-age=120"),
            Some(Duration::from_secs(120))
        );
        assert_eq!(cache_max_age("max-age=120, no-store"), Some(Duration::ZERO));
        assert_eq!(
            Duration::from_secs(1).clamp(MIN_CACHE_AGE, MAX_CACHE_AGE),
            MIN_CACHE_AGE
        );
    }

    #[test]
    fn empty_and_duplicate_usable_sets_are_rejected() {
        let algorithms = test_algorithms();
        assert!(validate_keys(JwkSet { keys: vec![] }, &algorithms).is_err());
        let key = rsa_set("same").keys.remove(0);
        assert!(
            validate_keys(
                JwkSet {
                    keys: vec![key.clone(), key]
                },
                &algorithms
            )
            .is_err()
        );
    }

    #[test]
    fn mixed_jwks_filters_unusable_keys() {
        let valid = rsa_set("valid").keys.remove(0);
        let mixed: JwkSet = serde_json::from_value(json!({"keys": [
            {"kty":"oct", "kid":"symmetric", "alg":"HS256", "k":"c2VjcmV0"},
            {"kty":"RSA", "kid":"encryption", "use":"enc", "alg":"RS256", "n":"AQAB", "e":"AQAB"},
            {"kty":"RSA", "use":"sig", "alg":"RS256", "n":"AQAB", "e":"AQAB"},
            {"kty":"RSA", "kid":"unsupported", "use":"sig", "alg":"RSA-OAEP", "n":"AQAB", "e":"AQAB"},
            {"kty":"RSA", "kid":"malformed", "use":"sig", "alg":"RS256", "n":"%%%", "e":"AQAB"}
        ]})).unwrap();
        let mut keys = mixed.keys;
        keys.push(valid);

        let filtered = validate_keys(JwkSet { keys }, &test_algorithms()).unwrap();
        assert_eq!(filtered.len(), 1);
        assert!(filtered.contains_key("valid"));
    }

    #[test]
    fn malformed_compatible_key_cannot_satisfy_jwks() {
        let malformed = serde_json::from_value(json!({"keys": [{
            "kty":"RSA", "kid":"malformed", "use":"sig", "alg":"RS256",
            "n":"%%%", "e":"AQAB"
        }]}))
        .unwrap();
        assert!(validate_keys(malformed, &test_algorithms()).is_err());
    }

    #[test]
    fn symmetric_only_set_is_rejected_after_filtering() {
        let symmetric =
            serde_json::from_value(json!({"keys": [{"kty":"oct", "kid":"hmac", "k":"c2VjcmV0"}]}))
                .unwrap();
        assert!(validate_keys(symmetric, &test_algorithms()).is_err());
    }
}
