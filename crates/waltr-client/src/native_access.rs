//! Native OIDC Authorization Code with PKCE runtime support.

use std::{
    collections::HashSet,
    fmt, io,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use rand::RngCore;
use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
    time,
};
use url::{Position, Url};

use crate::{AccessAttemptId, ClientCommand, NativeCredentialLease, NativeCredentialProvider};

const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH_SKEW: Duration = Duration::from_mins(1);
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 32 * 1024;
const MAX_CALLBACK_BYTES: usize = 8 * 1024;
const EXTERNAL_REDIRECT_URI: &str = "dev.waltr.app:/oauth2redirect";
const EXTERNAL_CALLBACK_CAPACITY: usize = 1;
const STORE_WARNING: &str =
    "Secure credential storage is unavailable; login lasts only until the app stops";
const DELETE_UNCONFIRMED: &str =
    "Waltr credentials were rejected, but secure credential deletion could not be confirmed";

/// Secret persistence used by native hosts. Implementations must store only these opaque bytes.
#[async_trait]
pub trait NativeRefreshTokenStore: Send + Sync {
    /// Load a refresh token, returning `None` when no credential exists.
    async fn load(&self) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError>;
    /// Atomically replace the durable refresh token.
    async fn store(&self, token: &[u8]) -> Result<(), NativeRefreshTokenStoreError>;
    /// Delete the durable refresh token. Missing credentials count as success.
    async fn delete(&self) -> Result<(), NativeRefreshTokenStoreError>;
}

/// Bounded, secret-free secure-store failure classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativeRefreshTokenStoreError {
    /// The platform secure store could not be accessed.
    Unavailable,
    /// Stored data could not be decrypted or decoded.
    Malformed,
}

/// Intentional process-memory-only store used by non-persistent native hosts.
#[derive(Debug, Default)]
pub struct NonPersistentNativeRefreshTokenStore;

#[async_trait]
impl NativeRefreshTokenStore for NonPersistentNativeRefreshTokenStore {
    async fn load(&self) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
        Ok(None)
    }

    async fn store(&self, _token: &[u8]) -> Result<(), NativeRefreshTokenStoreError> {
        Ok(())
    }

    async fn delete(&self) -> Result<(), NativeRefreshTokenStoreError> {
        Ok(())
    }
}

/// Action accepted by the native access coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeAccessAction {
    /// Begin or retry Waltr login.
    Start,
    /// Cancel the active login.
    Cancel,
    /// Remove in-memory Waltr credentials.
    Logout,
    /// The server terminally rejected the current Waltr bearer credential.
    CredentialRejected,
    /// Shared client accepted a login start and assigned its attempt ID.
    AttemptStarted {
        /// Reducer-issued generation that must accompany terminal commands.
        attempt_id: AccessAttemptId,
    },
    /// Deliver an authorization redirect received by the platform host.
    AuthorizationCallback(NativeAuthorizationCallback),
    /// Request shutdown without aborting an in-flight credential write.
    Shutdown,
}

/// An externally delivered authorization redirect kept opaque at the host boundary.
#[derive(Clone, Eq, PartialEq)]
pub struct NativeAuthorizationCallback(String);

impl NativeAuthorizationCallback {
    /// Wrap a redirect URI received from the operating system.
    #[must_use]
    pub fn new(uri: String) -> Self {
        Self(uri)
    }
}

impl fmt::Debug for NativeAuthorizationCallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("NativeAuthorizationCallback([REDACTED])")
    }
}

/// Explicit native OIDC configuration supplied by a platform host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeAccessConfig {
    /// Waltr API endpoint used for RFC 9728 discovery and resource binding.
    pub endpoint: String,
    /// Public native OIDC client identifier.
    pub client_id: String,
    /// Requested scopes, or an empty list to use all advertised resource scopes.
    pub scopes: Vec<String>,
    /// Optional provider-specific audience request parameter.
    pub audience: Option<String>,
    /// Authorization callback delivery policy.
    pub callback: NativeCallbackPolicy,
}

/// Discovered public configuration and exact issuer for host credential namespacing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeAccessBootstrap {
    /// Configuration suitable for `NativeAccessCoordinator::spawn_with_store`.
    pub config: NativeAccessConfig,
    /// Exact issuer validated against authorization-server discovery.
    pub issuer: String,
}

/// Sanitized failure to discover public native access configuration.
#[derive(Debug)]
pub struct NativeAccessDiscoveryError(String);

impl fmt::Display for NativeAccessDiscoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NativeAccessDiscoveryError {}

/// Discover the server-advertised public native client using verified API metadata.
///
/// # Errors
///
/// Returns an error for invalid endpoints, discovery, or missing/invalid native metadata.
pub async fn discover_native_access_config(
    endpoint: &str,
    callback: NativeCallbackPolicy,
) -> Result<NativeAccessBootstrap, NativeAccessDiscoveryError> {
    async {
        let client = http_client()?;
        let (provider, issuer, native) = discover_with_native(&client, endpoint).await?;
        let native: NativeClientMetadata = serde_json::from_value(native.ok_or_else(|| {
            NativeAuthError("Waltr server does not advertise a native client".to_owned())
        })?)
        .map_err(|_| NativeAuthError("Waltr native client metadata is invalid".to_owned()))?;
        if native.client_id.trim().is_empty()
            || native.client_id.trim() != native.client_id
            || native.client_id.chars().any(char::is_control)
            || native.scopes.is_empty()
            || native.scopes.iter().any(|scope| {
                scope.is_empty() || scope.chars().any(|c| c.is_whitespace() || c.is_control())
            })
            || !["openid", "offline_access"]
                .iter()
                .all(|scope| native.scopes.iter().any(|s| s == scope))
            || !provider
                .scopes_supported
                .iter()
                .all(|scope| native.scopes.contains(scope))
            || native.audience.as_ref().is_some_and(|audience| {
                audience.trim().is_empty()
                    || audience.trim() != audience
                    || audience.chars().any(char::is_control)
            })
        {
            return Err(NativeAuthError(
                "Waltr native client metadata is invalid".to_owned(),
            ));
        }
        if let NativeCallbackPolicy::Loopback(NativeLoopbackPolicy::Fixed(url)) = &callback {
            validate_loopback_url(url)?;
        }
        Ok(NativeAccessBootstrap {
            config: NativeAccessConfig {
                endpoint: endpoint.to_owned(),
                client_id: native.client_id,
                scopes: native.scopes,
                audience: native.audience,
                callback,
            },
            issuer,
        })
    }
    .await
    .map_err(|error: NativeAuthError| NativeAccessDiscoveryError(error.0))
}

#[derive(Deserialize)]
struct NativeClientMetadata {
    client_id: String,
    scopes: Vec<String>,
    audience: Option<String>,
}

/// Authorization callback policy for native access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeCallbackPolicy {
    /// Receive the callback from a desktop loopback listener.
    Loopback(NativeLoopbackPolicy),
    /// Receive the exact `dev.waltr.app:/oauth2redirect` callback from the platform host.
    External,
}

/// Loopback callback policy for desktop native access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeLoopbackPolicy {
    /// Bind an ephemeral port at `127.0.0.1` and use `/callback`.
    Ephemeral,
    /// Bind the port and callback path from this loopback URL.
    Fixed(String),
}

/// Secret-free native access progress suitable for host serialization.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum NativeAccessProgress {
    /// Metadata discovery is in progress.
    Discovering,
    /// The authorization URL is ready and the callback listener is active.
    AwaitingBrowser {
        /// URL the platform should open or display.
        #[serde(skip_serializing, default)]
        authorization_url: String,
        /// Remaining callback lifetime in milliseconds.
        timeout_millis: u64,
    },
    /// The authorization code is being exchanged.
    Exchanging,
    /// The access token is being refreshed.
    Refreshing,
    /// Refresh completed and credentials were atomically replaced.
    RefreshSucceeded,
    /// Secure persistence is unavailable, but process-memory access may continue.
    Warning {
        /// Bounded secret-free warning text.
        warning: String,
    },
    /// Credentials were installed before client reconnection.
    Succeeded,
    /// The flow failed with safe user-facing text.
    Failed {
        /// Safe failure text; never contains token response bodies.
        error: String,
    },
    /// The flow was canceled.
    Canceled,
}

impl fmt::Debug for NativeAccessProgress {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discovering => formatter.write_str("Discovering"),
            Self::AwaitingBrowser { timeout_millis, .. } => formatter
                .debug_struct("AwaitingBrowser")
                .field("authorization_url", &"[REDACTED]")
                .field("timeout_millis", timeout_millis)
                .finish(),
            Self::Exchanging => formatter.write_str("Exchanging"),
            Self::Refreshing => formatter.write_str("Refreshing"),
            Self::RefreshSucceeded => formatter.write_str("RefreshSucceeded"),
            Self::Warning { warning } => formatter
                .debug_struct("Warning")
                .field("warning", warning)
                .finish(),
            Self::Succeeded => formatter.write_str("Succeeded"),
            Self::Failed { error } => formatter
                .debug_struct("Failed")
                .field("error", error)
                .finish(),
            Self::Canceled => formatter.write_str("Canceled"),
        }
    }
}

/// Output emitted by the native access coordinator.
#[derive(Clone, Debug, PartialEq)]
pub enum NativeAccessOutput {
    /// Secret-free progress for presentation and platform effects.
    Progress(NativeAccessProgress),
    /// Shared reducer command that the host must dispatch in order.
    ClientCommand {
        /// Reducer command.
        command: Box<ClientCommand>,
        /// Whether logout presentation must be published before its resulting snapshot.
        logout_barrier: bool,
    },
}

/// Running native access coordinator backed by bounded action and output channels.
#[derive(Debug)]
pub struct NativeAccessCoordinator {
    actions: mpsc::Sender<NativeAccessAction>,
    outputs: mpsc::Receiver<NativeAccessOutput>,
    task: JoinHandle<()>,
    credentials: NativeCredentialProvider,
}

impl NativeAccessCoordinator {
    /// Spawn a coordinator with bounded host-facing channels.
    #[must_use]
    pub fn spawn(
        config: NativeAccessConfig,
        credentials: NativeCredentialProvider,
        capacity: usize,
    ) -> Self {
        Self::spawn_with_store(
            config,
            credentials,
            Arc::new(NonPersistentNativeRefreshTokenStore),
            capacity,
        )
    }

    /// Spawn a coordinator with a host-owned secure refresh-token store.
    #[must_use]
    pub fn spawn_with_store(
        config: NativeAccessConfig,
        credentials: NativeCredentialProvider,
        refresh_store: Arc<dyn NativeRefreshTokenStore>,
        capacity: usize,
    ) -> Self {
        let capacity = capacity.max(1);
        let (actions, action_receiver) = mpsc::channel(capacity);
        let (outputs, output_receiver) = mpsc::channel(capacity);
        let (results, result_receiver) = mpsc::channel(2);
        let task = tokio::spawn(coordinate(
            action_receiver,
            result_receiver,
            CoordinatorContext {
                outputs,
                config,
                credentials: credentials.clone(),
                refresh_store,
                results,
            },
        ));
        Self {
            actions,
            outputs: output_receiver,
            task,
            credentials,
        }
    }

    /// Clone the bounded action sender used by platform and reducer adapters.
    #[must_use]
    pub fn action_sender(&self) -> mpsc::Sender<NativeAccessAction> {
        self.actions.clone()
    }

    /// Receive the next coordinator output.
    pub async fn next_output(&mut self) -> Option<NativeAccessOutput> {
        self.outputs.recv().await
    }

    /// Explicitly stop the coordinator and await credential cleanup.
    pub async fn stop(self) {
        let Self {
            actions,
            mut outputs,
            task,
            credentials,
        } = self;
        outputs.close();
        drop(outputs);
        drop(actions.send(NativeAccessAction::Shutdown).await);
        drop(task.await);
        let _ = credentials.clear_bearer_token();
    }
}

struct LoginTokens {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Duration,
    token_endpoint: Url,
    client_id: String,
}

impl fmt::Debug for LoginTokens {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LoginTokens")
            .field("access_token", &"[REDACTED]")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("expires_in", &self.expires_in)
            .field("token_endpoint", &self.token_endpoint)
            .field("client_id", &self.client_id)
            .finish()
    }
}

#[derive(Debug)]
struct NativeAuthError(String);

impl fmt::Display for NativeAuthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for NativeAuthError {}

impl NativeAuthError {
    fn invalid_grant() -> Self {
        Self("authorization server rejected the refresh credential".to_owned())
    }

    fn is_invalid_grant(&self) -> bool {
        self.0 == "authorization server rejected the refresh credential"
    }
}

impl From<io::Error> for NativeAuthError {
    fn from(error: io::Error) -> Self {
        Self(format!("loopback callback failed: {error}"))
    }
}

#[derive(Deserialize)]
struct ProtectedResourceMetadata {
    resource: String,
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    waltr_native_client: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct AuthorizationServerMetadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    response_types_supported: Vec<String>,
    #[serde(default)]
    grant_types_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    token_type: Option<String>,
}

#[derive(Deserialize)]
struct OAuthErrorResponse {
    error: String,
}

struct AuthorizationRequest {
    url: Url,
    state: String,
    verifier: String,
    redirect_uri: Url,
}

enum CoordinatorResult {
    Restore {
        lease: NativeCredentialLease,
        result: RestoreResult,
    },
    Login {
        attempt_id: AccessAttemptId,
        lease: NativeCredentialLease,
        result: Result<Box<LoginTokens>, NativeAuthError>,
    },
    Lifecycle {
        lease: NativeCredentialLease,
        result: LifecycleResult,
    },
}

enum LifecycleResult {
    Refreshed(Result<Box<LoginTokens>, NativeAuthError>),
    Expired,
}

enum RestoreResult {
    None,
    Refreshed(Result<Box<LoginTokens>, NativeAuthError>),
    Malformed,
    Unavailable,
}

struct CoordinatorContext {
    outputs: mpsc::Sender<NativeAccessOutput>,
    config: NativeAccessConfig,
    credentials: NativeCredentialProvider,
    refresh_store: Arc<dyn NativeRefreshTokenStore>,
    results: mpsc::Sender<CoordinatorResult>,
}

struct OwnedTask<Owner> {
    owner: Owner,
    task: Option<JoinHandle<()>>,
}

impl<Owner> OwnedTask<Owner> {
    fn new(owner: Owner, task: JoinHandle<()>) -> Self {
        Self {
            owner,
            task: Some(task),
        }
    }

    async fn abort(mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            drop(task.await);
        }
    }
}

