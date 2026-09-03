use std::{collections::HashMap, sync::Arc};

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response as AxumResponse},
    routing::get,
};
use waltr::grpc::waltr::auth::v1::{
    AuthAttemptLifecycle, AuthAttemptStatus, AuthCompletionMode, AuthMethod, AuthStatus,
    AuthStatusKind, AuthTarget as ProtoAuthTarget, AuthenticationDomain, AuthenticationFailure,
    AuthenticationFailureReason, BeginAuthRequest, BeginAuthResponse, CancelAuthRequest,
    CancelAuthResponse, CompleteAuthRequest, CompleteAuthResponse, GetAuthAttemptStatusRequest,
    GetAuthAttemptStatusResponse, GetAuthStatusRequest, GetAuthStatusResponse,
    ListAuthStatusesRequest, ListAuthStatusesResponse, LogoutRequest, LogoutResponse,
    RefreshAuthRequest, RefreshAuthResponse,
    auth_service_server::AuthService as GeneratedAuthService, auth_target,
};
use waltr_agent::{
    AuthenticationFailureReason as AgentAuthenticationFailureReason, CredentialRefreshCoordinator,
    CredentialRefreshError, CredentialStore, McpConnectionKind, McpOAuthRegistration,
    McpOAuthRegistrationMethod, McpResolvedConnection, OAuthCredential, RefreshMode,
};
use mcp::oauth::{
    McpBearerChallenge, McpOAuthGrantType, McpOAuthResponseType, McpOAuthSecret,
    McpPublicClientRegistrationRequest, McpRevocationRequest, McpRevocationTokenTypeHint,
    McpTokenEndpointAuthMethod, authorization_server_metadata_urls,
    protected_resource_metadata_urls,
};
use prost::Message;
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use uuid::Uuid;

#[cfg(test)]
use waltr_agent::{
    McpOAuthContext, McpOAuthEndpoints, OAuthCredentialStore, OAuthRefreshClaim, OAuthTokenSet,
};

use crate::{
    agent_definition::AgentDefinitionState,
    config::headers::{configured_credential, has_unresolved_credential, resolve_provider_headers},
    persistence::{DurableAttemptRecord, OutboundOAuthStore},
};

use super::outbound_http::{
    OAUTH_RESPONSE_LIMIT, OutboundHttpPolicy, read_json_body, validate_response_peer,
};
use super::{AuthTarget, oauth};

const OAUTH_COMPLETION_LEASE_MS: i64 = 30_000;

/// gRPC auth service for provider credentials.
#[derive(Debug)]
pub struct AuthService {
    resources: AgentDefinitionState,
    credential_store: CredentialStore,
    outbound_store: Arc<OutboundOAuthStore>,
    attempts: Mutex<HashMap<String, PendingOAuthAttempt>>,
    outbound_policy: OutboundHttpPolicy,
    outbound_oauth: OutboundOAuthConfig,
    self_mcp_credential_ownership: CredentialOwnership,
    #[cfg(test)]
    refresh_outcomes: Mutex<HashMap<String, TestRefreshOutcome>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialOwnership {
    Durable,
    EmbeddedHost,
}

#[derive(Clone, Default)]
pub(crate) struct OutboundOAuthConfig {
    server_callback_uri: Option<String>,
    client_metadata_uri: Option<String>,
    environment: Arc<HashMap<std::ffi::OsString, std::ffi::OsString>>,
    allow_private_test_destinations: bool,
    durable_callback_marker: Option<String>,
}

impl OutboundOAuthConfig {
    pub(crate) fn durable_callback_marker(&self) -> Option<&str> {
        self.durable_callback_marker.as_deref()
    }

    pub(crate) fn from_env() -> Self {
        Self::from_environment(std::env::vars_os().collect())
    }

    fn from_environment(environment: HashMap<std::ffi::OsString, std::ffi::OsString>) -> Self {
        let allow_private_test_destinations = environment
            .get(std::ffi::OsStr::new(
                "WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS",
            ))
            .and_then(|value| value.to_str())
            == Some("true");
        let environment = Arc::new(
            environment
                .into_iter()
                .filter(|(name, _)| name.to_str().is_some_and(outbound_oauth_environment_name))
                .collect::<HashMap<_, _>>(),
        );
        let public_url = environment
            .get(std::ffi::OsStr::new("WALTR_PUBLIC_URL"))
            .and_then(|value| value.to_str());
        Self {
            server_callback_uri: public_url
                .and_then(|url| public_url_with_path(url, "/outbound-oauth/callback")),
            client_metadata_uri: public_url
                .and_then(|url| public_url_with_path(url, "/.well-known/oauth-client/waltr")),
            environment,
            allow_private_test_destinations,
            durable_callback_marker: None,
        }
    }

    pub(crate) fn for_server_callback(callback: String) -> Self {
        Self {
            server_callback_uri: Some(callback),
            client_metadata_uri: None,
            environment: Arc::default(),
            allow_private_test_destinations: false,
            durable_callback_marker: Some(format!(
                "urn:waltr:outbound-oauth-callback:{}",
                Uuid::new_v4()
            )),
        }
    }

    fn variable(&self, name: &str) -> Option<String> {
        self.environment
            .get(std::ffi::OsStr::new(name))
            .and_then(|value| value.to_str())
            .filter(|value| !value.trim().is_empty())
            .map(ToOwned::to_owned)
    }

    fn redirect_uri(&self, server_key: &str) -> Result<String, Status> {
        if let Some(callback) = &self.server_callback_uri {
            return Ok(callback.clone());
        }
        self.variable(&format!(
            "WALTR_MCP_{}_OAUTH_REDIRECT_URI",
            environment_server_key(server_key)
        ))
        .ok_or_else(|| missing_redirect_uri(server_key))
    }

    fn attempt_for_persistence(&self, attempt: &oauth::McpOAuthAttempt) -> oauth::McpOAuthAttempt {
        let mut persisted = attempt.clone();
        if self.server_callback_uri.as_deref() == Some(attempt.redirect_uri())
            && let Some(marker) = &self.durable_callback_marker
        {
            persisted.set_redirect_uri(marker.clone());
        }
        persisted
    }

    fn rehydrate_attempt(&self, attempt: &mut oauth::McpOAuthAttempt) -> Result<(), Status> {
        let Some(marker) = &self.durable_callback_marker else {
            return Ok(());
        };
        if attempt.redirect_uri() != marker {
            return Err(Status::failed_precondition(
                "OAuth attempt belongs to a different server generation",
            ));
        }
        let callback = self
            .server_callback_uri
            .clone()
            .ok_or_else(|| Status::failed_precondition("OAuth server callback is unavailable"))?;
        attempt.set_redirect_uri(callback);
        Ok(())
    }

    fn persisted_attempt_matches_callback(&self, attempt: &oauth::McpOAuthAttempt) -> bool {
        match &self.durable_callback_marker {
            Some(marker) => attempt.redirect_uri() == marker,
            None => self.server_callback_uri.as_deref() == Some(attempt.redirect_uri()),
        }
    }

    fn credential_for_persistence(&self, mut credential: OAuthCredential) -> OAuthCredential {
        if let (Some(callback), Some(marker), Some(context)) = (
            self.server_callback_uri.as_deref(),
            self.durable_callback_marker.as_ref(),
            credential.mcp_context_mut(),
        ) && context.registration.redirect_uri == callback
        {
            context.registration.redirect_uri.clone_from(marker);
        }
        credential
    }

    fn durable_attempt_for_current_generation(
        &self,
        record: &DurableAttemptRecord,
    ) -> Result<oauth::McpOAuthAttempt, Status> {
        let attempt = serde_json::from_str::<oauth::McpOAuthAttempt>(&record.attempt_json)
            .map_err(|_| Status::data_loss("stored OAuth attempt is invalid"))?;
        if self
            .durable_callback_marker
            .as_ref()
            .is_some_and(|marker| attempt.redirect_uri() != marker)
        {
            return Err(Status::not_found("OAuth attempt does not exist"));
        }
        Ok(attempt)
    }
}

impl std::fmt::Debug for OutboundOAuthConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OutboundOAuthConfig")
            .field(
                "server_callback_configured",
                &self.server_callback_uri.is_some(),
            )
            .field(
                "client_metadata_configured",
                &self.client_metadata_uri.is_some(),
            )
            .field("target_environment_entries", &self.environment.len())
            .field(
                "private_test_destinations_enabled",
                &self.allow_private_test_destinations,
            )
            .field(
                "durable_callback_marker_configured",
                &self.durable_callback_marker.is_some(),
            )
            .finish()
    }
}

fn outbound_oauth_environment_name(name: &str) -> bool {
    name == "WALTR_PUBLIC_URL"
        || (name.starts_with("WALTR_MCP_")
            && ["_OAUTH_CLIENT_ID", "_OAUTH_ISSUER", "_OAUTH_REDIRECT_URI"]
                .iter()
                .any(|suffix| name.ends_with(suffix)))
}

#[derive(Debug, Clone)]
enum PendingOAuthAttempt {
    Provider(oauth::OAuthAttempt),
}

#[cfg(test)]
#[derive(Debug)]
enum TestRefreshOutcome {
    Success(Box<OAuthCredential>),
    Failure {
        reason: AgentAuthenticationFailureReason,
        message: String,
        internal: bool,
    },
}

struct RefreshFailure {
    reason: AgentAuthenticationFailureReason,
    message: String,
    internal: bool,
}

struct McpAuthDiscovery {
    challenge: McpBearerChallenge,
    protected: mcp::McpProtectedResourceMetadata,
    authorization_server: mcp::McpAuthorizationServerMetadata,
}

struct McpRegistrationRequest<'a> {
    server_key: &'a str,
    target: &'a AuthTarget,
    resource: &'a str,
    authorization_server: &'a mcp::McpAuthorizationServerMetadata,
    redirect_uri: &'a str,
    policy: OutboundHttpPolicy,
}

