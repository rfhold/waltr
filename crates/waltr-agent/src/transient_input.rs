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

use crate::{BoxStreamFuture, CompletionRequest, LlmError, LlmStream, Result, TimelineCursor};

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
    /// Metadata only; never contains source or encoded bytes.
    #[must_use]
    pub fn audit(&self) -> Value {
        json!({"mediaType": self.0.kind.media_type(), "size": self.0.bytes.len(), "sha256": self.0.digest})
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
    capabilities: TransientModelCapabilities,
    budget: TransientInputBudget,
    measure_binary: bool,
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
        if attachments.is_empty() || attachments.len() > 4 || capabilities.max_body_bytes == 0 {
            return Err(invalid("invalid transient input bounds"));
        }
        let mut total = 0usize;
        for attachment in &attachments {
            if !Arc::ptr_eq(&attachment.0.source_credit.counter, &budget.source) {
                return Err(invalid("transient input budget mismatch"));
            }
            total = total
                .checked_add(attachment.0.source_credit.bytes)
                .ok_or_else(|| invalid("transient source overflow"))?;
        }
        if total > MAX_TRANSIENT_SOURCE_BYTES {
            return Err(invalid("transient source limit exceeded"));
        }
        let input = Self {
            attachments,
            target,
            capabilities,
            budget: budget.clone(),
            measure_binary: false,
        };
        input.preflight()?;
        Ok(input)
    }

    /// Borrow the host-pinned prompt identity.
    #[must_use]
    pub const fn target(&self) -> TransientPromptTarget {
        self.target
    }

    /// Metadata-only, explicitly non-replayable audit representation.
    pub fn audit(&self) -> Value {
        json!({"replayable": false, "status": "invocationLocal", "target": {
            "cursor": self.target.cursor.get(), "turn": self.target.turn, "promptIndex": self.target.prompt_index
        }, "attachments": self.attachments.iter().map(TransientAttachment::audit).collect::<Vec<_>>()})
    }

    pub(crate) fn preflight(&self) -> Result<()> {
        for item in &self.attachments {
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

    pub(crate) const fn matches_prompt(&self, index: usize) -> bool {
        self.target.prompt_index == index
    }

    pub(crate) fn content(&self, api: TransientInputApi) -> Vec<Value> {
        self.attachments.iter().map(|item| {
            let payload = &item.0;
            if payload.kind == TransientInputKind::Utf8 {
                let text = std::str::from_utf8(&payload.bytes).expect("constructor checked UTF-8");
                return json!({"type": if api == TransientInputApi::OpenAiResponses { "input_text" } else { "text" }, "text": text});
            }
            let data = if self.measure_binary { String::new() } else {
                #[cfg(test)] payload.encodings.fetch_add(1, Ordering::SeqCst);
                STANDARD.encode(&payload.bytes)
            };
            let mime = payload.kind.media_type();
            match (api, payload.kind) {
                (TransientInputApi::Anthropic, kind) => json!({"type": if kind == TransientInputKind::Pdf { "document" } else { "image" }, "source": {"type": "base64", "media_type": mime, "data": data}}),
                (TransientInputApi::OpenAiChat, TransientInputKind::Pdf) => json!({"type": "file", "file": {"filename": "attachment.pdf", "file_data": format!("data:{mime};base64,{data}")}}),
                (TransientInputApi::OpenAiResponses, TransientInputKind::Pdf) => json!({"type": "input_file", "filename": "attachment.pdf", "file_data": format!("data:{mime};base64,{data}")}),
                (TransientInputApi::OpenAiChat, _) => json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}")}}),
                (TransientInputApi::OpenAiResponses, _) => json!({"type": "input_image", "image_url": format!("data:{mime};base64,{data}")}),
            }
        }).collect()
    }

    pub(crate) fn validate_request(&self, request: &CompletionRequest<'_>) -> Result<()> {
        let prompt = request
            .prompt_turns
            .get(self.target.prompt_index)
            .ok_or_else(|| invalid("transient prompt target missing"))?;
        if prompt.turn != self.target.turn {
            return Err(invalid("transient prompt target changed"));
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
            for item in &self.attachments {
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
    Ok(Arc::new(InputProvider { inner, input }))
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
        Some(json!({"projection": "metadataOnly", "transientInput": self.input.audit()}))
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