impl<Owner> Drop for OwnedTask<Owner> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "credential task ownership requires one ordered coordinator loop"
)]
async fn coordinate(
    mut actions: mpsc::Receiver<NativeAccessAction>,
    mut result_receiver: mpsc::Receiver<CoordinatorResult>,
    context: CoordinatorContext,
) {
    let CoordinatorContext {
        outputs,
        config,
        credentials,
        refresh_store,
        results,
    } = context;
    let restore_lease = credentials.begin_credential_lease();
    let restore_results = results.clone();
    let restore_config = config.clone();
    let restore_outputs = outputs.clone();
    let restore_store = Arc::clone(&refresh_store);
    let mut restore_task = Some(OwnedTask::new(
        restore_lease,
        tokio::spawn(async move {
            let result = restore(restore_config, restore_outputs, restore_store).await;
            drop(
                restore_results
                    .send(CoordinatorResult::Restore {
                        lease: restore_lease,
                        result,
                    })
                    .await,
            );
        }),
    ));
    let mut login_task: Option<OwnedTask<AccessAttemptId>> = None;
    let mut lifecycle_task: Option<OwnedTask<NativeCredentialLease>> = None;
    let mut pending_lease = None;
    let mut waiting_for_attempt = false;
    let mut cancel_pending = false;
    let mut active_attempt: Option<(AccessAttemptId, NativeCredentialLease)> = None;
    let mut active_callback = None;
    let mut seen_attempts = HashSet::new();
    // Complete may still be queued while the host cancels its pending reducer attempt.
    let mut login_persistence: Option<LoginPersistence> = None;
    let mut completed_attempt = None;

    loop {
        tokio::select! {
            action = actions.recv() => {
                let Some(action) = action else { break };
                match action {
                    NativeAccessAction::Start => {
                        if login_start_in_progress(
                            waiting_for_attempt,
                            active_attempt.is_some(),
                            login_task.is_some(),
                        ) {
                            continue;
                        }
                        if login_persistence.as_ref().is_some_and(|owned| owned.canceled)
                            && !compensate_login(&outputs, refresh_store.as_ref(), &mut login_persistence).await
                        {
                            continue;
                        }
                        login_persistence = None;
                        completed_attempt = None;
                        abort_owned(&mut restore_task).await;
                        abort_owned(&mut login_task).await;
                        abort_owned(&mut lifecycle_task).await;
                        pending_lease = Some(credentials.begin_credential_lease());
                        waiting_for_attempt = true;
                        cancel_pending = false;
                        active_attempt = None;
                        active_callback = None;
                        if send_command(&outputs, ClientCommand::StartWaltrLogin, false).await.is_err() {
                            break;
                        }
                    }
                    NativeAccessAction::Cancel => {
                        abort_owned(&mut login_task).await;
                        abort_owned(&mut lifecycle_task).await;
                        let _ = credentials.clear_bearer_token();
                        pending_lease = None;
                        active_callback = None;
                        let canceled_attempt = active_attempt.take().map(|(attempt_id, _)| attempt_id).or_else(|| completed_attempt.take());
                        if login_persistence.as_ref().is_some_and(|owned| Some(&owned.attempt_id) == canceled_attempt.as_ref()) {
                            compensate_login(&outputs, refresh_store.as_ref(), &mut login_persistence).await;
                        }
                        send_progress(&outputs, NativeAccessProgress::Canceled).await;
                        if let Some(attempt_id) = canceled_attempt {
                            if send_command(&outputs, ClientCommand::CancelWaltrLogin { attempt_id }, false).await.is_err() {
                                break;
                            }
                        } else if waiting_for_attempt {
                            cancel_pending = true;
                        }
                    }
                    NativeAccessAction::Logout => {
                        abort_owned(&mut restore_task).await;
                        abort_owned(&mut login_task).await;
                        abort_owned(&mut lifecycle_task).await;
                        pending_lease = None;
                        active_callback = None;
                        let _ = credentials.clear_bearer_token();
                        if refresh_store.delete().await.is_err() {
                            send_progress(&outputs, NativeAccessProgress::Failed {
                                error: "Waltr signed out in memory, but secure credential deletion could not be confirmed".to_owned(),
                            }).await;
                        }
                        waiting_for_attempt = false;
                        cancel_pending = false;
                        active_attempt = None;
                        if send_command(&outputs, ClientCommand::LogoutWaltr, true).await.is_err() {
                            break;
                        }
                    }
                    NativeAccessAction::CredentialRejected => {
                        abort_owned(&mut restore_task).await;
                        abort_owned(&mut lifecycle_task).await;
                        let _ = credentials.clear_bearer_token();
                        if refresh_store.delete().await.is_err() {
                            send_progress(&outputs, NativeAccessProgress::Failed {
                                error: "Waltr access was rejected, but secure credential deletion could not be confirmed".to_owned(),
                            }).await;
                        }
                    }
                    NativeAccessAction::AttemptStarted { attempt_id } => {
                        if !seen_attempts.insert(attempt_id.clone()) || !waiting_for_attempt {
                            continue;
                        }
                        waiting_for_attempt = false;
                        if cancel_pending {
                            cancel_pending = false;
                            if send_command(&outputs, ClientCommand::CancelWaltrLogin { attempt_id }, false).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        let Some(lease) = pending_lease.take() else { continue };
                        active_attempt = Some((attempt_id.clone(), lease));
                        let task_attempt = attempt_id.clone();
                        let outputs = outputs.clone();
                        let results = results.clone();
                        let config = config.clone();
                        let callback_receiver = if matches!(config.callback, NativeCallbackPolicy::External) {
                            let (callback_sender, callback_receiver) = mpsc::channel(EXTERNAL_CALLBACK_CAPACITY);
                            active_callback = Some(callback_sender);
                            Some(callback_receiver)
                        } else {
                            active_callback = None;
                            None
                        };
                        let task = tokio::spawn(async move {
                                let result = login(config, outputs, callback_receiver).await.map(Box::new);
                            drop(results.send(CoordinatorResult::Login {
                                attempt_id: task_attempt,
                                lease,
                                result,
                            }).await);
                        });
                        login_task = Some(OwnedTask::new(attempt_id, task));
                    }
                    NativeAccessAction::AuthorizationCallback(callback) => {
                        if let Some(callbacks) = &active_callback {
                            drop(callbacks.send(callback).await);
                        }
                    }
                    NativeAccessAction::Shutdown => break,
                }
            }
            result = result_receiver.recv() => {
                let Some(result) = result else { break };
                match result {
                    CoordinatorResult::Restore { lease, result } => {
                        if restore_task.as_ref().is_none_or(|task| task.owner != lease)
                            || credentials.credential_lease() != lease
                        {
                            continue;
                        }
                        restore_task = None;
                        match result {
                            RestoreResult::None => {}
                            RestoreResult::Unavailable => {
                                send_store_warning(&outputs).await;
                            }
                            RestoreResult::Malformed => {
                                if refresh_store.delete().await.is_err() {
                                    send_progress(&outputs, NativeAccessProgress::Failed {
                                        error: "Malformed secure credentials could not be deleted".to_owned(),
                                    }).await;
                                }
                            }
                            RestoreResult::Refreshed(Ok(tokens)) => {
                                persist_refresh_token(&outputs, refresh_store.as_ref(), tokens.refresh_token.as_deref()).await;
                                if credentials.replace_bearer_token_if_current(lease, &tokens.access_token).unwrap_or(false)
                                {
                                    send_progress(&outputs, NativeAccessProgress::RefreshSucceeded).await;
                                    if send_command(&outputs, ClientCommand::ReprobeWaltrAccess, false).await.is_err() { break; }
                                    lifecycle_task = Some(OwnedTask::new(lease, spawn_lifecycle(
                                        *tokens, lease, results.clone(), outputs.clone(),
                                    )));
                                }
                            }
                            RestoreResult::Refreshed(Err(error)) => {
                                let error = terminal_refresh_error(
                                    refresh_store.as_ref(),
                                    error,
                                ).await;
                                send_progress(&outputs, NativeAccessProgress::Failed {
                                    error,
                                }).await;
                            }
                        }
                    }
                    CoordinatorResult::Login { attempt_id, lease, result } => {
                        if active_attempt.as_ref() != Some(&(attempt_id.clone(), lease)) {
                            continue;
                        }
                        active_callback = None;
                        if login_task.as_ref().is_some_and(|task| task.owner == attempt_id) {
                            login_task = None;
                        }
                        match result {
                            Ok(tokens) => {
                                if credentials.credential_lease() != lease { continue; }
                                let previous = if let Ok(previous) = refresh_store.load().await {
                                    Some(previous)
                                } else {
                                    send_store_warning(&outputs).await;
                                    None
                                };
                                if credentials.credential_lease() != lease { continue; }
                                if let Some(previous) = previous {
                                    login_persistence = Some(LoginPersistence {
                                        attempt_id: attempt_id.clone(), lease, token: tokens.refresh_token.as_ref().map(|token| token.as_bytes().to_vec()), previous, canceled: false,
                                    });
                                    persist_refresh_token(
                                        &outputs,
                                        refresh_store.as_ref(),
                                        tokens.refresh_token.as_deref(),
                                    ).await;
                                }
                                match credentials.replace_bearer_token_if_current(lease, &tokens.access_token) {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        compensate_login(&outputs, refresh_store.as_ref(), &mut login_persistence).await;
                                        continue;
                                    }
                                    Err(error) => {
                                        compensate_login(&outputs, refresh_store.as_ref(), &mut login_persistence).await;
                                        active_attempt = None;
                                        fail_attempt(&outputs, &credentials, (attempt_id, lease), error.to_string()).await;
                                        continue;
                                    }
                                }
                                active_attempt = None;
                                completed_attempt = Some(attempt_id.clone());
                                send_progress(&outputs, NativeAccessProgress::Succeeded).await;
                                // Output closure must not bypass an already queued Cancel of this persisted login.
                                if send_command(&outputs, ClientCommand::CompleteWaltrLogin { attempt_id }, false).await.is_err() { continue; }
                                lifecycle_task = Some(OwnedTask::new(lease, spawn_lifecycle(
                                        *tokens,
                                    lease,
                                    results.clone(),
                                    outputs.clone(),
                                )));
                            }
                            Err(error) => {
                                active_attempt = None;
                                fail_attempt(&outputs, &credentials, (attempt_id, lease), error.to_string()).await;
                            }
                        }
                    }
                    CoordinatorResult::Lifecycle { lease, result } => {
                        if lifecycle_task.as_ref().is_none_or(|task| task.owner != lease)
                            || credentials.credential_lease() != lease
                        {
                            continue;
                        }
                        lifecycle_task = None;
                        match result {
                                LifecycleResult::Refreshed(Ok(tokens)) => {
                                if let Some(owned) = &mut login_persistence
                                    && owned.lease == lease
                                {
                                    owned.token = tokens.refresh_token.as_ref().map(|token| token.as_bytes().to_vec());
                                }
                                persist_refresh_token(
                                    &outputs,
                                    refresh_store.as_ref(),
                                    tokens.refresh_token.as_deref(),
                                ).await;
                                match credentials.replace_bearer_token_if_current(lease, &tokens.access_token) {
                                    Ok(true) => {
                                        send_progress(&outputs, NativeAccessProgress::RefreshSucceeded).await;
                                        lifecycle_task = Some(OwnedTask::new(lease, spawn_lifecycle(
                                                *tokens,
                                            lease,
                                            results.clone(),
                                            outputs.clone(),
                                        )));
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        expire_access(&outputs, &credentials, lease, error.to_string()).await;
                                    }
                                }
                            }
                            LifecycleResult::Refreshed(Err(error)) => {
                                let error = terminal_refresh_error(
                                    refresh_store.as_ref(),
                                    error,
                                ).await;
                                expire_access(&outputs, &credentials, lease, error).await;
                            }
                            LifecycleResult::Expired => {
                                expire_access(
                                    &outputs,
                                    &credentials,
                                    lease,
                                    "Waltr access token expired".to_owned(),
                                ).await;
                            }
                        }
                    }
                }
            }
        }
    }
    abort_owned(&mut restore_task).await;
    abort_owned(&mut login_task).await;
    abort_owned(&mut lifecycle_task).await;
    let _ = credentials.clear_bearer_token();
}

struct LoginPersistence {
    attempt_id: AccessAttemptId,
    lease: NativeCredentialLease,
    token: Option<Vec<u8>>,
    previous: Option<Vec<u8>>,
    canceled: bool,
}

async fn compensate_login(
    outputs: &mpsc::Sender<NativeAccessOutput>,
    store: &dyn NativeRefreshTokenStore,
    owned: &mut Option<LoginPersistence>,
) -> bool {
    let Some(persistence) = owned else {
        return true;
    };
    persistence.canceled = true;
    let removed = match store.load().await {
        Ok(current) if current == persistence.token => match &persistence.previous {
            Some(previous) if current.as_ref() == Some(previous) => true,
            Some(previous) => store.store(previous).await.is_ok(),
            None => store.delete().await.is_ok(),
        },
        Ok(_) => true,
        Err(_) => false,
    };
    if removed {
        *owned = None;
    } else {
        send_progress(
            outputs,
            NativeAccessProgress::Failed {
                error:
                    "Login canceled in memory, but secure credential cleanup could not be confirmed"
                        .to_owned(),
            },
        )
        .await;
    }
    removed
}

const fn login_start_in_progress(
    waiting_for_attempt: bool,
    has_active_attempt: bool,
    has_login_task: bool,
) -> bool {
    waiting_for_attempt || has_active_attempt || has_login_task
}

async fn abort_owned<Owner>(task: &mut Option<OwnedTask<Owner>>) {
    if let Some(task) = task.take() {
        task.abort().await;
    }
}

async fn fail_attempt(
    outputs: &mpsc::Sender<NativeAccessOutput>,
    credentials: &NativeCredentialProvider,
    attempt: (AccessAttemptId, NativeCredentialLease),
    error: String,
) {
    let (attempt_id, lease) = attempt;
    let revision = credentials.credential_revision();
    if !credentials.clear_bearer_token_if_current(lease, revision) {
        return;
    }
    send_progress(
        outputs,
        NativeAccessProgress::Failed {
            error: error.clone(),
        },
    )
    .await;
    drop(
        send_command(
            outputs,
            ClientCommand::FailWaltrLogin { attempt_id, error },
            false,
        )
        .await,
    );
}

async fn expire_access(
    outputs: &mpsc::Sender<NativeAccessOutput>,
    credentials: &NativeCredentialProvider,
    lease: NativeCredentialLease,
    error: String,
) {
    let revision = credentials.credential_revision();
    if !credentials.clear_bearer_token_if_current(lease, revision) {
        return;
    }
    send_progress(
        outputs,
        NativeAccessProgress::Failed {
            error: error.clone(),
        },
    )
    .await;
    drop(send_command(outputs, ClientCommand::WaltrAccessExpired { error }, false).await);
}

async fn send_store_warning(outputs: &mpsc::Sender<NativeAccessOutput>) {
    send_progress(
        outputs,
        NativeAccessProgress::Warning {
            warning: STORE_WARNING.to_owned(),
        },
    )
    .await;
}

async fn terminal_refresh_error(
    store: &dyn NativeRefreshTokenStore,
    error: NativeAuthError,
) -> String {
    if error.is_invalid_grant() && store.delete().await.is_err() {
        DELETE_UNCONFIRMED.to_owned()
    } else {
        error.to_string()
    }
}

async fn persist_refresh_token(
    outputs: &mpsc::Sender<NativeAccessOutput>,
    store: &dyn NativeRefreshTokenStore,
    refresh_token: Option<&str>,
) {
    let result = match refresh_token {
        Some(token) => store.store(token.as_bytes()).await,
        None => store.delete().await,
    };
    if result.is_err() {
        send_store_warning(outputs).await;
    }
}

async fn restore(
    config: NativeAccessConfig,
    outputs: mpsc::Sender<NativeAccessOutput>,
    store: Arc<dyn NativeRefreshTokenStore>,
) -> RestoreResult {
    let bytes = match store.load().await {
        Ok(None) => return RestoreResult::None,
        Ok(Some(bytes)) => bytes,
        Err(NativeRefreshTokenStoreError::Unavailable) => return RestoreResult::Unavailable,
        Err(NativeRefreshTokenStoreError::Malformed) => return RestoreResult::Malformed,
    };
    if bytes.is_empty() || bytes.len() > MAX_TOKEN_BYTES {
        return RestoreResult::Malformed;
    }
    let Ok(refresh_token) = String::from_utf8(bytes) else {
        return RestoreResult::Malformed;
    };
    if config.client_id.trim().is_empty() {
        return RestoreResult::Refreshed(Err(NativeAuthError(
            "native OIDC client ID is required".to_owned(),
        )));
    }
    send_progress(&outputs, NativeAccessProgress::Discovering).await;
    let client = match http_client() {
        Ok(client) => client,
        Err(error) => return RestoreResult::Refreshed(Err(error)),
    };
    let discovered = match discover(&client, &config.endpoint).await {
        Ok(discovered) => discovered,
        Err(error) => return RestoreResult::Refreshed(Err(error)),
    };
    send_progress(&outputs, NativeAccessProgress::Refreshing).await;
    let tokens = LoginTokens {
        access_token: String::new(),
        refresh_token: None,
        expires_in: Duration::ZERO,
        token_endpoint: discovered.token_endpoint,
        client_id: config.client_id,
    };
    RestoreResult::Refreshed(
        refresh_token_request(&tokens, &refresh_token)
            .await
            .map(Box::new),
    )
}

