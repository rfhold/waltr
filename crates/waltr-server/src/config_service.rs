//! Unified live-resource gRPC service.

use std::{path::PathBuf, pin::Pin, sync::Arc};

use waltr::grpc::waltr::resource::v1::{
    GetResourceCatalogRequest, GetResourceCatalogResponse, InspectResourceRequest,
    InspectResourceResponse, ReplaceResourceRequest, ReplaceResourceResponse, ResetResourceRequest,
    ResetResourceResponse, WatchResourceCatalogRequest, WatchResourceCatalogResponse,
    resource_service_server::ResourceService as GeneratedResourceService,
};
use tokio_stream::{Stream, StreamExt as _, wrappers::BroadcastStream};
use tonic::{Request, Response, Status};

use crate::{
    agent_definition::{AgentDefinitionLoadError, AgentDefinitionState},
    catalog::{CatalogLoader, CatalogMaterializer},
    resource_error::ResourceError,
};

/// Resource administration service backed by one last-known-good catalog state.
#[derive(Clone, Debug)]
pub struct ConfigService {
    state: AgentDefinitionState,
    loader: CatalogLoader,
}

impl ConfigService {
    /// Load the complete catalog rooted at the current working directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the current directory, models.dev catalog, or resource catalog cannot
    /// be initialized.
    pub async fn from_current_dir(loader: CatalogLoader) -> Result<Arc<Self>, ResourceError> {
        let root = std::env::current_dir().map_err(ResourceError::current_dir)?;
        Self::new(root, loader).await
    }

    /// Load the complete catalog rooted at `root`.
    ///
    /// # Errors
    ///
    /// Returns an error when the models.dev catalog or resource catalog cannot be initialized.
    pub async fn new(
        root: impl Into<PathBuf>,
        loader: CatalogLoader,
    ) -> Result<Arc<Self>, ResourceError> {
        let materializer = CatalogMaterializer::load(&loader)
            .await
            .map_err(|error| ResourceError::unavailable(error.to_string()))?;
        let state = AgentDefinitionState::load_unified(root, materializer)
            .map_err(|error| resource_error_to_config(&error))?;
        Ok(Arc::new(Self { state, loader }))
    }

    /// Construct from an already initialized state.
    #[must_use]
    pub fn from_state(state: AgentDefinitionState, loader: CatalogLoader) -> Arc<Self> {
        Arc::new(Self { state, loader })
    }

    /// Return the shared live resource state.
    #[must_use]
    pub const fn state(&self) -> &AgentDefinitionState {
        &self.state
    }

    /// Return the models.dev loader shared with catalog search.
    #[must_use]
    pub fn catalog_loader(&self) -> CatalogLoader {
        self.loader.clone()
    }
}

#[tonic::async_trait]
impl GeneratedResourceService for ConfigService {
    type WatchResourceCatalogStream =
        Pin<Box<dyn Stream<Item = Result<WatchResourceCatalogResponse, Status>> + Send>>;

    async fn get_resource_catalog(
        &self,
        _request: Request<GetResourceCatalogRequest>,
    ) -> Result<Response<GetResourceCatalogResponse>, Status> {
        Ok(Response::new(GetResourceCatalogResponse {
            catalog: Some(self.state.resource_catalog()),
        }))
    }

    async fn watch_resource_catalog(
        &self,
        _request: Request<WatchResourceCatalogRequest>,
    ) -> Result<Response<Self::WatchResourceCatalogStream>, Status> {
        let updates = self.state.subscribe();
        Ok(Response::new(resource_catalog_stream(&self.state, updates)))
    }

    async fn inspect_resource(
        &self,
        request: Request<InspectResourceRequest>,
    ) -> Result<Response<InspectResourceResponse>, Status> {
        let path = request.into_inner().path;
        let (resources, catalog_etag, reload_error) = self
            .state
            .resource_inspection_snapshot(path.as_deref())
            .map_err(|error| resource_error_to_status(&error))?;
        Ok(Response::new(InspectResourceResponse {
            resources,
            catalog_etag,
            reload_error,
        }))
    }

    async fn replace_resource(
        &self,
        request: Request<ReplaceResourceRequest>,
    ) -> Result<Response<ReplaceResourceResponse>, Status> {
        let request = request.into_inner();
        self.state
            .replace(&request.path, request.source, &request.expected_etag)
            .await
            .map_err(|error| resource_error_to_status(&error))?;
        let resource = self
            .state
            .resource_inspections(Some(&request.path))
            .map_err(|error| resource_error_to_status(&error))?
            .into_iter()
            .next();
        Ok(Response::new(ReplaceResourceResponse {
            resource,
            catalog: Some(self.state.resource_catalog()),
        }))
    }

    async fn reset_resource(
        &self,
        request: Request<ResetResourceRequest>,
    ) -> Result<Response<ResetResourceResponse>, Status> {
        let request = request.into_inner();
        self.state
            .reset(&request.path, &request.expected_etag)
            .await
            .map_err(|error| resource_error_to_status(&error))?;
        let resource = self
            .state
            .resource_inspections(Some(&request.path))
            .map_err(|error| resource_error_to_status(&error))?
            .into_iter()
            .next();
        Ok(Response::new(ResetResourceResponse {
            resource,
            catalog: Some(self.state.resource_catalog()),
        }))
    }
}

