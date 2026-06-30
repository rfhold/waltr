use std::{collections::HashMap, sync::Arc};

use waltr::grpc::waltr::auth::v1::{
    BeginProviderAuthRequest, BeginProviderAuthResponse, CancelProviderAuthRequest,
    CancelProviderAuthResponse, CompleteProviderAuthRequest, CompleteProviderAuthResponse,
    GetProviderAuthStatusRequest, GetProviderAuthStatusResponse, ListProviderAuthStatusesRequest,
    ListProviderAuthStatusesResponse, LogoutProviderRequest, LogoutProviderResponse,
    ProviderAuthMethod, ProviderAuthStatus, ProviderAuthStatusKind,
    auth_service_server::AuthService as GeneratedAuthService,
};
use tokio::sync::Mutex;
use tonic::{Request, Response, Status};
use uuid::Uuid;

use crate::config::{
    ConfigStore,
    headers::{configured_credential, has_unresolved_credential, resolve_provider_headers},
};

use super::{FileCredentialStore, StoredCredential, oauth};

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

    async fn status_for(&self, provider_key: &str) -> Result<ProviderAuthStatus, Status> {
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
            return Ok(ProviderAuthStatus {
                provider_key: provider_key.to_owned(),
                kind: ProviderAuthStatusKind::ApiKeyConfigured as i32,
                supported_methods,
                ..ProviderAuthStatus::default()
            });
        }
        let Some(_definition) = definition else {
            if has_unresolved_credential(&resolved_headers) {
                return Ok(ProviderAuthStatus {
                    provider_key: provider_key.to_owned(),
                    kind: ProviderAuthStatusKind::NotConfigured as i32,
                    message: "credential environment variable is not set".to_owned(),
                    ..ProviderAuthStatus::default()
                });
            }
            return Ok(ProviderAuthStatus {
                provider_key: provider_key.to_owned(),
                kind: ProviderAuthStatusKind::Unsupported as i32,
                message: if provider.oauth_compatible {
                    "provider requires custom OAuth settings".to_owned()
                } else {
                    "provider is not OAuth compatible".to_owned()
                },
                ..ProviderAuthStatus::default()
            });
        };
        match self
            .credential_store
            .read(provider_key)
            .await
            .map_err(|error| Status::internal(format!("failed to read credentials: {error}")))?
        {
            Some(StoredCredential::OAuth(credential)) => {
                let expired = credential.expires_at_ms <= now_ms();
                Ok(ProviderAuthStatus {
                    provider_key: provider_key.to_owned(),
                    kind: if expired {
                        ProviderAuthStatusKind::Expired as i32
                    } else {
                        ProviderAuthStatusKind::OauthConfigured as i32
                    },
                    supported_methods,
                    expires_at_ms: credential.expires_at_ms,
                    metadata: credential.metadata.into_iter().collect(),
                    ..ProviderAuthStatus::default()
                })
            }
            None => Ok(ProviderAuthStatus {
                provider_key: provider_key.to_owned(),
                kind: ProviderAuthStatusKind::NotConfigured as i32,
                supported_methods,
                ..ProviderAuthStatus::default()
            }),
        }
    }
}

#[tonic::async_trait]
impl<S> GeneratedAuthService for AuthService<S>
where
    S: ConfigStore,
{
    async fn list_provider_auth_statuses(
        &self,
        _request: Request<ListProviderAuthStatusesRequest>,
    ) -> Result<Response<ListProviderAuthStatusesResponse>, Status> {
        let config = self.config_store.load().await.map_err(|error| {
            Status::internal(format!("failed to load config for auth statuses: {error}"))
        })?;
        let mut statuses = Vec::with_capacity(config.providers.len());
        for provider_key in config.providers.keys() {
            statuses.push(self.status_for(provider_key).await?);
        }
        Ok(Response::new(ListProviderAuthStatusesResponse { statuses }))
    }

    async fn get_provider_auth_status(
        &self,
        request: Request<GetProviderAuthStatusRequest>,
    ) -> Result<Response<GetProviderAuthStatusResponse>, Status> {
        let status = self.status_for(&request.into_inner().provider_key).await?;
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
        let (attempt, mut response) =
            oauth::begin_manual(request.provider_key, definition.definition, now_ms())?;
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
        let credential = oauth::complete_manual(&attempt, &request.input, now_ms()).await?;
        self.attempts.lock().await.remove(&request.attempt_id);
        self.credential_store
            .modify(attempt.provider_key(), |_| {
                Some(StoredCredential::OAuth(credential))
            })
            .await
            .map_err(|error| Status::internal(format!("failed to store credential: {error}")))?;
        let status = self.status_for(attempt.provider_key()).await?;
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
            .delete(&provider_key)
            .await
            .map_err(|error| Status::internal(format!("failed to delete credential: {error}")))?;
        let status = self.status_for(&provider_key).await?;
        Ok(Response::new(LogoutProviderResponse {
            status: Some(status),
        }))
    }
}

fn supported_methods() -> Vec<i32> {
    vec![ProviderAuthMethod::OauthManual as i32]
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() as i64)
}