impl AuthService {
    /// Create an auth service.
    #[must_use]
    pub(crate) fn new(
        resources: AgentDefinitionState,
        credential_store: CredentialStore,
        outbound_store: Arc<OutboundOAuthStore>,
        configured_private_origins: Vec<reqwest::Url>,
        automatic_self_resource: Option<reqwest::Url>,
        outbound_oauth: OutboundOAuthConfig,
    ) -> Arc<Self> {
        let self_mcp_credential_ownership = if outbound_oauth.durable_callback_marker.is_some()
            && automatic_self_resource.is_some()
        {
            CredentialOwnership::EmbeddedHost
        } else {
            CredentialOwnership::Durable
        };
        Arc::new(Self {
            resources,
            credential_store,
            outbound_store,
            attempts: Mutex::new(HashMap::new()),
            outbound_policy: OutboundHttpPolicy::production(
                configured_private_origins,
                automatic_self_resource,
                outbound_oauth.allow_private_test_destinations,
            ),
            outbound_oauth,
            self_mcp_credential_ownership,
            #[cfg(test)]
            refresh_outcomes: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/outbound-oauth/callback", get(outbound_callback))
            .route("/.well-known/oauth-client/waltr", get(client_metadata))
            .with_state(Arc::clone(self))
    }

    /// Return the credential store used by this service.
    #[must_use]
    pub const fn credential_store(&self) -> &CredentialStore {
        &self.credential_store
    }

    fn configuration(&self) -> waltr::grpc::waltr::config::v1::Configuration {
        self.resources.snapshot().configuration().clone()
    }

    async fn status_for(&self, target: AuthTarget) -> Result<AuthStatus, Status> {
        match target {
            AuthTarget::Provider { key } => self.provider_status_for(&key).await,
            AuthTarget::McpServer { key } => self.mcp_status_for(&key).await,
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "provider status combines configuration capabilities with durable credential state"
    )]
    async fn provider_status_for(&self, provider_key: &str) -> Result<AuthStatus, Status> {
        let config = self.configuration();
        let provider = config.providers.get(provider_key).ok_or_else(|| {
            Status::not_found(format!("provider '{provider_key}' does not exist"))
        })?;
        let definition = oauth::provider_definition(provider_key, provider);
        let supported_methods = definition
            .as_ref()
            .map_or_else(Vec::new, |_| supported_methods());
        let resolved_headers = resolve_provider_headers(&provider.headers);
        if !configured_credential(&resolved_headers.headers).is_empty() {
            return Ok(AuthStatus {
                target: Some(proto_provider_target(provider_key)),
                kind: AuthStatusKind::ApiKeyConfigured as i32,
                supported_methods: supported_methods.clone(),
                ..AuthStatus::default()
            });
        }
        let Some(_definition) = definition else {
            if has_unresolved_credential(&resolved_headers) {
                return Ok(AuthStatus {
                    target: Some(proto_provider_target(provider_key)),
                    kind: AuthStatusKind::NotConfigured as i32,
                    message: "credential environment variable is not set".to_owned(),
                    ..AuthStatus::default()
                });
            }
            return Ok(AuthStatus {
                target: Some(proto_provider_target(provider_key)),
                kind: AuthStatusKind::Unsupported as i32,
                message: if provider.oauth_compatible {
                    "provider requires custom OAuth settings".to_owned()
                } else {
                    "provider is not OAuth compatible".to_owned()
                },
                ..AuthStatus::default()
            });
        };
        match self
            .credential_store
            .read(&AuthTarget::provider(provider_key).credential_key())
            .await
            .map_err(|error| Status::internal(format!("failed to read credentials: {error}")))?
        {
            Some(record) if record.credential.provider_metadata().is_some() => {
                let credential = record.credential;
                let expires_at_ms = credential.token.expires_at_ms;
                let expired = expires_at_ms.is_some_and(|expiry| expiry <= now_ms());
                Ok(AuthStatus {
                    target: Some(proto_provider_target(provider_key)),
                    kind: if record.reauthorization_required {
                        AuthStatusKind::AuthorizationRequired as i32
                    } else if expired {
                        AuthStatusKind::Expired as i32
                    } else {
                        AuthStatusKind::OauthConfigured as i32
                    },
                    supported_methods,
                    expires_at_ms: expires_at_ms.unwrap_or_default(),
                    metadata: display_safe_provider_metadata(
                        credential.provider_metadata().cloned().unwrap_or_default(),
                    ),
                    can_refresh: credential.token.refresh_token.is_some()
                        && !record.reauthorization_required,
                    message: if record.reauthorization_required {
                        "provider OAuth credential requires reauthorization".to_owned()
                    } else {
                        String::new()
                    },
                    ..AuthStatus::default()
                })
            }
            Some(_) | None => Ok(AuthStatus {
                target: Some(proto_provider_target(provider_key)),
                kind: AuthStatusKind::NotConfigured as i32,
                supported_methods,
                ..AuthStatus::default()
            }),
        }
    }

    async fn mcp_status_for(&self, server_key: &str) -> Result<AuthStatus, Status> {
        let config = self.configuration();
        let server = config.mcp_servers.get(server_key).ok_or_else(|| {
            Status::not_found(format!("mcp server '{server_key}' does not exist"))
        })?;
        let target = AuthTarget::mcp_server(server_key);
        match self
            .credential_store
            .read(&target.credential_key())
            .await
            .map_err(|error| Status::internal(format!("failed to read credentials: {error}")))?
        {
            Some(record) if record.credential.mcp_context().is_some() => Ok(mcp_oauth_status(
                server_key,
                &server.url,
                &record.credential,
                record.reauthorization_required,
            )),
            Some(_) | None => Ok(AuthStatus {
                target: Some(proto_mcp_target(server_key)),
                kind: AuthStatusKind::NotConfigured as i32,
                supported_methods: vec![AuthMethod::OauthManual as i32],
                message: "MCP OAuth is discovered during Streamable HTTP initialization".to_owned(),
                ..AuthStatus::default()
            }),
        }
    }

    async fn begin_mcp_auth(
        &self,
        server_key: String,
    ) -> Result<(BeginAuthResponse, AuthCompletionMode, i32), Status> {
        let config = self.configuration();
        let server = config.mcp_servers.get(&server_key).ok_or_else(|| {
            Status::not_found(format!("mcp server '{server_key}' does not exist"))
        })?;
        let target = AuthTarget::mcp_server(&server_key);
        if server.url.is_empty() {
            return Err(outbound_failure_status(
                &target,
                tonic::Code::FailedPrecondition,
                AuthenticationFailureReason::ConfigurationRequired,
                format!("mcp server '{server_key}' does not have a streamable HTTP URL"),
            ));
        }

        let policy = self.outbound_policy.for_mcp_resource(&server.url);
        let discovery = discover_mcp_auth(&target, &server.url, policy.clone()).await?;
        let auth_server = discovery.authorization_server;
        let (authorize_url, token_url) = required_authorization_endpoints(&target, &auth_server)?;
        let redirect_uri = self.outbound_oauth.redirect_uri(&server_key)?;
        let registration = self
            .resolve_mcp_registration(McpRegistrationRequest {
                server_key: &server_key,
                target: &target,
                resource: &discovery.protected.resource,
                authorization_server: &auth_server,
                redirect_uri: &redirect_uri,
                policy,
            })
            .await?;
        let scopes = discovery.challenge.scope.as_deref().map_or_else(
            || discovery.protected.scopes_supported.clone(),
            |scope| scope.split_whitespace().map(ToOwned::to_owned).collect(),
        );
        let (attempt, mut response) = oauth::begin_mcp(
            target.clone(),
            auth_server.issuer.clone(),
            &authorize_url,
            token_url,
            auth_server.revocation_endpoint,
            registration,
            scopes,
            discovery.protected.resource,
            auth_server.authorization_response_iss_parameter_supported == Some(true),
            now_ms(),
        )
        .map_err(|status| outbound_oauth_status(&target, &status))?;
        let (attempt_id, completion_mode, registration_method) =
            self.save_mcp_attempt(&target, &attempt).await?;
        response.attempt_id.clone_from(&attempt_id);
        Ok((response, completion_mode, registration_method))
    }

    async fn begin_provider_oauth(
        &self,
        provider_key: String,
    ) -> Result<BeginAuthResponse, Status> {
        let config = self.configuration();
        let provider = config.providers.get(&provider_key).ok_or_else(|| {
            Status::not_found(format!("provider '{provider_key}' does not exist"))
        })?;
        let definition = oauth::provider_definition(&provider_key, provider).ok_or_else(|| {
            Status::failed_precondition(format!("provider '{provider_key}' does not support OAuth"))
        })?;
        let (attempt, mut response) = oauth::begin_manual_with_policy(
            AuthTarget::provider(provider_key),
            definition.definition,
            now_ms(),
            &self.outbound_policy.without_trusted_private_origins(),
        )?;
        let policy = self.outbound_policy.without_trusted_private_origins();
        policy
            .validate_destination(&attempt.definition().authorize_url)
            .await?;
        policy
            .validate_destination(&attempt.definition().token_url)
            .await?;
        let attempt_id = Uuid::new_v4().to_string();
        response.attempt_id.clone_from(&attempt_id);
        self.attempts
            .lock()
            .await
            .insert(attempt_id, PendingOAuthAttempt::Provider(attempt));
        Ok(response)
    }

    #[cfg(test)]
    async fn apply_test_refresh_outcome(&self, target: &AuthTarget) -> Option<Result<(), Status>> {
        let outcome = self
            .refresh_outcomes
            .lock()
            .await
            .remove(&target.credential_key())?;
        Some(match outcome {
            TestRefreshOutcome::Success(credential) => self
                .credential_store
                .write(&target.credential_key(), *credential)
                .await
                .map_err(|error| {
                    Status::internal(format!("failed to store refreshed credential: {error}"))
                }),
            TestRefreshOutcome::Failure {
                reason,
                message,
                internal,
            } => Err(refresh_failure_status(
                target,
                &RefreshFailure {
                    reason,
                    message,
                    internal,
                },
            )),
        })
    }

    async fn refresh_auth(&self, target: &AuthTarget) -> Result<(), Status> {
        #[cfg(test)]
        if let Some(result) = self.apply_test_refresh_outcome(target).await {
            return result;
        }
        let coordinator = CredentialRefreshCoordinator::new(self.credential_store.clone());
        match target {
            AuthTarget::Provider { key } => {
                let config = self.configuration();
                let provider = config
                    .providers
                    .get(key)
                    .ok_or_else(|| Status::not_found(format!("provider '{key}' does not exist")))?;
                let definition = oauth::provider_definition(key, provider).ok_or_else(|| {
                    outbound_failure_status(
                        target,
                        tonic::Code::FailedPrecondition,
                        AuthenticationFailureReason::ConfigurationRequired,
                        format!("provider '{key}' does not support OAuth"),
                    )
                })?;
                coordinator
                    .refresh_provider(
                        key,
                        &oauth::provider_refresh_settings(&definition.definition),
                        RefreshMode::Forced,
                    )
                    .await
                    .map_err(|error| {
                        refresh_failure_status(target, &RefreshFailure::from(&error))
                    })?;
            }
            AuthTarget::McpServer { key } => {
                let config = self.configuration();
                let server = config.mcp_servers.get(key).ok_or_else(|| {
                    Status::not_found(format!("mcp server '{key}' does not exist"))
                })?;
                let policy = self.outbound_policy.for_mcp_resource(&server.url);
                let coordinator = coordinator
                    .with_trusted_mcp_oauth_origins(policy.trusted_private_origins().to_vec());
                coordinator
                    .refresh_mcp(
                        &McpResolvedConnection {
                            key: key.clone(),
                            kind: McpConnectionKind::StreamableHttp,
                            command: server.command.clone(),
                            url: server.url.clone(),
                        },
                        RefreshMode::Forced,
                    )
                    .await
                    .map_err(|error| {
                        refresh_failure_status(target, &RefreshFailure::from(&error))
                    })?;
            }
        }
        Ok(())
    }

    async fn save_mcp_attempt(
        &self,
        target: &AuthTarget,
        attempt: &oauth::McpOAuthAttempt,
    ) -> Result<(String, AuthCompletionMode, i32), Status> {
        let attempt_id = Uuid::new_v4().to_string();
        let completion_mode =
            if self.outbound_oauth.server_callback_uri.as_deref() == Some(attempt.redirect_uri()) {
                AuthCompletionMode::ServerCallback
            } else {
                AuthCompletionMode::Manual
            };
        let registration_method = proto_registration_method(attempt.registration_method()) as i32;
        self.outbound_store
            .save_attempt(
                &DurableAttemptRecord {
                    attempt_id: attempt_id.clone(),
                    target_key: target.credential_key(),
                    attempt_json: serde_json::to_string(
                        &self.outbound_oauth.attempt_for_persistence(attempt),
                    )
                    .map_err(|_| Status::internal("failed to encode MCP OAuth attempt"))?,
                    registration_method: registration_method_name(attempt.registration_method())
                        .to_owned(),
                    completion_mode: completion_mode_name(completion_mode).to_owned(),
                    status: "pending".to_owned(),
                    failure_reason: None,
                    expires_at_ms: attempt.expires_at_ms(),
                },
                attempt.state(),
            )
            .await
            .map_err(Status::internal)?;
        Ok((attempt_id, completion_mode, registration_method))
    }

    async fn resolve_mcp_registration(
        &self,
        request: McpRegistrationRequest<'_>,
    ) -> Result<McpOAuthRegistration, Status> {
        let persisted = self.reusable_mcp_registration(&request).await?;
        if let Some(registration) = configured_mcp_registration(
            &self.outbound_oauth,
            request.server_key,
            request.redirect_uri,
            &request.authorization_server.issuer,
        )? {
            return Ok(registration);
        }
        if let Some(registration) = persisted
            .as_ref()
            .filter(|registration| registration.method == McpOAuthRegistrationMethod::Preregistered)
        {
            return Ok(registration.clone());
        }
        if let Some(registration) = persisted.as_ref().filter(|registration| {
            registration.method == McpOAuthRegistrationMethod::ClientMetadata
        }) {
            return Ok(registration.clone());
        }
        if request
            .authorization_server
            .client_id_metadata_document_supported
            == Some(true)
            && let Some(client_id) = self.outbound_oauth.client_metadata_uri.clone()
        {
            return Ok(McpOAuthRegistration {
                method: McpOAuthRegistrationMethod::ClientMetadata,
                client_id,
                redirect_uri: request.redirect_uri.to_owned(),
                token_endpoint_auth_method: "none".to_owned(),
                created_at_ms: Some(now_ms()),
                expires_at_ms: None,
            });
        }
        if let Some(registration) = persisted
            .filter(|registration| registration.method == McpOAuthRegistrationMethod::Dynamic)
        {
            return Ok(registration);
        }
        self.register_mcp_client(&request).await
    }

    async fn reusable_mcp_registration(
        &self,
        request: &McpRegistrationRequest<'_>,
    ) -> Result<Option<McpOAuthRegistration>, Status> {
        if let Some(current) = self
            .credential_store
            .read(&request.target.credential_key())
            .await
            .map_err(|error| {
                Status::internal(format!("failed to read MCP registration: {error}"))
            })?
            && let Some(current) = current.credential.mcp_context()
            && current.issuer == request.authorization_server.issuer
            && current.resource == request.resource
            && current
                .registration
                .expires_at_ms
                .is_none_or(|expiry| expiry > now_ms())
            && !current.registration.client_id.is_empty()
        {
            let registration = current.registration.clone();
            if registration.redirect_uri != request.redirect_uri {
                return Ok(None);
            }
            Ok(Some(registration))
        } else {
            Ok(None)
        }
    }

    async fn register_mcp_client(
        &self,
        request: &McpRegistrationRequest<'_>,
    ) -> Result<McpOAuthRegistration, Status> {
        if let Some(endpoint) = request
            .authorization_server
            .registration_endpoint
            .as_deref()
        {
            let registration_request = McpPublicClientRegistrationRequest {
                application_type: if url::Url::parse(request.redirect_uri)
                    .is_ok_and(|redirect| redirect.scheme() == "https")
                {
                    mcp::McpOAuthApplicationType::Web
                } else {
                    mcp::McpOAuthApplicationType::Native
                },
                redirect_uris: vec![request.redirect_uri.to_owned()],
                grant_types: vec![
                    McpOAuthGrantType::AuthorizationCode,
                    McpOAuthGrantType::RefreshToken,
                ],
                response_types: vec![McpOAuthResponseType::Code],
                token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
                client_name: Some("Waltr outbound MCP client".to_owned()),
            };
            let response = register_public_client_hardened(
                request.policy.clone(),
                endpoint,
                &registration_request,
            )
            .await
            .map_err(|status| outbound_oauth_status(request.target, &status))?;
            return Ok(McpOAuthRegistration {
                method: McpOAuthRegistrationMethod::Dynamic,
                client_id: response.client_id,
                redirect_uri: request.redirect_uri.to_owned(),
                token_endpoint_auth_method: "none".to_owned(),
                created_at_ms: response
                    .client_id_issued_at
                    .and_then(|seconds| i64::try_from(seconds).ok())
                    .map(|seconds| seconds.saturating_mul(1000))
                    .or_else(|| Some(now_ms())),
                expires_at_ms: None,
            });
        }
        Err(outbound_failure_status(
            request.target,
            tonic::Code::FailedPrecondition,
            AuthenticationFailureReason::ConfigurationRequired,
            "MCP authorization server supports neither configured preregistration, CIMD, nor DCR"
                .to_owned(),
        ))
    }

    async fn complete_attempt(&self, attempt_id: &str, input: &str) -> Result<AuthStatus, Status> {
        let attempt = self.attempts.lock().await.get(attempt_id).cloned();
        let Some(attempt) = attempt else {
            return self.complete_mcp_attempt(attempt_id, input).await;
        };
        let (target, credential) = match &attempt {
            PendingOAuthAttempt::Provider(attempt) => {
                let target = attempt.target().clone();
                let mut credential = oauth::complete_manual_with_policy(
                    attempt,
                    input,
                    now_ms(),
                    self.outbound_policy.without_trusted_private_origins(),
                )
                .await
                .map_err(|status| outbound_oauth_status(&target, &status))?;
                let metadata = credential
                    .provider_metadata_mut()
                    .expect("provider OAuth completion produces provider context");
                metadata.insert(
                    "token_url".to_owned(),
                    attempt.definition().token_url.clone(),
                );
                metadata.insert(
                    "client_id".to_owned(),
                    attempt.definition().client_id.clone(),
                );
                metadata.insert(
                    "redirect_uri".to_owned(),
                    attempt.definition().redirect_uri.clone(),
                );
                (target, credential)
            }
        };
        self.attempts.lock().await.remove(attempt_id);
        self.credential_store
            .write(&target.credential_key(), credential)
            .await
            .map_err(|error| Status::internal(format!("failed to store credential: {error}")))?;
        self.status_for(target).await
    }

    async fn complete_mcp_attempt(
        &self,
        attempt_id: &str,
        input: &str,
    ) -> Result<AuthStatus, Status> {
        self.complete_mcp_attempt_with_policy(attempt_id, input, None)
            .await
    }

    async fn complete_mcp_attempt_with_policy(
        &self,
        attempt_id: &str,
        input: &str,
        policy: Option<OutboundHttpPolicy>,
    ) -> Result<AuthStatus, Status> {
        let record = self
            .outbound_store
            .attempt(attempt_id)
            .await
            .map_err(Status::internal)?
            .ok_or_else(|| Status::not_found("OAuth attempt does not exist"))?;
        let mut attempt: oauth::McpOAuthAttempt = serde_json::from_str(&record.attempt_json)
            .map_err(|_| Status::data_loss("stored OAuth attempt is invalid"))?;
        self.outbound_oauth.rehydrate_attempt(&mut attempt)?;
        if record.expires_at_ms < now_ms() {
            let expired = self
                .outbound_store
                .transition_pending_attempt(
                    attempt_id,
                    "expired",
                    Some("authorization attempt expired"),
                )
                .await
                .map_err(Status::internal)?;
            if !expired {
                return Err(Status::failed_precondition(
                    "OAuth attempt is no longer pending",
                ));
            }
            return Err(Status::deadline_exceeded("OAuth attempt expired"));
        }
        let target = attempt.target().clone();
        if target.credential_key() != record.target_key {
            return Err(Status::data_loss("stored OAuth attempt target is invalid"));
        }
        let claim_id = Uuid::new_v4().to_string();
        if !self
            .outbound_store
            .claim_attempt(
                attempt_id,
                &claim_id,
                now_ms().saturating_add(OAUTH_COMPLETION_LEASE_MS),
            )
            .await
            .map_err(Status::internal)?
        {
            return Err(Status::failed_precondition(
                "OAuth attempt is no longer pending",
            ));
        }
        let policy =
            policy.unwrap_or_else(|| self.outbound_policy.for_mcp_resource(attempt.resource()));
        let completion = oauth::complete_mcp_with_policy(&attempt, input, now_ms(), policy).await;
        let credential = match completion {
            Ok(credential) => credential,
            Err(status) => {
                let failure = sanitized_completion_failure(&status);
                self.outbound_store
                    .finish_claimed_attempt(attempt_id, &claim_id, "failed", Some(failure))
                    .await
                    .map_err(Status::internal)?;
                return Err(outbound_oauth_status(&target, &status));
            }
        };
        let credential = self.outbound_oauth.credential_for_persistence(credential);
        self.outbound_store
            .complete_attempt_with_credential(
                attempt_id,
                &claim_id,
                &target.credential_key(),
                &credential,
            )
            .await
            .map_err(Status::internal)?;
        self.status_for(target).await
    }
}

#[tonic::async_trait]
impl GeneratedAuthService for AuthService {
    async fn list_auth_statuses(
        &self,
        _request: Request<ListAuthStatusesRequest>,
    ) -> Result<Response<ListAuthStatusesResponse>, Status> {
        let config = self.configuration();
        let mut statuses = Vec::with_capacity(config.providers.len() + config.mcp_servers.len());
        for provider_key in config.providers.keys() {
            statuses.push(self.provider_status_for(provider_key).await?);
        }
        for server_key in config.mcp_servers.keys() {
            statuses.push(self.mcp_status_for(server_key).await?);
        }
        Ok(Response::new(ListAuthStatusesResponse { statuses }))
    }

    async fn get_auth_status(
        &self,
        request: Request<GetAuthStatusRequest>,
    ) -> Result<Response<GetAuthStatusResponse>, Status> {
        let target = auth_target_from_proto(request.into_inner().target)?;
        let status = self.status_for(target).await?;
        Ok(Response::new(GetAuthStatusResponse {
            status: Some(status),
        }))
    }

    async fn begin_auth(
        &self,
        request: Request<BeginAuthRequest>,
    ) -> Result<Response<BeginAuthResponse>, Status> {
        let request = request.into_inner();
        let target = auth_target_from_proto(request.target)?;
        if AuthMethod::try_from(request.method).unwrap_or(AuthMethod::Unspecified)
            != AuthMethod::OauthManual
        {
            return Err(Status::failed_precondition(
                "only manual OAuth completion is supported",
            ));
        }
        let (response, completion_mode, registration_method) = match target {
            AuthTarget::Provider { key } => (
                self.begin_provider_oauth(key).await?,
                AuthCompletionMode::Manual,
                0,
            ),
            AuthTarget::McpServer { key } => self.begin_mcp_auth(key).await?,
        };
        Ok(Response::new(BeginAuthResponse {
            completion_mode: completion_mode as i32,
            registration_method,
            ..response
        }))
    }

    async fn complete_auth(
        &self,
        request: Request<CompleteAuthRequest>,
    ) -> Result<Response<CompleteAuthResponse>, Status> {
        let request = request.into_inner();
        let status = self
            .complete_attempt(&request.attempt_id, &request.input)
            .await?;
        Ok(Response::new(CompleteAuthResponse {
            status: Some(status),
        }))
    }

    async fn get_auth_attempt_status(
        &self,
        request: Request<GetAuthAttemptStatusRequest>,
    ) -> Result<Response<GetAuthAttemptStatusResponse>, Status> {
        let attempt_id = request.into_inner().attempt_id;
        let record = self
            .outbound_store
            .attempt(&attempt_id)
            .await
            .map_err(Status::internal)?
            .ok_or_else(|| Status::not_found("OAuth attempt does not exist"))?;
        self.outbound_oauth
            .durable_attempt_for_current_generation(&record)?;
        let lifecycle = attempt_lifecycle(&record);
        let target = target_from_credential_key(&record.target_key)?;
        let status = if lifecycle == AuthAttemptLifecycle::Succeeded {
            Some(self.status_for(target.clone()).await?)
        } else {
            None
        };
        Ok(Response::new(GetAuthAttemptStatusResponse {
            attempt: Some(AuthAttemptStatus {
                attempt_id,
                target: Some(match &target {
                    AuthTarget::Provider { key } => proto_provider_target(key),
                    AuthTarget::McpServer { key } => proto_mcp_target(key),
                }),
                lifecycle: lifecycle as i32,
                completion_mode: parse_completion_mode(&record.completion_mode) as i32,
                registration_method: parse_proto_registration_method(&record.registration_method)
                    as i32,
                expires_at_ms: record.expires_at_ms,
                failure_reason: record.failure_reason.unwrap_or_default(),
            }),
            status,
        }))
    }

    async fn cancel_auth(
        &self,
        request: Request<CancelAuthRequest>,
    ) -> Result<Response<CancelAuthResponse>, Status> {
        let attempt_id = request.into_inner().attempt_id;
        self.attempts.lock().await.remove(&attempt_id);
        if let Some(record) = self
            .outbound_store
            .attempt(&attempt_id)
            .await
            .map_err(Status::internal)?
        {
            self.outbound_oauth
                .durable_attempt_for_current_generation(&record)?;
        }
        self.outbound_store
            .transition_pending_attempt(&attempt_id, "canceled", None)
            .await
            .map_err(Status::internal)?;
        Ok(Response::new(CancelAuthResponse {}))
    }

    async fn logout(
        &self,
        request: Request<LogoutRequest>,
    ) -> Result<Response<LogoutResponse>, Status> {
        let target = auth_target_from_proto(request.into_inner().target)?;
        if self.self_mcp_credential_ownership == CredentialOwnership::EmbeddedHost
            && matches!(&target, AuthTarget::McpServer { key } if key == "waltr")
        {
            return Err(Status::failed_precondition(
                "embedded self-MCP credential is host-owned",
            ));
        }
        let mcp_credential = if matches!(target, AuthTarget::McpServer { .. }) {
            match self
                .credential_store
                .read(&target.credential_key())
                .await
                .map_err(|error| Status::internal(format!("failed to read credential: {error}")))?
            {
                Some(record) if record.credential.mcp_context().is_some() => {
                    Some(record.credential)
                }
                _ => None,
            }
        } else {
            None
        };
        self.credential_store
            .delete(&target.credential_key())
            .await
            .map_err(|error| Status::internal(format!("failed to delete credential: {error}")))?;
        if let Some(credential) = mcp_credential {
            let resource = credential
                .mcp_context()
                .map_or("", |context| context.resource.as_str());
            revoke_mcp_credential_with_policy(
                &credential,
                self.outbound_policy.for_mcp_resource(resource),
            )
            .await;
        }
        let status = self.status_for(target).await?;
        Ok(Response::new(LogoutResponse {
            status: Some(status),
        }))
    }

    async fn refresh_auth(
        &self,
        request: Request<RefreshAuthRequest>,
    ) -> Result<Response<RefreshAuthResponse>, Status> {
        let target = auth_target_from_proto(request.into_inner().target)?;
        Self::refresh_auth(self, &target).await?;
        let status = self.status_for(target).await?;
        Ok(Response::new(RefreshAuthResponse {
            status: Some(status),
        }))
    }
}

#[derive(serde::Deserialize)]
struct OutboundCallbackQuery {
    state: String,
    code: Option<String>,
    error: Option<String>,
    iss: Option<String>,
}

async fn outbound_callback(
    State(service): State<Arc<AuthService>>,
    Query(query): Query<OutboundCallbackQuery>,
) -> AxumResponse {
    outbound_callback_with_policy(service, query, None).await
}

#[allow(
    clippy::too_many_lines,
    reason = "the callback handles lookup, denial, completion, and durable claim responses"
)]
async fn outbound_callback_with_policy(
    service: Arc<AuthService>,
    query: OutboundCallbackQuery,
    policy: Option<OutboundHttpPolicy>,
) -> AxumResponse {
    let record = match service.outbound_store.attempt_by_state(&query.state).await {
        Ok(Some(record)) => record,
        Ok(None) => {
            return (StatusCode::NOT_FOUND, "Authorization attempt not found").into_response();
        }
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                "Authorization completion unavailable",
            )
                .into_response();
        }
    };
    if !serde_json::from_str::<oauth::McpOAuthAttempt>(&record.attempt_json).is_ok_and(|attempt| {
        service
            .outbound_oauth
            .persisted_attempt_matches_callback(&attempt)
    }) {
        return (StatusCode::NOT_FOUND, "Authorization attempt not found").into_response();
    }
    if !callback_response_issuer_is_valid(&record, query.iss.as_deref()) {
        return (
            StatusCode::BAD_REQUEST,
            "Authorization could not be completed",
        )
            .into_response();
    }
    if let Some(error) = query.error {
        let claim_id = Uuid::new_v4().to_string();
        match service
            .outbound_store
            .claim_attempt(
                &record.attempt_id,
                &claim_id,
                now_ms().saturating_add(OAUTH_COMPLETION_LEASE_MS),
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::CONFLICT,
                    "Authorization attempt is no longer pending",
                )
                    .into_response();
            }
            Err(_) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Authorization completion unavailable",
                )
                    .into_response();
            }
        }
        let reason = sanitize_oauth_failure(&error);
        drop(
            service
                .outbound_store
                .finish_claimed_attempt(&record.attempt_id, &claim_id, "failed", Some(&reason))
                .await,
        );
        return (
            StatusCode::BAD_REQUEST,
            Html("Authorization was denied. You may close this window."),
        )
            .into_response();
    }
    let Some(code) = query.code.filter(|code| !code.is_empty()) else {
        return (StatusCode::BAD_REQUEST, "Authorization code is required").into_response();
    };
    let input = callback_completion_input(&code, &query.state, query.iss.as_deref());
    match service
        .complete_mcp_attempt_with_policy(&record.attempt_id, &input, policy)
        .await
    {
        Ok(_) => Html("Authorization complete. You may close this window.").into_response(),
        Err(status) if status.code() == tonic::Code::FailedPrecondition => (
            StatusCode::CONFLICT,
            "Authorization attempt was already completed",
        )
            .into_response(),
        Err(_) => (
            StatusCode::BAD_REQUEST,
            "Authorization could not be completed",
        )
            .into_response(),
    }
}