async fn login(
    config: NativeAccessConfig,
    outputs: mpsc::Sender<NativeAccessOutput>,
    external_callbacks: Option<mpsc::Receiver<NativeAuthorizationCallback>>,
) -> Result<LoginTokens, NativeAuthError> {
    if config.client_id.trim().is_empty() {
        return Err(NativeAuthError(
            "native OIDC client ID is required".to_owned(),
        ));
    }
    let client = http_client()?;
    send_progress(&outputs, NativeAccessProgress::Discovering).await;
    let discovered = discover(&client, &config.endpoint).await?;
    let (redirect_uri, callback_source) = match &config.callback {
        NativeCallbackPolicy::Loopback(policy) => {
            let listener = bind_loopback(policy).await?;
            let redirect_uri = loopback_redirect_uri(&listener, policy)?;
            (redirect_uri, CallbackSource::Loopback(listener))
        }
        NativeCallbackPolicy::External => {
            let redirect_uri =
                Url::parse(EXTERNAL_REDIRECT_URI).expect("approved external redirect URI is valid");
            let callbacks = external_callbacks.expect("external callback receiver is configured");
            (redirect_uri, CallbackSource::External(callbacks))
        }
    };
    let request = authorization_request(&discovered, &config, redirect_uri)?;
    send_progress(
        &outputs,
        NativeAccessProgress::AwaitingBrowser {
            authorization_url: request.url.to_string(),
            timeout_millis: u64::try_from(LOGIN_TIMEOUT.as_millis())
                .expect("login timeout fits in milliseconds"),
        },
    )
    .await;

    let callback = time::timeout(
        LOGIN_TIMEOUT,
        receive_callback(callback_source, &request.redirect_uri, &request.state),
    )
    .await
    .map_err(|_| NativeAuthError("Waltr login timed out".to_owned()))??;
    send_progress(&outputs, NativeAccessProgress::Exchanging).await;
    exchange_code(
        &client,
        TokenExchangeRequest {
            token_endpoint: &discovered.token_endpoint,
            client_id: &config.client_id,
            redirect_uri: &request.redirect_uri,
            verifier: &request.verifier,
            code: &callback,
        },
    )
    .await
}

#[derive(Debug)]
struct DiscoveredProvider {
    authorization_endpoint: Url,
    token_endpoint: Url,
    resource: String,
    scopes_supported: Vec<String>,
}

async fn discover(client: &Client, endpoint: &str) -> Result<DiscoveredProvider, NativeAuthError> {
    discover_with_native(client, endpoint)
        .await
        .map(|(provider, _, _)| provider)
}

async fn discover_with_native(
    client: &Client,
    endpoint: &str,
) -> Result<(DiscoveredProvider, String, Option<serde_json::Value>), NativeAuthError> {
    let endpoint = parse_secure_url(endpoint, "Waltr server endpoint")?;
    if endpoint.query().is_some() {
        return Err(NativeAuthError(
            "Waltr server endpoint must not contain a query".to_owned(),
        ));
    }
    let expected_resource = waltr_api_resource(&endpoint);
    let mut protected_url = endpoint.clone();
    protected_url.set_path("/.well-known/oauth-protected-resource");
    protected_url.set_query(None);
    protected_url.set_fragment(None);
    let protected: ProtectedResourceMetadata =
        fetch_json(client, protected_url, MAX_METADATA_BYTES)
            .await
            .map_err(|_| {
                NativeAuthError("Waltr protected-resource metadata discovery failed".to_owned())
            })?;
    let resource = parse_secure_url(&protected.resource, "protected resource")?;
    validate_resource_binding(&resource, &expected_resource)?;
    if protected
        .scopes_supported
        .iter()
        .any(|scope| scope.is_empty() || scope.chars().any(char::is_whitespace))
    {
        return Err(NativeAuthError(
            "protected resource metadata advertises no usable required access scopes".to_owned(),
        ));
    }
    let issuer = protected.authorization_servers.first().ok_or_else(|| {
        NativeAuthError("protected resource metadata has no authorization server".to_owned())
    })?;
    if protected.authorization_servers.len() != 1 {
        return Err(NativeAuthError(
            "protected resource metadata must select exactly one authorization server".to_owned(),
        ));
    }
    let issuer_url = parse_secure_url(issuer, "authorization server issuer")?;
    let metadata = discover_authorization_server(client, &issuer_url).await?;
    if metadata.issuer != *issuer {
        return Err(NativeAuthError(
            "authorization server issuer mismatch".to_owned(),
        ));
    }
    validate_provider_metadata(&metadata)?;
    Ok((
        DiscoveredProvider {
            authorization_endpoint: parse_secure_url(
                &metadata.authorization_endpoint,
                "authorization endpoint",
            )?,
            token_endpoint: parse_secure_url(&metadata.token_endpoint, "token endpoint")?,
            resource: protected.resource,
            scopes_supported: protected.scopes_supported,
        },
        metadata.issuer,
        protected.waltr_native_client,
    ))
}

fn waltr_api_resource(endpoint: &Url) -> Url {
    let mut resource = endpoint.clone();
    let path = endpoint.path().trim_end_matches('/').to_owned();
    resource.set_path(if path.is_empty() { "/" } else { &path });
    resource.set_query(None);
    resource.set_fragment(None);
    resource
}

fn validate_resource_binding(resource: &Url, expected: &Url) -> Result<(), NativeAuthError> {
    if resource.username() != ""
        || resource.password().is_some()
        || resource.query().is_some()
        || resource.fragment().is_some()
        || resource.scheme() != expected.scheme()
        || resource.host_str() != expected.host_str()
        || resource.port_or_known_default() != expected.port_or_known_default()
        || resource.path() != expected.path()
    {
        return Err(NativeAuthError(
            "protected resource metadata does not match the configured Waltr API endpoint"
                .to_owned(),
        ));
    }
    Ok(())
}

async fn discover_authorization_server(
    client: &Client,
    issuer: &Url,
) -> Result<AuthorizationServerMetadata, NativeAuthError> {
    let urls = oidc_discovery_urls(issuer)?;
    let mut response = client.get(urls[0].clone()).send().await.map_err(|_| {
        NativeAuthError("authorization server metadata discovery failed".to_owned())
    })?;
    if response.status() == reqwest::StatusCode::NOT_FOUND && urls.len() == 2 {
        response = client.get(urls[1].clone()).send().await.map_err(|_| {
            NativeAuthError("authorization server metadata discovery failed".to_owned())
        })?;
    }
    parse_json_response(response, MAX_METADATA_BYTES)
        .await
        .map_err(|_| NativeAuthError("authorization server metadata discovery failed".to_owned()))
}

fn oidc_discovery_urls(issuer: &Url) -> Result<Vec<Url>, NativeAuthError> {
    let primary = Url::parse(&format!(
        "{}/.well-known/openid-configuration",
        issuer.as_str().trim_end_matches('/')
    ))
    .map_err(|_| NativeAuthError("authorization server discovery URL is invalid".to_owned()))?;
    let issuer_path = issuer.path();
    let suffix = if issuer_path == "/" { "" } else { issuer_path };
    let fallback = Url::parse(&format!(
        "{}/.well-known/openid-configuration{suffix}",
        &issuer[..Position::BeforePath]
    ))
    .map_err(|_| NativeAuthError("authorization server discovery URL is invalid".to_owned()))?;
    Ok(if primary == fallback {
        vec![primary]
    } else {
        vec![primary, fallback]
    })
}

fn validate_provider_metadata(
    metadata: &AuthorizationServerMetadata,
) -> Result<(), NativeAuthError> {
    if !metadata.response_types_supported.is_empty()
        && !metadata
            .response_types_supported
            .iter()
            .any(|value| value == "code")
    {
        return Err(NativeAuthError(
            "authorization server does not support code responses".to_owned(),
        ));
    }
    if !metadata.grant_types_supported.is_empty()
        && !metadata
            .grant_types_supported
            .iter()
            .any(|value| value == "authorization_code")
    {
        return Err(NativeAuthError(
            "authorization server does not support authorization code grants".to_owned(),
        ));
    }
    if !metadata
        .code_challenge_methods_supported
        .iter()
        .any(|value| value == "S256")
    {
        return Err(NativeAuthError(
            "authorization server does not support PKCE S256".to_owned(),
        ));
    }
    Ok(())
}

fn validate_loopback_url(value: &str) -> Result<Url, NativeAuthError> {
    let url = Url::parse(value)
        .map_err(|_| NativeAuthError("loopback policy must be a loopback URL".to_owned()))?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|port| port == 0)
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(NativeAuthError(
            "loopback policy must use http://127.0.0.1:<port> without query or fragment".to_owned(),
        ));
    }
    Ok(url)
}

async fn bind_loopback(policy: &NativeLoopbackPolicy) -> Result<TcpListener, NativeAuthError> {
    let port = match policy {
        NativeLoopbackPolicy::Ephemeral => 0,
        NativeLoopbackPolicy::Fixed(url) => validate_loopback_url(url)?
            .port()
            .expect("validated loopback port"),
    };
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .map_err(NativeAuthError::from)
}

fn loopback_redirect_uri(
    listener: &TcpListener,
    policy: &NativeLoopbackPolicy,
) -> Result<Url, NativeAuthError> {
    match policy {
        NativeLoopbackPolicy::Ephemeral => Url::parse(&format!(
            "http://127.0.0.1:{}/callback",
            listener.local_addr()?.port()
        ))
        .map_err(|_| NativeAuthError("could not construct loopback redirect URI".to_owned())),
        NativeLoopbackPolicy::Fixed(url) => validate_loopback_url(url),
    }
}

enum CallbackSource {
    Loopback(TcpListener),
    External(mpsc::Receiver<NativeAuthorizationCallback>),
}

fn authorization_request(
    provider: &DiscoveredProvider,
    config: &NativeAccessConfig,
    redirect_uri: Url,
) -> Result<AuthorizationRequest, NativeAuthError> {
    let state = random_urlsafe(32);
    let nonce = random_urlsafe(32);
    let verifier = random_urlsafe(32);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let scopes = if config.scopes.is_empty() {
        let mut scopes = vec!["openid".to_owned()];
        for scope in &provider.scopes_supported {
            if !scopes.contains(scope) {
                scopes.push(scope.clone());
            }
        }
        scopes
    } else {
        let mut scopes = config.scopes.clone();
        if !scopes.iter().any(|scope| scope == "openid") {
            scopes.insert(0, "openid".to_owned());
        }
        scopes
    };
    if !config.scopes.is_empty()
        && provider
            .scopes_supported
            .iter()
            .any(|scope| !config.scopes.contains(scope))
    {
        return Err(NativeAuthError(
            "configured scopes do not contain all required resource scopes".to_owned(),
        ));
    }
    let mut url = provider.authorization_endpoint.clone();
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("response_type", "code")
            .append_pair("client_id", &config.client_id)
            .append_pair("redirect_uri", redirect_uri.as_str())
            .append_pair("scope", &scopes.join(" "))
            .append_pair("state", &state)
            .append_pair("nonce", &nonce)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("resource", &provider.resource);
        if let Some(audience) = &config.audience {
            query.append_pair("audience", audience);
        }
    }
    Ok(AuthorizationRequest {
        url,
        state,
        verifier,
        redirect_uri,
    })
}

fn random_urlsafe(bytes: usize) -> String {
    let mut random = vec![0_u8; bytes];
    rand::rng().fill_bytes(&mut random);
    URL_SAFE_NO_PAD.encode(random)
}

async fn receive_callback(
    source: CallbackSource,
    redirect_uri: &Url,
    expected_state: &str,
) -> Result<String, NativeAuthError> {
    match source {
        CallbackSource::Loopback(listener) => {
            receive_loopback_callback(listener, redirect_uri, expected_state).await
        }
        CallbackSource::External(callbacks) => {
            receive_external_callback(callbacks, redirect_uri, expected_state).await
        }
    }
}

async fn receive_loopback_callback(
    listener: TcpListener,
    redirect_uri: &Url,
    expected_state: &str,
) -> Result<String, NativeAuthError> {
    let authority = format!(
        "{}:{}",
        redirect_uri.host_str().expect("validated redirect host"),
        redirect_uri.port().expect("validated redirect port")
    );
    loop {
        let (mut stream, _) = listener.accept().await?;
        let parsed = time::timeout(
            Duration::from_secs(2),
            read_callback(&mut stream, redirect_uri.path(), &authority, expected_state),
        )
        .await;
        match parsed {
            Ok(Ok(CallbackOutcome::Code(code))) => {
                write_callback_response(
                    &mut stream,
                    "200 OK",
                    "Waltr login complete. You can close this window.",
                )
                .await;
                return Ok(code);
            }
            Ok(Ok(CallbackOutcome::ProviderError(error))) => {
                write_callback_response(
                    &mut stream,
                    "400 Bad Request",
                    "Waltr login failed. Return to the terminal.",
                )
                .await;
                return Err(error);
            }
            Ok(Err(_)) => {
                write_callback_response(
                    &mut stream,
                    "400 Bad Request",
                    "Invalid callback. Return to the authorization page.",
                )
                .await;
            }
            Err(_) => {}
        }
        drop(stream.shutdown().await);
    }
}

async fn receive_external_callback(
    mut callbacks: mpsc::Receiver<NativeAuthorizationCallback>,
    redirect_uri: &Url,
    expected_state: &str,
) -> Result<String, NativeAuthError> {
    while let Some(callback) = callbacks.recv().await {
        match parse_external_callback(&callback.0, redirect_uri, expected_state) {
            Ok(CallbackOutcome::Code(code)) => return Ok(code),
            Ok(CallbackOutcome::ProviderError(error)) => return Err(error),
            Err(_) => {}
        }
    }
    Err(NativeAuthError(
        "authorization callback delivery stopped".to_owned(),
    ))
}

async fn write_callback_response(stream: &mut TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    drop(stream.write_all(response.as_bytes()).await);
    drop(stream.shutdown().await);
}

async fn read_callback(
    stream: &mut TcpStream,
    expected_path: &str,
    expected_authority: &str,
    expected_state: &str,
) -> Result<CallbackOutcome, NativeAuthError> {
    let mut bytes = Vec::with_capacity(1024);
    let mut buffer = [0_u8; 1024];
    loop {
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_CALLBACK_BYTES || bytes.windows(4).any(|window| window == b"\r\n\r\n")
        {
            break;
        }
    }
    parse_loopback_callback(&bytes, expected_path, expected_authority, expected_state)
}

enum CallbackOutcome {
    Code(String),
    ProviderError(NativeAuthError),
}

impl fmt::Debug for CallbackOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(_) => formatter.write_str("Code([REDACTED])"),
            Self::ProviderError(error) => {
                formatter.debug_tuple("ProviderError").field(error).finish()
            }
        }
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "callback validation is a single ordered security boundary"
)]
fn parse_loopback_callback(
    bytes: &[u8],
    expected_path: &str,
    expected_authority: &str,
    expected_state: &str,
) -> Result<CallbackOutcome, NativeAuthError> {
    if bytes.len() > MAX_CALLBACK_BYTES || !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        return Err(NativeAuthError(
            "invalid or oversized loopback callback".to_owned(),
        ));
    }
    let request = std::str::from_utf8(bytes)
        .map_err(|_| NativeAuthError("loopback callback was not valid HTTP".to_owned()))?;
    let mut lines = request.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    if parts.next() != Some("GET") {
        return Err(NativeAuthError(
            "loopback callback must be an HTTP GET".to_owned(),
        ));
    }
    let target = parts
        .next()
        .ok_or_else(|| NativeAuthError("loopback callback target is missing".to_owned()))?;
    if parts.next() != Some("HTTP/1.1") || parts.next().is_some() {
        return Err(NativeAuthError(
            "loopback callback must use HTTP/1.1".to_owned(),
        ));
    }
    let hosts = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("host").then(|| value.trim())
        })
        .collect::<Vec<_>>();
    if hosts.as_slice() != [expected_authority] {
        return Err(NativeAuthError(
            "loopback callback Host did not match".to_owned(),
        ));
    }
    let url = Url::parse(&format!("http://127.0.0.1{target}"))
        .map_err(|_| NativeAuthError("loopback callback target is invalid".to_owned()))?;
    if url.path() != expected_path {
        return Err(NativeAuthError(
            "loopback callback path did not match".to_owned(),
        ));
    }
    validate_callback_query(&url, expected_state)
}

