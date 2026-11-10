use crate::interaction::browser::{MediaOutputBuffer, OutputLifecycle, PendingAdmission};
use crate::interaction::transfer::owned::{Download, DownloadDeadline, Stop};
use crate::interaction::{
    AttachmentControl, AttachmentInput, AttachmentSession, BinaryOutputGrant, BlobUpload,
    DurableTextReader, InteractionEvent, InteractionLimits, wire,
};
use crate::{
    ClientManualRoutineDefinition, RoutineInteractionTransport, RoutineJson, RoutineLimits,
};
use futures_channel::oneshot;
use futures_util::{
    FutureExt, StreamExt,
    future::{Either, LocalBoxFuture, Shared, select},
    lock::Mutex,
};
use serde::{Deserialize, Serialize};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
};
use wasm_bindgen::prelude::*;

type Join = Shared<LocalBoxFuture<'static, bool>>;
#[wasm_bindgen(inline_js = "export function mediaMonotonicMs() { return performance.now(); }")]
extern "C" {
    #[wasm_bindgen(js_name = mediaMonotonicMs)]
    fn monotonic_ms() -> f64;
}
async fn send(
    input: &Rc<Mutex<AttachmentInput>>,
    event: wire::input_event::Event,
) -> Result<(), JsValue> {
    input
        .try_lock()
        .ok_or_else(|| js("media producer busy"))?
        .send(vec![wire::InputEvent { event: Some(event) }])
        .await
        .map_err(super::js_error)
}
enum OwnerStep {
    Output(Option<Result<InteractionEvent, crate::ClientError>>),
    Cancel,
    Tick,
    Renew(Result<(), crate::ClientError>),
}
async fn close_owner(control: &AttachmentControl) -> bool {
    let close = control.cancel_and_join();
    let timeout = super::sleep_ms(30000);
    futures_util::pin_mut!(close, timeout);
    match select(close, timeout).await {
        Either::Left((Ok(proof), _)) => proof,
        _ => false,
    }
}
struct Active {
    id: u32,
    input: Option<Rc<Mutex<AttachmentInput>>>,
    cancel: Stop,
    join: Join,
    uploads: Rc<Mutex<BTreeMap<String, BlobUpload>>>,
}
#[derive(Default)]
pub(super) struct Host {
    text_queries: Rc<Mutex<BTreeMap<u32, DurableTextReader>>>,
    query_ids: std::collections::BTreeSet<u32>,
    pub disposed: bool,
    next: u32,
    pending: Rc<Cell<Option<u32>>>,
    active: Option<Active>,
    status: Option<Status>,
    retained: Option<Retention>,
    downloads: BTreeMap<(u32, String), Download>,
}
struct Retention {
    id: u32,
    events: Rc<RefCell<MediaOutputBuffer>>,
    grants: BTreeMap<String, (BinaryOutputGrant, f64)>,
    reader_limit: u32,
}
impl Host {
    pub fn cancel(&mut self) {
        self.pending.set(None);
        self.retained = None;
        self.status = None;
        self.text_queries = Rc::new(Mutex::new(BTreeMap::new()));
        self.query_ids.clear();
        for download in self.downloads.values() {
            download.stop.cancel();
        }
        if let Some(a) = &self.active {
            a.cancel.cancel();
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        self.cancel();
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Metadata {
    routine: String,
    revision: String,
    entrypoint: String,
    parameters: RoutineJson,
    correlation: String,
    source_session: Option<String>,
    inputs: Vec<Declaration>,
    selected_text_output: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TextInputRevision {
    segment: String,
    text: String,
    replace: bool,
    committed: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PcmFormat {
    sample_rate: u32,
    channels: u32,
}
#[derive(Deserialize)]
#[serde(tag = "format", rename_all = "camelCase", deny_unknown_fields)]
enum Declaration {
    Pcm16le {
        port: String,
        #[serde(rename = "sampleRate")]
        sample_rate: u32,
        channels: u32,
    },
    Text {
        port: String,
    },
    File {
        port: String,
        #[serde(rename = "displayName")]
        display_name: String,
        #[serde(rename = "contentType")]
        content_type: String,
        #[serde(rename = "declaredBytes")]
        declared_bytes: u32,
        #[serde(rename = "expectedSha256")]
        expected_sha256: Option<Vec<u8>>,
    },
    Image {
        port: String,
        #[serde(rename = "displayName")]
        display_name: String,
        #[serde(rename = "contentType")]
        content_type: String,
        #[serde(rename = "declaredBytes")]
        declared_bytes: u32,
        #[serde(rename = "expectedSha256")]
        expected_sha256: Option<Vec<u8>>,
        image: String,
    },
}
impl Declaration {
    fn wire(self) -> Result<wire::InputDeclaration, JsValue> {
        use wire::{binary_input_declaration::Purpose, input_declaration::Format};
        let (port, format) = match self {
            Self::Pcm16le {
                port,
                sample_rate,
                channels,
            } => (
                port,
                Format::Pcm16le(wire::Pcm16Le {
                    sample_rate,
                    channels,
                }),
            ),
            Self::Text { port } => (port, Format::Text(wire::TextFormat {})),
            Self::File {
                port,
                display_name,
                content_type,
                declared_bytes,
                expected_sha256,
            } => (
                port,
                Format::Binary(wire::BinaryInputDeclaration {
                    display_name,
                    content_type,
                    declared_bytes: Some(u64::from(declared_bytes)),
                    expected_sha256,
                    purpose: Some(Purpose::File(wire::FilePurpose {})),
                }),
            ),
            Self::Image {
                port,
                display_name,
                content_type,
                declared_bytes,
                expected_sha256,
                image,
            } => {
                let image = match image.as_str() {
                    "png" => wire::ImageFormat::Png,
                    "jpeg" => wire::ImageFormat::Jpeg,
                    "webp" => wire::ImageFormat::Webp,
                    _ => return Err(js("invalid image purpose")),
                };
                (
                    port,
                    Format::Binary(wire::BinaryInputDeclaration {
                        display_name,
                        content_type,
                        declared_bytes: Some(u64::from(declared_bytes)),
                        expected_sha256,
                        purpose: Some(Purpose::Image(image as i32)),
                    }),
                )
            }
        };
        Ok(wire::InputDeclaration {
            port,
            format: Some(format),
        })
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    operation_id: u32,
    phase: &'static str,
    preview: String,
    final_text: Option<String>,
    cleanup_verified: bool,
}
fn js(s: &str) -> JsValue {
    JsValue::from_str(s)
}
use super::WaltrClient;

impl WaltrClient {
    fn active_input(&self, id: u32) -> Result<Rc<Mutex<AttachmentInput>>, JsValue> {
        self.interactions
            .borrow()
            .active
            .as_ref()
            .filter(|a| a.id == id)
            .and_then(|a| a.input.clone())
            .ok_or_else(|| js("stale media operation"))
    }
}
#[wasm_bindgen]
#[allow(tail_expr_drop_order)]
impl WaltrClient {
    /// Opens finite declarations, not a complete recording or upload.
    #[wasm_bindgen(js_name=openMediaInteraction)]
    pub async fn open_media_interaction(&self, metadata_json: String) -> Result<u32, JsValue> {
        if metadata_json.len() > 65536 {
            return Err(js("media metadata limit"));
        }
        let meta: Metadata = serde_json::from_str(&metadata_json).map_err(super::js_error)?;
        let snapshot: serde_json::Value =
            serde_json::from_str(&self.latest_snapshot.borrow()).map_err(super::js_error)?;
        let catalog = &snapshot["routines"]["definitions"];
        let definitions: Vec<ClientManualRoutineDefinition> =
            serde_json::from_value(catalog["definitions"].clone()).map_err(super::js_error)?;
        let definition = definitions
            .into_iter()
            .find(|d| d.routine_name == meta.routine && d.routine_revision == meta.revision)
            .ok_or_else(|| js("stale Routine selection"))?;
        let server: RoutineLimits =
            serde_json::from_value(catalog["limits"].clone()).map_err(super::js_error)?;
        let limits = InteractionLimits::for_definition(definition, server.into())
            .map_err(super::js_error)?;
        let open = wire::InteractionOpen {
            selection: Some(wire::InvocationSelection {
                routine: meta.routine,
                revision: meta.revision,
                entrypoint: meta.entrypoint,
                parameters: Some(wire::StructuredValue {
                    json: meta.parameters.as_str().as_bytes().to_vec(),
                }),
                correlation: meta.correlation,
                source_session: meta.source_session,
            }),
            inputs: meta
                .inputs
                .into_iter()
                .map(Declaration::wire)
                .collect::<Result<_, _>>()?,
        };
        limits.validate_open(&open).map_err(super::js_error)?;
        let admission_by = monotonic_ms() + 20000.0;
        let id = {
            let mut host = self.interactions.borrow_mut();
            if host.disposed || host.active.is_some() || host.pending.get().is_some() {
                return Err(js("media owner unavailable or busy"));
            }
            host.next = host
                .next
                .checked_add(1)
                .ok_or_else(|| js("media operation limit"))?;
            host.next
        };
        let pending = PendingAdmission::reserve(self.interactions.borrow().pending.clone(), id)
            .map_err(super::js_error)?;
        let renewal_interval = (limits.server.attachment_lease_ms / 3).clamp(1, 5000) as u32;
        let tick_ms = renewal_interval.min(1000);
        let reader_limit = limits.server.blob_readers;
        let cancel = Stop::default();
        let (joined, receiver) = oneshot::channel();
        let join = receiver.map(|r| r.unwrap_or(false)).boxed_local().shared();
        let active = Active {
            id,
            input: None,
            cancel: cancel.clone(),
            join: join.clone(),
            uploads: Rc::new(Mutex::new(BTreeMap::new())),
        };
        {
            let mut h = self.interactions.borrow_mut();
            h.active = Some(active);
            h.status = Some(Status {
                operation_id: id,
                phase: "opening",
                preview: String::new(),
                final_text: None,
                cleanup_verified: false,
            });
        }
        let events = Rc::new(RefCell::new(MediaOutputBuffer::default()));
        self.interactions.borrow_mut().retained = Some(Retention {
            id,
            events: events.clone(),
            grants: BTreeMap::new(),
            reader_limit,
        });
        let weak = Rc::downgrade(&self.interactions);
        let transfers = self.interaction_transport.media_transfers();
        wasm_bindgen_futures::spawn_local(async move {
            let mut admission_deadline = Box::pin(async move {
                let remaining = (admission_by - monotonic_ms()).clamp(0.0, 20000.0) as u32;
                if remaining > 0 {
                    super::sleep_ms(remaining).await;
                }
            });
            let authority = Rc::new(RefCell::new(None));
            let observed = authority.clone();
            let opening = AttachmentSession::open_observed(
                open,
                limits,
                transfers.rpc,
                transfers.stream,
                Some(Rc::new(move |control| {
                    *observed.borrow_mut() = Some(control)
                })),
            );
            let session = cancel
                .race(admission_deadline.as_mut(), opening)
                .await
                .and_then(|r| r);
            let session = match session {
                Ok(session) if pending.current() && !cancel.is_cancelled() => session,
                result => {
                    if let Ok(session) = result {
                        *authority.borrow_mut() = Some(session.control.clone());
                        drop(session);
                    }
                    let control = authority.borrow_mut().take();
                    let verified = match control {
                        Some(control) => close_owner(&control).await,
                        None => false,
                    };
                    drop(pending);
                    if let Some(host) = weak.upgrade() {
                        let mut host = host.borrow_mut();
                        if host.active.as_ref().is_some_and(|a| a.id == id) {
                            host.active = None;
                            if let Some(status) =
                                host.status.as_mut().filter(|s| s.operation_id == id)
                            {
                                status.phase = "error";
                                status.cleanup_verified = verified;
                            }
                        }
                    }
                    let _joined = joined.send(verified);
                    return;
                }
            };
            drop(pending);
            let mut output = Some(session.output);
            let control = session.control;
            if let Some(host) = weak.upgrade()
                && let Some(active) = host.borrow_mut().active.as_mut().filter(|a| a.id == id)
            {
                active.input = Some(Rc::new(Mutex::new(session.input)));
            }
            let mut cancelled_owner = false;
            let mut remote_cleanup = false;
            let mut accepted = false;
            let mut lifecycle = OutputLifecycle::default();
            let mut last_renew = 0u32;
            let mut renewal: Option<LocalBoxFuture<'static, Result<(), crate::ClientError>>> = None;
            let mut tick = Box::pin(super::sleep_ms(tick_ms));
            loop {
                let step = if control.is_cancelled()
                    || cancel.is_cancelled()
                    || (!accepted && admission_deadline.as_mut().now_or_never().is_some())
                {
                    OwnerStep::Cancel
                } else if tick.as_mut().now_or_never().is_some() {
                    OwnerStep::Tick
                } else if let Some(result) =
                    renewal.as_mut().and_then(|r| r.as_mut().now_or_never())
                {
                    OwnerStep::Renew(result)
                } else {
                    let next = output.as_mut().unwrap().next();
                    let stop = cancel.cancelled();
                    futures_util::pin_mut!(next, stop);
                    let renew = match renewal.as_mut() {
                        Some(r) => Either::Left(r.as_mut()),
                        None => Either::Right(futures_util::future::pending()),
                    };
                    futures_util::pin_mut!(renew);
                    let admission = if accepted {
                        Either::Left(futures_util::future::pending())
                    } else {
                        Either::Right(admission_deadline.as_mut())
                    };
                    futures_util::pin_mut!(admission);
                    let wait = select(select(next, stop), select(tick.as_mut(), renew));
                    futures_util::pin_mut!(wait);
                    match select(admission, wait).await {
                        Either::Left(_) => OwnerStep::Cancel,
                        Either::Right((step, _)) => match step {
                            Either::Left((Either::Left((event, _)), _)) => OwnerStep::Output(event),
                            Either::Left((Either::Right(_), _)) => OwnerStep::Cancel,
                            Either::Right((Either::Left(_), _)) => OwnerStep::Tick,
                            Either::Right((Either::Right((result, _)), _)) => {
                                OwnerStep::Renew(result)
                            }
                        },
                    }
                };
                match step {
                    OwnerStep::Output(Some(Ok(event))) => {
                        let projected = match event {
                            InteractionEvent::Accepted { .. } => {
                                accepted = true;
                                serde_json::Value::Null
                            }
                            InteractionEvent::Open(o) => {
                                serde_json::json!({"type":"open","port":o.port,"streamId":o.stream_id,"format":match o.format { Some(wire::stream_open::Format::Pcm16le(p))=>serde_json::json!({"type":"pcm16le","sampleRate":p.sample_rate,"channels":p.channels}),Some(wire::stream_open::Format::Text(_))=>serde_json::json!({"type":"text"}),None=>serde_json::Value::Null }})
                            }
                            InteractionEvent::Chunk(c) => match c.payload {
                                Some(wire::stream_chunk::Payload::Audio(data)) => {
                                    let stored =
                                        events.borrow_mut().audio(c.stream_id, c.sequence, data);
                                    if stored.is_err() {
                                        cancelled_owner = true;
                                        drop(output.take());
                                        remote_cleanup = close_owner(&control).await;
                                        break;
                                    }
                                    serde_json::Value::Null
                                }
                                _ => serde_json::Value::Null,
                            },
                            InteractionEvent::Close(c) => {
                                serde_json::json!({"type":"close","streamId":c.stream_id,"nextSequence":c.next_sequence.to_string()})
                            }
                            InteractionEvent::Blob { port, descriptor } => {
                                if let Some(grant) = output.as_mut().unwrap().take_blob_grant(&port)
                                    && let Some(h) = weak.upgrade()
                                    && let Some(r) =
                                        h.borrow_mut().retained.as_mut().filter(|r| r.id == id)
                                {
                                    let remaining =
                                        descriptor.remaining_validity_ms.map_or(0, |ms| ms.0);
                                    r.grants.insert(
                                        port.clone(),
                                        (grant, monotonic_ms() + remaining as f64),
                                    );
                                }
                                serde_json::json!({"type":"blob","port":port,"descriptor":descriptor})
                            }
                            InteractionEvent::Terminal { .. } => {
                                let now = super::utc_ms() as i64;
                                let absolute = control.deadlines().2;
                                lifecycle.terminal(now, absolute);
                                tick = Box::pin(super::sleep_ms(
                                    absolute.saturating_sub(now).clamp(0, 30000) as u32,
                                ));
                                renewal = None;
                                serde_json::Value::Null
                            }
                            _ => serde_json::Value::Null,
                        };
                        if !projected.is_null() {
                            let stored = events.borrow_mut().push(projected);
                            if stored.is_err() {
                                cancelled_owner = true;
                                drop(output.take());
                                remote_cleanup = close_owner(&control).await;
                                break;
                            }
                        }
                        let preview = meta
                            .selected_text_output
                            .as_deref()
                            .and_then(|p| output.as_ref().unwrap().preview_text(p))
                            .unwrap_or_default();
                        if let Some(h) = weak.upgrade() {
                            let mut h = h.borrow_mut();
                            if let Some(s) = h.status.as_mut().filter(|s| s.operation_id == id) {
                                s.phase = "live";
                                s.preview = preview;
                            }
                        }
                    }
                    OwnerStep::Output(Some(Err(_))) => {
                        cancelled_owner = true;
                        drop(output.take());
                        remote_cleanup = close_owner(&control).await;
                        break;
                    }
                    OwnerStep::Output(None) => break,
                    OwnerStep::Cancel => {
                        cancelled_owner = true;
                        drop(output.take());
                        remote_cleanup = close_owner(&control).await;
                        break;
                    }
                    OwnerStep::Tick => {
                        tick = Box::pin(super::sleep_ms(tick_ms));
                        let now = super::utc_ms() as i64;
                        if lifecycle.expired(now, control.deadlines(), accepted) {
                            cancelled_owner = true;
                            drop(output.take());
                            remote_cleanup = close_owner(&control).await;
                            break;
                        }
                        last_renew = last_renew.saturating_add(tick_ms);
                        if !lifecycle.draining()
                            && last_renew >= renewal_interval
                            && renewal.is_none()
                            && accepted
                        {
                            last_renew = 0;
                            let c = control.clone();
                            renewal = Some(async move { c.renew().await }.boxed_local());
                        }
                    }
                    OwnerStep::Renew(result) => {
                        renewal = None;
                        if result.is_err() && !lifecycle.draining() {
                            cancelled_owner = true;
                            drop(output.take());
                            remote_cleanup = close_owner(&control).await;
                            break;
                        }
                    }
                }
            }
            // The join sender runs only after this owner drops its output stream.
            let clean = output.as_ref().is_some_and(|o| o.cleanup_verified());
            let text = if cancelled_owner {
                None
            } else {
                meta.selected_text_output
                    .as_deref()
                    .and_then(|p| output.as_ref().and_then(|o| o.final_text(p).ok()))
            };
            if let Some(h) = weak.upgrade() {
                let mut h = h.borrow_mut();
                if h.active.as_ref().is_some_and(|a| a.id == id) {
                    h.active = None;
                    if h.retained.as_ref().is_some_and(|r| r.id == id) {
                        h.status = Some(Status {
                            operation_id: id,
                            phase: if output.as_ref().is_some_and(|o| o.succeeded())
                                && !cancelled_owner
                            {
                                "succeeded"
                            } else {
                                "error"
                            },
                            preview: if text.is_some() {
                                text.clone().unwrap()
                            } else {
                                String::new()
                            },
                            final_text: text,
                            cleanup_verified: clean || remote_cleanup,
                        });
                    }
                }
            }
            drop(output);
            let _sent = joined.send(clean || remote_cleanup);
        });
        Ok(id)
    }
    #[wasm_bindgen(js_name=openMediaPcmStream)]
    pub async fn open_media_pcm_stream(
        &self,
        id: u32,
        port: String,
        stream_id: String,
        format_json: String,
    ) -> Result<(), JsValue> {
        let PcmFormat {
            sample_rate,
            channels,
        } = serde_json::from_str(&format_json).map_err(|_| js("invalid PCM format"))?;
        let input = self.active_input(id)?;
        input
            .try_lock()
            .ok_or_else(|| js("media producer busy"))?
            .send(vec![wire::InputEvent {
                event: Some(wire::input_event::Event::Open(wire::StreamOpen {
                    port,
                    stream_id,
                    format: Some(wire::stream_open::Format::Pcm16le(wire::Pcm16Le {
                        sample_rate,
                        channels,
                    })),
                })),
            }])
            .await
            .map_err(super::js_error)
    }
    #[wasm_bindgen(js_name=pushMediaPcm)]
    pub async fn push_media_pcm(
        &self,
        id: u32,
        stream_id: String,
        sequence: u32,
        data: &[u8],
    ) -> Result<(), JsValue> {
        if data.is_empty() || data.len() > 65536 {
            return Err(js("PCM chunk limit"));
        }
        let input = self.active_input(id)?;
        input
            .try_lock()
            .ok_or_else(|| js("media producer busy"))?
            .send(vec![wire::InputEvent {
                event: Some(wire::input_event::Event::Chunk(wire::StreamChunk {
                    stream_id,
                    sequence: u64::from(sequence),
                    payload: Some(wire::stream_chunk::Payload::Audio(data.to_vec())),
                })),
            }])
            .await
            .map_err(super::js_error)
    }
    #[wasm_bindgen(js_name=openMediaTextStream)]
    pub async fn open_media_text_stream(
        &self,
        id: u32,
        port: String,
        stream: String,
    ) -> Result<(), JsValue> {
        send(
            &self.active_input(id)?,
            wire::input_event::Event::Open(wire::StreamOpen {
                port,
                stream_id: stream,
                format: Some(wire::stream_open::Format::Text(wire::TextFormat {})),
            }),
        )
        .await
    }
    #[wasm_bindgen(js_name=pushMediaTextRevision)]
    pub async fn push_media_text_revision(
        &self,
        id: u32,
        stream: String,
        sequence: String,
        revision: String,
    ) -> Result<(), JsValue> {
        let input = self.active_input(id)?;
        let mut producer = input.try_lock().ok_or_else(|| js("media producer busy"))?;
        let seq = sequence
            .parse::<u64>()
            .map_err(|_| js("invalid sequence"))?;
        if seq.to_string() != sequence {
            return Err(js("invalid sequence"));
        }
        if revision.len() > 131072 {
            return Err(js("text revision too large"));
        }
        let r: TextInputRevision =
            serde_json::from_str(&revision).map_err(|_| js("invalid text revision"))?;
        producer
            .send(vec![wire::InputEvent {
                event: Some(wire::input_event::Event::Chunk(wire::StreamChunk {
                    stream_id: stream,
                    sequence: seq,
                    payload: Some(wire::stream_chunk::Payload::Text(wire::TextRevision {
                        segment_id: r.segment,
                        mutation: if r.replace {
                            wire::TextMutation::Replace
                        } else {
                            wire::TextMutation::Append
                        } as i32,
                        text: r.text,
                        committed: r.committed,
                    })),
                })),
            }])
            .await
            .map_err(super::js_error)
    }
    #[wasm_bindgen(js_name=closeMediaStream)]
    pub async fn close_media_stream(
        &self,
        id: u32,
        stream_id: String,
        next_sequence: u32,
    ) -> Result<(), JsValue> {
        let input = self.active_input(id)?;
        input
            .try_lock()
            .ok_or_else(|| js("media producer busy"))?
            .send(vec![wire::InputEvent {
                event: Some(wire::input_event::Event::Close(wire::StreamClose {
                    stream_id,
                    next_sequence: u64::from(next_sequence),
                })),
            }])
            .await
            .map_err(super::js_error)
    }
    #[wasm_bindgen(js_name=finishMediaInput)]
    pub async fn finish_media_input(&self, id: u32) -> Result<(), JsValue> {
        self.active_input(id)?
            .try_lock()
            .ok_or_else(|| js("media producer busy"))?
            .finish_input()
            .await
            .map_err(super::js_error)
    }
    #[wasm_bindgen(js_name=takeMediaOutputJson)]
    pub fn take_media_output_json(&self, id: u32) -> Result<Option<String>, JsValue> {
        let h = self.interactions.borrow();
        let retained = h
            .retained
            .as_ref()
            .filter(|r| r.id == id)
            .ok_or_else(|| js("stale media operation"))?;
        retained
            .events
            .borrow_mut()
            .metadata()
            .map(|event| serde_json::to_string(&event).map_err(super::js_error))
            .transpose()
    }
    #[wasm_bindgen(js_name=takeMediaOutputBytes)]
    pub fn take_media_output_bytes(&self, id: u32, chunk_id: u32) -> Result<Vec<u8>, JsValue> {
        let h = self.interactions.borrow();
        let retained = h
            .retained
            .as_ref()
            .filter(|r| r.id == id)
            .ok_or_else(|| js("stale media operation"))?;
        retained
            .events
            .borrow_mut()
            .take_bytes(chunk_id)
            .map_err(super::js_error)
    }
    #[wasm_bindgen(js_name=beginMediaDownload)]
    pub async fn begin_media_download(&self, id: u32, port: String) -> Result<String, JsValue> {
        let (owner, expires) = {
            let mut host = self.interactions.borrow_mut();
            host.downloads
                .retain(|_, owner| !owner.finished.get() || owner.verified.get());
            if host.disposed
                || host.downloads.len() >= 32
                || host.downloads.contains_key(&(id, port.clone()))
            {
                return Err(js("media reader unavailable or busy"));
            }
            let retained = host
                .retained
                .as_mut()
                .filter(|r| r.id == id)
                .ok_or_else(|| js("stale media operation"))?;
            let reader_limit = retained.reader_limit;
            let (grant, expires) = retained
                .grants
                .remove(&port)
                .ok_or_else(|| js("output grant already consumed or unavailable"))?;
            let (owner, driver) = Download::start(
                self.interaction_transport.media_transfers(),
                grant,
                DownloadDeadline::new(
                    async move {
                        let remaining = (expires - monotonic_ms()).clamp(0.0, 900000.0) as u32;
                        if remaining > 0 {
                            super::sleep_ms(remaining).await;
                        }
                    }
                    .boxed_local(),
                    move || monotonic_ms() >= expires,
                ),
                || super::sleep_ms(30000).boxed_local(),
                reader_limit,
            )
            .map_err(super::js_error)?;
            host.downloads.insert((id, port), owner.clone());
            wasm_bindgen_futures::spawn_local(driver);
            (owner, expires)
        };
        let mut descriptor = owner.opened().await.map_err(super::js_error)?;
        let remaining = (expires - monotonic_ms()).clamp(0.0, 900000.0) as u64;
        owner
            .ensure_descriptor_live(remaining)
            .map_err(super::js_error)?;
        descriptor.remaining_validity_ms = Some(crate::RoutineRevision(remaining));
        let json = serde_json::to_string(&descriptor).map_err(super::js_error)?;
        owner.ensure_live().map_err(super::js_error)?;
        Ok(json)
    }
    #[wasm_bindgen(js_name=readMediaDownloadChunk)]
    pub async fn read_media_download_chunk(
        &self,
        id: u32,
        port: String,
    ) -> Result<Option<Vec<u8>>, JsValue> {
        let owner = self
            .interactions
            .borrow()
            .downloads
            .get(&(id, port))
            .cloned()
            .ok_or_else(|| js("unknown reader"))?;
        let chunk = owner.next().await.map_err(super::js_error)?;
        owner.ensure_live().map_err(super::js_error)?;
        Ok(chunk)
    }
    #[wasm_bindgen(js_name=beginRoutineTextResult)]
    pub fn begin_routine_text_result(
        &self,
        invocation: String,
        selected: String,
    ) -> Result<u32, JsValue> {
        let snapshot: serde_json::Value = serde_json::from_str(&self.latest_snapshot.borrow())
            .map_err(|_| js("missing client snapshot"))?;
        let detail = &snapshot["routines"]["details"][&invocation]["detail"]["summary"];
        let summary = if detail.is_object() {
            detail
        } else {
            snapshot["routines"]["history"]["recent"]
                .as_array()
                .and_then(|rows| {
                    rows.iter()
                        .find(|row| row["invocationId"].as_str() == Some(&invocation))
                })
                .ok_or_else(|| js("load invocation detail first"))?
        };
        let descriptors: Vec<crate::RoutineTextResult> =
            serde_json::from_value(summary["textResults"].clone())
                .map_err(|_| js("missing text result descriptors"))?;
        let reader = self
            .interaction_transport
            .media_transfers()
            .text_reader(invocation, &descriptors, &selected)
            .map_err(super::js_error)?;
        let mut host = self.interactions.borrow_mut();
        if host.disposed || host.query_ids.len() >= 32 {
            return Err(js("text result query limit"));
        }
        let id = host
            .next
            .checked_add(1)
            .ok_or_else(|| js("query identity exhausted"))?;
        host.next = id;
        host.text_queries
            .try_lock()
            .ok_or_else(|| js("text result query busy"))?
            .insert(id, reader);
        host.query_ids.insert(id);
        Ok(id)
    }
    #[wasm_bindgen(js_name=readRoutineTextResultChunk)]
    pub async fn read_routine_text_result_chunk(&self, id: u32) -> Result<Option<String>, JsValue> {
        let queries = {
            let host = self.interactions.borrow();
            if !host.query_ids.contains(&id) {
                return Err(js("stale text result query"));
            }
            host.text_queries.clone()
        };
        let result = queries
            .try_lock()
            .ok_or_else(|| js("text result query busy"))?
            .get_mut(&id)
            .ok_or_else(|| js("unknown text result query"))?
            .next_chunk()
            .await
            .map_err(super::js_error)?;
        if !self.interactions.borrow().query_ids.contains(&id) {
            return Err(js("stale text result query"));
        }
        Ok(result)
    }
    #[wasm_bindgen(js_name=closeRoutineTextResult)]
    pub async fn close_routine_text_result(&self, id: u32) -> Result<(), JsValue> {
        let queries = {
            let host = self.interactions.borrow();
            host.text_queries.clone()
        };
        queries
            .try_lock()
            .ok_or_else(|| js("text result query busy"))?
            .remove(&id);
        self.interactions.borrow_mut().query_ids.remove(&id);
        Ok(())
    }
    #[wasm_bindgen(js_name=closeMediaDownload)]
    pub async fn close_media_download(&self, id: u32, port: String) -> Result<(), JsValue> {
        let owner = self
            .interactions
            .borrow()
            .downloads
            .get(&(id, port.clone()))
            .cloned()
            .ok_or_else(|| js("unknown reader"))?;
        if owner.close().await {
            self.interactions.borrow_mut().downloads.remove(&(id, port));
            Ok(())
        } else {
            Err(js(
                "download cleanup unverified; lease quarantined until original expiry",
            ))
        }
    }
    #[wasm_bindgen(js_name=beginMediaUpload)]
    pub async fn begin_media_upload(&self, id: u32, port: String) -> Result<(), JsValue> {
        let (input, uploads) = {
            let h = self.interactions.borrow();
            let a = h
                .active
                .as_ref()
                .filter(|a| a.id == id)
                .ok_or_else(|| js("stale media operation"))?;
            (
                a.input
                    .clone()
                    .ok_or_else(|| js("media operation opening"))?,
                a.uploads.clone(),
            )
        };
        let input = input.try_lock().ok_or_else(|| js("media producer busy"))?;
        let mut uploads = uploads
            .try_lock()
            .ok_or_else(|| js("media producer busy"))?;
        if uploads.contains_key(&port) {
            return Err(js("upload already started"));
        }
        let mut upload = input.begin_upload(&port).map_err(super::js_error)?;
        upload.begin().await.map_err(super::js_error)?;
        self.active_input(id)?;
        uploads.insert(port, upload);
        Ok(())
    }
    #[wasm_bindgen(js_name=appendMediaUpload)]
    pub async fn append_media_upload(
        &self,
        id: u32,
        port: String,
        data: &[u8],
    ) -> Result<(), JsValue> {
        if data.is_empty() || data.len() > 65536 {
            return Err(js("upload chunk limit"));
        }
        let uploads = self
            .interactions
            .borrow()
            .active
            .as_ref()
            .filter(|a| a.id == id)
            .map(|a| a.uploads.clone())
            .ok_or_else(|| js("stale media operation"))?;
        let input = self.active_input(id)?;
        let _producer = input.try_lock().ok_or_else(|| js("media producer busy"))?;
        uploads
            .try_lock()
            .ok_or_else(|| js("media producer busy"))?
            .get_mut(&port)
            .ok_or_else(|| js("unknown upload"))?
            .append(data)
            .await
            .map_err(super::js_error)
    }
    #[wasm_bindgen(js_name=commitMediaUpload)]
    pub async fn commit_media_upload(&self, id: u32, port: String) -> Result<String, JsValue> {
        let input = self.active_input(id)?;
        let mut input = input.try_lock().ok_or_else(|| js("media producer busy"))?;
        let uploads = self
            .interactions
            .borrow()
            .active
            .as_ref()
            .filter(|a| a.id == id)
            .map(|a| a.uploads.clone())
            .ok_or_else(|| js("stale media operation"))?;
        let (descriptor, event) = uploads
            .try_lock()
            .ok_or_else(|| js("media producer busy"))?
            .get_mut(&port)
            .ok_or_else(|| js("unknown upload"))?
            .commit()
            .await
            .map_err(super::js_error)?;
        self.active_input(id)?;
        input.send(vec![event]).await.map_err(super::js_error)?;
        serde_json::to_string(&descriptor).map_err(super::js_error)
    }
    #[wasm_bindgen(js_name=mediaInteractionStatusJson)]
    pub fn media_interaction_status_json(&self, id: u32) -> Result<String, JsValue> {
        let h = self.interactions.borrow();
        let s = h
            .status
            .as_ref()
            .filter(|s| s.operation_id == id)
            .ok_or_else(|| js("stale media operation"))?;
        serde_json::to_string(s).map_err(super::js_error)
    }
    #[wasm_bindgen(js_name=cancelMediaInteraction)]
    pub async fn cancel_media_interaction(&self, id: u32) -> Result<bool, JsValue> {
        let join = {
            let h = self.interactions.borrow();
            let a = h
                .active
                .as_ref()
                .filter(|a| a.id == id)
                .ok_or_else(|| js("stale media operation"))?;
            a.cancel.cancel();
            a.join.clone()
        };
        Ok(join.await)
    }
}