fn callback_response_issuer_is_valid(record: &DurableAttemptRecord, issuer: Option<&str>) -> bool {
    serde_json::from_str::<oauth::McpOAuthAttempt>(&record.attempt_json)
        .is_ok_and(|attempt| attempt.validate_response_issuer(issuer).is_ok())
}

fn callback_completion_input(code: &str, state: &str, issuer: Option<&str>) -> String {
    let mut input = url::form_urlencoded::Serializer::new(String::new());
    input.append_pair("code", code).append_pair("state", state);
    if let Some(issuer) = issuer {
        input.append_pair("iss", issuer);
    }
    input.finish()
}

async fn client_metadata(State(service): State<Arc<AuthService>>) -> AxumResponse {
    let Some(client_id) = service.outbound_oauth.client_metadata_uri.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(redirect_uri) = service.outbound_oauth.server_callback_uri.clone() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let document = mcp::McpClientMetadataDocument {
        client_id,
        redirect_uris: vec![redirect_uri],
        grant_types: vec![
            McpOAuthGrantType::AuthorizationCode,
            McpOAuthGrantType::RefreshToken,
        ],
        response_types: vec![McpOAuthResponseType::Code],
        token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
        client_name: Some("Waltr outbound MCP client".to_owned()),
        extra: std::collections::BTreeMap::from([
            (
                "client_uri".to_owned(),
                serde_json::Value::String("https://github.com/rfhold/waltr".to_owned()),
            ),
            (
                "code_challenge_methods".to_owned(),
                serde_json::json!(["S256"]),
            ),
        ]),
    };
    Json(document).into_response()
}