fn resource_catalog_stream(
    state: &AgentDefinitionState,
    receiver: tokio::sync::broadcast::Receiver<crate::agent_definition::PublishedState>,
) -> Pin<Box<dyn Stream<Item = Result<WatchResourceCatalogResponse, Status>> + Send>> {
    let publication = state.publication();
    let initial_catalog = AgentDefinitionState::published_resource_catalog(&publication);
    let mut last = (
        initial_catalog.etag.clone(),
        initial_catalog.reload_error.clone(),
    );
    let initial = tokio_stream::once(Ok(WatchResourceCatalogResponse {
        catalog: Some(initial_catalog),
    }));
    let updates = BroadcastStream::new(receiver).filter_map(move |event| {
        let publication = event.ok()?;
        let catalog = AgentDefinitionState::published_resource_catalog(&publication);
        let key = (catalog.etag.clone(), catalog.reload_error.clone());
        if key == last {
            return None;
        }
        last = key;
        Some(Ok(WatchResourceCatalogResponse {
            catalog: Some(catalog),
        }))
    });
    Box::pin(initial.chain(updates))
}

fn resource_error_to_config(error: &AgentDefinitionLoadError) -> ResourceError {
    if error.is_validation() {
        ResourceError::invalid(error.to_string())
    } else {
        ResourceError::unavailable(error.to_string())
    }
}

fn resource_error_to_status(error: &AgentDefinitionLoadError) -> Status {
    if error.is_failed_precondition() {
        Status::failed_precondition(error.to_string())
    } else if error.is_validation() {
        Status::invalid_argument(error.sanitized_validation_message())
    } else {
        Status::internal("resource catalog service unavailable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::Duration};

    #[tokio::test]
    async fn aggregate_read_and_watch_return_complete_snapshots() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let service =
            ConfigService::from_state(state.clone(), crate::test_support::default_catalog_loader());
        let response = service
            .get_resource_catalog(Request::new(GetResourceCatalogRequest {}))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(response.catalog.unwrap().resources.len(), 6);

        let mut watch = service
            .watch_resource_catalog(Request::new(WatchResourceCatalogRequest {}))
            .await
            .unwrap()
            .into_inner();
        let initial = watch.next().await.unwrap().unwrap().catalog.unwrap();
        let initial_resource = initial
            .resources
            .iter()
            .find(|resource| resource.path == "agents/coding.yaml")
            .unwrap();
        assert!(!initial_resource.active_source.is_empty());
        assert!(
            initial_resource
                .persisted_etag
                .starts_with("absent:sha256:")
        );
        let inspection = state
            .resource_inspections(Some("agents/coding.yaml"))
            .unwrap()
            .remove(0);
        state
            .replace(
                "agents/coding.yaml",
                "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: coding\nspec:\n  description: watched\n".to_owned(),
                &inspection.persisted_etag,
            )
            .await
            .unwrap();
        let changed = watch.next().await.unwrap().unwrap().catalog.unwrap();
        assert_ne!(changed.etag, initial.etag);
        assert_eq!(changed.resources.len(), 6);
    }

    #[tokio::test]
    async fn subscription_before_initial_projection_cannot_miss_a_publication() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let receiver = state.subscribe();
        let inspection = state
            .resource_inspections(Some("agents/coding.yaml"))
            .unwrap()
            .remove(0);
        state
            .replace(
                "agents/coding.yaml",
                "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: coding\nspec:\n  description: first\n".to_owned(),
                &inspection.persisted_etag,
            )
            .await
            .unwrap();

        let mut watch = resource_catalog_stream(&state, receiver);
        let initial = watch.next().await.unwrap().unwrap().catalog.unwrap();
        assert_eq!(initial.etag, state.snapshot().etag());

        let inspection = state
            .resource_inspections(Some("agents/coding.yaml"))
            .unwrap()
            .remove(0);
        state
            .replace(
                "agents/coding.yaml",
                "apiVersion: waltr.dev/v1alpha1\nkind: Agent\nmetadata:\n  name: coding\nspec:\n  description: second\n".to_owned(),
                &inspection.persisted_etag,
            )
            .await
            .unwrap();
        let changed = watch.next().await.unwrap().unwrap().catalog.unwrap();
        assert_ne!(changed.etag, initial.etag);
    }

    #[tokio::test]
    async fn watch_streams_rejected_reload_and_exact_restoration_clear() {
        let root = crate::test_support::temp_root();
        let state = AgentDefinitionState::load(&root, &[], "coding").unwrap();
        let service =
            ConfigService::from_state(state.clone(), crate::test_support::default_catalog_loader());
        let mut watch = service
            .watch_resource_catalog(Request::new(WatchResourceCatalogRequest {}))
            .await
            .unwrap()
            .into_inner();
        let initial = watch.next().await.unwrap().unwrap().catalog.unwrap();
        let path = root.join(".waltr/resources/agents/coding.yaml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "kind: [").unwrap();
        state.reload_from_disk().await;

        let rejected = watch.next().await.unwrap().unwrap().catalog.unwrap();
        assert_eq!(rejected.etag, initial.etag);
        assert!(!rejected.reload_error.is_empty());
        let resource = rejected
            .resources
            .iter()
            .find(|resource| resource.path == "agents/coding.yaml")
            .unwrap();
        assert_eq!(resource.active_source, initial.resources[0].active_source);
        assert!(resource.persisted_etag.starts_with("sha256:"));

        fs::remove_file(path).unwrap();
        state.reload_from_disk().await;
        let cleared = tokio::time::timeout(Duration::from_secs(1), watch.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .catalog
            .unwrap();
        assert_eq!(cleared.etag, initial.etag);
        assert!(cleared.reload_error.is_empty());
    }
}
