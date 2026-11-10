use super::{RemoteTransport, RequestCredentials, status_error};
use crate::interaction::split::*;
#[cfg(not(target_arch = "wasm32"))]
use crate::interaction::{
    InteractionInputHandle, InteractionOutput, InteractionQueue, InteractionRequests,
    InteractionSession,
};
use crate::transport::routine::invalid;
use crate::{
    ClientError,
    interaction::{AttachmentSession, InteractionLimits, MediaTransfers, wire},
};
use futures_util::{FutureExt, StreamExt};
use std::sync::Arc;
use tonic::codegen::{Body, Bytes, StdError};

async fn guarded<F: Future<Output = Result<R, ClientError>>, R>(
    c: RequestCredentials,
    future: F,
) -> Result<R, ClientError> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let waker = c.media_waker();
        futures_util::pin_mut!(future);
        futures_util::future::poll_fn(|cx| {
            waker.register(cx.waker());
            if !c.is_current() {
                return std::task::Poll::Ready(Err(ClientError::new(
                    "Routine credential generation changed",
                )));
            }
            future.as_mut().poll(cx)
        })
        .await
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _credentials = c;
        future.await
    }
}
fn guarded_stream<R: Send + 'static>(
    c: RequestCredentials,
    mut source: crate::interaction::ProtocolStream<R>,
) -> crate::interaction::ProtocolStream<R> {
    #[cfg(not(target_arch = "wasm32"))]
    let waker = c.media_waker();
    let mut ended = false;
    Box::pin(futures_util::stream::poll_fn(move |cx| {
        if ended {
            return std::task::Poll::Ready(None);
        }
        #[cfg(not(target_arch = "wasm32"))]
        waker.register(cx.waker());
        if !c.is_current() {
            ended = true;
            return std::task::Poll::Ready(Some(Err(ClientError::new(
                "Routine credential generation changed",
            ))));
        }
        source.as_mut().poll_next(cx)
    }))
}

#[cfg(not(target_arch = "wasm32"))]
fn native_output(
    mut source: crate::interaction::ProtocolStream<wire::OutputEvent>,
    queue: Arc<InteractionQueue>,
    cancel_rpc: impl FnOnce(String) -> futures_util::future::BoxFuture<'static, Result<(), ClientError>>
    + Send
    + 'static,
) -> crate::interaction::ProtocolStream<wire::OutputEvent> {
    let mut cancel_rpc = Some(cancel_rpc);
    let mut cancellation = None;
    let mut source_done = false;
    Box::pin(futures_util::stream::poll_fn(move |cx| {
        if let Some(invocation) = queue.take_native_cancel() {
            cancellation = Some(cancel_rpc.take().unwrap()(invocation));
        }
        if let Some(future) = &mut cancellation {
            if let std::task::Poll::Ready(result) = future.as_mut().poll(cx) {
                cancellation = None;
                if let Err(error) = result {
                    return std::task::Poll::Ready(Some(Err(error)));
                }
            }
        }
        // Drain output concurrently: cancellation acknowledgement is not cleanup proof.
        if !source_done {
            match source.as_mut().poll_next(cx) {
                std::task::Poll::Ready(None) => source_done = true,
                other => return other,
            }
        }
        if cancellation.is_some() {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(None)
        }
    }))
}

/// Caller-owned native media and browser-compatible split lanes.
pub trait RoutineInteractionTransport {
    /// Admit finite declarations and open exactly one output subscription.
    fn open_attachment(
        &self,
        open: wire::InteractionOpen,
        limits: InteractionLimits,
    ) -> impl Future<Output = Result<AttachmentSession, ClientError>>;
    /// Create generation-fenced sequential blob and text transfers.
    fn media_transfers(&self) -> MediaTransfers;
    #[cfg(not(target_arch = "wasm32"))]
    /// Open native bidirectional media without a background client task.
    fn interact(
        &self,
        open: wire::InteractionOpen,
        limits: InteractionLimits,
    ) -> impl Future<Output = Result<InteractionSession, ClientError>> + Send;
}
pub(super) fn media_error(e: &tonic::Status, c: &RequestCredentials) -> ClientError {
    let classified = status_error(e, c);
    if classified.is_waltr_login_required() {
        ClientError::waltr_login_required("Routine access lost")
    } else {
        ClientError::new(format!("Routine RPC {:?}", e.code()))
    }
}
#[allow(clippy::future_not_send)]
impl RoutineInteractionTransport for super::DefaultGrpcTransport {
    async fn open_attachment(
        &self,
        open: wire::InteractionOpen,
        limits: InteractionLimits,
    ) -> Result<AttachmentSession, ClientError> {
        let (rpc, stream) = callbacks(self.clone());
        AttachmentSession::open(open, limits, rpc, stream).await
    }
    fn media_transfers(&self) -> MediaTransfers {
        let (rpc, stream) = callbacks(self.clone());
        MediaTransfers { rpc, stream }
    }
    #[cfg(not(target_arch = "wasm32"))]
    async fn interact(
        &self,
        open: wire::InteractionOpen,
        limits: InteractionLimits,
    ) -> Result<InteractionSession, ClientError> {
        limits.validate_open(&open)?;
        let credentials = self.request_credentials();
        let mut client = self
            .routine_client(&credentials)
            .await?
            .max_encoding_message_size(limits.output_message_bytes)
            .max_decoding_message_size(limits.output_message_bytes);
        let queue = InteractionQueue::new();
        credentials.register_media_queue(&queue);
        queue.seed(wire::InteractRequest {
            frame: Some(wire::interact_request::Frame::Open(open.clone())),
        });
        let input = InteractionInputHandle::new(queue.clone(), limits.clone(), &open);
        let r = guarded(credentials.clone(), async {
            client
                .interact(InteractionRequests(queue.clone()))
                .await
                .map_err(|e| media_error(&e, &credentials))
        })
        .await?;
        let cancellation_transport = self.clone();
        let cancellation_generation = credentials.clone();
        let guard = credentials.clone();
        let source = r.into_inner().map(move |r| {
            if !credentials.is_current() {
                return Err(ClientError::new("Routine credential generation changed"));
            }
            r.map_err(|e| media_error(&e, &credentials))
                .and_then(|r| r.output.ok_or_else(invalid))
        });
        let source = native_output(Box::pin(source), queue.clone(), move |invocation_id| {
            async move {
                let c = cancellation_transport.request_credentials();
                if !c.same_generation(&cancellation_generation) {
                    return Err(ClientError::new("Routine credential generation changed"));
                }
                guarded(c.clone(), async {
                    let mut client = cancellation_transport.routine_client(&c).await?;
                    let response = client
                        .cancel_invocation(wire::CancelInvocationRequest { invocation_id })
                        .await
                        .map_err(|error| media_error(&error, &c))?
                        .into_inner();
                    crate::transport::routine::invocation_status(response.status)?;
                    Ok(())
                })
                .await
            }
            .boxed()
        });
        Ok(InteractionSession {
            input,
            output: InteractionOutput::new(guarded_stream(guard, source), queue, limits),
        })
    }
}

