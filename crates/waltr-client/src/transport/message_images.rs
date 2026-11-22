//! Runtime-bound message image transport. Payloads and staging IDs never enter reducer state.

use std::{
    fmt,
    future::{Future, poll_fn},
    pin::pin,
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
};

use futures_util::future::{AbortHandle, AbortRegistration, Abortable};
use tonic::{Code, Request, Status};
use waltr::grpc::waltr::session::v1::{
    AbortMessageImageUploadRequest, BeginMessageImageUploadRequest, MessageImageFormat,
    ReadMessageImageRequest, SealMessageImageUploadRequest, SubmitUserMessageRequest,
    UserMessageInputPart, WriteMessageImageChunkRequest,
    session_service_client::SessionServiceClient, user_message_input_part::Part,
};

use super::{RemoteTransport, RequestCredentials, status_code_name, status_error};
use crate::{
    ClientError, ClientErrorKind, DefaultGrpcTransport, MessageImageFormatView,
    MessageImageInfoView,
};

/// Maximum length of a single message image.
pub const MAX_MESSAGE_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum number of images in one message.
pub const MAX_MESSAGE_IMAGE_COUNT: usize = 4;
/// Maximum image bytes across all parts in one message.
pub const MAX_MESSAGE_TOTAL_IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_MESSAGE_PARTS: usize = 32;
pub(crate) const MAX_MESSAGE_TEXT_BYTES: usize = 16_384;
const CHUNK_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_READS: usize = 4;

type SessionClient = SessionServiceClient<<DefaultGrpcTransport as RemoteTransport>::Inner>;

/// One ordered composer part. Not serializable; debug output omits its data.
pub enum MessageInputPart {
    /// User text at this position in the message.
    Text(String),
    /// A complete raster image at this position in the message.
    Image {
        /// Declared image format, verified by the server during seal.
        format: MessageImageFormatView,
        /// Binary contents of the image.
        bytes: Vec<u8>,
    },
}

impl fmt::Debug for MessageInputPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text(_) => "MessageInputPart::Text(..)",
            Self::Image { .. } => "MessageInputPart::Image(..)",
        })
    }
}

/// Caller-owned bounded binary operations for an exact client runtime and route.
///
/// One send and up to four reads may be active per runtime. Drive the runtime's
/// state stream to establish access before calling this lane. Cancel on a UI
/// route change; dropping the runtime revokes its lane even if clones remain.
#[derive(Clone)]
pub struct MessageImageLane {
    transport: DefaultGrpcTransport,
    state: Arc<Mutex<LaneState>>,
}

impl fmt::Debug for MessageImageLane {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageImageLane").finish_non_exhaustive()
    }
}

#[derive(Default)]
struct LaneState {
    access_generation: u64,
    permission_version: u64,
    ready: bool,
    closed: bool,
    #[cfg(not(target_arch = "wasm32"))]
    credential_generation: u64,
    next_id: u64,
    active: Vec<Active>,
}

struct Active {
    id: u64,
    send: bool,
    cancel: AbortHandle,
    revoke: AbortHandle,
}

struct Operation<'a> {
    lane: &'a MessageImageLane,
    id: u64,
    permission_version: u64,
    credentials: RequestCredentials,
    cancel: AbortHandle,
    revoke: AbortHandle,
    #[cfg(not(target_arch = "wasm32"))]
    credential_waker: Arc<futures_util::task::AtomicWaker>,
}

impl Drop for Operation<'_> {
    fn drop(&mut self) {
        self.lane
            .state()
            .active
            .retain(|active| active.id != self.id);
    }
}

impl Operation<'_> {
    fn authority_is_current(&self) -> bool {
        let state = self.lane.state();
        !state.closed
            && state.ready
            && state.permission_version == self.permission_version
            && !self.revoke.is_aborted()
            && self.credentials.is_current()
    }

    fn is_current(&self) -> bool {
        self.authority_is_current() && !self.cancel.is_aborted()
    }

    fn ensure_current(&self) -> Result<(), ClientError> {
        self.is_current().then_some(()).ok_or_else(canceled)
    }

    async fn poll_while_authorized<T>(
        &self,
        future: impl Future<Output = Result<T, ClientError>>,
    ) -> Option<Result<T, ClientError>> {
        let mut future = pin!(future);
        poll_fn(|cx| {
            #[cfg(not(target_arch = "wasm32"))]
            self.credential_waker.register(cx.waker());
            if !self.authority_is_current() {
                return Poll::Ready(None);
            }
            future.as_mut().poll(cx).map(|result| {
                // A current credential rejection can itself revoke the lease.
                (self.authority_is_current()
                    || result
                        .as_ref()
                        .is_err_and(ClientError::is_waltr_login_required))
                .then_some(result)
            })
        })
        .await
    }
}

