//! Reconcile server-owned remotes on the current private worker stream.

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
use waltr::grpc::waltr::{config::v1 as config_proto, sandbox::v1::McpStdioLaunch};
use waltr_agent_sandbox::peer_config::WorkspaceExport;

use crate::{
    agent_definition::{AgentDefinitionSnapshot, AgentDefinitionState},
    agent_host::sandbox_gateway::{
        WorkspaceExportConnection, WorkspaceExportControl, stdio_launch,
    },
    auth::{AuthService, RemoteCredentialLease},
    resource_catalog::{PublishedState, Remote, RemoteBinding},
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
                    auth.remote_credential_lease(&key, &url)
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
    fn current(lease: &RemoteCredentialLease) -> Option<Self> {
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
    remote: Remote,
    binding: RemoteBinding,
    incarnation: Option<Uuid>,
    definitions: Vec<config_proto::McpServer>,
    launches: Vec<McpStdioLaunch>,
}

impl ObservedBinding {
    fn new(snapshot: &AgentDefinitionSnapshot, key: &str, spec: &Remote) -> Option<Self> {
        let binding = spec.binding()?;
        let definitions = binding
            .mcp_stdio_servers()
            .iter()
            .map(|name| {
                snapshot
                    .configuration()
                    .mcp_servers
                    .get(name)
                    .cloned()
                    .filter(|server| server.r#type == config_proto::McpServerType::Stdio as i32)
            })
            .collect::<Option<Vec<_>>>()?;
        let launches = binding
            .mcp_stdio_servers()
            .iter()
            .zip(&definitions)
            .map(|(name, server)| stdio_launch(name, &server.command))
            .collect::<Option<Vec<_>>>()?;
        Some(Self {
            remote: spec.clone(),
            binding: binding.clone(),
            incarnation: snapshot.remote_incarnation(key),
            definitions,
            launches,
        })
    }

    fn definitions_current(&self, configuration: &config_proto::Configuration) -> bool {
        self.binding
            .mcp_stdio_servers()
            .iter()
            .zip(&self.definitions)
            .all(|(name, expected)| configuration.mcp_servers.get(name) == Some(expected))
    }

    fn is_current(&self, snapshot: &AgentDefinitionSnapshot, key: &str) -> bool {
        self.incarnation == snapshot.remote_incarnation(key)
            && self.definitions_current(snapshot.configuration())
            && snapshot
                .remotes()
                .get(key)
                .is_some_and(|remote| self.remote == *remote)
    }
}

fn observe_publication(
    desired: &mut BTreeMap<String, BindingPoll>,
    publication: Option<PublishedState>,
) {
    if let Some(publication) = publication {
        if let Some(configuration) = publication.resource_catalog().effective_configuration {
            desired.retain(|_, poll| poll.binding.definitions_current(&configuration));
        }
    } else {
        // A missed publication may have revoked and then restored a referenced definition.
        desired.retain(|_, poll| poll.binding.definitions.is_empty());
    }
}

async fn supervise<F, Lease>(
    catalog: AgentDefinitionState,
    control: WorkspaceExportControl,
    stop: CancellationToken,
    lease: F,
) where
    F: Fn(String, Url) -> Lease + Sync,
    Lease: Future<Output = Option<RemoteCredentialLease>>,
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
    Lease: Future<Output = Option<RemoteCredentialLease>>,
{
    let mut publications = catalog.subscribe();
    let mut desired = BTreeMap::<String, BindingPoll>::new();
    let mut installed = BTreeMap::<String, Installed>::new();
    let mut polls = FuturesUnordered::new();
    while !stop.is_cancelled() {
        // Observe intermediate accepted definitions too, including an edit reverted before an
        // outstanding control acknowledgement returns.
        loop {
            match publications.try_recv() {
                Ok(update) => observe_publication(&mut desired, Some(update)),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {
                    observe_publication(&mut desired, None)
                }
                Err(_) => break,
            }
        }
        let snapshot = catalog.snapshot();
        desired.retain(|key, poll| poll.binding.is_current(&snapshot, key));
        for (key, binding) in snapshot.remotes() {
            if let std::collections::btree_map::Entry::Vacant(entry) = desired.entry(key.clone())
                && let Some(binding) = ObservedBinding::new(&snapshot, key, binding)
            {
                entry.insert(BindingPoll {
                    binding: Arc::new(binding),
                    due: Some(Instant::now()),
                });
            }
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
            let request = lease(key.clone(), binding.remote.endpoint().clone());
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
                match update {
                    Ok(update) => observe_publication(&mut desired, Some(update)),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => observe_publication(&mut desired, None),
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
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
    lease: Option<RemoteCredentialLease>,
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
            endpoint: binding.remote.endpoint().to_string(),
            machine_id: binding.binding.machine_id().to_owned(),
            export_root: binding.binding.export_root().to_owned(),
        };
        connection
            .apply(
                &key,
                export,
                binding.launches.clone(),
                lease.access_token,
                version.expires,
            )
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
