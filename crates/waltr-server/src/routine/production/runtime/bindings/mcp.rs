use super::*;
use crate::{
    agent_host::{
        dispatcher::CapabilityPolicy,
        mcp::{McpClientRuntime, worker_stdio::WorkerStdio},
        runtime_factory::{McpConnectionKind, McpRuntimeFingerprint},
        sandbox_gateway::stdio::SandboxStdioTarget,
    },
    permission::ServerPermissionPolicy,
    routine::{
        adapters::authorized_tools::{McpArguments, RoutineMcpPort},
        history::{Caller, InvocationBinding},
    },
};
use futures_util::StreamExt as _;
use tokio::sync::oneshot;
use tokio_util::task::TaskTracker;
use waltr_agent::{
    HostDispatchBinding, SessionRecord, ToolCall, ToolExecutionContext, ToolProgressSink,
    ToolResult,
};

pub(super) struct Host<S: SessionStore + Clone + Send + Sync + 'static> {
    pub base: RoutineAgentHostFactory,
    pub mcp: Factory<S>,
}

#[derive(Clone)]
pub(super) struct Factory<S: SessionStore + Clone + Send + Sync + 'static> {
    pub sessions: SessionCoordinator<S>,
    pub factory: Arc<AgentSessionRuntimeFactory>,
    pub catalog: AgentDefinitionState,
    pub name: String,
    pub revision: String,
    pub publication: uuid::Uuid,
    pub source: SessionRecord,
    pub principal: AuthenticatedPrincipal,
    pub ceiling: Option<PermissionPolicy>,
}

impl<S: SessionStore + Clone + Send + Sync + 'static> HostFactory for Host<S> {
    fn preflight(
        &self,
        binding: InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        caller: Caller,
    ) -> PortFuture<PreparedHost> {
        let mcp = Arc::new(Invocation {
            source: self.mcp.clone(),
            runtime: tokio::runtime::Handle::current(),
            lifetime: CancellationToken::new(),
            tasks: TaskTracker::new(),
            gate: Mutex::default(),
            cache: tokio::sync::Mutex::default(),
            cleanup: tokio::sync::watch::channel(None).0,
        });
        self.base
            .preflight_mcp(binding, definition, selected, caller, Some(mcp))
    }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> Factory<S> {
    async fn current_record(&self) -> Result<SessionRecord, PortError> {
        self.check_access()?;
        self.factory
            .authorize_session_workspace(&self.source)
            .await
            .map_err(|_| PortError::Denied)?;
        self.current_snapshot().await
    }

    async fn current_snapshot(&self) -> Result<SessionRecord, PortError> {
        let record = self
            .sessions
            .load(&self.source.id)
            .await
            .map_err(|_| PortError::Denied)?
            .record;
        if record.metadata.incarnation != self.source.metadata.incarnation
            || record.agent_definition != self.source.agent_definition
            || record.inference_settings != self.source.inference_settings
            || self.factory.session_roles.source_excluded(&record)
        {
            return Err(PortError::Denied);
        }
        self.check_access()?;
        Ok(record)
    }

    fn check_access(&self) -> Result<(), PortError> {
        if self.principal.access_ended().now_or_never().is_some()
            || self.catalog.snapshot().routine_incarnation(&self.name) != Some(self.publication)
        {
            return Err(PortError::Denied);
        }
        Ok(())
    }

    async fn retired(&self) {
        let mut sessions = self.sessions.watch_sessions();
        let mut updates = self.catalog.subscribe();
        let mut sequence = sessions.initial_sequence();
        if self.current_snapshot().await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                () = self.principal.access_ended() => return,
                update = sessions.next() => {
                    use waltr_agent::SessionListEvent;
                    let Some(update) = update else { return };
                    let gap = update.sequence != sequence.saturating_add(1);
                    sequence = update.sequence;
                    match update.event {
                        SessionListEvent::Cleared => return,
                        SessionListEvent::Deleted(id) if id == self.source.id => return,
                        SessionListEvent::Upsert(summary) if summary.id == self.source.id => {}
                        _ if gap => {}
                        _ => continue,
                    }
                    if self.current_snapshot().await.is_err() {
                        return;
                    }
                }
                update = updates.recv() => {
                    // The identity survives unrelated publications, even if the receiver lagged.
                    if matches!(update, Err(tokio::sync::broadcast::error::RecvError::Closed))
                        || self.check_access().is_err()
                    {
                        return;
                    }
                }
            }
        }
    }
}