fn parse_external_callback(
    value: &str,
    redirect_uri: &Url,
    expected_state: &str,
) -> Result<CallbackOutcome, NativeAuthError> {
    if value.len() > MAX_CALLBACK_BYTES {
        return Err(NativeAuthError(
            "invalid or oversized authorization callback".to_owned(),
        ));
    }
    let url = Url::parse(value)
        .map_err(|_| NativeAuthError("authorization callback URI is invalid".to_owned()))?;
    if url.scheme() != redirect_uri.scheme()
        || url.username() != ""
        || url.password().is_some()
        || url.host_str().is_some()
        || url.port().is_some()
        || url.path() != redirect_uri.path()
        || url.fragment().is_some()
    {
        return Err(NativeAuthError(
            "authorization callback URI did not match".to_owned(),
        ));
    }
    validate_callback_query(&url, expected_state)
}

fn validate_callback_query(
    url: &Url,
    expected_state: &str,
) -> Result<CallbackOutcome, NativeAuthError> {
    let values = |name: &str| {
        url.query_pairs()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>()
    };
    let states = values("state");
    let state = exactly_one(&states, "state")?
        .ok_or_else(|| NativeAuthError("authorization callback state is missing".to_owned()))?;
    if state.as_bytes() != expected_state.as_bytes() {
        return Err(NativeAuthError(
            "authorization callback state did not match".to_owned(),
        ));
    }
    let errors = values("error");
    let codes = values("code");
    let error = exactly_one(&errors, "error")?;
    let code = exactly_one(&codes, "code")?;
    if error.is_some() && code.is_some() {
        return Err(NativeAuthError(
            "authorization callback contained both code and error".to_owned(),
        ));
    }
    if let Some(error) = error {
        let safe_error: String = error
            .chars()
            .filter(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
            .take(64)
            .collect();
        return Ok(CallbackOutcome::ProviderError(NativeAuthError(format!(
            "authorization provider returned {}",
            if safe_error.is_empty() {
                "an error"
            } else {
                &safe_error
            }
        ))));
    }
    code.filter(|code| !code.is_empty())
        .map(|code| CallbackOutcome::Code(code.to_owned()))
        .ok_or_else(|| NativeAuthError("authorization callback code is missing".to_owned()))
}

fn exactly_one<'a>(values: &'a [String], name: &str) -> Result<Option<&'a str>, NativeAuthError> {
    match values {
        [] => Ok(None),
        [value] => Ok(Some(value)),
        _ => Err(NativeAuthError(format!(
            "authorization callback contained duplicate {name} parameters"
        ))),
    }
}

struct TokenExchangeRequest<'a> {
    token_endpoint: &'a Url,
    client_id: &'a str,
    redirect_uri: &'a Url,
    verifier: &'a str,
    code: &'a str,
}

async fn exchange_code(
    client: &Client,
    request: TokenExchangeRequest<'_>,
) -> Result<LoginTokens, NativeAuthError> {
    let response = client
        .post(request.token_endpoint.clone())
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", request.client_id),
            ("redirect_uri", request.redirect_uri.as_str()),
            ("code_verifier", request.verifier),
            ("code", request.code),
        ])
        .send()
        .await
        .map_err(|_| NativeAuthError("token exchange request failed".to_owned()))?;
    let token = parse_token_response(response).await?;
    let expires_in = token_lifetime(&token.access_token, token.expires_in)?;
    Ok(LoginTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_in,
        token_endpoint: request.token_endpoint.clone(),
        client_id: request.client_id.to_owned(),
    })
}

fn spawn_lifecycle(
    tokens: LoginTokens,
    lease: NativeCredentialLease,
    results: mpsc::Sender<CoordinatorResult>,
    outputs: mpsc::Sender<NativeAccessOutput>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = if let Some(refresh_token) = tokens.refresh_token.as_deref() {
            let advance = REFRESH_SKEW
                .min(tokens.expires_in / 10)
                .max(Duration::from_millis(1));
            time::sleep(tokens.expires_in.saturating_sub(advance)).await;
            send_progress(&outputs, NativeAccessProgress::Refreshing).await;
            LifecycleResult::Refreshed(
                refresh_token_request(&tokens, refresh_token)
                    .await
                    .map(Box::new),
            )
        } else {
            time::sleep(tokens.expires_in).await;
            LifecycleResult::Expired
        };
        drop(
            results
                .send(CoordinatorResult::Lifecycle { lease, result })
                .await,
        );
    })
}

async fn refresh_token_request(
    tokens: &LoginTokens,
    refresh_token: &str,
) -> Result<LoginTokens, NativeAuthError> {
    let client = http_client()?;
    let response = client
        .post(tokens.token_endpoint.clone())
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", tokens.client_id.as_str()),
            ("refresh_token", refresh_token),
        ])
        .send()
        .await
        .map_err(|_| NativeAuthError("token refresh request failed".to_owned()))?;
    let refreshed = parse_token_response(response).await?;
    let expires_in = token_lifetime(&refreshed.access_token, refreshed.expires_in)?;
    Ok(LoginTokens {
        access_token: refreshed.access_token,
        refresh_token: refreshed
            .refresh_token
            .or_else(|| Some(refresh_token.to_owned())),
        expires_in,
        token_endpoint: tokens.token_endpoint.clone(),
        client_id: tokens.client_id.clone(),
    })
}

async fn parse_token_response(response: Response) -> Result<TokenResponse, NativeAuthError> {
    let status = response.status();
    let bytes = bounded_body(response, MAX_TOKEN_BYTES).await?;
    if !status.is_success() {
        if serde_json::from_slice::<OAuthErrorResponse>(&bytes)
            .is_ok_and(|error| error.error == "invalid_grant")
        {
            return Err(NativeAuthError::invalid_grant());
        }
        return Err(NativeAuthError(format!(
            "token endpoint rejected the request ({status})"
        )));
    }
    let token: TokenResponse = serde_json::from_slice(&bytes)
        .map_err(|_| NativeAuthError("token endpoint returned an invalid response".to_owned()))?;
    validate_token_response(token)
}

fn validate_token_response(token: TokenResponse) -> Result<TokenResponse, NativeAuthError> {
    if token.access_token.is_empty() {
        return Err(NativeAuthError(
            "token endpoint returned no access token".to_owned(),
        ));
    }
    if token
        .token_type
        .as_deref()
        .is_none_or(|kind| !kind.eq_ignore_ascii_case("bearer"))
    {
        return Err(NativeAuthError(
            "token endpoint did not return a bearer token".to_owned(),
        ));
    }
    Ok(token)
}

fn token_lifetime(
    access_token: &str,
    expires_in: Option<u64>,
) -> Result<Duration, NativeAuthError> {
    if let Some(seconds) = expires_in {
        if seconds > 0 {
            return Ok(Duration::from_secs(seconds));
        }
        return Err(NativeAuthError(
            "token endpoint returned an unusable expiry".to_owned(),
        ));
    }
    let payload = access_token
        .split('.')
        .nth(1)
        .and_then(|payload| URL_SAFE_NO_PAD.decode(payload).ok())
        .and_then(|payload| serde_json::from_slice::<JwtExpiry>(&payload).ok())
        .ok_or_else(|| NativeAuthError("token endpoint returned no usable expiry".to_owned()))?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| NativeAuthError("system clock is before the Unix epoch".to_owned()))?
        .as_secs();
    let seconds = payload
        .exp
        .checked_sub(now)
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| NativeAuthError("access token is already expired".to_owned()))?;
    Ok(Duration::from_secs(seconds))
}

#[derive(Deserialize)]
struct JwtExpiry {
    exp: u64,
}

async fn fetch_json<T: for<'de> Deserialize<'de>>(
    client: &Client,
    url: Url,
    limit: usize,
) -> Result<T, NativeAuthError> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|_| NativeAuthError("OIDC metadata request failed".to_owned()))?;
    parse_json_response(response, limit).await
}

async fn parse_json_response<T: for<'de> Deserialize<'de>>(
    response: Response,
    limit: usize,
) -> Result<T, NativeAuthError> {
    if !response.status().is_success() {
        return Err(NativeAuthError(
            "OIDC metadata endpoint rejected the request".to_owned(),
        ));
    }
    let bytes = bounded_body(response, limit).await?;
    serde_json::from_slice(&bytes)
        .map_err(|_| NativeAuthError("OIDC metadata response was invalid".to_owned()))
}

async fn bounded_body(mut response: Response, limit: usize) -> Result<Vec<u8>, NativeAuthError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(NativeAuthError(
            "OIDC response exceeded the size limit".to_owned(),
        ));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| NativeAuthError("OIDC response could not be read".to_owned()))?
    {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(NativeAuthError(
                "OIDC response exceeded the size limit".to_owned(),
            ));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_secure_url(value: &str, label: &str) -> Result<Url, NativeAuthError> {
    let url =
        Url::parse(value).map_err(|_| NativeAuthError(format!("{label} is not a valid URL")))?;
    let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && local) {
        return Err(NativeAuthError(format!(
            "{label} must use HTTPS (HTTP is allowed only for local development)"
        )));
    }
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
        return Err(NativeAuthError(format!(
            "{label} contains unsupported URL components"
        )));
    }
    Ok(url)
}

fn http_client() -> Result<Client, NativeAuthError> {
    let tls = webpki_tls_config()?;
    Client::builder()
        .tls_backend_preconfigured(tls)
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| NativeAuthError("could not initialize the OIDC HTTP client".to_owned()))
}

#[cfg(test)]
pub(crate) fn test_http_client() -> Client {
    http_client().expect("native HTTP client should configure")
}

fn webpki_tls_config() -> Result<rustls::ClientConfig, NativeAuthError> {
    let roots = webpki_root_store();
    rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|_| NativeAuthError("could not initialize the OIDC HTTP client".to_owned()))
        .map(|builder| builder.with_root_certificates(roots).with_no_client_auth())
}

fn webpki_root_store() -> rustls::RootCertStore {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots
}

async fn send_progress(outputs: &mpsc::Sender<NativeAccessOutput>, progress: NativeAccessProgress) {
    drop(outputs.send(NativeAccessOutput::Progress(progress)).await);
}

