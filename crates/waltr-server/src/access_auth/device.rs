use std::{
    collections::HashMap,
    fmt::Write as _,
    net::SocketAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{ConnectInfo, DefaultBodyLimit, Query, State, rejection::ExtensionRejection},
    http::{HeaderMap, HeaderValue, Request, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use qrcodegen::{QrCode, QrCodeEcc};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{
    AccessAuthenticator, RequestAuthFailure,
    handlers::{
        HandlerError, LoginQuery, build_session_cookie, handler_error, persist_browser_session,
        random_secret, raw_cookie, redirect, session_cookie, set_cookie_header,
        validate_logout_request, validate_return_path,
    },
    oidc::{DeviceAuthorization, DeviceTokenPoll},
    service::OidcAccessAuthenticator,
};

const MAX_ATTEMPTS: usize = 256;
const MAX_ATTEMPT_TTL: Duration = Duration::from_mins(10);
const BROWSER_COOKIE_TTL: Duration = Duration::from_mins(20);
const DEFAULT_INTERVAL: Duration = Duration::from_secs(5);
const BUSY_RETRY: Duration = Duration::from_secs(1);
const DEVICE_CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";

pub(super) fn router() -> Router<AccessAuthenticator> {
    Router::new()
        .route("/oidc/sign-in", get(sign_in))
        .route(
            "/oidc/sign-in.js",
            get(|| async { asset("text/javascript; charset=utf-8", include_str!("device.js")) }),
        )
        .route(
            "/oidc/sign-in.css",
            get(|| async { asset("text/css; charset=utf-8", include_str!("device.css")) }),
        )
        .route("/oidc/device/start", post(start))
        .route("/oidc/device/poll", post(poll))
        .route("/oidc/device/cancel", post(cancel))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn(device_response_headers))
}

async fn device_response_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(DEVICE_CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response
}

fn asset(content_type: &'static str, contents: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], contents).into_response()
}

async fn sign_in(
    State(auth): State<AccessAuthenticator>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    if let Err(error) = validate_destination(&query.return_to) {
        return handler_error(error);
    }
    match auth.authenticate_browser_session_headers(&headers).await {
        Ok(_) => return redirect(&query.return_to),
        Err(RequestAuthFailure::Internal) => return handler_error(HandlerError::Internal),
        Err(RequestAuthFailure::Unauthenticated | RequestAuthFailure::InsufficientScope) => {}
    }
    let query_string = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &query.return_to)
        .finish();
    let fallback = format!("/oidc/login?{query_string}");
    if !auth.supports_device_authorization() {
        return redirect(&fallback);
    }
    let name = cookie_name(&auth);
    let correlation = match raw_cookie(&headers, name) {
        Ok(value) => value
            .filter(|value| valid_secret(value))
            .map_or_else(random_secret, ToOwned::to_owned),
        Err(error) => return handler_error(error),
    };
    let mut response = Html(include_str!("device.html").replace("/oidc/login?fallback", &fallback))
        .into_response();
    set_cookie_header(
        &mut response,
        &build_session_cookie(
            name,
            &correlation,
            !auth.allow_insecure_cookie(),
            BROWSER_COOKIE_TTL,
        ),
    );
    response
}

async fn start(
    State(auth): State<AccessAuthenticator>,
    peer: Result<ConnectInfo<SocketAddr>, ExtensionRejection>,
    headers: HeaderMap,
    Json(query): Json<LoginQuery>,
) -> Response {
    device_result(start_attempt(&auth, peer.ok().map(|peer| peer.0), &headers, query).await)
}

async fn start_attempt(
    auth: &AccessAuthenticator,
    peer: Option<SocketAddr>,
    headers: &HeaderMap,
    query: LoginQuery,
) -> Result<Response, HandlerError> {
    let Some(oidc) = device_oidc(auth) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let key = validate_request(auth, oidc, headers)?;
    validate_destination(&query.return_to)?;
    let now = Instant::now();
    let mut reservation = match oidc
        .device_attempts
        .reserve_start(key, &query.return_to, now)
    {
        Start::Ready(details, until_next_poll) => return Ok(details.response(until_next_poll)),
        Start::Busy => return Ok(StatusCode::CONFLICT.into_response()),
        Start::Full => return Err(HandlerError::RateLimited),
        Start::Reserved(reservation) => reservation,
    };
    if !oidc.admit_device_login(oidc.login_source(peer.map(|peer| peer.ip()), headers)) {
        return Err(HandlerError::RateLimited);
    }
    let grant = oidc
        .provider()
        .request_device_authorization()
        .await
        .map_err(|_| HandlerError::Internal)?;
    let (attempt, response) = Attempt::new(&grant, query.return_to, now, reservation.id.clone())?;
    let expires_at = attempt.expires_at;
    let Some(removed) = reservation.store(attempt, Instant::now()) else {
        return Ok(PollResponse::Expired.response());
    };
    oidc.device_attempts
        .drop_at_expiry(key, reservation.id.clone(), expires_at, removed);
    Ok(response)
}

