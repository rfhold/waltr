use super::{AdapterFuture, Adapters, Chunk, Error, Frame, State, TaskContext, TaskResult, Text};
use crate::{
    resource_catalog::{
        routine::{RoutineStreamFormat, RoutineTask},
        speech::{SpeechSettings, resolve_speech_model},
    },
    session::SessionHost,
};
use futures_util::{StreamExt, stream};
use std::{collections::BTreeMap, num::NonZeroU64, sync::Arc, time::Duration};
use waltr_agent::{
    AgentRunLimits, CHANNEL_ASSISTANT_TEXT, SessionId, SessionStart, SessionStartMessage,
    SessionTimelineProjectionUpdateEvent, TimelineContent,
};
use waltr_audio::{audio::PcmS16Le, client::Client, config::Config, stt, tts};

/// Production adapters use the existing durable helper host and explicit pinned speech settings.
pub(crate) struct SessionAdapters {
    sessions: Arc<dyn SessionHost>,
}
impl SessionAdapters {
    pub(crate) fn new(sessions: Arc<dyn SessionHost>) -> Self {
        Self { sessions }
    }
}
impl Adapters for SessionAdapters {
    fn execute<'a>(
        &'a self,
        task: &'a RoutineTask,
        context: &'a mut TaskContext,
    ) -> AdapterFuture<'a> {
        Box::pin(async move {
            if task.task_spec.collect.is_some() {
                collect(task, context).await
            } else if task.task_spec.transcribe.is_some() {
                transcribe(task, context).await
            } else if task.task_spec.synthesize.is_some() {
                synthesize(task, context).await
            } else if task.task_spec.session_template.is_some() {
                self.agent(task, context).await
            } else {
                Err(Error::new(
                    "unsupported_task",
                    "interaction action adapter is not available",
                ))
            }
        })
    }
}
fn validate_helper(task: &RoutineTask) -> Result<(), Error> {
    let template = task
        .task_spec
        .session_template
        .as_ref()
        .expect("helper task");
    if template.model.is_none() || template.source.is_some() || template.inference.is_some() {
        return Err(Error::new(
            "invalid_helper",
            "interaction helper requires standalone model selection",
        ));
    }
    if task
        .task_spec
        .limits
        .as_ref()
        .is_none_or(|v| v.max_turns != 1)
        || task.task_spec.tools.as_ref().is_some_and(|v| !v.is_empty())
        || task
            .task_spec
            .features
            .as_ref()
            .is_some_and(|v| !v.is_empty())
        || task
            .task_spec
            .permissions
            .as_ref()
            .is_some_and(|v| !v.is_empty())
    {
        return Err(Error::new(
            "invalid_helper",
            "interaction cleanup requires maxTurns 1 with no tools, features, or permissions",
        ));
    }
    Ok(())
}
fn validate_synthesis(
    options: &waltr::grpc::waltr::config::v1::SpeechSynthesisOptions,
) -> Result<(), Error> {
    if options.channels != 1 || options.pitch.is_some_and(|v| v != 0.0) {
        return Err(Error::new(
            "unsupported_format",
            "streaming synthesis requires mono and cannot map pitch",
        ));
    }
    Ok(())
}