fn supported_methods() -> Vec<i32> {
    vec![AuthMethod::OauthManual as i32]
}

fn display_safe_provider_metadata(
    metadata: std::collections::BTreeMap<String, String>,
) -> HashMap<String, String> {
    metadata
        .into_iter()
        .filter(|(key, value)| {
            key == "account_id"
                && !value.is_empty()
                && value.len() <= 256
                && !value.chars().any(char::is_control)
        })
        .collect()
}

fn proto_provider_target(provider_key: &str) -> ProtoAuthTarget {
    ProtoAuthTarget {
        target: Some(auth_target::Target::ProviderKey(provider_key.to_owned())),
    }
}

fn proto_mcp_target(server_key: &str) -> ProtoAuthTarget {
    ProtoAuthTarget {
        target: Some(auth_target::Target::McpServerKey(server_key.to_owned())),
    }
}

fn auth_target_from_proto(target: Option<ProtoAuthTarget>) -> Result<AuthTarget, Status> {
    match target.and_then(|target| target.target) {
        Some(auth_target::Target::ProviderKey(key)) => Ok(AuthTarget::provider(key)),
        Some(auth_target::Target::McpServerKey(key)) => Ok(AuthTarget::mcp_server(key)),
        None => Err(Status::invalid_argument("auth target is required")),
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

const fn proto_registration_method(
    method: McpOAuthRegistrationMethod,
) -> waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod {
    use waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod as Proto;
    match method {
        McpOAuthRegistrationMethod::Preregistered => Proto::Preregistered,
        McpOAuthRegistrationMethod::ClientMetadata => Proto::ClientMetadata,
        McpOAuthRegistrationMethod::Dynamic => Proto::Dynamic,
        McpOAuthRegistrationMethod::Unknown => Proto::Unspecified,
    }
}

const fn registration_method_name(method: McpOAuthRegistrationMethod) -> &'static str {
    match method {
        McpOAuthRegistrationMethod::Preregistered => "preregistered",
        McpOAuthRegistrationMethod::ClientMetadata => "client_metadata",
        McpOAuthRegistrationMethod::Dynamic => "dynamic",
        McpOAuthRegistrationMethod::Unknown => "unknown",
    }
}

fn parse_proto_registration_method(
    method: &str,
) -> waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod {
    use waltr::grpc::waltr::auth::v1::McpOAuthRegistrationMethod as Proto;
    match method {
        "preregistered" => Proto::Preregistered,
        "client_metadata" => Proto::ClientMetadata,
        "dynamic" => Proto::Dynamic,
        _ => Proto::Unspecified,
    }
}

const fn completion_mode_name(mode: AuthCompletionMode) -> &'static str {
    match mode {
        AuthCompletionMode::ServerCallback => "server_callback",
        AuthCompletionMode::Manual => "manual",
        AuthCompletionMode::Unspecified => "unspecified",
    }
}

fn parse_completion_mode(mode: &str) -> AuthCompletionMode {
    match mode {
        "server_callback" => AuthCompletionMode::ServerCallback,
        "manual" => AuthCompletionMode::Manual,
        _ => AuthCompletionMode::Unspecified,
    }
}

fn attempt_lifecycle(record: &DurableAttemptRecord) -> AuthAttemptLifecycle {
    if record.status == "pending" && record.expires_at_ms < now_ms() {
        return AuthAttemptLifecycle::Expired;
    }
    match record.status.as_str() {
        "pending" => AuthAttemptLifecycle::Pending,
        "completing" => AuthAttemptLifecycle::Completing,
        "succeeded" => AuthAttemptLifecycle::Succeeded,
        "failed" => AuthAttemptLifecycle::Failed,
        "canceled" => AuthAttemptLifecycle::Canceled,
        "expired" => AuthAttemptLifecycle::Expired,
        _ => AuthAttemptLifecycle::Unspecified,
    }
}

fn target_from_credential_key(key: &str) -> Result<AuthTarget, Status> {
    key.strip_prefix("mcp:")
        .map(AuthTarget::mcp_server)
        .or_else(|| key.strip_prefix("provider:").map(AuthTarget::provider))
        .ok_or_else(|| Status::data_loss("stored OAuth attempt target is invalid"))
}

fn sanitize_oauth_failure(value: &str) -> String {
    match value {
        "access_denied" => "authorization denied".to_owned(),
        "temporarily_unavailable" => "authorization server temporarily unavailable".to_owned(),
        _ => "authorization failed".to_owned(),
    }
}

fn sanitized_completion_failure(status: &Status) -> &'static str {
    match status.code() {
        tonic::Code::DeadlineExceeded => "authorization attempt expired",
        tonic::Code::InvalidArgument => "authorization response was invalid",
        tonic::Code::Unavailable => "authorization server unavailable",
        tonic::Code::ResourceExhausted | tonic::Code::DataLoss => {
            "authorization server response was invalid"
        }
        _ => "authorization failed",
    }
}

fn public_authorization_server_compatible(metadata: &mcp::McpAuthorizationServerMetadata) -> bool {
    metadata.authorization_endpoint.is_some()
        && metadata.token_endpoint.is_some()
        && (metadata.grant_types_supported.is_empty()
            || metadata
                .grant_types_supported
                .iter()
                .any(|grant| grant == "authorization_code"))
        && metadata
            .response_types_supported
            .iter()
            .any(|response| response == "code")
        && metadata
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
        && metadata
            .token_endpoint_auth_methods_supported
            .iter()
            .any(|method| method == "none")
}

fn mcp_oauth_status(
    server_key: &str,
    current_url: &str,
    credential: &OAuthCredential,
    reauthorization_required: bool,
) -> AuthStatus {
    let context = credential
        .mcp_context()
        .expect("MCP status requires MCP credential context");
    let mut metadata = std::collections::HashMap::new();
    if !context.resource.is_empty() {
        metadata.insert("resource".to_owned(), context.resource.clone());
    }
    if !context.issuer.is_empty() {
        metadata.insert("issuer".to_owned(), context.issuer.clone());
    }
    metadata.insert(
        "registration_method".to_owned(),
        match context.registration.method {
            McpOAuthRegistrationMethod::Preregistered => "preregistered",
            McpOAuthRegistrationMethod::ClientMetadata => "client_metadata",
            McpOAuthRegistrationMethod::Dynamic => "dynamic",
            McpOAuthRegistrationMethod::Unknown => "unknown",
        }
        .to_owned(),
    );
    if !credential.token.scopes.is_empty() {
        metadata.insert(
            "scope".to_owned(),
            credential
                .token
                .scopes
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    let resource_matches = context.resource == current_url;
    let authorization_required = reauthorization_required
        || !resource_matches
        || credential.token.access_token.secret().is_empty();
    let expires_at_ms = credential.token.expires_at_ms;
    let can_refresh =
        !reauthorization_required && resource_matches && credential.token.refresh_token.is_some();
    AuthStatus {
        target: Some(proto_mcp_target(server_key)),
        kind: if authorization_required {
            AuthStatusKind::AuthorizationRequired as i32
        } else if expires_at_ms.is_some_and(|expiry| expiry <= now_ms()) {
            AuthStatusKind::Expired as i32
        } else {
            AuthStatusKind::OauthConfigured as i32
        },
        supported_methods: vec![AuthMethod::OauthManual as i32],
        expires_at_ms: expires_at_ms.unwrap_or_default(),
        registration_method: proto_registration_method(context.registration.method) as i32,
        metadata,
        message: if reauthorization_required {
            "MCP OAuth credential requires reauthorization".to_owned()
        } else if !resource_matches {
            "MCP OAuth credential is bound to a different resource".to_owned()
        } else {
            String::new()
        },
        can_refresh,
    }
}

async fn revoke_mcp_credential_with_policy(
    credential: &OAuthCredential,
    policy: OutboundHttpPolicy,
) {
    let Some(context) = credential.mcp_context() else {
        return;
    };
    let Some(endpoint) = context.endpoints.revocation.as_deref() else {
        return;
    };
    let (token, token_type_hint) = credential.token.refresh_token.as_ref().map_or_else(
        || {
            (
                credential.token.access_token.secret(),
                McpRevocationTokenTypeHint::AccessToken,
            )
        },
        |refresh_token| {
            (
                refresh_token.secret(),
                McpRevocationTokenTypeHint::RefreshToken,
            )
        },
    );
    if token.is_empty() {
        return;
    }
    let request = McpRevocationRequest {
        token: McpOAuthSecret::new(token),
        token_type_hint: Some(token_type_hint),
        client_id: context.registration.client_id.clone(),
    };
    let Ok((endpoint, client)) = policy.client_for(endpoint).await else {
        return;
    };
    if let Ok(response) = client.post(endpoint).form(&request).send().await {
        drop(validate_response_peer(&policy, &response));
    }
}

fn configured_mcp_registration(
    outbound_oauth: &OutboundOAuthConfig,
    server_key: &str,
    default_redirect_uri: &str,
    discovered_issuer: &str,
) -> Result<Option<McpOAuthRegistration>, Status> {
    let key = environment_server_key(server_key);
    let prefix = format!("WALTR_MCP_{key}_OAUTH");
    let Some(client_id) = outbound_oauth.variable(&format!("{prefix}_CLIENT_ID")) else {
        return Ok(None);
    };
    let redirect_uri = outbound_oauth
        .variable(&format!("{prefix}_REDIRECT_URI"))
        .unwrap_or_else(|| default_redirect_uri.to_owned());
    let configured_issuer = outbound_oauth.variable(&format!("{prefix}_ISSUER"));
    if redirect_uri.trim().is_empty() || configured_issuer.as_deref() != Some(discovered_issuer) {
        return Err(outbound_failure_status(
            &AuthTarget::mcp_server(server_key),
            tonic::Code::FailedPrecondition,
            AuthenticationFailureReason::ConfigurationRequired,
            format!(
                "MCP OAuth public client registration is incomplete or issuer-mismatched for '{server_key}'"
            ),
        ));
    }
    Ok(Some(McpOAuthRegistration {
        method: McpOAuthRegistrationMethod::Preregistered,
        client_id,
        redirect_uri,
        token_endpoint_auth_method: "none".to_owned(),
        created_at_ms: None,
        expires_at_ms: None,
    }))
}

fn environment_server_key(server_key: &str) -> String {
    server_key
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect()
}

fn missing_redirect_uri(server_key: &str) -> Status {
    outbound_failure_status(
        &AuthTarget::mcp_server(server_key),
        tonic::Code::FailedPrecondition,
        AuthenticationFailureReason::ConfigurationRequired,
        "WALTR_PUBLIC_URL or an MCP OAuth redirect URI is required".to_owned(),
    )
}

fn public_url_with_path(public_url: &str, path: &str) -> Option<String> {
    let mut url = url::Url::parse(public_url).ok()?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }
    let base = url.path().trim_end_matches('/');
    url.set_path(&format!("{base}{path}"));
    url.set_query(None);
    url.set_fragment(None);
    Some(url.to_string())
}

async fn discover_mcp_auth(
    target: &AuthTarget,
    endpoint: &str,
    policy: OutboundHttpPolicy,
) -> Result<McpAuthDiscovery, Status> {
    policy
        .validate_destination(endpoint)
        .await
        .map_err(|status| outbound_oauth_status(target, &status))?;
    let challenge = request_mcp_challenge(policy.clone(), endpoint)
        .await
        .map_err(|status| outbound_oauth_status(target, &status))?;
    if let Some(metadata_url) = challenge.resource_metadata.as_deref() {
        policy
            .validate_destination(metadata_url)
            .await
            .map_err(|status| outbound_oauth_status(target, &status))?;
    }
    let protected = discover_protected_resource(policy.clone(), endpoint, &challenge)
        .await
        .map_err(|status| outbound_oauth_status(target, &status))?;
    for issuer in &protected.authorization_servers {
        if policy.validate_destination(issuer).await.is_err() {
            continue;
        }
        let Ok(authorization_server) = discover_authorization_server(policy.clone(), issuer).await
        else {
            continue;
        };
        if public_authorization_server_compatible(&authorization_server)
            && validate_authorization_server_destinations(policy.clone(), &authorization_server)
                .await
                .is_ok()
        {
            return Ok(McpAuthDiscovery {
                challenge,
                protected,
                authorization_server,
            });
        }
    }
    Err(outbound_failure_status(
        target,
        tonic::Code::FailedPrecondition,
        AuthenticationFailureReason::ConfigurationRequired,
        "MCP protected resource has no compatible public authorization server".to_owned(),
    ))
}

fn required_authorization_endpoints(
    target: &AuthTarget,
    server: &mcp::McpAuthorizationServerMetadata,
) -> Result<(String, String), Status> {
    let authorization = server.authorization_endpoint.clone().ok_or_else(|| {
        outbound_failure_status(
            target,
            tonic::Code::FailedPrecondition,
            AuthenticationFailureReason::ConfigurationRequired,
            "MCP OAuth authorization server metadata is missing authorization_endpoint".to_owned(),
        )
    })?;
    let token = server.token_endpoint.clone().ok_or_else(|| {
        outbound_failure_status(
            target,
            tonic::Code::FailedPrecondition,
            AuthenticationFailureReason::ConfigurationRequired,
            "MCP OAuth authorization server metadata is missing token_endpoint".to_owned(),
        )
    })?;
    Ok((authorization, token))
}

async fn request_mcp_challenge(
    policy: OutboundHttpPolicy,
    endpoint: &str,
) -> Result<McpBearerChallenge, Status> {
    let (endpoint, client) = policy.client_for(endpoint).await?;
    let response = client
        .post(endpoint)
        .header("accept", "application/json, text/event-stream")
        .header("content-type", "application/json")
        .header("mcp-protocol-version", mcp::protocol::MCP_PROTOCOL_VERSION)
        .header("mcp-method", "server/discover")
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "waltr-oauth-discovery",
            "method": "server/discover",
            "params": {
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": mcp::protocol::MCP_PROTOCOL_VERSION,
                    "io.modelcontextprotocol/clientCapabilities": {},
                    "io.modelcontextprotocol/clientInfo": {
                        "name": "waltr",
                        "version": env!("CARGO_PKG_VERSION")
                    }
                }
            }
        }))
        .send()
        .await
        .map_err(|error| {
            Status::unavailable(format!("failed to request MCP OAuth challenge: {error}"))
        })?;
    validate_response_peer(&policy, &response)?;
    if !matches!(response.status().as_u16(), 401 | 403) {
        return Err(Status::failed_precondition(
            "MCP endpoint did not return an OAuth bearer challenge",
        ));
    }
    McpBearerChallenge::parse_any(
        response
            .headers()
            .get_all(reqwest::header::WWW_AUTHENTICATE)
            .iter()
            .filter_map(|value| value.to_str().ok()),
    )
    .ok_or_else(|| Status::failed_precondition("MCP endpoint returned no valid bearer challenge"))
}

