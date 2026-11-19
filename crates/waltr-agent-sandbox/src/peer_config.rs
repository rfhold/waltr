//! Outbound workspace destinations and private stream-owned exports.
use std::{
    collections::{HashMap, HashSet},
    fmt, io,
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;
use tonic::{
    metadata::AsciiMetadataValue,
    transport::{ClientTlsConfig, Endpoint},
};
use waltr::grpc::waltr::sandbox::v1::{
    WorkspaceExportAccess, WorkspaceExportApply, WorkspaceExportControl,
    workspace_export_control::Change,
};

use crate::wire::{DialAuthorization, run_worker_with_dial_authorization};

/// One configured destination on an exporting installation.
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceExport {
    /// Hosted HTTP/2 origin. Plain HTTP is restricted to loopback IP addresses.
    pub endpoint: String,
    /// Stable identity registered at that origin.
    pub machine_id: String,
    /// Exporter's absolute or home-relative workspace directory.
    pub export_root: PathBuf,
}

impl fmt::Debug for WorkspaceExport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WorkspaceExport").finish_non_exhaustive()
    }
}

fn invalid() -> io::Error {
    io::Error::other("invalid outbound workspace export configuration")
}

/// One validated export, eligible for use only at its exact API root.
#[derive(Clone)]
pub struct CheckedExport {
    origin: url::Url,
    machine: String,
    root: PathBuf,
    endpoint: Endpoint,
}

impl fmt::Debug for CheckedExport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckedExport").finish_non_exhaustive()
    }
}

/// Resolved export destinations. This configuration contains no credentials.
#[derive(Clone, Default)]
pub struct CheckedExports(Vec<CheckedExport>);

impl fmt::Debug for CheckedExports {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CheckedExports")
            .field("count", &self.0.len())
            .finish()
    }
}

impl CheckedExports {
    /// Resolve and validate every configured export without a network request.
    ///
    /// # Errors
    /// Returns an error for an invalid destination, machine ID, or local directory.
    pub fn load(config: &[WorkspaceExport]) -> io::Result<Self> {
        let mut destinations = Vec::new();
        let mut origins = HashSet::new();
        for peer in config {
            let url = url::Url::parse(&peer.endpoint).map_err(|_| invalid())?;
            if !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
                || url.path() != "/"
                || url.host().is_none()
                || !origins.insert(url.origin().ascii_serialization())
            {
                return Err(invalid());
            }
            let loopback = match url.host() {
                Some(url::Host::Ipv4(v)) => v.is_loopback(),
                Some(url::Host::Ipv6(v)) => v.is_loopback(),
                _ => false,
            };
            let mut endpoint = Endpoint::from_shared(url.to_string()).map_err(|_| invalid())?;
            match url.scheme() {
                "https" => {
                    endpoint = endpoint
                        .tls_config(ClientTlsConfig::new().with_webpki_roots())
                        .map_err(|_| invalid())?;
                }
                "http" if loopback => {}
                _ => return Err(invalid()),
            }
            if peer.machine_id.is_empty()
                || peer.machine_id == "local"
                || peer.machine_id.len() > 128
                || !peer
                    .machine_id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            {
                return Err(invalid());
            }
            let root = if let Ok(relative) = peer.export_root.strip_prefix("~") {
                std::env::home_dir().ok_or_else(invalid)?.join(relative)
            } else {
                peer.export_root.clone()
            };
            if !root.is_absolute() {
                return Err(invalid());
            }
            let root = root.canonicalize().map_err(|_| invalid())?;
            if !root.is_dir() {
                return Err(invalid());
            }
            destinations.push(CheckedExport {
                origin: url,
                machine: peer.machine_id.clone(),
                root,
                endpoint,
            });
        }
        Ok(Self(destinations))
    }

    /// Find the export for the exact normalized native API root.
    #[must_use]
    pub fn matching(&self, endpoint: &url::Url) -> Option<CheckedExport> {
        self.0.iter().find(|dest| &dest.origin == endpoint).cloned()
    }
}

impl CheckedExport {
    /// Machine identity claimed by this export.
    #[must_use]
    pub fn machine_id(&self) -> &str {
        &self.machine
    }

