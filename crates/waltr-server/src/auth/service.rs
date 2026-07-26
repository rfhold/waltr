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
    AuthenticationFailureReason, BeginAuthRequest, BeginAuthResponse, BeginProviderAuthRequest,
    BeginProviderAuthResponse, CancelAuthRequest, CancelAuthResponse, CancelProviderAuthRequest,
    CancelProviderAuthResponse, CompleteAuthRequest, CompleteAuthResponse,
    CompleteProviderAuthRequest, CompleteProviderAuthResponse, GetAuthAttemptStatusRequest,
    GetAuthAttemptStatusResponse, GetAuthStatusRequest, GetAuthStatusResponse,
    GetProviderAuthStatusRequest, GetProviderAuthStatusResponse, ListAuthStatusesRequest,
    ListAuthStatusesResponse, ListProviderAuthStatusesRequest, ListProviderAuthStatusesResponse,
    LogoutProviderRequest, LogoutProviderResponse, LogoutRequest, LogoutResponse,
    ProviderAuthMethod, ProviderAuthStatus, ProviderAuthStatusKind,
    auth_service_server::AuthService as GeneratedAuthService, auth_target,
};
use waltr_agent::{
    McpOAuthEndpoints, McpOAuthRegistration, McpOAuthRegistrationMethod, McpOAuthState,
    McpOAuthToken,
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

use crate::{
    config::{
        ConfigStore,
        headers::{configured_credential, has_unresolved_credential, resolve_provider_headers},
    },
    persistence::{DurableAttemptRecord, OutboundOAuthStore},
};

use super::outbound_http::{
    OAUTH_RESPONSE_LIMIT, OutboundHttpPolicy, read_json_body, validate_response_peer,
};
use super::{AuthTarget, FileCredentialStore, StoredCredential, oauth};

const OAUTH_COMPLETION_LEASE_MS: i64 = 30_000;

/// gRPC auth service for provider credentials.
#[derive(Debug)]
pub struct AuthService<S> {
    config_store: S,
    credential_store: FileCredentialStore,
    outbound_store: Arc<OutboundOAuthStore>,
    attempts: Mutex<HashMap<String, PendingOAuthAttempt>>,
}

#[derive(Debug, Clone)]
enum PendingOAuthAttempt {
    Provider(oauth::OAuthAttempt),
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

impl<S> AuthService<S>
where
    S: ConfigStore,
{
    /// Create an auth service.
    #[must_use]
    pub(crate) fn new(
        config_store: S,
        credential_store: FileCredentialStore,
        outbound_store: Arc<OutboundOAuthStore>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config_store,
            credential_store,
            outbound_store,
            attempts: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn router(self: &Arc<Self>) -> Router {
        Router::new()
            .route("/outbound-oauth/callback", get(outbound_callback::<S>))
            .route("/.well-known/oauth-client/waltr", get(client_metadata::<S>))
            .with_state(Arc::clone(self))
    }

    /// Return the credential store used by this service.
    #[must_use]
    pub const fn credential_store(&self) -> &FileCredentialStore {
        &self.credential_store
    }

    async fn status_for(&self, target: AuthTarget) -> Result<AuthStatus, Status> {
        match target {
            AuthTarget::Provider { key } => self.provider_status_for(&key).await,
            AuthTarget::McpServer { key } => self.mcp_status_for(&key).await,
        }
    }

    async fn provider_status_for(&self, provider_key: &str) -> Result<AuthStatus, Status> {
        let config = self.config_store.load().await.map_err(|error| {
            Status::internal(format!("failed to load config for auth status: {error}"))
        })?;
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
            Some(StoredCredential::OAuth(credential)) => {
                let expired = credential.expires_at_ms <= now_ms();
                Ok(AuthStatus {
                    target: Some(proto_provider_target(provider_key)),
                    kind: if expired {
                        AuthStatusKind::Expired as i32
                    } else {
                        AuthStatusKind::OauthConfigured as i32
                    },
                    supported_methods,
                    expires_at_ms: credential.expires_at_ms,
                    metadata: credential.metadata.into_iter().collect(),
                    ..AuthStatus::default()
                })
            }
            Some(StoredCredential::McpOAuth(_)) | None => Ok(AuthStatus {
                target: Some(proto_provider_target(provider_key)),
                kind: AuthStatusKind::NotConfigured as i32,
                supported_methods,
                ..AuthStatus::default()
            }),
        }
    }

    async fn mcp_status_for(&self, server_key: &str) -> Result<AuthStatus, Status> {
        let config = self.config_store.load().await.map_err(|error| {
            Status::internal(format!("failed to load config for auth status: {error}"))
        })?;
        if !config.mcp_servers.contains_key(server_key) {
            return Err(Status::not_found(format!(
                "mcp server '{server_key}' does not exist"
            )));
        }
        match self
            .credential_store
            .read(&AuthTarget::mcp_server(server_key).credential_key())
            .await
            .map_err(|error| Status::internal(format!("failed to read credentials: {error}")))?
        {
            Some(StoredCredential::McpOAuth(credential)) => {
                Ok(mcp_oauth_status(server_key, &credential))
            }
            Some(StoredCredential::OAuth(credential)) => Ok(AuthStatus {
                target: Some(proto_mcp_target(server_key)),
                kind: if credential.expires_at_ms <= now_ms() {
                    AuthStatusKind::Expired as i32
                } else {
                    AuthStatusKind::OauthConfigured as i32
                },
                expires_at_ms: credential.expires_at_ms,
                metadata: credential.metadata.into_iter().collect(),
                ..AuthStatus::default()
            }),
            None => Ok(AuthStatus {
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
    ) -> Result<(BeginProviderAuthResponse, AuthCompletionMode, i32), Status> {
        let config = self.config_store.load().await.map_err(|error| {
            Status::internal(format!("failed to load config for MCP auth: {error}"))
        })?;
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

        let policy = OutboundHttpPolicy::production();
        let discovery = discover_mcp_auth(&target, &server.url, policy).await?;
        let auth_server = discovery.authorization_server;
        let (authorize_url, token_url) = required_authorization_endpoints(&target, &auth_server)?;
        let redirect_uri = outbound_redirect_uri(&server_key)?;
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
        let registration_state = McpOAuthState {
            version: 1,
            resource: discovery.protected.resource.clone(),
            issuer: auth_server.issuer.clone(),
            registration: registration.clone(),
            endpoints: McpOAuthEndpoints {
                authorization: authorize_url.clone(),
                token: token_url.clone(),
                revocation: auth_server.revocation_endpoint.clone(),
            },
            token: McpOAuthToken::default(),
        };
        self.store_mcp_registration(&target, registration_state)
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

    async fn store_mcp_registration(
        &self,
        target: &AuthTarget,
        state: McpOAuthState,
    ) -> Result<(), Status> {
        self.credential_store
            .modify(&target.credential_key(), |_| {
                Some(StoredCredential::McpOAuth(state))
            })
            .await
            .map_err(|error| Status::internal(format!("failed to store MCP registration: {error}")))
            .map(|_| ())
    }

    async fn save_mcp_attempt(
        &self,
        target: &AuthTarget,
        attempt: &oauth::McpOAuthAttempt,
    ) -> Result<(String, AuthCompletionMode, i32), Status> {
        let attempt_id = Uuid::new_v4().to_string();
        let completion_mode = if attempt.redirect_uri() == server_callback_uri().unwrap_or_default()
        {
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
                    attempt_json: serde_json::to_string(attempt)
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
        let persisted = self.persisted_mcp_registration(&request).await?;
        if let Some(registration) =
            configured_mcp_registration(request.server_key, request.redirect_uri)?
        {
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
            && let Some(client_id) = client_metadata_uri()
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

    async fn persisted_mcp_registration(
        &self,
        request: &McpRegistrationRequest<'_>,
    ) -> Result<Option<McpOAuthRegistration>, Status> {
        if let Some(StoredCredential::McpOAuth(current)) = self
            .credential_store
            .read(&request.target.credential_key())
            .await
            .map_err(|error| {
                Status::internal(format!("failed to read MCP registration: {error}"))
            })?
            && current.issuer == request.authorization_server.issuer
            && current.resource == request.resource
            && current.registration.redirect_uri == request.redirect_uri
            && current
                .registration
                .expires_at_ms
                .is_none_or(|expiry| expiry > now_ms())
            && !current.registration.client_id.is_empty()
        {
            Ok(Some(current.registration))
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
                redirect_uris: vec![request.redirect_uri.to_owned()],
                grant_types: vec![
                    McpOAuthGrantType::AuthorizationCode,
                    McpOAuthGrantType::RefreshToken,
                ],
                response_types: vec![McpOAuthResponseType::Code],
                token_endpoint_auth_method: McpTokenEndpointAuthMethod::None,
                client_name: Some("Waltr outbound MCP client".to_owned()),
            };
            let response =
                register_public_client_hardened(request.policy, endpoint, &registration_request)
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
                let mut credential = oauth::complete_manual(attempt, input, now_ms())
                    .await
                    .map_err(|status| outbound_oauth_status(&target, &status))?;
                credential.metadata.insert(
                    "token_url".to_owned(),
                    attempt.definition().token_url.clone(),
                );
                credential.metadata.insert(
                    "client_id".to_owned(),
                    attempt.definition().client_id.clone(),
                );
                credential.metadata.insert(
                    "redirect_uri".to_owned(),
                    attempt.definition().redirect_uri.clone(),
                );
                (target, StoredCredential::OAuth(credential))
            }
        };
        self.attempts.lock().await.remove(attempt_id);
        self.credential_store
            .modify(&target.credential_key(), |_| Some(credential))
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
        let attempt: oauth::McpOAuthAttempt = serde_json::from_str(&record.attempt_json)
            .map_err(|_| Status::data_loss("stored OAuth attempt is invalid"))?;
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
        let completion = match policy {
            Some(policy) => {
                oauth::complete_mcp_with_policy(&attempt, input, now_ms(), policy).await
            }
            None => oauth::complete_mcp(&attempt, input, now_ms()).await,
        };
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
impl<S> GeneratedAuthService for AuthService<S>
where
    S: ConfigStore,
{
    async fn list_auth_statuses(
        &self,
        _request: Request<ListAuthStatusesRequest>,
    ) -> Result<Response<ListAuthStatusesResponse>, Status> {
        let config = self.config_store.load().await.map_err(|error| {
            Status::internal(format!("failed to load config for auth statuses: {error}"))
        })?;
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
        let (response, completion_mode, registration_method) = match target {
            AuthTarget::Provider { key } => (
                self.begin_provider_auth(Request::new(BeginProviderAuthRequest {
                    provider_key: key,
                    method: provider_method_from_auth(request.method) as i32,
                }))
                .await?
                .into_inner(),
                AuthCompletionMode::Manual,
                0,
            ),
            AuthTarget::McpServer { key } => self.begin_mcp_auth(key).await?,
        };
        Ok(Response::new(BeginAuthResponse {
            attempt_id: response.attempt_id,
            method: auth_method_from_provider(response.method) as i32,
            auth_url: response.auth_url,
            verification_url: response.verification_url,
            user_code: response.user_code,
            expires_at_ms: response.expires_at_ms,
            poll_interval_seconds: response.poll_interval_seconds,
            messages: response.messages,
            completion_mode: completion_mode as i32,
            registration_method,
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
        let mcp_credential = if matches!(target, AuthTarget::McpServer { .. }) {
            match self
                .credential_store
                .read(&target.credential_key())
                .await
                .map_err(|error| Status::internal(format!("failed to read credential: {error}")))?
            {
                Some(StoredCredential::McpOAuth(credential)) => Some(credential),
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
            revoke_mcp_credential(&credential).await;
        }
        let status = self.status_for(target).await?;
        Ok(Response::new(LogoutResponse {
            status: Some(status),
        }))
    }

    async fn list_provider_auth_statuses(
        &self,
        _request: Request<ListProviderAuthStatusesRequest>,
    ) -> Result<Response<ListProviderAuthStatusesResponse>, Status> {
        let config = self.config_store.load().await.map_err(|error| {
            Status::internal(format!("failed to load config for auth statuses: {error}"))
        })?;
        let mut statuses = Vec::with_capacity(config.providers.len());
        for provider_key in config.providers.keys() {
            statuses.push(provider_status_from_auth(
                self.provider_status_for(provider_key).await?,
            )?);
        }
        Ok(Response::new(ListProviderAuthStatusesResponse { statuses }))
    }

    async fn get_provider_auth_status(
        &self,
        request: Request<GetProviderAuthStatusRequest>,
    ) -> Result<Response<GetProviderAuthStatusResponse>, Status> {
        let status = provider_status_from_auth(
            self.provider_status_for(&request.into_inner().provider_key)
                .await?,
        )?;
        Ok(Response::new(GetProviderAuthStatusResponse {
            status: Some(status),
        }))
    }

    async fn begin_provider_auth(
        &self,
        request: Request<BeginProviderAuthRequest>,
    ) -> Result<Response<BeginProviderAuthResponse>, Status> {
        let request = request.into_inner();
        let method =
            ProviderAuthMethod::try_from(request.method).unwrap_or(ProviderAuthMethod::Unspecified);
        if method != ProviderAuthMethod::OauthManual {
            return Err(Status::failed_precondition(
                "only manual OAuth completion is supported",
            ));
        }
        let config = self.config_store.load().await.map_err(|error| {
            Status::internal(format!("failed to load config for auth: {error}"))
        })?;
        let provider = config.providers.get(&request.provider_key).ok_or_else(|| {
            Status::not_found(format!(
                "provider '{}' does not exist",
                request.provider_key
            ))
        })?;
        let definition =
            oauth::provider_definition(&request.provider_key, provider).ok_or_else(|| {
                Status::failed_precondition(format!(
                    "provider '{}' does not support OAuth",
                    request.provider_key
                ))
            })?;
        let (attempt, mut response) = oauth::begin_manual(
            AuthTarget::provider(request.provider_key),
            definition.definition,
            now_ms(),
        )?;
        let policy = OutboundHttpPolicy::production();
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
        Ok(Response::new(response))
    }

    async fn complete_provider_auth(
        &self,
        request: Request<CompleteProviderAuthRequest>,
    ) -> Result<Response<CompleteProviderAuthResponse>, Status> {
        let request = request.into_inner();
        let attempt = self
            .attempts
            .lock()
            .await
            .get(&request.attempt_id)
            .cloned()
            .ok_or_else(|| Status::not_found("OAuth attempt does not exist"))?;
        let PendingOAuthAttempt::Provider(attempt) = attempt;
        attempt
            .provider_key()
            .ok_or_else(|| Status::invalid_argument("OAuth attempt is not for a provider"))?;
        let status = provider_status_from_auth(
            self.complete_attempt(&request.attempt_id, &request.input)
                .await?,
        )?;
        Ok(Response::new(CompleteProviderAuthResponse {
            status: Some(status),
        }))
    }

    async fn cancel_provider_auth(
        &self,
        request: Request<CancelProviderAuthRequest>,
    ) -> Result<Response<CancelProviderAuthResponse>, Status> {
        self.attempts
            .lock()
            .await
            .remove(&request.into_inner().attempt_id);
        Ok(Response::new(CancelProviderAuthResponse {}))
    }

    async fn logout_provider(
        &self,
        request: Request<LogoutProviderRequest>,
    ) -> Result<Response<LogoutProviderResponse>, Status> {
        let provider_key = request.into_inner().provider_key;
        self.credential_store
            .delete(&AuthTarget::provider(&provider_key).credential_key())
            .await
            .map_err(|error| Status::internal(format!("failed to delete credential: {error}")))?;
        let status = provider_status_from_auth(self.provider_status_for(&provider_key).await?)?;
        Ok(Response::new(LogoutProviderResponse {
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

async fn outbound_callback<S>(
    State(service): State<Arc<AuthService<S>>>,
    Query(query): Query<OutboundCallbackQuery>,
) -> AxumResponse
where
    S: ConfigStore,
{
    outbound_callback_with_policy(service, query, None).await
}

#[allow(
    clippy::too_many_lines,
    reason = "the callback handles lookup, denial, completion, and durable claim responses"
)]
async fn outbound_callback_with_policy<S>(
    service: Arc<AuthService<S>>,
    query: OutboundCallbackQuery,
    policy: Option<OutboundHttpPolicy>,
) -> AxumResponse
where
    S: ConfigStore,
{
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

async fn client_metadata<S>(State(_service): State<Arc<AuthService<S>>>) -> AxumResponse
where
    S: ConfigStore,
{
    let Some(client_id) = client_metadata_uri() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(redirect_uri) = server_callback_uri() else {
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
    vec![ProviderAuthMethod::OauthManual as i32]
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

fn provider_status_from_auth(status: AuthStatus) -> Result<ProviderAuthStatus, Status> {
    let Some(auth_target::Target::ProviderKey(provider_key)) =
        status.target.and_then(|target| target.target)
    else {
        return Err(Status::internal(
            "auth status did not contain a provider target",
        ));
    };
    Ok(ProviderAuthStatus {
        provider_key,
        kind: match AuthStatusKind::try_from(status.kind).unwrap_or(AuthStatusKind::Unspecified) {
            AuthStatusKind::Unspecified => ProviderAuthStatusKind::Unspecified,
            AuthStatusKind::NotConfigured => ProviderAuthStatusKind::NotConfigured,
            AuthStatusKind::ApiKeyConfigured => ProviderAuthStatusKind::ApiKeyConfigured,
            AuthStatusKind::OauthConfigured => ProviderAuthStatusKind::OauthConfigured,
            AuthStatusKind::Expired => ProviderAuthStatusKind::Expired,
            AuthStatusKind::Unsupported | AuthStatusKind::AuthorizationRequired => {
                ProviderAuthStatusKind::Unsupported
            }
        } as i32,
        supported_methods: status
            .supported_methods
            .into_iter()
            .map(|method| provider_method_from_auth(method) as i32)
            .collect(),
        expires_at_ms: status.expires_at_ms,
        metadata: status.metadata,
        message: status.message,
    })
}

fn auth_method_from_provider(method: i32) -> AuthMethod {
    match ProviderAuthMethod::try_from(method).unwrap_or(ProviderAuthMethod::Unspecified) {
        ProviderAuthMethod::OauthManual => AuthMethod::OauthManual,
        ProviderAuthMethod::OauthDevice => AuthMethod::OauthDevice,
        ProviderAuthMethod::Unspecified => AuthMethod::Unspecified,
    }
}

fn provider_method_from_auth(method: i32) -> ProviderAuthMethod {
    match AuthMethod::try_from(method).unwrap_or(AuthMethod::Unspecified) {
        AuthMethod::OauthManual => ProviderAuthMethod::OauthManual,
        AuthMethod::OauthDevice => ProviderAuthMethod::OauthDevice,
        AuthMethod::Unspecified => ProviderAuthMethod::Unspecified,
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

fn mcp_oauth_status(server_key: &str, credential: &McpOAuthState) -> AuthStatus {
    let mut metadata = std::collections::HashMap::new();
    if !credential.resource.is_empty() {
        metadata.insert("resource".to_owned(), credential.resource.clone());
    }
    if !credential.issuer.is_empty() {
        metadata.insert("issuer".to_owned(), credential.issuer.clone());
    }
    metadata.insert(
        "registration_method".to_owned(),
        match credential.registration.method {
            McpOAuthRegistrationMethod::Preregistered => "preregistered",
            McpOAuthRegistrationMethod::ClientMetadata => "client_metadata",
            McpOAuthRegistrationMethod::Dynamic => "dynamic",
            McpOAuthRegistrationMethod::Unknown => "unknown",
        }
        .to_owned(),
    );
    if !credential.token.granted_scopes.is_empty() {
        metadata.insert(
            "scope".to_owned(),
            credential.token.granted_scopes.join(" "),
        );
    }
    AuthStatus {
        target: Some(proto_mcp_target(server_key)),
        kind: if credential.token.access_token.is_empty() {
            AuthStatusKind::AuthorizationRequired as i32
        } else if credential.token.expires_at_ms <= now_ms() {
            AuthStatusKind::Expired as i32
        } else {
            AuthStatusKind::OauthConfigured as i32
        },
        supported_methods: credential
            .token
            .access_token
            .is_empty()
            .then_some(AuthMethod::OauthManual as i32)
            .into_iter()
            .collect(),
        expires_at_ms: credential.token.expires_at_ms,
        registration_method: proto_registration_method(credential.registration.method) as i32,
        metadata,
        ..AuthStatus::default()
    }
}

async fn revoke_mcp_credential(credential: &McpOAuthState) {
    revoke_mcp_credential_with_policy(credential, OutboundHttpPolicy::production()).await;
}

async fn revoke_mcp_credential_with_policy(credential: &McpOAuthState, policy: OutboundHttpPolicy) {
    let Some(endpoint) = credential.endpoints.revocation.as_deref() else {
        return;
    };
    let (token, token_type_hint) = credential.token.refresh_token.as_deref().map_or(
        (
            credential.token.access_token.as_str(),
            McpRevocationTokenTypeHint::AccessToken,
        ),
        |refresh_token| (refresh_token, McpRevocationTokenTypeHint::RefreshToken),
    );
    if token.is_empty() {
        return;
    }
    let request = McpRevocationRequest {
        token: McpOAuthSecret::new(token),
        token_type_hint: Some(token_type_hint),
        client_id: credential.registration.client_id.clone(),
    };
    let Ok((endpoint, client)) = policy.client_for(endpoint).await else {
        return;
    };
    if let Ok(response) = client.post(endpoint).form(&request).send().await {
        drop(validate_response_peer(policy, &response));
    }
}

fn configured_mcp_registration(
    server_key: &str,
    default_redirect_uri: &str,
) -> Result<Option<McpOAuthRegistration>, Status> {
    let key = server_key
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let prefix = format!("WALTR_MCP_{key}_OAUTH");
    let Some(client_id) = std::env::var(format!("{prefix}_CLIENT_ID"))
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let redirect_uri = std::env::var(format!("{prefix}_REDIRECT_URI"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default_redirect_uri.to_owned());
    if redirect_uri.trim().is_empty() {
        return Err(outbound_failure_status(
            &AuthTarget::mcp_server(server_key),
            tonic::Code::FailedPrecondition,
            AuthenticationFailureReason::ConfigurationRequired,
            format!("MCP OAuth public client registration is incomplete for '{server_key}'"),
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

fn outbound_redirect_uri(server_key: &str) -> Result<String, Status> {
    if let Some(callback) = server_callback_uri() {
        return Ok(callback);
    }
    let key = server_key
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    std::env::var(format!("WALTR_MCP_{key}_OAUTH_REDIRECT_URI"))
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            outbound_failure_status(
                &AuthTarget::mcp_server(server_key),
                tonic::Code::FailedPrecondition,
                AuthenticationFailureReason::ConfigurationRequired,
                "WALTR_PUBLIC_URL or an MCP OAuth redirect URI is required".to_owned(),
            )
        })
}

fn server_callback_uri() -> Option<String> {
    public_url_with_path("/outbound-oauth/callback")
}

fn client_metadata_uri() -> Option<String> {
    public_url_with_path("/.well-known/oauth-client/waltr")
}

fn public_url_with_path(path: &str) -> Option<String> {
    let mut url = url::Url::parse(&std::env::var("WALTR_PUBLIC_URL").ok()?).ok()?;
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
    let challenge = request_mcp_challenge(policy, endpoint)
        .await
        .map_err(|status| outbound_oauth_status(target, &status))?;
    if let Some(metadata_url) = challenge.resource_metadata.as_deref() {
        policy
            .validate_destination(metadata_url)
            .await
            .map_err(|status| outbound_oauth_status(target, &status))?;
    }
    let protected = discover_protected_resource(policy, endpoint, &challenge)
        .await
        .map_err(|status| outbound_oauth_status(target, &status))?;
    for issuer in &protected.authorization_servers {
        if policy.validate_destination(issuer).await.is_err() {
            continue;
        }
        let Ok(authorization_server) = discover_authorization_server(policy, issuer).await else {
            continue;
        };
        if public_authorization_server_compatible(&authorization_server)
            && validate_authorization_server_destinations(policy, &authorization_server)
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
        .json(&serde_json::json!({
            "jsonrpc": "2.0",
            "id": "waltr-oauth-discovery",
            "method": "initialize",
            "params": {
                "protocolVersion": mcp::protocol::MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "waltr", "version": env!("CARGO_PKG_VERSION") }
            }
        }))
        .send()
        .await
        .map_err(|error| {
            Status::unavailable(format!("failed to request MCP OAuth challenge: {error}"))
        })?;
    validate_response_peer(policy, &response)?;
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
            get_oauth_json::<mcp::McpProtectedResourceMetadata>(policy, &destination).await
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
            get_oauth_json::<mcp::McpAuthorizationServerMetadata>(policy, &destination).await
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
    validate_response_peer(policy, &response)?;
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
    validate_response_peer(policy, &response)?;
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
    use waltr::grpc::waltr::config::v1::{McpServer, McpServerType};
    use tower::ServiceExt as _;

    use super::*;
    use crate::{
        config::{FilesystemConfigStore, default_configuration},
        persistence::{DatabaseConfig, Persistence},
    };

    #[test]
    fn missing_per_server_mcp_registration_allows_discovery_mechanisms() {
        let key = "registration-definitely-not-configured-7f4332";
        let registration = configured_mcp_registration(key, "https://waltr.example/callback")
            .expect("missing preregistration is not an error");
        assert!(registration.is_none());
    }

    #[test]
    fn typed_mcp_status_reports_only_non_secret_metadata() {
        let credential = McpOAuthState {
            version: 1,
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
            token: waltr_agent::McpOAuthToken {
                access_token: "access-secret".to_owned(),
                refresh_token: Some("refresh-secret".to_owned()),
                token_type: "Bearer".to_owned(),
                granted_scopes: vec!["read".to_owned(), "write".to_owned()],
                expires_at_ms: i64::MAX,
                refresh_generation: 4,
            },
        };

        let status = mcp_oauth_status("remote", &credential);

        assert_eq!(status.kind, AuthStatusKind::OauthConfigured as i32);
        assert_eq!(status.expires_at_ms, i64::MAX);
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
    }

    #[test]
    fn registration_only_status_and_attempt_failures_are_sanitized() {
        let credential = McpOAuthState {
            version: 1,
            resource: "https://mcp.example/resource".to_owned(),
            issuer: "https://auth.example".to_owned(),
            registration: McpOAuthRegistration {
                method: McpOAuthRegistrationMethod::ClientMetadata,
                client_id: "https://waltr.example/.well-known/oauth-client/waltr".to_owned(),
                ..McpOAuthRegistration::default()
            },
            endpoints: McpOAuthEndpoints::default(),
            token: McpOAuthToken::default(),
        };
        let status = mcp_oauth_status("remote", &credential);
        assert_eq!(status.kind, AuthStatusKind::AuthorizationRequired as i32);
        assert_eq!(status.supported_methods, [AuthMethod::OauthManual as i32]);
        assert_eq!(
            sanitized_completion_failure(&Status::unavailable(
                "request to https://secret.example/token?code=secret failed"
            )),
            "authorization server unavailable"
        );
    }

    #[tokio::test]
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
                        "redirect_uris": ["https://waltr.example/outbound-oauth/callback"],
                        "grant_types": ["authorization_code", "refresh_token"],
                        "response_types": ["code"],
                        "token_endpoint_auth_method": "none"
                    }))
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let policy = OutboundHttpPolicy::loopback_test();
        let discovery = discover_mcp_auth(&AuthTarget::mcp_server("remote"), &endpoint, policy)
            .await
            .unwrap();
        assert_eq!(discovery.protected.resource, endpoint);
        assert_eq!(discovery.authorization_server.issuer, issuer);
        assert_eq!(discovery.challenge.scope.as_deref(), Some("tools.read"));

        let registration = register_public_client_hardened(
            policy,
            &registration_endpoint,
            &McpPublicClientRegistrationRequest {
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
        let credential = McpOAuthState {
            version: 1,
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
            token: McpOAuthToken {
                access_token: "access-secret".to_owned(),
                refresh_token: Some("refresh-secret".to_owned()),
                ..McpOAuthToken::default()
            },
        };
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

        let root = crate::test_support::temp_root();
        let config_store = FilesystemConfigStore::new(root.clone());
        let mut config = default_configuration();
        config.mcp_servers.insert(
            "hosted".to_owned(),
            McpServer {
                name: "Hosted Waltr".to_owned(),
                r#type: McpServerType::Sse as i32,
                url: "https://waltr.example/mcp".to_owned(),
                ..McpServer::default()
            },
        );
        config_store.save(config).await.unwrap();
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let outbound_store = persistence.outbound_oauth_store();
        let credential_store =
            FileCredentialStore::new(root.join("auth.json")).with_mcp_store(outbound_store.clone());
        let service = AuthService::new(config_store, credential_store, outbound_store);
        let router = Router::new()
            .route(
                "/outbound-oauth/callback",
                get(
                    |State(service): State<Arc<AuthService<FilesystemConfigStore>>>,
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
