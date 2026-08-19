//! Native OIDC Authorization Code with PKCE runtime support.

use std::{
    collections::HashSet,
    env, fmt, io,
    net::SocketAddr,
    process::Stdio,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use waltr_client::{
    AccessAttemptId, ClientCommand, NativeCredentialLease, NativeCredentialProvider,
};
use rand::RngCore;
use reqwest::{Client, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
    sync::mpsc,
    task::JoinHandle,
    time,
};
use url::{Position, Url};

use crate::event::{AppEvent, NativeLoginEvent};
use crate::runtime::RuntimeClientCommand;

const CLIENT_ID_ENV: &str = "WALTR_OIDC_NATIVE_CLIENT_ID";
const SCOPES_ENV: &str = "WALTR_OIDC_NATIVE_SCOPES";
const AUDIENCE_ENV: &str = "WALTR_OIDC_NATIVE_AUDIENCE";
const LOOPBACK_POLICY_ENV: &str = "WALTR_OIDC_NATIVE_LOOPBACK_POLICY";
const LOGIN_TIMEOUT: Duration = Duration::from_mins(5);
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
const REFRESH_SKEW: Duration = Duration::from_mins(1);
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_TOKEN_BYTES: usize = 32 * 1024;
const MAX_CALLBACK_BYTES: usize = 8 * 1024;

/// Native side effect requested by terminal input.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NativeAuthAction {
    /// Begin or retry Waltr login.
    Start,
    /// Cancel the active login.
    Cancel,
    /// Copy the currently displayed authorization URL.
    CopyUrl(String),
    /// Remove in-memory Waltr credentials.
    Logout,
    /// Shared client accepted a login start and assigned its attempt ID.
    AttemptStarted {
        /// Reducer-issued generation that must accompany terminal commands.
        attempt_id: AccessAttemptId,
    },
}

#[derive(Clone)]
struct NativeConfig {
    client_id: String,
    scopes: Vec<String>,
    audience: Option<String>,
    loopback: LoopbackPolicy,
}