#[derive(Deserialize)]
struct AttemptQuery {
    attempt: String,
}

async fn poll(
    State(auth): State<AccessAuthenticator>,
    headers: HeaderMap,
    Json(query): Json<AttemptQuery>,
) -> Response {
    device_result(poll_attempt(&auth, &headers, query).await)
}

async fn poll_attempt(
    auth: &AccessAuthenticator,
    headers: &HeaderMap,
    query: AttemptQuery,
) -> Result<Response, HandlerError> {
    let Some(oidc) = device_oidc(auth) else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let key = validate_request(auth, oidc, headers)?;
    let now = Instant::now();
    let (mut reservation, mut attempt) =
        match oidc.device_attempts.reserve_poll(key, &query.attempt, now) {
            Poll::Missing => return Ok(PollResponse::Expired.response()),
            Poll::Early(wait) => return Ok(PollResponse::pending(wait).response()),
            Poll::Reserved(reservation, attempt) => (reservation, attempt),
        };
    let tokens = tokio::select! {
        () = reservation.removed.cancelled() => return Ok(PollResponse::Expired.response()),
        result = oidc.provider().poll_device_tokens(&attempt.device_code) => result,
    };
    match tokens {
        Ok(DeviceTokenPoll::Authorized(tokens)) => {
            let verifier = oidc.verifier();
            let principal = tokio::select! {
                () = reservation.removed.cancelled() => return Ok(PollResponse::Expired.response()),
                result = verifier.validate_device_id_token(&tokens) => result,
            }
            .map_err(|_| HandlerError::Unauthenticated)?;
            // Claim completion before the session write; cancellation can no longer win.
            if !reservation.consume(Instant::now()) {
                return Ok(PollResponse::Expired.response());
            }
            let token = persist_browser_session(oidc, principal.subject()).await?;
            let mut response = PollResponse::Complete {
                return_to: attempt.return_to,
            }
            .response();
            set_cookie_header(&mut response, &session_cookie(auth, &token));
            Ok(response)
        }
        Ok(
            outcome @ (DeviceTokenPoll::Pending
            | DeviceTokenPoll::SlowDown
            | DeviceTokenPoll::TimedOut),
        ) => {
            let now = Instant::now();
            attempt.advance(&outcome, now);
            let wait = attempt
                .next_poll_at
                .saturating_duration_since(now)
                .min(attempt.expires_at.saturating_duration_since(now));
            if reservation.store(attempt, now).is_none() {
                return Ok(PollResponse::Expired.response());
            }
            Ok(PollResponse::pending(wait).response())
        }
        Ok(DeviceTokenPoll::Denied) => Ok(PollResponse::Denied.response()),
        Ok(DeviceTokenPoll::Expired) => Ok(PollResponse::Expired.response()),
        Err(_) => Err(HandlerError::Internal),
    }
}

async fn cancel(
    State(auth): State<AccessAuthenticator>,
    headers: HeaderMap,
    Json(query): Json<AttemptQuery>,
) -> Response {
    let Some(oidc) = device_oidc(&auth) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match validate_request(&auth, oidc, &headers) {
        Ok(key) => match oidc.device_attempts.remove(key, &query.attempt) {
            true => StatusCode::NO_CONTENT.into_response(),
            false => StatusCode::CONFLICT.into_response(),
        },
        Err(error) => handler_error(error),
    }
}

fn device_result(result: Result<Response, HandlerError>) -> Response {
    result.unwrap_or_else(handler_error)
}

fn device_oidc(auth: &AccessAuthenticator) -> Option<&OidcAccessAuthenticator> {
    auth.oidc()
        .filter(|oidc| oidc.provider().supports_device_authorization())
}

type BrowserKey = [u8; 32];

fn validate_request(
    auth: &AccessAuthenticator,
    oidc: &OidcAccessAuthenticator,
    headers: &HeaderMap,
) -> Result<BrowserKey, HandlerError> {
    let public_url = Url::parse(oidc.config().public_url()).map_err(|_| HandlerError::Internal)?;
    validate_logout_request(headers, &public_url)?;
    let correlation = raw_cookie(headers, cookie_name(auth))?
        .filter(|value| valid_secret(value))
        .ok_or(HandlerError::Unauthenticated)?;
    Ok(Sha256::digest(correlation.as_bytes()).into())
}

