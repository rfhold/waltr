//! Bounded child invocations use the shared invocation service and immutable definitions.

use super::{
    definition::Definition,
    history::{Caller, Origin, Ownership, Status},
    ports::{PortError, PortFuture},
    service::{ChildInvocation, InvocationHandle, InvocationService},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tokio_util::sync::CancellationToken;
use waltr_agent::ContextIdentity;

/// Conservative whole-tree limits; zero, unbounded, and independently extended deadlines reject.
#[derive(Clone, Copy, Debug)]
pub struct ChildBounds {
    /// Maximum ancestry length, including the root.
    pub depth: usize,
    /// Maximum attempted children across the entire tree.
    pub total: usize,
    /// Fail-fast whole-tree execution reservations.
    pub parallel: usize,
}
impl Default for ChildBounds {
    fn default() -> Self {
        Self {
            depth: 4,
            total: 16,
            parallel: 4,
        }
    }
}
impl ChildBounds {
    /// Reject zero or values above the conservative service ceilings.
    pub fn validate(self) -> Result<Self, PortError> {
        if self.depth == 0
            || self.depth > 4
            || self.total == 0
            || self.total > 16
            || self.parallel == 0
            || self.parallel > 4
        {
            return Err(PortError::Invalid);
        }
        Ok(self)
    }
}

/// Host-created ancestry and capability ceiling. None of these fields is guest-deserializable.
#[derive(Clone, Debug)]
pub struct ChildScope {
    /// Exact authorized source identity; not guest-deserializable.
    pub snapshot: ContextIdentity,
    /// Inherited authenticated principal.
    pub principal: String,
    /// Maximum allowed capabilities; descendants only reduce this set.
    pub capabilities: BTreeSet<String>,
    /// One absolute tree deadline, never extended by a child.
    pub deadline: tokio::time::Instant,
    ancestry: Vec<String>,
    bounds: ChildBounds,
    total: Arc<AtomicUsize>,
    parallel: Arc<tokio::sync::Semaphore>,
}
impl ChildScope {
    /// Construct a trusted tree root with one bounded total and concurrency budget.
    pub fn root(
        snapshot: ContextIdentity,
        principal: String,
        capabilities: BTreeSet<String>,
        key: String,
        deadline: tokio::time::Instant,
    ) -> Result<Self, PortError> {
        let bounds = ChildBounds::default();
        let now = tokio::time::Instant::now();
        if principal.is_empty()
            || principal.len() > 128
            || key.is_empty()
            || key.len() > 256
            || capabilities.len() > 64
            || deadline <= now
            || deadline.duration_since(now) > std::time::Duration::from_secs(30)
        {
            return Err(PortError::Invalid);
        }
        Ok(Self {
            snapshot,
            principal,
            capabilities,
            deadline,
            ancestry: vec![key],
            bounds,
            total: Arc::new(AtomicUsize::new(0)),
            parallel: Arc::new(tokio::sync::Semaphore::new(bounds.parallel)),
        })
    }
    fn child(&self, definition: &Definition, entrypoint: &str) -> Result<Self, PortError> {
        let key = format!("{}:{entrypoint}", definition.envelope().name);
        if self.ancestry.contains(&key) || self.ancestry.len() >= self.bounds.depth {
            return Err(PortError::Conflict);
        }
        if !definition
            .envelope()
            .spec
            .capabilities
            .is_subset(&self.capabilities)
        {
            return Err(PortError::Denied);
        }
        if tokio::time::Instant::now() >= self.deadline {
            return Err(PortError::Denied);
        }
        self.total
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.bounds.total).then_some(count + 1)
            })
            .map_err(|_| PortError::Denied)?;
        let mut child = self.clone();
        child.ancestry.push(key);
        child.capabilities = definition.envelope().spec.capabilities.clone();
        Ok(child)
    }
}

/// One child program request; definitions are resolved from the host's immutable map.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChildCall {
    /// Unique batch decision identity.
    pub key: String,
    /// Exact host-configured program identity.
    pub definition: String,
    /// Exact declared entrypoint.
    pub entrypoint: String,
    /// Complete bounded child input, never authority.
    pub input: Value,
}
/// Joined observation for every requested decision, including unadmitted children.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChildResult {
    /// Original unique decision identity.
    pub key: String,
    /// Immutable definition selection.
    pub definition: String,
    /// Successful output only; no partial output becomes a success.
    pub result: Option<Value>,
    /// True only after durable success and joined cleanup.
    pub succeeded: bool,
    /// Whether required teardown was verified.
    pub cleanup_complete: bool,
}

/// Replaceable child composition boundary with explicit cleanup ownership.
pub trait ChildPort: Send + Sync {
    /// Validate a complete batch before any operation intent.
    fn check(&self, calls: &[ChildCall]) -> Result<(), PortError>;
    /// Execute bounded children, cancelling and joining siblings after failure.
    fn call(
        &self,
        calls: Vec<ChildCall>,
        cancellation: CancellationToken,
    ) -> PortFuture<Vec<ChildResult>>;
    /// Close admission, cancel, and join all registered host-runtime drivers.
    fn cleanup(&self) -> PortFuture<()>;
}