#[derive(Clone)]
enum LoopbackPolicy {
    Ephemeral,
    Fixed(Url),
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

#[derive(Debug)]
struct AuthorizationRequest {
    url: Url,
    state: String,
    verifier: String,
    redirect_uri: Url,
}

enum CoordinatorResult {
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

struct CoordinatorContext {
    events: mpsc::Sender<io::Result<AppEvent>>,
    commands: mpsc::Sender<RuntimeClientCommand>,
    endpoint: String,
    credentials: NativeCredentialProvider,
    results: mpsc::Sender<CoordinatorResult>,
}

/// Spawn the native authentication coordinator.
#[must_use]
pub(crate) fn spawn(
    events: mpsc::Sender<io::Result<AppEvent>>,
    commands: mpsc::Sender<RuntimeClientCommand>,
    actions: mpsc::Receiver<NativeAuthAction>,
    endpoint: String,
    credentials: NativeCredentialProvider,
) -> JoinHandle<()> {
    let (results, result_receiver) = mpsc::channel(2);
    tokio::spawn(coordinate(
        actions,
        result_receiver,
        CoordinatorContext {
            events,
            commands,
            endpoint,
            credentials,
            results,
        },
    ))
}

#[allow(
    clippy::too_many_lines,
    reason = "credential task ownership requires one ordered coordinator loop"
)]
async fn coordinate(
    mut actions: mpsc::Receiver<NativeAuthAction>,
    mut result_receiver: mpsc::Receiver<CoordinatorResult>,
    context: CoordinatorContext,
) {
    let CoordinatorContext {
        events,
        commands,
        endpoint,
        credentials,
        results,
    } = context;
    let mut login_task: Option<(AccessAttemptId, JoinHandle<()>)> = None;
    let mut lifecycle_task: Option<(NativeCredentialLease, JoinHandle<()>)> = None;
    let mut pending_lease = None;
    let mut waiting_for_attempt = false;
    let mut cancel_pending = false;
    let mut active_attempt: Option<(AccessAttemptId, NativeCredentialLease)> = None;
    let mut seen_attempts = HashSet::new();

    loop {
        tokio::select! {
            action = actions.recv() => {
                let Some(action) = action else { break };
                match action {
                    NativeAuthAction::Start => {
                        if login_start_in_progress(
                            waiting_for_attempt,
                            active_attempt.is_some(),
                            login_task.is_some(),
                        ) {
                            continue;
                        }
                        abort_owned(&mut login_task).await;
                        abort_owned(&mut lifecycle_task).await;
                        pending_lease = Some(credentials.begin_credential_lease());
                        waiting_for_attempt = true;
                        cancel_pending = false;
                        active_attempt = None;
                        if commands.send(RuntimeClientCommand::dispatch(ClientCommand::StartWaltrLogin)).await.is_err() {
                            break;
                        }
                    }
                    NativeAuthAction::Cancel => {
                        abort_owned(&mut login_task).await;
                        abort_owned(&mut lifecycle_task).await;
                        let _ = credentials.clear_bearer_token();
                        pending_lease = None;
                        send_event(&events, NativeLoginEvent::Canceled).await;
                        if let Some((attempt_id, _)) = active_attempt.take() {
                            if commands.send(RuntimeClientCommand::dispatch(ClientCommand::CancelWaltrLogin { attempt_id })).await.is_err() {
                                break;
                            }
                        } else if waiting_for_attempt {
                            cancel_pending = true;
                        }
                    }
                    NativeAuthAction::CopyUrl(url) => {
                        let copied = copy_to_clipboard(&url).await;
                        send_event(&events, NativeLoginEvent::UrlCopyFinished { copied }).await;
                    }
                    NativeAuthAction::Logout => {
                        if !credentials.has_bearer_token() {
                            continue;
                        }
                        abort_owned(&mut login_task).await;
                        abort_owned(&mut lifecycle_task).await;
                        pending_lease = None;
                        waiting_for_attempt = false;
                        cancel_pending = false;
                        active_attempt = None;
                        let _ = credentials.clear_bearer_token();
                        if commands.send(RuntimeClientCommand::logout()).await.is_err() {
                            break;
                        }
                    }
                    NativeAuthAction::AttemptStarted { attempt_id } => {
                        if !seen_attempts.insert(attempt_id.clone()) || !waiting_for_attempt {
                            continue;
                        }
                        waiting_for_attempt = false;
                        if cancel_pending {
                            cancel_pending = false;
                            if commands.send(RuntimeClientCommand::dispatch(ClientCommand::CancelWaltrLogin { attempt_id })).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        let Some(lease) = pending_lease.take() else { continue };
                        active_attempt = Some((attempt_id.clone(), lease));
                        let task_attempt = attempt_id.clone();
                        let events = events.clone();
                        let results = results.clone();
                        let endpoint = endpoint.clone();
                        let task = tokio::spawn(async move {
                                let result = login(endpoint, events).await.map(Box::new);
                            drop(results.send(CoordinatorResult::Login {
                                attempt_id: task_attempt,
                                lease,
                                result,
                            }).await);
                        });
                        login_task = Some((attempt_id, task));
                    }
                }
            }
            result = result_receiver.recv() => {
                let Some(result) = result else { break };
                match result {
                    CoordinatorResult::Login { attempt_id, lease, result } => {
                        if active_attempt.as_ref() != Some(&(attempt_id.clone(), lease)) {
                            continue;
                        }
                        active_attempt = None;
                        if login_task.as_ref().is_some_and(|(owned_attempt, _)| owned_attempt == &attempt_id) {
                            login_task = None;
                        }
                        match result {
                            Ok(tokens) => {
                                match credentials.replace_bearer_token_if_current(lease, &tokens.access_token) {
                                    Ok(true) => {}
                                    Ok(false) => continue,
                                    Err(error) => {
                                            fail_attempt(&events, &commands, &credentials, (attempt_id, lease), error.to_string()).await;
                                        continue;
                                    }
                                }
                                send_event(&events, NativeLoginEvent::Succeeded).await;
                                if commands.send(RuntimeClientCommand::dispatch(ClientCommand::CompleteWaltrLogin { attempt_id })).await.is_err() { break; }
                                lifecycle_task = Some((lease, spawn_lifecycle(
                                        *tokens,
                                    lease,
                                    results.clone(),
                                    events.clone(),
                                )));
                            }
                            Err(error) => {
                                    fail_attempt(&events, &commands, &credentials, (attempt_id, lease), error.to_string()).await;
                            }
                        }
                    }
                    CoordinatorResult::Lifecycle { lease, result } => {
                        if lifecycle_task.as_ref().is_none_or(|(owned_lease, _)| *owned_lease != lease)
                            || credentials.credential_lease() != lease
                        {
                            continue;
                        }
                        lifecycle_task = None;
                        match result {
                                LifecycleResult::Refreshed(Ok(tokens)) => {
                                match credentials.replace_bearer_token_if_current(lease, &tokens.access_token) {
                                    Ok(true) => {
                                        send_event(&events, NativeLoginEvent::RefreshSucceeded).await;
                                        lifecycle_task = Some((lease, spawn_lifecycle(
                                                *tokens,
                                            lease,
                                            results.clone(),
                                            events.clone(),
                                        )));
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        expire_access(&events, &commands, &credentials, lease, error.to_string()).await;
                                    }
                                }
                            }
                            LifecycleResult::Refreshed(Err(error)) => {
                                expire_access(&events, &commands, &credentials, lease, error.to_string()).await;
                            }
                            LifecycleResult::Expired => {
                                expire_access(
                                    &events,
                                    &commands,
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
    abort_owned(&mut login_task).await;
    abort_owned(&mut lifecycle_task).await;
    let _ = credentials.clear_bearer_token();
}

const fn login_start_in_progress(
    waiting_for_attempt: bool,
    has_active_attempt: bool,
    has_login_task: bool,
) -> bool {
    waiting_for_attempt || has_active_attempt || has_login_task
}

async fn abort_owned<Owner>(task: &mut Option<(Owner, JoinHandle<()>)>) {
    if let Some((_, task)) = task.take() {
        task.abort();
        drop(task.await);
    }
}

async fn fail_attempt(
    events: &mpsc::Sender<io::Result<AppEvent>>,
    commands: &mpsc::Sender<RuntimeClientCommand>,
    credentials: &NativeCredentialProvider,
    attempt: (AccessAttemptId, NativeCredentialLease),
    error: String,
) {
    let (attempt_id, lease) = attempt;
    let revision = credentials.credential_revision();
    if !credentials.clear_bearer_token_if_current(lease, revision) {
        return;
    }
    send_event(
        events,
        NativeLoginEvent::Failed {
            error: error.clone(),
        },
    )
    .await;
    drop(
        commands
            .send(RuntimeClientCommand::dispatch(
                ClientCommand::FailWaltrLogin { attempt_id, error },
            ))
            .await,
    );
}

async fn expire_access(
    events: &mpsc::Sender<io::Result<AppEvent>>,
    commands: &mpsc::Sender<RuntimeClientCommand>,
    credentials: &NativeCredentialProvider,
    lease: NativeCredentialLease,
    error: String,
) {
    let revision = credentials.credential_revision();
    if !credentials.clear_bearer_token_if_current(lease, revision) {
        return;
    }
    send_event(
        events,
        NativeLoginEvent::Failed {
            error: error.clone(),
        },
    )
    .await;
    drop(
        commands
            .send(RuntimeClientCommand::dispatch(
                ClientCommand::WaltrAccessExpired { error },
            ))
            .await,
    );
}

async fn login(
    endpoint: String,
    events: mpsc::Sender<io::Result<AppEvent>>,
) -> Result<LoginTokens, NativeAuthError> {
    let config = NativeConfig::from_env()?;
    let client = http_client()?;
    send_event(&events, NativeLoginEvent::Discovering).await;
    let discovered = discover(&client, &endpoint).await?;
    let listener = bind_loopback(&config.loopback).await?;
    let redirect_uri = redirect_uri(&listener, &config.loopback)?;
    let request = authorization_request(&discovered, &config, redirect_uri)?;
    let browser = open_browser(request.url.as_str());
    let browser_opened = browser.is_some();
    send_event(
        &events,
        NativeLoginEvent::AwaitingBrowser {
            authorization_url: request.url.to_string(),
            expires_at: std::time::Instant::now() + LOGIN_TIMEOUT,
            browser_opened,
        },
    )
    .await;

    let callback = time::timeout(
        LOGIN_TIMEOUT,
        receive_callback_with_browser(
            listener,
            &request.redirect_uri,
            &request.state,
            browser,
            &events,
        ),
    )
    .await
    .map_err(|_| NativeAuthError("Waltr login timed out".to_owned()))??;
    send_event(&events, NativeLoginEvent::Exchanging).await;
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

async fn receive_callback_with_browser(
    listener: TcpListener,
    redirect_uri: &Url,
    expected_state: &str,
    browser: Option<tokio::process::Child>,
    events: &mpsc::Sender<io::Result<AppEvent>>,
) -> Result<String, NativeAuthError> {
    let callback = receive_callback(listener, redirect_uri, expected_state);
    tokio::pin!(callback);
    let Some(mut browser) = browser else {
        return callback.await;
    };
    tokio::select! {
        result = &mut callback => result,
        status = browser.wait() => {
            if !matches!(status, Ok(status) if status.success()) {
                send_event(events, NativeLoginEvent::BrowserLaunchFailed).await;
            }
            callback.await
        }
    }
}

#[derive(Debug)]
struct DiscoveredProvider {
    authorization_endpoint: Url,
    token_endpoint: Url,
    resource: String,
    scopes_supported: Vec<String>,
}

async fn discover(client: &Client, endpoint: &str) -> Result<DiscoveredProvider, NativeAuthError> {
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
    Ok(DiscoveredProvider {
        authorization_endpoint: parse_secure_url(
            &metadata.authorization_endpoint,
            "authorization endpoint",
        )?,
        token_endpoint: parse_secure_url(&metadata.token_endpoint, "token endpoint")?,
        resource: protected.resource,
        scopes_supported: protected.scopes_supported,
    })
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

impl NativeConfig {
    fn from_env() -> Result<Self, NativeAuthError> {
        let client_id = env::var(CLIENT_ID_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| NativeAuthError(format!("{CLIENT_ID_ENV} is required")))?;
        let scopes = env::var(SCOPES_ENV)
            .ok()
            .map(|value| value.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        let audience = env::var(AUDIENCE_ENV)
            .ok()
            .filter(|value| !value.trim().is_empty());
        let loopback = match env::var(LOOPBACK_POLICY_ENV).as_deref() {
            Ok("ephemeral") | Err(env::VarError::NotPresent) => LoopbackPolicy::Ephemeral,
            Ok(value) => LoopbackPolicy::Fixed(validate_loopback_url(value)?),
            Err(error) => {
                return Err(NativeAuthError(format!(
                    "invalid {LOOPBACK_POLICY_ENV}: {error}"
                )));
            }
        };
        Ok(Self {
            client_id,
            scopes,
            audience,
            loopback,
        })
    }
}

fn validate_loopback_url(value: &str) -> Result<Url, NativeAuthError> {
    let url = Url::parse(value).map_err(|_| {
        NativeAuthError(format!(
            "{LOOPBACK_POLICY_ENV} must be ephemeral or a loopback URL"
        ))
    })?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || url.port().is_none_or(|port| port == 0)
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(NativeAuthError(format!(
            "{LOOPBACK_POLICY_ENV} must use http://127.0.0.1:<port> without query or fragment"
        )));
    }
    Ok(url)
}

async fn bind_loopback(policy: &LoopbackPolicy) -> Result<TcpListener, NativeAuthError> {
    let port = match policy {
        LoopbackPolicy::Ephemeral => 0,
        LoopbackPolicy::Fixed(url) => url.port().expect("validated loopback port"),
    };
    TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .map_err(NativeAuthError::from)
}

fn redirect_uri(listener: &TcpListener, policy: &LoopbackPolicy) -> Result<Url, NativeAuthError> {
    match policy {
        LoopbackPolicy::Ephemeral => Url::parse(&format!(
            "http://127.0.0.1:{}/callback",
            listener.local_addr()?.port()
        )),
        LoopbackPolicy::Fixed(url) => Ok(url.clone()),
    }
    .map_err(|_| NativeAuthError("could not construct loopback redirect URI".to_owned()))
}

fn authorization_request(
    provider: &DiscoveredProvider,
    config: &NativeConfig,
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
    parse_callback(&bytes, expected_path, expected_authority, expected_state)
}

#[derive(Debug)]
enum CallbackOutcome {
    Code(String),
    ProviderError(NativeAuthError),
}

#[allow(
    clippy::too_many_lines,
    reason = "callback validation is a single ordered security boundary"
)]
fn parse_callback(
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
    let values = |name: &str| {
        url.query_pairs()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
            .collect::<Vec<_>>()
    };
    let states = values("state");
    let state = exactly_one(&states, "state")?
        .ok_or_else(|| NativeAuthError("loopback callback state is missing".to_owned()))?;
    if state.as_bytes() != expected_state.as_bytes() {
        return Err(NativeAuthError(
            "loopback callback state did not match".to_owned(),
        ));
    }
    let errors = values("error");
    let codes = values("code");
    let error = exactly_one(&errors, "error")?;
    let code = exactly_one(&codes, "code")?;
    if error.is_some() && code.is_some() {
        return Err(NativeAuthError(
            "loopback callback contained both code and error".to_owned(),
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
        .ok_or_else(|| {
            NativeAuthError("loopback callback authorization code is missing".to_owned())
        })
}

fn exactly_one<'a>(values: &'a [String], name: &str) -> Result<Option<&'a str>, NativeAuthError> {
    match values {
        [] => Ok(None),
        [value] => Ok(Some(value)),
        _ => Err(NativeAuthError(format!(
            "loopback callback contained duplicate {name} parameters"
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
    events: mpsc::Sender<io::Result<AppEvent>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let result = if let Some(refresh_token) = tokens.refresh_token.as_deref() {
            let advance = REFRESH_SKEW
                .min(tokens.expires_in / 10)
                .max(Duration::from_millis(1));
            time::sleep(tokens.expires_in.saturating_sub(advance)).await;
            send_event(&events, NativeLoginEvent::Refreshing).await;
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
            .or_else(|| tokens.refresh_token.clone()),
        expires_in,
        token_endpoint: tokens.token_endpoint.clone(),
        client_id: tokens.client_id.clone(),
    })
}

async fn parse_token_response(response: Response) -> Result<TokenResponse, NativeAuthError> {
    if !response.status().is_success() {
        return Err(NativeAuthError(format!(
            "token endpoint rejected the request ({})",
            response.status()
        )));
    }
    let bytes = bounded_body(response, MAX_TOKEN_BYTES).await?;
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
    Client::builder()
        .timeout(HTTP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| NativeAuthError("could not initialize the OIDC HTTP client".to_owned()))
}

fn open_browser(url: &str) -> Option<tokio::process::Child> {
    let command = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("rundll32", vec!["url.dll,FileProtocolHandler", url])
    } else {
        ("xdg-open", vec![url])
    };
    let mut process = Command::new(command.0);
    process
        .args(command.1)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    process.spawn().ok()
}

async fn copy_to_clipboard(value: &str) -> bool {
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else if cfg!(target_os = "windows") {
        &[("clip", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    };
    for (program, args) in candidates {
        let Ok(mut child) = Command::new(program)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut stdin) = child.stdin.take()
            && stdin.write_all(value.as_bytes()).await.is_ok()
        {
            drop(stdin);
            if child.wait().await.is_ok_and(|status| status.success()) {
                return true;
            }
        }
    }
    false
}

async fn send_event(events: &mpsc::Sender<io::Result<AppEvent>>, event: NativeLoginEvent) {
    drop(events.send(Ok(AppEvent::NativeLogin(event))).await);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

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
            &NativeConfig {
                client_id: "native".to_owned(),
                scopes: Vec::new(),
                audience: None,
                loopback: LoopbackPolicy::Ephemeral,
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
            &NativeConfig {
                client_id: "native".to_owned(),
                scopes: Vec::new(),
                audience: None,
                loopback: LoopbackPolicy::Ephemeral,
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
            &NativeConfig {
                client_id: "native".to_owned(),
                scopes: vec!["waltr.read".to_owned()],
                audience: None,
                loopback: LoopbackPolicy::Ephemeral,
            },
            redirect,
        )
        .unwrap_err();
        assert!(error.to_string().contains("all required resource scopes"));

        let no_required_scopes = DiscoveredProvider {
            authorization_endpoint: provider.authorization_endpoint.clone(),
            token_endpoint: provider.token_endpoint.clone(),
            resource: provider.resource,
            scopes_supported: Vec::new(),
        };
        let request = authorization_request(
            &no_required_scopes,
            &NativeConfig {
                client_id: "native".to_owned(),
                scopes: vec!["profile".to_owned()],
                audience: None,
                loopback: LoopbackPolicy::Ephemeral,
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
            parse_callback(valid, "/callback", "127.0.0.1", "expected").unwrap(),
            CallbackOutcome::Code(code) if code == "abc"
        ));
        assert!(
            parse_callback(valid, "/callback", "127.0.0.1", "other")
                .unwrap_err()
                .to_string()
                .contains("state did not match")
        );
        let provider_error =
            b"GET /callback?error=access_denied&state=expected HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n";
        assert!(matches!(
            parse_callback(provider_error, "/callback", "127.0.0.1", "expected").unwrap(),
            CallbackOutcome::ProviderError(error) if error.to_string().contains("access_denied")
        ));
        assert!(
            parse_callback(
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
                parse_callback(
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
        let listener = bind_loopback(&LoopbackPolicy::Ephemeral).await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        TcpListener::bind(addr)
            .await
            .expect("canceled listener should release port");
    }

    #[tokio::test]
    async fn cancel_and_timeout_release_callback_port() {
        let listener = bind_loopback(&LoopbackPolicy::Ephemeral).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let redirect = Url::parse(&format!("http://{addr}/callback")).unwrap();
        let callback =
            tokio::spawn(async move { receive_callback(listener, &redirect, "state").await });
        callback.abort();
        drop(callback.await);
        TcpListener::bind(addr)
            .await
            .expect("aborted callback should release port");

        let listener = bind_loopback(&LoopbackPolicy::Ephemeral).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let redirect = Url::parse(&format!("http://{addr}/callback")).unwrap();
        assert!(
            time::timeout(
                Duration::from_millis(10),
                receive_callback(listener, &redirect, "state")
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
        let callback =
            tokio::spawn(async move { receive_callback(listener, &redirect, "expected").await });

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
        let (events, _event_receiver) = mpsc::channel(8);
        let (commands, mut command_receiver) = mpsc::channel(8);
        let (actions, action_receiver) = mpsc::channel(8);
        let coordinator = spawn(
            events,
            commands,
            action_receiver,
            "http://127.0.0.1:1".to_owned(),
            credentials.clone(),
        );
        actions.send(NativeAuthAction::Start).await.unwrap();
        assert_eq!(
            command_receiver.recv().await,
            Some(RuntimeClientCommand::dispatch(
                ClientCommand::StartWaltrLogin
            ))
        );
        let first_lease = credentials.credential_lease();
        actions.send(NativeAuthAction::Start).await.unwrap();
        assert!(
            time::timeout(Duration::from_millis(20), command_receiver.recv())
                .await
                .is_err()
        );
        assert_eq!(credentials.credential_lease(), first_lease);
        actions.send(NativeAuthAction::Cancel).await.unwrap();
        actions
            .send(NativeAuthAction::AttemptStarted {
                attempt_id: "login-shared".to_owned(),
            })
            .await
            .unwrap();
        actions
            .send(NativeAuthAction::AttemptStarted {
                attempt_id: "login-stale".to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(
            command_receiver.recv().await,
            Some(RuntimeClientCommand::dispatch(
                ClientCommand::CancelWaltrLogin {
                    attempt_id: "login-shared".to_owned()
                }
            ))
        );
        assert!(
            time::timeout(Duration::from_millis(20), command_receiver.recv())
                .await
                .is_err()
        );
        drop(actions);
        coordinator.await.unwrap();
    }

    #[tokio::test]
    async fn logout_without_native_credential_is_a_no_op() {
        let credentials = NativeCredentialProvider::new();
        let (events, mut event_receiver) = mpsc::channel(1);
        let (commands, mut command_receiver) = mpsc::channel(1);
        let (actions, action_receiver) = mpsc::channel(1);
        let coordinator = spawn(
            events,
            commands,
            action_receiver,
            "http://127.0.0.1:1".to_owned(),
            credentials,
        );

        actions.send(NativeAuthAction::Logout).await.unwrap();
        assert!(
            time::timeout(Duration::from_millis(20), command_receiver.recv())
                .await
                .is_err()
        );
        assert!(event_receiver.try_recv().is_err());
        drop(actions);
        coordinator.await.unwrap();
    }

    #[tokio::test]
    async fn logout_with_credential_queues_ordered_barrier_without_native_event() {
        let credentials = NativeCredentialProvider::new();
        credentials.replace_bearer_token("access-token").unwrap();
        let (events, mut event_receiver) = mpsc::channel(1);
        let (commands, mut command_receiver) = mpsc::channel(1);
        let (actions, action_receiver) = mpsc::channel(1);
        let coordinator = spawn(
            events,
            commands,
            action_receiver,
            "http://127.0.0.1:1".to_owned(),
            credentials.clone(),
        );

        actions.send(NativeAuthAction::Logout).await.unwrap();
        assert_eq!(
            command_receiver.recv().await,
            Some(RuntimeClientCommand::logout())
        );
        assert!(!credentials.has_bearer_token());
        assert!(event_receiver.try_recv().is_err());
        drop(actions);
        coordinator.await.unwrap();
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
        let (events, _event_receiver) = mpsc::channel(1);
        let (commands, mut command_receiver) = mpsc::channel(1);
        expire_access(
            &events,
            &commands,
            &credentials,
            stale,
            "stale expiry".to_owned(),
        )
        .await;
        assert!(credentials.has_bearer_token());
        assert_eq!(credentials.credential_lease(), current);
        assert!(command_receiver.try_recv().is_err());

        fail_attempt(
            &events,
            &commands,
            &credentials,
            ("stale-login".to_owned(), stale),
            "stale login failure".to_owned(),
        )
        .await;
        assert!(credentials.has_bearer_token());
        assert_eq!(credentials.credential_lease(), current);
        assert!(command_receiver.try_recv().is_err());
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
        let (events, mut event_receiver) = mpsc::channel(1);
        let (commands, mut command_receiver) = mpsc::channel(1);
        expire_access(
            &events,
            &commands,
            &credentials,
            lease,
            "Waltr access token expired".to_owned(),
        )
        .await;
        assert!(!credentials.has_bearer_token());
        assert_eq!(
            command_receiver.recv().await,
            Some(RuntimeClientCommand::dispatch(
                ClientCommand::WaltrAccessExpired {
                    error: "Waltr access token expired".to_owned()
                }
            ))
        );
        assert!(matches!(
            event_receiver.recv().await,
            Some(Ok(AppEvent::NativeLogin(NativeLoginEvent::Failed { .. })))
        ));
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
