use std::{
    net::SocketAddr,
    time::{Duration, SystemTime},
};

use axum::{
    Json, Router,
    extract::{
        ConnectInfo, Query, State,
        rejection::{ExtensionRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use cookie::{Cookie, SameSite, time};
use rand::random;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use url::Url;

use super::{
    AccessAuthError, AccessAuthStoreError, AccessAuthenticator, AuthorizationCode, BrowserSession,
    BrowserSessionTokenDigest, LoginAttempt, LoginCorrelationDigest, LoginStateDigest,
    RequestAuthFailure,
};

const LOGIN_ATTEMPT_TTL: Duration = Duration::from_mins(10);
const AUTH_CLEANUP_LIMIT: u32 = 64;
const NO_STORE: &str = "no-store";

/// Build the public browser access-authentication routes.
pub fn access_auth_router(authenticator: AccessAuthenticator) -> Router {
    let router = Router::new()
        .route("/oidc/login", get(login))
        .route("/oidc/callback", get(callback))
        .route("/oidc/logout", post(logout))
        .route("/oidc/session", get(session));
    let router = if authenticator.enabled() {
        router.route(
            "/.well-known/oauth-protected-resource",
            get(api_protected_resource),
        )
    } else {
        router
    };
    router
        .layer(middleware::from_fn(no_store_layer))
        .with_state(authenticator)
}

impl AccessAuthenticator {
    /// Build this service's public browser authentication router.
    pub fn router(&self) -> Router {
        access_auth_router(self.clone())
    }
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    #[serde(default = "default_return_path")]
    return_to: String,
}

#[derive(Debug, Deserialize)]
struct CallbackQuery {
    state: Option<String>,
    code: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionStatus {
    authenticated: bool,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: &'static str,
}

#[derive(Debug, Serialize)]
struct ApiProtectedResourceMetadata<'a> {
    resource: &'a str,
    authorization_servers: [&'a str; 1],
    scopes_supported: &'a [String],
}

/// Serve Waltr native API protected-resource metadata.
pub async fn api_protected_resource(State(auth): State<AccessAuthenticator>) -> Response {
    let (Some(resource), Some(issuer)) =
        (auth.api_resource_url(), auth.api_authorization_server_url())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    no_store(
        Json(ApiProtectedResourceMetadata {
            resource,
            authorization_servers: [issuer],
            scopes_supported: auth.api_required_scopes(),
        })
        .into_response(),
    )
}

async fn login(
    State(auth): State<AccessAuthenticator>,
    peer: Result<ConnectInfo<SocketAddr>, ExtensionRejection>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let source = auth
        .oidc()
        .and_then(|oidc| oidc.login_source(peer.ok().map(|peer| peer.0.ip()), &headers));
    match start_login(&auth, source, &query.return_to).await {
        Ok(start) => {
            let mut response = redirect(&start.location);
            if let Some(correlation) = start.correlation {
                set_cookie_header(&mut response, &login_cookie(&auth, &correlation));
            }
            response
        }
        Err(error) => handler_error(error),
    }
}

struct LoginStart {
    location: String,
    correlation: Option<String>,
}

async fn start_login(
    auth: &AccessAuthenticator,
    source: Option<std::net::IpAddr>,
    return_to: &str,
) -> Result<LoginStart, HandlerError> {
    validate_return_path(return_to)?;
    let Some(oidc) = auth.oidc() else {
        return Ok(LoginStart {
            location: return_to.to_owned(),
            correlation: None,
        });
    };
    if !oidc.admit_login(source) {
        return Err(HandlerError::RateLimited);
    }
    let state = random_secret();
    let correlation = random_secret();
    let nonce = random_secret();
    let verifier = random_secret();
    let challenge = pkce_challenge(&verifier);
    let now = SystemTime::now();
    let expires_at = now
        .checked_add(LOGIN_ATTEMPT_TTL)
        .ok_or(HandlerError::Internal)?;
    oidc.store()
        .insert_login_attempt(&LoginAttempt {
            state_digest: LoginStateDigest::from_state(&state),
            correlation_digest: LoginCorrelationDigest::from_correlation(&correlation),
            nonce: nonce.clone(),
            pkce_verifier: verifier,
            return_path: return_to.to_owned(),
            created_at: now,
            expires_at,
        })
        .await
        .map_err(|error| HandlerError::Service(AccessAuthError::persistence(error)))?;
    oidc.store()
        .cleanup_best_effort(now, AUTH_CLEANUP_LIMIT)
        .await;
    let location = oidc
        .provider()
        .authorization_url(&state, &nonce, &challenge)
        .map(String::from)
        .map_err(|error| HandlerError::Service(AccessAuthError::from_oidc(error)))?;
    Ok(LoginStart {
        location,
        correlation: Some(correlation),
    })
}

async fn callback(
    State(auth): State<AccessAuthenticator>,
    headers: HeaderMap,
    query: Result<Query<CallbackQuery>, QueryRejection>,
) -> Response {
    let result = match query {
        Ok(Query(query)) => complete_callback(&auth, query, &headers).await,
        Err(_) => Err(HandlerError::BadRequest),
    };
    let mut response = match result {
        Ok((return_path, session_token)) => {
            let mut response = redirect(&return_path);
            if let Some(token) = session_token {
                set_cookie_header(&mut response, &session_cookie(&auth, &token));
            }
            response
        }
        Err(error) => handler_error(error),
    };
    set_cookie_header(&mut response, &clearing_login_cookie(&auth));
    response
}

async fn complete_callback(
    auth: &AccessAuthenticator,
    query: CallbackQuery,
    headers: &HeaderMap,
) -> Result<(String, Option<String>), HandlerError> {
    let Some(oidc) = auth.oidc() else {
        return Err(HandlerError::BadRequest);
    };
    let state = query.state.as_deref().ok_or(HandlerError::BadRequest)?;
    if state.is_empty() || state.len() > 1024 || state.chars().any(char::is_control) {
        return Err(HandlerError::BadRequest);
    }
    let correlation =
        raw_cookie(headers, auth.login_cookie_name())?.ok_or(HandlerError::Unauthenticated)?;
    let attempt = oidc
        .store()
        .consume_login_attempt(
            &LoginStateDigest::from_state(state),
            &LoginCorrelationDigest::from_correlation(correlation),
            SystemTime::now(),
        )
        .await
        .map_err(|error| HandlerError::Service(AccessAuthError::persistence(error)))?
        .ok_or(HandlerError::Unauthenticated)?;
    if query.error.is_some() {
        return Err(HandlerError::Unauthenticated);
    }
    let code = query.code.as_deref().ok_or(HandlerError::BadRequest)?;
    if code.is_empty() || code.chars().any(char::is_control) {
        return Err(HandlerError::BadRequest);
    }
    let tokens = oidc
        .provider()
        .exchange_code(&AuthorizationCode {
            code,
            pkce_verifier: &attempt.pkce_verifier,
        })
        .await
        .map_err(|error| HandlerError::Service(AccessAuthError::from_oidc(error)))?;
    let verifier = oidc.verifier();
    let principal = verifier
        .validate_id_token(
            tokens.id_token(),
            &attempt.nonce,
            Some(tokens.access_token()),
        )
        .await
        .map_err(|error| HandlerError::Service(AccessAuthError::from_oidc(error)))?;
    let token = random_secret();
    let now = SystemTime::now();
    let expires_at = now
        .checked_add(oidc.config().session_ttl())
        .ok_or(HandlerError::Internal)?;
    oidc.store()
        .insert_browser_session(&BrowserSession {
            token_digest: BrowserSessionTokenDigest::from_token(&token),
            issuer: oidc.config().issuer().to_owned(),
            client_id: oidc.config().client_id().to_owned(),
            subject: principal.subject().to_owned(),
            created_at: now,
            expires_at,
            last_seen_at: now,
        })
        .await
        .map_err(map_session_store_error)?;
    oidc.store()
        .cleanup_best_effort(now, AUTH_CLEANUP_LIMIT)
        .await;
    Ok((attempt.return_path, Some(token)))
}

async fn logout(State(auth): State<AccessAuthenticator>, headers: HeaderMap) -> Response {
    if let Some(oidc) = auth.oidc() {
        let public_url =
            Url::parse(oidc.config().public_url()).expect("validated public URL must remain valid");
        if validate_logout_request(&headers, &public_url).is_err() {
            return handler_error(HandlerError::Forbidden);
        }
        let token = match raw_cookie(&headers, auth.session_cookie_name()) {
            Ok(token) => token,
            Err(error) => return handler_error(error),
        };
        if let Some(token) = token {
            if auth
                .revoke_browser_session_token(token, SystemTime::now())
                .await
                .is_err()
            {
                return handler_error(HandlerError::Internal);
            }
        }
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    set_cookie_header(&mut response, &clearing_cookie(&auth));
    no_store(response)
}

fn validate_logout_request(headers: &HeaderMap, public_url: &Url) -> Result<(), HandlerError> {
    let origin = exactly_one_header(headers, header::ORIGIN)?;
    let origin = Url::parse(origin).map_err(|_| HandlerError::Forbidden)?;
    if !matches!(origin.scheme(), "http" | "https")
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
        || origin.scheme() != public_url.scheme()
        || origin.host_str() != public_url.host_str()
        || origin.port_or_known_default() != public_url.port_or_known_default()
    {
        return Err(HandlerError::Forbidden);
    }

    let mut fetch_sites = headers.get_all("sec-fetch-site").iter();
    if let Some(fetch_site) = fetch_sites.next()
        && (fetch_sites.next().is_some() || fetch_site.to_str().ok() != Some("same-origin"))
    {
        return Err(HandlerError::Forbidden);
    }
    Ok(())
}

fn exactly_one_header(headers: &HeaderMap, name: header::HeaderName) -> Result<&str, HandlerError> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(HandlerError::Forbidden)?;
    if values.next().is_some() {
        return Err(HandlerError::Forbidden);
    }
    value.to_str().map_err(|_| HandlerError::Forbidden)
}

async fn session(State(auth): State<AccessAuthenticator>, headers: HeaderMap) -> Response {
    let authenticated = match auth.authenticate_browser_headers(&headers).await {
        Ok(_) => true,
        Err(RequestAuthFailure::Unauthenticated | RequestAuthFailure::InsufficientScope) => false,
        Err(RequestAuthFailure::Internal) => return handler_error(HandlerError::Internal),
    };
    no_store(Json(SessionStatus { authenticated }).into_response())
}

fn default_return_path() -> String {
    "/".to_owned()
}

fn validate_return_path(path: &str) -> Result<(), HandlerError> {
    let mut candidate = path.as_bytes().to_vec();
    for _ in 0..4 {
        let text = std::str::from_utf8(&candidate).map_err(|_| HandlerError::BadRequest)?;
        if !text.starts_with('/')
            || text.starts_with("//")
            || text.contains('\\')
            || text.chars().any(char::is_control)
        {
            return Err(HandlerError::BadRequest);
        }
        let Some(percent) = candidate.iter().position(|byte| *byte == b'%') else {
            return Ok(());
        };
        let mut decoded = Vec::with_capacity(candidate.len());
        decoded.extend_from_slice(&candidate[..percent]);
        let mut index = percent;
        while index < candidate.len() {
            if candidate[index] != b'%' {
                decoded.push(candidate[index]);
                index += 1;
                continue;
            }
            if index + 2 >= candidate.len() {
                return Err(HandlerError::BadRequest);
            }
            let high = hex(candidate[index + 1]).ok_or(HandlerError::BadRequest)?;
            let low = hex(candidate[index + 2]).ok_or(HandlerError::BadRequest)?;
            decoded.push(high << 4 | low);
            index += 3;
        }
        candidate = decoded;
    }
    Err(HandlerError::BadRequest)
}

const fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn random_secret() -> String {
    URL_SAFE_NO_PAD.encode(random::<[u8; 32]>())
}

fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn session_cookie(auth: &AccessAuthenticator, token: &str) -> Cookie<'static> {
    let oidc = auth.oidc().expect("session cookies require OIDC mode");
    build_session_cookie(
        auth.session_cookie_name(),
        token,
        !oidc.config().allow_insecure_cookie(),
        oidc.config().session_ttl(),
    )
}

fn login_cookie(auth: &AccessAuthenticator, correlation: &str) -> Cookie<'static> {
    let oidc = auth.oidc().expect("login cookies require OIDC mode");
    build_session_cookie(
        auth.login_cookie_name(),
        correlation,
        !oidc.config().allow_insecure_cookie(),
        LOGIN_ATTEMPT_TTL,
    )
}

fn build_session_cookie(
    name: &'static str,
    token: &str,
    secure: bool,
    ttl: Duration,
) -> Cookie<'static> {
    let seconds = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
    Cookie::build((name, token.to_owned()))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::seconds(seconds))
        .build()
}

