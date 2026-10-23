use super::*;
use crate::resource_catalog::routine::{RoutineAudioEncoding, RoutineStreamFormat};
use interaction::{Chunk, Error, Input, Text};
use std::collections::{BTreeSet, VecDeque};
use tokio_util::sync::CancellationToken;
use wire::{interact_request::Event as In, interact_response::Event as Out};

fn invalid(message: impl Into<String>) -> Error {
    Error::new("invalid_stream", message)
}

fn format(value: &wire::InteractionStreamFormat) -> Result<RoutineStreamFormat, Error> {
    match &value.content {
        Some(wire::interaction_stream_format::Content::Text(_)) => Ok(RoutineStreamFormat::Text),
        Some(wire::interaction_stream_format::Content::Audio(v))
            if v.encoding == 1
                && (8000..=48000).contains(&v.sample_rate_hz)
                && (1..=8).contains(&v.channels) =>
        {
            Ok(RoutineStreamFormat::Audio {
                encoding: RoutineAudioEncoding::PcmS16Le,
                sample_rate_hz: v.sample_rate_hz,
                channels: v.channels,
            })
        }
        _ => Err(invalid("invalid stream format")),
    }
}

fn wire_format(value: RoutineStreamFormat) -> wire::InteractionStreamFormat {
    wire::InteractionStreamFormat {
        content: Some(match value {
            RoutineStreamFormat::Text => {
                wire::interaction_stream_format::Content::Text(wire::InteractionTextFormat {})
            }
            RoutineStreamFormat::Audio {
                sample_rate_hz,
                channels,
                ..
            } => wire::interaction_stream_format::Content::Audio(wire::InteractionAudioFormat {
                encoding: 1,
                sample_rate_hz,
                channels,
            }),
        }),
    }
}

struct Opened {
    port: String,
    closed: bool,
}

pub(super) async fn read_inputs(
    requests: InputRequests,
    inputs: BTreeMap<String, Input>,
    trigger: &RoutineInteractionTrigger,
    limits: &InteractionLimits,
    cancel: &CancellationToken,
    input_finished: &tokio::sync::watch::Sender<bool>,
    on_failure: impl Fn(&Error),
) -> Result<(), Error> {
    let (requests, streamed_bytes) = match requests {
        InputRequests::Native(requests, bytes) => (requests, Some(bytes)),
        InputRequests::Complete(bodies) => {
            return feed_complete(
                bodies,
                inputs,
                trigger,
                limits,
                cancel,
                input_finished,
                on_failure,
            )
            .await;
        }
    };
    let mut ingress = super::ingress::Ingress::new(inputs, limits, cancel, false);
    let result = dispatch_inputs(
        requests,
        &mut ingress,
        trigger,
        limits,
        cancel,
        input_finished,
        streamed_bytes,
    )
    .await;
    if let Err(error) = &result {
        // Input validation/enqueue failures already belong to the runtime's failure recorder.
        if !ingress.owns_failure(error) {
            on_failure(error);
        }
    }
    ingress.shutdown().await;
    result
}

