//! Invocation-local provider inputs. No payload in this module is serializable.

use std::{
    fmt,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use crate::{
    BoxStreamFuture, CompletionRequest, LlmError, LlmStream, MessageImageLoadError,
    MessageImageLocation, MessageImageResolver, Result, TimelineCursor, UserMessagePart,
    has_user_message_content, user_message::text_part,
};

const MIB: usize = 1024 * 1024;
/// Maximum source size of one inline attachment.
pub const MAX_TRANSIENT_ATTACHMENT_BYTES: usize = 8 * MIB;
/// Maximum aggregate source bytes in one invocation.
pub const MAX_TRANSIENT_SOURCE_BYTES: usize = 16 * MIB;
/// Maximum serialized provider request body.
pub const MAX_TRANSIENT_BODY_BYTES: usize = 32 * MIB;
const SOURCE_PROCESS_BYTES: usize = 64 * MIB;
const ENCODER_SLOTS: usize = 2;

/// A trusted host's validated input classification, not a JSON validation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientInputKind {
    /// Host-validated PNG.
    Png,
    /// Host-validated JPEG.
    Jpeg,
    /// Host-validated WebP.
    WebP,
    /// Opaque native PDF; the host does not parse, sanitize, or execute it.
    Pdf,
    /// Exact UTF-8 bytes, checked by the constructor.
    Utf8,
}

impl TransientInputKind {
    /// Exact wire MIME type.
    #[must_use]
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
            Self::Pdf => "application/pdf",
            Self::Utf8 => "text/plain",
        }
    }
}

#[derive(Debug)]
struct Counter {
    used: AtomicUsize,
    limit: usize,
}

impl Counter {
    fn reserve(self: &Arc<Self>, bytes: usize) -> Result<Credit> {
        self.used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes).filter(|total| *total <= self.limit)
            })
            .map_err(|_| invalid("transient input credit exhausted"))?;
        Ok(Credit {
            counter: self.clone(),
            bytes,
        })
    }
}

