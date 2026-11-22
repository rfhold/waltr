mod native;
#[cfg(test)]
mod tests;

use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest as _, Sha256};
use tokio::{sync::Mutex, time::Instant};
use tokio_util::sync::CancellationToken;
use tonic::Status;
use url::Url;
use uuid::Uuid;
use waltr::grpc::waltr::auth::v1::{
    AuthAttemptLifecycle, AuthAttemptStatus, AuthCompletionMode, AuthMethod, AuthStatus,
    AuthStatusKind, AuthTarget as ProtoAuthTarget, BeginAuthResponse, auth_target,
};

use crate::{agent_definition::AgentDefinitionState, persistence::OutboundOAuthStore};

use super::{
    AuthTarget, CredentialStore, OAUTH_CREDENTIAL_VERSION, OAuthCredential, OAuthCredentialContext,
    OAuthCredentialRecord, OAuthRefreshClaim, OAuthRefreshClaimResult, RemoteOAuthContext,
    outbound_http::OutboundHttpPolicy, service::now_ms,
};

const ATTEMPT_LIMIT: Duration = Duration::from_secs(15 * 60);
const ATTEMPT_RETENTION_MS: i64 = 900_000;
const MAX_ATTEMPTS: usize = 1024;
const REFRESH_WINDOW_MS: i64 = 30_000;
const REFRESH_TIMEOUT: Duration = Duration::from_secs(20);

/// A validated, expiring workspace bearer for the private worker control channel.
pub struct RemoteCredentialLease {
    /// Access token for the exact configured destination API root.
    pub access_token: String,
    /// Absolute Unix expiry timestamp in milliseconds.
    pub expires_at_ms: i64,
    /// Authorization incarnation, unchanged when the same grant rotates.
    pub authority_id: String,
}

impl fmt::Debug for RemoteCredentialLease {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteCredentialLease")
            .field("access_token", &"<redacted>")
            .field("expires_at_ms", &self.expires_at_ms)
            .field("authority_id", &self.authority_id)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BindingIdentity {
    endpoint: Url,
    fingerprint: String,
    incarnation: Option<String>,
}

impl BindingIdentity {
    fn admits(&self, context: &RemoteOAuthContext) -> bool {
        self.endpoint == context.endpoint
            && self.fingerprint == context.binding_fingerprint
            // At cold load, the exact spec still admits a durable grant from a prior process.
            && self.incarnation.as_ref().is_none_or(|generation| {
                context.publication_incarnation.as_ref() == Some(generation)
            })
    }
}

struct Attempt {
    key: String,
    binding: BindingIdentity,
    cancellation: CancellationToken,
    expires_at_ms: i64,
    lifecycle: AuthAttemptLifecycle,
    failure: &'static str,
}

impl Attempt {
    fn finish(&mut self, lifecycle: AuthAttemptLifecycle, failure: &'static str) {
        self.lifecycle = lifecycle;
        self.failure = failure;
        self.cancellation.cancel();
    }
}

pub(super) struct RemoteAuth {
    resources: AgentDefinitionState,
    sql: Arc<OutboundOAuthStore>,
    credentials: CredentialStore,
    policy: OutboundHttpPolicy,
    attempts: Mutex<HashMap<String, Attempt>>,
    route_lifetimes: std::sync::Mutex<HashMap<String, std::sync::Weak<CancellationToken>>>,
}

impl fmt::Debug for RemoteAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteAuth").finish_non_exhaustive()
    }
}

impl RemoteAuth {
    pub(super) fn new(
        resources: AgentDefinitionState,
        sql: Arc<OutboundOAuthStore>,
        policy: OutboundHttpPolicy,
    ) -> Arc<Self> {
        Arc::new(Self {
            resources,
            credentials: CredentialStore::shared(Arc::clone(&sql)),
            sql,
            policy,
            attempts: Mutex::new(HashMap::new()),
            route_lifetimes: std::sync::Mutex::default(),
        })
    }