fn validate_destination(destination: &str) -> Result<(), HandlerError> {
    if destination.len() > 2048 || HeaderValue::from_str(destination).is_err() {
        return Err(HandlerError::BadRequest);
    }
    validate_return_path(destination)
}

fn cookie_name(auth: &AccessAuthenticator) -> &'static str {
    if auth.allow_insecure_cookie() {
        "waltr-device-local"
    } else {
        "__Host-waltr-device"
    }
}

fn valid_secret(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[derive(Serialize)]
struct Details {
    attempt: String,
    user_code: String,
    verification_uri: String,
    qr_image: String,
}

impl Details {
    fn response(&self, wait: Duration) -> Response {
        #[derive(Serialize)]
        struct StartResponse<'a> {
            #[serde(flatten)]
            details: &'a Details,
            retry_after_ms: u64,
        }
        Json(StartResponse {
            details: self,
            retry_after_ms: milliseconds(wait),
        })
        .into_response()
    }
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum PollResponse {
    Pending { retry_after_ms: u64 },
    Complete { return_to: String },
    Denied,
    Expired,
}

impl PollResponse {
    fn pending(wait: Duration) -> Self {
        Self::Pending {
            retry_after_ms: milliseconds(wait),
        }
    }

    fn response(self) -> Response {
        Json(self).into_response()
    }
}

fn milliseconds(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn qr_image(destination: &str) -> Result<String, HandlerError> {
    let qr =
        QrCode::encode_text(destination, QrCodeEcc::Medium).map_err(|_| HandlerError::Internal)?;
    let size = qr.size();
    let canvas = size + 8;
    let mut svg = format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {canvas} {canvas}" shape-rendering="crispEdges"><path fill="white" d="M0 0h{canvas}v{canvas}H0z"/><path fill="black" d=""#
    );
    for y in 0..size {
        for x in 0..size {
            if qr.get_module(x, y) {
                write!(svg, "M{} {}h1v1h-1z", x + 4, y + 4)
                    .expect("writing to a String cannot fail");
            }
        }
    }
    svg.push_str("\"/></svg>");
    Ok(format!(
        "data:image/svg+xml;base64,{}",
        STANDARD.encode(svg)
    ))
}

struct Attempt {
    details: Arc<Details>,
    device_code: String,
    return_to: String,
    expires_at: Instant,
    interval: Duration,
    next_poll_at: Instant,
}

impl Attempt {
    fn new(
        grant: &DeviceAuthorization,
        return_to: String,
        started_at: Instant,
        id: String,
    ) -> Result<(Self, Response), HandlerError> {
        let expires_at = started_at + Duration::from_secs(grant.expires_in).min(MAX_ATTEMPT_TTL);
        let now = Instant::now();
        let interval = grant
            .interval
            .map_or(DEFAULT_INTERVAL, Duration::from_secs)
            .min(MAX_ATTEMPT_TTL);
        let next_poll_at = (now + interval).min(expires_at);
        let details = Arc::new(Details {
            attempt: id,
            user_code: grant.user_code.clone(),
            verification_uri: grant.verification_uri.clone(),
            qr_image: qr_image(
                grant
                    .verification_uri_complete
                    .as_deref()
                    .unwrap_or(&grant.verification_uri),
            )?,
        });
        let response = details.response(next_poll_at.saturating_duration_since(now));
        Ok((
            Self {
                details,
                device_code: grant.device_code.clone(),
                return_to,
                expires_at,
                interval,
                next_poll_at,
            },
            response,
        ))
    }

    fn advance(&mut self, outcome: &DeviceTokenPoll, now: Instant) {
        self.interval = match outcome {
            DeviceTokenPoll::SlowDown => self.interval + DEFAULT_INTERVAL,
            DeviceTokenPoll::TimedOut => self.interval * 2,
            _ => self.interval,
        }
        .min(MAX_ATTEMPT_TTL);
        self.next_poll_at = (now + self.interval).min(self.expires_at);
    }
}

#[derive(Default)]
pub(super) struct DeviceAttempts(Arc<Mutex<HashMap<BrowserKey, Slot>>>);

struct Slot {
    id: String,
    expires_at: Instant,
    attempt: Option<Box<Attempt>>,
    removed: CancellationToken,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.removed.cancel();
    }
}

enum Start<'a> {
    Ready(Arc<Details>, Duration),
    Busy,
    Full,
    Reserved(Reservation<'a>),
}