fn clearing_cookie(auth: &AccessAuthenticator) -> Cookie<'static> {
    let secure = auth
        .oidc()
        .is_none_or(|oidc| !oidc.config().allow_insecure_cookie());
    build_clearing_cookie(auth.session_cookie_name(), secure)
}

fn build_clearing_cookie(name: &'static str, secure: bool) -> Cookie<'static> {
    Cookie::build((name, ""))
        .http_only(true)
        .secure(secure)
        .same_site(SameSite::Lax)
        .path("/")
        .max_age(time::Duration::ZERO)
        .build()
}

fn clearing_login_cookie(auth: &AccessAuthenticator) -> Cookie<'static> {
    let secure = auth
        .oidc()
        .is_none_or(|oidc| !oidc.config().allow_insecure_cookie());
    build_clearing_cookie(auth.login_cookie_name(), secure)
}

fn raw_cookie<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, HandlerError> {
    let mut found = None;
    for value in headers.get_all(header::COOKIE) {
        let value = value.to_str().map_err(|_| HandlerError::BadRequest)?;
        for pair in value.split(';') {
            let (candidate, value) = pair
                .trim()
                .split_once('=')
                .ok_or(HandlerError::BadRequest)?;
            if candidate == name && (value.is_empty() || found.replace(value).is_some()) {
                return Err(HandlerError::BadRequest);
            }
        }
    }
    Ok(found)
}

