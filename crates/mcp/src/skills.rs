use std::collections::{BTreeMap, HashSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest as _, Sha256};

use crate::{
    Error, McpResourceResult, Result,
    server::{ServerError, ServerResult},
};

/// Official extension identifier.
pub const SKILLS_EXTENSION: &str = "io.modelcontextprotocol/skills";
/// Maximum files accepted per skill.
pub const MAX_SKILL_FILES: usize = 512;
/// Maximum aggregate raw bytes accepted per skill.
pub const MAX_SKILL_BYTES: u64 = 16 * 1024 * 1024;

/// Official server extension settings. Directory reading is not implemented by this library.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSkillsCapability {
    /// Whether the peer additionally implements `resources/directory/read`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub directory_read: bool,
}

/// One raw-byte manifest entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSkillResource {
    /// Absolute resource URI within the skill root.
    pub uri: String,
    /// `sha256:` followed by 64 lowercase hexadecimal digits.
    pub digest: String,
    /// Length of the raw bytes, before any transport encoding.
    pub size: u64,
}

/// Complete manifest or explicitly unverifiable dynamic content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpSkillResources {
    /// Complete point-in-time resource manifest.
    Files(Vec<McpSkillResource>),
    /// Explicitly dynamic, unverifiable resource set.
    Dynamic(DynamicResources),
}

/// The only permitted dynamic manifest marker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DynamicResources {
    /// Official dynamic marker.
    #[serde(rename = "dynamic")]
    Dynamic,
}

/// A skill is identified by its URI within its originating server, never by name alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpSkill {
    /// Absolute entrypoint URI ending in `/SKILL.md`.
    pub uri: String,
    /// Verbatim YAML frontmatter represented as a JSON object.
    pub frontmatter: Map<String, Value>,
    /// Complete manifest or dynamic marker.
    pub resources: McpSkillResources,
}

/// Standard complete result discriminator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompleteResult {
    /// Fully materialized result, never a partial patch.
    #[serde(rename = "complete")]
    Complete,
}

/// Cache scope is a freshness policy, not an integrity guarantee.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SkillCacheScope {
    /// May be shared across callers.
    Public,
    /// Bound to the requesting caller.
    Private,
}

/// One atomic page of skill entries, including required cache metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSkillList {
    /// Complete page discriminator.
    pub result_type: CompleteResult,
    /// Skills on this page.
    pub skills: Vec<McpSkill>,
    /// Opaque continuation cursor, absent on the final page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Freshness lifetime in milliseconds.
    pub ttl_ms: u64,
    /// Permitted cache sharing scope.
    pub cache_scope: SkillCacheScope,
    /// Additional result metadata, preserved for callers.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

/// One point-in-time skill entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSkillResult {
    /// Complete entry discriminator.
    pub result_type: CompleteResult,
    /// Held point-in-time skill entry.
    pub skill: McpSkill,
    /// Freshness lifetime in milliseconds.
    pub ttl_ms: u64,
    /// Permitted cache sharing scope.
    pub cache_scope: SkillCacheScope,
    /// Additional result metadata, preserved for callers.
    #[serde(rename = "_meta", default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Map<String, Value>>,
}

fn invalid(message: impl Into<String>) -> Error {
    Error::protocol(message)
}

fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && path.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && !part
                    .chars()
                    .any(|c| c.is_control() || matches!(c, '\\' | '%' | '?' | '#'))
        })
}

fn encode_path(path: &str) -> String {
    let mut encoded = String::new();
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            encoded.push(byte as char);
        } else {
            use std::fmt::Write as _;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn uri_segment(segment: &str) -> Option<String> {
    let mut decoded = Vec::new();
    let mut bytes = segment.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let high = (bytes.next()? as char).to_digit(16)?;
            let low = (bytes.next()? as char).to_digit(16)?;
            (high * 16 + low) as u8
        } else {
            byte
        });
    }
    let decoded = String::from_utf8(decoded).ok()?;
    (!decoded.is_empty()
        && decoded != "."
        && decoded != ".."
        && !decoded
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\')))
    .then_some(decoded)
}

