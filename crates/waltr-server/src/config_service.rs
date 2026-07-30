//! Implementation of the generated `ConfigService`.

use crate::config::{
    ConfigError, ConfigPatch, ConfigPubSub, ConfigStore, ConfigWatchEvent, DefaultConfigBuilder,
    FilesystemConfigPubSub, FilesystemConfigStore,
};
use waltr::grpc::waltr::config::v1::{
    GetConfigRequest, GetConfigResponse, PatchConfigRequest, PatchConfigResponse,
    ResetConfigRequest, ResetConfigResponse, WatchConfigRequest, WatchConfigResponse,
    config_service_server::ConfigService as GeneratedConfigService,
};
use std::{fmt, path::PathBuf, pin::Pin, sync::Arc};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status};

/// gRPC config service backed directly by storage and pubsub implementations.
pub struct ConfigService<S = FilesystemConfigStore, P = FilesystemConfigPubSub> {
    store: S,
    pubsub: P,
}

impl<S, P> fmt::Debug for ConfigService<S, P>
where
    S: fmt::Debug,
    P: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConfigService")
            .field("store", &self.store)
            .field("pubsub", &self.pubsub)
            .finish()
    }
}

impl ConfigService<FilesystemConfigStore, FilesystemConfigPubSub> {
    /// Create a service rooted at the current working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the current directory cannot be resolved or the watcher cannot start.
    pub fn from_current_dir(
        default_builder: DefaultConfigBuilder,
    ) -> Result<Arc<Self>, ConfigError> {
        let root = std::env::current_dir().map_err(ConfigError::current_dir)?;
        tracing::debug!(root = %root.display(), "creating config service from current directory");
        Self::new(root, default_builder)
    }

    /// Create a service rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error when the filesystem watcher cannot start.
    pub fn new(
        root: impl Into<PathBuf>,
        default_builder: DefaultConfigBuilder,
    ) -> Result<Arc<Self>, ConfigError> {
        let root = root.into();
        tracing::debug!(root = %root.display(), "creating config service");
        let store = FilesystemConfigStore::new(root, default_builder);
        let pubsub = FilesystemConfigPubSub::watching_file_store(store.clone())?;
        Ok(Self::from_parts(store, pubsub))
    }

    pub(crate) fn catalog_loader(&self) -> crate::catalog::CatalogLoader {
        self.store.catalog_loader()
    }
}

impl<S, P> ConfigService<S, P>
where
    S: ConfigStore,
    P: ConfigPubSub,
{
    /// Create a service from explicit storage and pubsub implementations.
    #[must_use]
    pub fn from_parts(store: S, pubsub: P) -> Arc<Self> {
        tracing::debug!("creating config service from explicit parts");
        Arc::new(Self { store, pubsub })
    }

    /// Return the backing config store.
    #[must_use]
    pub const fn store(&self) -> &S {
        &self.store
    }

    /// Return the backing config pubsub.
    #[must_use]
    pub const fn pubsub(&self) -> &P {
        &self.pubsub
    }
}

#[tonic::async_trait]
impl<S, P> GeneratedConfigService for ConfigService<S, P>
where
    S: ConfigStore,
    P: ConfigPubSub,
{
    type WatchConfigStream =
        Pin<Box<dyn Stream<Item = Result<WatchConfigResponse, Status>> + Send>>;

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "patch_config", expected_etag, etag)
    )]
    async fn patch_config(
        &self,
        request: Request<PatchConfigRequest>,
    ) -> Result<Response<PatchConfigResponse>, Status> {
        let request = request.into_inner();
        tracing::Span::current().record("expected_etag", request.expected_etag.as_str());
        tracing::info!("config patch accepted");
        tracing::debug!(expected_etag = %request.expected_etag, "patch config expected etag");
        let patch = ConfigPatch {
            providers: request.providers.map(|patch| patch.providers),
            mcp_servers: request.mcp_servers.map(|patch| patch.mcp_servers),
            runtime_defaults: request.runtime_defaults,
        };
        let config = self
            .store
            .patch(patch, &request.expected_etag)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "config patch failed");
                config_error_to_status(&error)
            })?;
        tracing::Span::current().record("etag", config.etag.as_str());
        tracing::info!(etag = %config.etag, "config patch completed");

        Ok(Response::new(PatchConfigResponse {
            config: Some(config),
        }))
    }

    async fn get_config(
        &self,
        _request: Request<GetConfigRequest>,
    ) -> Result<Response<GetConfigResponse>, Status> {
        tracing::trace!("get config requested");
        let config = self.store.load().await.map_err(|error| {
            tracing::warn!(%error, "get config failed");
            config_error_to_status(&error)
        })?;
        tracing::trace!(etag = %config.etag, "get config completed");

        Ok(Response::new(GetConfigResponse {
            config: Some(config),
        }))
    }

    #[tracing::instrument(
        name = "server.write",
        skip(self, request),
        fields(operation = "reset_config", expected_etag, etag)
    )]
    async fn reset_config(
        &self,
        request: Request<ResetConfigRequest>,
    ) -> Result<Response<ResetConfigResponse>, Status> {
        let request = request.into_inner();
        tracing::Span::current().record("expected_etag", request.expected_etag.as_str());
        let config = self
            .store
            .reset(&request.expected_etag)
            .await
            .map_err(|error| {
                tracing::warn!(%error, "config reset failed");
                config_error_to_status(&error)
            })?;
        tracing::Span::current().record("etag", config.etag.as_str());

        Ok(Response::new(ResetConfigResponse {
            config: Some(config),
        }))
    }

    async fn watch_config(
        &self,
        _request: Request<WatchConfigRequest>,
    ) -> Result<Response<Self::WatchConfigStream>, Status> {
        tracing::trace!("watch config requested");
        let subscription = self.pubsub.subscribe();
        let config = self.store.load().await.map_err(|error| {
            tracing::warn!(%error, "watch config initial snapshot failed");
            config_error_to_status(&error)
        })?;
        let initial = tokio_stream::once(Ok(config_snapshot_to_response(config)));
        let changes = subscription.filter_map(|event| {
            event
                .ok()
                .map(|event| Ok(config_watch_event_to_response(event)))
        });
        let stream = initial.chain(changes);

        Ok(Response::new(Box::pin(stream)))
    }
}

fn config_snapshot_to_response(
    config: waltr::grpc::waltr::config::v1::Configuration,
) -> WatchConfigResponse {
    WatchConfigResponse {
        etag: config.etag.clone(),
        config: Some(config),
        ..WatchConfigResponse::default()
    }
}

fn config_watch_event_to_response(event: ConfigWatchEvent) -> WatchConfigResponse {
    match event {
        ConfigWatchEvent::Changed {
            previous_etag,
            etag,
            changed_keys,
            config,
        } => WatchConfigResponse {
            previous_etag,
            etag,
            changed_keys,
            config: Some(config),
            error: String::new(),
        },
        ConfigWatchEvent::Error { error } => WatchConfigResponse {
            error,
            ..WatchConfigResponse::default()
        },
    }
}

fn config_error_to_status(error: &ConfigError) -> Status {
    if error.is_etag_mismatch() {
        Status::failed_precondition(error.to_string())
    } else if error.is_invalid() {
        Status::invalid_argument(error.to_string())
    } else if error.is_unavailable() {
        Status::unavailable(error.to_string())
    } else {
        Status::internal(error.to_string())
    }
}
