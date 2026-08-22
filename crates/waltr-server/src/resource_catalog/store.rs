//! Canonical resource identity, persisted trees, and conflict-safe slot operations.

use waltr_agent::{AgentDefinitionResourceKind, inspect_agent_definition_source};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use super::routine::MAX_ROUTINE_SOURCE_BYTES;
use super::state::{AgentDefinitionLoadError, ResourceKind};

const ABSENT_ETAG_SEED: &[u8] = b"waltr-agent-definition-absent-slot-v1";

#[derive(Debug)]
pub(super) struct ResourceKey {
    pub(super) kind: ResourceKind,
    pub(super) name: String,
}

#[derive(Clone, Copy)]
pub(super) enum PersistedSlotPostcondition<'a> {
    Exact(&'a str),
    Absent,
}

impl PersistedSlotPostcondition<'_> {
    pub(super) fn matches(self, source: Option<&String>) -> bool {
        match self {
            Self::Exact(expected) => source.is_some_and(|source| source == expected),
            Self::Absent => source.is_none(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceHeader {
    api_version: String,
    kind: String,
    metadata: ResourceMetadata,
    spec: serde_yaml_ng::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceMetadata {
    name: String,
}

pub(super) fn validate_resource_source(
    path: &str,
    source: &str,
) -> Result<ResourceKey, AgentDefinitionLoadError> {
    let (expected_kind, expected_name) = validate_resource_path(path)?;
    if expected_kind == ResourceKind::Routine && source.len() > MAX_ROUTINE_SOURCE_BYTES {
        return Err(AgentDefinitionLoadError::InvalidResourceValue {
            path: path.to_owned(),
            message: format!("source exceeds {MAX_ROUTINE_SOURCE_BYTES} bytes"),
        });
    }
    if matches!(
        expected_kind,
        ResourceKind::Provider
            | ResourceKind::McpServer
            | ResourceKind::Defaults
            | ResourceKind::Routine
    ) {
        let header: ResourceHeader = serde_yaml_ng::from_str(source).map_err(|source| {
            AgentDefinitionLoadError::ResourceYaml {
                path: path.to_owned(),
                source,
            }
        })?;
        let expected_kind_name = expected_kind.to_string();
        if header.api_version != waltr_agent::AGENT_DEFINITION_API_VERSION
            || header.kind != expected_kind_name
        {
            return Err(AgentDefinitionLoadError::InvalidResourceValue {
                path: path.to_owned(),
                message: format!(
                    "expected apiVersion `waltr.dev/v1alpha1` and kind `{expected_kind_name}`"
                ),
            });
        }
        let _ = &header.spec;
        let actual_kind = parse_resource_kind(&header.kind)
            .ok_or_else(|| AgentDefinitionLoadError::InvalidResourcePath(path.to_owned()))?;
        if actual_kind != expected_kind || header.metadata.name != expected_name {
            return Err(AgentDefinitionLoadError::ResourceIdentity {
                path: path.to_owned(),
                expected_kind,
                expected_name,
                actual_kind,
                actual_name: header.metadata.name,
            });
        }
        return Ok(ResourceKey {
            kind: actual_kind,
            name: header.metadata.name,
        });
    }

    let keys = inspect_agent_definition_source(source).map_err(|source| {
        AgentDefinitionLoadError::InvalidResource {
            path: path.to_owned(),
            source,
        }
    })?;
    if keys.len() != 1 {
        return Err(AgentDefinitionLoadError::ResourceCount {
            path: path.to_owned(),
            count: keys.len(),
        });
    }
    let key = keys.into_iter().next().expect("one key was validated");
    let actual_kind = agent_resource_kind(key.kind);
    if actual_kind != expected_kind || key.name != expected_name {
        return Err(AgentDefinitionLoadError::ResourceIdentity {
            path: path.to_owned(),
            expected_kind,
            expected_name,
            actual_kind,
            actual_name: key.name,
        });
    }
    Ok(ResourceKey {
        kind: actual_kind,
        name: key.name,
    })
}

#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the resource contract intentionally accepts exact lowercase .yaml only"
)]
pub(super) fn validate_resource_path(
    path: &str,
) -> Result<(ResourceKind, String), AgentDefinitionLoadError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.matches('/').count() != 1
        || !path.ends_with(".yaml")
    {
        return Err(AgentDefinitionLoadError::InvalidResourcePath(
            path.to_owned(),
        ));
    }
    let (category, file) = path
        .split_once('/')
        .expect("validated paths contain one slash");
    let name = file
        .strip_suffix(".yaml")
        .expect("validated paths have a yaml suffix");
    if name.is_empty() || name.contains('/') {
        return Err(AgentDefinitionLoadError::InvalidResourcePath(
            path.to_owned(),
        ));
    }
    let kind = match category {
        "agents" => ResourceKind::Agent,
        "features" => ResourceKind::FeatureSet,
        "permissions" => ResourceKind::PermissionSet,
        "providers" => ResourceKind::Provider,
        "mcp-servers" => ResourceKind::McpServer,
        "defaults" if name == "default" => ResourceKind::Defaults,
        "routines" => ResourceKind::Routine,
        _ => {
            return Err(AgentDefinitionLoadError::InvalidResourcePath(
                path.to_owned(),
            ));
        }
    };
    Ok((kind, name.to_owned()))
}

const fn agent_resource_kind(kind: AgentDefinitionResourceKind) -> ResourceKind {
    match kind {
        AgentDefinitionResourceKind::Agent => ResourceKind::Agent,
        AgentDefinitionResourceKind::FeatureSet => ResourceKind::FeatureSet,
        AgentDefinitionResourceKind::PermissionSet => ResourceKind::PermissionSet,
    }
}

fn parse_resource_kind(kind: &str) -> Option<ResourceKind> {
    match kind {
        "Agent" => Some(ResourceKind::Agent),
        "FeatureSet" => Some(ResourceKind::FeatureSet),
        "PermissionSet" => Some(ResourceKind::PermissionSet),
        "Provider" => Some(ResourceKind::Provider),
        "McpServer" => Some(ResourceKind::McpServer),
        "Defaults" => Some(ResourceKind::Defaults),
        "Routine" => Some(ResourceKind::Routine),
        _ => None,
    }
}

pub(super) fn discover_persisted(
    resources_root: &Path,
) -> Result<BTreeMap<String, String>, AgentDefinitionLoadError> {
    discover_persisted_for(resources_root, false)
}

#[allow(
    clippy::too_many_lines,
    reason = "strict discovery keeps directory shape, file type, and exact path validation together"
)]
pub(super) fn discover_persisted_for(
    resources_root: &Path,
    unified: bool,
) -> Result<BTreeMap<String, String>, AgentDefinitionLoadError> {
    match fs::symlink_metadata(resources_root) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(AgentDefinitionLoadError::UnsupportedResourcePath(
                PathBuf::from(".waltr/resources"),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(source) => {
            return Err(AgentDefinitionLoadError::Inspect {
                path: PathBuf::from(".waltr/resources"),
                source,
            });
        }
    }

    for entry in
        fs::read_dir(resources_root).map_err(|source| AgentDefinitionLoadError::ReadDirectory {
            path: PathBuf::from(".waltr/resources"),
            source,
        })?
    {
        let entry = entry.map_err(|source| AgentDefinitionLoadError::ReadDirectory {
            path: PathBuf::from(".waltr/resources"),
            source,
        })?;
        let name = entry.file_name();
        let allowed = if unified {
            matches!(
                name.to_str(),
                Some(
                    "agents"
                        | "features"
                        | "permissions"
                        | "providers"
                        | "mcp-servers"
                        | "defaults"
                        | "routines"
                )
            )
        } else {
            matches!(name.to_str(), Some("agents" | "features" | "permissions"))
        };
        if !allowed {
            return Err(AgentDefinitionLoadError::UnsupportedResourcePath(
                resource_error_path(resources_root, &entry.path()),
            ));
        }
    }

    let categories: &[&str] = if unified {
        &[
            "agents",
            "features",
            "permissions",
            "providers",
            "mcp-servers",
            "defaults",
            "routines",
        ]
    } else {
        &["agents", "features", "permissions"]
    };
    let mut persisted = BTreeMap::new();
    for category in categories {
        let category_path = resources_root.join(category);
        match fs::symlink_metadata(&category_path) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(AgentDefinitionLoadError::UnsupportedResourcePath(
                    resource_error_path(resources_root, &category_path),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(source) => {
                return Err(AgentDefinitionLoadError::Inspect {
                    path: resource_error_path(resources_root, &category_path),
                    source,
                });
            }
        }
        for entry in fs::read_dir(&category_path).map_err(|source| {
            AgentDefinitionLoadError::ReadDirectory {
                path: resource_error_path(resources_root, &category_path),
                source,
            }
        })? {
            let entry = entry.map_err(|source| AgentDefinitionLoadError::ReadDirectory {
                path: resource_error_path(resources_root, &category_path),
                source,
            })?;
            let path = entry.path();
            let file_type =
                entry
                    .file_type()
                    .map_err(|source| AgentDefinitionLoadError::Inspect {
                        path: resource_error_path(resources_root, &path),
                        source,
                    })?;
            if file_type.is_dir() {
                return Err(AgentDefinitionLoadError::NestedResource(
                    resource_error_path(resources_root, &path),
                ));
            }
            if file_type.is_file() && is_internal_temporary_file(&path) {
                continue;
            }
            if file_type.is_symlink() || !file_type.is_file() || !is_exact_yaml(&path) {
                return Err(AgentDefinitionLoadError::UnsupportedResourcePath(
                    resource_error_path(resources_root, &path),
                ));
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| {
                    AgentDefinitionLoadError::NonUnicodeResourcePath(resource_error_path(
                        resources_root,
                        &path,
                    ))
                })?;
            let relative = format!("{category}/{name}");
            let source =
                fs::read_to_string(&path).map_err(|source| AgentDefinitionLoadError::Read {
                    path: resource_error_path(resources_root, &path),
                    source,
                })?;
            persisted.insert(relative, source);
        }
    }
    Ok(persisted)
}

fn is_exact_yaml(path: &Path) -> bool {
    path.extension().and_then(|extension| extension.to_str()) == Some("yaml")
}

#[allow(
    clippy::case_sensitive_file_extension_comparisons,
    reason = "the atomic filename contains the exact lowercase resource target"
)]
fn is_internal_temporary_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some(body) = name
        .strip_prefix('.')
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    let Some((target, nonce)) = body.rsplit_once('.') else {
        return false;
    };
    target
        .strip_suffix(".yaml")
        .is_some_and(|name| !name.is_empty())
        && uuid::Uuid::parse_str(nonce).is_ok()
}