    /// Start this export under the caller's credential lifecycle.
    ///
    /// The callback supplies the current complete Authorization value on every dial.
    /// When it returns `None`, the worker waits and does not dial.
    pub fn start(
        self,
        authorization: impl Fn() -> Option<AsciiMetadataValue> + Send + Sync + 'static,
    ) -> ExportWorkers {
        self.start_with_deadline(move || {
            authorization().map(|header| DialAuthorization {
                header,
                expires: None,
            })
        })
    }

    fn start_with_deadline(
        self,
        authorization: impl Fn() -> Option<DialAuthorization> + Send + Sync + 'static,
    ) -> ExportWorkers {
        let stop = CancellationToken::new();
        let task = tokio::spawn(run_worker_with_dial_authorization(
            self.endpoint.connect_lazy(),
            self.root,
            self.machine,
            stop.clone(),
            Arc::new(AtomicBool::new(false)),
            authorization,
        ));
        ExportWorkers { stop, task }
    }
}

/// One owned outbound worker task.
#[must_use]
pub struct ExportWorkers {
    stop: CancellationToken,
    task: JoinHandle<Result<(), crate::SandboxError>>,
}
impl fmt::Debug for ExportWorkers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExportWorkers").finish_non_exhaustive()
    }
}
impl ExportWorkers {
    /// Cancel the stream and join the worker, even during a handshake.
    pub async fn shutdown(mut self) {
        self.stop.cancel();
        drop((&mut self.task).await);
    }
}
impl Drop for ExportWorkers {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Default)]
pub(crate) struct StreamExports(HashMap<String, ActiveExport>);

struct ActiveExport {
    config: CheckedExport,
    access: Arc<Mutex<Option<DialAuthorization>>>,
    worker: ExportWorkers,
}

impl ActiveExport {
    fn cancel(&self) {
        self.access.lock().unwrap().take();
        self.worker.stop.cancel();
    }

    async fn shutdown(self) {
        self.cancel();
        self.worker.shutdown().await;
    }
}

fn checked_access(access: WorkspaceExportAccess) -> io::Result<DialAuthorization> {
    let expiry = UNIX_EPOCH
        .checked_add(Duration::from_secs(access.expires_at_unix))
        .ok_or_else(invalid)?;
    let lifetime = expiry
        .duration_since(SystemTime::now())
        .map_err(|_| invalid())?;
    let expires = Instant::now().checked_add(lifetime).ok_or_else(invalid)?;
    if access.access_token.is_empty()
        || !access
            .access_token
            .bytes()
            .all(|byte| byte.is_ascii_graphic())
    {
        return Err(invalid());
    }
    let mut header = format!("Bearer {}", access.access_token)
        .parse::<AsciiMetadataValue>()
        .map_err(|_| invalid())?;
    header.set_sensitive(true);
    Ok(DialAuthorization {
        header,
        expires: Some(expires),
    })
}

impl StreamExports {
    pub(crate) async fn change(&mut self, control: WorkspaceExportControl) -> bool {
        if control.request_id.is_empty() || control.binding.is_empty() {
            return false;
        }
        let result = match control.change {
            Some(Change::Apply(apply)) => self.apply(control.binding, apply).await,
            Some(Change::Rotate(access)) => checked_access(access).and_then(|access| {
                let active = self.0.get(&control.binding).ok_or_else(invalid)?;
                *active.access.lock().unwrap() = Some(access);
                Ok(())
            }),
            Some(Change::Remove(_)) => {
                if let Some(active) = self.0.remove(&control.binding) {
                    active.shutdown().await;
                }
                Ok(())
            }
            None => Err(invalid()),
        };
        result.is_ok()
    }