async fn discover_protected_resource(
    policy: OutboundHttpPolicy,
    endpoint: &str,
    challenge: &McpBearerChallenge,
) -> Result<mcp::McpProtectedResourceMetadata, Status> {
    let destinations = challenge
        .resource_metadata
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .map_or_else(
            || protected_resource_metadata_urls(endpoint),
            |value| vec![value.clone()],
        );
    for destination in destinations {
        let Ok(metadata) =
            get_oauth_json::<mcp::McpProtectedResourceMetadata>(policy.clone(), &destination).await
        else {
            continue;
        };
        if metadata.validate_for_mcp_resource(endpoint).is_ok() {
            return Ok(metadata);
        }
    }
    Err(Status::failed_precondition(
        "MCP protected resource metadata discovery failed",
    ))
}

async fn discover_authorization_server(
    policy: OutboundHttpPolicy,
    issuer: &str,
) -> Result<mcp::McpAuthorizationServerMetadata, Status> {
    for destination in authorization_server_metadata_urls(issuer) {
        let Ok(metadata) =
            get_oauth_json::<mcp::McpAuthorizationServerMetadata>(policy.clone(), &destination)
                .await
        else {
            continue;
        };
        if metadata.issuer == issuer {
            return Ok(metadata);
        }
    }
    Err(Status::failed_precondition(
        "MCP authorization server metadata discovery failed",
    ))
}

async fn get_oauth_json<T: serde::de::DeserializeOwned>(
    policy: OutboundHttpPolicy,
    destination: &str,
) -> Result<T, Status> {
    let (destination, client) = policy.client_for(destination).await?;
    let response = client
        .get(destination)
        .send()
        .await
        .map_err(|_| Status::unavailable("OAuth discovery request failed"))?;
    validate_response_peer(&policy, &response)?;
    if !response.status().is_success() {
        return Err(Status::failed_precondition(
            "OAuth discovery endpoint returned an error",
        ));
    }
    let body = read_json_body(response, OAUTH_RESPONSE_LIMIT).await?;
    serde_json::from_slice(&body)
        .map_err(|_| Status::data_loss("OAuth discovery response was invalid"))
}

async fn register_public_client_hardened(
    policy: OutboundHttpPolicy,
    endpoint: &str,
    request: &McpPublicClientRegistrationRequest,
) -> Result<mcp::McpClientRegistrationResponse, Status> {
    let (endpoint, client) = policy.client_for(endpoint).await?;
    let response = client
        .post(endpoint)
        .json(request)
        .send()
        .await
        .map_err(|_| Status::unavailable("OAuth client registration request failed"))?;
    validate_response_peer(&policy, &response)?;
    if !response.status().is_success() {
        return Err(Status::failed_precondition(
            "OAuth client registration endpoint returned an error",
        ));
    }
    let body = read_json_body(response, OAUTH_RESPONSE_LIMIT).await?;
    let registration: mcp::McpClientRegistrationResponse = serde_json::from_slice(&body)
        .map_err(|_| Status::data_loss("OAuth client registration response was invalid"))?;
    registration
        .validate_public_response(request)
        .map_err(|_| {
            Status::failed_precondition("OAuth client registration response was unsafe")
        })?;
    Ok(registration)
}

async fn validate_authorization_server_destinations(
    policy: OutboundHttpPolicy,
    metadata: &mcp::McpAuthorizationServerMetadata,
) -> Result<(), Status> {
    policy.validate_destination(&metadata.issuer).await?;
    for endpoint in [
        metadata.authorization_endpoint.as_deref(),
        metadata.token_endpoint.as_deref(),
        metadata.revocation_endpoint.as_deref(),
        metadata.registration_endpoint.as_deref(),
        metadata.jwks_uri.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        policy.validate_destination(endpoint).await?;
    }
    Ok(())
}

fn outbound_oauth_status(target: &AuthTarget, status: &Status) -> Status {
    let reason = match status.code() {
        tonic::Code::Unavailable | tonic::Code::DeadlineExceeded => {
            AuthenticationFailureReason::NetworkFailed
        }
        tonic::Code::DataLoss | tonic::Code::ResourceExhausted => {
            AuthenticationFailureReason::ResponseInvalid
        }
        tonic::Code::FailedPrecondition | tonic::Code::PermissionDenied => {
            AuthenticationFailureReason::ConfigurationRequired
        }
        _ => AuthenticationFailureReason::TokenExchangeFailed,
    };
    outbound_failure_status(target, status.code(), reason, status.message().to_owned())
}

impl From<&CredentialRefreshError> for RefreshFailure {
    fn from(error: &CredentialRefreshError) -> Self {
        Self {
            reason: error.reason(),
            message: error.message().to_owned(),
            internal: error.is_internal(),
        }
    }
}

fn refresh_failure_status(target: &AuthTarget, failure: &RefreshFailure) -> Status {
    let reason = match failure.reason {
        AgentAuthenticationFailureReason::CredentialRequired => {
            AuthenticationFailureReason::CredentialRequired
        }
        AgentAuthenticationFailureReason::CredentialRejected => {
            AuthenticationFailureReason::CredentialRejected
        }
        AgentAuthenticationFailureReason::TokenExchangeFailed => {
            AuthenticationFailureReason::TokenExchangeFailed
        }
        AgentAuthenticationFailureReason::NetworkFailed => {
            AuthenticationFailureReason::NetworkFailed
        }
        AgentAuthenticationFailureReason::ResponseInvalid => {
            AuthenticationFailureReason::ResponseInvalid
        }
        AgentAuthenticationFailureReason::ConfigurationRequired => {
            AuthenticationFailureReason::ConfigurationRequired
        }
    };
    let code = if failure.internal {
        tonic::Code::Internal
    } else {
        match failure.reason {
            AgentAuthenticationFailureReason::CredentialRequired
            | AgentAuthenticationFailureReason::ConfigurationRequired => {
                tonic::Code::FailedPrecondition
            }
            AgentAuthenticationFailureReason::CredentialRejected
            | AgentAuthenticationFailureReason::TokenExchangeFailed => tonic::Code::Unauthenticated,
            AgentAuthenticationFailureReason::NetworkFailed => tonic::Code::Unavailable,
            AgentAuthenticationFailureReason::ResponseInvalid => tonic::Code::DataLoss,
        }
    };
    outbound_failure_status(target, code, reason, failure.message.clone())
}