impl MessageImageLane {
    pub(crate) fn new(transport: DefaultGrpcTransport) -> Self {
        Self {
            transport,
            state: Arc::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, LaneState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn update_access(&self, generation: u64, ready: bool) {
        self.set_access(Some(generation), ready);
    }

    #[cfg(all(target_arch = "wasm32", feature = "wasm"))]
    pub(crate) fn suspend_for_access_change(&self) {
        self.set_access(None, false);
    }

    fn set_access(&self, generation: Option<u64>, ready: bool) {
        let mut state = self.state();
        let generation = generation.unwrap_or(state.access_generation);
        if state.closed || state.access_generation == generation && state.ready == ready {
            return;
        }
        state.ready = ready;
        state.access_generation = generation;
        state.permission_version = state.permission_version.saturating_add(1);
        #[cfg(not(target_arch = "wasm32"))]
        if ready {
            state.credential_generation = self.transport.request_credentials().lease_generation;
        }
        let handles: Vec<_> = state
            .active
            .iter()
            .map(|active| active.revoke.clone())
            .collect();
        drop(state);
        for handle in handles {
            handle.abort();
        }
    }

    pub(crate) fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        let handles: Vec<_> = state
            .active
            .iter()
            .map(|active| active.revoke.clone())
            .collect();
        drop(state);
        for handle in handles {
            handle.abort();
        }
    }

    /// Cancel the active composer send without affecting history reads.
    ///
    /// Keep polling the send to let it attempt cleanup. A canceled admission
    /// can still have reached the server; inspect the timeline before resending.
    pub fn cancel_send(&self) {
        self.cancel_matching(true);
    }

    /// Cancel this runtime's in-flight image send and reads.
    pub fn cancel_all(&self) {
        self.cancel_matching(false);
    }

    fn cancel_matching(&self, send_only: bool) {
        let handles: Vec<_> = self
            .state()
            .active
            .iter()
            .filter(|active| !send_only || active.send)
            .map(|active| active.cancel.clone())
            .collect();
        for handle in handles {
            handle.abort();
        }
    }

    fn begin(
        &self,
        send: bool,
    ) -> Result<(Operation<'_>, AbortRegistration, AbortRegistration), ClientError> {
        let mut state = self.state();
        if !state.ready || state.closed {
            return Err(ClientError::new(
                "The selected Waltr session route is not ready",
            ));
        }
        let credentials = self.transport.request_credentials();
        #[cfg(not(target_arch = "wasm32"))]
        if credentials.lease_generation != state.credential_generation {
            return Err(ClientError::new(
                "Waltr access changed; reopen the selected route",
            ));
        }
        let limit = if send { 1 } else { MAX_ACTIVE_READS };
        if state
            .active
            .iter()
            .filter(|active| active.send == send)
            .count()
            >= limit
        {
            return Err(ClientError::new(if send {
                "A message send is already in progress"
            } else {
                "Too many message image reads are in progress"
            }));
        }
        let (cancel, cancel_registration) = AbortHandle::new_pair();
        let (revoke, revoke_registration) = AbortHandle::new_pair();
        let id = state.next_id;
        let permission_version = state.permission_version;
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| ClientError::new("The image lane is exhausted"))?;
        state.active.push(Active {
            id,
            send,
            cancel: cancel.clone(),
            revoke: revoke.clone(),
        });
        drop(state);
        Ok((
            Operation {
                lane: self,
                id,
                permission_version,
                #[cfg(not(target_arch = "wasm32"))]
                credential_waker: credentials.media_waker(),
                credentials,
                cancel,
                revoke,
            },
            cancel_registration,
            revoke_registration,
        ))
    }

