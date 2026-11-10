//! Production Routine RPC adapter over the shared invocation service.
use crate::{
    access_auth::AuthenticatedPrincipal,
    routine::{
        definition::Trigger,
        history::{Caller, Origin, Ownership},
        rpc::{
            self, StartRequest,
            api::{self, ReadAdapter, ResponseStream},
            attachments::AttachmentRegistry,
            blobs::BlobRegistry,
            catalog::PublicDefinition,
            wire,
        },
        service::ChildInvocation,
    },
};
use futures_util::{StreamExt as _, stream};
use prost::Message as _;
use std::{sync::Arc, time::Duration};
use tonic::{Request, Response, Status};
use waltr::grpc::waltr::routine::v2::{
    self as p,
    routine_service_server::{RoutineService, RoutineServiceServer},
};

/// All 21 Routine RPCs over one existing invocation service and injected shared registries.
#[derive(Clone)]
pub struct RoutineRpcService {
    read: ReadAdapter,
    attachments: Arc<AttachmentRegistry>,
    blobs: Arc<BlobRegistry>,
}
impl std::fmt::Debug for RoutineRpcService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoutineRpcService").finish_non_exhaustive()
    }
}
impl RoutineRpcService {
    /// Reuse these registries across every service clone and native/browser transport.
    pub fn new(
        read: ReadAdapter,
        attachments: Arc<AttachmentRegistry>,
        blobs: Arc<BlobRegistry>,
    ) -> Self {
        Self {
            read,
            attachments,
            blobs,
        }
    }
    /// Bound decoding before allocating an application input batch.
    pub fn into_server(self) -> RoutineServiceServer<Self> {
        RoutineServiceServer::new(self)
            .max_decoding_message_size(131072)
            .max_encoding_message_size(16777216)
    }
    /// Join transport owners before shutting down the shared invocation service.
    pub async fn shutdown(&self) -> Result<(), crate::routine::ports::PortError> {
        let attachments = self.attachments.shutdown().await;
        let readers = self.blobs.shutdown().await;
        let service = self.read.service.shutdown().await;
        attachments?;
        readers?;
        service
    }
    async fn open(
        &self,
        principal: AuthenticatedPrincipal,
        open: p::InteractionOpen,
        native: bool,
    ) -> Result<(Arc<rpc::attachments::Attachment>, p::AttachmentAdmission), Status> {
        if open.encoded_len() > 131072 {
            return Err(Status::invalid_argument("interaction open bound"));
        }
        let owner = rpc::principal_id(&principal);
        let selection = open
            .selection
            .ok_or_else(|| Status::invalid_argument("missing interaction selection"))?;
        let (entry, input) = self
            .read
            .selection(&owner, &selection, Trigger::Interactive)?;
        let deadline = Duration::from_millis(entry.definition.envelope().spec.budgets.deadline_ms);
        let (pending, slots) = self
            .attachments
            .prepare(owner.clone(), entry.definition.clone(), open.inputs)
            .await?;
        if native {
            pending.require_native_eof();
        }
        *pending
            .blob_registry
            .lock()
            .map_err(|_| Status::unavailable("blob registry"))? = Some(Arc::downgrade(&self.blobs));
        let source = selection.source_session.clone();
        let authorized = self
            .read
            .policy
            .authorize(StartRequest {
                authenticated: principal.clone(),
                transport: Some(pending.clone()),
                principal: owner.clone(),
                definition: entry.definition.clone(),
                entrypoint: selection.entrypoint.clone(),
                source_session: source.clone(),
            })
            .await
            .map_err(wire::error)?;
        if authorized.source_session != source {
            return Err(Status::permission_denied("source binding mismatch"));
        }
        let projection =
            serde_json::to_value(PublicDefinition::capture(&entry).map_err(wire::error)?)
                .map_err(|_| Status::internal("public projection"))?;
        let admission = rpc::invoke_authorized(
            &self.read.service,
            ChildInvocation {
                definition: entry.definition,
                entrypoint: selection.entrypoint,
                caller: Caller {
                    principal: owner,
                    origin: Origin::Interactive,
                    ownership: Ownership::Leased,
                    correlation: selection.correlation,
                },
                input,
            },
            authorized,
            projection,
            Some(pending.clone()),
        )
        .map_err(wire::error)?;
        self.attachments
            .admit(pending, slots, admission, principal, deadline)
            .await
    }
}
#[tonic::async_trait]
impl RoutineService for RoutineRpcService {
    type WatchDefinitionsStream = ResponseStream<p::WatchDefinitionsResponse>;
    type WatchInvocationsStream = ResponseStream<p::WatchInvocationsResponse>;
    type InteractStream = ResponseStream<p::InteractResponse>;
    type WatchLiveOutputStream = ResponseStream<p::WatchLiveOutputResponse>;
    type ReadBlobStream = ResponseStream<p::ReadBlobResponse>;
    async fn watch_definitions(
        &self,
        request: Request<p::WatchDefinitionsRequest>,
    ) -> Result<Response<Self::WatchDefinitionsStream>, Status> {
        self.read.watch_definitions(request).await
    }
    async fn start_invocation(
        &self,
        request: Request<p::StartInvocationRequest>,
    ) -> Result<Response<p::StartInvocationResponse>, Status> {
        self.read.start_invocation(request).await
    }
    async fn list_invocations(
        &self,
        request: Request<p::ListInvocationsRequest>,
    ) -> Result<Response<p::ListInvocationsResponse>, Status> {
        self.read.list_invocations(request).await
    }
    async fn get_invocation(
        &self,
        request: Request<p::GetInvocationRequest>,
    ) -> Result<Response<p::GetInvocationResponse>, Status> {
        self.read.get_invocation(request).await
    }
    async fn watch_invocations(
        &self,
        request: Request<p::WatchInvocationsRequest>,
    ) -> Result<Response<Self::WatchInvocationsStream>, Status> {
        self.read.watch_invocations(request).await
    }
    async fn list_operations(
        &self,
        request: Request<p::ListOperationsRequest>,
    ) -> Result<Response<p::ListOperationsResponse>, Status> {
        self.read.list_operations(request).await
    }
    async fn cancel_invocation(
        &self,
        request: Request<p::CancelInvocationRequest>,
    ) -> Result<Response<p::CancelInvocationResponse>, Status> {
        self.read.cancel_invocation(request).await
    }
    async fn read_text_result(
        &self,
        request: Request<p::ReadTextResultRequest>,
    ) -> Result<Response<p::ReadTextResultResponse>, Status> {
        self.read.read_text_result(request).await
    }
    async fn open_attachment(
        &self,
        request: Request<p::OpenAttachmentRequest>,
    ) -> Result<Response<p::OpenAttachmentResponse>, Status> {
        let principal = api::principal(&request)?;
        let request = request.into_inner();
        let result = match request.open {
            Some(open) => self
                .open(principal, open, false)
                .await
                .map(|(_, admission)| p::open_attachment_response::Admission::Accepted(admission)),
            None => Err(Status::invalid_argument("missing interaction open")),
        };
        Ok(Response::new(p::OpenAttachmentResponse {
            admission: Some(result.unwrap_or_else(|error| {
                p::open_attachment_response::Admission::Rejected(rejected(error))
            })),
        }))
    }
    async fn send_input_batch(
        &self,
        request: Request<p::SendInputBatchRequest>,
    ) -> Result<Response<p::SendInputBatchResponse>, Status> {
        let principal = api::principal(&request)?;
        let request = request.into_inner();
        if request.encoded_len() > 131072 {
            return Err(Status::invalid_argument("input batch bound"));
        }
        self.attachments
            .send(&rpc::principal_id(&principal), request)
            .await
            .map(Response::new)
    }
    async fn watch_live_output(
        &self,
        request: Request<p::WatchLiveOutputRequest>,
    ) -> Result<Response<Self::WatchLiveOutputStream>, Status> {
        let principal = api::principal(&request)?;
        self.attachments
            .watch(&rpc::principal_id(&principal), request.into_inner())
            .map(Response::new)
    }
    async fn renew_attachment(
        &self,
        request: Request<p::RenewAttachmentRequest>,
    ) -> Result<Response<p::RenewAttachmentResponse>, Status> {
        let principal = api::principal(&request)?;
        self.attachments
            .renew(&rpc::principal_id(&principal), request.into_inner())
            .map(Response::new)
    }
    async fn close_attachment(
        &self,
        request: Request<p::CloseAttachmentRequest>,
    ) -> Result<Response<p::CloseAttachmentResponse>, Status> {
        let principal = api::principal(&request)?;
        self.attachments
            .close(&rpc::principal_id(&principal), request.into_inner())
            .await
            .map(Response::new)
    }
    async fn interact(
        &self,
        request: Request<tonic::Streaming<p::InteractRequest>>,
    ) -> Result<Response<Self::InteractStream>, Status> {
        use tracing::Instrument as _;
        let operation = crate::routine::telemetry::rpc(request.metadata(), false);
        let result = async {
        let principal = api::principal(&request)?;
        let mut input = request.into_inner();
        let mut access = rpc::access_ended(&principal);
        let first = tokio::select! { ()=&mut access=>return Err(Status::unauthenticated("access ended")),first=tokio::time::timeout(Duration::from_secs(5),input.message())=>first.map_err(|_|Status::deadline_exceeded("interaction open timeout"))?? };
        let open = match first.and_then(|frame| frame.frame) {
            Some(p::interact_request::Frame::Open(open)) => open,
            _ => {
                return Ok(Response::new(Box::pin(stream::once(async {
                    Ok(p::InteractResponse {
                        output: Some(p::OutputEvent {
                            event: Some(p::output_event::Event::Rejected(rejected(
                                Status::invalid_argument("first interaction frame must be open"),
                            ))),
                        }),
                    })
                })) as Self::InteractStream));
            }
        };
        let (entry, admission) = match self.open(principal.clone(), open, true).await {
            Ok(value) => value,
            Err(error) => {
                return Ok(Response::new(Box::pin(stream::once(async move {
                    Ok(p::InteractResponse {
                        output: Some(p::OutputEvent {
                            event: Some(p::output_event::Event::Rejected(rejected(error))),
                        }),
                    })
                })) as Self::InteractStream));
            }
        };
        let credentials = admission.credentials;
        self.attachments.native(entry, input).await?;
        let output = self.attachments.watch(
            &rpc::principal_id(&principal),
            p::WatchLiveOutputRequest { credentials },
        )?;
        Ok(Response::new(Box::pin(output.map(|response| {
            response.map(|response| p::InteractResponse {
                output: response.output,
            })
        })) as Self::InteractStream))
        }.instrument(operation.span.clone()).await;
        operation.span.record(
            "grpc_status",
            result
                .as_ref()
                .err()
                .map_or(0, |status| status.code() as i32),
        );
        operation.outcome(if result.is_ok() { "success" } else { "failure" });
        result
    }
    async fn begin_blob_upload(
        &self,
        request: Request<p::BeginBlobUploadRequest>,
    ) -> Result<Response<p::BeginBlobUploadResponse>, Status> {
        let principal = api::principal(&request)?;
        self.read
            .authorize(&principal, &request.get_ref().invocation_id)
            .await?;
        self.blobs
            .begin(&rpc::principal_id(&principal), request.into_inner())
            .await
            .map(Response::new)
    }
    async fn append_blob_upload(
        &self,
        request: Request<p::AppendBlobUploadRequest>,
    ) -> Result<Response<p::AppendBlobUploadResponse>, Status> {
        let principal = api::principal(&request)?;
        self.read
            .authorize(&principal, &request.get_ref().invocation_id)
            .await?;
        self.blobs
            .append(&rpc::principal_id(&principal), request.into_inner())
            .await
            .map(Response::new)
    }
    async fn commit_blob_upload(
        &self,
        request: Request<p::CommitBlobUploadRequest>,
    ) -> Result<Response<p::CommitBlobUploadResponse>, Status> {
        let principal = api::principal(&request)?;
        self.read
            .authorize(&principal, &request.get_ref().invocation_id)
            .await?;
        self.blobs
            .commit(&rpc::principal_id(&principal), request.into_inner())
            .await
            .map(Response::new)
    }
    async fn cancel_blob_upload(
        &self,
        request: Request<p::CancelBlobUploadRequest>,
    ) -> Result<Response<p::CancelBlobUploadResponse>, Status> {
        let principal = api::principal(&request)?;
        self.read
            .authorize(&principal, &request.get_ref().invocation_id)
            .await?;
        self.blobs
            .cancel_upload(&rpc::principal_id(&principal), request.into_inner())
            .await
            .map(Response::new)
    }
    async fn open_blob_read(
        &self,
        request: Request<p::OpenBlobReadRequest>,
    ) -> Result<Response<p::OpenBlobReadResponse>, Status> {
        let principal = api::principal(&request)?;
        self.read
            .authorize(&principal, &request.get_ref().invocation_id)
            .await?;
        self.blobs
            .open(
                &rpc::principal_id(&principal),
                principal.clone(),
                request.into_inner(),
            )
            .await
            .map(Response::new)
    }
    async fn read_blob(
        &self,
        request: Request<p::ReadBlobRequest>,
    ) -> Result<Response<Self::ReadBlobStream>, Status> {
        let principal = api::principal(&request)?;
        let id = request
            .get_ref()
            .credentials
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing reader credentials"))?
            .invocation_id
            .clone();
        self.read.authorize(&principal, &id).await?;
        self.blobs
            .read(&rpc::principal_id(&principal), request.into_inner())
            .map(Response::new)
    }
    async fn close_blob_read(
        &self,
        request: Request<p::CloseBlobReadRequest>,
    ) -> Result<Response<p::CloseBlobReadResponse>, Status> {
        let principal = api::principal(&request)?;
        let id = request
            .get_ref()
            .credentials
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing reader credentials"))?
            .invocation_id
            .clone();
        self.read.authorize(&principal, &id).await?;
        self.blobs
            .close(&rpc::principal_id(&principal), request.into_inner())
            .await
            .map(Response::new)
    }
}
fn rejected(error: Status) -> p::InteractionRejected {
    use tonic::Code;
    let (code, reason) = match error.code() {
        Code::InvalidArgument => (1, "invalid interaction"),
        Code::PermissionDenied | Code::Unauthenticated => (2, "interaction denied"),
        Code::Unimplemented => (3, "unsupported interaction"),
        Code::ResourceExhausted => (4, "interaction capacity unavailable"),
        Code::FailedPrecondition => (5, "stale or unsupported selection"),
        _ => (6, "interaction unavailable"),
    };
    p::InteractionRejected {
        code,
        reason: reason.into(),
    }
}