fn redirect(location: &str) -> Response {
    HeaderValue::from_str(location).map_or_else(
        |_| handler_error(HandlerError::Internal),
        |location| {
            let mut response = StatusCode::SEE_OTHER.into_response();
            response.headers_mut().insert(header::LOCATION, location);
            no_store(response)
        },
    )
}

fn set_cookie_header(response: &mut Response, cookie: &Cookie<'_>) {
    if let Ok(value) = HeaderValue::from_str(&cookie.to_string()) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static(NO_STORE));
    response
}

async fn no_store_layer(request: Request<axum::body::Body>, next: Next) -> Response {
    no_store(next.run(request).await)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HandlerError {
    BadRequest,
    Forbidden,
    Unauthenticated,
    RateLimited,
    Capacity,
    Internal,
    Service(AccessAuthError),
}

fn handler_error(error: HandlerError) -> Response {
    let (status, message) = match error {
        HandlerError::BadRequest => (StatusCode::BAD_REQUEST, "invalid authentication request"),
        HandlerError::Forbidden => (StatusCode::FORBIDDEN, "same-origin request required"),
        HandlerError::Unauthenticated => (StatusCode::UNAUTHORIZED, "authentication failed"),
        HandlerError::RateLimited => (StatusCode::TOO_MANY_REQUESTS, "login rate limit exceeded"),
        HandlerError::Capacity => (
            StatusCode::SERVICE_UNAVAILABLE,
            "browser session capacity reached",
        ),
        HandlerError::Internal | HandlerError::Service(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "authentication service unavailable",
        ),
    };
    no_store((status, Json(ErrorBody { error: message })).into_response())
}

impl From<HandlerError> for AccessAuthError {
    fn from(error: HandlerError) -> Self {
        match error {
            HandlerError::BadRequest => Self::InvalidRequest("invalid authentication request"),
            HandlerError::Forbidden => Self::InvalidRequest("same-origin request required"),
            HandlerError::Unauthenticated => Self::InvalidRequest("authentication failed"),
            HandlerError::RateLimited => Self::InvalidRequest("login rate limit exceeded"),
            HandlerError::Capacity => Self::Persistence("browser session capacity reached"),
            HandlerError::Internal => Self::Provider("authentication service unavailable"),
            HandlerError::Service(error) => error,
        }
    }
}

const fn map_session_store_error(error: AccessAuthStoreError) -> HandlerError {
    if matches!(
        error,
        AccessAuthStoreError::BrowserSessionGlobalCapacity
            | AccessAuthStoreError::BrowserSessionIssuerSubjectCapacity
    ) {
        HandlerError::Capacity
    } else {
        HandlerError::Service(AccessAuthError::persistence(error))
    }
}

#[cfg(test)]
#[allow(
    clippy::duration_suboptimal_units,
    clippy::too_many_lines,
    reason = "HTTP fixtures keep setup and boundary timing explicit"
)]
mod tests {
    use super::*;