fn outbound_failure_status(
    target: &AuthTarget,
    code: tonic::Code,
    reason: AuthenticationFailureReason,
    message: String,
) -> Status {
    let (domain, target) = match target {
        AuthTarget::Provider { key } => (
            AuthenticationDomain::OutboundProvider,
            proto_provider_target(key),
        ),
        AuthTarget::McpServer { key } => (AuthenticationDomain::OutboundMcp, proto_mcp_target(key)),
    };
    let failure = AuthenticationFailure {
        version: 1,
        domain: domain as i32,
        reason: reason as i32,
        target: Some(target),
        mcp_authorization: None,
    };
    Status::with_details(code, message, failure.encode_to_vec().into())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::{Router, routing::post};
    use tower::ServiceExt as _;

    use super::*;
    use crate::{
        local_admission::{LocalBearer, LocalCredentialStore},
        persistence::{DatabaseConfig, Persistence},
    };

    #[test]
    fn missing_per_server_mcp_registration_allows_discovery_mechanisms() {
        let key = "registration-definitely-not-configured-7f4332";
        let registration = configured_mcp_registration(
            &OutboundOAuthConfig::default(),
            key,
            "https://waltr.example/callback",
            "https://issuer.example",
        )
        .expect("missing preregistration is not an error");
        assert!(registration.is_none());
    }

    #[test]
    fn environment_oauth_config_preserves_public_and_per_target_redirects() {
        let public = OutboundOAuthConfig::from_environment(HashMap::from([(
            "WALTR_PUBLIC_URL".into(),
            "https://waltr.example/base".into(),
        )]));
        assert_eq!(
            public.redirect_uri("remote").unwrap(),
            "https://waltr.example/base/outbound-oauth/callback"
        );
        assert_eq!(
            public.client_metadata_uri.as_deref(),
            Some("https://waltr.example/base/.well-known/oauth-client/waltr")
        );

        let target = OutboundOAuthConfig::from_environment(HashMap::from([(
            "WALTR_MCP_REMOTE_SERVER_OAUTH_REDIRECT_URI".into(),
            "http://127.0.0.1/manual-callback".into(),
        )]));
        assert_eq!(
            target.redirect_uri("remote-server").unwrap(),
            "http://127.0.0.1/manual-callback"
        );
        assert!(target.server_callback_uri.is_none());
    }

    #[test]
    fn hostile_environment_is_redacted_and_embedded_private_policy_stays_off() {
        let config = OutboundOAuthConfig::from_environment(HashMap::from([
            ("WALTR_PUBLIC_URL".into(), "https://waltr.example".into()),
            (
                "WALTR_MCP_REMOTE_OAUTH_CLIENT_ID".into(),
                "SENTINEL_ALLOWED_VALUE".into(),
            ),
            (
                "UNRELATED_SECRET".into(),
                "SENTINEL_UNRELATED_SECRET".into(),
            ),
            (
                "WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS".into(),
                "true".into(),
            ),
        ]));
        assert!(config.allow_private_test_destinations);
        assert!(config.variable("UNRELATED_SECRET").is_none());
        assert_eq!(
            config
                .variable("WALTR_MCP_REMOTE_OAUTH_CLIENT_ID")
                .as_deref(),
            Some("SENTINEL_ALLOWED_VALUE")
        );
        let debug = format!("{config:?}");
        assert!(!debug.contains("SENTINEL_ALLOWED_VALUE"));
        assert!(!debug.contains("SENTINEL_UNRELATED_SECRET"));
        assert!(!debug.contains("https://waltr.example"));

        let embedded = OutboundOAuthConfig::for_server_callback(
            "http://127.0.0.1:43123/outbound-oauth/callback".to_owned(),
        );
        assert!(!embedded.allow_private_test_destinations);
    }

    #[test]
    fn typed_mcp_status_reports_only_non_secret_metadata() {
        let credential = OAuthCredential::mcp(
            OAuthTokenSet {
                access_token: oauth2::AccessToken::new("access-secret".to_owned()),
                refresh_token: Some(oauth2::RefreshToken::new("refresh-secret".to_owned())),
                token_type: "Bearer".to_owned(),
                scopes: vec![
                    oauth2::Scope::new("read".to_owned()),
                    oauth2::Scope::new("write".to_owned()),
                ],
                expires_at_ms: None,
            },
            McpOAuthContext {
                resource: "https://mcp.example/resource".to_owned(),
                issuer: "https://auth.example".to_owned(),
                registration: waltr_agent::McpOAuthRegistration {
                    method: McpOAuthRegistrationMethod::Dynamic,
                    client_id: "public-client".to_owned(),
                    ..waltr_agent::McpOAuthRegistration::default()
                },
                endpoints: waltr_agent::McpOAuthEndpoints {
                    authorization: "https://auth.example/authorize".to_owned(),
                    token: "https://auth.example/token".to_owned(),
                    revocation: Some("https://auth.example/revoke".to_owned()),
                },
            },
        );

        let status = mcp_oauth_status("remote", "https://mcp.example/resource", &credential, false);

        assert_eq!(status.kind, AuthStatusKind::OauthConfigured as i32);
        assert_eq!(status.supported_methods, [AuthMethod::OauthManual as i32]);
        assert_eq!(status.expires_at_ms, 0);
        assert!(status.can_refresh);
        assert_eq!(
            status.metadata.get("scope").map(String::as_str),
            Some("read write")
        );
        assert_eq!(
            status
                .metadata
                .get("registration_method")
                .map(String::as_str),
            Some("dynamic")
        );
        let rendered = format!("{status:?}");
        assert!(!rendered.contains("access-secret"));
        assert!(!rendered.contains("refresh-secret"));
        assert!(!rendered.contains("public-client"));
        assert!(!rendered.contains("/token"));
        assert!(!rendered.contains("/revoke"));

        let mut expired = credential;
        expired.token.expires_at_ms = Some(0);
        let status = mcp_oauth_status("remote", "https://mcp.example/resource", &expired, false);
        assert_eq!(status.kind, AuthStatusKind::Expired as i32);
        assert_eq!(status.supported_methods, [AuthMethod::OauthManual as i32]);
    }

    #[test]
    fn registration_only_status_and_attempt_failures_are_sanitized() {
        let credential = OAuthCredential::mcp(
            OAuthTokenSet::bearer("", None, None),
            McpOAuthContext {
                resource: "https://mcp.example/resource".to_owned(),
                issuer: "https://auth.example".to_owned(),
                registration: McpOAuthRegistration {
                    method: McpOAuthRegistrationMethod::ClientMetadata,
                    client_id: "https://waltr.example/.well-known/oauth-client/waltr".to_owned(),
                    ..McpOAuthRegistration::default()
                },
                endpoints: McpOAuthEndpoints::default(),
            },
        );
        let status = mcp_oauth_status("remote", "https://mcp.example/resource", &credential, false);
        assert_eq!(status.kind, AuthStatusKind::AuthorizationRequired as i32);
        assert_eq!(status.supported_methods, [AuthMethod::OauthManual as i32]);
        assert!(!status.can_refresh);
        assert_eq!(
            sanitized_completion_failure(&Status::unavailable(
                "request to https://secret.example/token?code=secret failed"
            )),
            "authorization server unavailable"
        );
    }

    #[test]
    fn mcp_status_requires_reauthorization_for_durable_flag_or_resource_mismatch() {
        let credential = OAuthCredential::mcp(
            OAuthTokenSet::bearer("access-secret", Some("refresh-secret".to_owned()), None),
            McpOAuthContext {
                resource: "https://mcp.example/old".to_owned(),
                issuer: "https://auth.example".to_owned(),
                registration: McpOAuthRegistration::default(),
                endpoints: McpOAuthEndpoints::default(),
            },
        );

        let mismatch =
            mcp_oauth_status("remote", "https://mcp.example/current", &credential, false);
        assert_eq!(mismatch.kind, AuthStatusKind::AuthorizationRequired as i32);
        assert!(!mismatch.can_refresh);
        assert!(mismatch.message.contains("different resource"));

        let durable = mcp_oauth_status("remote", "https://mcp.example/old", &credential, true);
        assert_eq!(durable.kind, AuthStatusKind::AuthorizationRequired as i32);
        assert!(!durable.can_refresh);
        assert!(durable.message.contains("requires reauthorization"));
    }

    async fn generic_auth_service() -> Arc<AuthService> {
        let resources = crate::test_support::resource_state_with_overrides(&[(
            "mcp-servers/remote.yaml",
            "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: remote\nspec:\n  transport: streamableHttp\n  url: https://mcp.example/resource\n",
        )]);
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let outbound_store = persistence.outbound_oauth_store();
        let credential_store = CredentialStore::shared(outbound_store.clone());
        credential_store
            .write(
                &AuthTarget::provider("codex").credential_key(),
                OAuthCredential::provider(
                    OAuthTokenSet::bearer(
                        "provider-access-secret",
                        Some("provider-refresh-secret".to_owned()),
                        None,
                    ),
                    std::collections::BTreeMap::new(),
                ),
            )
            .await
            .unwrap();
        credential_store
            .write(
                &AuthTarget::mcp_server("remote").credential_key(),
                OAuthCredential::mcp(
                    OAuthTokenSet::bearer("mcp-access-secret", None, None),
                    McpOAuthContext {
                        resource: "https://mcp.example/resource".to_owned(),
                        issuer: "https://auth.example".to_owned(),
                        registration: McpOAuthRegistration::default(),
                        endpoints: McpOAuthEndpoints::default(),
                    },
                ),
            )
            .await
            .unwrap();
        AuthService::new(
            resources,
            credential_store,
            outbound_store,
            Vec::new(),
            None,
            OutboundOAuthConfig::default(),
        )
    }

    #[tokio::test]
    async fn provider_status_requires_reauthorization_after_indeterminate_refresh() {
        let service = generic_auth_service().await;
        let target_key = AuthTarget::provider("codex").credential_key();
        let record = service
            .credential_store
            .read(&target_key)
            .await
            .unwrap()
            .unwrap();
        let claim = OAuthRefreshClaim {
            revision: record.revision,
            claim_id: "provider-status-claim".to_owned(),
            lease_expires_at_ms: i64::MAX,
        };
        OAuthCredentialStore::claim_refresh(
            service.outbound_store.as_ref(),
            target_key.clone(),
            claim.clone(),
        )
        .await
        .unwrap();
        OAuthCredentialStore::abort_refresh(
            service.outbound_store.as_ref(),
            target_key,
            claim,
            true,
        )
        .await
        .unwrap();

        let status = service.provider_status_for("codex").await.unwrap();

        assert_eq!(status.kind, AuthStatusKind::AuthorizationRequired as i32);
        assert!(!status.can_refresh);
        assert!(status.message.contains("requires reauthorization"));
    }

    fn successful_oauth_router(
        base: &str,
        resource: &str,
        registration_requests: Arc<AtomicUsize>,
    ) -> Router {
        use axum::{http::header, routing::get};

        let resource_metadata = format!("{base}/resource-metadata");
        let advertised_resource = resource.to_owned();
        let resource_issuer = base.to_owned();
        let metadata_issuer = base.to_owned();
        let registration_endpoint = format!("{base}/register");
        Router::new()
            .route(
                "/mcp",
                post(move || {
                    let challenge_metadata = resource_metadata.clone();
                    async move {
                        (
                            StatusCode::UNAUTHORIZED,
                            [(
                                header::WWW_AUTHENTICATE,
                                format!("Bearer resource_metadata=\"{challenge_metadata}\""),
                            )],
                        )
                    }
                }),
            )
            .route(
                "/resource-metadata",
                get(move || {
                    let resource = advertised_resource.clone();
                    let issuer = resource_issuer.clone();
                    async move {
                        Json(serde_json::json!({
                            "resource": resource,
                            "authorization_servers": [issuer]
                        }))
                    }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(move || {
                    let issuer = metadata_issuer.clone();
                    let registration = registration_endpoint.clone();
                    async move {
                        Json(serde_json::json!({
                            "issuer": issuer,
                            "authorization_endpoint": format!("{issuer}/authorize"),
                            "token_endpoint": format!("{issuer}/token"),
                            "registration_endpoint": registration,
                            "client_id_metadata_document_supported": true,
                            "response_types_supported": ["code"],
                            "grant_types_supported": ["authorization_code", "refresh_token"],
                            "code_challenge_methods_supported": ["S256"],
                            "token_endpoint_auth_methods_supported": ["none"]
                        }))
                    }
                }),
            )
            .route(
                "/register",
                post(
                    move |Json(request): Json<McpPublicClientRegistrationRequest>| {
                        let registration_requests = Arc::clone(&registration_requests);
                        async move {
                            registration_requests.fetch_add(1, Ordering::SeqCst);
                            Json(serde_json::json!({
                                "client_id": "local-public-client",
                                "application_type": request.application_type,
                                "redirect_uris": request.redirect_uris,
                                "grant_types": request.grant_types,
                                "response_types": request.response_types,
                                "token_endpoint_auth_method": request.token_endpoint_auth_method,
                                "client_name": request.client_name
                            }))
                        }
                    },
                ),
            )
            .route(
                "/token",
                post(|| async {
                    Json(serde_json::json!({
                        "access_token": "local-access-secret",
                        "refresh_token": "local-refresh-secret",
                        "token_type": "Bearer",
                        "expires_in": 3600
                    }))
                }),
            )
    }

    async fn successful_lifecycle_service() -> (
        Arc<AuthService>,
        tokio::task::JoinHandle<Result<(), std::io::Error>>,
        Arc<AtomicUsize>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let resource = format!("{base}/mcp");
        let registration_requests = Arc::new(AtomicUsize::new(0));
        let app = successful_oauth_router(&base, &resource, Arc::clone(&registration_requests));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });

        let codex = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: codex\nspec:\n  transport: openaiResponses\n  url: https://chatgpt.com/backend-api/codex/responses\n  oauth:\n    name: Test Codex\n    authorizeUrl: {base}/authorize\n    tokenUrl: {base}/token\n    clientId: test-client\n    redirectUri: http://127.0.0.1/provider-callback\n    tokenRequestStyle: formUrlencoded\n  catalog:\n    provider: openai\n    include: [gpt-5.*]\n"
        );
        let remote = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: remote\nspec:\n  transport: streamableHttp\n  url: {resource}\n"
        );
        let resources = crate::test_support::resource_state_with_overrides(&[
            ("providers/codex.yaml", codex.as_str()),
            ("mcp-servers/remote.yaml", remote.as_str()),
        ]);
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let outbound_store = persistence.outbound_oauth_store();
        let credentials = CredentialStore::shared(outbound_store.clone());
        let service = Arc::new(AuthService {
            resources,
            credential_store: credentials,
            outbound_store,
            attempts: Mutex::new(HashMap::new()),
            outbound_policy: OutboundHttpPolicy::loopback_test(),
            outbound_oauth: OutboundOAuthConfig {
                server_callback_uri: Some(
                    "http://127.0.0.1:43123/outbound-oauth/callback".to_owned(),
                ),
                ..OutboundOAuthConfig::default()
            },
            self_mcp_credential_ownership: CredentialOwnership::Durable,
            refresh_outcomes: Mutex::new(HashMap::new()),
        });
        (service, server, registration_requests)
    }

    fn state_from_auth_url(auth_url: &str) -> String {
        url::Url::parse(auth_url)
            .unwrap()
            .query_pairs()
            .find_map(|(key, value)| (key == "state").then(|| value.into_owned()))
            .unwrap()
    }

    fn embedded_auth_service(
        resources: AgentDefinitionState,
        outbound_store: Arc<OutboundOAuthStore>,
        base: &str,
        bearer_value: &str,
    ) -> Arc<AuthService> {
        let self_mcp_url = format!("{base}/mcp");
        let outbound_oauth =
            OutboundOAuthConfig::for_server_callback(format!("{base}/outbound-oauth/callback"));
        let credential_store = CredentialStore::new(LocalCredentialStore::new(
            Arc::clone(&outbound_store),
            &LocalBearer::new(bearer_value).unwrap(),
            &self_mcp_url,
            outbound_oauth
                .durable_callback_marker()
                .map(ToOwned::to_owned),
        ));
        Arc::new(AuthService {
            resources,
            credential_store,
            outbound_store,
            attempts: Mutex::new(HashMap::new()),
            outbound_policy: OutboundHttpPolicy::loopback_test(),
            outbound_oauth,
            self_mcp_credential_ownership: CredentialOwnership::EmbeddedHost,
            refresh_outcomes: Mutex::new(HashMap::new()),
        })
    }

    async fn assert_non_destructive_mcp_reauthorization(service: &AuthService) {
        let (reauthorization, _) = oauth::begin_mcp(
            AuthTarget::mcp_server("remote"),
            "https://auth.example".to_owned(),
            "https://auth.example/authorize",
            "https://auth.example/token".to_owned(),
            None,
            McpOAuthRegistration {
                client_id: "replacement-registration".to_owned(),
                redirect_uri: "https://waltr.example/outbound-oauth/callback".to_owned(),
                ..McpOAuthRegistration::default()
            },
            Vec::new(),
            "https://mcp.example/resource".to_owned(),
            false,
            now_ms(),
        )
        .unwrap();
        service
            .save_mcp_attempt(&AuthTarget::mcp_server("remote"), &reauthorization)
            .await
            .unwrap();
        let still_usable = service
            .credential_store
            .read(&AuthTarget::mcp_server("remote").credential_key())
            .await
            .unwrap()
            .expect("existing MCP credential must remain installed")
            .credential;
        assert_eq!(
            still_usable.token.access_token.secret(),
            "mcp-access-secret"
        );
    }

    async fn assert_generic_target_lifecycle(service: &AuthService, target: ProtoAuthTarget) {
        let begin = GeneratedAuthService::begin_auth(
            service,
            Request::new(BeginAuthRequest {
                target: Some(target.clone()),
                method: AuthMethod::OauthDevice as i32,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(begin.code(), tonic::Code::FailedPrecondition);

        let reset = GeneratedAuthService::logout(
            service,
            Request::new(LogoutRequest {
                target: Some(target.clone()),
            }),
        )
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
        assert_eq!(reset.kind, AuthStatusKind::NotConfigured as i32);

        let refreshed = GeneratedAuthService::refresh_auth(
            service,
            Request::new(RefreshAuthRequest {
                target: Some(target),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(refreshed.code(), tonic::Code::FailedPrecondition);
    }

    #[tokio::test]
    async fn generic_handlers_cover_provider_and_mcp_status_cancel_reset_and_refresh_errors() {
        let service = generic_auth_service().await;
        assert_non_destructive_mcp_reauthorization(service.as_ref()).await;

        let listed = GeneratedAuthService::list_auth_statuses(
            service.as_ref(),
            Request::new(ListAuthStatusesRequest {}),
        )
        .await
        .unwrap()
        .into_inner();
        assert!(listed.statuses.iter().any(|status| {
            status.target == Some(proto_provider_target("codex"))
                && status.can_refresh
                && status.expires_at_ms == 0
        }));
        assert!(listed.statuses.iter().any(|status| {
            status.target == Some(proto_mcp_target("remote")) && !status.can_refresh
        }));

        for target in [proto_provider_target("codex"), proto_mcp_target("remote")] {
            assert_generic_target_lifecycle(service.as_ref(), target).await;
        }

        let complete = GeneratedAuthService::complete_auth(
            service.as_ref(),
            Request::new(CompleteAuthRequest {
                attempt_id: "missing".to_owned(),
                input: "code".to_owned(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(complete.code(), tonic::Code::NotFound);
        GeneratedAuthService::cancel_auth(
            service.as_ref(),
            Request::new(CancelAuthRequest {
                attempt_id: "missing".to_owned(),
            }),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn generic_provider_and_mcp_begin_and_complete_succeed() {
        let (service, server, registration_requests) = successful_lifecycle_service().await;
        let provider_target = proto_provider_target("codex");
        let provider_begin = GeneratedAuthService::begin_auth(
            service.as_ref(),
            Request::new(BeginAuthRequest {
                target: Some(provider_target),
                method: AuthMethod::OauthManual as i32,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        let provider_state = state_from_auth_url(&provider_begin.auth_url);
        let provider_status = GeneratedAuthService::complete_auth(
            service.as_ref(),
            Request::new(CompleteAuthRequest {
                attempt_id: provider_begin.attempt_id,
                input: format!("code=provider-code&state={provider_state}"),
            }),
        )
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
        assert_eq!(provider_status.kind, AuthStatusKind::OauthConfigured as i32);
        assert!(provider_status.can_refresh);
        assert!(provider_status.metadata.is_empty());

        let mcp_begin = GeneratedAuthService::begin_auth(
            service.as_ref(),
            Request::new(BeginAuthRequest {
                target: Some(proto_mcp_target("remote")),
                method: AuthMethod::OauthManual as i32,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(
            mcp_begin.completion_mode,
            AuthCompletionMode::ServerCallback as i32
        );
        assert_eq!(
            mcp_begin.registration_method,
            proto_registration_method(McpOAuthRegistrationMethod::Dynamic) as i32
        );
        let attempt_id = mcp_begin.attempt_id.clone();
        let durable_attempt = service
            .outbound_store
            .attempt(&attempt_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            durable_attempt
                .attempt_json
                .contains("http://127.0.0.1:43123/outbound-oauth/callback")
        );
        let mcp_authorize_url = url::Url::parse(&mcp_begin.auth_url).unwrap();
        assert_eq!(
            mcp_authorize_url
                .query_pairs()
                .find_map(|(key, value)| (key == "redirect_uri").then(|| value.into_owned())),
            Some("http://127.0.0.1:43123/outbound-oauth/callback".to_owned())
        );
        let mcp_state = state_from_auth_url(&mcp_begin.auth_url);
        let mcp_status = GeneratedAuthService::complete_auth(
            service.as_ref(),
            Request::new(CompleteAuthRequest {
                attempt_id,
                input: format!("code=mcp-code&state={mcp_state}"),
            }),
        )
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
        assert_eq!(mcp_status.kind, AuthStatusKind::OauthConfigured as i32);
        assert!(mcp_status.can_refresh);
        let credential = service
            .credential_store
            .read(&AuthTarget::mcp_server("remote").credential_key())
            .await
            .unwrap()
            .unwrap()
            .credential;
        let durable_credential = serde_json::to_string(&credential).unwrap();
        assert!(durable_credential.contains("http://127.0.0.1:43123/outbound-oauth/callback"));

        let reauthorization = GeneratedAuthService::begin_auth(
            service.as_ref(),
            Request::new(BeginAuthRequest {
                target: Some(proto_mcp_target("remote")),
                method: AuthMethod::OauthManual as i32,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(
            reauthorization.registration_method,
            proto_registration_method(McpOAuthRegistrationMethod::Dynamic) as i32
        );
        assert_eq!(registration_requests.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn embedded_dcr_reuses_only_current_generation_registration() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let resource = format!("{base}/mcp");
        let registration_requests = Arc::new(AtomicUsize::new(0));
        let app = successful_oauth_router(&base, &resource, Arc::clone(&registration_requests));
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let remote = format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: remote\nspec:\n  transport: streamableHttp\n  url: {resource}\n"
        );
        let resources = crate::test_support::resource_state_with_overrides(&[(
            "mcp-servers/remote.yaml",
            remote.as_str(),
        )]);
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let outbound_store = persistence.outbound_oauth_store();
        let generation_one = embedded_auth_service(
            resources.clone(),
            Arc::clone(&outbound_store),
            &base,
            "generation-one-bearer",
        );

        let first = GeneratedAuthService::begin_auth(
            generation_one.as_ref(),
            Request::new(BeginAuthRequest {
                target: Some(proto_mcp_target("remote")),
                method: AuthMethod::OauthManual as i32,
            }),
        )
        .await
        .unwrap()
        .into_inner();
        let state = state_from_auth_url(&first.auth_url);
        GeneratedAuthService::complete_auth(
            generation_one.as_ref(),
            Request::new(CompleteAuthRequest {
                attempt_id: first.attempt_id,
                input: format!("code=mcp-code&state={state}"),
            }),
        )
        .await
        .unwrap();
        assert_eq!(registration_requests.load(Ordering::SeqCst), 1);

        let target_key = AuthTarget::mcp_server("remote").credential_key();
        let durable = OAuthCredentialStore::read(outbound_store.as_ref(), target_key.clone())
            .await
            .unwrap()
            .unwrap()
            .credential;
        assert!(
            durable
                .mcp_context()
                .unwrap()
                .registration
                .redirect_uri
                .starts_with("urn:waltr:outbound-oauth-callback:")
        );
        assert!(
            !serde_json::to_string(&durable)
                .unwrap()
                .contains(&format!("{base}/outbound-oauth/callback"))
        );
        let current = generation_one
            .credential_store
            .read(&target_key)
            .await
            .unwrap()
            .unwrap()
            .credential;
        assert_eq!(
            current.mcp_context().unwrap().registration.redirect_uri,
            format!("{base}/outbound-oauth/callback")
        );

        GeneratedAuthService::begin_auth(
            generation_one.as_ref(),
            Request::new(BeginAuthRequest {
                target: Some(proto_mcp_target("remote")),
                method: AuthMethod::OauthManual as i32,
            }),
        )
        .await
        .unwrap();
        assert_eq!(registration_requests.load(Ordering::SeqCst), 1);

        let generation_two = embedded_auth_service(
            resources.clone(),
            Arc::clone(&outbound_store),
            &base,
            "generation-two-bearer",
        );
        GeneratedAuthService::begin_auth(
            generation_two.as_ref(),
            Request::new(BeginAuthRequest {
                target: Some(proto_mcp_target("remote")),
                method: AuthMethod::OauthManual as i32,
            }),
        )
        .await
        .unwrap();
        assert_eq!(registration_requests.load(Ordering::SeqCst), 2);

        let mut pre_marker_credential = durable;
        pre_marker_credential
            .mcp_context_mut()
            .unwrap()
            .registration
            .redirect_uri = format!("{base}/outbound-oauth/callback");
        OAuthCredentialStore::write(outbound_store.as_ref(), target_key, pre_marker_credential)
            .await
            .unwrap();
        let repeated_port_generation = embedded_auth_service(
            resources,
            Arc::clone(&outbound_store),
            &base,
            "generation-three-bearer",
        );
        GeneratedAuthService::begin_auth(
            repeated_port_generation.as_ref(),
            Request::new(BeginAuthRequest {
                target: Some(proto_mcp_target("remote")),
                method: AuthMethod::OauthManual as i32,
            }),
        )
        .await
        .unwrap();
        assert_eq!(registration_requests.load(Ordering::SeqCst), 3);
        server.abort();
    }

    #[tokio::test]
    async fn stale_embedded_attempt_status_and_cancel_are_not_found_and_non_mutating() {
        let resources = crate::test_support::resource_state_with_overrides(&[]);
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let outbound_store = persistence.outbound_oauth_store();
        let generation_one = embedded_auth_service(
            resources.clone(),
            Arc::clone(&outbound_store),
            "http://127.0.0.1:43123",
            "generation-one-bearer",
        );
        let callback = generation_one
            .outbound_oauth
            .server_callback_uri
            .clone()
            .unwrap();
        let (attempt, _) = oauth::begin_mcp(
            AuthTarget::mcp_server("remote"),
            "https://auth.example".to_owned(),
            "https://auth.example/authorize",
            "https://auth.example/token".to_owned(),
            None,
            McpOAuthRegistration {
                method: McpOAuthRegistrationMethod::Dynamic,
                client_id: "generation-one-client".to_owned(),
                redirect_uri: callback,
                token_endpoint_auth_method: "none".to_owned(),
                ..McpOAuthRegistration::default()
            },
            Vec::new(),
            "https://mcp.example/resource".to_owned(),
            false,
            now_ms(),
        )
        .unwrap();
        let (attempt_id, _, _) = generation_one
            .save_mcp_attempt(&AuthTarget::mcp_server("remote"), &attempt)
            .await
            .unwrap();
        let current = GeneratedAuthService::get_auth_attempt_status(
            generation_one.as_ref(),
            Request::new(GetAuthAttemptStatusRequest {
                attempt_id: attempt_id.clone(),
            }),
        )
        .await
        .unwrap()
        .into_inner();
        assert_eq!(
            current.attempt.unwrap().lifecycle,
            AuthAttemptLifecycle::Pending as i32
        );

        let generation_two = embedded_auth_service(
            resources,
            Arc::clone(&outbound_store),
            "http://127.0.0.1:43123",
            "generation-two-bearer",
        );
        let status = GeneratedAuthService::get_auth_attempt_status(
            generation_two.as_ref(),
            Request::new(GetAuthAttemptStatusRequest {
                attempt_id: attempt_id.clone(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(status.code(), tonic::Code::NotFound);
        let cancel = GeneratedAuthService::cancel_auth(
            generation_two.as_ref(),
            Request::new(CancelAuthRequest {
                attempt_id: attempt_id.clone(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(cancel.code(), tonic::Code::NotFound);
        assert_eq!(
            outbound_store
                .attempt(&attempt_id)
                .await
                .unwrap()
                .unwrap()
                .status,
            "pending"
        );
    }

    #[tokio::test]
    async fn embedded_self_mcp_logout_is_failed_precondition_and_preserves_credential() {
        let resources = crate::test_support::resource_state_with_overrides(&[]);
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let service = embedded_auth_service(
            resources,
            persistence.outbound_oauth_store(),
            "http://127.0.0.1:43123",
            "host-owned-bearer",
        );
        let error = GeneratedAuthService::logout(
            service.as_ref(),
            Request::new(LogoutRequest {
                target: Some(proto_mcp_target("waltr")),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), tonic::Code::FailedPrecondition);
        let credential = service
            .credential_store
            .read(&AuthTarget::mcp_server("waltr").credential_key())
            .await
            .unwrap()
            .expect("host-owned credential remains installed")
            .credential;
        assert_eq!(credential.token.access_token.secret(), "host-owned-bearer");
        assert!(!error.message().contains("host-owned-bearer"));
    }

    #[tokio::test]
    async fn generic_provider_and_mcp_forced_refresh_succeed_with_sanitized_status() {
        let (service, server, _) = successful_lifecycle_service().await;
        service.refresh_outcomes.lock().await.insert(
            AuthTarget::provider("codex").credential_key(),
            TestRefreshOutcome::Success(Box::new(OAuthCredential::provider(
                OAuthTokenSet::bearer(
                    "refreshed-provider-access-secret",
                    Some("refreshed-provider-refresh-secret".to_owned()),
                    None,
                ),
                std::collections::BTreeMap::from([
                    ("account_id".to_owned(), "account-123".to_owned()),
                    (
                        "token_url".to_owned(),
                        "https://secret.example/token".to_owned(),
                    ),
                    ("client_id".to_owned(), "secret-client".to_owned()),
                    (
                        "redirect_uri".to_owned(),
                        "https://secret.example/callback".to_owned(),
                    ),
                    ("access_token".to_owned(), "access-secret".to_owned()),
                    ("custom_secret".to_owned(), "must-not-cross".to_owned()),
                ]),
            ))),
        );
        let provider = GeneratedAuthService::refresh_auth(
            service.as_ref(),
            Request::new(RefreshAuthRequest {
                target: Some(proto_provider_target("codex")),
            }),
        )
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
        assert_eq!(provider.metadata.len(), 1);
        assert_eq!(provider.metadata["account_id"], "account-123");
        assert!(provider.can_refresh);

        let config = service.configuration();
        let resource = config.mcp_servers["remote"].url.clone();
        service.refresh_outcomes.lock().await.insert(
            AuthTarget::mcp_server("remote").credential_key(),
            TestRefreshOutcome::Success(Box::new(OAuthCredential::mcp(
                OAuthTokenSet::bearer(
                    "refreshed-mcp-access-secret",
                    Some("refreshed-mcp-refresh-secret".to_owned()),
                    None,
                ),
                McpOAuthContext {
                    resource,
                    issuer: "https://auth.example".to_owned(),
                    registration: McpOAuthRegistration::default(),
                    endpoints: McpOAuthEndpoints::default(),
                },
            ))),
        );
        let mcp = GeneratedAuthService::refresh_auth(
            service.as_ref(),
            Request::new(RefreshAuthRequest {
                target: Some(proto_mcp_target("remote")),
            }),
        )
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
        assert_eq!(mcp.kind, AuthStatusKind::OauthConfigured as i32);
        assert!(mcp.can_refresh);
        server.abort();
    }

    async fn assert_typed_refresh_failure(
        service: &AuthService,
        target: ProtoAuthTarget,
        code: tonic::Code,
        reason: AuthenticationFailureReason,
    ) {
        let error = GeneratedAuthService::refresh_auth(
            service,
            Request::new(RefreshAuthRequest {
                target: Some(target.clone()),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code(), code);
        let failure = AuthenticationFailure::decode(error.details()).unwrap();
        assert_eq!(failure.reason, reason as i32);
        assert_eq!(failure.target, Some(target));
        assert!(!error.message().contains("secret"));
    }

    async fn assert_provider_refresh_failures(service: &AuthService) {
        assert_typed_refresh_failure(
            service,
            proto_provider_target("anthropic"),
            tonic::Code::FailedPrecondition,
            AuthenticationFailureReason::ConfigurationRequired,
        )
        .await;

        service
            .credential_store
            .delete(&AuthTarget::provider("codex").credential_key())
            .await
            .unwrap();
        assert_typed_refresh_failure(
            service,
            proto_provider_target("codex"),
            tonic::Code::FailedPrecondition,
            AuthenticationFailureReason::CredentialRequired,
        )
        .await;
        service
            .credential_store
            .write(
                &AuthTarget::provider("codex").credential_key(),
                OAuthCredential::provider(
                    OAuthTokenSet::bearer("access-secret", None, None),
                    std::collections::BTreeMap::new(),
                ),
            )
            .await
            .unwrap();
        assert_typed_refresh_failure(
            service,
            proto_provider_target("codex"),
            tonic::Code::FailedPrecondition,
            AuthenticationFailureReason::CredentialRequired,
        )
        .await;
    }

    async fn assert_mcp_refresh_failures(service: &AuthService) {
        let key = AuthTarget::mcp_server("remote").credential_key();
        let mut credential = service
            .credential_store
            .read(&key)
            .await
            .unwrap()
            .unwrap()
            .credential;
        credential
            .mcp_context_mut()
            .expect("MCP credential context")
            .resource = "https://mcp.example/different".to_owned();
        service
            .credential_store
            .write(&key, credential)
            .await
            .unwrap();
        assert_typed_refresh_failure(
            service,
            proto_mcp_target("remote"),
            tonic::Code::FailedPrecondition,
            AuthenticationFailureReason::ConfigurationRequired,
        )
        .await;

        for (injected_reason, expected_code, expected_reason, message) in [
            (
                AgentAuthenticationFailureReason::CredentialRequired,
                tonic::Code::FailedPrecondition,
                AuthenticationFailureReason::CredentialRequired,
                "MCP OAuth refresh requires reauthorization",
            ),
            (
                AgentAuthenticationFailureReason::TokenExchangeFailed,
                tonic::Code::Unauthenticated,
                AuthenticationFailureReason::TokenExchangeFailed,
                "OAuth token exchange failed",
            ),
            (
                AgentAuthenticationFailureReason::NetworkFailed,
                tonic::Code::Unavailable,
                AuthenticationFailureReason::NetworkFailed,
                "OAuth service unavailable",
            ),
            (
                AgentAuthenticationFailureReason::ResponseInvalid,
                tonic::Code::DataLoss,
                AuthenticationFailureReason::ResponseInvalid,
                "OAuth response was invalid",
            ),
        ] {
            service.refresh_outcomes.lock().await.insert(
                AuthTarget::mcp_server("remote").credential_key(),
                TestRefreshOutcome::Failure {
                    reason: injected_reason,
                    message: message.to_owned(),
                    internal: false,
                },
            );
            assert_typed_refresh_failure(
                service,
                proto_mcp_target("remote"),
                expected_code,
                expected_reason,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn generic_forced_refresh_failures_are_typed_and_sanitized() {
        let service = generic_auth_service().await;
        assert_provider_refresh_failures(service.as_ref()).await;
        assert_mcp_refresh_failures(service.as_ref()).await;
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the fixture verifies one complete hardened OAuth binding flow"
    )]
    async fn hardened_discovery_and_dynamic_registration_preserve_exact_bindings() {
        use axum::{http::header, routing::get};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let endpoint = format!("{base}/mcp");
        let resource_metadata = format!("{base}/resource-metadata");
        let issuer = base.clone();
        let challenge_metadata = resource_metadata.clone();
        let protected_endpoint = endpoint.clone();
        let protected_issuer = issuer.clone();
        let authorization_issuer = issuer.clone();
        let registration_endpoint = format!("{base}/register");
        let authorization_registration = registration_endpoint.clone();
        let app = Router::new()
            .route(
                "/mcp",
                post(move || {
                    let challenge_metadata = challenge_metadata.clone();
                    async move {
                        (
                            StatusCode::UNAUTHORIZED,
                            [(
                                header::WWW_AUTHENTICATE,
                                format!(
                                    "Bearer resource_metadata=\"{challenge_metadata}\", scope=\"tools.read\""
                                ),
                            )],
                        )
                    }
                }),
            )
            .route(
                "/resource-metadata",
                get(move || {
                    let endpoint = protected_endpoint.clone();
                    let issuer = protected_issuer.clone();
                    async move {
                        Json(serde_json::json!({
                            "resource": endpoint,
                            "authorization_servers": [issuer],
                            "scopes_supported": ["tools.read"]
                        }))
                    }
                }),
            )
            .route(
                "/.well-known/oauth-authorization-server",
                get(move || {
                    let issuer = authorization_issuer.clone();
                    let registration = authorization_registration.clone();
                    async move {
                        Json(serde_json::json!({
                            "issuer": issuer,
                            "authorization_endpoint": format!("{issuer}/authorize"),
                            "token_endpoint": format!("{issuer}/token"),
                            "registration_endpoint": registration,
                            "response_types_supported": ["code"],
                            "grant_types_supported": ["authorization_code", "refresh_token"],
                            "code_challenge_methods_supported": ["S256"],
                            "token_endpoint_auth_methods_supported": ["none"]
                        }))
                    }
                }),
            )
            .route(
                "/register",
                post(|| async {
                    Json(serde_json::json!({
                        "client_id": "registered-client",
                        "application_type": "web",
                        "redirect_uris": ["https://waltr.example/outbound-oauth/callback"],
                        "grant_types": ["authorization_code", "refresh_token"],
                        "response_types": ["code"],
                        "token_endpoint_auth_method": "none"
                    }))
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let policy = OutboundHttpPolicy::loopback_test();
        let discovery =
            discover_mcp_auth(&AuthTarget::mcp_server("remote"), &endpoint, policy.clone())
                .await
                .unwrap();
        assert_eq!(discovery.protected.resource, endpoint);
        assert_eq!(discovery.authorization_server.issuer, issuer);
        assert_eq!(discovery.challenge.scope.as_deref(), Some("tools.read"));

        let registration = register_public_client_hardened(
            policy,
            &registration_endpoint,
            &McpPublicClientRegistrationRequest {
                application_type: mcp::McpOAuthApplicationType::Web,
                redirect_uris: vec!["https://waltr.example/outbound-oauth/callback".to_owned()],
                grant_types: vec![
                    McpOAuthGrantType::AuthorizationCode,
                    McpOAuthGrantType::RefreshToken,
                ],
                response_types: vec![McpOAuthResponseType::Code],
                token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
                client_name: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(registration.client_id, "registered-client");
        server.abort();
    }

    #[tokio::test]
    async fn revocation_uses_the_bound_public_client_without_exposing_secrets() {
        let requests = Arc::new(AtomicUsize::new(0));
        let request_count = Arc::clone(&requests);
        let app = Router::new().route(
            "/revoke",
            post(move || {
                let request_count = Arc::clone(&request_count);
                async move {
                    request_count.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/revoke", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let credential = OAuthCredential::mcp(
            OAuthTokenSet::bearer("access-secret", Some("refresh-secret".to_owned()), None),
            McpOAuthContext {
                resource: "https://mcp.example/resource".to_owned(),
                issuer: "https://auth.example".to_owned(),
                registration: McpOAuthRegistration {
                    client_id: "public-client".to_owned(),
                    ..McpOAuthRegistration::default()
                },
                endpoints: McpOAuthEndpoints {
                    revocation: Some(endpoint),
                    ..McpOAuthEndpoints::default()
                },
            },
        );
        revoke_mcp_credential_with_policy(&credential, OutboundHttpPolicy::loopback_test()).await;
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    #[allow(
        clippy::too_many_lines,
        reason = "the test exercises the durable callback route through token exchange"
    )]
    async fn server_callback_requires_and_preserves_advertised_response_issuer() {
        let token_requests = Arc::new(AtomicUsize::new(0));
        let requests = Arc::clone(&token_requests);
        let token_app = Router::new().route(
            "/token",
            post(move || {
                let requests = Arc::clone(&requests);
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({
                        "access_token": "access-token",
                        "token_type": "Bearer",
                        "expires_in": 3600
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let token_url = format!("http://{}/token", listener.local_addr().unwrap());
        let token_server = tokio::spawn(async move { axum::serve(listener, token_app).await });

        let resources = crate::test_support::resource_state_with_overrides(&[(
            "mcp-servers/hosted.yaml",
            "apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: hosted\nspec:\n  transport: streamableHttp\n  url: https://waltr.example/mcp\n",
        )]);
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let outbound_store = persistence.outbound_oauth_store();
        let credential_store = CredentialStore::shared(outbound_store.clone());
        let service = AuthService::new(
            resources,
            credential_store,
            outbound_store,
            Vec::new(),
            None,
            OutboundOAuthConfig::for_server_callback(
                "https://waltr.example/outbound-oauth/callback".to_owned(),
            ),
        );
        let router = Router::new()
            .route(
                "/outbound-oauth/callback",
                get(
                    |State(service): State<Arc<AuthService>>,
                     Query(query): Query<OutboundCallbackQuery>| async move {
                        outbound_callback_with_policy(
                            service,
                            query,
                            Some(OutboundHttpPolicy::loopback_test()),
                        )
                        .await
                    },
                ),
            )
            .with_state(Arc::clone(&service));

        let issuer = "https://waltr.example/oauth";
        let metadata = mcp::McpAuthorizationServerMetadata {
            authorization_response_iss_parameter_supported: Some(true),
            ..mcp::McpAuthorizationServerMetadata::new(issuer)
        };
        for response_issuer in [None, Some("https://wrong.example"), Some(issuer)] {
            let (attempt, _) = oauth::begin_mcp(
                AuthTarget::mcp_server("hosted"),
                metadata.issuer.clone(),
                "https://waltr.example/oauth/authorize",
                token_url.clone(),
                None,
                McpOAuthRegistration {
                    method: McpOAuthRegistrationMethod::Preregistered,
                    client_id: "waltr-outbound".to_owned(),
                    redirect_uri: "https://waltr.example/outbound-oauth/callback".to_owned(),
                    token_endpoint_auth_method: "none".to_owned(),
                    created_at_ms: None,
                    expires_at_ms: None,
                },
                vec!["mcp:use".to_owned()],
                "https://waltr.example/mcp".to_owned(),
                metadata.authorization_response_iss_parameter_supported == Some(true),
                now_ms(),
            )
            .unwrap();
            let state = attempt.state().to_owned();
            service
                .save_mcp_attempt(&AuthTarget::mcp_server("hosted"), &attempt)
                .await
                .unwrap();
            let mut callback =
                url::Url::parse("https://waltr.example/outbound-oauth/callback").unwrap();
            callback
                .query_pairs_mut()
                .append_pair("code", "authorization-code")
                .append_pair("state", &state);
            if let Some(response_issuer) = response_issuer {
                callback
                    .query_pairs_mut()
                    .append_pair("iss", response_issuer);
            }
            let response = router
                .clone()
                .oneshot(
                    axum::http::Request::builder()
                        .uri(callback[url::Position::BeforePath..].to_owned())
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if response_issuer == Some(issuer) {
                    StatusCode::OK
                } else {
                    StatusCode::BAD_REQUEST
                }
            );
        }
        assert_eq!(token_requests.load(Ordering::SeqCst), 1);
        token_server.abort();
    }
}