/// Network-free adapter preflight; dynamic prompts and context size remain execution checks.
pub(super) fn validate_capabilities(
    routine: &crate::resource_catalog::routine::MaterializedRoutine,
    configuration: &waltr::grpc::waltr::config::v1::Configuration,
) -> Result<(), Error> {
    use waltr::grpc::waltr::config::v1::ModelType;
    for task in &routine.definition().spec.tasks {
        let spec = &task.task_spec;
        if spec.action.is_some() {
            return Err(Error::new(
                "unsupported_task",
                "interaction action adapter is not available",
            ));
        }
        if let Some(transcribe) = &spec.transcribe {
            resolve_speech_model(
                configuration,
                &transcribe.model.provider,
                &transcribe.model.model,
                ModelType::Stt,
                true,
            )
            .map_err(|_| Error::new("invalid_model", "invalid streaming recognition model"))?;
        }
        if let Some(synthesize) = &spec.synthesize {
            let resolved = resolve_speech_model(
                configuration,
                &synthesize.model.provider,
                &synthesize.model.model,
                ModelType::Tts,
                true,
            )
            .map_err(|_| Error::new("invalid_model", "invalid streaming synthesis model"))?;
            let SpeechSettings::Synthesis(options) = resolved.settings else {
                unreachable!()
            };
            validate_synthesis(&options)?;
        }
        if let Some(template) = &spec.session_template {
            validate_helper(task)?;
            let model = template.model.as_ref().expect("validated helper model");
            crate::session::pinned_inference_settings(
                model.provider.clone(),
                model.model.clone(),
                configuration,
            )
            .map_err(|_| Error::new("invalid_model", "invalid pinned inference selection"))?;
        }
    }
    crate::resource_catalog::routine::validate_routine_models(
        routine.definition(),
        configuration,
        routine.source_path(),
    )
    .map_err(|_| {
        Error::new(
            "invalid_model",
            "invalid pinned interaction model selection",
        )
    })?;
    Ok(())
}

async fn collect(task: &RoutineTask, context: &mut TaskContext) -> Result<TaskResult, Error> {
    let collect = task.task_spec.collect.as_ref().expect("collect task");
    let max = collect.max_bytes.min(context.limits.collector_bytes);
    let mut state = State::new(RoutineStreamFormat::Text, context.limits.collector_bytes);
    let input = context
        .input
        .as_mut()
        .ok_or_else(|| Error::new("invalid_stream", "collector input missing"))?;
    loop {
        match input.recv(&context.cancellation).await? {
            Frame::Chunk(chunk) => state.accept(&chunk)?,
            Frame::Close => {
                state.close()?;
                break;
            }
            Frame::Failed(error) => return Err(error),
        }
    }
    let output = state.collect(&collect.separator, max)?;
    let values = super::super::executor::collect_results(
        &task.task_spec.results,
        &context.expression,
        &context.results,
        Some(&output),
        None,
    )?;
    Ok(TaskResult {
        values,
        ..Default::default()
    })
}
fn provider_error(_: impl std::fmt::Display) -> Error {
    Error::new("provider", "speech provider operation failed")
}
#[derive(Default)]
struct InputStatus {
    error: std::sync::Mutex<Option<Error>>,
    closed: std::sync::atomic::AtomicBool,
}
impl InputStatus {
    fn fail(&self, error: Error) -> waltr_audio::error::Error {
        *self.error.lock().expect("speech input status") = Some(error);
        waltr_audio::error::Error::Invalid("speech input failed")
    }
    fn provider_error(&self, error: impl std::fmt::Display) -> Error {
        self.error
            .lock()
            .expect("speech input status")
            .clone()
            .unwrap_or_else(|| provider_error(error))
    }
    fn finish(&self) -> Result<(), Error> {
        if let Some(error) = self.error.lock().expect("speech input status").clone() {
            return Err(error);
        }
        if !self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::new(
                "stream_failed",
                "speech provider closed before successful input close",
            ));
        }
        Ok(())
    }
}
async fn connect(endpoint: String, context: &TaskContext) -> Result<Client, Error> {
    let mut config = Config::new(endpoint);
    config.request_timeout = Duration::from_secs(context.limits.invocation_seconds);
    config.queue_chunks = context.limits.queued_chunks;
    config.queue_bytes = context.limits.edge_bytes;
    // Protobuf overhead is finite and separate from the PCM/text payload limit.
    config.max_encoding_bytes = context.limits.chunk_bytes.saturating_add(16 * 1024);
    config.max_decoding_bytes = context.limits.chunk_bytes.saturating_add(16 * 1024);
    tokio::select! { biased;
        () = context.cancellation.cancelled() => Err(Error::cancelled()),
        client = Client::connect(config) => client.map_err(provider_error),
    }
}

