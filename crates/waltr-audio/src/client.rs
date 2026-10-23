//! Reusable transport and bounded, owned duplex operations.
use crate::telemetry::{Operation, Rpc};
use crate::{config::Config, error::Error, stt, tts};
use futures_core::Stream;
use futures_util::StreamExt;
use prost::Message;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tracing::Instrument as _;

/// Reusable channel and generated clients. Clones share the connection, not requests.
#[derive(Clone, Debug)]
pub struct Client {
    pub(crate) config: Config,
    pub(crate) speech: stt::SpeechClient<Channel>,
    pub(crate) synthesis: tts::TextToSpeechClient<Channel>,
}
impl Client {
    /// Connect to the explicit endpoint. HTTPS verifies hostname and public roots.
    /// No credentials, discovery, or automatic observable-request retries are added.
    pub async fn connect(config: Config) -> Result<Self, Error> {
        config.validate()?;
        let mut endpoint = Endpoint::from_shared(config.uri.clone()).map_err(Error::Transport)?;
        match endpoint.uri().scheme_str() {
            Some("https") => {
                endpoint = endpoint
                    .tls_config(ClientTlsConfig::new().with_webpki_roots())
                    .map_err(Error::Transport)?;
            }
            Some("http") => {}
            _ => return Err(Error::Invalid("explicit http or https URI required")),
        }
        let channel = endpoint
            .connect_timeout(config.connect_timeout)
            .connect()
            .await
            .map_err(Error::Transport)?;
        Ok(Self {
            speech: stt::SpeechClient::new(channel.clone())
                .max_encoding_message_size(config.max_encoding_bytes)
                .max_decoding_message_size(config.max_decoding_bytes),
            synthesis: tts::TextToSpeechClient::new(channel)
                .max_encoding_message_size(config.max_encoding_bytes)
                .max_decoding_message_size(config.max_decoding_bytes),
            config,
        })
    }
    pub(crate) async fn unary<T>(
        &self,
        cancel: CancellationToken,
        future: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
    ) -> Result<T, Error> {
        tokio::select! { biased;
            () = cancel.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(self.config.request_timeout, future) => result.map_or_else(|_| Err(Error::Deadline), |result| result.map(tonic::Response::into_inner).map_err(Error::from))
        }
    }
}