    fn identity(&self, key: &str) -> Result<BindingIdentity, Status> {
        let snapshot = self.resources.snapshot();
        let binding = snapshot
            .remotes()
            .get(key)
            .ok_or_else(|| Status::not_found("remote does not exist"))?;
        let encoded = serde_json::to_vec(&(
            binding.endpoint().as_str(),
            binding.binding().map(|binding| {
                (
                    binding.machine_id(),
                    binding.export_root().as_os_str(),
                    binding.mcp_stdio_servers(),
                )
            }),
        ))
        .map_err(|_| Status::internal("remote identity is invalid"))?;
        Ok(BindingIdentity {
            endpoint: binding.endpoint().clone(),
            fingerprint: URL_SAFE_NO_PAD.encode(Sha256::digest(encoded)),
            incarnation: snapshot
                .remote_incarnation(key)
                .map(|incarnation| incarnation.to_string()),
        })
    }

    fn current(&self, key: &str, expected: &BindingIdentity) -> bool {
        self.identity(key).is_ok_and(|current| current == *expected)
    }

    fn ensure_current(&self, key: &str, expected: &BindingIdentity) -> Result<(), Status> {
        if self.current(key, expected) {
            Ok(())
        } else {
            Err(Status::aborted("remote changed"))
        }
    }

    pub(super) async fn begin(self: &Arc<Self>, key: String) -> Result<BeginAuthResponse, Status> {
        let binding = self.identity(&key)?;
        let attempt_id = format!("remote-{}", Uuid::new_v4());
        let cancellation = CancellationToken::new();
        let guard = cancellation.clone().drop_guard();
        let start = Instant::now();
        let start_ms = now_ms();
        {
            let mut attempts = self.attempts.lock().await;
            attempts.retain(|_, attempt| {
                attempt.expires_at_ms.saturating_add(ATTEMPT_RETENTION_MS) > start_ms
            });
            if attempts.len() >= MAX_ATTEMPTS {
                return Err(Status::resource_exhausted(
                    "too many workspace authorization attempts",
                ));
            }
            if attempts.values().any(|attempt| {
                attempt.key == key
                    && attempt.binding == binding
                    && !attempt.cancellation.is_cancelled()
            }) {
                return Err(Status::already_exists(
                    "remote authorization is already in progress",
                ));
            }
            if self.read(&key).await?.is_some() {
                return Err(Status::failed_precondition(
                    "reset remote authorization before authorizing again",
                ));
            }
            self.ensure_current(&key, &binding)?;
            attempts.insert(
                attempt_id.clone(),
                Attempt {
                    key: key.clone(),
                    binding: binding.clone(),
                    cancellation: cancellation.clone(),
                    expires_at_ms: start_ms.saturating_add(duration_ms(ATTEMPT_LIMIT)),
                    lifecycle: AuthAttemptLifecycle::Pending,
                    failure: "",
                },
            );
        }
        let device = tokio::select! {
            () = cancellation.cancelled() => return Err(Status::cancelled("remote authorization canceled")),
            result = tokio::time::timeout_at(start + ATTEMPT_LIMIT, native::begin(
                &self.policy, &binding.endpoint, binding.fingerprint.clone(), binding.incarnation.clone(),
            )) => result.map_err(|_| Status::deadline_exceeded("remote authorization expired"))??,
        };
        let lifetime = device.lifetime.min(ATTEMPT_LIMIT);
        let deadline = start + lifetime;
        if deadline <= Instant::now() {
            return Err(Status::deadline_exceeded("remote authorization expired"));
        }
        let expires_at_ms = start_ms.saturating_add(duration_ms(lifetime));
        let mut attempts = self.attempts.lock().await;
        self.ensure_current(&key, &binding)?;
        let attempt = attempts
            .get_mut(&attempt_id)
            .filter(|attempt| !attempt.cancellation.is_cancelled())
            .ok_or_else(|| Status::cancelled("remote authorization canceled"))?;
        attempt.expires_at_ms = expires_at_ms;
        let response = BeginAuthResponse {
            attempt_id: attempt_id.clone(),
            method: AuthMethod::OauthDevice as i32,
            verification_url: device.verification_url.to_string(),
            user_code: device.user_code.clone(),
            expires_at_ms,
            poll_interval_seconds: i32::try_from(device.interval.as_secs()).unwrap_or(i32::MAX),
            completion_mode: AuthCompletionMode::ServerDevicePoll as i32,
            ..BeginAuthResponse::default()
        };
        let service = Arc::downgrade(self);
        let policy = self.policy.clone();
        tokio::spawn(async move {
            let result = tokio::select! {
                biased;
                () = cancellation.cancelled() => return,
                result = tokio::time::timeout_at(deadline, poll_device(&service, &attempt_id, &policy, device)) => result,
            };
            if let Some(service) = service.upgrade() {
                let result = result.unwrap_or(Err(DeviceFailure::Expired));
                service.finish(&attempt_id, result).await;
            }
        });
        drop(guard.disarm());
        Ok(response)
    }