enum Poll<'a> {
    Missing,
    Early(Duration),
    Reserved(Reservation<'a>, Box<Attempt>),
}

impl DeviceAttempts {
    fn lock(&self) -> MutexGuard<'_, HashMap<BrowserKey, Slot>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn reserve_start(&self, key: BrowserKey, return_to: &str, now: Instant) -> Start<'_> {
        let mut slots = self.lock();
        slots.retain(|_, slot| slot.expires_at > now);
        if let Some(slot) = slots.get(&key) {
            return slot
                .attempt
                .as_ref()
                .filter(|attempt| attempt.return_to == return_to)
                .map_or(Start::Busy, |attempt| {
                    Start::Ready(
                        Arc::clone(&attempt.details),
                        attempt.next_poll_at.saturating_duration_since(now),
                    )
                });
        }
        if slots.len() >= MAX_ATTEMPTS {
            return Start::Full;
        }
        let id = random_secret();
        let removed = CancellationToken::new();
        slots.insert(
            key,
            Slot {
                id: id.clone(),
                expires_at: now + MAX_ATTEMPT_TTL,
                attempt: None,
                removed: removed.clone(),
            },
        );
        Start::Reserved(Reservation {
            registry: self,
            key,
            id,
            armed: true,
            removed,
        })
    }

    fn reserve_poll(&self, key: BrowserKey, id: &str, now: Instant) -> Poll<'_> {
        let mut slots = self.lock();
        let Some(slot) = slots
            .get_mut(&key)
            .filter(|slot| slot.id == id && slot.expires_at > now)
        else {
            return Poll::Missing;
        };
        let Some(attempt) = slot.attempt.as_ref() else {
            return Poll::Early(BUSY_RETRY);
        };
        if now < attempt.next_poll_at {
            return Poll::Early(attempt.next_poll_at.saturating_duration_since(now));
        }
        Poll::Reserved(
            Reservation {
                registry: self,
                key,
                id: id.to_owned(),
                armed: true,
                removed: slot.removed.clone(),
            },
            slot.attempt.take().expect("ready attempt is present"),
        )
    }

    fn remove(&self, key: BrowserKey, id: &str) -> bool {
        let mut slots = self.lock();
        if slots.get(&key).is_some_and(|slot| slot.id == id) {
            slots.remove(&key);
            true
        } else {
            false
        }
    }

    fn drop_at_expiry(
        &self,
        key: BrowserKey,
        id: String,
        expires_at: Instant,
        removed: CancellationToken,
    ) {
        let slots = Arc::downgrade(&self.0);
        tokio::spawn(async move {
            tokio::select! {
                () = removed.cancelled() => {}
                () = tokio::time::sleep_until(expires_at.into()) => {
                    if let Some(slots) = slots.upgrade() {
                        Self(slots).remove(key, &id);
                    }
                }
            }
        });
    }
}

struct Reservation<'a> {
    registry: &'a DeviceAttempts,
    key: BrowserKey,
    id: String,
    armed: bool,
    removed: CancellationToken,
}