#[derive(Debug)]
struct Credit {
    counter: Arc<Counter>,
    bytes: usize,
}
impl Drop for Credit {
    fn drop(&mut self) {
        self.counter.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

/// Shared process-wide, fail-fast source and request credits.
///
/// Obtain this handle explicitly in host composition. All production handles share
/// the same counters; constructing one per invocation cannot bypass the limits.
/// Credits bound source allocations and two concurrent attachment encoders, not
/// agent memory, storage, MCP catalogs, provider decoders, or process RSS.
#[derive(Clone, Debug)]
pub struct TransientInputBudget {
    source: Arc<Counter>,
    request: Arc<Counter>,
}

impl TransientInputBudget {
    /// Reserve source capacity before allocating or reading a host-owned attachment.
    ///
    /// # Errors
    /// Rejects empty/oversized declarations, exhausted credit, or allocation failure.
    pub fn attachment_buffer(
        &self,
        kind: TransientInputKind,
        declared_bytes: usize,
    ) -> Result<TransientAttachmentBuffer> {
        if declared_bytes == 0 || declared_bytes > MAX_TRANSIENT_ATTACHMENT_BYTES {
            return Err(invalid("invalid transient attachment size"));
        }
        let credit = self.source.reserve(declared_bytes)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(declared_bytes)
            .map_err(|_| invalid("transient allocation failed"))?;
        if bytes.capacity() > declared_bytes {
            return Err(invalid("transient allocation exceeds reserved capacity"));
        }
        Ok(TransientAttachmentBuffer {
            kind,
            bytes,
            credit,
        })
    }
    #[cfg(test)]
    pub(crate) fn isolated_for_test() -> Self {
        Self {
            source: Arc::new(Counter {
                used: AtomicUsize::new(0),
                limit: SOURCE_PROCESS_BYTES,
            }),
            request: Arc::new(Counter {
                used: AtomicUsize::new(0),
                limit: ENCODER_SLOTS,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn used_for_test(&self) -> (usize, usize) {
        (
            self.source.used.load(Ordering::SeqCst),
            self.request.used.load(Ordering::SeqCst),
        )
    }

    /// Obtain the process-global budget. Admission never waits.
    pub fn process() -> Self {
        static BUDGET: OnceLock<TransientInputBudget> = OnceLock::new();
        BUDGET
            .get_or_init(|| Self {
                source: Arc::new(Counter {
                    used: AtomicUsize::new(0),
                    limit: SOURCE_PROCESS_BYTES,
                }),
                request: Arc::new(Counter {
                    used: AtomicUsize::new(0),
                    limit: ENCODER_SLOTS,
                }),
            })
            .clone()
    }

    /// Transfer trusted host-owned bytes without copying them.
    ///
    /// Image validation is the host's responsibility. This is deliberately not
    /// deserializable. PDF classification is authorization, not a parser claim.
    /// Source credits charge vector capacity, including unused allocation slack.
    ///
    /// # Errors
    /// Rejects empty/oversized sources, invalid UTF-8, or exhausted process credits.
    pub fn attachment(
        &self,
        kind: TransientInputKind,
        bytes: Vec<u8>,
    ) -> Result<TransientAttachment> {
        if bytes.is_empty() || bytes.capacity() > MAX_TRANSIENT_ATTACHMENT_BYTES {
            return Err(invalid("invalid transient attachment size"));
        }
        if kind == TransientInputKind::Utf8 && std::str::from_utf8(&bytes).is_err() {
            return Err(invalid("transient text must be UTF-8"));
        }
        let credit = self.source.reserve(bytes.capacity())?;
        Ok(finish_attachment(kind, bytes, credit))
    }
}

/// Precharged host-owned buffer. Appending never grows its allocation or credit.
pub struct TransientAttachmentBuffer {
    kind: TransientInputKind,
    bytes: Vec<u8>,
    credit: Credit,
}
impl TransientAttachmentBuffer {
    /// Append borrowed source bytes without reallocating.
    ///
    /// # Errors
    /// Rejects any write beyond the reserved source capacity.
    pub fn append(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.len() > self.credit.bytes.saturating_sub(self.bytes.len()) {
            return Err(invalid("transient source exceeds declared size"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    /// Transfer the exact completed source and its credit without copying.
    ///
    /// # Errors
    /// Rejects incomplete sources or invalid UTF-8.
    pub fn finish(self) -> Result<TransientAttachment> {
        if self.bytes.len() != self.credit.bytes
            || (self.kind == TransientInputKind::Utf8 && std::str::from_utf8(&self.bytes).is_err())
        {
            return Err(invalid("incomplete or invalid transient source"));
        }
        Ok(finish_attachment(self.kind, self.bytes, self.credit))
    }
}
impl fmt::Debug for TransientAttachmentBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientAttachmentBuffer")
            .field("reserved", &self.credit.bytes)
            .field("written", &self.bytes.len())
            .finish_non_exhaustive()
    }
}
fn finish_attachment(
    kind: TransientInputKind,
    bytes: Vec<u8>,
    credit: Credit,
) -> TransientAttachment {
    let digest =
        Sha256::digest(&bytes)
            .iter()
            .fold(String::with_capacity(64), |mut digest, byte| {
                use std::fmt::Write as _;
                write!(&mut digest, "{byte:02x}").expect("writing to String");
                digest
            });
    TransientAttachment(Arc::new(Payload {
        kind,
        bytes,
        digest,
        source_credit: credit,
        #[cfg(test)]
        encodings: AtomicUsize::new(0),
    }))
}

struct Payload {
    kind: TransientInputKind,
    bytes: Vec<u8>,
    digest: String,
    source_credit: Credit,
    #[cfg(test)]
    encodings: AtomicUsize,
}

/// Accounted owned bytes. Every clone retains the original source credit.
#[derive(Clone)]
pub struct TransientAttachment(Arc<Payload>);

impl fmt::Debug for TransientAttachment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransientAttachment")
            .field("metadata", &self.audit())
            .finish()
    }
}

impl TransientAttachment {
    /// Return a verified descriptor for a supported image, without exposing its bytes.
    #[must_use]
    pub fn image_descriptor(&self) -> Option<crate::MessageImageDescriptor> {
        let (format, bytes) = self.image_source()?;
        crate::MessageImageDescriptor::for_bytes(format, bytes)
    }

    /// Borrow the encoded image source for host-side verification and storage.
    #[must_use]
    pub fn image_source(&self) -> Option<(crate::MessageImageFormat, &[u8])> {
        let format = crate::MessageImageFormat::from_media_type(self.0.kind.media_type())?;
        Some((format, &self.0.bytes))
    }

    /// Copy this payload into a precharged buffer of the same kind.
    ///
    /// # Errors
    /// Rejects a kind mismatch or insufficient remaining capacity.
    pub fn copy_into(&self, target: &mut TransientAttachmentBuffer) -> Result<()> {
        if self.0.kind != target.kind {
            return Err(invalid("transient attachment kind mismatch"));
        }
        target.append(&self.0.bytes)
    }

    /// Metadata only; never contains source or encoded bytes, or an image digest.
    #[must_use]
    pub fn audit(&self) -> Value {
        let mut metadata =
            json!({"mediaType": self.0.kind.media_type(), "size": self.0.bytes.len()});
        if matches!(
            self.0.kind,
            TransientInputKind::Utf8 | TransientInputKind::Pdf
        ) {
            metadata["sha256"] = json!(self.0.digest);
        }
        metadata
    }
}

/// Exact durable initial task prompt selected by trusted admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransientPromptTarget {
    /// User timeline snapshot cursor, not the copied source boundary.
    pub cursor: TimelineCursor,
    /// Initial task's provider turn.
    pub turn: u64,
    /// Zero-based chronological prompt index in effective helper context.
    pub prompt_index: usize,
}

/// Explicit model capabilities pinned by the caller; never inferred from names.
#[derive(Debug, Clone, Copy)]
pub struct TransientModelCapabilities {
    /// Native PNG inputs for this exact model/API.
    pub png: bool,
    /// Native JPEG inputs for this exact model/API.
    pub jpeg: bool,
    /// Native WebP inputs for this exact model/API.
    pub webp: bool,
    /// Native PDF input.
    pub pdf: bool,
    /// Native UTF-8 text parts.
    pub text: bool,
    /// Provider/model-specific request body limit, at most the local 32 MiB.
    pub max_body_bytes: usize,
}

/// Provider API encoding family. This does not grant model capabilities.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransientInputApi {
    /// Anthropic Messages.
    Anthropic,
    /// `OpenAI` Chat Completions.
    OpenAiChat,
    /// `OpenAI` Responses HTTP or WebSocket.
    OpenAiResponses,
}

/// Invocation-local attachments bound to the initial helper task prompt.
#[derive(Clone, Debug)]
pub struct TransientInput {
    attachments: Vec<TransientAttachment>,
    target: TransientPromptTarget,
    messages: Vec<OrderedMessage>,
    capabilities: TransientModelCapabilities,
    budget: TransientInputBudget,
    measure_binary: bool,
    follow_up: bool,
    canonical_messages: bool,
}

#[derive(Clone)]
struct OrderedMessage {
    target: TransientPromptTarget,
    parts: Vec<UserMessagePart<TransientAttachment>>,
}

impl fmt::Debug for OrderedMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OrderedMessage")
            .field("target", &self.target)
            .field("part_count", &self.parts.len())
            .finish()
    }
}

impl TransientInput {
    /// Validate run bounds without encoding or copying payloads.
    ///
    /// # Errors
    /// Rejects empty/more than four attachments, mismatched budgets, aggregate
    /// source overflow, zero body limits, or unsupported model capabilities.
    pub fn new(
        budget: &TransientInputBudget,
        target: TransientPromptTarget,
        capabilities: TransientModelCapabilities,
        attachments: Vec<TransientAttachment>,
    ) -> Result<Self> {
        let input = Self {
            attachments,
            target,
            messages: Vec::new(),
            capabilities,
            budget: budget.clone(),
            measure_binary: false,
            follow_up: false,
            canonical_messages: false,
        };
        input.check_source_bounds()?;
        input.preflight()?;
        Ok(input)
    }

    /// Bind verified parts to a committed follow-up prompt. The supplied text
    /// must equal the text projection of that prompt at provider dispatch.
    ///
    /// # Errors
    /// Rejects non-image attachments, invalid order, or aggregate limit violations.
    pub fn with_ordered_follow_up(
        mut self,
        target: TransientPromptTarget,
        parts: Vec<UserMessagePart<TransientAttachment>>,
    ) -> Result<Self> {
        let previous = self
            .messages
            .last()
            .map_or(self.target, |entry| entry.target);
        if target.prompt_index <= previous.prompt_index
            || target.turn < previous.turn
            || target.cursor <= previous.cursor
            || parts.len() > 32
            || !has_user_message_content(&parts)
            || !parts
                .iter()
                .any(|part| matches!(part, UserMessagePart::Image { .. }))
        {
            return Err(invalid("invalid ordered transient message"));
        }
        let text_bytes = parts.iter().try_fold(0usize, |bytes, part| match part {
            UserMessagePart::Text { text } => bytes.checked_add(text.len()),
            UserMessagePart::Image { image } => {
                let format = crate::MessageImageFormat::from_media_type(image.0.kind.media_type())?;
                format.matches_signature(&image.0.bytes).then_some(bytes)
            }
        });
        if text_bytes.is_none_or(|bytes| bytes > 16384) {
            return Err(invalid("invalid ordered transient message part"));
        }
        self.messages.push(OrderedMessage { target, parts });
        self.check_source_bounds()?;
        self.preflight()?;
        Ok(self)
    }

    fn sources(&self) -> impl Iterator<Item = &TransientAttachment> {
        self.attachments
            .iter()
            .chain(self.messages.iter().flat_map(|message| {
                message.parts.iter().filter_map(|part| match part {
                    UserMessagePart::Image { image } => Some(image),
                    UserMessagePart::Text { .. } => None,
                })
            }))
    }

    fn check_source_bounds(&self) -> Result<()> {
        if self.capabilities.max_body_bytes == 0 {
            return Err(invalid("invalid transient input bounds"));
        }
        let mut total = 0usize;
        let mut count = 0;
        for attachment in self.sources() {
            if !Arc::ptr_eq(&attachment.0.source_credit.counter, &self.budget.source) {
                return Err(invalid("transient input budget mismatch"));
            }
            total = total
                .checked_add(attachment.0.source_credit.bytes)
                .ok_or_else(|| invalid("transient source overflow"))?;
            count += 1;
        }
        if (!self.canonical_messages && !(1..=4).contains(&count))
            || total > MAX_TRANSIENT_SOURCE_BYTES
        {
            return Err(invalid("transient source limit exceeded"));
        }
        Ok(())
    }

    /// Borrow the host-pinned prompt identity.
    #[must_use]
    pub const fn target(&self) -> TransientPromptTarget {
        self.target
    }

    pub(crate) const fn is_follow_up(&self) -> bool {
        self.follow_up
    }

    pub(crate) fn ordered_targets(&self) -> impl Iterator<Item = TransientPromptTarget> + '_ {
        self.messages.iter().map(|message| message.target)
    }

    pub(crate) fn for_image_follow_up(&self) -> Option<Self> {
        self.attachments
            .iter()
            .all(|item| {
                matches!(
                    item.0.kind,
                    TransientInputKind::Png | TransientInputKind::Jpeg | TransientInputKind::WebP
                )
            })
            .then(|| Self {
                follow_up: true,
                ..self.clone()
            })
    }

    /// Metadata-only, explicitly non-replayable audit representation.
    pub fn audit(&self) -> Value {
        json!({"replayable": false, "status": "invocationLocal", "target": {
            "cursor": self.target.cursor.get(), "turn": self.target.turn, "promptIndex": self.target.prompt_index
        }, "attachments": self.attachments.iter().map(TransientAttachment::audit).collect::<Vec<_>>()})
    }

    pub(crate) fn preflight(&self) -> Result<()> {
        for item in self.sources() {
            let supported = match item.0.kind {
                TransientInputKind::Utf8 => self.capabilities.text,
                TransientInputKind::Pdf => self.capabilities.pdf,
                TransientInputKind::Png => self.capabilities.png,
                TransientInputKind::Jpeg => self.capabilities.jpeg,
                TransientInputKind::WebP => self.capabilities.webp,
            };
            if !supported {
                return Err(invalid("model does not support transient input kind"));
            }
        }
        Ok(())
    }

    pub(crate) fn user_content(
        &self,
        index: usize,
        prompt: &str,
        api: TransientInputApi,
    ) -> Option<Vec<Value>> {
        if !self.attachments.is_empty() && self.target.prompt_index == index {
            let mut parts = self.content(api);
            parts.push(text_part(api, prompt));
            return Some(parts);
        }
        self.messages
            .iter()
            .find(|message| message.target.prompt_index == index)
            .map(|message| {
                message
                    .parts
                    .iter()
                    .map(|part| match part {
                        UserMessagePart::Text { text } => text_part(api, text),
                        UserMessagePart::Image { image } => self.encode_part(image, api),
                    })
                    .collect()
            })
    }

    pub(crate) fn content(&self, api: TransientInputApi) -> Vec<Value> {
        self.attachments
            .iter()
            .map(|item| self.encode_part(item, api))
            .collect()
    }

    fn encode_part(&self, item: &TransientAttachment, api: TransientInputApi) -> Value {
        let payload = &item.0;
        if payload.kind == TransientInputKind::Utf8 {
            let text = std::str::from_utf8(&payload.bytes).expect("constructor checked UTF-8");
            return text_part(api, text);
        }
        let data = if self.measure_binary {
            String::new()
        } else {
            #[cfg(test)]
            payload.encodings.fetch_add(1, Ordering::SeqCst);
            STANDARD.encode(&payload.bytes)
        };
        let mime = payload.kind.media_type();
        match (api, payload.kind) {
            (TransientInputApi::Anthropic, kind) => {
                json!({"type": if kind == TransientInputKind::Pdf { "document" } else { "image" }, "source": {"type": "base64", "media_type": mime, "data": data}})
            }
            (TransientInputApi::OpenAiChat, TransientInputKind::Pdf) => {
                json!({"type": "file", "file": {"filename": "attachment.pdf", "file_data": format!("data:{mime};base64,{data}")}})
            }
            (TransientInputApi::OpenAiResponses, TransientInputKind::Pdf) => {
                json!({"type": "input_file", "filename": "attachment.pdf", "file_data": format!("data:{mime};base64,{data}")})
            }
            (TransientInputApi::OpenAiChat, _) => {
                json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}")}})
            }
            (TransientInputApi::OpenAiResponses, _) => {
                json!({"type": "input_image", "image_url": format!("data:{mime};base64,{data}")})
            }
        }
    }