struct Owners {
    closed: bool,
    drivers: Vec<tokio::task::JoinHandle<()>>,
    cleanup: Option<tokio::task::JoinHandle<()>>,
    result: Option<Result<(), PortError>>,
}
/// Fail-fast capacity: no child waits for a service/engine slot held by its parent.
/// At most 16 registered host-runtime drivers; no second engine or bypass executor.
pub struct ServiceChildren {
    service: InvocationService,
    factory: Option<Arc<dyn super::ports::HostFactory>>,
    definitions: BTreeMap<String, Arc<Definition>>,
    scope: ChildScope,
    runtime: tokio::runtime::Handle,
    cancellation: CancellationToken,
    owners: Arc<Mutex<Owners>>,
    notify: Arc<tokio::sync::Notify>,
}
impl std::fmt::Debug for ServiceChildren {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServiceChildren").finish_non_exhaustive()
    }
}
impl ServiceChildren {
    /// Construct with the shared invocation service, not a second engine or execution path.
    pub fn new(
        service: InvocationService,
        definitions: BTreeMap<String, Arc<Definition>>,
        scope: ChildScope,
    ) -> Result<Self, PortError> {
        if definitions.len() > 16 {
            return Err(PortError::Invalid);
        }
        Ok(Self {
            service,
            factory: None,
            definitions,
            scope,
            runtime: tokio::runtime::Handle::try_current().map_err(|_| PortError::Unavailable)?,
            cancellation: CancellationToken::new(),
            owners: Arc::new(Mutex::new(Owners {
                closed: false,
                drivers: Vec::new(),
                cleanup: None,
                result: None,
            })),
            notify: Arc::new(tokio::sync::Notify::new()),
        })
    }
}
impl ServiceChildren {
    pub(crate) fn with_factory(mut self, factory: Arc<dyn super::ports::HostFactory>) -> Self {
        self.factory = Some(factory);
        self
    }
}
impl ChildPort for ServiceChildren {
    fn check(&self, calls: &[ChildCall]) -> Result<(), PortError> {
        if calls.is_empty()
            || calls.len() > self.scope.bounds.parallel
            || serde_json::to_vec(&calls.iter().map(|call| &call.input).collect::<Vec<_>>())
                .map_err(|_| PortError::Invalid)?
                .len()
                > 65_536
        {
            return Err(PortError::Invalid);
        }
        let mut keys = BTreeSet::new();
        for call in calls {
            if call.key.is_empty()
                || call.key.len() > 128
                || call.definition.len() > 100
                || call.entrypoint.len() > 128
                || !keys.insert(&call.key)
            {
                return Err(PortError::Invalid);
            }
            let definition = self
                .definitions
                .get(&call.definition)
                .ok_or(PortError::Denied)?;
            let key = format!("{}:{}", definition.envelope().name, call.entrypoint);
            if self.scope.ancestry.contains(&key)
                || self.scope.ancestry.len() >= self.scope.bounds.depth
            {
                return Err(PortError::Conflict);
            }
            if !definition
                .envelope()
                .spec
                .capabilities
                .is_subset(&self.scope.capabilities)
            {
                return Err(PortError::Denied);
            }
        }
        Ok(())
    }
    fn call(
        &self,
        calls: Vec<ChildCall>,
        cancellation: CancellationToken,
    ) -> PortFuture<Vec<ChildResult>> {
        let checked = self.check(&calls);
        if let Err(error) = checked {
            return Box::pin(async move { Err(error) });
        }
        let mut owners = match self.owners.lock() {
            Ok(owners) => owners,
            Err(_) => return Box::pin(async { Err(PortError::Uncertain) }),
        };
        if owners.closed || owners.drivers.len() >= 16 {
            return Box::pin(async { Err(PortError::Denied) });
        }
        let service = self.service.clone();
        let factory = self.factory.clone();
        let definitions = self.definitions.clone();
        let scope = self.scope.clone();
        let stop = self.cancellation.clone();
        let admission_gate = self.owners.clone();
        let (send, receive) = tokio::sync::oneshot::channel();
        owners.drivers.push(self.runtime.spawn(async move {
            // Admission cannot race ahead of durable driver ownership registration.
            let closed = admission_gate
                .lock()
                .map(|owners| owners.closed)
                .unwrap_or(true);
            let result = if closed {
                Err(PortError::Denied)
            } else {
                run_batch(
                    service,
                    factory,
                    definitions,
                    scope,
                    calls,
                    [cancellation, stop],
                )
                .await
            };
            drop(send.send(result));
        }));
        Box::pin(async move { receive.await.map_err(|_| PortError::Uncertain)? })
    }
    fn cleanup(&self) -> PortFuture<()> {
        self.cancellation.cancel();
        let owners = self.owners.clone();
        let notify = self.notify.clone();
        if let Ok(mut state) = owners.lock() {
            state.closed = true;
            if state.cleanup.is_none() {
                let drivers = std::mem::take(&mut state.drivers);
                let owners = owners.clone();
                let notify = notify.clone();
                state.cleanup = Some(self.runtime.spawn(async move {
                    let mut result = Ok(());
                    for driver in drivers {
                        if driver.await.is_err() {
                            result = Err(PortError::Uncertain);
                        }
                    }
                    if let Ok(mut owners) = owners.lock() {
                        owners.result = Some(result);
                    }
                    notify.notify_waiters();
                }));
            }
        }
        Box::pin(async move {
            loop {
                let notified = notify.notified();
                if let Some(result) = owners.lock().map_err(|_| PortError::Uncertain)?.result {
                    return result;
                }
                notified.await;
            }
        })
    }
}

