use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use url::Url;

use super::{
    AgentDefinitionLoadError,
    materialize::{ResourceMetadata, invalid_value, parse_resource, validate_envelope},
};

/// A named outbound Waltr connection, optionally exporting one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    endpoint: Url,
    binding: Option<RemoteBinding>,
}

/// Worker-side workspace export, without resolved filesystem state.
#[derive(Debug, Clone, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBinding {
    machine_id: String,
    export_root: PathBuf,
    mcp_stdio_servers: Vec<String>,
}

impl PartialEq for RemoteBinding {
    fn eq(&self, other: &Self) -> bool {
        self.machine_id == other.machine_id
            && self.export_root.as_os_str() == other.export_root.as_os_str()
            && self.mcp_stdio_servers == other.mcp_stdio_servers
    }
}

impl Remote {
    /// The exact normalized destination API root.
    #[must_use]
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }

    /// Optional workspace that this server exports to the destination.
    pub const fn binding(&self) -> Option<&RemoteBinding> {
        self.binding.as_ref()
    }

    pub(super) fn effective_value(&self) -> serde_json::Value {
        let mut value = serde_json::json!({ "endpoint": self.endpoint.as_str() });
        if let Some(binding) = &self.binding {
            value["binding"] = serde_json::json!(binding);
        }
        value
    }
}

impl RemoteBinding {
    /// The machine identity to claim at this destination.
    #[must_use]
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// The worker-local export path, without home expansion or canonicalization.
    #[must_use]
    pub fn export_root(&self) -> &Path {
        &self.export_root
    }