async fn dispatch_inputs(
    mut requests: Requests,
    ingress: &mut super::ingress::Ingress,
    trigger: &RoutineInteractionTrigger,
    limits: &InteractionLimits,
    cancel: &CancellationToken,
    input_finished: &tokio::sync::watch::Sender<bool>,
    mut streamed_bytes: Option<usize>,
) -> Result<(), Error> {
    let mut eof = false;
    let mut streams = BTreeMap::<String, Opened>::new();
    let mut ports = BTreeSet::new();
    let mut complete = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(limits.input_seconds);
    loop {
        if complete && ingress.tasks.is_empty() {
            input_finished.send_replace(true);
            if eof {
                return Ok(());
            }
        }
        let next = tokio::select! { biased;
            () = cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep_until(deadline), if !complete => return Err(Error::new("deadline_exceeded", "interaction input deadline exceeded")),
            result = ingress.tasks.join_next(), if !ingress.tasks.is_empty() => {
                result.expect("input dispatcher task").map_err(|_| Error::new("stream_failed", "input dispatcher task failed"))??;
                continue;
            }
            next = requests.next(), if !eof => next,
        };
        let Some(next) = next else {
            if !complete {
                finish_inputs(ingress, &streams, &ports, trigger)?;
                complete = true;
            }
            eof = true;
            continue;
        };
        let next =
            next.map_err(|_| Error::new("cancelled", "interaction transport disconnected"))?;
        if let Some(bytes) = streamed_bytes.as_mut() {
            *bytes = bytes
                .checked_add(next.encoded_len())
                .filter(|v| *v <= limits.complete_request_bytes)
                .ok_or_else(|| {
                    Error::new("resource_exhausted", "streamed request exceeds byte limit")
                })?;
        }
        let event = next
            .event
            .ok_or_else(|| invalid("input event is required"))?;
        if matches!(event, In::Cancel(_)) {
            return Err(Error::new("cancelled", "interaction cancelled by client"));
        }
        if complete {
            return Err(invalid("input event follows input completion"));
        }
        match event {
            In::Open(_) => return Err(invalid("interaction open may occur only first")),
            In::InputOpened(v) => {
                if v.stream_id.is_empty()
                    || v.stream_id.len() > 128
                    || streams.contains_key(&v.stream_id)
                    || ports.contains(&v.port_name)
                {
                    return Err(invalid("duplicate or invalid input stream or port"));
                }
                let declared = trigger
                    .input_ports
                    .iter()
                    .find(|p| p.name == v.port_name)
                    .ok_or_else(|| invalid("input port is not declared"))?;
                if format(
                    v.format
                        .as_ref()
                        .ok_or_else(|| invalid("input format is required"))?,
                )? != declared.format
                {
                    return Err(invalid("input format differs from declared port"));
                }
                ports.insert(v.port_name.clone());
                streams.insert(
                    v.stream_id,
                    Opened {
                        port: v.port_name,
                        closed: false,
                    },
                );
            }
            In::InputChunk(v) => {
                let stream = streams
                    .get(&v.stream_id)
                    .filter(|s| !s.closed)
                    .ok_or_else(|| invalid("chunk requires an open stream"))?;
                let chunk = match v.content {
                    Some(wire::interaction_input_chunk::Content::Audio(v)) => Chunk::Audio(v),
                    Some(wire::interaction_input_chunk::Content::Text(v))
                        if v.operation == 1 || v.operation == 2 =>
                    {
                        Chunk::Text(Text {
                            segment: v.segment_id,
                            text: v.text,
                            replace: v.operation == 2,
                            committed: v.committed,
                        })
                    }
                    _ => return Err(invalid("invalid chunk content or text operation")),
                };
                ingress.send(&stream.port, chunk, limits, deadline).await?;
            }
            In::InputClosed(v) => {
                let stream = streams
                    .get_mut(&v.stream_id)
                    .filter(|s| !s.closed)
                    .ok_or_else(|| invalid("close requires an open stream"))?;
                if !matches!(v.status, 1..=3) || (v.status == 3) != v.failure.is_some() {
                    return Err(invalid("close status and failure presence disagree"));
                }
                if v.status == 3 {
                    return Err(Error::new("input_failed", "client input stream failed"));
                }
                if v.status == 2 {
                    return Err(Error::new("cancelled", "input stream cancelled by client"));
                }
                stream.closed = true;
                // Closing a port is independent of another port's blocked data send.
                ingress.close(&stream.port);
            }
            In::InputCompleted(_) => {
                finish_inputs(ingress, &streams, &ports, trigger)?;
                complete = true;
            }
            In::Cancel(_) => unreachable!(),
        }
    }
}

fn finish_inputs(
    ingress: &mut super::ingress::Ingress,
    streams: &BTreeMap<String, Opened>,
    ports: &BTreeSet<String>,
    trigger: &RoutineInteractionTrigger,
) -> Result<(), Error> {
    if streams.values().any(|s| !s.closed) {
        return Err(invalid(
            "input completion requires every opened stream closed",
        ));
    }
    if trigger
        .input_ports
        .iter()
        .any(|p| p.required && !ports.contains(&p.name))
    {
        return Err(invalid("required input port was never opened"));
    }
    ingress.close_remaining();
    Ok(())
}

pub(super) fn complete_inputs(
    trigger: &RoutineInteractionTrigger,
    inputs: Vec<wire::InteractionInput>,
    limits: &InteractionLimits,
) -> Result<Vec<wire::InteractionInput>, String> {
    let mut ports = BTreeSet::new();
    for input in &inputs {
        if !ports.insert(input.port_name.clone()) {
            return Err("duplicate input port".into());
        }
        let declared = trigger
            .input_ports
            .iter()
            .find(|v| v.name == input.port_name)
            .ok_or("undeclared input port")?;
        match (&declared.format, &input.content) {
            (RoutineStreamFormat::Text, Some(wire::interaction_input::Content::Text(_)))
                if limits.chunk_bytes >= 5 =>
            {
                ()
            }
            (
                RoutineStreamFormat::Audio { channels, .. },
                Some(wire::interaction_input::Content::Audio(audio)),
            ) => {
                let audio_format = wire::InteractionStreamFormat {
                    content: audio
                        .format
                        .clone()
                        .map(wire::interaction_stream_format::Content::Audio),
                };
                if format(&audio_format).map_err(|v| v.message)? != declared.format
                    || audio.data.len() % (*channels as usize * 2) != 0
                    || limits.chunk_bytes < *channels as usize * 2
                {
                    return Err("audio format, frame alignment, or chunk limit is invalid".into());
                }
            }
            _ => return Err("input content differs from declared format".into()),
        }
    }
    if trigger
        .input_ports
        .iter()
        .any(|p| p.required && !ports.contains(&p.name))
    {
        return Err("required input port is missing".into());
    }
    Ok(inputs)
}

