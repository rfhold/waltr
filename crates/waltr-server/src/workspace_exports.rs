//! Reconcile server-owned workspace bindings on the current private worker stream.

use std::{
    collections::BTreeMap,
    future::Future,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::{StreamExt as _, stream::FuturesUnordered};
use sha2::{Digest as _, Sha256};
use tokio::{task::JoinHandle, time::Instant};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;
use waltr_agent_sandbox::peer_config::WorkspaceExport;

use crate::{
    agent_definition::{AgentDefinitionSnapshot, AgentDefinitionState},
    agent_host::sandbox_gateway::{WorkspaceExportConnection, WorkspaceExportControl},
    auth::{AuthService, WorkspaceBindingCredentialLease},
    resource_catalog::WorkspaceBinding,
};

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const LEASE_TIMEOUT: Duration = Duration::from_secs(50);
const MAX_LEASE_REQUESTS: usize = 8;
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) struct WorkspaceExports {
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl WorkspaceExports {
    pub(crate) fn spawn(
        catalog: AgentDefinitionState,
        auth: Arc<AuthService>,
        control: WorkspaceExportControl,
    ) -> Self {
        let stop = CancellationToken::new();
        let task = tokio::spawn(supervise(
            catalog,
            control,
            stop.clone(),
            move |key, url| {
                let auth = auth.clone();
                async move {
                    auth.workspace_binding_credential_lease(&key, &url)
                        .await
                        .unwrap_or(None)
                }
            },
        ));
        Self {
            stop,
            task: Some(task),
        }
    }

    pub(crate) async fn shutdown(mut self) {
        self.stop.cancel();
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut task)
                .await
                .is_err()
        {
            task.abort();
            drop(task.await);
        }
    }
}