type CacheKey = (String, Option<(String, Option<String>)>);

struct Entry {
    fingerprint: McpRuntimeFingerprint,
    credentialed: BTreeSet<String>,
    worker: Option<WorkerStdio>,
    runtime: Arc<McpClientRuntime>,
}

#[derive(Default)]
struct Cache {
    entries: BTreeMap<CacheKey, Entry>,
    uncertain: bool,
}

struct Invocation<S: SessionStore + Clone + Send + Sync + 'static> {
    source: Factory<S>,
    runtime: tokio::runtime::Handle,
    lifetime: CancellationToken,
    tasks: TaskTracker,
    gate: Mutex<()>,
    cache: tokio::sync::Mutex<Cache>,
    cleanup: tokio::sync::watch::Sender<Option<Result<(), PortError>>>,
}

impl<S: SessionStore + Clone + Send + Sync + 'static> RoutineMcpPort for Invocation<S> {
    fn call(
        self: Arc<Self>,
        operation: String,
        request: McpArguments,
        cancellation: CancellationToken,
    ) -> PortFuture<ToolResult> {
        Box::pin(async move {
            let operation_cancel = self.lifetime.child_token();
            let _owner = operation_cancel.clone().drop_guard();
            let task = {
                let _gate = self.gate.lock().map_err(|_| PortError::Uncertain)?;
                if self.lifetime.is_cancelled() || cancellation.is_cancelled() {
                    return Err(PortError::Denied);
                }
                let this = self.clone();
                self.tasks.spawn_on(
                    async move {
                        match std::panic::AssertUnwindSafe(this.execute_watched(
                            &operation,
                            request,
                            operation_cancel,
                        ))
                        .catch_unwind()
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => {
                                this.cache.lock().await.uncertain = true;
                                Err(PortError::Uncertain)
                            }
                        }
                    },
                    &self.runtime,
                )
            };
            tokio::select! {
                biased;
                () = cancellation.cancelled() => Err(PortError::Uncertain),
                result = task => result.map_err(|_| PortError::Uncertain)?,
            }
        })
    }

    fn cleanup(self: Arc<Self>) -> PortFuture<()> {
        Box::pin(async move {
            let mut cleanup = self.cleanup.subscribe();
            {
                let _gate = self.gate.lock().map_err(|_| PortError::Uncertain)?;
                if !self.tasks.is_closed() {
                    self.lifetime.cancel();
                    self.tasks.close();
                    let owner = self.clone();
                    // The guest runtime has its own bounded shutdown; it cannot own MCP tasks.
                    drop(self.runtime.spawn(async move {
                        let outcome = std::panic::AssertUnwindSafe(owner.join_children())
                            .catch_unwind()
                            .await
                            .unwrap_or(Err(PortError::Uncertain));
                        owner.cleanup.send_replace(Some(outcome));
                    }));
                }
            }
            cleanup
                .wait_for(Option::is_some)
                .await
                .map_err(|_| PortError::Uncertain)?
                .ok_or(PortError::Uncertain)?
        })
    }
}