    async fn still_pending(&self, id: &str) -> bool {
        self.attempts.lock().await.get(id).is_some_and(|attempt| {
            !attempt.cancellation.is_cancelled() && self.current(&attempt.key, &attempt.binding)
        })
    }

    async fn finish(&self, id: &str, result: Result<OAuthCredential, DeviceFailure>) {
        let mut attempts = self.attempts.lock().await;
        let Some(attempt) = attempts
            .get_mut(id)
            .filter(|attempt| !attempt.cancellation.is_cancelled())
        else {
            return;
        };
        let lifecycle = AuthAttemptLifecycle::Failed;
        if !self.current(&attempt.key, &attempt.binding) {
            attempt.finish(AuthAttemptLifecycle::Canceled, "remote changed");
            return;
        }
        match result {
            Ok(_) if attempt.expires_at_ms <= now_ms() => {
                attempt.finish(
                    AuthAttemptLifecycle::Expired,
                    "authorization attempt expired",
                );
            }
            Ok(credential) => {
                let stored = self
                    .sql
                    .install_workspace_credential(&credential_key(&attempt.key), &credential)
                    .await;
                if !self.current(&attempt.key, &attempt.binding) {
                    attempt.finish(AuthAttemptLifecycle::Canceled, "remote changed");
                } else {
                    match stored {
                        Ok(true) => attempt.finish(AuthAttemptLifecycle::Succeeded, ""),
                        Ok(false) => attempt.finish(
                            lifecycle,
                            "reset remote authorization before authorizing again",
                        ),
                        Err(_) => {
                            attempt.finish(lifecycle, "could not persist remote authorization")
                        }
                    }
                }
            }
            Err(DeviceFailure::Expired) => attempt.finish(
                AuthAttemptLifecycle::Expired,
                "authorization attempt expired",
            ),
            Err(DeviceFailure::Canceled) => {
                attempt.finish(AuthAttemptLifecycle::Canceled, "remote changed")
            }
            Err(DeviceFailure::Denied) => attempt.finish(lifecycle, "authorization denied"),
            Err(DeviceFailure::Other) => attempt.finish(lifecycle, "remote authorization failed"),
        }
    }

    pub(super) async fn attempt(&self, id: &str) -> Option<AuthAttemptStatus> {
        let mut attempts = self.attempts.lock().await;
        let attempt = attempts.get_mut(id)?;
        if !self.current(&attempt.key, &attempt.binding) {
            attempt.finish(AuthAttemptLifecycle::Canceled, "remote changed");
        } else if attempt.lifecycle == AuthAttemptLifecycle::Pending
            && attempt.expires_at_ms <= now_ms()
        {
            attempt.finish(
                AuthAttemptLifecycle::Expired,
                "authorization attempt expired",
            );
        }
        Some(AuthAttemptStatus {
            attempt_id: id.to_owned(),
            target: Some(proto_target(&attempt.key)),
            lifecycle: attempt.lifecycle as i32,
            completion_mode: AuthCompletionMode::ServerDevicePoll as i32,
            expires_at_ms: attempt.expires_at_ms,
            failure_reason: attempt.failure.to_owned(),
            ..AuthAttemptStatus::default()
        })
    }

    pub(super) async fn cancel(&self, id: &str) -> bool {
        let mut attempts = self.attempts.lock().await;
        let Some(attempt) = attempts.get_mut(id) else {
            return false;
        };
        if attempt.lifecycle == AuthAttemptLifecycle::Pending {
            attempt.finish(AuthAttemptLifecycle::Canceled, "");
        }
        true
    }

