//! Implementation of the generated `StatusService`.

use waltr::grpc::waltr::status::v1::{
    SystemStatus, WatchStatusRequest, WatchStatusResponse,
    status_service_server::StatusService as GeneratedStatusService,
};
use std::pin::Pin;
use tokio_stream::{Stream, StreamExt as _};
use tonic::{Request, Response, Status};

/// gRPC status service for system-level server readiness.
#[derive(Clone, Debug, Default)]
pub struct StatusService;

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
}
