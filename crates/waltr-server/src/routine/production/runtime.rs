use super::RoutineAuthority;
use crate::agent_host::runtime_factory::ServerAgentRuntimeFactory as AgentSessionRuntimeFactory;
use crate::{
    persistence::Persistence,
    resource_catalog::AgentDefinitionState,
    routine::{
        blob::{
            self,
            filesystem::{FilesystemBlobStore, FilesystemConfig},
        },
        definition::{Definition, Trigger},
        history::{Caller, InvocationBinding},
        ports::{
            Clock, HostFactory, InvocationStore, PortError, PortFuture, PreparedHost,
            SelectedEntrypoint, SystemClock,
        },
        rpc::{
            api::ReadAdapter,
            attachments::AttachmentRegistry,
            blobs::BlobRegistry,
            catalog::{CatalogEntry, MemoryCatalog, StartContext},
            service::RoutineRpcService,
        },
        service::InvocationService,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;
use waltr_agent::{SessionCoordinator, SessionStore};

mod bindings;
mod lease;
#[cfg(test)]
mod tests;

/// Explicit application composition inputs; no database or workspace defaults are resolved here.
#[derive(Debug)]
pub struct ProductionInput {
    /// Current validated resource catalog.
    pub state: AgentDefinitionState,
    /// Explicitly initialized application databases.
    pub persistence: Persistence,
    /// Existing sandbox workspace root.
    pub workspace: PathBuf,
    /// Existing private-storage parent outside the workspace.
    pub private_parent: PathBuf,
    /// Explicit operator grants; empty denies privileged operations.
    pub authority: RoutineAuthority,
}

/// One shared application execution owner and its authenticated transport adapters.
pub struct ProductionRuntime {
    pub(crate) invocations: InvocationService,
    pub(crate) rpc: RoutineRpcService,
    pub(crate) code: Arc<dyn bindings::CodeComposition>,
    cancel: CancellationToken,
    catalog_task: Mutex<Option<tokio::task::JoinHandle<Result<(), PortError>>>>,
    _private: PrivateRoot,
    health: tokio::sync::watch::Sender<super::RuntimeHealth>,
}
impl std::fmt::Debug for ProductionRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProductionRuntime").finish_non_exhaustive()
    }
}
struct PrivateRoot(PathBuf);
impl Drop for PrivateRoot {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_dir_all(&self.0) {
            tracing::warn!(%error, "private Routine storage cleanup failed");
        }
    }
}
impl ProductionRuntime {
    pub(crate) fn local_generation_lifetime(&self) -> CancellationToken {
        self.cancel.clone()
    }
    /// Compose the authenticated adapters around one shared execution service.
    pub async fn new<S>(
        input: ProductionInput,
        sessions: SessionCoordinator<S>,
        factory: Arc<AgentSessionRuntimeFactory>,
    ) -> Result<Arc<Self>, PortError>
    where
        S: SessionStore + Clone + Send + Sync + 'static,
    {
        input.authority.validate()?;
        let mut updates = input.state.subscribe();
        let snapshot = input.state.snapshot();
        let (entries, capabilities) = catalog_entries(&snapshot, &input.authority)?;
        let catalog = Arc::new(MemoryCatalog::new(entries, capabilities)?);
        let engine = Arc::new(waltr_script::Engine::new(128).map_err(|_| PortError::Unavailable)?);
        let history = input.persistence.script_invocation_store();
        history.reconcile(SystemClock.now_ms()).await?;
        let invocations = InvocationService::new(
            engine.clone(),
            history.clone(),
            Arc::new(Unbound),
            Arc::new(SystemClock),
            128,
        )?;
        let private_parent =
            std::fs::canonicalize(&input.private_parent).map_err(|_| PortError::Unavailable)?;
        let workspace =
            std::fs::canonicalize(&input.workspace).map_err(|_| PortError::Unavailable)?;
        let private_path = private_parent.join(format!("waltr-routine-{}", uuid::Uuid::new_v4()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&private_path)
                .map_err(|_| PortError::Unavailable)?;
        }
        #[cfg(not(unix))]
        return Err(PortError::UnsupportedCapability);
        let private = PrivateRoot(private_path);
        let clock = Arc::new(blob::MonotonicClock::default());
        let blobs = Arc::new(
            FilesystemBlobStore::new(FilesystemConfig {
                private_root: private.0.clone(),
                workspace_root: workspace,
                clock: clock.clone(),
            })
            .map_err(|_| PortError::Unavailable)?,
        );
        let text = input.persistence.script_text_result_store();
        let attachments = AttachmentRegistry::new(history.clone(), blobs.clone());
        let blob_registry =
            BlobRegistry::new(blobs.clone(), clock, history.clone(), attachments.clone());
        let catalog_authority = input.authority.clone();
        let policy = bindings::Composition::new(
            input.state.clone(),
            input.authority,
            sessions,
            factory,
            invocations.clone(),
            input.persistence,
            blobs,
            text.clone(),
        )?;
        let rpc = RoutineRpcService::new(
            ReadAdapter {
                service: invocations.clone(),
                history,
                catalog: catalog.clone(),
                policy: policy.clone(),
                text,
            },
            attachments,
            blob_registry,
        );
        let cancel = CancellationToken::new();
        let stopping = cancel.clone();
        let (health, _) = tokio::sync::watch::channel(super::RuntimeHealth::Running);
        let task_health = health.clone();
        let task_service = invocations.clone();
        let state = input.state;
        let catalog_task = tokio::spawn(async move {
            let result = async {
            loop {
                tokio::select! { biased;
                    () = stopping.cancelled() => return Ok(()),
                    changed = updates.recv() => {
                        match changed {
                            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {},
                            Err(_) => return Err(PortError::Unavailable),
                        }
                        let snapshot = state.snapshot();
                        let (entries, capabilities) = catalog_entries(&snapshot, &catalog_authority)?;
                        catalog.replace(entries, capabilities)?;
                    }
                }
            }
            }.await;
            if result.is_err() {
                let _maintenance = task_service.set_maintenance(true);
                task_health.send_replace(super::RuntimeHealth::Failed(
                    "catalog coordination failed".into(),
                ));
            }
            result
        });
        Ok(Arc::new(Self {
            invocations,
            rpc,
            code: policy,
            cancel,
            catalog_task: Mutex::new(Some(catalog_task)),
            _private: private,
            health,
        }))
    }
    pub(crate) fn health(&self) -> tokio::sync::watch::Receiver<super::RuntimeHealth> {
        self.health.subscribe()
    }
    /// Close admission, cancel resources, and join every retained owner.
    pub async fn shutdown(&self) -> Result<(), PortError> {
        let maintenance = self.invocations.set_maintenance(true);
        self.cancel.cancel();
        self.health
            .send_replace(super::RuntimeHealth::Failed("shutdown".into()));
        let code = self.code.stop();
        let rpc = self.rpc.shutdown().await;
        let coordination = self.code.join().await;
        let (task, lock) = match self.catalog_task.lock() {
            Ok(mut slot) => (slot.take(), Ok(())),
            Err(poisoned) => (poisoned.into_inner().take(), Err(PortError::Uncertain)),
        };
        let catalog = match task {
            Some(task) => task.await.unwrap_or(Err(PortError::Uncertain)),
            None => Ok(()),
        };
        maintenance
            .and(code)
            .and(rpc)
            .and(coordination)
            .and(lock)
            .and(catalog)
    }
}
struct Unbound;
impl HostFactory for Unbound {
    fn preflight(
        &self,
        _: InvocationBinding,
        _: Arc<Definition>,
        _: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<PreparedHost> {
        Box::pin(async { Err(PortError::Denied) })
    }
}
fn catalog_entries(
    snapshot: &crate::resource_catalog::AgentDefinitionSnapshot,
    authority: &RoutineAuthority,
) -> Result<(BTreeMap<String, CatalogEntry>, BTreeSet<String>), PortError> {
    let mut entries = BTreeMap::new();
    let mut capabilities = BTreeSet::from(
        [
            "helper.run",
            "helper.validate",
            "context.snapshot",
            "context.original",
            "context.history",
            "context.metadata",
            "context.transact",
            "context.create",
            "context.start",
            "context.continue",
            "children.call",
        ]
        .map(str::to_owned),
    );
    capabilities.insert("media.execute".into());
    if !authority.targets.is_empty() {
        capabilities.extend(["tools.execute".into(), "tools.schemas".into()]);
        capabilities.extend(authority.targets.keys().cloned());
    }
    for (name, definition) in snapshot.scripts() {
        for capability in &definition.envelope().spec.capabilities {
            if let Some(capture) = capability.strip_prefix("helper.capture.") {
                if definition.envelope().spec.captures.contains_key(capture) {
                    capabilities.insert(capability.clone());
                }
            }
            if let Some(agent) = capability.strip_prefix("helper.named.") {
                if snapshot.catalog().resolved(agent).is_some() {
                    capabilities.insert(capability.clone());
                }
            }
        }
        let session = name == "configuration"
            || definition
                .envelope()
                .spec
                .capabilities
                .iter()
                .any(|cap| cap.starts_with("context."));
        let offers = definition
            .envelope()
            .spec
            .entrypoints
            .iter()
            .filter_map(|(entry, descriptor)| {
                matches!(descriptor.trigger, Trigger::Manual | Trigger::Interactive).then(|| {
                    (
                        entry.clone(),
                        BTreeSet::from([if session {
                            StartContext::Session
                        } else {
                            StartContext::Global
                        }]),
                    )
                })
            })
            .collect();
        entries.insert(
            name.clone(),
            CatalogEntry {
                definition: definition.clone(),
                offers,
            },
        );
    }
    Ok((entries, capabilities))
}
