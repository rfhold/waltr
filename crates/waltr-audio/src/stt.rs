//! Google Speech v2 messages and PCM adapters. Results retain upstream timing,
//! alternatives, final/interim state and request-local speaker attribution.
use crate::telemetry::{Operation, Rpc};
use crate::{
    audio::PcmS16Le,
    client::{Client, Responses, duplex},
    error::Error,
};
use futures_core::Stream;
use futures_util::{StreamExt, stream};
pub use waltr::grpc::google::cloud::speech::v2::*;
use prost::Message;
pub use speech_client::SpeechClient;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

const fn audio_request(bytes: Vec<u8>) -> StreamingRecognizeRequest {
    StreamingRecognizeRequest {
        recognizer: String::new(),
        streaming_request: Some(streaming_recognize_request::StreamingRequest::Audio(bytes)),
    }
}

const fn decoding(config: &mut RecognitionConfig, format: PcmS16Le) {
    config.decoding_config = Some(recognition_config::DecodingConfig::ExplicitDecodingConfig(
        ExplicitDecodingConfig {
            encoding: explicit_decoding_config::AudioEncoding::Linear16 as i32,
            sample_rate_hertz: format.rate(),
            audio_channel_count: format.channels(),
        },
    ));
}
impl Client {
    /// Recognize complete headerless PCM. Overrides decoding metadata to match bytes.
    #[allow(clippy::too_many_arguments)] // Explicit resolved recognition inputs.
    pub async fn recognize(
        &self,
        recognizer: String,
        mut config: RecognitionConfig,
        format: PcmS16Le,
        audio: Vec<u8>,
        cancel: CancellationToken,
    ) -> Result<RecognizeResponse, Error> {
        format.validate(&audio)?;
        decoding(&mut config, format);
        let mut request = tonic::Request::new(RecognizeRequest {
            recognizer,
            config: Some(config),
            audio_source: Some(recognize_request::AudioSource::Content(audio)),
            ..Default::default()
        });
        request.set_timeout(self.config.request_timeout);
        let operation = Operation::new(Rpc::Recognize);
        operation.inject(&mut request);
        let result = self
            .unary(cancel, self.speech.clone().recognize(request))
            .instrument(operation.span.clone())
            .await;
        operation.result(&result);
        result
    }
    /// Recognize incremental PCM; sends exactly one configuration before audio.
    /// Caller chunks must contain full interleaved frames and fit the configured
    /// queue/encoding byte limits. They are split lazily into frame-aligned audio
    /// messages of at most 15,360 bytes. Decoding is set from
    /// `format` and added to any explicit field mask so recognizer defaults cannot
    /// silently override the supplied PCM format.
    #[allow(clippy::too_many_arguments)] // Matches unary recognition inputs.
    pub fn streaming_recognize<S>(
        &self,
        recognizer: String,
        mut config: StreamingRecognitionConfig,
        format: PcmS16Le,
        input: S,
        cancel: CancellationToken,
    ) -> Result<Responses<StreamingRecognizeResponse>, Error>
    where
        S: Stream<Item = Result<Vec<u8>, Error>> + Send + 'static,
    {
        decoding(config.config.get_or_insert_default(), format);
        if let Some(mask) = &mut config.config_mask
            && !mask
                .paths
                .iter()
                .any(|path| path == "*" || path == "explicit_decoding_config")
        {
            mask.paths.push("explicit_decoding_config".into());
        }
        let first = StreamingRecognizeRequest {
            recognizer,
            streaming_request: Some(
                streaming_recognize_request::StreamingRequest::StreamingConfig(config),
            ),
        };
        let frame_bytes = usize::try_from(format.channels())
            .ok()
            .and_then(|channels| channels.checked_mul(2))
            .filter(|bytes| *bytes <= 15_360)
            .ok_or(Error::Invalid("PCM frame exceeds streaming audio limit"))?;
        let chunk_bytes = 15_360 / frame_bytes * frame_bytes;
        let max_bytes = self.config.queue_bytes.min(self.config.max_encoding_bytes);
        let input = stream::try_unfold(
            (Box::pin(input), None::<(Vec<u8>, usize)>),
            move |(mut input, pending)| async move {
                let (bytes, offset) = if let Some(pending) = pending {
                    pending
                } else {
                    let Some(bytes) = input.next().await else {
                        return Ok(None);
                    };
                    let bytes = bytes?;
                    format.validate(&bytes)?;
                    let request = audio_request(bytes);
                    // Retain the caller-chunk admission bound before making fragments.
                    if request.encoded_len() > max_bytes {
                        return Err(Error::Invalid(
                            "input chunk exceeds queue or encoding limit",
                        ));
                    }
                    let Some(streaming_recognize_request::StreamingRequest::Audio(bytes)) =
                        request.streaming_request
                    else {
                        unreachable!()
                    };
                    (bytes, 0)
                };
                if offset == 0 && bytes.len() <= chunk_bytes {
                    return Ok(Some((bytes, (input, None))));
                }
                let end = offset.saturating_add(chunk_bytes).min(bytes.len());
                let chunk = bytes[offset..end].to_vec();
                let pending = (end < bytes.len()).then_some((bytes, end));
                Ok(Some((chunk, (input, pending))))
            },
        );
        let mut client = self.speech.clone();
        duplex(
            &self.config,
            Rpc::StreamingRecognize,
            cancel,
            first,
            input,
            |bytes| Ok(audio_request(bytes)),
            move |request| async move { client.streaming_recognize(request).await },
        )
    }
}