    use crate::{
        access_auth::{AccessAuthConfig, AccessAuthHttpClient, OidcConfig},
        persistence::{DatabaseConfig, Persistence},
    };

    #[test]
    fn validates_only_local_absolute_return_paths() {
        for valid in ["/", "/sessions/current", "/path?tab=one#item"] {
            assert!(validate_return_path(valid).is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "relative",
            "//evil.example",
            "https://evil.example",
            "/\\evil",
            "/\npath",
            "/%2fexample",
            "/%5cevil",
            "/%00evil",
            "/%zz",
            "/%",
        ] {
            assert!(validate_return_path(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn generated_pkce_values_have_standard_shape() {
        let verifier = random_secret();
        let challenge = pkce_challenge(&verifier);
        assert_eq!(verifier.len(), 43);
        assert_eq!(challenge.len(), 43);
        assert!(
            verifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
        assert_eq!(
            challenge,
            URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
        );
    }

    #[test]
    fn clearing_cookie_has_bounded_host_cookie_attributes() {
        let auth = AccessAuthenticator::disabled();
        let cookie = clearing_cookie(&auth).to_string();
        assert!(cookie.starts_with("__Host-waltr-session=;"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Lax"));
        assert!(cookie.contains("Secure"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("Max-Age=0"));
    }

    #[test]
    fn issued_cookie_uses_secure_host_policy_or_distinct_explicit_local_policy() {
        let secure = build_session_cookie(
            "__Host-waltr-session",
            "opaque",
            true,
            Duration::from_secs(3600),
        )
        .to_string();
        assert!(secure.starts_with("__Host-waltr-session=opaque;"));
        assert!(secure.contains("HttpOnly"));
        assert!(secure.contains("Max-Age=3600"));
        assert!(secure.contains("SameSite=Lax"));
        assert!(secure.contains("Secure"));
        assert!(secure.contains("Path=/"));

        let local = build_session_cookie(
            "waltr-session-local",
            "opaque",
            false,
            Duration::from_secs(60),
        )
        .to_string();
        assert!(local.starts_with("waltr-session-local=opaque;"));
        assert!(!local.contains("Secure"));
        assert!(!local.starts_with("__Host-"));
    }

    #[test]
    fn login_correlation_cookie_has_short_lived_host_and_local_policies() {
        let secure =
            build_session_cookie("__Host-waltr-login", "correlation", true, LOGIN_ATTEMPT_TTL)
                .to_string();
        assert!(secure.starts_with("__Host-waltr-login=correlation;"));
        assert!(secure.contains("HttpOnly"));
        assert!(secure.contains("Max-Age=600"));
        assert!(secure.contains("SameSite=Lax"));
        assert!(secure.contains("Secure"));
        assert!(secure.contains("Path=/"));

        let local =
            build_session_cookie("waltr-login-local", "correlation", false, LOGIN_ATTEMPT_TTL)
                .to_string();
        assert!(local.starts_with("waltr-login-local=correlation;"));
        assert!(!local.contains("Secure"));
        assert!(!local.starts_with("__Host-"));

        let cleared = build_clearing_cookie("__Host-waltr-login", true).to_string();
        assert!(cleared.starts_with("__Host-waltr-login=;"));
        assert!(cleared.contains("Max-Age=0"));
        assert!(cleared.contains("Secure"));
        assert!(cleared.contains("Path=/"));
    }

    #[test]
    fn correlation_cookie_parser_requires_one_nonempty_value() {
        let mut headers = HeaderMap::new();
        assert_eq!(raw_cookie(&headers, "__Host-waltr-login"), Ok(None));
        headers.insert(
            header::COOKIE,
            "other=value; __Host-waltr-login=correlation"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            raw_cookie(&headers, "__Host-waltr-login"),
            Ok(Some("correlation"))
        );
        headers.insert(
            header::COOKIE,
            "__Host-waltr-login=first; __Host-waltr-login=second"
                .parse()
                .unwrap(),
        );
        assert_eq!(
            raw_cookie(&headers, "__Host-waltr-login"),
            Err(HandlerError::BadRequest)
        );
    }

    #[test]
    fn logout_origin_requires_exact_public_origin() {
        let public_url = Url::parse("https://Waltr.Example.com:443/oidc/callback").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://waltr.example.com".parse().unwrap());
        assert_eq!(validate_logout_request(&headers, &public_url), Ok(()));

        headers.insert(header::ORIGIN, "https://login.example.com".parse().unwrap());
        assert_eq!(
            validate_logout_request(&headers, &public_url),
            Err(HandlerError::Forbidden)
        );
    }

    #[test]
    fn logout_origin_rejects_missing_malformed_and_non_origin_urls() {
        let public_url = Url::parse("https://waltr.example/oidc/callback").unwrap();
        assert_eq!(
            validate_logout_request(&HeaderMap::new(), &public_url),
            Err(HandlerError::Forbidden)
        );
        for origin in [
            "not-an-origin",
            "https://user@waltr.example",
            "https://waltr.example/path",
            "https://waltr.example?query",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::ORIGIN, origin.parse().unwrap());
            assert_eq!(
                validate_logout_request(&headers, &public_url),
                Err(HandlerError::Forbidden),
                "{origin}"
            );
        }
    }

    #[test]
    fn logout_sec_fetch_site_must_be_same_origin_when_present() {
        let public_url = Url::parse("https://waltr.example/oidc/callback").unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, "https://waltr.example".parse().unwrap());
        headers.insert("sec-fetch-site", "same-site".parse().unwrap());
        assert_eq!(
            validate_logout_request(&headers, &public_url),
            Err(HandlerError::Forbidden)
        );
        headers.insert("sec-fetch-site", "same-origin".parse().unwrap());
        assert_eq!(validate_logout_request(&headers, &public_url), Ok(()));
    }

    #[tokio::test]
    async fn disabled_router_sensitive_responses_are_no_store() {
        use tower::ServiceExt as _;

        let response = access_auth_router(AccessAuthenticator::disabled())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/oidc/session")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CACHE_CONTROL], NO_STORE);

        let malformed = access_auth_router(AccessAuthenticator::disabled())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/oidc/login?return_to=%FF")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(malformed.headers()[header::CACHE_CONTROL], NO_STORE);

        let callback = access_auth_router(AccessAuthenticator::disabled())
            .oneshot(
                axum::http::Request::builder()
                    .uri("/oidc/callback?state=value")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(callback.status(), StatusCode::BAD_REQUEST);
        assert!(
            callback.headers()[header::SET_COOKIE]
                .to_str()
                .unwrap()
                .starts_with("__Host-waltr-login=;")
        );
    }

    #[tokio::test]
    async fn committed_login_is_returned_when_advisory_cleanup_fails() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let issuer = format!("http://{address}");
        let metadata_issuer = issuer.clone();
        let provider = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || {
                    let issuer = metadata_issuer.clone();
                    async move {
                        Json(serde_json::json!({
                            "issuer": issuer,
                            "authorization_endpoint": format!("{issuer}/authorize"),
                            "token_endpoint": format!("{issuer}/token"),
                            "jwks_uri": format!("{issuer}/jwks"),
                            "response_types_supported": ["code"],
                            "code_challenge_methods_supported": ["S256"],
                            "token_endpoint_auth_methods_supported": ["none"],
                            "id_token_signing_alg_values_supported": ["RS256"]
                        }))
                    }
                }),
            )
            .route(
                "/jwks",
                get(|| async {
                    Json(serde_json::json!({"keys": [{
                        "kty": "RSA",
                        "kid": "cleanup-test",
                        "use": "sig",
                        "alg": "RS256",
                        "n": "jTXRkLxK45qmKMMdODisx4iMHYQbwI_WncwB-9usF31IUewAgcVtRMWpW9YJRkA713NsP-cfs4NyurzloJ955iwNGJmzBF6_E515c-QFyAAEyyFptkpFqcgZBFUZGHslz8AlgUysSyshjOCNG15VEg-gjdLwDJW1RrvQQC-w1YHj3sgZV_XWfYWy401T5Gc7OdxstVK1uIj2Lg5xusvKGCEBuDdYZmTjgzuQqF2rGFWVC7r4ag3C8xxBfwApRZtk7XvZac3kAtYDCVlHyFbtkmapONybuDijJXzwVfS96x0I7PoXc-c6tSPf-7WpNtIDoLxHcI5Qv66tfsOdfPCc8w",
                        "e": "AQAB"
                    }]}))
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, provider).await.unwrap() });
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let config = OidcConfig::new(
            &issuer,
            "waltr",
            None,
            "http://127.0.0.1:14333/oidc/callback",
            "api",
            vec!["openid".to_owned()],
            Duration::from_secs(60),
            true,
            true,
        )
        .unwrap();
        let auth = AccessAuthenticator::initialize(
            AccessAuthConfig::oidc(config),
            &persistence,
            AccessAuthHttpClient::build(reqwest::Client::builder()).unwrap(),
        )
        .await
        .unwrap();
        let now = SystemTime::now();
        auth.oidc()
            .unwrap()
            .store()
            .insert_login_attempt(&LoginAttempt {
                state_digest: LoginStateDigest::from_state("expired"),
                correlation_digest: LoginCorrelationDigest::from_correlation("expired"),
                nonce: "nonce".to_owned(),
                pkce_verifier: "verifier".to_owned(),
                return_path: "/".to_owned(),
                created_at: now - Duration::from_secs(2),
                expires_at: now - Duration::from_secs(1),
            })
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER fail_handler_cleanup BEFORE DELETE ON waltr_oidc_login_attempts BEGIN SELECT RAISE(ABORT, 'cleanup failed'); END",
        )
        .execute(persistence.sqlite_pool().unwrap())
        .await
        .unwrap();

        let start = start_login(&auth, Some("192.0.2.1".parse().unwrap()), "/")
            .await
            .expect("cleanup must not replace the committed login response");
        let state = Url::parse(&start.location)
            .unwrap()
            .query_pairs()
            .find_map(|(name, value)| (name == "state").then(|| value.into_owned()))
            .unwrap();
        let correlation = start.correlation.unwrap();
        assert!(
            auth.oidc()
                .unwrap()
                .store()
                .consume_login_attempt(
                    &LoginStateDigest::from_state(&state),
                    &LoginCorrelationDigest::from_correlation(&correlation),
                    SystemTime::now(),
                )
                .await
                .unwrap()
                .is_some()
        );
        server.abort();
    }

    #[test]
    fn handler_errors_are_sanitized_and_no_store() {
        for (error, status) in [
            (HandlerError::BadRequest, StatusCode::BAD_REQUEST),
            (HandlerError::Forbidden, StatusCode::FORBIDDEN),
            (HandlerError::Unauthenticated, StatusCode::UNAUTHORIZED),
            (HandlerError::RateLimited, StatusCode::TOO_MANY_REQUESTS),
            (HandlerError::Capacity, StatusCode::SERVICE_UNAVAILABLE),
            (HandlerError::Internal, StatusCode::SERVICE_UNAVAILABLE),
            (
                HandlerError::Service(AccessAuthError::Provider("secret provider detail")),
                StatusCode::SERVICE_UNAVAILABLE,
            ),
        ] {
            let response = handler_error(error);
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CACHE_CONTROL], NO_STORE);
        }
    }

    #[test]
    fn browser_session_capacity_has_a_sanitized_callback_mapping() {
        assert_eq!(
            map_session_store_error(AccessAuthStoreError::BrowserSessionIssuerSubjectCapacity),
            HandlerError::Capacity
        );
    }
}
