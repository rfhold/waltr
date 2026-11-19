use std::{
    net::{Ipv4Addr, Ipv6Addr},
    path::{Path, PathBuf},
};

use serde::Deserialize;
use url::Url;

use super::{
    AgentDefinitionLoadError,
    materialize::{ResourceMetadata, invalid_value, parse_resource, validate_envelope},
};

/// A validated outbound workspace binding, without credentials or resolved filesystem state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceBinding {
    endpoint: Url,
    machine_id: String,
    export_root: PathBuf,
}

impl WorkspaceBinding {
    /// The exact normalized destination API root.
    #[must_use]
    pub const fn endpoint(&self) -> &Url {
        &self.endpoint
    }

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

    pub(super) fn effective_value(&self) -> serde_json::Value {
        serde_json::json!({
            "endpoint": self.endpoint.as_str(),
            "machineId": self.machine_id(),
            "exportRoot": self.export_root(),
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceBindingResource {
    api_version: String,
    kind: String,
    #[serde(rename = "metadata")]
    _metadata: ResourceMetadata,
    spec: WorkspaceBindingSpec,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceBindingSpec {
    endpoint: String,
    machine_id: String,
    export_root: String,
}

pub(super) fn materialize_workspace_binding(
    source: &str,
    path: &str,
) -> Result<WorkspaceBinding, AgentDefinitionLoadError> {
    let resource: WorkspaceBindingResource = parse_resource(source, path)?;
    validate_envelope(
        &resource.api_version,
        &resource.kind,
        "WorkspaceBinding",
        path,
    )?;
    let WorkspaceBindingSpec {
        endpoint,
        machine_id,
        export_root,
    } = resource.spec;
    let endpoint = parse_api_root(&endpoint).ok_or_else(|| {
        invalid_value(
            path,
            "endpoint must be an HTTPS API root or a literal loopback HTTP API root, without userinfo, query, or fragment".into(),
        )
    })?;
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
    Ok(WorkspaceBinding {
        endpoint,
        machine_id,
        export_root: export_root.into(),
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

    const PATH: &str = "workspace-bindings/office.yaml";

    fn source(endpoint: &str, machine: &str, root: &str) -> String {
        format!(
            "apiVersion: waltr.dev/v1alpha1\nkind: WorkspaceBinding\nmetadata:\n  name: office\nspec:\n  endpoint: '{endpoint}'\n  machineId: '{machine}'\n  exportRoot: '{root}'\n"
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
                let binding = materialize_workspace_binding(
                    &source(raw, "Machine_01-test.example", root),
                    PATH,
                )
                .unwrap();
                assert_eq!(binding.endpoint().as_str(), normalized);
                assert_eq!(binding.machine_id(), "Machine_01-test.example");
                assert_eq!(binding.export_root().as_os_str(), root);
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
                materialize_workspace_binding(&source(endpoint, "machine", "~/workspace"), PATH)
                    .is_err(),
                "{endpoint}"
            );
        }
    }

    #[test]
    fn rejects_invalid_machine_path_and_unrecognized_fields() {
        let endpoint = "https://host.example/";
        for machine in ["", "local", "a b", "a/b", "résumé", &"a".repeat(129)] {
            assert!(
                materialize_workspace_binding(&source(endpoint, machine, "~/workspace"), PATH)
                    .is_err(),
                "{machine}"
            );
        }
        for root in ["", "~", "~user/workspace", "workspace", "./workspace"] {
            assert!(
                materialize_workspace_binding(&source(endpoint, "machine", root), PATH).is_err(),
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
            valid.replace("kind: WorkspaceBinding", "kind: WorkspaceExport"),
            valid.replace("'~/workspace'", r#""/\0""#),
        ] {
            assert!(materialize_workspace_binding(&invalid, PATH).is_err());
        }
    }
}