    pub(super) async fn reset(&self, key: &str) -> Result<AuthStatus, Status> {
        let mut attempts = self.attempts.lock().await;
        if let Some(lifetime) = self.route_lifetimes.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key).and_then(|lifetime| lifetime.upgrade())
        {
            lifetime.cancel();
        }
        for attempt in attempts.values_mut().filter(|attempt| attempt.key == key) {
            if matches!(
                attempt.lifecycle,
                AuthAttemptLifecycle::Pending | AuthAttemptLifecycle::Succeeded
            ) {
                attempt.finish(AuthAttemptLifecycle::Canceled, "");
            }
        }
        self.credentials
            .delete(&credential_key(key))
            .await
            .map_err(storage_error)?;
        Ok(unconfigured_status(key))
    }

    pub(super) async fn status(&self, key: &str) -> Result<AuthStatus, Status> {
        let binding = self.identity(key)?;
        let mut status = unconfigured_status(key);
        if let Some(record) = self.read(key).await? {
            self.ensure_current(key, &binding)?;
            if record.reauthorization_required || !record_matches(&record, &binding) {
                status.kind = AuthStatusKind::AuthorizationRequired as i32;
                status.message = "reset remote authorization before authorizing again".to_owned();
            } else {
                status.kind = if record.credential.token.expires_by(now_ms()) {
                    AuthStatusKind::Expired
                } else {
                    AuthStatusKind::OauthConfigured
                } as i32;
                status.expires_at_ms = record.credential.token.expires_at_ms.unwrap_or_default();
                status.can_refresh = record.credential.token.refresh_token.is_some();
                if let Some(id) = record.credential.authority_id.as_deref().and_then(route_id) {
                    status.metadata.insert(waltr::destination::ROUTE_ID.into(), id);
                }
            }
        }
        Ok(status)
    }

    async fn read(&self, key: &str) -> Result<Option<OAuthCredentialRecord>, Status> {
        self.credentials
            .read(&credential_key(key))
            .await
            .map_err(storage_error)
    }

    pub(super) async fn refresh(self: &Arc<Self>, key: &str) -> Result<(), Status> {
        let binding = self.identity(key)?;
        self.usable(key, &binding, true, None).await?;
        Ok(())
    }

    pub(super) async fn lease(
        self: &Arc<Self>,
        key: &str,
        expected_endpoint: &Url,
    ) -> Result<Option<RemoteCredentialLease>, Status> {
        Ok(self.leased(key, expected_endpoint, None).await?.map(|(lease, _)| lease))
    }

    pub(super) async fn route_lease(
        self: &Arc<Self>,
        key: &str,
        endpoint: &Url,
        expected_route_id: &str,
    ) -> Result<(RemoteCredentialLease, Arc<CancellationToken>), Status> {
        self.policy.validate_url(endpoint.as_str())?;
        self.leased(key, endpoint, Some(expected_route_id)).await?
            .and_then(|(lease, lifetime)| Some((lease, lifetime?)))
            .ok_or_else(authorization_required)
    }

    async fn leased(
        self: &Arc<Self>,
        key: &str,
        expected_endpoint: &Url,
        expected_route_id: Option<&str>,
    ) -> Result<Option<(RemoteCredentialLease, Option<Arc<CancellationToken>>)>, Status> {
        let binding = match self.identity(key) {
            Ok(binding) if &binding.endpoint == expected_endpoint => binding,
            Ok(_) => return Ok(None),
            Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
            Err(status) => return Err(status),
        };
        let Some(record) = self.usable(key, &binding, false, expected_route_id).await? else {
            return Ok(None);
        };
        // Serialize publication with local reset and re-read after a concurrent rotation.
        let _attempts = self.attempts.lock().await;
        let current = self.read(key).await?;
        self.ensure_current(key, &binding)?;
        Ok(current
            .filter(|current| same_authorization(&record, current))
            .and_then(lease_from_record)
            .map(|lease| {
                let lifetime = expected_route_id.map(|_| {
                    let mut lifetimes = self.route_lifetimes.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    lifetimes.retain(|_, token| token.strong_count() != 0);
                    if let Some(lifetime) = lifetimes.get(key).and_then(std::sync::Weak::upgrade) {
                        return lifetime;
                    }
                    let lifetime = Arc::new(CancellationToken::new());
                    lifetimes.insert(key.to_owned(), Arc::downgrade(&lifetime));
                    lifetime
                });
                (lease, lifetime)
            }))
    }

    async fn usable(
        self: &Arc<Self>,
        key: &str,
        binding: &BindingIdentity,
        force: bool,
        expected_route_id: Option<&str>,
    ) -> Result<Option<OAuthCredentialRecord>, Status> {
        let Some(record) = self
            .read(key)
            .await?
            .filter(|record| !record.reauthorization_required && record_matches(record, binding))
        else {
            return if force {
                Err(authorization_required())
            } else {
                Ok(None)
            };
        };
        self.ensure_current(key, binding)?;
        if let Some(expected) = expected_route_id
            && record.credential.authority_id.as_deref().and_then(route_id).as_deref() != Some(expected)
        {
            return Err(Status::failed_precondition(
                "Remote authorization changed; reopen the destination",
            ));
        }
        if !force
            && !record
                .credential
                .token
                .expires_by(now_ms().saturating_add(REFRESH_WINDOW_MS))
        {
            return Ok(Some(record));
        }
        if record.credential.token.refresh_token.is_none() {
            return if force {
                Err(Status::failed_precondition(
                    "remote grant does not include a refresh token",
                ))
            } else {
                Ok((!record.credential.token.expires_by(now_ms())).then_some(record))
            };
        }
        let owner = Arc::clone(self);
        let key = key.to_owned();
        let binding = binding.clone();
        // A disconnected caller cannot abandon a claimed credential rotation.
        tokio::spawn(async move { owner.rotate(&key, &binding, record).await })
            .await
            .map_err(|_| Status::unavailable("remote refresh task ended"))?
    }

    async fn rotate(
        &self,
        key: &str,
        binding: &BindingIdentity,
        mut record: OAuthCredentialRecord,
    ) -> Result<Option<OAuthCredentialRecord>, Status> {
        let context = record
            .credential
            .workspace_context()
            .ok_or_else(authorization_required)?;
        let request = tokio::time::timeout(
            REFRESH_TIMEOUT,
            native::RefreshRequest::prepare(&self.policy, context),
        )
        .await
        .map_err(|_| Status::unavailable("remote refresh preparation timed out"))??;
        self.ensure_current(key, binding)?;
        let target = credential_key(key);
        let claim = OAuthRefreshClaim {
            revision: record.revision,
            claim_id: Uuid::new_v4().to_string(),
            lease_expires_at_ms: now_ms().saturating_add(30_000),
        };
        let claim = match self
            .credentials
            .claim_refresh(&target, claim)
            .await
            .map_err(storage_error)?
        {
            OAuthRefreshClaimResult::Current(current) => {
                self.ensure_current(key, binding)?;
                return if same_authorization(&record, &current) {
                    Ok(Some(*current))
                } else {
                    Err(authorization_required())
                };
            }
            OAuthRefreshClaimResult::ReauthorizationRequired => {
                return Err(authorization_required());
            }
            OAuthRefreshClaimResult::Claimed(claim) => claim,
        };
        // Deletion and reauthorization can reuse a row revision; verify authority after claim.
        let current = self.read(key).await?;
        if !self.current(key, binding)
            || !current
                .as_ref()
                .is_some_and(|current| same_authorization(&record, current))
        {
            self.credentials
                .abort_refresh(&target, claim, false)
                .await
                .map_err(storage_error)?;
            return Err(Status::aborted("remote changed during refresh"));
        }
        let prior = record
            .credential
            .token
            .refresh_token
            .as_ref()
            .ok_or_else(authorization_required)?;
        let outcome =
            tokio::time::timeout(REFRESH_TIMEOUT, request.exchange(context, prior.secret()))
                .await
                .unwrap_or_else(|_| Err(native::RefreshExchangeError::outcome_unknown()));
        match outcome {
            Ok(token) if self.current(key, binding) => {
                record.credential.token = token;
                if !self
                    .credentials
                    .commit_refresh(&target, claim, record.credential.clone())
                    .await
                    .map_err(storage_error)?
                {
                    return Err(Status::aborted(
                        "remote authorization changed during refresh",
                    ));
                }
                self.ensure_current(key, binding)?;
                Ok(Some(record))
            }
            outcome => {
                let indeterminate = !matches!(outcome, Err(native::RefreshExchangeError::Connect));
                self.credentials
                    .abort_refresh(&target, claim, indeterminate)
                    .await
                    .map_err(storage_error)?;
                Err(outcome
                    .err()
                    .map(native::RefreshExchangeError::into_status)
                    .unwrap_or_else(|| Status::aborted("remote changed during refresh")))
            }
        }
    }
}