    /// Upload bounded image parts and submit one ordered message. Success is
    /// admission, not a durable write. Retain the composer draft until success.
    ///
    /// # Errors
    /// Invalid input, canceled work, staging failure, or failed/uncertain
    /// admission. The lane never retries. Dropping this future leaves unknown
    /// or un-aborted uploads to the server's staging lease expiry.
    #[allow(clippy::future_not_send, reason = "WASM RPCs are locally polled")]
    pub async fn send(
        &self,
        session_id: String,
        parts: Vec<MessageInputPart>,
    ) -> Result<(), ClientError> {
        self.send_with_access_expiry(session_id, parts).await.0
    }

    /// Send an ordered message and report the exact native credential revision
    /// confirmed by an access rejection. Shells must still compare it to the
    /// current revision before dispatching access expiry. Browser sends have no
    /// native credential revision.
    #[allow(clippy::future_not_send, reason = "WASM RPCs are locally polled")]
    pub async fn send_with_access_expiry(
        &self,
        session_id: String,
        parts: Vec<MessageInputPart>,
    ) -> (Result<(), ClientError>, Option<u64>) {
        if let Err(error) = validate_parts(&parts) {
            return (Err(error), None);
        }
        if session_id.is_empty() {
            return (
                Err(ClientError::new(
                    "Create or select a session before sending a message",
                )),
                None,
            );
        }
        let (operation, cancel, revoke) = match self.begin(true) {
            Ok(prepared) => prepared,
            Err(error) => return (Err(error), None),
        };
        let result = self
            .send_ordered(&operation, cancel, revoke, session_id, parts)
            .await;
        #[cfg(not(target_arch = "wasm32"))]
        let expiry_revision = operation
            .credentials
            .access_expiry_revision
            .get()
            .copied()
            .filter(|_| {
                result
                    .as_ref()
                    .is_err_and(ClientError::is_waltr_login_required)
            });
        #[cfg(target_arch = "wasm32")]
        let expiry_revision = None;
        (result, expiry_revision)
    }

    #[allow(clippy::future_not_send, reason = "WASM RPCs are locally polled")]
    async fn send_ordered(
        &self,
        operation: &Operation<'_>,
        cancel: AbortRegistration,
        revoke: AbortRegistration,
        session_id: String,
        parts: Vec<MessageInputPart>,
    ) -> Result<(), ClientError> {
        let admitting = AtomicBool::new(false);
        let run = async {
            let mut client = None;
            let mut uploads = Vec::new();
            let stage_and_submit = async {
                let pinned_client = self
                    .transport
                    .session_client(&operation.credentials)
                    .await
                    .map_err(connection_error)?
                    .max_decoding_message_size(CHUNK_BYTES + 4);
                let client = client.insert(pinned_client);
                let mut wire_parts = Vec::with_capacity(parts.len());
                for part in parts {
                    match part {
                        MessageInputPart::Text(text) => wire_parts.push(text_part(text)),
                        MessageInputPart::Image { format, bytes } => {
                            wire_parts.push(
                                stage_image(
                                    client,
                                    operation,
                                    &session_id,
                                    format,
                                    &bytes,
                                    &mut uploads,
                                )
                                .await?,
                            );
                        }
                    }
                }
                operation.ensure_current()?;
                admitting.store(true, Ordering::Relaxed);
                let result = client
                    .submit_user_message(rpc(SubmitUserMessageRequest {
                        session_id: session_id.clone(),
                        parts: wire_parts,
                    }))
                    .await;
                match result {
                    Ok(_) => Ok(()),
                    Err(error) if uncertain(error.code()) => Err(admission_unconfirmed()),
                    Err(error) => {
                        admitting.store(false, Ordering::Relaxed);
                        Err(image_error(
                            "Message was not admitted",
                            &error,
                            &operation.credentials,
                        ))
                    }
                }
            };
            let result = Abortable::new(stage_and_submit, cancel)
                .await
                .unwrap_or_else(|_| {
                    Err(if admitting.load(Ordering::Relaxed) {
                        admission_unconfirmed()
                    } else {
                        canceled()
                    })
                });
            if result.is_err()
                && !admitting.load(Ordering::Relaxed)
                && let Some(client) = client.as_mut()
            {
                for upload_id in uploads {
                    if !operation.authority_is_current() {
                        break;
                    }
                    let mut request = Request::new(AbortMessageImageUploadRequest {
                        session_id: session_id.clone(),
                        upload_id,
                    });
                    request.set_timeout(std::time::Duration::from_secs(3));
                    drop(client.abort_message_image_upload(request).await);
                }
            }
            result
        };
        operation
            .poll_while_authorized(async {
                Abortable::new(run, revoke)
                    .await
                    .unwrap_or_else(|_| Err(canceled()))
            })
            .await
            .unwrap_or_else(|| {
                Err(if admitting.load(Ordering::Relaxed) {
                    admission_unconfirmed()
                } else {
                    canceled()
                })
            })
    }