async fn send_command(
    outputs: &mpsc::Sender<NativeAccessOutput>,
    command: ClientCommand,
    logout_barrier: bool,
) -> Result<(), mpsc::error::SendError<NativeAccessOutput>> {
    outputs
        .send(NativeAccessOutput::ClientCommand {
            command: Box::new(command),
            logout_barrier,
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::oneshot;

    #[derive(Debug, Default)]
    struct MockRefreshStore {
        token: Mutex<Option<Vec<u8>>>,
        operations: Mutex<Vec<&'static str>>,
        fail: std::sync::atomic::AtomicBool,
        credential_probe: Mutex<Option<NativeCredentialProvider>>,
        bearer_seen_during_store: Mutex<Option<bool>>,
    }

    #[async_trait]
    impl NativeRefreshTokenStore for MockRefreshStore {
        async fn load(&self) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
            self.operations.lock().unwrap().push("load");
            if self.fail.load(std::sync::atomic::Ordering::Acquire) {
                return Err(NativeRefreshTokenStoreError::Unavailable);
            }
            Ok(self.token.lock().unwrap().clone())
        }

        async fn store(&self, token: &[u8]) -> Result<(), NativeRefreshTokenStoreError> {
            self.operations.lock().unwrap().push("store");
            *self.bearer_seen_during_store.lock().unwrap() = self
                .credential_probe
                .lock()
                .unwrap()
                .as_ref()
                .map(NativeCredentialProvider::has_bearer_token);
            if self.fail.load(std::sync::atomic::Ordering::Acquire) {
                return Err(NativeRefreshTokenStoreError::Unavailable);
            }
            *self.token.lock().unwrap() = Some(token.to_vec());
            Ok(())
        }

        async fn delete(&self) -> Result<(), NativeRefreshTokenStoreError> {
            self.operations.lock().unwrap().push("delete");
            if self.fail.load(std::sync::atomic::Ordering::Acquire) {
                return Err(NativeRefreshTokenStoreError::Unavailable);
            }
            *self.token.lock().unwrap() = None;
            Ok(())
        }
    }

    fn test_config() -> NativeAccessConfig {
        NativeAccessConfig {
            endpoint: "http://127.0.0.1:1".to_owned(),
            client_id: "native".to_owned(),
            scopes: Vec::new(),
            audience: None,
            callback: NativeCallbackPolicy::Loopback(NativeLoopbackPolicy::Ephemeral),
        }
    }

    #[test]
    fn native_http_tls_uses_bundled_isrg_root_x1_compatible_roots() {
        const ISRG_ROOT_X1_SUBJECT: &[u8] = b"1\x0b0\t\x06\x03U\x04\x06\x13\x02US1)0'\x06\x03U\x04\n\x13 Internet Security Research Group1\x150\x13\x06\x03U\x04\x03\x13\x0cISRG Root X1";

        let roots = webpki_root_store();

        assert!(
            roots
                .roots
                .iter()
                .any(|anchor| anchor.subject.as_ref() == ISRG_ROOT_X1_SUBJECT),
            "Mozilla WebPKI roots must contain ISRG Root X1"
        );
    }

    #[test]
    fn pkce_values_and_state_are_urlsafe_and_rfc_length() {
        let provider = DiscoveredProvider {
            authorization_endpoint: Url::parse("https://id.example/authorize").unwrap(),
            token_endpoint: Url::parse("https://id.example/token").unwrap(),
            resource: "https://waltr.example".to_owned(),
            scopes_supported: vec!["openid".to_owned()],
        };
        let request = authorization_request(
            &provider,
            &NativeAccessConfig {
                endpoint: "https://waltr.example".to_owned(),
                client_id: "native".to_owned(),
                scopes: Vec::new(),
                audience: None,
                callback: NativeCallbackPolicy::Loopback(NativeLoopbackPolicy::Ephemeral),
            },
            Url::parse("http://127.0.0.1:1234/callback").unwrap(),
        )
        .unwrap();
        assert_eq!(request.verifier.len(), 43);
        assert_eq!(request.state.len(), 43);
        assert!(
            request
                .verifier
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        );
        let query = request
            .url
            .query_pairs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            query
                .get("code_challenge_method")
                .map(std::convert::AsRef::as_ref),
            Some("S256")
        );
        assert_ne!(
            query.get("code_challenge"),
            Some(&std::borrow::Cow::Borrowed(request.verifier.as_str()))
        );
    }

    #[test]
    fn scopes_include_openid_and_all_advertised_resource_scopes() {
        let provider = DiscoveredProvider {
            authorization_endpoint: Url::parse("https://id.example/authorize").unwrap(),
            token_endpoint: Url::parse("https://id.example/token").unwrap(),
            resource: "https://waltr.example".to_owned(),
            scopes_supported: vec!["waltr.read".to_owned(), "waltr.write".to_owned()],
        };
        let redirect = Url::parse("http://127.0.0.1:1234/callback").unwrap();
        let request = authorization_request(
            &provider,
            &NativeAccessConfig {
                endpoint: "https://waltr.example".to_owned(),
                client_id: "native".to_owned(),
                scopes: Vec::new(),
                audience: None,
                callback: NativeCallbackPolicy::Loopback(NativeLoopbackPolicy::Ephemeral),
            },
            redirect.clone(),
        )
        .unwrap();
        assert_eq!(
            request
                .url
                .query_pairs()
                .find(|(name, _)| name == "scope")
                .unwrap()
                .1,
            "openid waltr.read waltr.write"
        );
        assert_eq!(
            request
                .url
                .query_pairs()
                .find(|(name, _)| name == "resource")
                .unwrap()
                .1,
            "https://waltr.example"
        );

        let error = authorization_request(
            &provider,
            &NativeAccessConfig {
                endpoint: "https://waltr.example".to_owned(),
                client_id: "native".to_owned(),
                scopes: vec!["waltr.read".to_owned()],
                audience: None,
                callback: NativeCallbackPolicy::Loopback(NativeLoopbackPolicy::Ephemeral),
            },
            redirect,
        )
        .err()
        .expect("missing required resource scope should fail");
        assert!(error.to_string().contains("all required resource scopes"));

        let no_required_scopes = DiscoveredProvider {
            authorization_endpoint: provider.authorization_endpoint.clone(),
            token_endpoint: provider.token_endpoint.clone(),
            resource: provider.resource,
            scopes_supported: Vec::new(),
        };
        let request = authorization_request(
            &no_required_scopes,
            &NativeAccessConfig {
                endpoint: "https://waltr.example".to_owned(),
                client_id: "native".to_owned(),
                scopes: vec!["profile".to_owned()],
                audience: None,
                callback: NativeCallbackPolicy::Loopback(NativeLoopbackPolicy::Ephemeral),
            },
            Url::parse("http://127.0.0.1:1234/callback").unwrap(),
        )
        .unwrap();
        assert_eq!(
            request
                .url
                .query_pairs()
                .find(|(name, _)| name == "scope")
                .unwrap()
                .1,
            "openid profile"
        );
    }

    #[test]
    fn oidc_discovery_uses_appended_path_then_prefix_fallback() {
        assert_eq!(
            oidc_discovery_urls(&Url::parse("https://issuer.example").unwrap()).unwrap(),
            [Url::parse("https://issuer.example/.well-known/openid-configuration").unwrap()]
        );
        assert_eq!(
            oidc_discovery_urls(&Url::parse("https://issuer.example/tenant%2Fblue/sub").unwrap())
                .unwrap(),
            [
                Url::parse(
                    "https://issuer.example/tenant%2Fblue/sub/.well-known/openid-configuration"
                )
                .unwrap(),
                Url::parse(
                    "https://issuer.example/.well-known/openid-configuration/tenant%2Fblue/sub"
                )
                .unwrap(),
            ]
        );
    }

    #[test]
    fn callback_parser_validates_state_errors_and_bounds() {
        let valid = b"GET /callback?code=abc&state=expected HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert!(matches!(
            parse_loopback_callback(valid, "/callback", "127.0.0.1", "expected").unwrap(),
            CallbackOutcome::Code(code) if code == "abc"
        ));
        assert!(
            parse_loopback_callback(valid, "/callback", "127.0.0.1", "other")
                .unwrap_err()
                .to_string()
                .contains("state did not match")
        );
        let provider_error =
            b"GET /callback?error=access_denied&state=expected HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert!(matches!(
            parse_loopback_callback(provider_error, "/callback", "127.0.0.1", "expected").unwrap(),
            CallbackOutcome::ProviderError(error) if error.to_string().contains("access_denied")
        ));
        assert!(
            parse_loopback_callback(
                &vec![b'x'; MAX_CALLBACK_BYTES + 1],
                "/callback",
                "127.0.0.1",
                "expected"
            )
            .is_err()
        );
    }

    #[test]
    fn callback_parser_rejects_missing_duplicate_or_wrong_host_and_duplicate_values() {
        let request =
            |query: &str, headers: &str| format!("GET /callback?{query} HTTP/1.1\r\n{headers}\r\n");
        for callback in [
            request("code=abc&state=expected", ""),
            request(
                "code=abc&state=expected",
                "Host: 127.0.0.1:1234\r\nHost: 127.0.0.1:1234\r\n",
            ),
            request("code=abc&state=expected", "Host: 127.0.0.1:9999\r\n"),
            request(
                "code=abc&code=def&state=expected",
                "Host: 127.0.0.1:1234\r\n",
            ),
            request(
                "code=abc&state=expected&state=expected",
                "Host: 127.0.0.1:1234\r\n",
            ),
        ] {
            assert!(
                parse_loopback_callback(
                    callback.as_bytes(),
                    "/callback",
                    "127.0.0.1:1234",
                    "expected"
                )
                .is_err()
            );
        }
    }

    #[test]
    fn external_callback_accepts_only_exact_private_use_uri() {
        let redirect = Url::parse(EXTERNAL_REDIRECT_URI).unwrap();
        assert!(matches!(
            parse_external_callback(
                "dev.waltr.app:/oauth2redirect?code=exact-code&state=exact-state",
                &redirect,
                "exact-state"
            )
            .unwrap(),
            CallbackOutcome::Code(code) if code == "exact-code"
        ));

        for callback in [
            "other.waltr:/oauth2redirect?code=x&state=exact-state",
            "dev.waltr.app://oauth2redirect?code=x&state=exact-state",
            "dev.waltr.app://user@host/oauth2redirect?code=x&state=exact-state",
            "dev.waltr.app://host:123/oauth2redirect?code=x&state=exact-state",
            "dev.waltr.app:/wrong?code=x&state=exact-state",
            "dev.waltr.app:/oauth2redirect?code=x&state=exact-state#fragment",
        ] {
            assert!(
                parse_external_callback(callback, &redirect, "exact-state").is_err(),
                "accepted malformed callback: {callback}"
            );
        }
    }

    #[test]
    fn external_callback_rejects_duplicate_or_conflicting_result_parameters() {
        let redirect = Url::parse(EXTERNAL_REDIRECT_URI).unwrap();
        for query in [
            "code=a&state=s&state=s",
            "code=a&code=b&state=s",
            "error=denied&error=other&state=s",
            "code=a&error=denied&state=s",
        ] {
            assert!(
                parse_external_callback(
                    &format!("{EXTERNAL_REDIRECT_URI}?{query}"),
                    &redirect,
                    "s"
                )
                .is_err(),
                "accepted invalid query: {query}"
            );
        }
        assert!(
            parse_external_callback(
                "dev.waltr.app:/oauth2redirect?code=a&state=wrong",
                &redirect,
                "s"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn external_callback_ignores_invalid_then_accepts_valid() {
        let redirect = Url::parse(EXTERNAL_REDIRECT_URI).unwrap();
        let (callbacks, receiver) = mpsc::channel(EXTERNAL_CALLBACK_CAPACITY);
        let callback =
            tokio::spawn(
                async move { receive_external_callback(receiver, &redirect, "state").await },
            );
        callbacks
            .send(NativeAuthorizationCallback::new(
                "dev.waltr.app:/wrong?code=bad&state=state".to_owned(),
            ))
            .await
            .unwrap();
        callbacks
            .send(NativeAuthorizationCallback::new(
                "dev.waltr.app:/oauth2redirect?code=accepted&state=state".to_owned(),
            ))
            .await
            .unwrap();

        assert_eq!(callback.await.unwrap().unwrap(), "accepted");
        assert!(callbacks.is_closed());
    }

    #[tokio::test]
    async fn external_callback_lane_retains_first_and_rejects_additional_without_blocking() {
        let (callbacks, mut receiver) = mpsc::channel(EXTERNAL_CALLBACK_CAPACITY);
        let first = NativeAuthorizationCallback::new(
            "dev.waltr.app:/oauth2redirect?code=first&state=state".to_owned(),
        );
        let additional = NativeAuthorizationCallback::new(
            "dev.waltr.app:/oauth2redirect?code=additional&state=state".to_owned(),
        );

        callbacks.try_send(first.clone()).unwrap();
        assert!(matches!(
            callbacks.try_send(additional),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        assert_eq!(receiver.recv().await, Some(first));
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn exact_external_provider_error_is_terminal() {
        let redirect = Url::parse(EXTERNAL_REDIRECT_URI).unwrap();
        let (callbacks, receiver) = mpsc::channel(1);
        callbacks
            .send(NativeAuthorizationCallback::new(
                "dev.waltr.app:/oauth2redirect?error=access_denied&state=state".to_owned(),
            ))
            .await
            .unwrap();
        let error = receive_external_callback(receiver, &redirect, "state")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("access_denied"));
    }

    #[tokio::test]
    async fn external_callback_after_receiver_timeout_is_discarded() {
        let redirect = Url::parse(EXTERNAL_REDIRECT_URI).unwrap();
        let (callbacks, receiver) = mpsc::channel(1);
        assert!(
            time::timeout(
                Duration::from_millis(10),
                receive_external_callback(receiver, &redirect, "state")
            )
            .await
            .is_err()
        );
        assert!(
            callbacks
                .try_send(NativeAuthorizationCallback::new(
                    "dev.waltr.app:/oauth2redirect?code=late&state=state".to_owned()
                ))
                .is_err()
        );
    }

    #[test]
    fn endpoint_validation_allows_only_https_or_explicit_local_http() {
        assert!(parse_secure_url("https://waltr.example", "endpoint").is_ok());
        assert!(parse_secure_url("http://127.0.0.1:14333", "endpoint").is_ok());
        assert!(parse_secure_url("http://waltr.example", "endpoint").is_err());
        assert!(validate_loopback_url("http://127.0.0.1:9000/callback").is_ok());
        assert!(validate_loopback_url("http://localhost:9000/callback").is_err());
        assert_eq!(
            waltr_api_resource(&Url::parse("https://waltr.example/base/").unwrap()).as_str(),
            "https://waltr.example/base"
        );
    }

    #[test]
    fn discovery_requires_code_and_s256_but_tolerates_confidential_token_auth_metadata() {
        let mut metadata: AuthorizationServerMetadata = serde_json::from_value(serde_json::json!({
            "issuer": "https://id.example",
            "authorization_endpoint": "https://id.example/authorize",
            "token_endpoint": "https://id.example/token",
            "response_types_supported": ["token"],
            "grant_types_supported": ["authorization_code"],
            "token_endpoint_auth_methods_supported": ["client_secret_post", "client_secret_basic"],
            "code_challenge_methods_supported": ["S256"]
        }))
        .unwrap();
        assert!(
            validate_provider_metadata(&metadata)
                .unwrap_err()
                .to_string()
                .contains("code responses")
        );

        metadata.response_types_supported = vec!["code".to_owned()];
        metadata.grant_types_supported = vec!["implicit".to_owned()];
        assert!(
            validate_provider_metadata(&metadata)
                .unwrap_err()
                .to_string()
                .contains("authorization code grants")
        );

        metadata.grant_types_supported = vec!["authorization_code".to_owned()];
        metadata.code_challenge_methods_supported.clear();
        assert!(
            validate_provider_metadata(&metadata)
                .unwrap_err()
                .to_string()
                .contains("PKCE S256")
        );

        metadata.code_challenge_methods_supported = vec!["S256".to_owned()];
        assert!(validate_provider_metadata(&metadata).is_ok());
    }

    #[tokio::test]
    async fn dropping_listener_releases_ephemeral_port() {
        let listener = bind_loopback(&NativeLoopbackPolicy::Ephemeral)
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        TcpListener::bind(addr)
            .await
            .expect("canceled listener should release port");
    }

    #[tokio::test]
    async fn cancel_and_timeout_release_callback_port() {
        let listener = bind_loopback(&NativeLoopbackPolicy::Ephemeral)
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let redirect = Url::parse(&format!("http://{addr}/callback")).unwrap();
        let callback = tokio::spawn(async move {
            receive_callback(CallbackSource::Loopback(listener), &redirect, "state").await
        });
        callback.abort();
        drop(callback.await);
        TcpListener::bind(addr)
            .await
            .expect("aborted callback should release port");

        let listener = bind_loopback(&NativeLoopbackPolicy::Ephemeral)
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let redirect = Url::parse(&format!("http://{addr}/callback")).unwrap();
        assert!(
            time::timeout(
                Duration::from_millis(10),
                receive_callback(CallbackSource::Loopback(listener), &redirect, "state")
            )
            .await
            .is_err()
        );
        TcpListener::bind(addr)
            .await
            .expect("timed out callback should release port");
    }

    #[tokio::test]
    async fn callback_ignores_irrelevant_connection_then_accepts_valid_callback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let redirect = Url::parse(&format!("http://{addr}/callback")).unwrap();
        let callback = tokio::spawn(async move {
            receive_callback(CallbackSource::Loopback(listener), &redirect, "expected").await
        });

        let mut scanner = TcpStream::connect(addr).await.unwrap();
        scanner
            .write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: wrong.example\r\n\r\n")
            .await
            .unwrap();
        let mut scanner_response = Vec::new();
        scanner.read_to_end(&mut scanner_response).await.unwrap();
        assert!(
            String::from_utf8(scanner_response)
                .unwrap()
                .contains("400 Bad Request")
        );

        let mut browser = TcpStream::connect(addr).await.unwrap();
        browser
            .write_all(
                format!("GET /callback?code=abc&state=expected HTTP/1.1\r\nHost: {addr}\r\n\r\n")
                    .as_bytes(),
            )
            .await
            .unwrap();
        let mut browser_response = Vec::new();
        browser.read_to_end(&mut browser_response).await.unwrap();
        assert!(
            String::from_utf8(browser_response)
                .unwrap()
                .contains("200 OK")
        );
        assert_eq!(callback.await.unwrap().unwrap(), "abc");
    }

    #[tokio::test]
    async fn discovery_uses_only_waltr_api_metadata_and_never_mcp_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server_base = base.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                let path = request.split_whitespace().nth(1).unwrap();
                let body = if path == "/.well-known/oauth-protected-resource" {
                    format!(
                        r#"{{"resource":"{server_base}","authorization_servers":["{server_base}"],"scopes_supported":["waltr:api"]}}"#
                    )
                } else {
                    assert_eq!(path, "/.well-known/openid-configuration");
                    format!(
                        r#"{{"issuer":"{server_base}","authorization_endpoint":"{server_base}/authorize","token_endpoint":"{server_base}/token","response_types_supported":["code"],"grant_types_supported":["authorization_code"],"token_endpoint_auth_methods_supported":["none"],"code_challenge_methods_supported":["S256"]}}"#
                    )
                };
                write_json_response(&mut stream, "200 OK", &body).await;
            }
        });

        let provider = discover(&http_client().unwrap(), &base).await.unwrap();
        assert_eq!(provider.resource, base);
        assert_eq!(provider.scopes_supported, ["waltr:api"]);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn native_bootstrap_requires_valid_public_client_metadata() {
        let valid = serde_json::json!({
            "client_id": "native-public",
            "scopes": ["openid", "offline_access", "waltr:api"],
            "audience": "waltr-api"
        });
        let mut without_audience = valid.clone();
        without_audience.as_object_mut().unwrap().remove("audience");
        let mut cases = vec![
            (Some(valid.clone()), true),
            (Some(without_audience), true),
            (None, false),
            (Some(serde_json::json!({})), false),
            (Some(serde_json::json!("not-an-object")), false),
        ];
        for patch in [
            serde_json::json!({"client_id": ""}),
            serde_json::json!({"client_id": " padded "}),
            serde_json::json!({"client_id": 7}),
            serde_json::json!({"scopes": []}),
            serde_json::json!({"scopes": ["openid", "offline_access"]}),
            serde_json::json!({"scopes": ["openid", "waltr:api"]}),
            serde_json::json!({"scopes": ["openid", "offline_access", "waltr:api", "bad scope"]}),
            serde_json::json!({"audience": ""}),
        ] {
            let mut native = valid.clone();
            native
                .as_object_mut()
                .unwrap()
                .extend(patch.as_object().unwrap().clone());
            cases.push((Some(native), false));
        }
        for (native, succeeds) in cases {
            let expected_audience = native
                .as_ref()
                .and_then(|native| native.get("audience"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let server_base = base.clone();
            let server = tokio::spawn(async move {
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let request = read_http_request(&mut stream).await;
                    let path = request.split_whitespace().nth(1).unwrap();
                    let body = if path == "/.well-known/oauth-protected-resource" {
                        let mut metadata = serde_json::json!({
                            "resource": server_base,
                            "authorization_servers": [server_base],
                            "scopes_supported": ["waltr:api"]
                        });
                        if let Some(native) = &native {
                            metadata["waltr_native_client"] = native.clone();
                        }
                        metadata
                    } else {
                        assert_eq!(path, "/.well-known/openid-configuration");
                        serde_json::json!({
                            "issuer": server_base,
                            "authorization_endpoint": format!("{server_base}/authorize"),
                            "token_endpoint": format!("{server_base}/token"),
                            "response_types_supported": ["code"],
                            "code_challenge_methods_supported": ["S256"]
                        })
                    };
                    write_json_response(&mut stream, "200 OK", &body.to_string()).await;
                }
            });
            let result = discover_native_access_config(&base, NativeCallbackPolicy::External).await;
            assert_eq!(result.is_ok(), succeeds, "{result:?}");
            if let Ok(bootstrap) = result {
                assert_eq!(bootstrap.issuer, base);
                assert_eq!(
                    bootstrap.config,
                    NativeAccessConfig {
                        endpoint: base,
                        client_id: "native-public".to_owned(),
                        scopes: vec![
                            "openid".to_owned(),
                            "offline_access".to_owned(),
                            "waltr:api".to_owned()
                        ],
                        audience: expected_audience,
                        callback: NativeCallbackPolicy::External,
                    }
                );
            }
            server.await.unwrap();
        }
        for endpoint in [
            "http://remote.example",
            "https://user@waltr.example",
            "https://waltr.example?query",
            "not-a-url",
        ] {
            assert!(
                discover_native_access_config(endpoint, NativeCallbackPolicy::External)
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test]
    async fn issuer_path_discovery_falls_back_only_after_not_found() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let issuer = Url::parse(&format!("{base}/tenant")).unwrap();
        let server_base = base.clone();
        let server = tokio::spawn(async move {
            let (mut primary, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut primary).await;
            assert!(request.starts_with("GET /tenant/.well-known/openid-configuration "));
            write_json_response(&mut primary, "404 Not Found", "{}").await;

            let (mut fallback, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut fallback).await;
            assert!(request.starts_with("GET /.well-known/openid-configuration/tenant "));
            write_json_response(
                &mut fallback,
                "200 OK",
                &format!(
                    r#"{{"issuer":"{server_base}/tenant","authorization_endpoint":"{server_base}/authorize","token_endpoint":"{server_base}/token","token_endpoint_auth_methods_supported":["none"],"code_challenge_methods_supported":["S256"]}}"#
                ),
            )
            .await;
        });

        let metadata = discover_authorization_server(&http_client().unwrap(), &issuer)
            .await
            .unwrap();
        assert_eq!(metadata.issuer, format!("{base}/tenant"));
        server.await.unwrap();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let issuer =
            Url::parse(&format!("http://{}/tenant", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut primary, _) = listener.accept().await.unwrap();
            drop(read_http_request(&mut primary).await);
            write_json_response(&mut primary, "500 Internal Server Error", "{}").await;
            assert!(
                time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        assert!(
            discover_authorization_server(&http_client().unwrap(), &issuer)
                .await
                .is_err()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn discovery_rejects_resource_mixup_before_oidc_discovery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server_base = base.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with("GET /.well-known/oauth-protected-resource "));
            write_json_response(
                &mut stream,
                "200 OK",
                &format!(
                    r#"{{"resource":"http://127.0.0.1:9","authorization_servers":["{server_base}"],"scopes_supported":["waltr:api"]}}"#
                ),
            )
            .await;
            assert!(
                time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        let error = discover(&http_client().unwrap(), &base).await.unwrap_err();
        assert!(error.to_string().contains("does not match"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn protected_resource_discovery_does_not_fallback() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.starts_with("GET /.well-known/oauth-protected-resource "));
            write_json_response(&mut stream, "500 Internal Server Error", "{}").await;
            assert!(
                time::timeout(Duration::from_millis(50), listener.accept())
                    .await
                    .is_err()
            );
        });
        assert!(discover(&http_client().unwrap(), &base).await.is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn token_exchange_uses_pkce_as_public_client_and_redacts_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap();
        let (request_sender, request_receiver) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            request_sender.send(request).unwrap();
            write_json_response(
                &mut stream,
                "200 OK",
                r#"{"access_token":"access-secret","refresh_token":"refresh-secret","expires_in":120,"token_type":"Bearer"}"#,
            )
            .await;
        });

        let tokens = exchange_code(
            &http_client().unwrap(),
            TokenExchangeRequest {
                token_endpoint: &endpoint,
                client_id: "native-client",
                redirect_uri: &Url::parse("http://127.0.0.1:1234/callback").unwrap(),
                verifier: "pkce-verifier",
                code: "authorization-code",
            },
        )
        .await
        .unwrap();
        let request = request_receiver.await.unwrap();
        assert!(request.contains("grant_type=authorization_code"));
        assert!(request.contains("client_id=native-client"));
        assert!(request.contains("code_verifier=pkce-verifier"));
        assert!(
            !request
                .to_ascii_lowercase()
                .contains("authorization: basic")
        );
        assert!(!format!("{tokens:?}").contains("access-secret"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refresh_failure_is_safe_and_non_secret() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.contains("grant_type=refresh_token"));
            write_json_response(
                &mut stream,
                "400 Bad Request",
                r#"{"access_token":"must-not-leak"}"#,
            )
            .await;
        });
        let tokens = LoginTokens {
            access_token: "old-secret".to_owned(),
            refresh_token: Some("refresh-secret".to_owned()),
            expires_in: Duration::from_mins(1),
            token_endpoint: endpoint,
            client_id: "native-client".to_owned(),
        };
        let error = refresh_token_request(&tokens, "refresh-secret")
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("must-not-leak"));
        assert!(!error.contains("refresh-secret"));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refresh_success_returns_rotated_bearer_with_new_expiry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.contains("refresh_token=refresh-secret"));
            write_json_response(
                &mut stream,
                "200 OK",
                r#"{"access_token":"new-secret","refresh_token":"next-refresh","expires_in":3600,"token_type":"Bearer"}"#,
            )
            .await;
        });
        let refreshed = refresh_token_request(
            &LoginTokens {
                access_token: "old-secret".to_owned(),
                refresh_token: Some("refresh-secret".to_owned()),
                expires_in: Duration::from_mins(1),
                token_endpoint: endpoint,
                client_id: "native-client".to_owned(),
            },
            "refresh-secret",
        )
        .await
        .unwrap();
        assert_eq!(refreshed.refresh_token.as_deref(), Some("next-refresh"));
        assert_eq!(refreshed.expires_in, Duration::from_hours(1));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn refresh_without_rotation_retains_and_persists_current_refresh_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint =
            Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut stream).await;
            assert!(request.contains("refresh_token=current-refresh"));
            write_json_response(
                &mut stream,
                "200 OK",
                r#"{"access_token":"new-secret","expires_in":3600,"token_type":"Bearer"}"#,
            )
            .await;
        });
        let refreshed = refresh_token_request(
            &LoginTokens {
                access_token: "old-secret".to_owned(),
                refresh_token: Some("current-refresh".to_owned()),
                expires_in: Duration::from_mins(1),
                token_endpoint: endpoint,
                client_id: "native-client".to_owned(),
            },
            "current-refresh",
        )
        .await
        .unwrap();
        assert_eq!(refreshed.refresh_token.as_deref(), Some("current-refresh"));

        let store = MockRefreshStore::default();
        let (outputs, mut output_receiver) = mpsc::channel(1);
        persist_refresh_token(&outputs, &store, refreshed.refresh_token.as_deref()).await;
        assert_eq!(
            store.token.lock().unwrap().as_deref(),
            Some(b"current-refresh".as_slice())
        );
        assert!(output_receiver.try_recv().is_err());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_without_refresh_expires() {
        let credentials = NativeCredentialProvider::new();
        let lease = credentials.begin_credential_lease();
        let (results, mut result_receiver) = mpsc::channel(1);
        let (events, _event_receiver) = mpsc::channel(1);
        let task = spawn_lifecycle(
            LoginTokens {
                access_token: "access-secret".to_owned(),
                refresh_token: None,
                expires_in: Duration::from_millis(10),
                token_endpoint: Url::parse("https://id.example/token").unwrap(),
                client_id: "native".to_owned(),
            },
            lease,
            results,
            events,
        );
        assert!(matches!(
            time::timeout(Duration::from_secs(1), result_receiver.recv())
                .await
                .unwrap()
                .unwrap(),
            CoordinatorResult::Lifecycle {
                result: LifecycleResult::Expired,
                ..
            }
        ));
        task.await.unwrap();
    }

    #[tokio::test]
    async fn repeated_start_is_idempotent_and_cancel_binds_only_first_shared_attempt() {
        let credentials = NativeCredentialProvider::new();
        let mut coordinator = NativeAccessCoordinator::spawn(test_config(), credentials.clone(), 8);
        let actions = coordinator.action_sender();
        actions.send(NativeAccessAction::Start).await.unwrap();
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::ClientCommand {
                command: Box::new(ClientCommand::StartWaltrLogin),
                logout_barrier: false,
            })
        );
        let first_lease = credentials.credential_lease();
        actions.send(NativeAccessAction::Start).await.unwrap();
        assert!(
            time::timeout(Duration::from_millis(20), coordinator.next_output())
                .await
                .is_err()
        );
        assert_eq!(credentials.credential_lease(), first_lease);
        actions.send(NativeAccessAction::Cancel).await.unwrap();
        actions
            .send(NativeAccessAction::AttemptStarted {
                attempt_id: "login-shared".to_owned(),
            })
            .await
            .unwrap();
        actions
            .send(NativeAccessAction::AttemptStarted {
                attempt_id: "login-stale".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(NativeAccessProgress::Canceled))
        );
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::ClientCommand {
                command: Box::new(ClientCommand::CancelWaltrLogin {
                    attempt_id: "login-shared".to_owned(),
                }),
                logout_barrier: false,
            })
        );
        assert!(
            time::timeout(Duration::from_millis(20), coordinator.next_output())
                .await
                .is_err()
        );
        coordinator.stop().await;
    }

    #[tokio::test]
    async fn callback_after_cancel_logout_or_stop_is_ignored_nonblocking() {
        for terminal_action in [NativeAccessAction::Cancel, NativeAccessAction::Logout] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (_stream, _) = listener.accept().await.unwrap();
                std::future::pending::<()>().await;
            });
            let credentials = NativeCredentialProvider::new();
            let mut config = test_config();
            config.endpoint = endpoint;
            config.callback = NativeCallbackPolicy::External;
            let mut coordinator = NativeAccessCoordinator::spawn(config, credentials, 8);
            let actions = coordinator.action_sender();
            actions.send(NativeAccessAction::Start).await.unwrap();
            assert!(matches!(
                coordinator.next_output().await,
                Some(NativeAccessOutput::ClientCommand { command, .. })
                    if *command == ClientCommand::StartWaltrLogin
            ));
            actions
                .send(NativeAccessAction::AttemptStarted {
                    attempt_id: "attempt".to_owned(),
                })
                .await
                .unwrap();
            assert_eq!(
                coordinator.next_output().await,
                Some(NativeAccessOutput::Progress(
                    NativeAccessProgress::Discovering
                ))
            );
            actions.send(terminal_action.clone()).await.unwrap();
            actions
                .try_send(NativeAccessAction::AuthorizationCallback(
                    NativeAuthorizationCallback::new(
                        "dev.waltr.app:/oauth2redirect?code=late&state=stale".to_owned(),
                    ),
                ))
                .expect("host callback delivery must not block");

            if terminal_action == NativeAccessAction::Cancel {
                assert_eq!(
                    coordinator.next_output().await,
                    Some(NativeAccessOutput::Progress(NativeAccessProgress::Canceled))
                );
            }
            let expected = if terminal_action == NativeAccessAction::Logout {
                ClientCommand::LogoutWaltr
            } else {
                ClientCommand::CancelWaltrLogin {
                    attempt_id: "attempt".to_owned(),
                }
            };
            assert!(matches!(
                coordinator.next_output().await,
                Some(NativeAccessOutput::ClientCommand { command, .. }) if *command == expected
            ));
            assert!(
                time::timeout(Duration::from_millis(20), coordinator.next_output())
                    .await
                    .is_err()
            );
            coordinator.stop().await;
            assert!(
                actions
                    .try_send(NativeAccessAction::AuthorizationCallback(
                        NativeAuthorizationCallback::new(
                            "dev.waltr.app:/oauth2redirect?code=stopped&state=stale".to_owned()
                        )
                    ))
                    .is_err()
            );
            server.abort();
            drop(server.await);
        }
    }

    #[tokio::test]
    async fn stale_external_callback_cannot_complete_or_clear_newer_attempt() {
        let (endpoint, mut token_requests, server) = spawn_test_oauth_server().await;
        let credentials = NativeCredentialProvider::new();
        let mut config = test_config();
        config.endpoint = endpoint;
        config.callback = NativeCallbackPolicy::External;
        let mut coordinator = NativeAccessCoordinator::spawn(config, credentials.clone(), 8);
        let actions = coordinator.action_sender();

        let first_url = begin_external_attempt(&mut coordinator, &actions, "first").await;
        let first_state = query_value(&first_url, "state");
        actions.send(NativeAccessAction::Cancel).await.unwrap();
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(NativeAccessProgress::Canceled))
        );
        assert!(matches!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::ClientCommand { command, .. })
                if *command == ClientCommand::CancelWaltrLogin { attempt_id: "first".to_owned() }
        ));

        let second_url = begin_external_attempt(&mut coordinator, &actions, "second").await;
        let second_state = query_value(&second_url, "state");
        assert_ne!(first_state, second_state);
        let second_lease = credentials.credential_lease();
        actions
            .try_send(NativeAccessAction::AuthorizationCallback(
                NativeAuthorizationCallback::new(format!(
                    "{EXTERNAL_REDIRECT_URI}?code=stale-code&state={first_state}"
                )),
            ))
            .unwrap();

        actions
            .try_send(NativeAccessAction::AuthorizationCallback(
                NativeAuthorizationCallback::new(format!(
                    "{EXTERNAL_REDIRECT_URI}?code=accepted-code&state={second_state}"
                )),
            ))
            .unwrap();
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Exchanging
            ))
        );
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Succeeded
            ))
        );
        assert!(matches!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::ClientCommand { command, .. })
                if *command == ClientCommand::CompleteWaltrLogin { attempt_id: "second".to_owned() }
        ));
        assert_eq!(credentials.credential_lease(), second_lease);
        assert!(credentials.has_bearer_token());

        let request = token_requests.recv().await.unwrap();
        let form = request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or_default();
        let fields = url::form_urlencoded::parse(form.as_bytes())
            .into_owned()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            fields.get("redirect_uri").map(String::as_str),
            Some(EXTERNAL_REDIRECT_URI)
        );
        assert_eq!(
            fields.get("code").map(String::as_str),
            Some("accepted-code")
        );
        assert!(
            fields
                .get("code_verifier")
                .is_some_and(|value| value.len() == 43)
        );
        assert!(
            time::timeout(Duration::from_millis(20), token_requests.recv())
                .await
                .is_err()
        );

        actions
            .try_send(NativeAccessAction::AuthorizationCallback(
                NativeAuthorizationCallback::new(format!(
                    "{EXTERNAL_REDIRECT_URI}?code=late-code&state={second_state}"
                )),
            ))
            .unwrap();
        assert!(
            time::timeout(Duration::from_millis(20), coordinator.next_output())
                .await
                .is_err()
        );
        assert_eq!(credentials.credential_lease(), second_lease);
        assert!(credentials.has_bearer_token());

        coordinator.stop().await;
        server.abort();
        drop(server.await);
    }

    #[tokio::test]
    async fn logout_without_native_credential_still_clears_durable_state() {
        let credentials = NativeCredentialProvider::new();
        let mut coordinator = NativeAccessCoordinator::spawn(test_config(), credentials, 1);
        let actions = coordinator.action_sender();

        actions.send(NativeAccessAction::Logout).await.unwrap();
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::ClientCommand {
                command: Box::new(ClientCommand::LogoutWaltr),
                logout_barrier: true,
            })
        );
        coordinator.stop().await;
    }

    #[tokio::test]
    async fn logout_with_credential_queues_ordered_barrier_without_native_event() {
        let credentials = NativeCredentialProvider::new();
        credentials.replace_bearer_token("access-token").unwrap();
        let mut coordinator = NativeAccessCoordinator::spawn(test_config(), credentials.clone(), 1);
        let actions = coordinator.action_sender();

        actions.send(NativeAccessAction::Logout).await.unwrap();
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::ClientCommand {
                command: Box::new(ClientCommand::LogoutWaltr),
                logout_barrier: true,
            })
        );
        assert!(!credentials.has_bearer_token());
        coordinator.stop().await;
    }

    #[tokio::test]
    async fn stop_completes_and_clears_credentials_with_saturated_output() {
        let credentials = NativeCredentialProvider::new();
        let coordinator = NativeAccessCoordinator::spawn(test_config(), credentials.clone(), 1);
        coordinator
            .action_sender()
            .send(NativeAccessAction::Start)
            .await
            .unwrap();
        time::timeout(Duration::from_secs(1), async {
            while coordinator.outputs.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        credentials.replace_bearer_token("access-token").unwrap();

        time::timeout(Duration::from_secs(1), coordinator.stop())
            .await
            .expect("stop must not wait for output capacity");
        assert!(!credentials.has_bearer_token());
    }

    #[tokio::test]
    async fn stop_completes_with_saturated_action_and_output_channels() {
        let credentials = NativeCredentialProvider::new();
        let coordinator = NativeAccessCoordinator::spawn(test_config(), credentials.clone(), 1);
        let actions = coordinator.action_sender();
        actions.send(NativeAccessAction::Start).await.unwrap();
        time::timeout(Duration::from_secs(1), async {
            while coordinator.outputs.len() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        actions.send(NativeAccessAction::Cancel).await.unwrap();
        actions.send(NativeAccessAction::Start).await.unwrap();
        assert_eq!(actions.capacity(), 0);
        credentials.replace_bearer_token("access-token").unwrap();

        time::timeout(Duration::from_secs(1), coordinator.stop())
            .await
            .expect("stop must not wait for action or output capacity");
        assert!(!credentials.has_bearer_token());
    }

    #[tokio::test]
    async fn channel_closure_cleans_up_credentials() {
        let credentials = NativeCredentialProvider::new();
        credentials.replace_bearer_token("access-token").unwrap();
        let coordinator = NativeAccessCoordinator::spawn(test_config(), credentials.clone(), 1);

        drop(coordinator);

        time::timeout(Duration::from_secs(1), async {
            while credentials.has_bearer_token() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("closed host channels must trigger credential cleanup");
    }

    #[test]
    fn repeated_start_is_blocked_for_waiting_active_and_owned_task_states() {
        assert!(!login_start_in_progress(false, false, false));
        assert!(login_start_in_progress(true, false, false));
        assert!(login_start_in_progress(false, true, false));
        assert!(login_start_in_progress(false, false, true));
    }

    #[tokio::test]
    async fn stale_failure_expiry_and_refresh_cannot_replace_or_clear_new_lease() {
        let credentials = NativeCredentialProvider::new();
        let stale = credentials.begin_credential_lease();
        assert!(
            credentials
                .replace_bearer_token_if_current(stale, "old-token")
                .unwrap()
        );
        let current = credentials.begin_credential_lease();
        assert!(
            credentials
                .replace_bearer_token_if_current(current, "new-token")
                .unwrap()
        );
        assert!(
            !credentials
                .replace_bearer_token_if_current(stale, "stale-refresh")
                .unwrap()
        );
        let (outputs, mut output_receiver) = mpsc::channel(1);
        expire_access(&outputs, &credentials, stale, "stale expiry".to_owned()).await;
        assert!(credentials.has_bearer_token());
        assert_eq!(credentials.credential_lease(), current);
        assert!(output_receiver.try_recv().is_err());

        fail_attempt(
            &outputs,
            &credentials,
            ("stale-login".to_owned(), stale),
            "stale login failure".to_owned(),
        )
        .await;
        assert!(credentials.has_bearer_token());
        assert_eq!(credentials.credential_lease(), current);
        assert!(output_receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn current_expiry_clears_credentials_and_dispatches_access_expired() {
        let credentials = NativeCredentialProvider::new();
        let lease = credentials.begin_credential_lease();
        assert!(
            credentials
                .replace_bearer_token_if_current(lease, "access-token")
                .unwrap()
        );
        let (outputs, mut output_receiver) = mpsc::channel(2);
        expire_access(
            &outputs,
            &credentials,
            lease,
            "Waltr access token expired".to_owned(),
        )
        .await;
        assert!(!credentials.has_bearer_token());
        assert!(matches!(
            output_receiver.recv().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Failed { .. }
            ))
        ));
        assert_eq!(
            output_receiver.recv().await,
            Some(NativeAccessOutput::ClientCommand {
                command: Box::new(ClientCommand::WaltrAccessExpired {
                    error: "Waltr access token expired".to_owned(),
                }),
                logout_barrier: false,
            })
        );
    }

    #[test]
    fn bearer_tokens_require_positive_expiry_or_future_jwt_exp() {
        assert!(
            validate_token_response(TokenResponse {
                access_token: "opaque".to_owned(),
                refresh_token: Some("refresh".to_owned()),
                expires_in: Some(60),
                token_type: None,
            })
            .err()
            .expect("missing token type should fail")
            .to_string()
            .contains("bearer")
        );
        assert!(token_lifetime("opaque", None).is_err());
        assert!(token_lifetime("opaque", Some(0)).is_err());
        assert_eq!(
            token_lifetime("opaque", Some(30)).unwrap(),
            Duration::from_secs(30)
        );
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 60;
        let payload = URL_SAFE_NO_PAD.encode(format!(r#"{{"exp":{exp}}}"#));
        let token = format!("header.{payload}.signature");
        assert!(token_lifetime(&token, None).unwrap() <= Duration::from_mins(1));
    }

    #[test]
    fn token_debug_is_redacted() {
        let tokens = LoginTokens {
            access_token: "access-secret".to_owned(),
            refresh_token: Some("refresh-secret".to_owned()),
            expires_in: Duration::from_mins(1),
            token_endpoint: Url::parse("https://id.example/token").unwrap(),
            client_id: "native".to_owned(),
        };
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("access-secret"));
        assert!(!debug.contains("refresh-secret"));
    }

    #[test]
    fn progress_serializes_relative_timeout_without_runtime_clock_state() {
        let progress = NativeAccessProgress::AwaitingBrowser {
            authorization_url: "https://id.example/authorize".to_owned(),
            timeout_millis: 300_000,
        };
        let json = serde_json::to_string(&progress).unwrap();

        assert!(json.contains("timeout_millis"));
        assert!(!json.contains("Instant"));
    }

    #[test]
    fn callback_debug_and_host_outputs_do_not_expose_callback_secrets() {
        let markers = [
            "unique-callback-uri",
            "unique-code",
            "unique-state",
            "unique-nonce",
            "unique-verifier",
            "unique-access",
            "unique-refresh",
        ];
        let callback = NativeAuthorizationCallback::new(format!(
            "dev.waltr.app:/oauth2redirect?code={}&state={}&nonce={}&verifier={}&access={}&refresh={}&marker={}",
            markers[1], markers[2], markers[3], markers[4], markers[5], markers[6], markers[0]
        ));
        let action = NativeAccessAction::AuthorizationCallback(callback);
        let debug = format!("{action:?}");
        let progress = NativeAccessProgress::AwaitingBrowser {
            authorization_url: markers.join("/"),
            timeout_millis: 1,
        };
        let serialized = serde_json::to_string(&progress).unwrap();
        let output = format!("{:?}", NativeAccessOutput::Progress(progress));
        for marker in markers {
            assert!(!debug.contains(marker));
            assert!(!serialized.contains(marker));
            assert!(!output.contains(marker));
        }
        assert!(debug.contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn persistence_stores_rotation_deletes_omission_and_warns_on_failure() {
        let store = MockRefreshStore::default();
        let (outputs, mut output_receiver) = mpsc::channel(2);

        persist_refresh_token(&outputs, &store, Some("rotated-refresh")).await;
        assert_eq!(
            store.token.lock().unwrap().as_deref(),
            Some(b"rotated-refresh".as_slice())
        );
        persist_refresh_token(&outputs, &store, None).await;
        assert!(store.token.lock().unwrap().is_none());
        assert_eq!(*store.operations.lock().unwrap(), ["store", "delete"]);

        store.fail.store(true, std::sync::atomic::Ordering::Release);
        persist_refresh_token(&outputs, &store, Some("never-persisted")).await;
        assert!(matches!(
            output_receiver.recv().await,
            Some(NativeAccessOutput::Progress(NativeAccessProgress::Warning { warning }))
                if warning == STORE_WARNING
        ));
    }

    #[tokio::test]
    async fn intentional_nonpersistent_store_never_reports_unavailability() {
        let store = NonPersistentNativeRefreshTokenStore;

        assert_eq!(store.load().await, Ok(None));
        assert_eq!(store.store(b"process-only-refresh").await, Ok(()));
        assert_eq!(store.delete().await, Ok(()));
    }

    #[tokio::test]
    async fn invalid_grant_delete_failure_is_reported_without_backend_details() {
        let store = MockRefreshStore::default();
        store.fail.store(true, std::sync::atomic::Ordering::Release);

        let error = terminal_refresh_error(&store, NativeAuthError::invalid_grant()).await;

        assert_eq!(error, DELETE_UNCONFIRMED);
        assert!(!error.contains("invalid_grant"));
    }

    #[tokio::test]
    async fn startup_restoration_rediscovers_rotates_and_stop_retains_durable_token() {
        let (endpoint, mut token_requests, server) = spawn_test_oauth_server().await;
        let store = Arc::new(MockRefreshStore::default());
        *store.token.lock().unwrap() = Some(b"stored-refresh".to_vec());
        let credentials = NativeCredentialProvider::new();
        *store.credential_probe.lock().unwrap() = Some(credentials.clone());
        let mut config = test_config();
        config.endpoint = endpoint;
        let mut coordinator = NativeAccessCoordinator::spawn_with_store(
            config,
            credentials.clone(),
            store.clone(),
            8,
        );

        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Discovering
            ))
        );
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Refreshing
            ))
        );
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::RefreshSucceeded
            ))
        );
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::ClientCommand {
                command: Box::new(ClientCommand::ReprobeWaltrAccess),
                logout_barrier: false,
            })
        );
        assert!(credentials.has_bearer_token());
        assert_eq!(*store.bearer_seen_during_store.lock().unwrap(), Some(false));
        let request = token_requests.recv().await.unwrap();
        assert!(request.contains("refresh_token=stored-refresh"));
        assert_eq!(
            store.token.lock().unwrap().as_deref(),
            Some(b"test-refresh".as_slice())
        );

        coordinator.stop().await;
        assert!(!credentials.has_bearer_token());
        assert_eq!(
            store.token.lock().unwrap().as_deref(),
            Some(b"test-refresh".as_slice())
        );
        server.abort();
    }

    #[tokio::test]
    async fn malformed_storage_and_terminal_rejection_delete_durable_state() {
        let store = Arc::new(MockRefreshStore::default());
        *store.token.lock().unwrap() = Some(vec![0xff]);
        let coordinator = NativeAccessCoordinator::spawn_with_store(
            test_config(),
            NativeCredentialProvider::new(),
            store.clone(),
            4,
        );
        time::timeout(Duration::from_secs(1), async {
            while store.token.lock().unwrap().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        coordinator.stop().await;

        *store.token.lock().unwrap() = Some(b"terminal-refresh".to_vec());
        let mut coordinator = NativeAccessCoordinator::spawn_with_store(
            test_config(),
            NativeCredentialProvider::new(),
            store.clone(),
            4,
        );
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Discovering
            ))
        );
        coordinator
            .action_sender()
            .send(NativeAccessAction::CredentialRejected)
            .await
            .unwrap();
        time::timeout(Duration::from_secs(1), async {
            while store.token.lock().unwrap().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        coordinator.stop().await;
    }

    #[tokio::test]
    async fn exact_invalid_grant_is_terminal_but_other_oauth_errors_are_transient() {
        for (body, terminal) in [
            (
                r#"{"error":"invalid_grant","error_description":"secret"}"#,
                true,
            ),
            (r#"{"error":"temporarily_unavailable"}"#, false),
            ("not-json", false),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint =
                Url::parse(&format!("http://{}/token", listener.local_addr().unwrap())).unwrap();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                drop(read_http_request(&mut stream).await);
                write_json_response(&mut stream, "400 Bad Request", body).await;
            });
            let response = http_client().unwrap().post(endpoint).send().await.unwrap();
            let error = match parse_token_response(response).await {
                Ok(_) => panic!("error response must not produce tokens"),
                Err(error) => error,
            };
            assert_eq!(error.is_invalid_grant(), terminal);
            assert!(!error.to_string().contains("secret"));
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn restored_invalid_grant_deletes_durable_token() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server_endpoint = endpoint.clone();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let (status, body) = match path {
                    "/.well-known/oauth-protected-resource" => (
                        "200 OK",
                        format!(
                            r#"{{"resource":"{server_endpoint}","authorization_servers":["{server_endpoint}"]}}"#
                        ),
                    ),
                    "/.well-known/openid-configuration" => (
                        "200 OK",
                        format!(
                            r#"{{"issuer":"{server_endpoint}","authorization_endpoint":"{server_endpoint}/authorize","token_endpoint":"{server_endpoint}/token","code_challenge_methods_supported":["S256"]}}"#
                        ),
                    ),
                    "/token" => ("400 Bad Request", r#"{"error":"invalid_grant"}"#.to_owned()),
                    _ => panic!("unexpected OAuth test request: {path}"),
                };
                write_json_response(&mut stream, status, &body).await;
            }
        });
        let store = Arc::new(MockRefreshStore::default());
        *store.token.lock().unwrap() = Some(b"rejected-refresh".to_vec());
        let mut config = test_config();
        config.endpoint = endpoint;
        let mut coordinator = NativeAccessCoordinator::spawn_with_store(
            config,
            NativeCredentialProvider::new(),
            store.clone(),
            4,
        );
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Discovering
            ))
        );
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Refreshing
            ))
        );
        assert!(matches!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Failed { .. }
            ))
        ));
        assert!(store.token.lock().unwrap().is_none());
        coordinator.stop().await;
        server.await.unwrap();
    }

    #[derive(Debug, Default)]
    struct LoginStore {
        token: std::sync::Mutex<Option<Vec<u8>>>,
        writes: std::sync::atomic::AtomicUsize,
        deletes: std::sync::atomic::AtomicUsize,
        block_first: std::sync::atomic::AtomicBool,
        fail_delete: std::sync::atomic::AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait]
    impl NativeRefreshTokenStore for LoginStore {
        async fn load(&self) -> Result<Option<Vec<u8>>, NativeRefreshTokenStoreError> {
            Ok(self.token.lock().unwrap().clone())
        }
        async fn store(&self, token: &[u8]) -> Result<(), NativeRefreshTokenStoreError> {
            if self
                .writes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
                && self.block_first.load(std::sync::atomic::Ordering::Acquire)
            {
                self.entered.notify_one();
                self.release.notified().await;
            }
            *self.token.lock().unwrap() = Some(token.to_vec());
            Ok(())
        }
        async fn delete(&self) -> Result<(), NativeRefreshTokenStoreError> {
            self.deletes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.fail_delete.load(std::sync::atomic::Ordering::Acquire) {
                return Err(NativeRefreshTokenStoreError::Unavailable);
            }
            *self.token.lock().unwrap() = None;
            Ok(())
        }
    }

    async fn next_login_output(coordinator: &mut NativeAccessCoordinator) -> NativeAccessOutput {
        time::timeout(Duration::from_secs(3), coordinator.next_output())
            .await
            .unwrap()
            .unwrap()
    }

    async fn cancel_outputs(coordinator: &mut NativeAccessCoordinator, attempt: &str) {
        assert_eq!(
            next_login_output(coordinator).await,
            NativeAccessOutput::Progress(NativeAccessProgress::Canceled)
        );
        assert!(
            matches!(next_login_output(coordinator).await, NativeAccessOutput::ClientCommand { command, .. } if *command == ClientCommand::CancelWaltrLogin { attempt_id: attempt.into() })
        );
    }

    async fn callback(actions: &mpsc::Sender<NativeAccessAction>, state: &str, code: &str) {
        actions
            .send(NativeAccessAction::AuthorizationCallback(
                NativeAuthorizationCallback::new(format!(
                    "{EXTERNAL_REDIRECT_URI}?code={code}&state={state}"
                )),
            ))
            .await
            .unwrap();
    }

    async fn login_succeeded(coordinator: &mut NativeAccessCoordinator) {
        loop {
            if next_login_output(coordinator).await
                == NativeAccessOutput::Progress(NativeAccessProgress::Succeeded)
            {
                break;
            }
        }
    }

    #[tokio::test]
    async fn queued_exchange_result_after_host_cancel_cannot_touch_prior_durable_credentials() {
        let (endpoint, _, server) = spawn_test_oauth_server().await;
        let credentials = NativeCredentialProvider::new();
        let store = Arc::new(LoginStore::default());
        let mut config = test_config();
        config.endpoint = endpoint;
        config.callback = NativeCallbackPolicy::External;
        let (actions, receiver) = mpsc::channel(8);
        let (outputs, output_receiver) = mpsc::channel(8);
        let (results, result_receiver) = mpsc::channel(2);
        let task = tokio::spawn(coordinate(
            receiver,
            result_receiver,
            CoordinatorContext {
                outputs,
                config,
                credentials: credentials.clone(),
                refresh_store: store.clone(),
                results: results.clone(),
            },
        ));
        let mut coordinator = NativeAccessCoordinator {
            actions: actions.clone(),
            outputs: output_receiver,
            task,
            credentials: credentials.clone(),
        };
        let url = begin_external_attempt(&mut coordinator, &actions, "canceled").await;
        *store.token.lock().unwrap() = Some(b"prior-attempt-refresh".to_vec());
        let lease = credentials.credential_lease();
        // Both branches become ready without yielding; either selection order must be safe.
        assert!(
            results
                .try_send(CoordinatorResult::Login {
                    attempt_id: "canceled".into(),
                    lease,
                    result: Ok(Box::new(LoginTokens {
                        access_token: "canceled-access".into(),
                        refresh_token: Some("canceled-refresh".into()),
                        expires_in: Duration::from_secs(3600),
                        token_endpoint: url,
                        client_id: "native".into(),
                    }))
                })
                .is_ok()
        );
        let _ = credentials.clear_bearer_token();
        actions.try_send(NativeAccessAction::Cancel).unwrap();
        cancel_outputs(&mut coordinator, "canceled").await;
        assert_eq!(store.writes.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(store.deletes.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            store.token.lock().unwrap().as_deref(),
            Some(b"prior-attempt-refresh".as_slice())
        );
        assert!(!credentials.has_bearer_token());
        coordinator.stop().await;
        server.abort();
    }

    #[tokio::test]
    async fn cancel_during_store_compensates_before_retry_and_only_new_callback_persists() {
        let (endpoint, mut requests, server) = spawn_test_oauth_server().await;
        let credentials = NativeCredentialProvider::new();
        let store = Arc::new(LoginStore::default());
        store
            .block_first
            .store(true, std::sync::atomic::Ordering::Release);
        let mut config = test_config();
        config.endpoint = endpoint;
        config.callback = NativeCallbackPolicy::External;
        let mut coordinator = NativeAccessCoordinator::spawn_with_store(
            config,
            credentials.clone(),
            store.clone(),
            8,
        );
        let actions = coordinator.action_sender();
        let url = begin_external_attempt(&mut coordinator, &actions, "first").await;
        let old_state = query_value(&url, "state");
        callback(&actions, &old_state, "first-code").await;
        time::timeout(Duration::from_secs(3), store.entered.notified())
            .await
            .unwrap();
        let _ = credentials.clear_bearer_token();
        actions.try_send(NativeAccessAction::Cancel).unwrap();
        store.release.notify_one();
        // Exchanging is emitted before the store barrier.
        assert!(matches!(
            next_login_output(&mut coordinator).await,
            NativeAccessOutput::Progress(NativeAccessProgress::Exchanging)
        ));
        cancel_outputs(&mut coordinator, "first").await;
        assert!(store.token.lock().unwrap().is_none());
        assert!(!credentials.has_bearer_token());
        assert_eq!(store.deletes.load(std::sync::atomic::Ordering::SeqCst), 1);
        let next = begin_external_attempt(&mut coordinator, &actions, "second").await;
        callback(&actions, &old_state, "late-code").await;
        callback(&actions, &query_value(&next, "state"), "new-code").await;
        login_succeeded(&mut coordinator).await;
        assert!(
            matches!(next_login_output(&mut coordinator).await, NativeAccessOutput::ClientCommand { command, .. } if *command == ClientCommand::CompleteWaltrLogin { attempt_id: "second".into() })
        );
        assert!(requests.recv().await.unwrap().contains("first-code"));
        assert!(requests.recv().await.unwrap().contains("new-code"));
        assert!(requests.try_recv().is_err());
        assert_eq!(store.writes.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            store.token.lock().unwrap().as_deref(),
            Some(b"test-refresh".as_slice())
        );
        coordinator.stop().await;
        assert!(
            store.token.lock().unwrap().is_some(),
            "ordinary stop retains the successful retry"
        );
        server.abort();
    }

    #[tokio::test]
    async fn cancel_after_complete_is_queued_compensates_and_cleanup_failure_blocks_retry() {
        let (endpoint, _requests, server) = spawn_test_oauth_server().await;
        let credentials = NativeCredentialProvider::new();
        let store = Arc::new(LoginStore::default());
        let mut config = test_config();
        config.endpoint = endpoint;
        config.callback = NativeCallbackPolicy::External;
        let mut coordinator = NativeAccessCoordinator::spawn_with_store(
            config,
            credentials.clone(),
            store.clone(),
            8,
        );
        let actions = coordinator.action_sender();
        let url = begin_external_attempt(&mut coordinator, &actions, "completed").await;
        callback(&actions, &query_value(&url, "state"), "complete-code").await;
        login_succeeded(&mut coordinator).await;
        assert!(
            !coordinator.outputs.is_empty(),
            "Complete is already queued before the host cancellation"
        );
        store
            .fail_delete
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = credentials.clear_bearer_token();
        actions.send(NativeAccessAction::Cancel).await.unwrap();
        assert!(
            matches!(next_login_output(&mut coordinator).await, NativeAccessOutput::ClientCommand { command, .. } if *command == ClientCommand::CompleteWaltrLogin { attempt_id: "completed".into() })
        );
        assert!(
            matches!(next_login_output(&mut coordinator).await, NativeAccessOutput::Progress(NativeAccessProgress::Failed { error }) if error.contains("cleanup could not be confirmed"))
        );
        cancel_outputs(&mut coordinator, "completed").await;
        assert!(!credentials.has_bearer_token());
        assert!(
            store.token.lock().unwrap().is_some(),
            "failure is reported, never claimed as durable deletion"
        );
        actions.send(NativeAccessAction::Start).await.unwrap();
        assert!(matches!(
            next_login_output(&mut coordinator).await,
            NativeAccessOutput::Progress(NativeAccessProgress::Failed { .. })
        ));
        store
            .fail_delete
            .store(false, std::sync::atomic::Ordering::Release);
        actions.send(NativeAccessAction::Start).await.unwrap();
        assert!(
            matches!(next_login_output(&mut coordinator).await, NativeAccessOutput::ClientCommand { command, .. } if *command == ClientCommand::StartWaltrLogin)
        );
        assert!(store.token.lock().unwrap().is_none());
        coordinator.stop().await;
        server.abort();
    }

    #[tokio::test]
    async fn backpressured_success_output_cannot_skip_cancel_compensation_on_stop() {
        for previous in [None, Some(b"previous-refresh".to_vec())] {
            for cancel in [true, false] {
                let (endpoint, _requests, server) = spawn_test_oauth_server().await;
                let credentials = NativeCredentialProvider::new();
                let store = Arc::new(LoginStore::default());
                let mut config = test_config();
                config.endpoint = endpoint;
                config.callback = NativeCallbackPolicy::External;
                let mut coordinator = NativeAccessCoordinator::spawn_with_store(
                    config,
                    credentials.clone(),
                    store.clone(),
                    1,
                );
                let actions = coordinator.action_sender();
                let url = begin_external_attempt(&mut coordinator, &actions, "backpressured").await;
                *store.token.lock().unwrap() = previous.clone();
                callback(&actions, &query_value(&url, "state"), "backpressured-code").await;
                time::timeout(Duration::from_secs(3), async {
                    while !credentials.has_bearer_token() {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                // Exchanging fills the only slot; persistence and bearer installation precede
                // the blocked Succeeded send. Do not drain that slot before stopping.
                assert_eq!(coordinator.outputs.len(), 1);
                assert_eq!(
                    store.token.lock().unwrap().as_deref(),
                    Some(b"test-refresh".as_slice())
                );
                if cancel {
                    let _ = credentials.clear_bearer_token();
                    actions.send(NativeAccessAction::Cancel).await.unwrap();
                }
                time::timeout(Duration::from_secs(3), coordinator.stop())
                    .await
                    .unwrap();
                assert!(!credentials.has_bearer_token());
                let expected = if cancel {
                    previous.clone()
                } else {
                    Some(b"test-refresh".to_vec())
                };
                assert_eq!(*store.token.lock().unwrap(), expected);
                assert_eq!(
                    store.deletes.load(std::sync::atomic::Ordering::SeqCst),
                    usize::from(cancel && expected.is_none())
                );
                server.abort();
            }
        }
    }

    #[tokio::test]
    async fn stop_drains_cancel_and_inflight_store_without_dropping_compensation() {
        let (endpoint, _requests, server) = spawn_test_oauth_server().await;
        let credentials = NativeCredentialProvider::new();
        let store = Arc::new(LoginStore::default());
        store
            .block_first
            .store(true, std::sync::atomic::Ordering::Release);
        let mut config = test_config();
        config.endpoint = endpoint;
        config.callback = NativeCallbackPolicy::External;
        let mut coordinator = NativeAccessCoordinator::spawn_with_store(
            config,
            credentials.clone(),
            store.clone(),
            8,
        );
        let actions = coordinator.action_sender();
        let url = begin_external_attempt(&mut coordinator, &actions, "stopping").await;
        callback(&actions, &query_value(&url, "state"), "stop-code").await;
        time::timeout(Duration::from_secs(3), store.entered.notified())
            .await
            .unwrap();
        let _ = credentials.clear_bearer_token();
        actions.try_send(NativeAccessAction::Cancel).unwrap();
        let stopped = tokio::spawn(coordinator.stop());
        store.release.notify_one();
        time::timeout(Duration::from_secs(3), stopped)
            .await
            .unwrap()
            .unwrap();
        assert!(store.token.lock().unwrap().is_none());
        assert!(!credentials.has_bearer_token());
        assert_eq!(store.deletes.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn cancellation_compensation_preserves_a_different_durable_owner() {
        let store = LoginStore::default();
        *store.token.lock().unwrap() = Some(b"different-attempt".to_vec());
        let credentials = NativeCredentialProvider::new();
        let (outputs, _receiver) = mpsc::channel(1);
        let mut owner = Some(LoginPersistence {
            attempt_id: "canceled".into(),
            lease: credentials.credential_lease(),
            token: Some(b"canceled-refresh".to_vec()),
            previous: None,
            canceled: false,
        });
        assert!(compensate_login(&outputs, &store, &mut owner).await);
        assert!(owner.is_none());
        assert_eq!(store.deletes.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            store.token.lock().unwrap().as_deref(),
            Some(b"different-attempt".as_slice())
        );
    }

    #[tokio::test]
    async fn canceled_persistence_restores_the_previous_attempt_including_reused_token() {
        for candidate in [
            Some(b"canceled-refresh".to_vec()),
            Some(b"previous-refresh".to_vec()),
            None,
        ] {
            let store = LoginStore::default();
            *store.token.lock().unwrap() = candidate.clone();
            let credentials = NativeCredentialProvider::new();
            let (outputs, _receiver) = mpsc::channel(1);
            let mut owner = Some(LoginPersistence {
                attempt_id: "canceled".into(),
                lease: credentials.credential_lease(),
                token: candidate,
                previous: Some(b"previous-refresh".to_vec()),
                canceled: false,
            });
            assert!(compensate_login(&outputs, &store, &mut owner).await);
            assert!(owner.is_none());
            assert_eq!(
                store.token.lock().unwrap().as_deref(),
                Some(b"previous-refresh".as_slice())
            );
            assert_eq!(store.deletes.load(std::sync::atomic::Ordering::SeqCst), 0);
        }
    }

    async fn begin_external_attempt(
        coordinator: &mut NativeAccessCoordinator,
        actions: &mpsc::Sender<NativeAccessAction>,
        attempt_id: &str,
    ) -> Url {
        actions.send(NativeAccessAction::Start).await.unwrap();
        assert!(matches!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::ClientCommand { command, .. })
                if *command == ClientCommand::StartWaltrLogin
        ));
        actions
            .send(NativeAccessAction::AttemptStarted {
                attempt_id: attempt_id.to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            coordinator.next_output().await,
            Some(NativeAccessOutput::Progress(
                NativeAccessProgress::Discovering
            ))
        );
        let Some(NativeAccessOutput::Progress(NativeAccessProgress::AwaitingBrowser {
            authorization_url,
            ..
        })) = coordinator.next_output().await
        else {
            panic!("expected authorization URL");
        };
        Url::parse(&authorization_url).unwrap()
    }

    fn query_value(url: &Url, name: &str) -> String {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .unwrap()
    }

    async fn spawn_test_oauth_server() -> (String, mpsc::Receiver<String>, JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server_endpoint = endpoint.clone();
        let (token_requests, token_receiver) = mpsc::channel(4);
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = read_http_request(&mut stream).await;
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let body = match path {
                    "/.well-known/oauth-protected-resource" => format!(
                        r#"{{"resource":"{server_endpoint}","authorization_servers":["{server_endpoint}"],"scopes_supported":["waltr:api"]}}"#
                    ),
                    "/.well-known/openid-configuration" => format!(
                        r#"{{"issuer":"{server_endpoint}","authorization_endpoint":"{server_endpoint}/authorize","token_endpoint":"{server_endpoint}/token","response_types_supported":["code"],"grant_types_supported":["authorization_code"],"code_challenge_methods_supported":["S256"]}}"#
                    ),
                    "/token" => {
                        token_requests.send(request).await.unwrap();
                        r#"{"access_token":"test-access","refresh_token":"test-refresh","expires_in":3600,"token_type":"Bearer"}"#.to_owned()
                    }
                    _ => panic!("unexpected OAuth test request: {path}"),
                };
                write_json_response(&mut stream, "200 OK", &body).await;
            }
        });
        (endpoint, token_receiver, server)
    }

    async fn read_http_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 1024];
        loop {
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(header_end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length: ")
                            .or_else(|| line.strip_prefix("Content-Length: "))
                    })
                    .and_then(|value| value.parse::<usize>().ok())
                    .unwrap_or(0);
                if bytes.len() >= header_end + 4 + content_length {
                    break;
                }
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    async fn write_json_response(stream: &mut TcpStream, status: &str, body: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }
}
