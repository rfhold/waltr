use std::{
    cell::Cell,
    fmt,
    fs::{self, File},
    future,
    io::Read,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::task::{self, JoinHandle};
use waltr_client::{
    ClientError, ClientSnapshot, MAX_MESSAGE_IMAGE_BYTES, MessageImageFormatView, MessageImageLane,
    MessageInputPart, NativeCredentialProvider,
};

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

pub(crate) fn route_ready(snapshot: &ClientSnapshot) -> bool {
    snapshot.access.status == waltr_client::AccessStatus::Authenticated
        && snapshot.status.ready
        && snapshot.status.watch.status == "received"
}

pub(crate) fn request_id() -> u64 {
    NEXT_REQUEST.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct Image {
    pub format: MessageImageFormatView,
    bytes: Arc<Vec<u8>>,
}

impl Image {
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn label(&self) -> String {
        image_label(self.format, self.len())
    }

    #[cfg(test)]
    pub fn test_png() -> Self {
        Self {
            format: MessageImageFormatView::Png,
            bytes: Arc::new(b"\x89PNG\r\n\x1a\n".to_vec()),
        }
    }
}

impl fmt::Debug for Image {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Image(..)")
    }
}

pub(crate) fn image_label(format: MessageImageFormatView, bytes: usize) -> String {
    let format = match format {
        MessageImageFormatView::Png => "PNG",
        MessageImageFormatView::Jpeg => "JPEG",
        MessageImageFormatView::Webp => "WebP",
    };
    format!("{format}, {bytes} bytes")
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct Request {
    pub id: u64,
    pub operation: Operation,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ImageRequest(..)")
    }
}

#[derive(Clone, Eq, PartialEq)]
pub(crate) enum Operation {
    Load {
        path: PathBuf,
        remaining: usize,
    },
    Send {
        session_id: String,
        text: String,
        images: Vec<Image>,
    },
}

pub(crate) enum Completion {
    Loaded(Result<Image, String>),
    Sent(Result<(), ClientError>, Option<u64>),
}

impl Completion {
    pub fn access_expiry_revision(&self) -> Option<u64> {
        match self {
            Self::Sent(Err(error), revision) if error.is_waltr_login_required() => *revision,
            _ => None,
        }
    }
}

pub(crate) struct Runtime {
    lane: MessageImageLane,
    credentials: NativeCredentialProvider,
    active: Option<Active>,
}

struct Active {
    id: u64,
    send: bool,
    canceled: Rc<Cell<bool>>,
    task: JoinHandle<Completion>,
}

impl Runtime {
    pub fn new(lane: MessageImageLane, credentials: NativeCredentialProvider) -> Self {
        Self {
            lane,
            credentials,
            active: None,
        }
    }

    pub fn start(&mut self, request: Request) -> Result<(), Completion> {
        let send = matches!(request.operation, Operation::Send { .. });
        if self.active.is_some() {
            let error = "Previous image operation is still finishing";
            return if send {
                Err(Completion::Sent(Err(error.into()), None))
            } else {
                Err(Completion::Loaded(Err(error.into())))
            };
        }
        let lane = self.lane.clone();
        let credentials = self.credentials.clone();
        let credential_revision = credentials.credential_revision();
        let canceled = Rc::new(Cell::new(false));
        let task_canceled = canceled.clone();
        let task = task::spawn_local(async move {
            match request.operation {
                Operation::Load { path, remaining } => {
                    if task_canceled.get() {
                        return Completion::Loaded(Err("Image selection canceled".into()));
                    }
                    let result = task::spawn_blocking(move || load_image(&path, remaining))
                        .await
                        .unwrap_or_else(|_| Err("Could not read selected image".into()));
                    Completion::Loaded(if task_canceled.get() {
                        Err("Image selection canceled".into())
                    } else {
                        result
                    })
                }
                Operation::Send {
                    session_id,
                    text,
                    images,
                } => {
                    if task_canceled.get() {
                        return Completion::Sent(Err("Message send canceled".into()), None);
                    }
                    let parts = message_parts(text, images);
                    if credentials.credential_revision() != credential_revision {
                        return Completion::Sent(
                            Err("Waltr access changed before sending".into()),
                            None,
                        );
                    }
                    let (result, expiry_revision) =
                        lane.send_with_access_expiry(session_id, parts).await;
                    Completion::Sent(result, expiry_revision)
                }
            }
        });
        self.active = Some(Active {
            id: request.id,
            send,
            canceled,
            task,
        });
        Ok(())
    }

    pub fn sync_owner(&self, owner: Option<u64>) {
        if let Some(active) = &self.active
            && owner != Some(active.id)
            && !active.canceled.replace(true)
            && active.send
        {
            self.lane.cancel_send();
        }
    }

    pub async fn wait(&mut self) -> (u64, Completion) {
        let Some(active) = &mut self.active else {
            return future::pending().await;
        };
        let result = (&mut active.task).await.unwrap_or_else(|_| {
            if active.send {
                Completion::Sent(
                    Err("Message admission could not be confirmed. Check the timeline before resending.".into()),
                    None,
                )
            } else {
                Completion::Loaded(Err("Could not read selected image".into()))
            }
        });
        let finished = (active.id, result);
        self.active = None;
        finished
    }

    pub async fn shutdown(&mut self) {
        self.sync_owner(None);
        if self.active.as_ref().is_some_and(|active| active.send) {
            drop(tokio::time::timeout(std::time::Duration::from_secs(3), self.wait()).await);
        }
        if let Some(active) = self.active.take() {
            active.task.abort();
        }
    }
}

fn message_parts(text: String, images: Vec<Image>) -> Vec<MessageInputPart> {
    let mut parts = Vec::with_capacity(images.len() + usize::from(!text.is_empty()));
    if !text.is_empty() {
        parts.push(MessageInputPart::Text(text));
    }
    parts.extend(images.into_iter().map(|image| MessageInputPart::Image {
        format: image.format,
        bytes: Arc::unwrap_or_clone(image.bytes),
    }));
    parts
}

const REGULAR_FILE: &str = "Choose a finite regular image file";
const READ_FAILED: &str = "Could not read selected image";

fn check_image_path(path: &Path) -> Result<(), String> {
    if !fs::metadata(path).map_err(|_| READ_FAILED)?.is_file() {
        return Err(REGULAR_FILE.into());
    }
    Ok(())
}

fn load_image(path: &Path, remaining: usize) -> Result<Image, String> {
    check_image_path(path)?;
    read_image(path, remaining)
}

#[cfg(unix)]
fn open_image(path: &Path) -> Result<File, String> {
    use rustix::fs::{Mode, OFlags, open};

    open(
        path,
        OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map(File::from)
    .map_err(|_| READ_FAILED.into())
}

#[cfg(not(unix))]
fn open_image(path: &Path) -> Result<File, String> {
    File::open(path).map_err(|_| READ_FAILED.into())
}

fn read_image(path: &Path, remaining: usize) -> Result<Image, String> {
    let mut file = open_image(path)?;
    let metadata = file.metadata().map_err(|_| READ_FAILED)?;
    if !metadata.is_file() {
        return Err(REGULAR_FILE.into());
    }
    let len = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if len == 0 || len > MAX_MESSAGE_IMAGE_BYTES {
        return Err("Each image must contain between 1 byte and 8 MiB".into());
    }
    if len > remaining {
        return Err("Images in one message must total no more than 16 MiB".into());
    }
    let mut bytes = vec![0; len];
    let mut extra = [0];
    if file.read_exact(&mut bytes).is_err() || file.read(&mut extra).map_err(|_| READ_FAILED)? != 0
    {
        return Err("Selected image changed length or could not be read".into());
    }
    let format = match bytes.as_slice() {
        [137, b'P', b'N', b'G', 13, 10, 26, 10, ..] => MessageImageFormatView::Png,
        [0xff, 0xd8, 0xff, ..] => MessageImageFormatView::Jpeg,
        [
            b'R',
            b'I',
            b'F',
            b'F',
            _,
            _,
            _,
            _,
            b'W',
            b'E',
            b'B',
            b'P',
            ..,
        ] => MessageImageFormatView::Webp,
        _ => return Err("Choose a PNG, JPEG, or WebP image".into()),
    };
    Ok(Image {
        format,
        bytes: Arc::new(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n";
    const JPEG: &[u8] = b"\xff\xd8\xff";

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "waltr-tui-image-{}-{}",
                std::process::id(),
                request_id()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            fs::remove_file(self.0.join("selected")).unwrap();
            fs::remove_dir(&self.0).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn fifo_replacing_a_regular_file_is_rejected_without_waiting_for_a_writer() {
        use std::{process::Command, sync::mpsc, thread, time::Duration};

        let scratch = Scratch::new();
        let selected = scratch.0.join("selected");
        fs::write(&selected, PNG).unwrap();
        check_image_path(&selected).unwrap();
        fs::remove_file(&selected).unwrap();
        assert!(
            Command::new("mkfifo")
                .arg(&selected)
                .status()
                .unwrap()
                .success()
        );

        let (send, receive) = mpsc::channel();
        let worker = thread::spawn(move || send.send(read_image(&selected, PNG.len())));
        let result = receive.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(result.unwrap_err(), REGULAR_FILE);
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn selection_checks_signature_and_byte_budget_without_exposing_path() {
        let scratch = Scratch::new();
        let selected = scratch.0.join("selected");
        fs::write(&selected, PNG).unwrap();
        assert_eq!(
            load_image(&selected, PNG.len()).unwrap().format,
            MessageImageFormatView::Png
        );
        assert!(
            load_image(&scratch.0, usize::MAX)
                .unwrap_err()
                .contains("regular")
        );
        assert!(
            load_image(&selected, PNG.len() - 1)
                .unwrap_err()
                .contains("total")
        );
        fs::write(&selected, b"<svg/>").unwrap();
        assert_eq!(
            load_image(&selected, usize::MAX).unwrap_err(),
            "Choose a PNG, JPEG, or WebP image"
        );
        File::options()
            .write(true)
            .open(&selected)
            .unwrap()
            .set_len((MAX_MESSAGE_IMAGE_BYTES + 1) as u64)
            .unwrap();
        assert!(
            load_image(&selected, usize::MAX)
                .unwrap_err()
                .contains("8 MiB")
        );
    }

    #[test]
    fn ordered_parts_allow_image_only_or_text_then_images() {
        let images = [
            Image {
                format: MessageImageFormatView::Jpeg,
                bytes: Arc::new(JPEG.to_vec()),
            },
            Image {
                format: MessageImageFormatView::Png,
                bytes: Arc::new(PNG.to_vec()),
            },
        ];
        let mixed = message_parts("compare".into(), images.to_vec());
        let only_images = message_parts(String::new(), images.to_vec());
        assert!(matches!(&mixed[0], MessageInputPart::Text(text) if text == "compare"));
        for parts in [&mixed[1..], only_images.as_slice()] {
            assert!(matches!(parts, [
                MessageInputPart::Image { format: MessageImageFormatView::Jpeg, bytes: first },
                MessageInputPart::Image { format: MessageImageFormatView::Png, bytes: second }
            ] if first == JPEG && second == PNG));
        }
    }
}