fn safe_uri_relative(path: &str) -> bool {
    !path
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '\\' | '?' | '#'))
        && path.split('/').all(|part| uri_segment(part).is_some())
}

fn json_representable(value: &serde_yaml_ng::Value) -> bool {
    use serde_yaml_ng::Value as Yaml;
    match value {
        Yaml::Null | Yaml::Bool(_) | Yaml::String(_) => true,
        Yaml::Number(number) => number.as_f64().is_none_or(f64::is_finite),
        Yaml::Sequence(items) => items.iter().all(json_representable),
        Yaml::Mapping(map) => map
            .iter()
            .all(|(key, value)| key.as_str().is_some() && json_representable(value)),
        Yaml::Tagged(_) => false,
    }
}

/// Parse frontmatter without discarding author-defined fields.
pub fn parse_skill_frontmatter(bytes: &[u8]) -> Result<Map<String, Value>> {
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("SKILL.md must be UTF-8"))?;
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim_end) != Some("---") {
        return Err(invalid("SKILL.md must begin with YAML frontmatter"));
    }
    let mut yaml = String::new();
    let mut closed = false;
    for line in lines {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
    }
    if !closed {
        return Err(invalid("unterminated skill frontmatter"));
    }
    let yaml_value: serde_yaml_ng::Value =
        serde_yaml_ng::from_str(&yaml).map_err(|e| invalid(format!("invalid skill YAML: {e}")))?;
    if !json_representable(&yaml_value) {
        return Err(invalid("frontmatter must be losslessly JSON-representable"));
    }
    let value = serde_json::to_value(yaml_value)
        .map_err(|e| invalid(format!("frontmatter is not JSON-representable: {e}")))?;
    let object = value
        .as_object()
        .cloned()
        .ok_or_else(|| invalid("frontmatter must be an object"))?;
    validate_frontmatter(&object)?;
    Ok(object)
}

fn validate_frontmatter(object: &Map<String, Value>) -> Result<()> {
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("skill name must be a string"))?;
    if name.is_empty()
        || name.chars().count() > 64
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
        || !name
            .chars()
            .all(|c| c == '-' || c.is_lowercase() || c.is_numeric())
    {
        return Err(invalid("invalid skill name"));
    }
    let description = object
        .get("description")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("skill description must be a string"))?;
    if description.trim().is_empty() || description.chars().count() > 1024 {
        return Err(invalid("invalid skill description"));
    }
    for field in ["license", "allowed-tools", "compatibility"] {
        if let Some(value) = object.get(field) {
            let text = value
                .as_str()
                .ok_or_else(|| invalid(format!("{field} must be a string")))?;
            if field == "compatibility" && (text.is_empty() || text.chars().count() > 500) {
                return Err(invalid("invalid compatibility"));
            }
        }
    }
    if let Some(metadata) = object.get("metadata") {
        if !metadata
            .as_object()
            .is_some_and(|map| map.values().all(Value::is_string))
        {
            return Err(invalid("metadata must map strings to strings"));
        }
    }
    Ok(())
}

