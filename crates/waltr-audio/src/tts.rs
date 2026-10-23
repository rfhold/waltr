//! Google Text-to-Speech v1 messages and synthesis adapters.
use crate::telemetry::{Operation, Rpc};
use crate::{
    audio::{Container, SynthesizedAudio},
    client::{Client, Responses, duplex},
    error::Error,
};
use futures_core::Stream;
pub use waltr::grpc::google::cloud::texttospeech::v1::*;
pub use text_to_speech_client::TextToSpeechClient;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

/// Container implied by Google's unary encoding contract (not content sniffing).
#[must_use]
pub fn unary_container(encoding: i32) -> Container {
    match AudioEncoding::try_from(encoding) {
        Ok(AudioEncoding::Linear16 | AudioEncoding::Mulaw | AudioEncoding::Alaw) => Container::Wav,
        Ok(AudioEncoding::Pcm) => Container::RawPcm,
        _ => Container::Encoded(encoding),
    }
}
/// Container implied by Google's streaming encoding contract.
#[must_use]
pub fn streaming_container(encoding: i32) -> Container {
    match AudioEncoding::try_from(encoding) {
        Ok(AudioEncoding::Linear16 | AudioEncoding::Pcm) => Container::RawPcm,
        _ => Container::Encoded(encoding),
    }
}
impl Client {
    /// Synthesize complete text/SSML using upstream fields, without converting bytes.
    pub async fn synthesize_speech(
        &self,
        request: SynthesizeSpeechRequest,
        cancel: CancellationToken,
    ) -> Result<SynthesizedAudio, Error> {
        let container = unary_container(
            request
                .audio_config
                .as_ref()
                .ok_or(Error::Invalid("audio config required"))?
                .audio_encoding,
        );
        if request
            .input
            .as_ref()
            .and_then(|input| input.input_source.as_ref())
            .is_none()
        {
            return Err(Error::Invalid("synthesis input required"));
        }
        let mut request = tonic::Request::new(request);
        request.set_timeout(self.config.request_timeout);
        let operation = Operation::new(Rpc::Synthesize);
        operation.inject(&mut request);
        let result = self
            .unary(cancel, self.synthesis.clone().synthesize_speech(request))
            .instrument(operation.span.clone())
            .await;
        operation.result(&result);
        let response = result?;
        Ok(SynthesizedAudio {
            response,
            container,
        })
    }
    /// Send one configuration then incremental text/markup, returning raw upstream chunks.
    /// Use `streaming_container` with the configured encoding to interpret audio.
    pub fn streaming_synthesize<S>(
        &self,
        config: StreamingSynthesizeConfig,
        input: S,
        cancel: CancellationToken,
    ) -> Result<Responses<StreamingSynthesizeResponse>, Error>
    where
        S: Stream<Item = Result<StreamingSynthesisInput, Error>> + Send + 'static,
    {
        if config.streaming_audio_config.is_none() {
            return Err(Error::Invalid("streaming audio config required"));
        }
        let first = StreamingSynthesizeRequest {
            streaming_request: Some(
                streaming_synthesize_request::StreamingRequest::StreamingConfig(config),
            ),
        };
        let mut client = self.synthesis.clone();
        duplex(
            &self.config,
            Rpc::StreamingSynthesize,
            cancel,
            first,
            input,
            |input| {
                if input.input_source.is_none() {
                    return Err(Error::Invalid("streaming synthesis input required"));
                }
                Ok(StreamingSynthesizeRequest {
                    streaming_request: Some(streaming_synthesize_request::StreamingRequest::Input(
                        input,
                    )),
                })
            },
            move |request| async move { client.streaming_synthesize(request).await },
        )
    }
}