/// Plain results use channel/ordinal segments; diarized results replace one cumulative segment.
/// Channel IDs are never speaker IDs.
#[derive(Default)]
pub(super) struct Transcripts {
    channels: BTreeMap<i32, u64>,
    diarization: bool,
    finalized: bool,
}
impl Transcripts {
    pub(super) fn new(diarization: bool) -> Self {
        Self {
            diarization,
            ..Default::default()
        }
    }

    pub(super) fn update(
        &mut self,
        response: stt::StreamingRecognizeResponse,
    ) -> Result<Vec<Text>, Error> {
        if !self.diarization {
            return Ok(self.map(response));
        }
        let mut output = Vec::new();
        for result in response.results {
            if self.finalized {
                return Err(Error::new(
                    "invalid_stream",
                    "response after final cumulative diarization",
                ));
            }
            let alternative = result.alternatives.first().ok_or_else(|| {
                Error::new("invalid_stream", "diarization response missing alternative")
            })?;
            let text = if result.is_final {
                speaker_text(alternative)?
            } else {
                alternative.transcript.clone()
            };
            output.push(Text {
                segment: "diarization".into(),
                text,
                replace: true,
                committed: result.is_final,
            });
            self.finalized = result.is_final;
        }
        Ok(output)
    }

    pub(super) fn map(&mut self, response: stt::StreamingRecognizeResponse) -> Vec<Text> {
        let mut output: Vec<Text> = Vec::new();
        let mut interim: BTreeMap<i32, usize> = BTreeMap::new();
        for result in response.results {
            let text = result
                .alternatives
                .first()
                .map(|a| a.transcript.clone())
                .unwrap_or_default();
            let ordinal = self.channels.entry(result.channel_tag).or_default();
            if result.is_final {
                output.push(Text {
                    segment: format!("channel-{}-{}", result.channel_tag, ordinal),
                    text,
                    replace: true,
                    committed: true,
                });
                interim.remove(&result.channel_tag);
                *ordinal += 1;
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
        output
    }
}
fn speaker_text(alternative: &stt::SpeechRecognitionAlternative) -> Result<String, Error> {
    let invalid = || {
        Error::new(
            "invalid_stream",
            "invalid cumulative diarization words or labels",
        )
    };
    if !alternative
        .transcript
        .split_whitespace()
        .eq(alternative.words.iter().map(|word| word.word.as_str()))
    {
        return Err(invalid());
    }
    let mut text = String::new();
    let mut previous = None;
    for word in &alternative.words {
        if word.word.is_empty()
            || word.word.chars().any(char::is_whitespace)
            || word.speaker_label.len() > 256
            || word.speaker_label.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        let speaker = if word.speaker_label.trim().is_empty() {
            "unknown"
        } else {
            word.speaker_label.as_str()
        };
        if previous != Some(speaker) {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(speaker);
            text.push(':');
            previous = Some(speaker);
        }
        text.push(' ');
        text.push_str(&word.word);
    }
    Ok(text)
}

pub(super) fn recognition_features(
    options: &waltr::grpc::waltr::config::v1::SpeechRecognitionOptions,
) -> stt::RecognitionFeatures {
    stt::RecognitionFeatures {
        enable_automatic_punctuation: options.automatic_punctuation,
        diarization_config: options
            .diarization
            .then(stt::SpeakerDiarizationConfig::default),
        ..Default::default()
    }
}

async fn transcribe(task: &RoutineTask, context: &mut TaskContext) -> Result<TaskResult, Error> {
    let task = task.task_spec.transcribe.as_ref().expect("transcribe task");
    let resolved = resolve_speech_model(
        &context.configuration,
        &task.model.provider,
        &task.model.model,
        waltr::grpc::waltr::config::v1::ModelType::Stt,
        true,
    )
    .map_err(|_| Error::new("invalid_model", "invalid recognition model"))?;
    let SpeechSettings::Recognition(options) = resolved.settings else {
        unreachable!()
    };
    let client = connect(resolved.endpoint, context).await?;
    let input = context
        .input
        .take()
        .ok_or_else(|| Error::new("invalid_stream", "recognition input missing"))?;
    let cancel = context.cancellation.clone();
    let status = Arc::new(InputStatus::default());
    let input = stream::try_unfold(
        (input, cancel, status.clone()),
        |(mut input, cancel, status)| async move {
            match input.recv(&cancel).await.map_err(|e| status.fail(e))? {
                Frame::Chunk(Chunk::Audio(bytes)) => Ok(Some((bytes, (input, cancel, status)))),
                Frame::Chunk(_) => {
                    Err(status.fail(Error::new("invalid_format", "recognition requires PCM")))
                }
                Frame::Close => {
                    status
                        .closed
                        .store(true, std::sync::atomic::Ordering::Release);
                    Ok(None)
                }
                Frame::Failed(error) => Err(status.fail(error)),
            }
        },
    );
    let features = recognition_features(&options);
    let mut transcripts = Transcripts::new(options.diarization);
    let config = stt::StreamingRecognitionConfig {
        config: Some(stt::RecognitionConfig {
            model: options.model,
            language_codes: options.language_codes,
            features: Some(features),
            ..Default::default()
        }),
        streaming_features: Some(stt::StreamingRecognitionFeatures {
            interim_results: options.interim_results,
            ..Default::default()
        }),
        ..Default::default()
    };
    let format = PcmS16Le::new(options.sample_rate_hz as i32, options.channels as i32)
        .map_err(provider_error)?;
    let mut responses = client
        .streaming_recognize(
            options.recognizer,
            config,
            format,
            input,
            context.cancellation.clone(),
        )
        .map_err(provider_error)?;
    while let Some(response) = responses.next().await {
        for text in transcripts.update(response.map_err(|e| status.provider_error(e))?)? {
            context.output.publish(Chunk::Text(text)).await?;
        }
    }
    status.finish()?;
    if options.diarization && !transcripts.finalized {
        return Err(Error::new(
            "invalid_stream",
            "missing final cumulative diarization",
        ));
    }
    Ok(TaskResult::default())
}
async fn synthesize(task: &RoutineTask, context: &mut TaskContext) -> Result<TaskResult, Error> {
    let task = task.task_spec.synthesize.as_ref().expect("synthesize task");
    let resolved = resolve_speech_model(
        &context.configuration,
        &task.model.provider,
        &task.model.model,
        waltr::grpc::waltr::config::v1::ModelType::Tts,
        true,
    )
    .map_err(|_| Error::new("invalid_model", "invalid synthesis model"))?;
    let SpeechSettings::Synthesis(options) = resolved.settings else {
        unreachable!()
    };
    validate_synthesis(&options)?;
    let client = connect(resolved.endpoint, context).await?;
    let input = context
        .input
        .take()
        .ok_or_else(|| Error::new("invalid_stream", "synthesis input missing"))?;
    let state = State::new(
        RoutineStreamFormat::Text,
        context.limits.complete_request_bytes,
    );
    let cancel = context.cancellation.clone();
    let status = Arc::new(InputStatus::default());
    let input = stream::try_unfold(
        (input, state, 0, cancel, status.clone()),
        |(mut input, mut state, mut next_segment, cancel, status)| async move {
            loop {
                if let Some(committed) = state.committed_at(next_segment) {
                    let message = tts::StreamingSynthesisInput {
                        input_source: Some(tts::streaming_synthesis_input::InputSource::Text(
                            committed.to_owned(),
                        )),
                        ..Default::default()
                    };
                    next_segment += 1;
                    return Ok(Some((
                        message,
                        (input, state, next_segment, cancel, status),
                    )));
                }
                match input.recv(&cancel).await.map_err(|e| status.fail(e))? {
                    Frame::Chunk(chunk) => {
                        state.accept(&chunk).map_err(|e| status.fail(e))?;
                    }
                    Frame::Close => {
                        state.close().map_err(|e| status.fail(e))?;
                        status
                            .closed
                            .store(true, std::sync::atomic::Ordering::Release);
                        return Ok(None);
                    }
                    Frame::Failed(error) => return Err(status.fail(error)),
                }
            }
        },
    );
    let config = tts::StreamingSynthesizeConfig {
        voice: Some(tts::VoiceSelectionParams {
            name: options.voice,
            language_code: options.language_code,
            ..Default::default()
        }),
        streaming_audio_config: Some(tts::StreamingAudioConfig {
            audio_encoding: tts::AudioEncoding::Pcm as i32,
            sample_rate_hertz: options.sample_rate_hz as i32,
            speaking_rate: options.speaking_rate.unwrap_or(1.0),
        }),
        ..Default::default()
    };
    let mut responses = client
        .streaming_synthesize(config, input, context.cancellation.clone())
        .map_err(provider_error)?;
    let mut pending = Vec::new();
    let chunk_bytes = context.limits.chunk_bytes / 2 * 2;
    if chunk_bytes == 0 {
        return Err(Error::new(
            "invalid_limits",
            "audio output needs at least one PCM frame per chunk",
        ));
    }
    while let Some(response) = responses.next().await {
        pending.extend_from_slice(
            &response
                .map_err(|e| status.provider_error(e))?
                .audio_content,
        );
        let aligned = pending.len() / 2 * 2;
        for bytes in pending[..aligned].chunks(chunk_bytes) {
            context.output.publish(Chunk::Audio(bytes.to_vec())).await?;
        }
        pending.drain(..aligned);
    }
    if !pending.is_empty() {
        return Err(Error::new(
            "invalid_format",
            "upstream PCM ended with an incomplete frame",
        ));
    }
    status.finish()?;
    Ok(TaskResult::default())
}

impl SessionAdapters {
    async fn agent(
        &self,
        task: &RoutineTask,
        context: &mut TaskContext,
    ) -> Result<TaskResult, Error> {
        let template = task
            .task_spec
            .session_template
            .as_ref()
            .expect("agent task");
        let model = template.model.as_ref().ok_or_else(|| {
            Error::new(
                "invalid_model",
                "interaction helper requires standalone model selection",
            )
        })?;
        let inference = crate::session::pinned_inference_settings(
            model.provider.clone(),
            model.model.clone(),
            &context.configuration,
        )
        .map_err(|_| Error::new("invalid_model", "invalid pinned inference selection"))?;
        let system = super::super::expression::substitute_string(
            &template.system_prompt.text,
            &context.expression,
            &context.results,
            None,
            None,
        )?;
        let prompt = super::super::expression::substitute_string(
            &template.user_prompt.text,
            &context.expression,
            &context.results,
            None,
            None,
        )?;
        let bundle = super::super::executor::agent_bundle(task, &system)?;
        validate_helper(task)?;
        // Conservative byte bound: no tokenizer is available at this coordination boundary.
        // A byte per token is an upper bound for supported UTF-8 inference tokenizers.
        let model_config = &context.configuration.providers[&model.provider].models[&model.model];
        let context_limit = model_config
            .limits
            .as_ref()
            .and_then(|v| v.context)
            .unwrap_or_default();
        if context_limit > 0
            && system
                .len()
                .saturating_add(prompt.len())
                .saturating_add(1024)
                > context_limit as usize
        {
            return Err(Error::new(
                "context_exceeded",
                "cleanup prompt exceeds conservative model context budget",
            ));
        }
        let request = super::super::helper::request(
            super::super::helper::HelperIdentity {
                execution_id: &context.request_id,
                invocation_id: context.invocation_id.as_deref(),
                routine: &context.routine_name,
                revision: &context.routine_revision,
                trigger: &context.trigger,
                task: &task.name,
                source: None,
            },
            inference,
            bundle,
            SessionStart::Fresh {
                messages: vec![
                    SessionStartMessage::user(prompt)
                        .map_err(|_| Error::new("invalid_prompt", "invalid cleanup prompt"))?,
                ],
            },
        );
        let id = request.id.clone();
        // Creation itself is durable and awaited; cancellation never abandons an ambiguous write.
        self.sessions
            .create_exact(request)
            .await
            .map_err(|_| Error::new("child_create", "helper creation failed"))?;
        let result = self.run_agent(id.clone(), context).await;
        if result.is_err() {
            self.sessions
                .interrupt(&id)
                .await
                .map_err(|_| Error::new("child_interrupt", "helper interruption failed"))?;
        }
        let output = result?;
        let values = super::super::executor::collect_results(
            &task.task_spec.results,
            &context.expression,
            &context.results,
            Some(&output),
            None,
        )?;
        Ok(TaskResult {
            values,
            session_references: BTreeMap::from([("helper".into(), id.as_str().into())]),
        })
    }
    async fn run_agent(&self, id: SessionId, context: &mut TaskContext) -> Result<String, Error> {
        let mut updates = self
            .sessions
            .observe_timeline(id.clone())
            .ok_or_else(|| Error::new("unsupported_host", "host cannot observe helper timeline"))?;
        let submission = tokio::select! { biased;
            () = context.cancellation.cancelled() => return Err(Error::cancelled()),
            submission = self.sessions.start_durable_prompt(id, AgentRunLimits::default().with_max_provider_turns(NonZeroU64::new(1).unwrap())) => submission.map_err(|_| Error::new("provider", "helper start failed"))?,
        };
        let run = submission.handle.wait();
        tokio::pin!(run);
        let mut observed = String::new();
        let mut ordinal = 0_u64;
        loop {
            tokio::select! { biased;
                () = context.cancellation.cancelled() => return Err(Error::cancelled()),
                update = updates.next() => {
                    let update = update.ok_or_else(|| Error::new("stream_failed", "helper timeline ended"))?;
                    let SessionTimelineProjectionUpdateEvent::Event(event) = update.event else { return Err(Error::new("stream_failed", "helper timeline reset; no text was silently dropped")); };
                    if event.channel.as_ref().is_none_or(|channel| channel.as_str() != CHANNEL_ASSISTANT_TEXT) { continue; }
                    let delta = match event.content {
                        TimelineContent::TextDelta { delta } => delta,
                        TimelineContent::TextSnapshot { text } => text.strip_prefix(&observed).ok_or_else(|| Error::new("invalid_stream", "helper revised already committed assistant text"))?.to_owned(),
                        _ => continue,
                    };
                    if observed.len().saturating_add(delta.len()) > context.limits.collector_bytes { return Err(Error::new("resource_exhausted", "helper output exceeds byte limit")); }
                    observed.push_str(&delta);
                    if !delta.is_empty() && context.output.state.is_some() {
                        context.output.publish(Chunk::Text(Text { segment: format!("assistant-{ordinal}"), text: delta, replace: false, committed: true })).await?;
                        ordinal += 1;
                    }
                }
                outcome = &mut run => {
                    let final_text = outcome.map_err(|_| Error::new("provider", "helper execution failed"))?.assistant_text.unwrap_or_default();
                    if !final_text.starts_with(&observed) { return Err(Error::new("invalid_stream", "helper final text revised committed output")); }
                    let suffix = &final_text[observed.len()..];
                    if final_text.len() > context.limits.collector_bytes { return Err(Error::new("resource_exhausted", "helper output exceeds byte limit")); }
                    if !suffix.is_empty() && context.output.state.is_some() { context.output.publish(Chunk::Text(Text { segment: format!("assistant-{ordinal}"), text: suffix.into(), replace: false, committed: true })).await?; }
                    if final_text.is_empty() { return Err(Error::new("empty_output", "helper produced no assistant text")); }
                    return Ok(final_text);
                }
            }
        }
    }
}