/// Validate an entry before offering it for approval or reading its files.
pub fn validate_skill(skill: &McpSkill) -> Result<()> {
    validate_frontmatter(&skill.frontmatter)?;
    let root = skill
        .uri
        .strip_suffix("/SKILL.md")
        .ok_or_else(|| invalid("skill URI must end in /SKILL.md"))?;
    let (_, path) = root
        .split_once(':')
        .ok_or_else(|| invalid("skill URI must be absolute"))?;
    let path = path.strip_prefix("//").unwrap_or(path);
    let parsed = url::Url::parse(&skill.uri).map_err(|_| invalid("invalid skill URI"))?;
    let segments = path.split_once('/').map_or("", |(_, path)| path);
    if parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || (!segments.is_empty() && !safe_uri_relative(segments))
        || (!path.starts_with('/') && uri_segment(path.split('/').next().unwrap()).is_none())
        || path
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
        || uri_segment(path.rsplit('/').next().unwrap()).as_deref()
            != skill.frontmatter["name"].as_str()
    {
        return Err(invalid("skill URI root/name mismatch"));
    }
    if let McpSkillResources::Files(files) = &skill.resources {
        if files.len() > MAX_SKILL_FILES {
            return Err(invalid("skill exceeds file limit"));
        }
        let prefix = format!("{root}/");
        let mut seen = HashSet::new();
        let mut size = 0u64;
        for file in files {
            let relative = file
                .uri
                .strip_prefix(&prefix)
                .ok_or_else(|| invalid("manifest resource outside skill root"))?;
            if !safe_uri_relative(relative) || !seen.insert(&file.uri) {
                return Err(invalid("invalid or duplicate manifest URI"));
            }
            let hex = file
                .digest
                .strip_prefix("sha256:")
                .ok_or_else(|| invalid("invalid manifest digest"))?;
            if hex.len() != 64
                || !hex
                    .bytes()
                    .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            {
                return Err(invalid("invalid manifest digest"));
            }
            size = size
                .checked_add(file.size)
                .ok_or_else(|| invalid("manifest size overflow"))?;
        }
        if !seen.contains(&skill.uri) || size > MAX_SKILL_BYTES {
            return Err(invalid("missing SKILL.md or skill exceeds byte limit"));
        }
    }
    Ok(())
}

fn digest(bytes: &[u8]) -> String {
    let mut digest = String::from("sha256:");
    for byte in Sha256::digest(bytes) {
        use std::fmt::Write as _;
        let _ = write!(digest, "{byte:02x}");
    }
    digest
}

/// Verify held-manifest membership, raw length/digest and SKILL.md frontmatter.
/// This does not establish trust or confer approval; origins and permissions belong to the host.
pub fn verify_skill_bytes(skill: &McpSkill, uri: &str, bytes: &[u8]) -> Result<()> {
    validate_skill(skill)?;
    let McpSkillResources::Files(files) = &skill.resources else {
        return Err(invalid("dynamic skills cannot be integrity verified"));
    };
    let file = files
        .iter()
        .find(|file| file.uri == uri)
        .ok_or_else(|| invalid("unlisted skill resource"))?;
    if file.size != bytes.len() as u64 || file.digest != digest(bytes) {
        return Err(invalid("skill content integrity mismatch"));
    }
    if uri == skill.uri && parse_skill_frontmatter(bytes)? != skill.frontmatter {
        return Err(invalid("skill frontmatter mismatch"));
    }
    Ok(())
}

/// Immutable snapshot built from explicit embedded files. No runtime filesystem access occurs.
#[derive(Debug, Clone, Default)]
pub struct SkillCatalog {
    skills: BTreeMap<String, McpSkill>,
    files: BTreeMap<String, Vec<u8>>,
}

