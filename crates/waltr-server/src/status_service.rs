//! Implementation of the generated `StatusService`.

use std::pin::Pin;
use tokio_stream::{Stream, StreamExt as _};
use tonic::{Request, Response, Status};
use waltr::grpc::waltr::status::v1::{
    GetFeatureStatusRequest, GetFeatureStatusResponse, SystemStatus, WatchStatusRequest,
    WatchStatusResponse, status_service_server::StatusService as GeneratedStatusService,
};

/// gRPC status service for system-level server readiness.
#[derive(Clone, Debug)]
pub struct StatusService(pub(crate) crate::readiness::Readiness);

#[tonic::async_trait]
impl GeneratedStatusService for StatusService {
    type WatchStatusStream =
        Pin<Box<dyn Stream<Item = Result<WatchStatusResponse, Status>> + Send>>;

    async fn watch_status(
        &self,
        _request: Request<WatchStatusRequest>,
    ) -> Result<Response<Self::WatchStatusStream>, Status> {
        tracing::trace!("watch status requested");
        let stream = tokio_stream::once(Ok(WatchStatusResponse {
            status: Some(SystemStatus { ready: true }),
        }))
        .chain(tokio_stream::pending());

        Ok(Response::new(Box::pin(stream)))
    }

    async fn get_feature_status(
        &self,
        _request: Request<GetFeatureStatusRequest>,
    ) -> Result<Response<GetFeatureStatusResponse>, Status> {
        Ok(Response::new(self.0.features().await))
    }
}