fn callbacks<T>(transport: T) -> (MediaRpc, MediaStreamRpc)
where
    T: RemoteTransport + Clone + 'static,
    T::Inner: tonic::client::GrpcService<tonic::body::Body> + 'static,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    let generation = transport.request_credentials();
    let t = transport.clone();
    let g = generation.clone();
    let rpc: MediaRpc = Arc::new(move |request| {
        let t = t.clone();
        let g = g.clone();
        async move {
            let c = t.request_credentials();
            if !g.same_generation(&c) {
                return Err(ClientError::new("Routine credential generation changed"));
            }
            let mut client = t
                .routine_client(&c)
                .await?
                .max_encoding_message_size(2 * 1024 * 1024)
                .max_decoding_message_size(2 * 1024 * 1024);
            macro_rules! call {
                ($method:ident,$request:expr,$variant:ident) => {
                    MediaResponse::$variant(
                        client
                            .$method($request)
                            .await
                            .map_err(|e| media_error(&e, &c))?
                            .into_inner(),
                    )
                };
            }
            guarded(c.clone(), async {
                let response = match request {
                    MediaRequest::Open(r) => MediaResponse::Open(Box::new(
                        client
                            .open_attachment(r)
                            .await
                            .map_err(|e| media_error(&e, &c))?
                            .into_inner(),
                    )),
                    MediaRequest::Batch(r) => call!(send_input_batch, r, Batch),
                    MediaRequest::Renew(r) => call!(renew_attachment, r, Renew),
                    MediaRequest::Close(r) => call!(close_attachment, r, Close),
                    MediaRequest::Begin(r) => call!(begin_blob_upload, r, Begin),
                    MediaRequest::Append(r) => call!(append_blob_upload, r, Append),
                    MediaRequest::Commit(r) => call!(commit_blob_upload, r, Commit),
                    MediaRequest::OpenRead(r) => call!(open_blob_read, r, OpenRead),
                    MediaRequest::Text(r) => call!(read_text_result, r, Text),
                    MediaRequest::CancelUpload(r) => {
                        client
                            .cancel_blob_upload(r)
                            .await
                            .map_err(|e| media_error(&e, &c))?;
                        MediaResponse::Unit
                    }
                    MediaRequest::CloseRead(r) => {
                        client
                            .close_blob_read(r)
                            .await
                            .map_err(|e| media_error(&e, &c))?;
                        MediaResponse::Unit
                    }
                };
                if !g.is_current() {
                    return Err(ClientError::new("Routine credential generation changed"));
                }
                Ok(response)
            })
            .await
        }
        .boxed_local()
    });
    let stream: MediaStreamRpc = Arc::new(move |request| {
        let t = transport.clone();
        let g = generation.clone();
        async move {
            let c = t.request_credentials();
            if !g.same_generation(&c) {
                return Err(ClientError::new("Routine credential generation changed"));
            }
            let mut client = t
                .routine_client(&c)
                .await?
                .max_encoding_message_size(131072)
                .max_decoding_message_size(2 * 1024 * 1024);
            let guard = c.clone();
            let source: MediaStream = guarded(c.clone(), async {
                let source: MediaStream = match request {
                    MediaStreamRequest::Output(r) => {
                        let r = client
                            .watch_live_output(r)
                            .await
                            .map_err(|e| media_error(&e, &c))?;
                        Box::pin(r.into_inner().map(move |r| {
                            if !g.is_current() {
                                return Err(ClientError::new(
                                    "Routine credential generation changed",
                                ));
                            }
                            r.map_err(|e| media_error(&e, &c)).and_then(|r| {
                                r.output
                                    .map(|o| MediaStreamResponse::Output(Box::new(o)))
                                    .ok_or_else(invalid)
                            })
                        }))
                    }
                    MediaStreamRequest::Blob(r) => {
                        let r = client.read_blob(r).await.map_err(|e| media_error(&e, &c))?;
                        Box::pin(r.into_inner().map(move |r| {
                            if !g.is_current() {
                                return Err(ClientError::new(
                                    "Routine credential generation changed",
                                ));
                            }
                            r.map(MediaStreamResponse::Blob)
                                .map_err(|e| media_error(&e, &c))
                        }))
                    }
                };
                Ok(source)
            })
            .await?;
            Ok(guarded_stream(guard, source))
        }
        .boxed_local()
    });
    (rpc, stream)
}
