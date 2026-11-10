use super::{CHUNK, LocalSelection, failure};
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};
use waltr_client::{
    ClientManualRoutineDefinition, RoutineDirection, RoutineMedia,
    interaction::{InteractionLimits, wire},
};

pub(super) struct LocalInput {
    pub declaration: wire::InputDeclaration,
    pub text: Option<String>,
    file: Option<File>,
    remaining: u64,
}
impl LocalInput {
    pub fn bytes(&self) -> u64 {
        self.text
            .as_ref()
            .map_or(self.remaining, |text| text.len() as u64)
    }
    pub fn open(
        selection: &LocalSelection,
        definition: &ClientManualRoutineDefinition,
        limits: &InteractionLimits,
    ) -> Result<Self, String> {
        let port = definition
            .ports
            .iter()
            .find(|p| p.name == selection.port && p.direction == RoutineDirection::Input)
            .ok_or_else(failure)?;
        let mut input = Self {
            declaration: wire::InputDeclaration {
                port: port.name.clone(),
                format: None,
            },
            text: None,
            file: None,
            remaining: 0,
        };
        let format = if port.media == RoutineMedia::Text {
            if selection.path.is_some() || selection.mime.is_some() || selection.pcm24k_mono {
                return Err(failure());
            }
            let text = selection.text.clone().ok_or_else(failure)?;
            if text.len() as u64 > port.max_bytes.0 || text.len() > limits.text_bytes {
                return Err(failure());
            }
            input.text = Some(text);
            wire::input_declaration::Format::Text(wire::TextFormat {})
        } else {
            if selection.text.is_some() {
                return Err(failure());
            }
            let path = PathBuf::from(
                selection
                    .path
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(failure)?,
            );
            if !std::fs::metadata(&path).map_err(|_| failure())?.is_file() {
                return Err("Only finite regular local files are supported".into());
            }
            let file = File::open(&path)
                .map_err(|_| "Cannot open explicitly selected local file".to_owned())?;
            let metadata = file.metadata().map_err(|_| failure())?;
            if !metadata.is_file() {
                return Err("Only finite regular local files are supported".into());
            }
            let size = metadata.len();
            if size > port.max_bytes.0 || size > limits.server.input_bytes {
                return Err("Selected input exceeds the admitted port limit".into());
            }
            input.remaining = size;
            input.file = Some(file);
            match port.media {
                RoutineMedia::Audio => {
                    if !selection.pcm24k_mono || selection.mime.is_some() || size % 2 != 0 {
                        return Err(
                            "Audio file requires explicit raw PCM16LE 24 kHz mono metadata".into(),
                        );
                    }
                    wire::input_declaration::Format::Pcm16le(wire::Pcm16Le {
                        sample_rate: 24000,
                        channels: 1,
                    })
                }
                RoutineMedia::File | RoutineMedia::Image => {
                    if selection.pcm24k_mono {
                        return Err(failure());
                    }
                    let mime = selection.mime.clone().ok_or_else(failure)?;
                    let purpose = if port.media == RoutineMedia::File {
                        if size > 64 * 1024 * 1024 || size > limits.server.file_bytes {
                            return Err("File exceeds 64 MiB or server limit".into());
                        }
                        wire::binary_input_declaration::Purpose::File(wire::FilePurpose {})
                    } else {
                        if size > 20 * 1024 * 1024 || size > limits.server.image_bytes {
                            return Err("Image exceeds 20 MiB or server limit".into());
                        }
                        let format = match mime.as_str() { "image/png" => wire::ImageFormat::Png, "image/jpeg" => wire::ImageFormat::Jpeg, "image/webp" => wire::ImageFormat::Webp, _ => return Err("Images require PNG, JPEG, or WebP metadata; server validates actual bytes".into()) };
                        wire::binary_input_declaration::Purpose::Image(format as i32)
                    };
                    let display_name = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .filter(|s| !s.is_empty() && s.len() <= 255)
                        .ok_or_else(failure)?
                        .to_owned();
                    wire::input_declaration::Format::Binary(wire::BinaryInputDeclaration {
                        display_name,
                        content_type: mime,
                        declared_bytes: Some(size),
                        expected_sha256: None,
                        purpose: Some(purpose),
                    })
                }
                RoutineMedia::Text => return Err(failure()),
            }
        };
        input.declaration.format = Some(format);
        Ok(input)
    }
    pub fn read(&mut self, bytes: &mut [u8]) -> Result<usize, String> {
        if bytes.is_empty() || bytes.len() > CHUNK {
            return Err(failure());
        }
        let file = self.file.as_mut().ok_or_else(failure)?;
        let max = bytes
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        if max == 0 {
            let mut extra = [0];
            if file.read(&mut extra).map_err(|_| failure())? != 0 {
                return Err("Selected file changed length".into());
            }
            return Ok(0);
        }
        file.read_exact(&mut bytes[..max])
            .map_err(|_| "Selected file became shorter than its declaration".to_owned())?;
        self.remaining -= max as u64;
        Ok(max)
    }
}

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);
pub(super) struct Destination {
    path: PathBuf,
    staging: PathBuf,
    file: Option<File>,
}
impl Destination {
    pub fn new(path: &str) -> Result<Self, String> {
        let path = PathBuf::from(path);
        if path.file_name().is_none() {
            return Err(failure());
        }
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let unique = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(
            ".waltr-tui-{}-{}-{unique}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| failure())?
                .as_nanos()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder
            .create(&staging)
            .map_err(|_| "Cannot create private save staging directory".to_owned())?;
        let mut destination = Self {
            path,
            staging,
            file: None,
        };
        destination.file = Some(
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination.staging.join("content"))
                .map_err(|_| failure())?,
        );
        Ok(destination)
    }
    pub fn write(&mut self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() > CHUNK {
            return Err(failure());
        }
        self.file
            .as_mut()
            .ok_or_else(failure)?
            .write_all(bytes)
            .map_err(|_| failure())
    }
    pub fn publish(&mut self) -> Result<(), String> {
        self.file
            .as_mut()
            .ok_or_else(failure)?
            .sync_all()
            .map_err(|_| failure())?;
        std::fs::hard_link(self.staging.join("content"), &self.path).map_err(|_| {
            "Save destination exists or cannot be published; nothing was overwritten".to_owned()
        })
    }
}
impl Drop for Destination {
    fn drop(&mut self) {
        self.file.take();
        // Never unlink the chosen destination, including a replacement at that path.
        let _removed = std::fs::remove_file(self.staging.join("content"));
        let _removed = std::fs::remove_dir(&self.staging);
    }
}