    /// Read one authorized image by its durable item and zero-based part index.
    ///
    /// # Errors
    /// Returns an error for a stale or unavailable route, a rejected read,
    /// cancellation, or malformed stream framing or length. Never returns partial bytes.
    #[allow(clippy::future_not_send, reason = "WASM RPCs are locally polled")]
    pub async fn read(
        &self,
        session_id: String,
        item_id: String,
        part_index: u32,
        image: MessageImageInfoView,
    ) -> Result<Vec<u8>, ClientError> {
        self.read_size(session_id, item_id, part_index, image.size_bytes)
            .await
    }

    #[allow(clippy::future_not_send, reason = "WASM RPCs are locally polled")]
    pub(crate) async fn read_size(
        &self,
        session_id: String,
        item_id: String,
        part_index: u32,
        size_bytes: u32,
    ) -> Result<Vec<u8>, ClientError> {
        let expected = size_bytes as usize;
        if expected == 0
            || expected > MAX_MESSAGE_IMAGE_BYTES
            || session_id.is_empty()
            || item_id.is_empty()
            || part_index as usize >= MAX_MESSAGE_PARTS
        {
            return Err(ClientError::new("Invalid message image descriptor"));
        }
        let (operation, cancel, revoke) = self.begin(false)?;
        let fetch = async {
            let mut client = self
                .transport
                .session_client(&operation.credentials)
                .await
                .map_err(connection_error)?
                .max_decoding_message_size(CHUNK_BYTES + 4);
            operation.ensure_current()?;
            let mut stream = client
                .read_message_image(rpc(ReadMessageImageRequest {
                    session_id,
                    item_id,
                    part_index,
                }))
                .await
                .map_err(|error| {
                    image_error(
                        "Could not read message image",
                        &error,
                        &operation.credentials,
                    )
                })?
                .into_inner();
            let mut data = Vec::with_capacity(expected);
            while let Some(chunk) = stream.message().await.map_err(|error| {
                image_error("Message image read failed", &error, &operation.credentials)
            })? {
                operation.ensure_current()?;
                append_chunk(&mut data, expected, &chunk.data)?;
            }
            if data.len() != expected {
                return Err(ClientError::new(
                    "Message image stream ended before the expected length",
                ));
            }
            Ok(data)
        };
        operation
            .poll_while_authorized(async {
                let result = Abortable::new(Abortable::new(fetch, cancel), revoke)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .unwrap_or_else(|| Err(canceled()));
                if result.is_ok() {
                    operation.ensure_current()?;
                }
                result
            })
            .await
            .unwrap_or_else(|| Err(canceled()))
    }
}

async fn stage_image(
    client: &mut SessionClient,
    operation: &Operation<'_>,
    session_id: &str,
    format: MessageImageFormatView,
    bytes: &[u8],
    uploads: &mut Vec<String>,
) -> Result<UserMessageInputPart, ClientError> {
    operation.ensure_current()?;
    let upload_id = client
        .begin_message_image_upload(rpc(BeginMessageImageUploadRequest {
            session_id: session_id.into(),
            format: MessageImageFormat::from(format) as i32,
            size_bytes: u32::try_from(bytes.len()).expect("validated image size"),
        }))
        .await
        .map_err(|error| {
            image_error(
                "Could not begin image upload",
                &error,
                &operation.credentials,
            )
        })?
        .into_inner()
        .upload_id;
    if upload_id.is_empty() || upload_id.len() > 256 {
        return Err(ClientError::new("Invalid image upload acknowledgement"));
    }
    uploads.push(upload_id.clone());
    for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
        operation.ensure_current()?;
        client
            .write_message_image_chunk(rpc(WriteMessageImageChunkRequest {
                session_id: session_id.into(),
                upload_id: upload_id.clone(),
                offset: u32::try_from(index * CHUNK_BYTES).expect("validated image offset"),
                data: chunk.to_vec(),
            }))
            .await
            .map_err(|error| {
                image_error(
                    "Could not upload image chunk",
                    &error,
                    &operation.credentials,
                )
            })?;
    }
    operation.ensure_current()?;
    client
        .seal_message_image_upload(rpc(SealMessageImageUploadRequest {
            session_id: session_id.into(),
            upload_id: upload_id.clone(),
        }))
        .await
        .map_err(|error| {
            image_error(
                "Could not verify uploaded image",
                &error,
                &operation.credentials,
            )
        })?;
    Ok(UserMessageInputPart {
        part: Some(Part::ImageUploadId(upload_id)),
    })
}

