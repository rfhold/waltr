//! Shared tonic fixture that never constructs generated `WordInfo` vectors.
#![allow(dead_code)] // Shared by standalone audio and server media test targets.
use futures_util::Stream;
use prost::{Message, bytes::BufMut};
use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tonic::{
    Request, Response, Status,
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    codegen::{Body, BoxFuture, Service, StdError, http},
};
use waltr_audio::{client::Client, config::Config, stt};

pub struct Server {
    pub endpoint: String,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
pub async fn start(frames: Vec<Vec<u8>>, wait_input: bool, fail_end: bool) -> (Client, Server) {
    start_inner(frames, wait_input, fail_end, false).await
}
pub async fn start_stalled(frames: Vec<Vec<u8>>, wait_input: bool) -> (Client, Server) {
    start_inner(frames, wait_input, false, true).await
}
async fn start_inner(
    frames: Vec<Vec<u8>>,
    wait_input: bool,
    fail_end: bool,
    stall_end: bool,
) -> (Client, Server) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(WireServer {
                frames: Arc::new(frames),
                wait_input,
                fail_end,
                stall_end,
            })
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });
    let mut config = Config::new(endpoint.clone());
    config.max_decoding_bytes = 8 * 1024 * 1024;
    config.queue_chunks = 1;
    let client = Client::connect(config).await.unwrap();
    (client, Server { endpoint, task })
}
#[derive(Clone)]
struct WireServer {
    frames: Arc<Vec<Vec<u8>>>,
    wait_input: bool,
    fail_end: bool,
    stall_end: bool,
}
impl tonic::server::NamedService for WireServer {
    const NAME: &'static str = "google.cloud.speech.v2.Speech";
}
impl<B> Service<http::Request<B>> for WireServer
where
    B: Body + Send + 'static,
    B::Error: Into<StdError> + Send + 'static,
{
    type Response = http::Response<tonic::body::Body>;
    type Error = std::convert::Infallible;
    type Future = BoxFuture<Self::Response, Self::Error>;
    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
    fn call(&mut self, request: http::Request<B>) -> Self::Future {
        let service = self.clone();
        Box::pin(async move {
            Ok(tonic::server::Grpc::new(WireCodec)
                .max_encoding_message_size(9 * 1024 * 1024)
                .streaming(service, request)
                .await)
        })
    }
}
type Frames = Pin<Box<dyn Stream<Item = Result<Vec<u8>, Status>> + Send>>;
impl tonic::server::StreamingService<stt::StreamingRecognizeRequest> for WireServer {
    type Response = Vec<u8>;
    type ResponseStream = Frames;
    type Future = BoxFuture<Response<Frames>, Status>;
    fn call(
        &mut self,
        request: Request<tonic::Streaming<stt::StreamingRecognizeRequest>>,
    ) -> Self::Future {
        let this = self.clone();
        Box::pin(async move {
            let mut input = request.into_inner();
            let (sender, receiver) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
                if this.wait_input {
                    while let Ok(Some(_)) = input.message().await {}
                }
                for frame in this.frames.iter() {
                    if sender.send(Ok(frame.clone())).await.is_err() {
                        return;
                    }
                }
                if this.fail_end {
                    let _sent = sender
                        .send(Err(Status::unavailable("fixture failure")))
                        .await;
                }
                if this.stall_end {
                    sender.closed().await;
                }
            });
            Ok(Response::new(Box::pin(futures_util::stream::unfold(
                receiver,
                |mut receiver| async move { receiver.recv().await.map(|item| (item, receiver)) },
            )) as Frames))
        })
    }
}
struct WireCodec;
struct RawEncoder;
struct RequestDecoder;
impl Encoder for RawEncoder {
    type Item = Vec<u8>;
    type Error = Status;
    fn encode(&mut self, bytes: Vec<u8>, buffer: &mut EncodeBuf<'_>) -> Result<(), Status> {
        buffer.put_slice(&bytes);
        Ok(())
    }
}
impl Decoder for RequestDecoder {
    type Item = stt::StreamingRecognizeRequest;
    type Error = Status;
    fn decode(&mut self, buffer: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Status> {
        Message::decode(buffer)
            .map(Some)
            .map_err(|_| Status::invalid_argument("fixture request"))
    }
}
impl Codec for WireCodec {
    type Encode = Vec<u8>;
    type Decode = stt::StreamingRecognizeRequest;
    type Encoder = RawEncoder;
    type Decoder = RequestDecoder;
    fn encoder(&mut self) -> RawEncoder {
        RawEncoder
    }
    fn decoder(&mut self) -> RequestDecoder {
        RequestDecoder
    }
}
pub fn varint(mut value: usize, output: &mut Vec<u8>) {
    while value >= 128 {
        output.push(u8::try_from(value & 127).unwrap() | 128);
        value >>= 7;
    }
    output.push(u8::try_from(value).unwrap());
}
pub fn field(tag: usize, bytes: &[u8], output: &mut Vec<u8>) {
    varint(tag * 8 + 2, output);
    varint(bytes.len(), output);
    output.extend_from_slice(bytes);
}
pub fn cumulative(words: usize, speaker: &str, final_portion: bool) -> Vec<u8> {
    cumulative_word(words, "a", speaker, final_portion)
}
pub fn cumulative_word(words: usize, token: &str, speaker: &str, final_portion: bool) -> Vec<u8> {
    let mut alternative = Vec::with_capacity(words * (speaker.len() + token.len() + 6));
    field(1, b"portion", &mut alternative);
    let mut word = Vec::new();
    field(3, token.as_bytes(), &mut word);
    field(6, speaker.as_bytes(), &mut word);
    for _ in 0..words {
        field(3, &word, &mut alternative);
    }
    let mut result = Vec::new();
    field(1, &alternative, &mut result);
    result.extend_from_slice(&[16, u8::from(final_portion), 40, 2]);
    let mut response = Vec::new();
    field(6, &result, &mut response);
    response
}