impl<S: SessionStore + Clone + Send + Sync + 'static> Invocation<S> {
    async fn execute_watched(
        &self,
        operation: &str,
        request: McpArguments,
        cancellation: CancellationToken,
    ) -> Result<ToolResult, PortError> {
        let settled = CancellationToken::new();
        let (target, worker) = oneshot::channel::<SandboxStdioTarget>();
        let work = async {
            let _settled = settled.clone().drop_guard();
            self.execute(operation, request, cancellation.clone(), target)
                .await
        };
        let watch = async {
            let retired_worker = async {
                match worker.await {
                    Ok(target) => target.retired().await,
                    Err(_) => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = settled.cancelled() => return,
                () = self.lifetime.cancelled() => return,
                () = cancellation.cancelled() => return,
                () = self.source.retired() => {},
                () = retired_worker => {},
            }
            // Cancel, but keep the tracked work alive so startup and child shutdown can settle.
            self.lifetime.cancel();
        };
        let (result, ()) = tokio::join!(work, watch);
        result
    }

    async fn join_children(&self) -> Result<(), PortError> {
        self.tasks.wait().await;
        let mut cache = self.cache.lock().await;
        let entries = std::mem::take(&mut cache.entries);
        for entry in entries.into_values() {
            cache.uncertain |= entry.runtime.shutdown().await.is_err();
        }
        if cache.uncertain {
            Err(PortError::Uncertain)
        } else {
            Ok(())
        }
    }

    async fn execute(
        &self,
        operation: &str,
        request: McpArguments,
        cancellation: CancellationToken,
        watch_worker: oneshot::Sender<SandboxStdioTarget>,
    ) -> Result<ToolResult, PortError> {
        let mut cache = self.cache.lock().await;
        if cache.uncertain {
            return Err(PortError::Uncertain);
        }
        let source = self.source.current_record().await?;
        let factory = &self.source.factory;
        let incarnation = source
            .metadata
            .incarnation
            .clone()
            .ok_or(PortError::Denied)?;
        let target = ConcreteToolTarget::new(&request.server, &request.tool)
            .map_err(|_| PortError::Invalid)?;
        let (discovery_alias, discovery_policy) = factory
            .routine_mcp_preflight_policy(&source, &target, self.source.ceiling.as_ref())
            .map_err(|_| PortError::Denied)?;
        let mut prepared = factory
            .prepare_routine_mcp(&source, &request.server, &request.tool)
            .await
            .map_err(|_| PortError::Denied)?
            .ok_or(PortError::Denied)?;
        let connection = prepared.connections[0].clone();
        let (selection, sandbox, worker) = if connection.kind == McpConnectionKind::Stdio {
            let selection = request.workspace.map(Into::into).ok_or(PortError::Denied)?;
            let sandbox = factory
                .resolve_workspace(&selection)
                .await
                .map_err(|_| PortError::Denied)?;
            let worker = factory
                .worker_stdio(&sandbox)
                .map_err(|_| PortError::Denied)?;
            let stream = sandbox.stdio_target().map_err(|_| PortError::Denied)?;
            watch_worker.send(stream).map_err(|_| PortError::Denied)?;
            (
                Some((selection.machine_id, selection.working_directory)),
                Some(sandbox),
                Some(worker),
            )
        } else {
            (None, None, None)
        };
        let key = (request.server, selection);
        let fingerprint = prepared.fingerprint.clone();
        let credentialed = prepared.credentialed_servers.clone();
        let current = {
            let source = self.source.clone();
            let server = connection.key.clone();
            let tool = request.tool.clone();
            let fingerprint = fingerprint.clone();
            let credentialed = credentialed.clone();
            Arc::new(move || {
                let (source, server, tool, fingerprint, credentialed) = (
                    source.clone(),
                    server.clone(),
                    tool.clone(),
                    fingerprint.clone(),
                    credentialed.clone(),
                );
                Box::pin(async move {
                    let Ok(record) = source.current_record().await else {
                        return false;
                    };
                    source
                        .factory
                        .prepare_routine_mcp(&record, &server, &tool)
                        .await
                        .is_ok_and(|prepared| {
                            prepared.is_some_and(|prepared| {
                                prepared.fingerprint == fingerprint
                                    && prepared.credentialed_servers == credentialed
                            })
                        })
                        && source.current_snapshot().await.is_ok()
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
            })
        };
        let make_context = |alias: String, revision: String| {
            ToolExecutionContext::with_progress_and_cancellation(
                ToolCall {
                    id: operation.into(),
                    name: alias,
                    arguments: Value::Object(request.arguments.clone()),
                },
                ToolProgressSink::noop(),
                cancellation.clone(),
            )
            .with_session_context(source.id.clone(), source.snapshot().turn)
            .with_host_operation(HostDispatchBinding {
                invocation_id: operation.into(),
                source_revision: revision,
            })
        };
        let policy = ServerPermissionPolicy::routine(
            factory.capability_policy().as_ref(),
            &make_context(String::new(), self.source.revision.clone()),
            incarnation,
            current,
        )
        .ok_or(PortError::Denied)?;
        if policy
            .authorize(
                &make_context(discovery_alias, self.source.revision.clone()),
                &target.permission_name(),
                &discovery_policy,
                &[],
            )
            .await
            .map_err(|_| PortError::Denied)?
            .is_some()
        {
            return Err(PortError::Denied);
        }
        // Do not start a replacement under a different transport or directory after a config change.
        for ((server, _), entry) in &cache.entries {
            if server == &key.0
                && (entry.fingerprint != fingerprint
                    || entry.credentialed != credentialed
                    || !entry.runtime.reusable())
            {
                entry.runtime.cancel();
                return Err(PortError::Denied);
            }
        }
        let runtime = if let Some(entry) = cache.entries.get(&key) {
            let same = match (&entry.worker, &worker) {
                (Some(a), Some(b)) => a.same_target(b),
                (None, None) => true,
                _ => false,
            };
            if !same
                || !entry.runtime.reusable()
                || entry.fingerprint != fingerprint
                || entry.credentialed != credentialed
            {
                cache.uncertain |= entry.runtime.shutdown().await.is_err();
                return Err(PortError::Denied);
            }
            entry.runtime.clone()
        } else {
            if cache.entries.len() >= 8 {
                return Err(PortError::Denied);
            }
            if let Some(worker) = &worker {
                prepared.config = prepared
                    .config
                    .with_worker_stdio(worker, &prepared.connections);
            }
            let runtime = match McpClientRuntime::start_with_optional_root(
                prepared.config,
                factory.mcp_root_uri().ok(),
                self.lifetime.clone(),
            )
            .await
            {
                Ok(runtime) => runtime,
                Err(error) if error.single_worker_open_rejected() => return Err(PortError::Denied),
                Err(_) if worker.is_some() => {
                    cache.uncertain = true;
                    return Err(PortError::Uncertain);
                }
                Err(_) => return Err(PortError::Denied),
            };
            cache.entries.insert(
                key,
                Entry {
                    fingerprint,
                    credentialed,
                    worker,
                    runtime: runtime.clone(),
                },
            );
            runtime
        };
        let dispatcher = factory
            .routine_mcp_dispatcher(
                &source,
                self.source.sessions.clone(),
                runtime,
                sandbox,
                self.source.ceiling.clone(),
                policy,
            )
            .await
            .map_err(|_| PortError::Denied)?;
        let alias = dispatcher
            .routine_mcp_alias(&target)
            .ok_or(PortError::Denied)?;
        let context = make_context(alias, dispatcher.revision().into());
        for (namespace, value) in source.snapshot().middleware_state {
            // Reserved execution state is intentionally not imported.
            drop(context.result_metadata().set_context(namespace, value));
        }
        match dispatcher.dispatch_routine_mcp(&target, context).await {
            Ok(result) => Ok(result),
            Err(_) => {
                cache.uncertain = true;
                Err(PortError::Uncertain)
            }
        }
    }
}