    pub(crate) fn validate_request(&self, request: &CompletionRequest<'_>) -> Result<()> {
        if !self.attachments.is_empty() {
            let prompt = request
                .prompt_turns
                .get(self.target.prompt_index)
                .ok_or_else(|| invalid("transient prompt target missing"))?;
            if prompt.turn != self.target.turn {
                return Err(invalid("transient prompt target changed"));
            }
        }
        for message in &self.messages {
            let prompt = request
                .prompt_turns
                .get(message.target.prompt_index)
                .ok_or_else(|| invalid("transient follow-up target missing"))?;
            let text = message
                .parts
                .iter()
                .filter_map(|part| match part {
                    UserMessagePart::Text { text } => Some(text.as_str()),
                    UserMessagePart::Image { .. } => None,
                })
                .collect::<String>();
            if prompt.turn != message.target.turn || text != prompt.prompt {
                return Err(invalid("transient follow-up target changed"));
            }
        }
        Ok(())
    }

    fn body_limit(&self) -> usize {
        self.capabilities
            .max_body_bytes
            .min(MAX_TRANSIENT_BODY_BYTES)
    }

    /// Check exact incremental serialized length; no serialized body is allocated.
    pub(crate) fn check_body(&self, body: &Value) -> Result<()> {
        let mut counter = LimitedWriter {
            used: 0,
            limit: self.body_limit(),
        };
        serde_json::to_writer(&mut counter, body)
            .map_err(|_| invalid("transient provider body limit exceeded"))?;
        if self.measure_binary {
            for item in self.sources() {
                if item.0.kind != TransientInputKind::Utf8 {
                    let encoded = item
                        .0
                        .bytes
                        .len()
                        .checked_add(2)
                        .and_then(|n| (n / 3).checked_mul(4))
                        .ok_or_else(|| invalid("transient encoding overflow"))?;
                    counter.used = counter
                        .used
                        .checked_add(encoded)
                        .filter(|size| *size <= counter.limit)
                        .ok_or_else(|| {
                            invalid("transient provider body preflight limit exceeded")
                        })?;
                }
            }
        }
        Ok(())
    }
}