/// Response stream owns both worker tasks. Dropping it aborts both directions.
///
/// Explicit cancellation yields `Error::Cancelled`; normal input EOF allows final responses.
/// Already queued results are drained before the terminal error. Cancellation and deadlines
/// stop transport and producer even when the output queue is full. Each operation owns
/// at most two tasks; neither survives response drop. The supplied token's parent is never
/// cancelled by this stream.
#[derive(Debug)]
pub struct Responses<T> {
    receiver: mpsc::Receiver<Result<T, Error>>,
    tasks: Vec<tokio::task::AbortHandle>,
    cancel: CancellationToken,
    terminal: oneshot::Receiver<Error>,
    terminal_done: bool,
}
impl<T> Stream for Responses<T> {
    type Item = Result<T, Error>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.receiver.poll_recv(cx) {
            Poll::Ready(None) if !self.terminal_done => {
                let result = std::task::ready!(Pin::new(&mut self.terminal).poll(cx));
                self.terminal_done = true;
                Poll::Ready(result.ok().map(Err))
            }
            result => result,
        }
    }
}
impl<T> Drop for Responses<T> {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}
struct AbortOnDrop(tokio::task::AbortHandle);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[allow(clippy::too_many_arguments, clippy::needless_pass_by_value)] // Internal typed bridge shared by both upstream duplex RPCs.
pub(crate) fn duplex<I, Q, R, S, F, Fut>(
    config: &Config,
    rpc: Rpc,
    cancel: CancellationToken,
    first: Q,
    input: S,
    mut convert: impl FnMut(I) -> Result<Q, Error> + Send + 'static,
    call: F,
) -> Result<Responses<R>, Error>
where
    I: Send + 'static,
    Q: Message + Send + Unpin + 'static,
    R: Send + 'static + Message + Default,
    S: Stream<Item = Result<I, Error>> + Send + 'static,
    F: FnOnce(tonic::Request<RequestStream<Q>>) -> Fut + Send + 'static,
    Fut: Future<Output = Result<tonic::Response<tonic::Streaming<R>>, tonic::Status>>
        + Send
        + 'static,
{
    if first.encoded_len() > config.max_encoding_bytes {
        return Err(Error::Invalid("configuration exceeds encoding limit"));
    }
    let operation = Operation::new(rpc);
    let operation_span = operation.span.clone();
    let operation_cancel = cancel.child_token();
    let producer_cancel = operation_cancel.clone();
    let (sender, receiver) = mpsc::channel(config.queue_chunks);
    let (errors, mut error_rx) = oneshot::channel();
    let budget = Arc::new(Semaphore::new(config.queue_bytes));
    let max_bytes = config.queue_bytes.min(config.max_encoding_bytes);
    let producer_span = operation_span.clone();
    let producer = tokio::spawn(
        async move {
            let work = async move {
                let mut input = Box::pin(input);
                while let Some(item) = input.next().await {
                    let request = convert(item?)?;
                    let size = request.encoded_len();
                    if size > max_bytes {
                        return Err(Error::Invalid(
                            "input chunk exceeds queue or encoding limit",
                        ));
                    }
                    let permits = budget
                        .clone()
                        .acquire_many_owned(u32::try_from(size).expect("validated byte bound"))
                        .await
                        .expect("private semaphore remains open");
                    if sender.send((request, permits)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            };
            tokio::select! { biased;
                () = producer_cancel.cancelled() => {},
                result = work => { if let Err(error) = result { let _sent = errors.send(error); } }
            }
        }
        .instrument(producer_span),
    );
    let producer_abort = producer.abort_handle();
    let guard = AbortOnDrop(producer_abort.clone());
    let mut request = tonic::Request::new(RequestStream {
        first: Some(first),
        receiver,
    });
    request.set_timeout(config.request_timeout);
    operation.inject(&mut request);
    let (out, responses) = mpsc::channel(config.queue_chunks);
    let (terminal_sender, terminal) = oneshot::channel();
    let deadline = tokio::time::sleep(config.request_timeout);
    let driver_cancel = operation_cancel.clone();
    let driver = tokio::spawn(
        async move {
            let guard = guard;
            let work = async {
                let mut upstream = call(request).await.map_err(Error::from)?.into_inner();
                while let Some(response) = upstream.message().await.map_err(Error::from)? {
                    if out.send(Ok(response)).await.is_err() {
                        return Ok(());
                    }
                }
                Ok(())
            };
            let result = tokio::select! { biased;
                () = driver_cancel.cancelled() => Err(Error::Cancelled),
                () = deadline => Err(Error::Deadline),
                Ok(error) = &mut error_rx => Err(error),
                result = work => result,
            };
            if out.is_closed() && result.is_ok() {
                operation.result::<()>(&Err(Error::Cancelled));
            } else {
                operation.result(&result);
            }
            if let Err(error) = result {
                guard.0.abort();
                let _sent = terminal_sender.send(error);
            }
        }
        .instrument(operation_span),
    );
    Ok(Responses {
        receiver: responses,
        tasks: vec![producer_abort, driver.abort_handle()],
        cancel: operation_cancel,
        terminal,
        terminal_done: false,
    })
}

pub(crate) struct RequestStream<Q> {
    first: Option<Q>,
    receiver: mpsc::Receiver<(Q, tokio::sync::OwnedSemaphorePermit)>,
}
impl<Q: Unpin> Stream for RequestStream<Q> {
    type Item = Q;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Q>> {
        if let Some(first) = self.first.take() {
            return Poll::Ready(Some(first));
        }
        self.receiver
            .poll_recv(cx)
            .map(|item| item.map(|(request, _permit)| request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Dropped(Option<oneshot::Sender<()>>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _sent = sender.send(());
            }
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_a_full_byte_budget_without_polling_transport() {
        let mut config = Config::new("http://unused");
        config.queue_chunks = 1;
        config.queue_bytes = 6;
        let cancel = CancellationToken::new();
        let (full_sender, full) = oneshot::channel();
        let (drop_sender, dropped) = oneshot::channel();
        let guard = Dropped(Some(drop_sender));
        let mut signal = Some(full_sender);
        let mut seen = 0;
        let input = futures_util::stream::repeat_with(move || {
            let _keep = &guard;
            seen += 1;
            if seen == 2 {
                let _sent = signal.take().unwrap().send(());
            }
            Ok(vec![0, 0, 0, 0])
        });
        let mut output: Responses<stt::StreamingRecognizeResponse> = duplex(
            &config,
            Rpc::StreamingRecognize,
            cancel.clone(),
            stt::StreamingRecognizeRequest::default(),
            input,
            |bytes| {
                Ok(stt::StreamingRecognizeRequest {
                    streaming_request: Some(
                        stt::streaming_recognize_request::StreamingRequest::Audio(bytes),
                    ),
                    ..Default::default()
                })
            },
            |request| async move {
                let _request = request;
                std::future::pending().await
            },
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(3), full)
            .await
            .unwrap()
            .unwrap();
        cancel.cancel();
        assert!(matches!(output.next().await, Some(Err(Error::Cancelled))));
        tokio::time::timeout(Duration::from_secs(3), dropped)
            .await
            .unwrap()
            .unwrap();
        assert!(output.next().await.is_none());
    }
}
