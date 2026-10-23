//! Strict startup configuration. Paths are independent of server credentials.

use crate::command::CommandSpec;
use serde::Deserialize;
use std::{
    collections::HashSet,
    fmt,
    io::Read,
    path::{Path, PathBuf},
};

/// Maximum startup YAML size, in bytes.
pub const MAX_CONFIG_BYTES: usize = 256 * 1024;
/// Maximum number of startup bindings.
pub const MAX_KEYBINDS: usize = 64;
/// Maximum identifier, chord, or encoded device-selector size, in bytes.
pub const MAX_FIELD_BYTES: usize = 256;

/// Errors never include YAML values or potentially sensitive file contents.
#[derive(Debug)]
pub enum ConfigError {
    /// The selected configuration root is missing or not absolute.
    Path(&'static str),
    /// File access failed; the underlying message is intentionally omitted.
    Io(std::io::ErrorKind),
    /// Input exceeds the file-size limit.
    TooLarge,
    /// Invalid YAML or strict schema, retaining only a safe source location.
    Syntax {
        /// One-based line, if available.
        line: Option<usize>,
        /// One-based column, if available.
        column: Option<usize>,
    },
    /// Semantic validation failed with a static, nonsensitive reason.
    Invalid(&'static str),
}
impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "desktop configuration: {self:?}")
    }
}
impl std::error::Error for ConfigError {}

/// XDG paths must be absolute. Empty XDG is treated as unset; no ambient access.
///
/// # Errors
/// Returns `Path` when the selected root is missing or relative.
pub fn config_path(
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf, ConfigError> {
    let base = match xdg_config_home.filter(|path| !path.as_os_str().is_empty()) {
        Some(path) => path.to_path_buf(),
        None => home
            .ok_or(ConfigError::Path("HOME unavailable"))?
            .join(".config"),
    };
    if !base.is_absolute() {
        return Err(ConfigError::Path("configuration root must be absolute"));
    }
    Ok(base.join("waltr/client.yaml"))
}

/// Resolve and read once at startup. Missing files disable all bindings.
///
/// # Errors
/// Returns path, I/O, size, schema, or validation errors without fallback.
pub fn load_ambient() -> Result<Config, ConfigError> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME");
    let home = std::env::var_os("HOME");
    load_path(&config_path(
        xdg.as_deref().map(Path::new),
        home.as_deref().map(Path::new),
    )?)
}

/// Read an explicit path without consulting the environment.
///
/// # Errors
/// Returns I/O, size, schema, or validation errors. Missing files are valid.
pub fn load_path(path: &Path) -> Result<Config, ConfigError> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(ConfigError::Io(error.kind())),
    };
    let mut bytes = Vec::new();
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| ConfigError::Io(e.kind()))?;
    parse(&bytes)
}

/// Parse and validate bounded YAML, retaining only safe error locations.
///
/// # Errors
/// Returns size, schema, or validation errors.
pub fn parse(bytes: &[u8]) -> Result<Config, ConfigError> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge);
    }
    let config: Config = serde_yaml_ng::from_slice(bytes).map_err(|error| ConfigError::Syntax {
        line: error.location().map(|location| location.line()),
        column: error.location().map(|location| location.column()),
    })?;
    config.validate()?;
    Ok(config)
}

/// Startup snapshot; deserialization must be followed by [`Self::validate`].
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Only version 1 is supported.
    pub version: u32,
    /// Local bindings; an empty list disables integration.
    #[serde(deserialize_with = "crate::bounded::vec::<_, _, MAX_KEYBINDS>")]
    pub keybinds: Vec<Keybind>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            keybinds: Vec::new(),
        }
    }
}
impl Config {
    /// Validate schema semantics without server I/O or backend registration.
    ///
    /// # Errors
    /// Returns `Invalid` for unsupported values, duplicates, or bounds violations.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.version != 1 {
            return Err(ConfigError::Invalid("unsupported version"));
        }
        if self.keybinds.len() > MAX_KEYBINDS {
            return Err(ConfigError::Invalid("too many keybinds"));
        }
        let mut ids = HashSet::new();
        let mut chords = HashSet::new();
        for binding in &self.keybinds {
            field(&binding.id)?;
            if !ids.insert(&binding.id) {
                return Err(ConfigError::Invalid("duplicate keybind ID"));
            }
            validate_chord(&binding.key)?;
            if !chords.insert(&binding.key) {
                return Err(ConfigError::Invalid("duplicate key chord"));
            }
            for value in [
                &binding.routine.name,
                &binding.routine.trigger,
                &binding.routine.input,
                &binding.routine.output,
            ] {
                field(value)?;
            }
            match &binding.audio.device {
                DeviceSelector::Default => {}
                DeviceSelector::Id(value) | DeviceSelector::Name(value) => field(value)?,
            }
            if !(8_000..=192_000).contains(&binding.audio.sample_rate_hz)
                || !(1..=2).contains(&binding.audio.channels)
            {
                return Err(ConfigError::Invalid("invalid PCM_S16LE audio format"));
            }
            match (&binding.command, &binding.typing) {
                (Some(command), None) => command
                    .validate()
                    .map_err(|_| ConfigError::Invalid("invalid command or template"))?,
                (None, Some(_)) => (),
                _ => {
                    return Err(ConfigError::Invalid(
                        "exactly one command or typing is required",
                    ));
                }
            }
        }
        Ok(())
    }
}