/// Resolve the committed media projection into the same bounded encoder used
/// for invocation-local inputs. The temporary provider holds no replay state.
pub(crate) async fn resolve_message_provider(
    provider: &Arc<dyn LlmStream>,
    resolver: Option<&dyn MessageImageResolver>,
    record: &crate::SessionRecord,
    messages: &std::collections::BTreeMap<usize, (crate::TimelineItemId, crate::UserMessage)>,
    request: &CompletionRequest<'_>,
) -> Result<Option<Arc<dyn LlmStream>>> {
    let has_images = |message: &crate::UserMessage| {
        message
            .parts()
            .iter()
            .any(|part| matches!(part, UserMessagePart::Image { .. }))
    };
    let has_user_images = messages.values().any(|(_, message)| has_images(message));
    if !has_user_images
        && (provider.transient_api().is_none()
            || !messages
                .values()
                .any(|(_, message)| message.parts().len() > 1))
    {
        return Ok(None);
    }
    if provider.transient_api().is_none() {
        return Err(invalid("selected provider API cannot encode user images"));
    }
    let mut input = provider
        .transient_input()
        .cloned()
        .unwrap_or_else(|| TransientInput {
            attachments: Vec::new(),
            target: TransientPromptTarget {
                cursor: TimelineCursor::ZERO,
                turn: 0,
                prompt_index: 0,
            },
            messages: Vec::new(),
            capabilities: TransientModelCapabilities {
                png: true,
                jpeg: true,
                webp: true,
                pdf: false,
                text: true,
                max_body_bytes: MAX_TRANSIENT_BODY_BYTES,
            },
            budget: TransientInputBudget::process(),
            measure_binary: false,
            follow_up: false,
            canonical_messages: true,
        });
    // The old invocation overlay has already passed its own capability check.
    // Canonical still images depend only on encoder support, not model names.
    input.canonical_messages = true;
    input.capabilities.png = true;
    input.capabilities.jpeg = true;
    input.capabilities.webp = true;
    let mut source_bytes: usize = input.sources().map(|item| item.0.source_credit.bytes).sum();
    for (&index, (item_id, message)) in messages {
        if message.parts().len() == 1 && !has_images(message) {
            continue;
        }
        if (!input.attachments.is_empty() && input.target.prompt_index == index)
            || input
                .messages
                .iter()
                .any(|message| message.target.prompt_index == index)
        {
            return Err(invalid("conflicting user image inputs"));
        }
        let turn = request
            .prompt_turns
            .get(index)
            .ok_or_else(|| invalid("user image prompt missing"))?
            .turn;
        let mut parts = Vec::with_capacity(message.parts().len());
        for (part_index, part) in message.parts().iter().enumerate() {
            let part = match part {
                UserMessagePart::Text { text } => UserMessagePart::Text { text: text.clone() },
                UserMessagePart::Image { image } => {
                    let resolver = resolver
                        .ok_or_else(|| invalid("user message image resolver unavailable"))?;
                    let descriptor = image.descriptor();
                    let bytes = descriptor.bytes() as usize;
                    source_bytes = source_bytes
                        .checked_add(bytes)
                        .filter(|total| *total <= MAX_TRANSIENT_SOURCE_BYTES)
                        .ok_or_else(|| invalid("user image request source limit exceeded"))?;
                    let kind = match descriptor.format() {
                        crate::MessageImageFormat::Png => TransientInputKind::Png,
                        crate::MessageImageFormat::Jpeg => TransientInputKind::Jpeg,
                        crate::MessageImageFormat::WebP => TransientInputKind::WebP,
                    };
                    let mut buffer = input.budget.attachment_buffer(kind, bytes)?;
                    let location = MessageImageLocation {
                        session_id: &record.id,
                        incarnation: record.metadata.incarnation.as_deref(),
                        item_id,
                        part_index,
                    };
                    resolver
                        .load(location, *image, &mut buffer)
                        .await
                        .map_err(|failure| {
                            invalid(match failure {
                                MessageImageLoadError::Missing => "user message image missing",
                                MessageImageLoadError::Corrupt => "user message image corrupt",
                                MessageImageLoadError::Unavailable => {
                                    "user message image unavailable"
                                }
                            })
                        })?;
                    if !descriptor.verifies(&buffer.bytes) {
                        return Err(invalid("user message image corrupt"));
                    }
                    UserMessagePart::Image {
                        image: buffer.finish()?,
                    }
                }
            };
            parts.push(part);
        }
        input.messages.push(OrderedMessage {
            target: TransientPromptTarget {
                cursor: TimelineCursor::ZERO,
                turn,
                prompt_index: index,
            },
            parts,
        });
    }
    input.check_source_bounds()?;
    input.validate_request(request)?;
    input.preflight()?;
    Ok(Some(Arc::new(InputProvider {
        inner: provider.clone(),
        input,
        private_message_replay: true,
    })))
}

