//! Loopback-only HTTPS verification; no production trust overrides.
use waltr_audio::{client::Client, config::Config, error::Error, tts};
use std::time::Duration;
use tokio::{sync::oneshot, task::JoinHandle};
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity, Server, ServerTlsConfig};

// Public, test-only identity. Never use this key outside these loopback tests.
const CERT: &[u8] = include_bytes!("fixtures/tls/localhost-test-only.cert.pem");
const KEY: &[u8] = include_bytes!("fixtures/tls/localhost-test-only.key.pem");
const DEADLINE: Duration = Duration::from_secs(5);

struct UnusedSpeech;

#[tonic::async_trait]
impl tts::text_to_speech_server::TextToSpeech for UnusedSpeech {
    async fn list_voices(
        &self,
        _: tonic::Request<tts::ListVoicesRequest>,
    ) -> Result<tonic::Response<tts::ListVoicesResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("transport-only test"))
    }

    async fn synthesize_speech(
        &self,
        _: tonic::Request<tts::SynthesizeSpeechRequest>,
    ) -> Result<tonic::Response<tts::SynthesizeSpeechResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("transport-only test"))
    }

    type StreamingSynthesizeStream =
        tokio_stream::Empty<Result<tts::StreamingSynthesizeResponse, tonic::Status>>;

    async fn streaming_synthesize(
        &self,
        _: tonic::Request<tonic::Streaming<tts::StreamingSynthesizeRequest>>,
    ) -> Result<tonic::Response<Self::StreamingSynthesizeStream>, tonic::Status> {
        Err(tonic::Status::unimplemented("transport-only test"))
    }
}

async fn start(
    tls: bool,
) -> (
    u16,
    oneshot::Sender<()>,
    JoinHandle<Result<(), tonic::transport::Error>>,
) {
    // Binding before spawning makes readiness independent of scheduler timing.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut builder = Server::builder();
    if tls {
        builder = builder
            .tls_config(ServerTlsConfig::new().identity(Identity::from_pem(CERT, KEY)))
            .unwrap();
    }
    let (shutdown, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        builder
            .add_service(tts::text_to_speech_server::TextToSpeechServer::new(
                UnusedSpeech,
            ))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
    });
    (port, shutdown, task)
}

async fn stop(
    shutdown: oneshot::Sender<()>,
    mut task: JoinHandle<Result<(), tonic::transport::Error>>,
) {
    let _ = shutdown.send(());
    if let Ok(result) = tokio::time::timeout(DEADLINE, &mut task).await {
        result.unwrap().unwrap();
    } else {
        task.abort();
        drop(task.await);
        panic!("loopback server did not shut down before deadline");
    }
}

fn config(port: u16, scheme: &str) -> Config {
    let mut config = Config::new(format!("{scheme}://localhost:{port}"));
    config.connect_timeout = DEADLINE;
    config
}

#[tokio::test]
async fn https_rejects_self_signed_localhost_identity() {
    let (port, shutdown, task) = start(true).await;
    // This control validates the server, certificate dates, and localhost SAN.
    // Only the independent test client trusts the fixture; Config has no override.
    let control = Endpoint::from_shared(format!("https://localhost:{port}"))
        .unwrap()
        .tls_config(ClientTlsConfig::new().ca_certificate(Certificate::from_pem(CERT)))
        .unwrap();
    let trusted = tokio::time::timeout(DEADLINE, control.connect()).await;
    let control_connected = matches!(trusted, Ok(Ok(_)));
    drop(trusted);
    let rejected = tokio::time::timeout(DEADLINE, Client::connect(config(port, "https"))).await;
    let transport_rejected = matches!(rejected, Ok(Err(Error::Transport(_))));
    let outcome = format!("{rejected:?}");
    drop(rejected);
    stop(shutdown, task).await;

    assert!(
        control_connected,
        "fixture must pass date and hostname verification when trusted"
    );
    assert!(
        transport_rejected,
        "untrusted HTTPS must fail at transport setup: {outcome}"
    );
}

#[tokio::test]
async fn https_does_not_fall_back_to_plaintext() {
    let (port, shutdown, task) = start(false).await;
    let plaintext = tokio::time::timeout(DEADLINE, Client::connect(config(port, "http"))).await;
    let plaintext_connected = matches!(plaintext, Ok(Ok(_)));
    drop(plaintext);
    let rejected = tokio::time::timeout(DEADLINE, Client::connect(config(port, "https"))).await;
    let transport_rejected = matches!(rejected, Ok(Err(Error::Transport(_))));
    let outcome = format!("{rejected:?}");
    drop(rejected);
    stop(shutdown, task).await;

    assert!(
        plaintext_connected,
        "explicit HTTP must reach the plaintext control server"
    );
    assert!(
        transport_rejected,
        "HTTPS must not downgrade to the working plaintext transport: {outcome}"
    );
}