    async fn apply(&mut self, binding: String, apply: WorkspaceExportApply) -> io::Result<()> {
        let access = checked_access(apply.access.ok_or_else(invalid)?)?;
        let config = WorkspaceExport {
            endpoint: apply.endpoint,
            machine_id: apply.machine_id,
            export_root: apply.export_root.into(),
        };
        let CheckedExports(mut checked) = CheckedExports::load(&[config])?;
        let config = checked.pop().ok_or_else(invalid)?;
        if self.0.iter().any(|(id, active)| {
            id != &binding
                && active.config.origin == config.origin
                && active.config.machine == config.machine
        }) {
            return Err(invalid());
        }
        if let Some(active) = self.0.get_mut(&binding)
            && active.config.origin == config.origin
            && active.config.machine == config.machine
            && active.config.root == config.root
        {
            *active.access.lock().unwrap() = Some(access);
            return Ok(());
        }
        if let Some(active) = self.0.remove(&binding) {
            active.shutdown().await;
        }
        let access = Arc::new(Mutex::new(Some(access)));
        let current = access.clone();
        let worker = config.clone().start_with_deadline(move || {
            let mut current = current.lock().unwrap();
            if current
                .as_ref()
                .and_then(|value| value.expires)
                .is_some_and(|expires| expires <= Instant::now())
            {
                current.take();
            }
            current.clone()
        });
        self.0.insert(
            binding,
            ActiveExport {
                config,
                access,
                worker,
            },
        );
        Ok(())
    }

    pub(crate) async fn shutdown(&mut self) {
        for active in self.0.values() {
            active.cancel();
        }
        for (_, active) in self.0.drain() {
            active.worker.shutdown().await;
        }
    }
}

impl Drop for StreamExports {
    fn drop(&mut self) {
        for active in self.0.values() {
            active.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_the_exact_normalized_api_root() {
        let dir = tempfile::tempdir().unwrap();
        let export = WorkspaceExport {
            endpoint: "https://HOST.example:443".into(),
            machine_id: "laptop".into(),
            export_root: dir.path().into(),
        };
        let checked = CheckedExports::load(std::slice::from_ref(&export)).unwrap();
        let url = |value| url::Url::parse(value).unwrap();
        assert_eq!(
            checked
                .matching(&url("https://host.example/"))
                .unwrap()
                .machine_id(),
            "laptop"
        );
        for other in [
            "http://host.example/",
            "https://host.example:8443/",
            "https://other.example/",
            "https://host.example/api/",
            "https://host.example/?q=1",
            "https://host.example/#fragment",
            "https://user@host.example/",
        ] {
            assert!(checked.matching(&url(other)).is_none(), "{other}");
        }
        let duplicate = WorkspaceExport {
            endpoint: "https://host.example/".into(),
            ..export.clone()
        };
        assert!(CheckedExports::load(&[export, duplicate]).is_err());
    }

    #[test]
    fn rejects_invalid_destination_machine_and_root() {
        let dir = tempfile::tempdir().unwrap();
        let valid = WorkspaceExport {
            endpoint: "http://127.0.0.1:1234".into(),
            machine_id: "laptop".into(),
            export_root: dir.path().into(),
        };
        assert!(CheckedExports::load(std::slice::from_ref(&valid)).is_ok());
        for endpoint in [
            "https://host.example/api/",
            "https://user:pass@host.example/",
            "http://host.example/",
            "https://host.example/?q=1",
            "https://host.example/#fragment",
        ] {
            let export = WorkspaceExport {
                endpoint: endpoint.into(),
                ..valid.clone()
            };
            assert!(CheckedExports::load(&[export]).is_err(), "{endpoint}");
        }
        for machine in ["", "local", "two words", "dir/path"] {
            let export = WorkspaceExport {
                machine_id: machine.into(),
                ..valid.clone()
            };
            assert!(CheckedExports::load(&[export]).is_err(), "{machine}");
        }
        let file = dir.path().join("file");
        std::fs::write(&file, b"not a directory").unwrap();
        for root in [PathBuf::from("relative"), dir.path().join("missing"), file] {
            let export = WorkspaceExport {
                export_root: root,
                ..valid.clone()
            };
            assert!(CheckedExports::load(&[export]).is_err());
        }
        assert!(
            serde_json::from_value::<WorkspaceExport>(serde_json::json!({
                "endpoint": "https://host.example/",
                "machine_id": "laptop",
                "export_root": dir.path(),
                "credential_file": "/obsolete"
            }))
            .is_err()
        );
    }
}