impl Reservation<'_> {
    fn store(
        &mut self,
        attempt: impl Into<Box<Attempt>>,
        now: Instant,
    ) -> Option<CancellationToken> {
        let mut slots = self.registry.lock();
        let attempt = attempt.into();
        if let Some(slot) = slots
            .get_mut(&self.key)
            .filter(|slot| slot.id == self.id && slot.expires_at > now && attempt.expires_at > now)
        {
            slot.expires_at = attempt.expires_at;
            slot.attempt = Some(attempt);
            self.armed = false;
            Some(slot.removed.clone())
        } else {
            None
        }
    }

    fn consume(&mut self, now: Instant) -> bool {
        let mut slots = self.registry.lock();
        if slots.get(&self.key).is_some_and(|slot| {
            slot.id == self.id && slot.expires_at > now && slot.attempt.is_none()
        }) {
            slots.remove(&self.key);
            self.armed = false;
            true
        } else {
            false
        }
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut slots = self.registry.lock();
        if slots
            .get(&self.key)
            .is_some_and(|slot| slot.id == self.id && slot.attempt.is_none())
        {
            slots.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reserve(registry: &DeviceAttempts, key: BrowserKey, now: Instant) -> Reservation<'_> {
        match registry.reserve_start(key, "/sessions", now) {
            Start::Reserved(reservation) => reservation,
            _ => panic!("expected a start reservation"),
        }
    }

    fn sample_attempt(id: &str, now: Instant) -> Attempt {
        Attempt {
            details: Arc::new(Details {
                attempt: id.to_owned(),
                user_code: String::new(),
                verification_uri: String::new(),
                qr_image: String::new(),
            }),
            device_code: String::new(),
            return_to: "/sessions".to_owned(),
            expires_at: now + MAX_ATTEMPT_TTL,
            interval: DEFAULT_INTERVAL,
            next_poll_at: now + DEFAULT_INTERVAL,
        }
    }

    #[test]
    fn device_attempt_cadence_is_bound_to_one_browser_and_one_in_flight_poll() {
        let attempts = DeviceAttempts::default();
        let now = Instant::now();
        let key = [1; 32];
        let other_key = [2; 32];
        let mut starting = reserve(&attempts, key, now);
        assert!(matches!(
            attempts.reserve_start(key, "/sessions", now),
            Start::Busy
        ));
        let id = starting.id.clone();
        assert!(starting.store(sample_attempt(&id, now), now).is_some());
        assert!(matches!(
            attempts.reserve_start(key, "/sessions", now),
            Start::Ready(_, DEFAULT_INTERVAL)
        ));
        assert!(matches!(
            attempts.reserve_start(key, "/other", now),
            Start::Busy
        ));
        assert!(matches!(
            attempts.reserve_poll(other_key, &id, now),
            Poll::Missing
        ));
        assert!(matches!(
            attempts.reserve_poll(key, &id, now),
            Poll::Early(DEFAULT_INTERVAL)
        ));

        let now = now + DEFAULT_INTERVAL;
        let Poll::Reserved(mut polling, mut attempt) = attempts.reserve_poll(key, &id, now) else {
            panic!("poll should be due");
        };
        drop(starting);
        assert!(matches!(
            attempts.reserve_poll(key, &id, now),
            Poll::Early(BUSY_RETRY)
        ));
        attempt.advance(&DeviceTokenPoll::SlowDown, now);
        assert_eq!(attempt.next_poll_at, now + DEFAULT_INTERVAL * 2);
        attempt.advance(&DeviceTokenPoll::TimedOut, now);
        assert_eq!(attempt.next_poll_at, now + DEFAULT_INTERVAL * 4);
        assert!(polling.store(attempt, now).is_some());
        assert!(
            matches!(attempts.reserve_poll(key, &id, now), Poll::Early(wait) if wait == DEFAULT_INTERVAL * 4)
        );
    }

    #[test]
    fn cancellation_expiry_and_replay_cannot_claim_completion() {
        let attempts = DeviceAttempts::default();
        let key = [1; 32];
        let now = Instant::now();
        let mut start = reserve(&attempts, key, now);
        let id = start.id.clone();
        assert!(start.store(sample_attempt(&id, now), now).is_some());

        let due = now + DEFAULT_INTERVAL;
        let Poll::Reserved(mut poll, _) = attempts.reserve_poll(key, &id, due) else {
            panic!("poll should be due");
        };
        assert!(attempts.remove(key, &id));
        assert!(poll.removed.is_cancelled());
        let mut replacement = reserve(&attempts, key, now);
        assert!(!poll.consume(due));
        assert!(!attempts.remove(key, &id));
        assert!(matches!(
            attempts.reserve_poll(key, &id, due),
            Poll::Missing
        ));
        let replacement_id = replacement.id.clone();
        assert!(
            replacement
                .store(sample_attempt(&replacement_id, now), now)
                .is_some()
        );

        let Poll::Reserved(mut poll, _) = attempts.reserve_poll(key, &replacement_id, due) else {
            panic!("poll should be due");
        };
        assert!(!poll.consume(now + MAX_ATTEMPT_TTL));
        assert!(poll.consume(due));
        assert!(!attempts.remove(key, &replacement_id));
        assert!(!poll.consume(due));
        assert!(matches!(
            attempts.reserve_poll(key, &replacement_id, due),
            Poll::Missing
        ));
    }

    #[test]
    fn registry_caps_reserved_attempts_and_recovers_abandoned_slots() {
        let attempts = DeviceAttempts::default();
        let now = Instant::now();
        let keys = (0..MAX_ATTEMPTS).map(|index| Sha256::digest(index.to_ne_bytes()).into());
        let mut reservations: Vec<_> = keys.map(|key| reserve(&attempts, key, now)).collect();
        let another = [0; 32];
        assert!(matches!(
            attempts.reserve_start(another, "/", now),
            Start::Full
        ));
        reservations.pop();
        assert!(matches!(
            attempts.reserve_start(another, "/", now),
            Start::Reserved(_)
        ));
    }
}
