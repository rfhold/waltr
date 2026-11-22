//! Raw IPC for chat images. The shared client runtime owns upload and read RPCs.

use std::{
    cell::Cell,
    future::Future as _,
    rc::Rc,
    sync::{Arc, LazyLock},
    task::Poll,
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;
use waltr_client::{
    MAX_MESSAGE_IMAGE_BYTES, MAX_MESSAGE_IMAGE_COUNT, MAX_MESSAGE_TOTAL_IMAGE_BYTES,
    MessageImageFormatView, MessageImageInfoView, MessageImageLane, MessageInputPart,
    NativeCredentialProvider,
};

use crate::bridge::RuntimeBridge;

const MAX_PARTS: usize = 32;
const MAX_TEXT_BYTES: usize = 16 * 1024;
const MAX_PACKET: usize = 1 + MAX_PARTS * 5 + MAX_TEXT_BYTES + MAX_MESSAGE_TOTAL_IMAGE_BYTES;
const MAX_ID_BYTES: usize = 256;
static SEND_CREDIT: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(1)));
static READ_CREDIT: LazyLock<Arc<Semaphore>> = LazyLock::new(|| Arc::new(Semaphore::new(4)));

fn invalid() -> String {
    "Invalid chat image request.".into()
}

fn stale() -> String {
    "The selected chat image route changed.".into()
}

fn bounded_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES && !value.chars().any(char::is_control)
}

pub(crate) enum Operation {
    Send(u32, String, Vec<MessageInputPart>),
    Read(String, String, u32, MessageImageInfoView),
    Cancel { send_only: bool, operation_id: u32 },
}

pub(crate) struct Request {
    pub(crate) operation: Operation,
    pub(crate) reply: oneshot::Sender<Result<Response, String>>,
    pub(crate) credit: Option<OwnedSemaphorePermit>,
}

pub(crate) struct Response {
    pub(crate) bytes: Vec<u8>,
    pub(crate) cancel_confirmed: bool,
    fence: Option<(CancellationToken, NativeCredentialProvider, u64)>,
    _credit: Option<OwnedSemaphorePermit>,
}

impl Response {
    pub(crate) fn ensure_current(&self) -> Result<(), String> {
        if let Some((authority, credentials, revision)) = &self.fence
            && (authority.is_cancelled() || credentials.credential_revision() != *revision)
        {
            return Err(stale());
        }
        Ok(())
    }
}

pub(crate) struct Host {
    lane: MessageImageLane,
    credentials: NativeCredentialProvider,
    authority: CancellationToken,
    order: Rc<SendOrder>,
    pub(crate) tasks: JoinSet<Option<u64>>,
}

#[derive(Default)]
struct SendOrder {
    watermark: Cell<u32>,
    active: Cell<Option<u32>>,
}

#[derive(Debug, PartialEq, Eq)]
enum CancelOrder {
    BeforeAdmission,
    Active,
    Unconfirmed,
}

impl SendOrder {
    fn admit(&self, id: u32) -> bool {
        if id <= self.watermark.get() || self.active.get().is_some() {
            return false;
        }
        self.watermark.set(id);
        self.active.set(Some(id));
        true
    }

    fn cancel(&self, id: u32) -> CancelOrder {
        if self.active.get() == Some(id) {
            return CancelOrder::Active;
        }
        if id > self.watermark.get() {
            self.watermark.set(id);
            return CancelOrder::BeforeAdmission;
        }
        CancelOrder::Unconfirmed
    }
}

impl Host {
    pub(crate) fn new(lane: MessageImageLane, credentials: NativeCredentialProvider) -> Self {
        Self {
            lane,
            credentials,
            authority: CancellationToken::new(),
            order: Rc::new(SendOrder::default()),
            tasks: JoinSet::new(),
        }
    }

    pub(crate) async fn revoke(&mut self) {
        self.authority.cancel();
        self.lane.cancel_all();
        while self.tasks.join_next().await.is_some() {}
        self.authority = CancellationToken::new();
    }