pub(super) fn text_part(text: String) -> UserMessageInputPart {
    UserMessageInputPart {
        part: Some(Part::Text(text)),
    }
}

fn validate_parts(parts: &[MessageInputPart]) -> Result<(), ClientError> {
    if parts.len() > MAX_MESSAGE_PARTS {
        return Err(ClientError::new("Too many message parts"));
    }
    let mut image_count = 0;
    let mut image_bytes = 0usize;
    let mut text_bytes = 0usize;
    let mut nonblank = false;
    for part in parts {
        match part {
            MessageInputPart::Text(text) => {
                text_bytes = text_bytes.saturating_add(text.len());
                nonblank |= !text.trim().is_empty();
            }
            MessageInputPart::Image { bytes, .. } => {
                if bytes.is_empty() || bytes.len() > MAX_MESSAGE_IMAGE_BYTES {
                    return Err(ClientError::new(
                        "Each image must contain between 1 byte and 8 MiB",
                    ));
                }
                image_count += 1;
                image_bytes = image_bytes.saturating_add(bytes.len());
            }
        }
    }
    if image_count > MAX_MESSAGE_IMAGE_COUNT || image_bytes > MAX_MESSAGE_TOTAL_IMAGE_BYTES {
        return Err(ClientError::new(
            "A message can contain up to 4 images totaling 16 MiB",
        ));
    }
    if text_bytes > MAX_MESSAGE_TEXT_BYTES {
        return Err(ClientError::new("Message text exceeds 16 KiB"));
    }
    if !nonblank && image_count == 0 {
        return Err(ClientError::new(
            "A message requires nonblank text or an image",
        ));
    }
    Ok(())
}

fn append_chunk(data: &mut Vec<u8>, expected: usize, chunk: &[u8]) -> Result<(), ClientError> {
    if chunk.is_empty()
        || chunk.len() > CHUNK_BYTES
        || chunk.len() > expected.saturating_sub(data.len())
    {
        return Err(ClientError::new(
            "Invalid message image stream chunk or length",
        ));
    }
    data.extend_from_slice(chunk);
    Ok(())
}

fn rpc<T>(message: T) -> Request<T> {
    let mut request = Request::new(message);
    request.set_timeout(std::time::Duration::from_secs(30));
    request
}

fn canceled() -> ClientError {
    ClientError::new("Message image operation canceled or access changed")
}

fn admission_unconfirmed() -> ClientError {
    ClientError::new(
        "Message admission could not be confirmed. Check the timeline before resending.",
    )
}

const fn uncertain(code: Code) -> bool {
    matches!(
        code,
        Code::Cancelled
            | Code::Unknown
            | Code::DeadlineExceeded
            | Code::Aborted
            | Code::Internal
            | Code::Unavailable
            | Code::DataLoss
    )
}

fn connection_error(error: ClientError) -> ClientError {
    if error.kind() == ClientErrorKind::WaltrLoginRequired {
        ClientError::waltr_login_required("Waltr login required")
    } else {
        ClientError::new("Could not open the selected Waltr image route")
    }
}

fn image_error(operation: &str, status: &Status, credentials: &RequestCredentials) -> ClientError {
    if status.code() == Code::Unauthenticated
        && status_error(status, credentials).kind() == ClientErrorKind::WaltrLoginRequired
    {
        return ClientError::waltr_login_required("Waltr login required");
    }
    ClientError::new(format!("{operation} ({})", status_code_name(status.code())))
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests;