pub(crate) fn route_id(authority: &str) -> Option<String> {
    Uuid::parse_str(authority).ok().map(|id| id.simple().to_string())
}

impl Drop for RemoteAuth {
    fn drop(&mut self) {
        for attempt in self.attempts.get_mut().values() {
            attempt.cancellation.cancel();
        }
    }
}

enum DeviceFailure {
    Denied,
    Expired,
    Canceled,
    Other,
}

async fn poll_device(
    service: &std::sync::Weak<RemoteAuth>,
    id: &str,
    policy: &OutboundHttpPolicy,
    mut device: native::DeviceStart,
) -> Result<OAuthCredential, DeviceFailure> {
    let mut interval = device.interval.min(ATTEMPT_LIMIT);
    loop {
        tokio::time::sleep(interval).await;
        let Some(manager) = service.upgrade() else {
            return Err(DeviceFailure::Canceled);
        };
        if !manager.still_pending(id).await {
            return Err(DeviceFailure::Canceled);
        }
        drop(manager);
        match native::poll(policy, &device.context, &device.device_code)
            .await
            .map_err(|_| DeviceFailure::Other)?
        {
            native::DevicePoll::Pending => {}
            native::DevicePoll::SlowDown => {
                interval = interval.saturating_add(native::STEP).min(ATTEMPT_LIMIT)
            }
            native::DevicePoll::Timeout => interval = interval.saturating_mul(2).min(ATTEMPT_LIMIT),
            native::DevicePoll::Denied => return Err(DeviceFailure::Denied),
            native::DevicePoll::Expired => return Err(DeviceFailure::Expired),
            native::DevicePoll::Authorized(tokens) => {
                let token = tokens
                    .validate_initial(policy, &mut device.context)
                    .await
                    .map_err(|_| DeviceFailure::Other)?;
                return Ok(OAuthCredential {
                    version: OAUTH_CREDENTIAL_VERSION,
                    authority_id: Some(Uuid::now_v7().to_string()),
                    token,
                    context: OAuthCredentialContext::Remote(Box::new(device.context)),
                });
            }
        }
    }
}