    pub(crate) async fn dispatch(&mut self, request: Request) {
        let Request {
            operation,
            reply,
            credit,
        } = request;
        if let Operation::Cancel {
            send_only,
            operation_id,
        } = operation
        {
            let order = self.order.cancel(operation_id);
            if send_only {
                if order == CancelOrder::Active {
                    self.lane.cancel_send();
                }
            } else if self
                .order
                .active
                .get()
                .is_none_or(|active| active <= operation_id)
            {
                // A delayed cleanup from an earlier view must not stop a newer send.
                self.revoke().await;
            }
            drop(reply.send(Ok(Response {
                bytes: Vec::new(),
                cancel_confirmed: order != CancelOrder::Unconfirmed,
                fence: None,
                _credit: credit,
            })));
            return;
        }
        if let Operation::Send(id, _, _) = &operation
            && !self.order.admit(*id)
        {
            drop(reply.send(Err(
                "The chat image send was cancelled before admission.".into(),
            )));
            return;
        }
        let lane = self.lane.clone();
        let order = Rc::clone(&self.order);
        let authority = self.authority.clone();
        let credentials = self.credentials.clone();
        let revision = credentials.credential_revision();
        let mut operation = Box::pin(async move {
            let result = match operation {
                Operation::Send(_, session, parts) => {
                    let result = lane.send(session, parts).await.map(|()| Vec::new());
                    order.active.set(None);
                    result
                }
                Operation::Read(session, item, index, image) => {
                    lane.read(session, item, index, image).await
                }
                Operation::Cancel { .. } => unreachable!(),
            };
            let auth_failed = result
                .as_ref()
                .err()
                .filter(|error| error.is_waltr_login_required())
                .map(|_| credentials.credential_revision());
            let result = result.map_err(|error| error.to_string()).and_then(|bytes| {
                let response = Response {
                    bytes,
                    cancel_confirmed: false,
                    fence: Some((authority, credentials, revision)),
                    _credit: credit,
                };
                response.ensure_current()?;
                Ok(response)
            });
            drop(reply.send(result));
            auth_failed
        });
        // Admit the lane operation before servicing a queued cancellation.
        match std::future::poll_fn(|cx| Poll::Ready(operation.as_mut().poll(cx))).await {
            Poll::Pending => {
                self.tasks.spawn_local(operation);
            }
            Poll::Ready(result) => {
                self.tasks.spawn_local(async move { result });
            }
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.authority.cancel();
        self.lane.cancel_all();
    }
}

fn decode_packet(mut body: &[u8]) -> Result<Vec<MessageInputPart>, String> {
    if body.len() > MAX_PACKET {
        return Err(invalid());
    }
    let (count, remaining) = body.split_first().ok_or_else(invalid)?;
    let count = usize::from(*count);
    if !(1..=MAX_PARTS).contains(&count) {
        return Err(invalid());
    }
    body = remaining;
    let mut parts = Vec::with_capacity(count);
    let (mut text_bytes, mut image_bytes, mut image_count) = (0, 0, 0);
    for _ in 0..count {
        let ([tag, a, b, c, d], remaining) = body.split_first_chunk().ok_or_else(invalid)?;
        let length = u32::from_le_bytes([*a, *b, *c, *d]) as usize;
        let (bytes, remaining) = remaining.split_at_checked(length).ok_or_else(invalid)?;
        body = remaining;
        if *tag == 0 {
            text_bytes += length;
            if text_bytes > MAX_TEXT_BYTES {
                return Err(invalid());
            }
            let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
            parts.push(MessageInputPart::Text(text.into()));
        } else {
            image_bytes += length;
            image_count += 1;
            if !(1..=MAX_MESSAGE_IMAGE_BYTES).contains(&length)
                || image_bytes > MAX_MESSAGE_TOTAL_IMAGE_BYTES
                || image_count > MAX_MESSAGE_IMAGE_COUNT
            {
                return Err(invalid());
            }
            let format = match tag {
                1 => MessageImageFormatView::Png,
                2 => MessageImageFormatView::Jpeg,
                3 => MessageImageFormatView::Webp,
                _ => return Err(invalid()),
            };
            parts.push(MessageInputPart::Image {
                format,
                bytes: bytes.to_vec(),
            });
        }
    }
    if !body.is_empty() {
        return Err(invalid());
    }
    Ok(parts)
}

#[tauri::command]
pub(crate) async fn send_chat_images(
    bridge: tauri::State<'_, Arc<RuntimeBridge>>,
    ipc: tauri::ipc::Request<'_>,
) -> Result<(), String> {
    let tauri::ipc::InvokeBody::Raw(body) = ipc.body() else {
        return Err(invalid());
    };
    let header = |key| {
        ipc.headers()
            .get(key)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(invalid)
    };
    let generation = header("x-waltr-generation")?
        .parse()
        .map_err(|_| invalid())?;
    let operation_id: u32 = header("x-waltr-operation-id")?
        .parse()
        .map_err(|_| invalid())?;
    if operation_id == 0 {
        return Err(invalid());
    }
    let session = header("x-waltr-session")?;
    if session.len() > MAX_ID_BYTES * 3 {
        return Err(invalid());
    }
    let encoded = format!("session={session}");
    let session = url::form_urlencoded::parse(encoded.as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .filter(|value| bounded_id(value))
        .ok_or_else(invalid)?;
    bridge.with_current(generation, || Ok(()))?;
    let credit = Arc::clone(&SEND_CREDIT)
        .try_acquire_owned()
        .map_err(|_| "A chat image send is already in progress.")?;
    let parts = decode_packet(body)?;
    bridge
        .image_request(
            generation,
            Operation::Send(operation_id, session, parts),
            Some(credit),
        )
        .await?;
    Ok(())
}

#[tauri::command]
pub(crate) async fn read_chat_image(
    bridge: tauri::State<'_, Arc<RuntimeBridge>>,
    generation: u64,
    session_id: String,
    item_id: String,
    part_index: u32,
    format: MessageImageFormatView,
    size_bytes: u32,
) -> Result<tauri::ipc::Response, String> {
    if !bounded_id(&session_id)
        || !bounded_id(&item_id)
        || part_index as usize >= MAX_PARTS
        || !(1..=MAX_MESSAGE_IMAGE_BYTES).contains(&(size_bytes as usize))
    {
        return Err(invalid());
    }
    let credit = Arc::clone(&READ_CREDIT)
        .try_acquire_owned()
        .map_err(|_| "Too many chat image reads are in progress.")?;
    let response = bridge
        .image_request(
            generation,
            Operation::Read(
                session_id,
                item_id,
                part_index,
                MessageImageInfoView { format, size_bytes },
            ),
            Some(credit),
        )
        .await?;
    Ok(tauri::ipc::Response::new(response.bytes))
}

#[tauri::command]
pub(crate) async fn cancel_chat_images(
    bridge: tauri::State<'_, Arc<RuntimeBridge>>,
    generation: u64,
    send_only: bool,
    operation_id: u32,
) -> Result<bool, String> {
    if operation_id == 0 {
        return Err(invalid());
    }
    let response = bridge
        .image_request(
            generation,
            Operation::Cancel {
                send_only,
                operation_id,
            },
            None,
        )
        .await?;
    Ok(response.cancel_confirmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_images_cancel_before_host_admission_fences_delayed_send() {
        let order = SendOrder::default();
        assert_eq!(order.cancel(3), CancelOrder::BeforeAdmission);
        assert!(!order.admit(3));
        assert!(!order.admit(2));
        assert!(order.admit(4));
    }

    #[test]
    fn chat_images_cancel_only_matches_active_send_and_never_confirms_finished_send() {
        let order = SendOrder::default();
        assert!(order.admit(3));
        assert_eq!(order.cancel(2), CancelOrder::Unconfirmed);
        assert_eq!(order.cancel(3), CancelOrder::Active);
        order.active.set(None);
        assert_eq!(order.cancel(3), CancelOrder::Unconfirmed);
        assert!(order.admit(4));
    }

    fn packet(parts: &[(u8, &[u8])]) -> Vec<u8> {
        let mut packet = vec![u8::try_from(parts.len()).unwrap()];
        for (tag, bytes) in parts {
            packet.push(*tag);
            packet.extend(u32::try_from(bytes.len()).unwrap().to_le_bytes());
            packet.extend_from_slice(bytes);
        }
        packet
    }

    #[test]
    fn chat_images_decodes_ordered_binary_parts() {
        let parts = decode_packet(&packet(&[(0, b"first"), (3, b"raw"), (0, b"last")])).unwrap();
        assert!(matches!(
            parts.as_slice(),
            [MessageInputPart::Text(first), MessageInputPart::Image { format: MessageImageFormatView::Webp, bytes }, MessageInputPart::Text(last)]
            if first == "first" && bytes == b"raw" && last == "last"
        ));
    }

    #[test]
    fn chat_images_rejects_malformed_and_excess_payloads() {
        let mut trailing = packet(&[(1, b"image")]);
        trailing.push(0);
        let max = vec![1; MAX_MESSAGE_IMAGE_BYTES];
        let excess_text = vec![b'x'; MAX_TEXT_BYTES + 1];
        let excess_image = vec![1; MAX_MESSAGE_IMAGE_BYTES + 1];
        for body in [
            vec![0],
            vec![33],
            vec![1, 1, 255, 255, 255, 255],
            packet(&[(0, b"\xff")]),
            packet(&[(4, b"image")]),
            packet(&[(1, b"")]),
            packet(&[(2, b"x".as_slice()); 5]),
            packet(&[(0, &excess_text)]),
            packet(&[(1, &excess_image)]),
            packet(&[(1, &max), (2, &max), (3, b"x")]),
            trailing,
        ] {
            assert!(decode_packet(&body).is_err());
        }
    }
}