    /// Exact exporter-local MCP resource keys permitted on this binding.
    pub fn mcp_stdio_servers(&self) -> &[String] {
        &self.mcp_stdio_servers
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RemoteResource {
    api_version: String,
    kind: String,
    metadata: ResourceMetadata,
    spec: RemoteSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RemoteSpec {
    endpoint: String,
    binding: Option<RemoteBindingSpec>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RemoteBindingSpec {
    machine_id: String,
    export_root: String,
    #[serde(default)]
    mcp_stdio_servers: Vec<String>,
}

pub(super) fn materialize_remote(
    source: &str,
    path: &str,
) -> Result<Remote, AgentDefinitionLoadError> {
    let resource: RemoteResource = parse_resource(source, path)?;
    validate_envelope(&resource.api_version, &resource.kind, "Remote", path)?;
    if !waltr::destination::is_remote_name(&resource.metadata.name) {
        return Err(invalid_value(
            path,
            "invalid or reserved Remote name".into(),
        ));
    }
    let endpoint = parse_api_root(&resource.spec.endpoint).ok_or_else(|| {
        invalid_value(
            path,
            "endpoint must be an HTTPS API root or a literal loopback HTTP API root, without userinfo, query, or fragment".into(),
        )
    })?;
    let binding = resource
        .spec
        .binding
        .map(|binding| materialize_binding(binding, path))
        .transpose()?;
    Ok(Remote { endpoint, binding })
}

fn materialize_binding(
    RemoteBindingSpec {
        machine_id,
        export_root,
        mut mcp_stdio_servers,
    }: RemoteBindingSpec,
    path: &str,
) -> Result<RemoteBinding, AgentDefinitionLoadError> {
    if machine_id.is_empty()
        || machine_id == "local"
        || machine_id.len() > 128
        || !machine_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(invalid_value(
            path,
            "machineId must be 1-128 ASCII letters, digits, hyphens, underscores, or periods and cannot be `local`".into(),
        ));
    }
    if export_root.contains('\0')
        || !(Path::new(&export_root).is_absolute() || export_root.starts_with("~/"))
    {
        return Err(invalid_value(
            path,
            "exportRoot must be absolute or start with `~/`".into(),
        ));
    }
    if mcp_stdio_servers.len() > 32
        || mcp_stdio_servers.iter().any(|name| {
            name.is_empty()
                || matches!(name.as_str(), "." | "..")
                || name.len() > 128
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        })
        || mcp_stdio_servers.iter().collect::<BTreeSet<_>>().len() != mcp_stdio_servers.len()
    {
        return Err(invalid_value(
            path,
            "mcpStdioServers must contain at most 32 distinct MCP resource keys".into(),
        ));
    }
    mcp_stdio_servers.sort_unstable();
    Ok(RemoteBinding {
        machine_id,
        export_root: export_root.into(),
        mcp_stdio_servers,
    })
}

fn parse_api_root(raw: &str) -> Option<Url> {
    // Do not let URL normalization hide a non-root path or empty userinfo.
    let (_, authority) = raw.split_once("://")?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.is_empty()
        || authority.contains(['/', '\\', '@', '?', '#'])
        || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return None;
    }
    let endpoint = Url::parse(raw).ok()?;
    match (endpoint.scheme(), endpoint.host()?) {
        ("https", _) => Some(endpoint),
        ("http", url::Host::Ipv4(_)) => {
            let host = authority.split(':').next()?;
            host.parse::<Ipv4Addr>()
                .ok()?
                .is_loopback()
                .then_some(endpoint)
        }
        ("http", url::Host::Ipv6(_)) => {
            let (host, _) = authority.strip_prefix('[')?.split_once(']')?;
            host.parse::<Ipv6Addr>()
                .ok()?
                .is_loopback()
                .then_some(endpoint)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATH: &str = "remotes/office.yaml";

    fn source(endpoint: &str, machine: &str, root: &str) -> String {
        format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: Remote\nmetadata:\n  name: office\nspec:\n  endpoint: '{endpoint}'\n  binding:\n    machineId: '{machine}'\n    exportRoot: '{root}'\n"
        )
    }

    #[test]
    fn accepts_normalized_root_and_preserves_worker_paths() {
        for (raw, normalized) in [
            ("HTTPS://HOST.example:443", "https://host.example/"),
            ("https://host.example:8443/", "https://host.example:8443/"),
            ("http://127.1.2.3:5000", "http://127.1.2.3:5000/"),
            ("http://[0:0:0:0:0:0:0:1]", "http://[::1]/"),
        ] {
            for root in ["/no-such-worker-dir/../workspace", "~/workspace"] {
                let binding =
                    materialize_remote(&source(raw, "Machine_01-test.example", root), PATH)
                        .unwrap();
                assert_eq!(binding.endpoint().as_str(), normalized);
                let export = binding.binding().unwrap();
                assert_eq!(export.machine_id(), "Machine_01-test.example");
                assert_eq!(export.export_root().as_os_str(), root);
            }
        }
    }

    #[test]
    fn rejects_non_root_or_insecure_endpoints() {
        for endpoint in [
            "https://host.example/api/",
            "https://host.example/x/../",
            "https://host.example//",
            "https://host.example/?q",
            "https://host.example/#",
            "https://@host.example/",
            "https://user:pass@host.example/",
            "https://host.example\\",
            "https:host.example",
            "http://host.example/",
            "http://localhost/",
            "http://127.1/",
            "http://0x7f000001/",
            "http://[::ffff:127.0.0.1]/",
        ] {
            assert!(
                materialize_remote(&source(endpoint, "machine", "~/workspace"), PATH).is_err(),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn rejects_invalid_machine_path_and_unrecognized_fields() {
        let endpoint = "https://host.example/";
        for machine in ["", "local", "a b", "a/b", "résumé", &"a".repeat(129)] {
            assert!(
                materialize_remote(&source(endpoint, machine, "~/workspace"), PATH).is_err(),
                "{machine}"
            );
        }
        for root in ["", "~", "~user/workspace", "workspace", "./workspace"] {
            assert!(
                materialize_remote(&source(endpoint, "machine", root), PATH).is_err(),
                "{root}"
            );
        }

        let valid = source(endpoint, "machine", "~/workspace");
        for invalid in [
            format!("{valid}  credential: invalid\n"),
            format!("{valid}credential: invalid\n"),
            valid.replace("  exportRoot", "  export_root"),
            valid.replace("  name: office", "  name: office\n  credential: invalid"),
            valid.replace("waltr.dev/v1alpha1", "waltr.dev/v1alpha2"),
            valid.replace("kind: Remote", "kind: WorkspaceExport"),
            valid.replace("'~/workspace'", r#""/\0""#),
        ] {
            assert!(materialize_remote(&invalid, PATH).is_err());
        }
    }

    #[test]
    fn stdio_allowlist_is_optional_exact_and_unique() {
        let yaml = source("https://host.example/", "machine", "~/workspace");
        let parse = |names: &str| {
            materialize_remote(&format!("{yaml}    mcpStdioServers: {names}\n"), PATH)
        };
        assert!(
            materialize_remote(&yaml, PATH)
                .unwrap()
                .binding()
                .unwrap()
                .mcp_stdio_servers()
                .is_empty()
        );
        let allowlist = parse("[echo_server, Z-1.dev]").unwrap();
        assert_eq!(
            allowlist.binding().unwrap().mcp_stdio_servers(),
            ["Z-1.dev", "echo_server"]
        );
        for names in [
            "[echo, echo]",
            "['']",
            "['../echo']",
            "['.']",
            "[' foo']",
            "[foo/bar]",
            "['foo\\bar']",
            "null",
        ] {
            assert!(parse(names).is_err(), "{names}");
        }
    }
}
