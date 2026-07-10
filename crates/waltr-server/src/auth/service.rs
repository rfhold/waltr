use std::{collections::HashMap, sync::Arc};

use waltr::grpc::waltr::auth::v1::{
    AuthMethod, AuthStatus, AuthStatusKind, AuthTarget as ProtoAuthTarget, BeginAuthRequest,
    BeginAuthResponse, BeginProviderAuthRequest, BeginProviderAuthResponse, CancelAuthRequest,
    CancelAuthResponse, CancelProviderAuthRequest, CancelProviderAuthResponse, CompleteAuthRequest,
    CompleteAuthResponse, CompleteProviderAuthRequest, CompleteProviderAuthResponse,
    GetAuthStatusRequest, GetAuthStatusResponse, GetProviderAuthStatusRequest,
    GetProviderAuthStatusResponse, ListAuthStatusesRequest, ListAuthStatusesResponse,
    ListProviderAuthStatusesRequest, ListProviderAuthStatusesResponse, LogoutProviderRequest,
    LogoutProviderResponse, LogoutRequest, LogoutResponse, ProviderAuthMethod, ProviderAuthStatus,
    ProviderAuthStatusKind, auth_service_server::AuthService as GeneratedAuthService, auth_target,
};
use waltr::grpc::waltr::config::v1::{OAuthProvider, OauthTokenRequestStyle};
use mcp::oauth::{
    McpBearerChallenge, discover_authorization_server_metadata,
    discover_protected_resource_metadata,
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::config::{
    ConfigStore,
    headers::{configured_credential, has_unresolved_credential, resolve_provider_headers},
};

use super::{AuthTarget, FileCredentialStore, StoredCredential, oauth};

/// gRPC auth service for provider credentials.
#[derive(Debug)]
pub struct AuthService<S> {
    config_store: S,
    credential_store: FileCredentialStore,
    attempts: Mutex<HashMap<String, oauth::OAuthAttempt>>,
}

impl<S> AuthService<S>
where
    S: ConfigStore,
{
    /// Create an auth service.
    #[must_use]
    pub fn new(config_store: S, credential_store: FileCredentialStore) -> Arc<Self> {
        Arc::new(Self {
            config_store,
            credential_store,
            attempts: Mutex::new(HashMap::new()),
        })
    }

    /// Return the credential store used by this service.
    #[must_use]
    pub fn credential_store(&self) -> &FileCredentialStore {
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
            None => Ok(AuthStatus {
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
    ) -> Result<BeginProviderAuthResponse, Status> {
        let config = self.config_store.load().await.map_err(|error| {
            Status::internal(format!("failed to load config for MCP auth: {error}"))
        })?;
        let server = config.mcp_servers.get(&server_key).ok_or_else(|| {
            Status::not_found(format!("mcp server '{server_key}' does not exist"))
        })?;
        if server.url.is_empty() {
            return Err(Status::failed_precondition(format!(
                "mcp server '{server_key}' does not have a streamable HTTP URL"
            )));
        }

        let client = reqwest::Client::new();
        let protected = discover_protected_resource_metadata(
            &client,
            &server.url,
            Some(&McpBearerChallenge::new()),
        )
        .await
        .map_err(|error| {
            Status::failed_precondition(format!(
                "failed to discover MCP OAuth protected resource metadata: {error}"
            ))
        })?;
        let issuer = protected.authorization_servers.first().ok_or_else(|| {
            Status::failed_precondition(
                "MCP protected resource metadata does not advertise an authorization server",
            )
        })?;
        let auth_server = discover_authorization_server_metadata(&client, issuer)
            .await
            .map_err(|error| {
                Status::failed_precondition(format!(
                    "failed to discover MCP OAuth authorization server metadata: {error}"
                ))
            })?;
        let authorize_url = auth_server.authorization_endpoint.ok_or_else(|| {
            Status::failed_precondition(
                "MCP OAuth authorization server metadata is missing authorization_endpoint",
            )
        })?;
        let token_url = auth_server.token_endpoint.ok_or_else(|| {
            Status::failed_precondition(
                "MCP OAuth authorization server metadata is missing token_endpoint",
            )
        })?;
        let definition = OAuthProvider {
            name: format!("MCP {server_key}"),
            authorize_url,
            token_url,
            client_id: std::env::var("WALTR_MCP_OAUTH_CLIENT_ID")
                .unwrap_or_else(|_| "waltr-test".to_owned()),
            redirect_uri: std::env::var("WALTR_MCP_OAUTH_REDIRECT_URI")
                .unwrap_or_else(|_| "http://127.0.0.1/oauth/callback".to_owned()),
            scopes: if protected.scopes_supported.is_empty() {
                vec![
                    "openid".to_owned(),
                    "email".to_owned(),
                    "profile".to_owned(),
                ]
            } else {
                protected.scopes_supported
            },
            token_request_style: OauthTokenRequestStyle::FormUrlencoded as i32,
            ..OAuthProvider::default()
        };
        let (attempt, mut response) =
            oauth::begin_manual(AuthTarget::mcp_server(server_key), definition, now_ms())?;
        let attempt_id = Uuid::new_v4().to_string();
        response.attempt_id.clone_from(&attempt_id);
        self.attempts.lock().await.insert(attempt_id, attempt);
        Ok(response)
    }

    async fn complete_attempt(&self, attempt_id: &str, input: &str) -> Result<AuthStatus, Status> {
        let attempt = self
            .attempts
            .lock()
            .await
            .get(attempt_id)
            .cloned()
            .ok_or_else(|| Status::not_found("OAuth attempt does not exist"))?;
        let mut credential = oauth::complete_manual(&attempt, input, now_ms()).await?;
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
        let target = attempt.target().clone();
        self.attempts.lock().await.remove(attempt_id);
        self.credential_store
            .modify(&target.credential_key(), |_| {
                Some(StoredCredential::OAuth(credential))
            })
            .await
            .map_err(|error| Status::internal(format!("failed to store credential: {error}")))?;
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
        let response = match target {
            AuthTarget::Provider { key } => self
                .begin_provider_auth(Request::new(BeginProviderAuthRequest {
                    provider_key: key,
                    method: provider_method_from_auth(request.method) as i32,
                }))
                .await?
                .into_inner(),
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

    async fn cancel_auth(
        &self,
        request: Request<CancelAuthRequest>,
    ) -> Result<Response<CancelAuthResponse>, Status> {
        self.attempts
            .lock()
            .await
            .remove(&request.into_inner().attempt_id);
        Ok(Response::new(CancelAuthResponse {}))
    }

    async fn logout(
        &self,
        request: Request<LogoutRequest>,
    ) -> Result<Response<LogoutResponse>, Status> {
        let target = auth_target_from_proto(request.into_inner().target)?;
        self.credential_store
            .delete(&target.credential_key())
            .await
            .map_err(|error| Status::internal(format!("failed to delete credential: {error}")))?;
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
        let attempt_id = Uuid::new_v4().to_string();
        response.attempt_id.clone_from(&attempt_id);
        self.attempts.lock().await.insert(attempt_id, attempt);
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
    let provider_key = match status.target.and_then(|target| target.target) {
        Some(auth_target::Target::ProviderKey(provider_key)) => provider_key,
        _ => {
            return Err(Status::internal(
                "auth status did not contain a provider target",
            ));
        }
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
        .map_or(0, |duration| duration.as_millis() as i64)
}