async fn feed_complete(
    bodies: Vec<wire::InteractionInput>,
    inputs: BTreeMap<String, Input>,
    trigger: &RoutineInteractionTrigger,
    limits: &InteractionLimits,
    cancel: &CancellationToken,
    input_finished: &tokio::sync::watch::Sender<bool>,
    on_failure: impl Fn(&Error),
) -> Result<(), Error> {
    let mut bodies: BTreeMap<_, _> = bodies
        .into_iter()
        .map(|body| (body.port_name.clone(), body))
        .collect();
    let token = cancel.child_token();
    let mut tasks = tokio::task::JoinSet::new();
    for (port, mut input) in inputs {
        let body = bodies.remove(&port);
        let format = trigger
            .input_ports
            .iter()
            .find(|p| p.name == port)
            .expect("validated complete port")
            .format
            .clone();
        let chunk = limits.chunk_bytes;
        let worker_cancel = token.clone();
        tasks.spawn(async move {
            let feed = async {
                if let Some(body) = body {
                    // Move the original bounded body into its owner; copy only the current chunk.
                    let frames = CompleteInputs {
                        inputs: VecDeque::from([body]),
                        current: None,
                        offset: 0,
                        stage: 0,
                        done: false,
                        formats: BTreeMap::from([(port, format)]),
                        chunk,
                    };
                    for frame in frames {
                        let Some(In::InputChunk(frame)) =
                            frame.expect("validated complete frame").event
                        else {
                            continue;
                        };
                        let chunk = match frame.content.expect("validated complete content") {
                            wire::interaction_input_chunk::Content::Audio(data) => {
                                Chunk::Audio(data)
                            }
                            wire::interaction_input_chunk::Content::Text(text) => {
                                Chunk::Text(Text {
                                    segment: text.segment_id,
                                    text: text.text,
                                    replace: false,
                                    committed: text.committed,
                                })
                            }
                        };
                        input.send_complete(chunk).await?;
                    }
                }
                input.close().await
            };
            tokio::select! { biased;
                () = worker_cancel.cancelled() => Ok(()),
                result = feed => result,
            }
        });
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(limits.input_seconds);
    let result = loop {
        if tasks.is_empty() {
            input_finished.send_replace(true);
            break Ok(());
        }
        tokio::select! { biased;
            () = cancel.cancelled() => break Ok(()),
            () = tokio::time::sleep_until(deadline) => {
                let error = Error::new("deadline_exceeded", "interaction input deadline exceeded");
                on_failure(&error);
                break Err(error);
            }
            result = tasks.join_next() => match result.expect("complete feeder task") {
                Ok(Ok(())) => {},
                // Input errors are already owned by the runtime failure recorder.
                Ok(Err(error)) => break Err(error),
                Err(_) => {
                    let error = Error::new("stream_failed", "complete input feeder task failed");
                    on_failure(&error);
                    break Err(error);
                }
            },
        }
    };
    token.cancel();
    while tasks.join_next().await.is_some() {}
    result
}

struct CompleteInputs {
    inputs: VecDeque<wire::InteractionInput>,
    current: Option<wire::InteractionInput>,
    offset: usize,
    stage: u8,
    done: bool,
    formats: BTreeMap<String, RoutineStreamFormat>,
    chunk: usize,
}
impl Iterator for CompleteInputs {
    type Item = Result<wire::InteractRequest, Status>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if self.current.is_none() {
            self.current = self.inputs.pop_front();
            self.offset = 0;
            self.stage = 0;
            if self.current.is_none() {
                self.done = true;
                return Some(Ok(wire::InteractRequest {
                    event: Some(In::InputCompleted(wire::InteractionInputCompleted {})),
                }));
            }
        }
        let input = self.current.as_ref().expect("complete input");
        let id = input.port_name.clone();
        let event = if self.stage == 0 {
            self.stage = if matches!(input.content.as_ref(), Some(wire::interaction_input::Content::Audio(audio)) if audio.data.is_empty())
            {
                2
            } else {
                1
            };
            In::InputOpened(wire::InteractionInputOpened {
                port_name: input.port_name.clone(),
                stream_id: id,
                format: Some(wire_format(self.formats[&input.port_name].clone())),
            })
        } else if self.stage == 1 {
            let content = match input.content.as_ref().expect("validated content") {
                wire::interaction_input::Content::Audio(audio) => {
                    let channels = match self.formats[&input.port_name] {
                        RoutineStreamFormat::Audio { channels, .. } => channels as usize,
                        _ => unreachable!(),
                    };
                    let chunk = self.chunk / (channels * 2) * (channels * 2);
                    let end = self.offset.saturating_add(chunk).min(audio.data.len());
                    let data = audio.data[self.offset..end].to_vec();
                    self.offset = end;
                    if end == audio.data.len() {
                        self.stage = 2;
                    }
                    wire::interaction_input_chunk::Content::Audio(data)
                }
                wire::interaction_input::Content::Text(text) => {
                    let mut end = self.offset.saturating_add(self.chunk - 1).min(text.len());
                    while !text.is_char_boundary(end) {
                        end -= 1;
                    }
                    // UTF-8 scalars need up to four bytes; this was checked before admission.
                    let data = text[self.offset..end].to_owned();
                    self.offset = end;
                    if end == text.len() {
                        self.stage = 2;
                    }
                    wire::interaction_input_chunk::Content::Text(wire::TextUpdate {
                        segment_id: "s".into(),
                        operation: 1,
                        text: data,
                        committed: self.stage == 2,
                    })
                }
            };
            In::InputChunk(wire::InteractionInputChunk {
                stream_id: id,
                content: Some(content),
            })
        } else {
            self.current = None;
            In::InputClosed(wire::InteractionInputClosed {
                stream_id: id,
                status: 1,
                failure: None,
            })
        };
        Some(Ok(wire::InteractRequest { event: Some(event) }))
    }
}