impl SkillCatalog {
    /// Create an empty immutable catalog builder.
    pub fn new() -> Self {
        Self::default()
    }
    /// Whether any skills have been published.
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    /// Register every file under a skill root. Paths are relative, e.g. `SKILL.md`.
    /// `include_bytes!` provides compile-time embedding; supplied bytes are copied once.
    pub fn with_skill(mut self, path: &str, files: &[(&str, &[u8])]) -> Result<Self> {
        if !safe_path(path) {
            return Err(invalid("invalid skill root"));
        }
        let root = format!("skill://{}", encode_path(path));
        let uri = format!("{root}/SKILL.md");
        if self.skills.contains_key(&uri) {
            return Err(invalid("duplicate skill URI"));
        }
        if files.len() > MAX_SKILL_FILES
            || files
                .iter()
                .try_fold(0u64, |total, (_, bytes)| {
                    total.checked_add(bytes.len() as u64)
                })
                .is_none_or(|total| total > MAX_SKILL_BYTES)
        {
            return Err(invalid("skill exceeds file or byte limit"));
        }
        let mut snapshot = BTreeMap::new();
        for (relative, bytes) in files {
            if !safe_path(relative) {
                return Err(invalid("invalid skill file path"));
            }
            let file_uri = format!("{root}/{}", encode_path(relative));
            if snapshot.insert(file_uri, bytes.to_vec()).is_some() {
                return Err(invalid("duplicate skill file URI"));
            }
        }
        let bytes = snapshot
            .get(&uri)
            .ok_or_else(|| invalid("skill requires root SKILL.md"))?;
        let skill = McpSkill {
            uri: uri.clone(),
            frontmatter: parse_skill_frontmatter(bytes)?,
            resources: McpSkillResources::Files(
                snapshot
                    .iter()
                    .map(|(uri, bytes)| McpSkillResource {
                        uri: uri.clone(),
                        digest: digest(bytes),
                        size: bytes.len() as u64,
                    })
                    .collect(),
            ),
        };
        validate_skill(&skill)?;
        for (uri, bytes) in &snapshot {
            if self
                .files
                .get(uri)
                .is_some_and(|existing| existing != bytes)
            {
                return Err(invalid("conflicting resource URI"));
            }
        }
        // Nested publications share bytes; each enclosing manifest must still be complete.
        for existing in self.skills.values() {
            let existing_root = existing.uri.strip_suffix("SKILL.md").unwrap();
            if snapshot
                .keys()
                .any(|uri| uri.starts_with(existing_root) && !self.files.contains_key(uri))
            {
                return Err(invalid(
                    "nested files must already occur in enclosing manifest",
                ));
            }
        }
        for file_uri in self
            .files
            .keys()
            .filter(|file_uri| file_uri.starts_with(&format!("{root}/")))
        {
            if !snapshot.contains_key(file_uri) {
                return Err(invalid("enclosing manifest omits nested file"));
            }
        }
        self.files.extend(snapshot);
        self.skills.insert(uri, skill);
        Ok(self)
    }

    /// Deterministic pagination with numeric continuation cursors.
    pub fn list(&self, cursor: Option<String>) -> ServerResult<McpSkillList> {
        let offset = match cursor {
            None => 0,
            Some(cursor) => cursor
                .strip_prefix("skills:")
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|n| *n > 0 && *n < self.skills.len())
                .ok_or_else(|| ServerError::invalid_params("invalid skill cursor"))?,
        };
        let skills = self
            .skills
            .values()
            .skip(offset)
            .take(32)
            .cloned()
            .collect::<Vec<_>>();
        let next = offset + skills.len();
        Ok(McpSkillList {
            result_type: CompleteResult::Complete,
            skills,
            next_cursor: (next < self.skills.len()).then(|| format!("skills:{next}")),
            ttl_ms: 0,
            cache_scope: SkillCacheScope::Private,
            meta: None,
        })
    }

    /// Fetch an exact entrypoint; unknown URIs are invalid parameters.
    pub fn get(&self, uri: &str) -> ServerResult<McpSkillResult> {
        let skill =
            self.skills.get(uri).cloned().ok_or_else(|| {
                ServerError::invalid_params(format!("No skill is served at {uri}"))
            })?;
        Ok(McpSkillResult {
            result_type: CompleteResult::Complete,
            skill,
            ttl_ms: 0,
            cache_scope: SkillCacheScope::Private,
            meta: None,
        })
    }

    /// Return exact text bytes when UTF-8, otherwise base64 raw bytes.
    pub fn read(&self, uri: &str) -> Option<McpResourceResult> {
        let bytes = self.files.get(uri)?;
        let mime = if uri.ends_with(".md") {
            "text/markdown"
        } else if std::str::from_utf8(bytes).is_ok() {
            "text/plain"
        } else {
            "application/octet-stream"
        };
        let content = match std::str::from_utf8(bytes) {
            Ok(text) => json!({"uri": uri, "mimeType": mime, "text": text}),
            Err(_) => json!({"uri": uri, "mimeType": mime, "blob": STANDARD.encode(bytes)}),
        };
        Some(McpResourceResult::new(
            json!({"resultType": "complete", "contents": [content], "ttlMs": 0, "cacheScope": "private"}),
        ))
    }
}
