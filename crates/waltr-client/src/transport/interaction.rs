use super::{RemoteTransport, status_error};
#[cfg(not(target_arch = "wasm32"))]
use crate::interaction::{InteractionInputHandle, InteractionRequests, InteractionSession};
use crate::{
    ClientError,
    interaction::{self, InteractionLimits, InteractionOutput, InteractionQueue, wire},
};
use futures_util::StreamExt;
#[cfg(not(target_arch = "wasm32"))]
use prost::Message;
use tonic::codegen::{Body, Bytes, StdError};

/// Credential-aware, caller-owned interaction RPCs, separate from snapshot/effect lanes.
/// Futures are local-capable. The caller polls input and output concurrently and owns cancellation.
pub trait RoutineInteractionTransport {
    /// Complete bounded upload for native and browser gRPC-web clients. Never retried.
    fn run_interaction(
        &self,
        request: wire::RunInteractionRequest,
        limits: InteractionLimits,
    ) -> impl Future<Output = Result<InteractionOutput, ClientError>>;
    /// Native full duplex. Observe Accepted on output before sending input.
    #[cfg(not(target_arch = "wasm32"))]
    fn interact(
        &self,
        open: wire::OpenInteraction,
        limits: InteractionLimits,
    ) -> impl Future<Output = Result<InteractionSession, ClientError>>;
}

#[allow(
    clippy::future_not_send,
    reason = "browser and local host RPC futures need not be Send"
)]
impl<T> RoutineInteractionTransport for T
where
    T: RemoteTransport,
    T::Inner: tonic::client::GrpcService<tonic::body::Body>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::Error: Into<StdError>,
    <T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody:
        Body<Data = Bytes> + Send + 'static,
    <<T::Inner as tonic::client::GrpcService<tonic::body::Body>>::ResponseBody as Body>::Error:
        Into<StdError> + Send,
{
    async fn run_interaction(
        &self,
        request: wire::RunInteractionRequest,
        limits: InteractionLimits,
    ) -> Result<InteractionOutput, ClientError> {
        interaction::validate_complete_interaction(&request, &limits)?;
        let credentials = self.request_credentials();
        let mut client = self
            .interaction_client(&credentials)
            .await?
            .max_encoding_message_size(limits.complete_body_bytes)
            .max_decoding_message_size(limits.output_message_bytes);
        let response = client
            .run(request)
            .await
            .map_err(|e| status_error(&e, &credentials))?;
        let stream = response
            .into_inner()
            .map(move |event| event.map_err(|e| status_error(&e, &credentials)));
        Ok(InteractionOutput::new(
            Box::pin(stream),
            InteractionQueue::new(),
            limits,
        ))
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn interact(
        &self,
        open: wire::OpenInteraction,
        limits: InteractionLimits,
    ) -> Result<InteractionSession, ClientError> {
        limits.validate()?;
        let request = wire::InteractRequest {
            event: Some(wire::interact_request::Event::Open(open)),
        };
        if request.encoded_len() > limits.input_bytes
            || request.encoded_len() > limits.output_message_bytes
        {
            return Err(ClientError::new(
                "interaction open exceeds input message limit",
            ));
        }
        let credentials = self.request_credentials();
        let mut client = self
            .interaction_client(&credentials)
            .await?
            .max_encoding_message_size(limits.output_message_bytes)
            .max_decoding_message_size(limits.output_message_bytes);
        let queue = InteractionQueue::new();
        queue.seed(request);
        // Own cancellation before awaiting headers; a dropped open must close the request lane.
        let input = InteractionInputHandle::new(queue.clone(), limits.clone());
        let response = client
            .interact(InteractionRequests(queue.clone()))
            .await
            .map_err(|e| status_error(&e, &credentials))?;
        let stream = response
            .into_inner()
            .map(move |event| event.map_err(|e| status_error(&e, &credentials)));
        Ok(InteractionSession {
            input,
            output: InteractionOutput::new(Box::pin(stream), queue, limits),
        })
    }
}