fn same_authorization(prior: &OAuthCredentialRecord, current: &OAuthCredentialRecord) -> bool {
    !current.reauthorization_required
        && prior.credential.authority_id == current.credential.authority_id
}

fn lease_from_record(record: OAuthCredentialRecord) -> Option<RemoteCredentialLease> {
    let expiry = record.credential.token.expires_at_ms?;
    (expiry > now_ms()).then_some(RemoteCredentialLease {
        access_token: record.credential.token.access_token.secret().clone(),
        expires_at_ms: expiry,
        authority_id: record.credential.authority_id?,
    })
}

fn record_matches(record: &OAuthCredentialRecord, binding: &BindingIdentity) -> bool {
    record.credential.authority_id.is_some()
        && record.credential.token.expires_at_ms.is_some()
        && record
            .credential
            .workspace_context()
            .is_some_and(|context| binding.admits(context))
}

fn storage_error(_: String) -> Status {
    Status::internal("remote credential storage failed")
}

fn unconfigured_status(key: &str) -> AuthStatus {
    AuthStatus {
        target: Some(proto_target(key)),
        kind: AuthStatusKind::NotConfigured as i32,
        supported_methods: vec![AuthMethod::OauthDevice as i32],
        ..AuthStatus::default()
    }
}

fn authorization_required() -> Status {
    Status::failed_precondition("remote authorization is required")
}

fn credential_key(key: &str) -> String {
    AuthTarget::remote(key).credential_key()
}

pub(super) fn proto_target(key: &str) -> ProtoAuthTarget {
    ProtoAuthTarget {
        target: Some(auth_target::Target::RemoteKey(key.to_owned())),
    }
}

fn duration_ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