fn field(value: &str) -> Result<(), ConfigError> {
    if value.is_empty()
        || value.trim() != value
        || value.len() > MAX_FIELD_BYTES
        || value.chars().any(char::is_control)
    {
        Err(ConfigError::Invalid(
            "empty, oversized, or control-containing field",
        ))
    } else {
        Ok(())
    }
}

/// Validate portable key chord grammar.
///
/// Unique Ctrl/Alt/Shift/Super modifiers in that order, then an ASCII
/// letter/digit, F1..F24, Space, Enter, Escape, Tab, Backspace,
/// Delete, Insert, Home, End, `PageUp`, `PageDown`, Left, Right, Up, or Down.
///
/// Registration and platform availability remain the backend's responsibility.
///
/// # Errors
/// Returns `Invalid` for invalid key names, ordering, or repeated modifiers.
pub fn validate_chord(key: &str) -> Result<(), ConfigError> {
    field(key)?;
    let mut parts = key.split('+').peekable();
    let mut previous = None;
    while let Some(part) = parts.next() {
        if parts.peek().is_none() {
            let valid = (part.len() == 1 && part.bytes().all(|b| b.is_ascii_alphanumeric()))
                || part
                    .strip_prefix('F')
                    .and_then(|s| s.parse::<u8>().ok())
                    .is_some_and(|n| (1..=24).contains(&n) && part == format!("F{n}"))
                || [
                    "Space",
                    "Enter",
                    "Escape",
                    "Tab",
                    "Backspace",
                    "Delete",
                    "Insert",
                    "Home",
                    "End",
                    "PageUp",
                    "PageDown",
                    "Left",
                    "Right",
                    "Up",
                    "Down",
                ]
                .contains(&part);
            return if valid {
                Ok(())
            } else {
                Err(ConfigError::Invalid("invalid key"))
            };
        }
        let index = ["Ctrl", "Alt", "Shift", "Super"]
            .iter()
            .position(|modifier| *modifier == part)
            .ok_or(ConfigError::Invalid("invalid modifier"))?;
        if previous.is_some_and(|last| index <= last) {
            return Err(ConfigError::Invalid("noncanonical or duplicate modifier"));
        }
        previous = Some(index);
    }
    Err(ConfigError::Invalid("invalid key chord"))
}

/// One local hold-to-invoke binding, not registered by this library.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keybind {
    /// Unique local binding identifier.
    #[serde(deserialize_with = "crate::bounded::string::<_, MAX_FIELD_BYTES>")]
    pub id: String,
    /// Portable chord validated by [`validate_chord`].
    #[serde(deserialize_with = "crate::bounded::string::<_, MAX_FIELD_BYTES>")]
    pub key: String,
    /// Initial interaction mode.
    pub mode: Mode,
    /// User-selected remote identifiers; no discovery is performed.
    pub routine: Routine,
    /// Requested native capture format and device.
    pub audio: Audio,
    /// Trusted local command, to be invoked only after successful completion.
    #[serde(default, deserialize_with = "present_action")]
    pub command: Option<CommandSpec>,
    /// Stream live text hypotheses to the current keyboard focus. Exclusive with command.
    #[serde(default, deserialize_with = "present_action")]
    pub typing: Option<TypingSpec>,
}
fn present_action<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| serde::de::Error::custom("action must not be null"))
}

/// Version-one typing policy has no configurable fallback or draft controls.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypingSpec {}
/// Supported interaction modes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Begin on press and finalize on release.
    Hold,
}
/// User-selected Routine identifiers, independent of examples or server state.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routine {
    /// Routine resource name.
    #[serde(deserialize_with = "crate::bounded::string::<_, MAX_FIELD_BYTES>")]
    pub name: String,
    /// Interaction trigger identifier.
    #[serde(deserialize_with = "crate::bounded::string::<_, MAX_FIELD_BYTES>")]
    pub trigger: String,
    /// Input port identifier.
    #[serde(deserialize_with = "crate::bounded::string::<_, MAX_FIELD_BYTES>")]
    pub input: String,
    /// Final text output port identifier.
    #[serde(deserialize_with = "crate::bounded::string::<_, MAX_FIELD_BYTES>")]
    pub output: String,
}
/// Unambiguous backend-independent device request, encoded as a YAML string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceSelector {
    /// Backend default device (`default`).
    Default,
    /// Exact backend device identifier (`id:<id>`).
    Id(String),
    /// Exact device display name (`name:<name>`).
    Name(String),
}
impl<'de> Deserialize<'de> for DeviceSelector {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = crate::bounded::string::<D, MAX_FIELD_BYTES>(deserializer)?;
        if value == "default" {
            return Ok(Self::Default);
        }
        for (prefix, constructor) in [
            ("id:", Self::Id as fn(String) -> Self),
            ("name:", Self::Name as fn(String) -> Self),
        ] {
            if let Some(selected) = value.strip_prefix(prefix) {
                field(selected).map_err(serde::de::Error::custom)?;
                return Ok(constructor(selected.to_owned()));
            }
        }
        Err(serde::de::Error::custom(
            "device must be default, id:<id>, or name:<name>",
        ))
    }
}

/// V1 format is interleaved signed 16-bit little-endian PCM.
/// Device selectors are `default`, `id:<backend-id>`, or `name:<exact-name>`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Audio {
    /// Requested capture device.
    pub device: DeviceSelector,
    /// Sample rate in 8,000..=192,000 Hz; backend support is checked later.
    pub sample_rate_hz: u32,
    /// One or two interleaved channels.
    pub channels: u16,
}