pub(super) fn completed(
    result: Result<Vec<routine::RoutineInvocationResult>, Error>,
) -> wire::InteractResponse {
    let (status, failure) = match result {
        Ok(_) => (wire::RoutineInvocationStatus::Succeeded, None),
        Err(error) => (
            if error.code == "cancelled" {
                wire::RoutineInvocationStatus::Interrupted
            } else {
                wire::RoutineInvocationStatus::Failed
            },
            Some(wire::RoutineFailure {
                code: error.code.into(),
                message: error.message,
            }),
        ),
    };
    response(Out::Completed(wire::InteractionCompleted {
        status: status as i32,
        failure,
    }))
}

pub(super) fn output(
    event: Event,
    ports: &mut BTreeMap<String, String>,
    failure: Option<Error>,
) -> Option<wire::InteractResponse> {
    Some(match event {
        Event::Opened { port, format } => {
            let id = format!("out_{}", uuid::Uuid::new_v4().simple());
            ports.insert(port.clone(), id.clone());
            response(Out::OutputOpened(wire::InteractionOutputOpened {
                port_name: port,
                stream_id: id,
                format: Some(wire_format(format)),
            }))
        }
        Event::Chunk { port, chunk } => {
            let id = ports.get(&port)?.clone();
            let content = match chunk {
                Chunk::Audio(v) => wire::interaction_output_chunk::Content::Audio(v),
                Chunk::Text(v) => wire::interaction_output_chunk::Content::Text(wire::TextUpdate {
                    segment_id: v.segment,
                    operation: if v.replace { 2 } else { 1 },
                    text: v.text,
                    committed: v.committed,
                }),
            };
            response(Out::OutputChunk(wire::InteractionOutputChunk {
                stream_id: id,
                content: Some(content),
            }))
        }
        Event::Closed { port, error } => {
            let id = ports.remove(&port)?;
            let error = failure.or(error);
            let status = match &error {
                None => 1,
                Some(v) if v.code == "cancelled" => 2,
                Some(_) => 3,
            };
            response(Out::OutputClosed(wire::InteractionOutputClosed {
                stream_id: id,
                status,
                failure: error.filter(|_| status == 3).map(|v| wire::RoutineFailure {
                    code: v.code.into(),
                    message: v.message,
                }),
            }))
        }
        Event::Completed(result) => completed(match failure {
            Some(error) => Err(error),
            None => result,
        }),
    })
}