impl Drop for WorkspaceExports {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

struct BindingPoll {
    binding: Arc<ObservedBinding>,
    due: Option<Instant>,
}

struct Installed {
    binding: Arc<ObservedBinding>,
    lease: LeaseVersion,
    remove: bool,
}

#[derive(PartialEq, Eq)]
struct LeaseVersion {
    authority: String,
    expires: u64,
    digest: [u8; 32],
}

impl LeaseVersion {
    fn current(lease: &WorkspaceBindingCredentialLease) -> Option<Self> {
        // Round down to avoid granting even a fractional second beyond the OAuth lease.
        let expires = u64::try_from(lease.expires_at_ms).ok()? / 1000;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
        (expires > now && !lease.access_token.is_empty()).then(|| Self {
            authority: lease.authority_id.clone(),
            expires,
            digest: Sha256::digest(lease.access_token.as_bytes()).into(),
        })
    }
}

struct ObservedBinding {
    spec: WorkspaceBinding,
    incarnation: Option<Uuid>,
}

impl ObservedBinding {
    fn is_current(&self, snapshot: &AgentDefinitionSnapshot, key: &str) -> bool {
        self.incarnation == snapshot.workspace_binding_incarnation(key)
            && snapshot
                .workspace_bindings()
                .get(key)
                .is_some_and(|binding| {
                    self.spec == *binding
                        && self.spec.export_root().as_os_str() == binding.export_root().as_os_str()
                })
    }
}

async fn supervise<F, Lease>(
    catalog: AgentDefinitionState,
    control: WorkspaceExportControl,
    stop: CancellationToken,
    lease: F,
) where
    F: Fn(String, Url) -> Lease + Sync,
    Lease: Future<Output = Option<WorkspaceBindingCredentialLease>>,
{
    let mut readiness = control.readiness();
    while !stop.is_cancelled() {
        readiness.borrow_and_update();
        if let Some(connection) = control.connection() {
            reconcile(&catalog, &connection, &stop, &lease).await;
        } else {
            tokio::select! {
                () = stop.cancelled() => break,
                result = readiness.changed() => if result.is_err() { break },
            }
        }
    }
}

async fn reconcile<F, Lease>(
    catalog: &AgentDefinitionState,
    connection: &WorkspaceExportConnection,
    stop: &CancellationToken,
    lease: &F,
) where
    F: Fn(String, Url) -> Lease,
    Lease: Future<Output = Option<WorkspaceBindingCredentialLease>>,
{
    let mut publications = catalog.subscribe();
    let mut desired = BTreeMap::<String, BindingPoll>::new();
    let mut installed = BTreeMap::<String, Installed>::new();
    let mut polls = FuturesUnordered::new();
    while !stop.is_cancelled() {
        let snapshot = catalog.snapshot();
        desired.retain(|key, poll| poll.binding.is_current(&snapshot, key));
        for (key, binding) in snapshot.workspace_bindings() {
            desired.entry(key.clone()).or_insert_with(|| BindingPoll {
                binding: Arc::new(ObservedBinding {
                    spec: binding.clone(),
                    incarnation: snapshot.workspace_binding_incarnation(key),
                }),
                due: Some(Instant::now()),
            });
        }
        for (key, entry) in &mut installed {
            entry.remove |= desired
                .get(key)
                .is_none_or(|poll| !Arc::ptr_eq(&poll.binding, &entry.binding));
        }
        remove_marked(connection, &mut installed).await;

        while polls.len() < MAX_LEASE_REQUESTS {
            let now = Instant::now();
            let Some((key, poll)) = desired
                .iter_mut()
                .filter(|(_, poll)| poll.due.is_some_and(|due| due <= now))
                .min_by_key(|(_, poll)| poll.due)
            else {
                break;
            };
            poll.due = None;
            let key = key.clone();
            let binding = poll.binding.clone();
            let request = lease(key.clone(), binding.spec.endpoint().clone());
            polls.push(async move {
                let result = tokio::time::timeout(LEASE_TIMEOUT, request)
                    .await
                    .ok()
                    .flatten();
                (key, binding, result)
            });
        }

        let next_poll = (polls.len() < MAX_LEASE_REQUESTS)
            .then(|| desired.values().filter_map(|poll| poll.due).min())
            .flatten();
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            () = connection.retired() => return,
            update = publications.recv() => {
                if matches!(update, Err(tokio::sync::broadcast::error::RecvError::Closed)) {
                    break;
                }
            }
            Some((key, binding, result)) = polls.next() => {
                let Some(poll) = desired.get_mut(&key)
                    .filter(|poll| Arc::ptr_eq(&poll.binding, &binding)) else { continue };
                poll.due = Some(Instant::now() + POLL_INTERVAL);
                observe_lease(catalog, connection, &mut installed, key, binding, result).await;
            }
            () = async {
                if let Some(next_poll) = next_poll {
                    tokio::time::sleep_until(next_poll).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {}
            () = tokio::time::sleep(POLL_INTERVAL), if installed.values().any(|entry| entry.remove) => {}
        }
    }
    for entry in installed.values_mut() {
        entry.remove = true;
    }
    remove_marked(connection, &mut installed).await;
}

async fn remove_marked(
    connection: &WorkspaceExportConnection,
    installed: &mut BTreeMap<String, Installed>,
) {
    let keys: Vec<_> = installed
        .iter()
        .filter(|(_, entry)| entry.remove)
        .map(|(key, _)| key.clone())
        .collect();
    for key in keys {
        if connection.remove(&key).await.is_ok() {
            installed.remove(&key);
        }
    }
}

async fn observe_lease(
    catalog: &AgentDefinitionState,
    connection: &WorkspaceExportConnection,
    installed: &mut BTreeMap<String, Installed>,
    key: String,
    binding: Arc<ObservedBinding>,
    lease: Option<WorkspaceBindingCredentialLease>,
) {
    let Some((lease, version)) =
        lease.and_then(|lease| LeaseVersion::current(&lease).map(|version| (lease, version)))
    else {
        if let Some(entry) = installed.get_mut(&key) {
            entry.remove = true;
        }
        return;
    };
    if let Some(entry) = installed.get_mut(&key) {
        if !Arc::ptr_eq(&entry.binding, &binding) || entry.lease.authority != version.authority {
            entry.remove = true;
        }
        if entry.remove {
            // Do not install new authority until removal is acknowledged and a new lease is read.
            return;
        }
        if entry.lease == version {
            return;
        }
    }
    if !binding.is_current(&catalog.snapshot(), &key) {
        return;
    }
    let result = if installed.contains_key(&key) {
        connection
            .rotate(&key, lease.access_token, version.expires)
            .await
    } else {
        let export = WorkspaceExport {
            endpoint: binding.spec.endpoint().to_string(),
            machine_id: binding.spec.machine_id().to_owned(),
            export_root: binding.spec.export_root().to_owned(),
        };
        connection
            .apply(&key, export, lease.access_token, version.expires)
            .await
    };
    if result.is_ok() {
        installed.insert(
            key,
            Installed {
                binding,
                lease: version,
                remove: false,
            },
        );
    }
}

#[cfg(test)]
mod tests;
