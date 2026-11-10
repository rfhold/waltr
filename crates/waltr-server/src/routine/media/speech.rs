//! Explicit pinned Google adapters, with no graph, credentials, retry, or conversion.
use super::{
    Error, Producer,
    stream::{Chunk, Direction, Format, Reader, Text, Writer},
};
use crate::resource_catalog::speech::{ResolvedSpeechModel, SpeechSettings};
use futures_util::{StreamExt, stream};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio_util::sync::CancellationToken;
use waltr_audio::{audio::PcmS16Le, client::Client, config::Config, stt, tts};

/// Explicit per-call choices, without endpoints, provider keys or credentials.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SynthesisSelection {
    /// Exact pinned synthesis model key.
    pub model: String,
    /// Supported provider voice identity.
    pub voice: String,
    /// Provider language code.
    pub language: String,
    /// Finite speaking rate in the supported range.
    pub rate: f64,
}
impl SynthesisSelection {
    pub(crate) fn validate(&self) -> Result<(), Error> {
        if self.model.is_empty() || self.model.len() > 256 || self.voice.is_empty() || self.voice.len() > 256
            || self.language.is_empty() || self.language.len() > 64 || !self.rate.is_finite() || !(0.25..=2.0).contains(&self.rate) {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

/// Object-safe producer boundary. Construction pins network-free resolved settings;
/// execution uses the input invocation's existing absolute deadline.
pub trait SpeechBackend: Send + Sync {
    /// Validate explicit choices without consuming streams or connecting.
    fn preflight_synthesis(&self, _selection: &SynthesisSelection) -> Result<(), Error> { Err(Error::Invalid) }
    /// One call's choices never mutate a shared backend.
    fn synthesize_selected(&self, input: Reader, output: Writer, selection: SynthesisSelection) -> Result<Producer, Error> {
        self.preflight_synthesis(&selection)?;
        drop((input, output));
        Err(Error::Invalid)
    }
    /// Check pinned direction and format without opening a provider connection.
    fn preflight(&self, _input: Format, _snapshot: bool) -> Result<(), Error> {
        Err(Error::Invalid)
    }
    /// Recognize typed input PCM into a typed output revision stream.
    fn transcribe(&self, input: Reader, output: Writer) -> Result<Producer, Error>;
    /// Large diarization revisions as host-owned, chunk-readable snapshots. The
    /// host supplies one shared additional workspace per invocation.
    fn transcribe_snapshot(
        &self,
        input: Reader,
        workspace: super::snapshot::Workspace,
    ) -> Result<(Producer, super::snapshot::SnapshotReader), Error>;
    /// Synthesize committed input text into raw 24 kHz mono PCM output.
    fn synthesize(&self, input: Reader, output: Writer) -> Result<Producer, Error>;
}
/// Bounded adapters with explicitly pinned endpoint and speech settings.
/// Ordinary stream frames remain capped at 16 KiB provider messages. The explicit
/// snapshot path admits compact wire frames up to 8 MiB and formatted notes to 1 MiB.
#[derive(Clone, Debug)]
pub struct GoogleSpeech {
    recognition: Option<ResolvedSpeechModel>,
    recognition_snapshot: Option<ResolvedSpeechModel>,
    synthesis: Option<ResolvedSpeechModel>,
    synthesis_choices: Vec<SynthesisSelection>,
}
impl GoogleSpeech {
    /// Validate settings without connecting; at least one speech direction is required.
    pub fn new(
        recognition: Option<ResolvedSpeechModel>,
        synthesis: Option<ResolvedSpeechModel>,
    ) -> Result<Self, Error> {
        Self::with_recognition_selections(recognition, None, synthesis)
    }
    /// Construct independently pinned ordinary, snapshot and synthesis directions.
    pub fn with_recognition_selections(
        recognition: Option<ResolvedSpeechModel>,
        recognition_snapshot: Option<ResolvedSpeechModel>,
        synthesis: Option<ResolvedSpeechModel>,
    ) -> Result<Self, Error> {
        if recognition.is_none() && recognition_snapshot.is_none() && synthesis.is_none() {
            return Err(Error::Invalid);
        }
        if recognition_snapshot.as_ref().is_some_and(|resolved| {
            !matches!(&resolved.settings, SpeechSettings::Recognition(options) if options.diarization)
        }) {
            return Err(Error::Invalid);
        }
        for resolved in recognition
            .iter()
            .chain(recognition_snapshot.iter())
            .chain(synthesis.iter())
        {
            if resolved.endpoint.len() > 4096
                || resolved.provider_key.len() > 256
                || resolved.model_key.len() > 256
            {
                return Err(Error::Limit);
            }
            let endpoint = url::Url::parse(&resolved.endpoint).map_err(|_| Error::Invalid)?;
            if !matches!(endpoint.scheme(), "http" | "https")
                || endpoint.host_str().is_none()
                || !endpoint.username().is_empty()
                || endpoint.password().is_some()
                || endpoint.query().is_some()
                || endpoint.fragment().is_some()
                || !resolved.capabilities.streaming
            {
                return Err(Error::Invalid);
            }
        }
        for resolved in recognition.iter().chain(recognition_snapshot.iter()) {
            let SpeechSettings::Recognition(options) = &resolved.settings else {
                return Err(Error::Invalid);
            };
            Format::Pcm16Le {
                rate: options.sample_rate_hz,
                channels: u16::try_from(options.channels).map_err(|_| Error::Invalid)?,
            }
            .validate()?;
            if options.recognizer.is_empty()
                || options.recognizer.len() > 1024
                || options.model.is_empty()
                || options.model.len() > 256
                || options.language_codes.is_empty()
                || options.language_codes.len() > 3
                || options
                    .language_codes
                    .iter()
                    .any(|code| code.is_empty() || code.len() > 64)
            {
                return Err(Error::Invalid);
            }
        }
        if let Some(resolved) = &synthesis {
            let SpeechSettings::Synthesis(options) = &resolved.settings else {
                return Err(Error::Invalid);
            };
            if options.sample_rate_hz != 24000
                || options.channels != 1
                || options.pitch.is_some_and(|v| v != 0.0)
                || options.voice.is_empty()
                || options.voice.len() > 256
                || options.language_code.is_empty()
                || options.language_code.len() > 64
                || options
                    .speaking_rate
                    .is_some_and(|v| !v.is_finite() || !(0.25..=2.0).contains(&v))
            {
                return Err(Error::Invalid);
            }
        }
        let synthesis_choices = synthesis.as_ref().and_then(|resolved| match &resolved.settings {
            SpeechSettings::Synthesis(options) => Some(SynthesisSelection { model:resolved.model_key.clone(), voice:options.voice.clone(), language:options.language_code.clone(), rate:options.speaking_rate.unwrap_or(1.0) }),
            _ => None,
        }).into_iter().collect();
        Ok(Self {
            recognition,
            recognition_snapshot,
            synthesis,
            synthesis_choices,
        })
    }
    fn snapshot_recognition(&self) -> Option<&ResolvedSpeechModel> {
        self.recognition_snapshot
            .as_ref()
            .or(self.recognition.as_ref())
    }
    /// Replace allowed choices under the same host-pinned provider/model identity.
    pub fn with_synthesis_choices(mut self, choices: Vec<SynthesisSelection>) -> Result<Self, Error> {
        if choices.len() > 128 { return Err(Error::Limit); }
        for choice in &choices {
            choice.validate()?;
            if self.synthesis.as_ref().is_none_or(|resolved| resolved.model_key != choice.model) { return Err(Error::Invalid); }
        }
        if !choices.is_empty() { self.synthesis_choices = choices; }
        Ok(self)
    }
    fn selected_synthesis(&self, selection: &SynthesisSelection) -> Result<Self, Error> {
        self.preflight_synthesis(selection)?;
        let mut selected = self.clone();
        let SpeechSettings::Synthesis(options) = &mut selected.synthesis.as_mut().ok_or(Error::Invalid)?.settings else { return Err(Error::Invalid); };
        options.voice = selection.voice.clone();
        options.language_code = selection.language.clone();
        options.speaking_rate = Some(selection.rate);
        Ok(selected)
    }
}
#[derive(Default)]
struct InputStatus {
    closed: AtomicBool,
    error: Mutex<Option<Error>>,
}
impl InputStatus {
    fn fail(&self, error: Error) -> waltr_audio::error::Error {
        *self.error.lock().expect("private speech status") = Some(error);
        waltr_audio::error::Error::Invalid("typed input failed")
    }
    fn finish(&self) -> Result<(), Error> {
        if let Some(error) = *self.error.lock().map_err(|_| Error::Worker)? {
            return Err(error);
        }
        if !self.closed.load(Ordering::Acquire) {
            return Err(Error::BrokenProducer);
        }
        Ok(())
    }
    fn error(&self, error: waltr_audio::error::Error) -> Error {
        self.error
            .lock()
            .ok()
            .and_then(|s| *s)
            .unwrap_or(match error {
                waltr_audio::error::Error::Cancelled => Error::Cancelled,
                waltr_audio::error::Error::Deadline => Error::Deadline,
                _ => Error::Provider,
            })
    }
}
async fn connect(
    endpoint: String,
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
) -> Result<Client, Error> {
    connect_bounded(endpoint, deadline, cancel, 16384).await
}
async fn connect_bounded(
    endpoint: String,
    deadline: tokio::time::Instant,
    cancel: &CancellationToken,
    frame_limit: usize,
) -> Result<Client, Error> {
    let remaining = deadline
        .checked_duration_since(tokio::time::Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or(Error::Deadline)?;
    let mut config = Config::new(endpoint);
    config.request_timeout = remaining;
    config.connect_timeout = config.connect_timeout.min(remaining);
    // Provider queues are deliberately smaller than host edges. The 4 MiB
    // workspace reserves payload copies; opaque prost/tonic allocations are excluded.
    config.queue_chunks = 1;
    config.queue_bytes = 65536 + 16384;
    config.max_encoding_bytes = 65536 + 16384;
    config.max_decoding_bytes = frame_limit;
    tokio::select! { biased;
        () = cancel.cancelled() => Err(Error::Cancelled),
        result = tokio::time::timeout_at(deadline, Client::connect(config)) => result.map_err(|_| Error::Deadline)?.map_err(|_| Error::Provider),
    }
}
impl SpeechBackend for GoogleSpeech {
    fn preflight_synthesis(&self, selection: &SynthesisSelection) -> Result<(), Error> {
        selection.validate()?;
        if self.synthesis.is_none() || !self.synthesis_choices.contains(selection) { return Err(Error::Invalid); }
        Ok(())
    }
    fn synthesize_selected(&self, input: Reader, output: Writer, selection: SynthesisSelection) -> Result<Producer, Error> {
        self.selected_synthesis(&selection)?.synthesize(input, output)
    }
    fn preflight(&self, input: Format, snapshot: bool) -> Result<(), Error> {
        match input {
            Format::Text if !snapshot && self.synthesis.is_some() => Ok(()),
            Format::Pcm16Le { rate, channels } => {
                let Some(ResolvedSpeechModel {
                    settings: SpeechSettings::Recognition(options),
                    ..
                }) = (if snapshot {
                    self.snapshot_recognition()
                } else {
                    self.recognition.as_ref()
                })
                else {
                    return Err(Error::Invalid);
                };
                if rate != options.sample_rate_hz
                    || u32::from(channels) != options.channels
                    || (snapshot && !options.diarization)
                {
                    return Err(Error::Invalid);
                }
                Ok(())
            }
            _ => Err(Error::Invalid),
        }
    }
    fn transcribe_snapshot(
        &self,
        input: Reader,
        workspace: super::snapshot::Workspace,
    ) -> Result<(Producer, super::snapshot::SnapshotReader), Error> {
        let resolved = self.snapshot_recognition().ok_or(Error::Invalid)?;
        let SpeechSettings::Recognition(options) = &resolved.settings else {
            return Err(Error::Invalid);
        };
        if !options.diarization
            || input.direction() != Direction::Input
            || input.format()
                != (Format::Pcm16Le {
                    rate: options.sample_rate_hz,
                    channels: options.channels as u16,
                })
        {
            return Err(Error::Invalid);
        }
        let lease = workspace.reserve(&input)?;
        let transport_workspace = input.workspace()?;
        let (mut publication, reader) = super::snapshot::channel(&input, lease)?;
        let deadline = input.deadline();
        let cancel = input.cancellation();
        let owner_cancel = cancel.clone();
        let endpoint = resolved.endpoint.clone();
        let options = options.clone();
        let producer = Producer::spawn(cancel.clone(), async move {
            let _transport_workspace = transport_workspace;
            let result = async {
                let client = connect_bounded(
                    endpoint,
                    deadline,
                    &cancel,
                    waltr_audio::compact_stt::MAX_FRAME_BYTES,
                )
                .await?;
                let status = Arc::new(InputStatus::default());
                let source =
                    stream::try_unfold((input, status.clone()), |(mut input, status)| async move {
                        match input.read().await.map_err(|e| status.fail(e))? {
                            Some(packet) => match packet.chunk() {
                                Chunk::Audio(bytes) => Ok(Some((bytes.clone(), (input, status)))),
                                _ => Err(status.fail(Error::Invalid)),
                            },
                            None => {
                                status.closed.store(true, Ordering::Release);
                                Ok(None)
                            }
                        }
                    });
                let config = stt::StreamingRecognitionConfig {
                    config: Some(stt::RecognitionConfig {
                        model: options.model,
                        language_codes: options.language_codes,
                        features: Some(stt::RecognitionFeatures {
                            enable_automatic_punctuation: options.automatic_punctuation,
                            diarization_config: Some(stt::SpeakerDiarizationConfig::default()),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    streaming_features: Some(stt::StreamingRecognitionFeatures {
                        interim_results: options.interim_results,
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let mut responses = client
                    .streaming_recognize_compact(
                        options.recognizer,
                        config,
                        PcmS16Le::new(options.sample_rate_hz as i32, options.channels as i32)
                            .map_err(|_| Error::Invalid)?,
                        source,
                        cancel.clone(),
                    )
                    .map_err(|e| status.error(e))?;
                let mut latest = None;
                let work = tokio::time::timeout_at(deadline, async {
                    // Diarization may produce no attributed words until a silent
                    // finalization completes; only the invocation deadline applies.
                    while let Some(response) = responses.next().await {
                        let response = response.map_err(|e| status.error(e))?;
                        for result in response.results() {
                            let alternative = result.alternatives().next().ok_or(Error::Invalid)?;
                            if let Some(revision) = publication.format(alternative).await? {
                                publication.publish(revision.clone()).await?;
                                latest = Some(revision);
                            }
                        }
                    }
                    status.finish()
                })
                .await
                .unwrap_or(Err(Error::Deadline));
                let joined = if work.is_err() {
                    responses.cancel_and_join().await
                } else {
                    responses.finish().await
                };
                joined.map_err(|_| Error::Worker)?;
                work?;
                publication.finish(latest.ok_or(Error::Uncommitted)?).await
            }
            .await;
            if result.is_err() {
                owner_cancel.cancel();
            }
            result
        });
        Ok((producer, reader))
    }
    fn transcribe(&self, input: Reader, mut output: Writer) -> Result<Producer, Error> {
        let resolved = self.recognition.as_ref().ok_or(Error::Invalid)?;
        let SpeechSettings::Recognition(options) = &resolved.settings else {
            return Err(Error::Invalid);
        };
        let format = Format::Pcm16Le {
            rate: options.sample_rate_hz,
            channels: options.channels as u16,
        };
        if input.format() != format
            || output.format() != Format::Text
            || !input.matches(&output)
            || input.direction() != Direction::Input
            || output.direction() != Direction::Output
        {
            return Err(Error::Invalid);
        }
        let deadline = input.deadline();
        let idle = input.idle_timeout();
        let cancel = input.cancellation();
        let owner_cancel = cancel.clone();
        // Four MiB covers the adapter's encoded/decoded bounded queues, pending
        // fragments, initial config and caller/output copies. Transport internals excluded.
        let workspace = input.workspace()?;
        let endpoint = resolved.endpoint.clone();
        let options = options.clone();
        Ok(Producer::spawn(cancel.clone(), async move {
            let _workspace = workspace;
            let result = async {
                let client = connect(endpoint, deadline, &cancel).await?;
                let status = Arc::new(InputStatus::default());
                let input_status = status.clone();
                let source =
                    stream::try_unfold((input, input_status), |(mut input, status)| async move {
                        match input.read().await.map_err(|e| status.fail(e))? {
                            Some(packet) => match packet.chunk() {
                                Chunk::Audio(bytes) => Ok(Some((bytes.clone(), (input, status)))),
                                _ => Err(status.fail(Error::Invalid)),
                            },
                            None => {
                                status.closed.store(true, Ordering::Release);
                                Ok(None)
                            }
                        }
                    });
                let config = stt::StreamingRecognitionConfig {
                    config: Some(stt::RecognitionConfig {
                        model: options.model,
                        language_codes: options.language_codes,
                        features: Some(stt::RecognitionFeatures {
                            enable_automatic_punctuation: options.automatic_punctuation,
                            diarization_config: options
                                .diarization
                                .then(stt::SpeakerDiarizationConfig::default),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }),
                    streaming_features: Some(stt::StreamingRecognitionFeatures {
                        interim_results: options.interim_results,
                        ..Default::default()
                    }),
                    ..Default::default()
                };
                let mut responses = client
                    .streaming_recognize_compact(
                        options.recognizer,
                        config,
                        PcmS16Le::new(options.sample_rate_hz as i32, options.channels as i32)
                            .map_err(|_| Error::Invalid)?,
                        source,
                        cancel.clone(),
                    )
                    .map_err(|e| status.error(e))?;
                let mut transcripts = Transcripts::new(options.diarization);
                let work = tokio::time::timeout_at(deadline, async {
                    while let Some(response) = tokio::time::timeout(idle, responses.next())
                        .await
                        .map_err(|_| Error::Deadline)?
                    {
                        for text in
                            transcripts.update_compact(&response.map_err(|e| status.error(e))?)?
                        {
                            output.write(output.key(), Chunk::Text(text)).await?;
                        }
                    }
                    status.finish()?;
                    Ok(())
                })
                .await
                .unwrap_or(Err(Error::Deadline));
                // Even failed reads/writes retain the audio owner until both tasks join.
                let joined = if work.is_err() {
                    responses.cancel_and_join().await
                } else {
                    responses.finish().await
                };
                joined.map_err(|_| Error::Worker)?;
                work?;
                if let Some(text) = transcripts.complete()? {
                    output.write(output.key(), Chunk::Text(text)).await?;
                }
                output.close(output.key()).await
            }
            .await;
            if result.is_err() {
                owner_cancel.cancel();
            }
            result
        }))
    }
    fn synthesize(&self, input: Reader, mut output: Writer) -> Result<Producer, Error> {
        let resolved = self.synthesis.as_ref().ok_or(Error::Invalid)?;
        let SpeechSettings::Synthesis(options) = &resolved.settings else {
            return Err(Error::Invalid);
        };
        if input.format() != Format::Text
            || output.format()
                != (Format::Pcm16Le {
                    rate: 24000,
                    channels: 1,
                })
            || !input.matches(&output)
            || input.direction() != Direction::Input
            || output.direction() != Direction::Output
        {
            return Err(Error::Invalid);
        }
        let deadline = input.deadline();
        let idle = input.idle_timeout();
        let cancel = input.cancellation();
        let owner_cancel = cancel.clone();
        let workspace = input.workspace()?;
        let endpoint = resolved.endpoint.clone();
        let options = options.clone();
        let state = input.text_state();
        Ok(Producer::spawn(cancel.clone(), async move {
            let _workspace = workspace;
            let result = async {
                let client = connect(endpoint, deadline, &cancel).await?;
                let status = Arc::new(InputStatus::default());
                let source = stream::try_unfold(
                    (input, state, 0, status.clone()),
                    |(mut input, mut state, mut next, status)| async move {
                        loop {
                            if let Some(text) = state.committed_at(next) {
                                let message = tts::StreamingSynthesisInput {
                                    input_source: Some(
                                        tts::streaming_synthesis_input::InputSource::Text(
                                            text.to_owned(),
                                        ),
                                    ),
                                    ..Default::default()
                                };
                                next += 1;
                                return Ok(Some((message, (input, state, next, status))));
                            }
                            match input.read().await.map_err(|e| status.fail(e))? {
                                Some(packet) => {
                                    state.accept(packet.chunk()).map_err(|e| status.fail(e))?
                                }
                                None => {
                                    state.close().map_err(|e| status.fail(e))?;
                                    status.closed.store(true, Ordering::Release);
                                    return Ok(None);
                                }
                            }
                        }
                    },
                );
                let config = tts::StreamingSynthesizeConfig {
                    voice: Some(tts::VoiceSelectionParams {
                        name: options.voice,
                        language_code: options.language_code,
                        model_name: options.model_name.unwrap_or_default(),
                        ..Default::default()
                    }),
                    streaming_audio_config: Some(tts::StreamingAudioConfig {
                        audio_encoding: tts::AudioEncoding::Pcm as i32,
                        sample_rate_hertz: 24000,
                        speaking_rate: options.speaking_rate.unwrap_or(1.0),
                    }),
                    ..Default::default()
                };
                let mut responses = client
                    .streaming_synthesize(config, source, cancel.clone())
                    .map_err(|e| status.error(e))?;
                let work = tokio::time::timeout_at(deadline, async {
                    let mut odd = None;
                    while let Some(response) = tokio::time::timeout(idle, responses.next())
                        .await
                        .map_err(|_| Error::Deadline)?
                    {
                        let bytes = response.map_err(|e| status.error(e))?.audio_content;
                        let mut at = 0;
                        if let Some(previous) = odd.take() {
                            if let Some(first) = bytes.first() {
                                output
                                    .write(output.key(), Chunk::Audio(vec![previous, *first]))
                                    .await?;
                                at = 1;
                            } else {
                                odd = Some(previous);
                            }
                        }
                        let end = at + (bytes.len() - at) / 2 * 2;
                        for chunk in bytes[at..end].chunks(65536) {
                            output
                                .write(output.key(), Chunk::Audio(chunk.to_vec()))
                                .await?;
                        }
                        if end < bytes.len() {
                            odd = Some(bytes[end]);
                        }
                    }
                    if odd.is_some() {
                        return Err(Error::Invalid);
                    }
                    status.finish()
                })
                .await
                .unwrap_or(Err(Error::Deadline));
                let joined = if work.is_err() {
                    responses.cancel_and_join().await
                } else {
                    responses.finish().await
                };
                joined.map_err(|_| Error::Worker)?;
                work?;
                output.close(output.key()).await
            }
            .await;
            if result.is_err() {
                owner_cancel.cancel();
            }
            result
        }))
    }
}

struct Transcripts {
    channels: BTreeMap<i32, u64>,
    diarization: bool,
    latest_speakers: Option<String>,
}
impl Transcripts {
    fn update_compact(
        &mut self,
        response: &waltr_audio::compact_stt::Response,
    ) -> Result<Vec<Text>, Error> {
        if self.diarization {
            let mut output = Vec::new();
            for result in response.results() {
                let alternative = result.alternatives().next().ok_or(Error::Invalid)?;
                let mut words = alternative.words().peekable();
                let text = if words.peek().is_some() {
                    let mut text = String::new();
                    let mut previous = None;
                    for word in words {
                        let speaker = if word.speaker().trim().is_empty() {
                            "unknown"
                        } else {
                            word.speaker()
                        };
                        if speaker.chars().any(char::is_control) {
                            return Err(Error::Invalid);
                        }
                        let extra = word.word().len()
                            + 1
                            + if previous != Some(speaker) {
                                speaker.len() + 1 + usize::from(!text.is_empty())
                            } else {
                                0
                            };
                        if text.len() + extra > 65536 - "diarization".len() {
                            return Err(Error::Limit);
                        }
                        if previous != Some(speaker) {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(speaker);
                            text.push(':');
                            previous = Some(speaker);
                        }
                        text.push(' ');
                        text.push_str(word.word());
                    }
                    self.latest_speakers = Some(text.clone());
                    text
                } else {
                    alternative.transcript().to_owned()
                };
                output.push(Text {
                    segment: "diarization".into(),
                    text,
                    replace: true,
                    committed: false,
                });
            }
            return Ok(output);
        }
        // Ordinary recognition reuses the tested segment-order algorithm, but only
        // materializes first-alternative transcripts, never generated WordInfo arrays.
        self.update(stt::StreamingRecognizeResponse {
            results: response
                .results()
                .map(|result| {
                    let alternative = result.alternatives().next().ok_or(Error::Invalid)?;
                    Ok(stt::StreamingRecognitionResult {
                        channel_tag: result.channel() as i32,
                        is_final: result.is_final(),
                        alternatives: vec![stt::SpeechRecognitionAlternative {
                            transcript: alternative.transcript().to_owned(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                })
                .collect::<Result<_, Error>>()?,
            ..Default::default()
        })
    }
    fn new(diarization: bool) -> Self {
        Self {
            channels: BTreeMap::new(),
            diarization,
            latest_speakers: None,
        }
    }
    fn update(&mut self, response: stt::StreamingRecognizeResponse) -> Result<Vec<Text>, Error> {
        if response.results.len() > 4096 {
            return Err(Error::Limit);
        }
        let mut output: Vec<Text> = Vec::new();
        let mut interim = BTreeMap::<i32, usize>::new();
        for result in response.results {
            if self.diarization {
                let alternative = result.alternatives.first().ok_or(Error::Invalid)?;
                // is_final freezes only a portion. Words can cover the entire recording
                // and revise earlier speakers; the portion transcript is not their checksum.
                let text = if !alternative.words.is_empty() {
                    let text = speaker_text(alternative)?;
                    self.latest_speakers = Some(text.clone());
                    text
                } else {
                    alternative.transcript.clone()
                };
                output.push(Text {
                    segment: "diarization".into(),
                    text,
                    replace: true,
                    committed: false,
                });
                continue;
            }
            if !self.channels.contains_key(&result.channel_tag) && self.channels.len() >= 8 {
                return Err(Error::Limit);
            }
            let ordinal = self.channels.entry(result.channel_tag).or_default();
            let text = result
                .alternatives
                .first()
                .map(|a| a.transcript.clone())
                .unwrap_or_default();
            if result.is_final {
                output.push(Text {
                    segment: format!("channel-{}-{}", result.channel_tag, ordinal),
                    text,
                    replace: true,
                    committed: true,
                });
                interim.remove(&result.channel_tag);
                *ordinal = ordinal.checked_add(1).ok_or(Error::Limit)?;
            } else if let Some(index) = interim.get(&result.channel_tag) {
                output[*index].text.push_str(&text);
            } else {
                interim.insert(result.channel_tag, output.len());
                output.push(Text {
                    segment: format!("channel-{}-{}", result.channel_tag, ordinal),
                    text,
                    replace: true,
                    committed: false,
                });
            }
        }
        Ok(output)
    }
    // Only called after successful provider EOF AND explicit successful input EOF.
    fn complete(&mut self) -> Result<Option<Text>, Error> {
        if !self.diarization {
            return Ok(None);
        }
        let text = self.latest_speakers.take().ok_or(Error::Uncommitted)?;
        Ok(Some(Text {
            segment: "diarization".into(),
            text,
            replace: true,
            committed: true,
        }))
    }
}
fn speaker_text(alternative: &stt::SpeechRecognitionAlternative) -> Result<String, Error> {
    let mut output = String::new();
    let mut previous = None;
    for word in &alternative.words {
        if word.word.is_empty()
            || word.word.chars().any(char::is_whitespace)
            || word.speaker_label.len() > 256
            || word.speaker_label.chars().any(char::is_control)
        {
            return Err(Error::Invalid);
        }
        let speaker = if word.speaker_label.trim().is_empty() {
            "unknown"
        } else {
            word.speaker_label.as_str()
        };
        if previous != Some(speaker) {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(speaker);
            output.push(':');
            previous = Some(speaker);
        }
        output.push(' ');
        output.push_str(&word.word);
        // Bound formatted amplification inside the pre-reserved speech workspace.
        if output.len() > 65536 - "diarization".len() {
            return Err(Error::Limit);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn recognition(diarization: bool) -> ResolvedSpeechModel {
        use waltr::grpc::waltr::config::v1 as proto;
        ResolvedSpeechModel {
            provider_key: "speech".into(),
            model_key: "pinned".into(),
            endpoint: "http://localhost:1".into(),
            capabilities: proto::SpeechCapabilities {
                streaming: true,
                unary: false,
            },
            settings: SpeechSettings::Recognition(proto::SpeechRecognitionOptions {
                recognizer: "projects/test/locations/global/recognizers/_".into(),
                model: "pinned-upstream".into(),
                language_codes: vec!["en-US".into()],
                sample_rate_hz: 24000,
                channels: 1,
                diarization,
                ..Default::default()
            }),
        }
    }
    #[test]
    fn snapshot_selection_preserves_legacy_and_snapshot_only_preflight() {
        let format = Format::Pcm16Le {
            rate: 24000,
            channels: 1,
        };
        let legacy = GoogleSpeech::new(Some(recognition(true)), None).unwrap();
        legacy.preflight(format, false).unwrap();
        legacy.preflight(format, true).unwrap();
        let plain = GoogleSpeech::new(Some(recognition(false)), None).unwrap();
        plain.preflight(format, false).unwrap();
        assert_eq!(plain.preflight(format, true), Err(Error::Invalid));
        let snapshot_only =
            GoogleSpeech::with_recognition_selections(None, Some(recognition(true)), None).unwrap();
        snapshot_only.preflight(format, true).unwrap();
        assert_eq!(snapshot_only.preflight(format, false), Err(Error::Invalid));
        assert_eq!(
            snapshot_only.preflight(
                Format::Pcm16Le {
                    rate: 16000,
                    channels: 1
                },
                true
            ),
            Err(Error::Invalid)
        );
        assert!(
            GoogleSpeech::with_recognition_selections(
                Some(recognition(true)),
                Some(recognition(false)),
                None
            )
            .is_err()
        );
        let mut invalid = recognition(true);
        invalid.capabilities.streaming = false;
        assert!(
            GoogleSpeech::with_recognition_selections(Some(recognition(true)), Some(invalid), None)
                .is_err()
        );
        let mut invalid = recognition(true);
        invalid.endpoint = "http://user:secret-sentinel@localhost:1".into();
        assert!(
            GoogleSpeech::with_recognition_selections(Some(recognition(true)), Some(invalid), None)
                .is_err()
        );
    }
    fn synthesis() -> GoogleSpeech {
        use waltr::grpc::waltr::config::v1 as proto;
        GoogleSpeech::new(None, Some(ResolvedSpeechModel {
            provider_key:"enabled".into(), model_key:"pinned".into(), endpoint:"http://localhost:1".into(),
            capabilities:proto::SpeechCapabilities { streaming:true, unary:false },
            settings:SpeechSettings::Synthesis(proto::SpeechSynthesisOptions {
                voice:"voice-a".into(), language_code:"en-US".into(), sample_rate_hz:24000, channels:1,
                speaking_rate:Some(1.0), model_name:Some("upstream-pinned".into()), ..Default::default()
            }),
        })).unwrap()
    }
    #[test]
    fn explicit_synthesis_rejects_unapproved_or_unsupported_choices_without_network() {
        let speech = synthesis();
        let allowed = SynthesisSelection { model:"pinned".into(), voice:"voice-a".into(), language:"en-US".into(), rate:1.0 };
        speech.preflight_synthesis(&allowed).unwrap();
        for change in [
            SynthesisSelection { model:"foreign".into(), ..allowed.clone() },
            SynthesisSelection { voice:"voice-b".into(), ..allowed.clone() },
            SynthesisSelection { language:"fr-FR".into(), ..allowed.clone() },
            SynthesisSelection { rate:0.0, ..allowed.clone() },
            SynthesisSelection { rate:f64::NAN, ..allowed.clone() },
        ] { assert!(speech.preflight_synthesis(&change).is_err()); }
        assert!(synthesis().with_synthesis_choices(vec![SynthesisSelection { model:"foreign".into(), ..allowed }]).is_err());
        assert!(serde_json::from_value::<SynthesisSelection>(serde_json::json!({"model":"pinned","voice":"voice-a","language":"en-US","rate":1,"endpoint":"https://foreign"})).is_err());
    }
    #[tokio::test]
    async fn concurrent_synthesis_selection_keeps_shared_backend_options_immutable() {
        let first = SynthesisSelection { model:"pinned".into(), voice:"voice-a".into(), language:"en-US".into(), rate:1.0 };
        let second = SynthesisSelection { voice:"voice-b".into(), language:"fr-FR".into(), rate:1.25, ..first.clone() };
        let speech = Arc::new(synthesis().with_synthesis_choices(vec![first.clone(), second.clone()]).unwrap());
        let (a, b) = tokio::join!(async { speech.selected_synthesis(&first).unwrap() }, async { speech.selected_synthesis(&second).unwrap() });
        let options = |backend: &GoogleSpeech| match &backend.synthesis.as_ref().unwrap().settings {
            SpeechSettings::Synthesis(options) => (options.voice.clone(), options.language_code.clone(), options.speaking_rate, options.model_name.clone()),
            _ => panic!("synthesis settings"),
        };
        assert_eq!(options(&a), ("voice-a".into(), "en-US".into(), Some(1.0), Some("upstream-pinned".into())));
        assert_eq!(options(&b), ("voice-b".into(), "fr-FR".into(), Some(1.25), Some("upstream-pinned".into())));
        assert_eq!(options(&speech), options(&a));
    }
    #[test]
    fn cumulative_speaker_words_are_validated_independently_of_portion_transcript() {
        let mut alternative = stt::SpeechRecognitionAlternative {
            transcript: "hello world".into(),
            words: vec![
                stt::WordInfo {
                    word: "hello".into(),
                    speaker_label: "A".into(),
                    ..Default::default()
                },
                stt::WordInfo {
                    word: "world".into(),
                    speaker_label: String::new(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            speaker_text(&alternative).unwrap(),
            "A: hello\nunknown: world"
        );
        alternative.words.pop();
        assert_eq!(speaker_text(&alternative).unwrap(), "A: hello");
        alternative.words[0].word = "two words".into();
        assert_eq!(speaker_text(&alternative), Err(Error::Invalid));
        let mut transcripts = Transcripts::new(false);
        let response = stt::StreamingRecognizeResponse {
            results: vec![stt::StreamingRecognitionResult {
                channel_tag: 3,
                is_final: true,
                alternatives: vec![stt::SpeechRecognitionAlternative {
                    transcript: "hello".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        };
        let output = transcripts.update(response).unwrap();
        assert_eq!(output[0].segment, "channel-3-0");
        assert_eq!(output[0].text, "hello");
    }
    #[test]
    fn pinned_constructor_rejects_wrong_kind_and_credentials_without_network() {
        use waltr::grpc::waltr::config::v1 as proto;
        let model = ResolvedSpeechModel {
            provider_key: "speech".into(),
            model_key: "wrong".into(),
            endpoint: "http://user:password@localhost:1".into(),
            capabilities: proto::SpeechCapabilities {
                streaming: true,
                unary: false,
            },
            settings: SpeechSettings::Synthesis(proto::SpeechSynthesisOptions::default()),
        };
        assert!(matches!(
            GoogleSpeech::new(Some(model.clone()), None),
            Err(Error::Invalid)
        ));
        let mut model = model;
        model.endpoint = "http://localhost:1".into();
        assert!(matches!(
            GoogleSpeech::new(Some(model), None),
            Err(Error::Invalid)
        ));
    }
}