fn resource_error_path(resources_root: &Path, path: &Path) -> PathBuf {
    let relative = path.strip_prefix(resources_root).unwrap_or(path);
    PathBuf::from(".waltr/resources").join(relative)
}

pub(super) fn load_external_sources(
    paths: &[PathBuf],
) -> Result<Vec<String>, AgentDefinitionLoadError> {
    let mut sources = Vec::new();
    for configured_path in paths {
        for source_path in source_paths(configured_path)? {
            sources.push(fs::read_to_string(&source_path).map_err(|source| {
                AgentDefinitionLoadError::Read {
                    path: source_path,
                    source,
                }
            })?);
        }
    }
    Ok(sources)
}

fn source_paths(path: &Path) -> Result<Vec<PathBuf>, AgentDefinitionLoadError> {
    let metadata = fs::metadata(path).map_err(|source| AgentDefinitionLoadError::Inspect {
        path: path.to_owned(),
        source,
    })?;
    if metadata.is_file() {
        return Ok(vec![path.to_owned()]);
    }
    if !metadata.is_dir() {
        return Err(AgentDefinitionLoadError::UnsupportedPath(path.to_owned()));
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(path).map_err(|source| AgentDefinitionLoadError::ReadDirectory {
        path: path.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| AgentDefinitionLoadError::ReadDirectory {
            path: path.to_owned(),
            source,
        })?;
        let entry_path = entry.path();
        if entry
            .file_type()
            .map_err(|source| AgentDefinitionLoadError::Inspect {
                path: entry_path.clone(),
                source,
            })?
            .is_file()
            && matches!(
                entry_path.extension().and_then(|value| value.to_str()),
                Some("yaml" | "yml")
            )
        {
            files.push(entry_path);
        }
    }
    files.sort();
    Ok(files)
}

pub(super) fn atomic_write(
    root: &Path,
    relative: &str,
    source: &[u8],
) -> Result<(), AgentDefinitionLoadError> {
    let target = resource_disk_path(root, relative);
    let parent = target
        .parent()
        .expect("validated resource path has a category parent");
    fs::create_dir_all(parent).map_err(|source| AgentDefinitionLoadError::Write {
        path: PathBuf::from(relative)
            .parent()
            .unwrap_or_else(|| Path::new(".waltr/resources"))
            .to_owned(),
        source,
    })?;
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .expect("validated resource path is Unicode");
    let temporary = parent.join(format!(".{file_name}.{}.tmp", uuid::Uuid::new_v4()));
    fs::write(&temporary, source).map_err(|source| AgentDefinitionLoadError::Write {
        path: PathBuf::from(relative),
        source,
    })?;
    if let Err(source) = fs::rename(&temporary, &target) {
        drop(fs::remove_file(&temporary));
        return Err(AgentDefinitionLoadError::Write {
            path: PathBuf::from(relative),
            source,
        });
    }
    Ok(())
}

pub(super) fn resource_disk_path(root: &Path, relative: &str) -> PathBuf {
    let (category, file) = relative
        .split_once('/')
        .expect("resource path was validated before disk access");
    root.join(category).join(file)
}

pub(super) fn check_expected_etag(
    path: &str,
    source: Option<&String>,
    expected: &str,
) -> Result<(), AgentDefinitionLoadError> {
    let actual = slot_etag(source.map(String::as_bytes));
    if actual == expected {
        Ok(())
    } else {
        Err(AgentDefinitionLoadError::StaleEtag {
            path: path.to_owned(),
            expected: expected.to_owned(),
            actual,
        })
    }
}

pub(super) fn slot_etag(source: Option<&[u8]>) -> String {
    source.map_or_else(
        || format!("absent:sha256:{}", hex_digest(ABSENT_ETAG_SEED)),
        source_etag,
    )
}

pub(super) fn source_etag(source: &[u8]) -> String {
    format!("sha256:{}", hex_digest(source))
}

pub(super) fn tree_fingerprint(resources: &BTreeMap<String, String>) -> String {
    let mut digest = Sha256::new();
    for (path, source) in resources {
        digest.update(path.len().to_be_bytes());
        digest.update(path.as_bytes());
        digest.update(source.len().to_be_bytes());
        digest.update(source.as_bytes());
    }
    format!("sha256:{}", encode_hex(&digest.finalize()))
}

fn hex_digest(value: &[u8]) -> String {
    encode_hex(&Sha256::digest(value))
}

pub(super) fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