async fn run_batch(
    service: InvocationService,
    factory: Option<Arc<dyn super::ports::HostFactory>>,
    definitions: BTreeMap<String, Arc<Definition>>,
    scope: ChildScope,
    calls: Vec<ChildCall>,
    cancellations: [CancellationToken; 2],
) -> Result<Vec<ChildResult>, PortError> {
    let [cancellation, stop] = cancellations;
    let requested = calls
        .iter()
        .map(|call| (call.key.clone(), call.definition.clone()))
        .collect::<Vec<_>>();
    let mut handles: Vec<(
        String,
        String,
        Arc<InvocationHandle>,
        tokio::sync::OwnedSemaphorePermit,
    )> = Vec::new();
    let mut pending = futures_util::stream::FuturesUnordered::new();
    let mut failure = false;
    for call in calls {
        let definition = definitions
            .get(&call.definition)
            .ok_or(PortError::Denied)?
            .clone();
        let child = match scope.child(&definition, &call.entrypoint) {
            Ok(child) => child,
            Err(_) => {
                failure = true;
                break;
            }
        };
        let permit = match scope.parallel.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                failure = true;
                break;
            }
        };
        let caller = Caller {
            principal: scope.principal.clone(),
            origin: Origin::Inline,
            ownership: Ownership::Tool,
            correlation: "bounded-child".into(),
        };
        let admission = match service.invoke_child_bound(
            ChildInvocation {
                definition,
                entrypoint: call.entrypoint,
                caller,
                input: call.input,
            },
            child,
            factory.clone(),
        ) {
            Ok(admission) => admission,
            Err(_) => {
                failure = true;
                break;
            }
        };
        let owner = Arc::new(admission.owner);
        let index = handles.len();
        handles.push((call.key, call.definition, owner.clone(), permit));
        let cancellation = cancellation.clone();
        let stop = stop.clone();
        let deadline = scope.deadline;
        pending.push(async move {
            let admitted = tokio::select! {
                biased;
                () = cancellation.cancelled() => None,
                () = stop.cancelled() => None,
                result = tokio::time::timeout_at(deadline, admission.admitted) => result.ok().and_then(Result::ok),
            };
            if admitted.is_none() {
                owner.cancel();
            }
            let outcome = owner.join().await;
            drop(admitted);
            (index, outcome)
        });
    }
    if failure {
        for (_, _, handle, _) in &handles {
            handle.cancel();
        }
    }
    let mut results = Vec::new();
    // Admission acknowledgments and completion are polled together, never in list order.
    use futures_util::StreamExt;
    let mut cancelled = failure;
    loop {
        let completion = tokio::select! {
            biased;
            () = cancellation.cancelled(), if !cancelled => { cancelled = true; for (_, _, handle, _) in &handles { handle.cancel(); } continue; }
            () = stop.cancelled(), if !cancelled => { cancelled = true; for (_, _, handle, _) in &handles { handle.cancel(); } continue; }
            () = tokio::time::sleep_until(scope.deadline), if !cancelled => { cancelled = true; for (_, _, handle, _) in &handles { handle.cancel(); } continue; }
            completion = pending.next() => completion,
        };
        let Some((index, outcome)) = completion else {
            break;
        };
        let succeeded = outcome.status == Some(Status::Succeeded) && outcome.cleanup_complete;
        if !succeeded && !cancelled {
            cancelled = true;
            for (_, _, handle, _) in &handles {
                handle.cancel();
            }
        }
        results.push(ChildResult {
            key: handles[index].0.clone(),
            definition: handles[index].1.clone(),
            result: outcome.result,
            succeeded,
            cleanup_complete: outcome.cleanup_complete,
        });
    }
    if results.iter().any(|result| !result.cleanup_complete) {
        return Err(PortError::Uncertain);
    }
    for (key, definition) in requested {
        if !results.iter().any(|result| result.key == key) {
            results.push(ChildResult {
                key,
                definition,
                result: None,
                succeeded: false,
                cleanup_complete: true,
            });
        }
    }
    if serde_json::to_vec(&results)
        .map_err(|_| PortError::Invalid)?
        .len()
        > 65_536
    {
        return Err(PortError::Invalid);
    }
    Ok(results)
}