struct LimitedWriter {
    used: usize,
    limit: usize,
}

/// Serialize once into exactly admitted capacity, avoiding geometric Vec growth.
pub(crate) fn encode_body(body: &Value) -> Result<Vec<u8>> {
    let mut counter = LimitedWriter {
        used: 0,
        limit: MAX_TRANSIENT_BODY_BYTES,
    };
    serde_json::to_writer(&mut counter, body)
        .map_err(|_| invalid("transient body serialization limit"))?;
    struct Writer {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self
                .bytes
                .len()
                .checked_add(bytes.len())
                .is_none_or(|n| n > self.limit)
            {
                return Err(std::io::Error::other("transient body serialization limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = Writer {
        bytes: Vec::with_capacity(counter.used),
        limit: counter.used,
    };
    serde_json::to_writer(&mut writer, body)
        .map_err(|_| invalid("transient body serialization limit"))?;
    Ok(writer.bytes)
}
impl std::io::Write for LimitedWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.used = self
            .used
            .checked_add(bytes.len())
            .filter(|n| *n <= self.limit)
            .ok_or_else(|| std::io::Error::other("body limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn invalid(message: &'static str) -> LlmError {
    LlmError::Protocol {
        provider: "transient_input",
        message: message.into(),
    }
}

struct InputProvider {
    inner: Arc<dyn LlmStream>,
    input: TransientInput,
    private_message_replay: bool,
}

/// Unforgeable active-request admission. Only the invocation adapter can create it.
/// Provider implementations borrow it while constructing and sending a body.
#[derive(Debug)]
pub struct AdmittedTransientInput {
    input: TransientInput,
    _credit: Arc<Credit>,
}

impl AdmittedTransientInput {
    /// Borrow the admitted, owned input. This grants no durable replay authority.
    #[must_use]
    pub const fn input(&self) -> &TransientInput {
        &self.input
    }
}

pub(crate) fn bind_provider(
    inner: Arc<dyn LlmStream>,
    input: TransientInput,
) -> Result<Arc<dyn LlmStream>> {
    if inner.transient_api().is_none() || inner.transient_input().is_some() {
        return Err(invalid("provider API does not support transient input"));
    }
    input.preflight()?;
    Ok(Arc::new(InputProvider {
        inner,
        input,
        private_message_replay: false,
    }))
}

impl LlmStream for InputProvider {
    fn name(&self) -> &'static str {
        self.inner.name()
    }
    fn transient_api(&self) -> Option<TransientInputApi> {
        self.inner.transient_api()
    }
    fn transient_input(&self) -> Option<&TransientInput> {
        Some(&self.input)
    }
    fn project_request(&self, _request: &CompletionRequest<'_>) -> Option<Value> {
        if self.private_message_replay {
            return Some(json!({"projection": "metadataOnly", "userMessageImages": true}));
        }
        Some(json!({"projection": "metadataOnly", "transientInput": self.input.audit()}))
    }
    fn preflight_transient_request(
        &self,
        request: &CompletionRequest<'_>,
        input: &AdmittedTransientInput,
    ) -> Result<()> {
        self.inner.preflight_transient_request(request, input)
    }
    fn stream_transient_request<'a>(
        &'a self,
        request: CompletionRequest<'a>,
        input: &'a AdmittedTransientInput,
    ) -> BoxStreamFuture<'a> {
        self.inner.stream_transient_request(request, input)
    }
    fn stream_request<'a>(&'a self, request: CompletionRequest<'a>) -> BoxStreamFuture<'a> {
        Box::pin(async move {
            self.input.validate_request(&request)?;
            self.input.preflight()?;
            let credit = Arc::new(self.input.budget.request.reserve(1)?);
            let mut admission = AdmittedTransientInput {
                input: self.input.clone(),
                _credit: credit,
            };
            admission.input.measure_binary = true;
            self.inner
                .preflight_transient_request(&request, &admission)
                .map_err(sanitize_error)?;
            admission.input.measure_binary = false;
            let stream = self
                .inner
                .stream_transient_request(request, &admission)
                .await
                .map_err(sanitize_error)?;
            // Retain request admission through the returned transport stream and cancellation.
            Ok(Box::pin(futures_util::stream::unfold(
                (stream, admission),
                |(mut stream, admission)| async move {
                    use futures_util::StreamExt as _;
                    stream
                        .next()
                        .await
                        .map(|item| (item.map_err(sanitize_error), (stream, admission)))
                },
            )) as crate::BoxCompletionStream)
        })
    }
}

pub(crate) fn sanitize_error(error: LlmError) -> LlmError {
    match error {
        LlmError::Protocol {
            provider: "transient_input",
            message,
        } => LlmError::Protocol {
            provider: "transient_input",
            message,
        },
        LlmError::Api {
            provider, status, ..
        } => LlmError::Api {
            provider,
            status,
            message: "transient provider request failed".into(),
            headers: Vec::new(),
            code: None,
            body: None,
        },
        LlmError::AuthenticationFailure { failure, .. } => LlmError::AuthenticationFailure {
            failure,
            message: "transient provider authentication failed".into(),
        },
        _ => invalid("transient provider transport or protocol failed"),
    }
}

#[cfg(test)]
mod tests;
