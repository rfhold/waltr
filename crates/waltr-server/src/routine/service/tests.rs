use super::*;
use crate::{
    persistence::{DatabaseConfig, Persistence},
    routine::{
        history::Invocation,
        ports::{PortFuture, SystemClock},
    },
};
use std::{
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};
use waltr_script::{
    Engine, EngineInfo, Error, ExecutionReport, HostReply, ScriptCancellation, ScriptExecution,
    ScriptFuture,
};

fn yaml(source: &str) -> String {
    serde_json::json!({
        "apiVersion": "waltr.routine/v1", "kind": "Routine", "name": "test", "description": "test program",
        "spec": {"source":source,"entrypoints":{"run":{"trigger":"manual"},
            "interactive":{"trigger":"interactive"}, "tool":{"trigger":"tool"},
            "event":{"trigger":"event"},"schedule":{"trigger":"schedule"},
            "lifecycle":{"trigger":"lifecycle"},"progress":{"trigger":"progress"},
            "inline":{"trigger":"inline"},"mcp":{"trigger":"mcp"}},
            "parameters":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":128}},
            "result":{"schemaVersion":"waltr.value/v1","schema":{"type":"string","maxLength":128}},
            "capabilities":["test.echo"]}
    }).to_string()
}
fn caller(origin: Origin) -> Caller {
    Caller {
        principal: "test-owner".into(),
        origin,
        ownership: match origin {
            Origin::Interactive => Ownership::Leased,
            Origin::Tool | Origin::Inline | Origin::Mcp => Ownership::Tool,
            _ => Ownership::Detached,
        },
        correlation: "test-correlation".into(),
    }
}
fn entry(origin: Origin) -> String {
    match origin {
        Origin::Manual => "run",
        Origin::Event => "event",
        Origin::Schedule => "schedule",
        Origin::Lifecycle => "lifecycle",
        Origin::Progress => "progress",
        Origin::Interactive => "interactive",
        Origin::Tool => "tool",
        Origin::Inline => "inline",
        Origin::Mcp => "mcp",
    }
    .into()
}
#[derive(Default)]
struct TestEngine {
    validates: AtomicUsize,
    starts: AtomicUsize,
    start_fail: AtomicBool,
    sources: Mutex<Vec<String>>,
}
impl ScriptEngine for TestEngine {
    fn validate(&self, source: &str, _: &Limits) -> Result<(), Error> {
        self.validates.fetch_add(1, Ordering::SeqCst);
        if source == "BAD" {
            Err(Error::Syntax)
        } else {
            Ok(())
        }
    }
    fn info(&self) -> EngineInfo {
        EngineInfo {
            javascript: "fake-js",
            interpreter: "fake-interpreter",
            api_version: 1,
            admission_limit: 128,
        }
    }
    fn start(
        &self,
        source: String,
        input: StructuredValue,
        limits: Limits,
        host: Arc<dyn HostDispatcher>,
    ) -> Result<Box<dyn ScriptExecution>, Error> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.sources.lock().unwrap().push(source.clone());
        if self.start_fail.load(Ordering::SeqCst) {
            return Err(Error::Shutdown);
        }
        Ok(Box::new(TestExecution {
            source,
            input,
            limits,
            host,
            cancelled: Arc::new(TestCancellation(CancellationToken::new())),
        }))
    }
    fn shutdown(&self) -> ScriptFuture<Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct TestExecution {
    source: String,
    input: StructuredValue,
    limits: Limits,
    host: Arc<dyn HostDispatcher>,
    cancelled: Arc<TestCancellation>,
}
struct TestCancellation(CancellationToken);
impl ScriptCancellation for TestCancellation {
    fn cancel(&self) {
        self.0.cancel();
    }
}
impl ScriptExecution for TestExecution {
    fn cancellation(&self) -> Arc<dyn ScriptCancellation> {
        self.cancelled.clone()
    }
    fn cancel(&self) {
        self.cancelled.cancel();
    }
    fn join(self: Box<Self>) -> ScriptFuture<ExecutionReport> {
        Box::pin(async move {
            let start = Instant::now();
            let context = HostContext::new(self.limits).unwrap();
            let invocation = context.invocation();
            let result = if self.cancelled.0.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                self.host
                    .dispatch(
                        HostRequest {
                            operation: self.source,
                            arguments: self.input,
                            bytes: None,
                            callback: None,
                            handle: None,
                        },
                        context.clone(),
                    )
                    .await
                    .and_then(|reply| match reply {
                        HostReply::Value(value) => Ok(value),
                        _ => Err(Error::Value),
                    })
            };
            context.close().unwrap();
            let cleanup_complete = self.host.cleanup(invocation).await.is_ok();
            ExecutionReport {
                result,
                cleanup_complete,
                elapsed: start.elapsed(),
                fuel: 1,
            }
        })
    }
}
#[derive(Default)]
struct Flags {
    effects: AtomicUsize,
    drops: AtomicUsize,
    finalized: AtomicUsize,
    cleanups: AtomicUsize,
    finalize_fail: AtomicBool,
    cleanup_fail: AtomicBool,
    effect_fail: AtomicBool,
    preflight_block: AtomicBool,
    preflight_entered: Notify,
    preflight_gate: Notify,
    gate: Notify,
    entered: Notify,
    cleanup_block: AtomicBool,
    cleanup_entered: Notify,
    cleanup_gate: Notify,
    preflights: AtomicUsize,
    selected: Mutex<Vec<SelectedEntrypoint>>,
}
struct TestReservation(Arc<Flags>);
impl Drop for TestReservation {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}
impl Reservation for TestReservation {
    fn finalize(&self, _: bool) -> PortFuture<()> {
        self.0.finalized.fetch_add(1, Ordering::SeqCst);
        let failed = self.0.finalize_fail.load(Ordering::SeqCst);
        Box::pin(async move {
            if failed {
                Err(PortError::Uncertain)
            } else {
                Ok(())
            }
        })
    }
}
struct TestFactory {
    flags: Arc<Flags>,
    store: Arc<dyn InvocationStore>,
}
impl HostFactory for TestFactory {
    fn preflight(
        &self,
        _: crate::routine::history::InvocationBinding,
        definition: Arc<Definition>,
        selected: SelectedEntrypoint,
        _: Caller,
    ) -> PortFuture<PreparedHost> {
        self.flags.preflights.fetch_add(1, Ordering::SeqCst);
        self.flags.selected.lock().unwrap().push(selected);
        let flags = self.flags.clone();
        let store = self.store.clone();
        Box::pin(async move {
            if definition
                .envelope()
                .spec
                .capabilities
                .iter()
                .any(|s| s != "test.echo")
            {
                return Err(PortError::UnsupportedCapability);
            }
            let reservation = Box::new(TestReservation(flags.clone()));
            if flags.preflight_block.load(Ordering::SeqCst) {
                flags.preflight_entered.notify_one();
                flags.preflight_gate.notified().await;
            }
            Ok(PreparedHost {
                dispatcher: Arc::new(TestHost {
                    flags: flags.clone(),
                    store,
                }),
                policy: Arc::new(TestPolicy),
                reservation,
                snapshot: Some("snapshot-v1".into()),
            })
        })
    }
}
struct TestPolicy;
impl OperationPolicy for TestPolicy {
    fn before(&self, request: &HostRequest, _: &HostContext) -> PortFuture<Value> {
        let name = request.operation.clone();
        let arguments = request.arguments.as_json().clone();
        Box::pin(async move {
            if !matches!(name.as_str(), "echo" | "wait") || !arguments.is_string() {
                return Err(PortError::Denied);
            }
            Ok(serde_json::json!({"argumentType":"string"}))
        })
    }
    fn project_result(&self, _: &str, _: &HostReply) -> Result<Option<Value>, PortError> {
        Ok(None)
    }
}
struct TestHost {
    flags: Arc<Flags>,
    store: Arc<dyn InvocationStore>,
}
impl HostDispatcher for TestHost {
    fn dispatch(&self, request: HostRequest, _: HostContext) -> HostFuture {
        let flags = self.flags.clone();
        let store = self.store.clone();
        Box::pin(async move {
            let invocations = store.list(0, 100).await.unwrap();
            let mut intents = 0;
            for invocation in invocations {
                intents += store.operations(invocation.id, 0, 100).await.unwrap().len();
            }
            assert!(intents > 0, "no host effect may precede a committed intent");
            flags.effects.fetch_add(1, Ordering::SeqCst);
            flags.entered.notify_one();
            if request.operation == "wait" {
                flags.gate.notified().await;
            }
            if flags.effect_fail.load(Ordering::SeqCst) {
                return Err(Error::Denied);
            }
            Ok(HostReply::Value(request.arguments))
        })
    }
    fn cleanup(&self, _: InvocationId) -> CleanupFuture {
        self.flags.cleanups.fetch_add(1, Ordering::SeqCst);
        let failed = self.flags.cleanup_fail.load(Ordering::SeqCst);
        let flags = self.flags.clone();
        Box::pin(async move {
            if flags.cleanup_block.load(Ordering::SeqCst) {
                flags.cleanup_entered.notify_one();
                flags.cleanup_gate.notified().await;
            }
            if failed { Err(Error::Cleanup) } else { Ok(()) }
        })
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    AdmitBefore,
    AdmitAfter,
    Intent,
    OperationDone,
    Terminal,
}
struct FaultStore {
    inner: Arc<dyn InvocationStore>,
    fault: Arc<Mutex<Fault>>,
    intent_entered: Arc<Notify>,
    intent_gate: Arc<Notify>,
}
impl InvocationStore for FaultStore {
    fn admit(&self, admission: Admission) -> PortFuture<()> {
        let fault = *self.fault.lock().unwrap();
        let inner = self.inner.clone();
        Box::pin(async move {
            if fault == Fault::AdmitBefore {
                return Err(PortError::Unavailable);
            }
            inner.admit(admission).await?;
            if fault == Fault::AdmitAfter {
                Err(PortError::Unavailable)
            } else {
                Ok(())
            }
        })
    }
    fn running(&self, id: String, now: i64) -> PortFuture<()> {
        self.inner.running(id, now)
    }
    fn dispatch_intent(&self, op: Operation) -> PortFuture<()> {
        if *self.fault.lock().unwrap() == Fault::Intent {
            return Box::pin(async { Err(PortError::Unavailable) });
        }
        let blocked = op.name == "blockedIntent";
        let entered = self.intent_entered.clone();
        let gate = self.intent_gate.clone();
        let commit = self.inner.dispatch_intent(op);
        Box::pin(async move {
            commit.await?;
            if blocked {
                entered.notify_one();
                gate.notified().await;
            }
            Ok(())
        })
    }
    fn operation_done(
        &self,
        id: String,
        state: OperationStatus,
        result: Option<Value>,
        now: i64,
    ) -> PortFuture<()> {
        if *self.fault.lock().unwrap() == Fault::OperationDone {
            return Box::pin(async { Err(PortError::Unavailable) });
        }
        self.inner.operation_done(id, state, result, now)
    }
    fn terminal(
        &self,
        id: String,
        state: Status,
        result: Option<Value>,
        now: i64,
    ) -> PortFuture<Status> {
        if *self.fault.lock().unwrap() == Fault::Terminal {
            return Box::pin(async { Err(PortError::Unavailable) });
        }
        self.inner.terminal(id, state, result, now)
    }
    fn get(&self, id: String) -> PortFuture<Option<Invocation>> {
        self.inner.get(id)
    }
    fn list(&self, after: i64, limit: u32) -> PortFuture<Vec<Invocation>> {
        self.inner.list(after, limit)
    }
    fn operations(&self, id: String, after: i64, limit: u32) -> PortFuture<Vec<Operation>> {
        self.inner.operations(id, after, limit)
    }
    fn reconcile(&self, now: i64) -> PortFuture<u64> {
        self.inner.reconcile(now)
    }
}
struct Fixture {
    service: InvocationService,
    engine: Arc<TestEngine>,
    store: Arc<FaultStore>,
    flags: Arc<Flags>,
    fault: Arc<Mutex<Fault>>,
    _persistence: Persistence,
}
impl Fixture {
    async fn new() -> Self {
        Self::with_clock(Arc::new(SystemClock)).await
    }
    async fn with_clock(clock: Arc<dyn Clock>) -> Self {
        let persistence = Persistence::initialize(DatabaseConfig::for_tests().unwrap())
            .await
            .unwrap();
        let fault = Arc::new(Mutex::new(Fault::None));
        let store = Arc::new(FaultStore {
            inner: persistence.script_invocation_store(),
            fault: fault.clone(),
            intent_entered: Arc::new(Notify::new()),
            intent_gate: Arc::new(Notify::new()),
        });
        let flags = Arc::new(Flags::default());
        let engine = Arc::new(TestEngine::default());
        let factory = Arc::new(TestFactory {
            flags: flags.clone(),
            store: store.clone(),
        });
        let service =
            InvocationService::new(engine.clone(), store.clone(), factory, clock, 4).unwrap();
        Self {
            service,
            engine,
            store,
            flags,
            fault,
            _persistence: persistence,
        }
    }
    fn definition(&self, source: &str) -> Arc<Definition> {
        Definition::parse(&yaml(source), self.engine.as_ref()).unwrap()
    }
    async fn invoke(&self, source: &str) -> InvocationHandle {
        self.service
            .invoke(
                self.definition(source),
                "run".into(),
                caller(Origin::Manual),
                Value::String("private-value".into()),
            )
            .unwrap()
            .await
            .unwrap()
    }
}

#[test]
fn script_definition_compile_only_closed_versioned_contracts() {
    let engine = TestEngine::default();
    let definition = Definition::parse(&yaml("echo"), &engine).unwrap();
    assert_eq!(engine.validates.load(Ordering::SeqCst), 1);
    assert_eq!(engine.starts.load(Ordering::SeqCst), 0);
    assert!(Definition::parse(&yaml("BAD"), &engine).is_err());
    assert!(Definition::parse("name: old\ntasks: []", &engine).is_err());
    for (from, to) in [
        ("waltr.routine/v1", "waltr.routine/v0"),
        ("waltr.value/v1", "json-schema"),
        ("maxLength", "pattern"),
        (
            "\"trigger\":\"manual\"",
            "\"trigger\":\"manual\",\"tasks\":[]",
        ),
    ] {
        assert!(
            Definition::parse(&yaml("echo").replace(from, to), &engine).is_err(),
            "{to}"
        );
    }
    assert!(
        definition
            .envelope()
            .spec
            .parameters
            .accepts(&serde_json::json!({"unexpected":1}), definition.limits())
            .is_err()
    );
    assert!(
        definition
            .envelope()
            .spec
            .parameters
            .accepts(&Value::String("x".repeat(129)), definition.limits())
            .is_err()
    );
    assert_eq!(
        definition.revision(),
        Definition::parse(&yaml("echo"), &engine)
            .unwrap()
            .revision()
    );
    assert_ne!(
        definition.revision(),
        Definition::parse(&yaml("wait"), &engine)
            .unwrap()
            .revision()
    );
    for budgets in [
        serde_json::json!({"valueBytes":65537}),
        serde_json::json!({"valueBytes":64,"resultBytes":65}),
        serde_json::json!({"deadlineMs":30001}),
        serde_json::json!({"hostCalls":1025}),
        serde_json::json!({"inFlight":33}),
    ] {
        let mut envelope: Value = serde_json::from_str(&yaml("echo")).unwrap();
        envelope["spec"]["budgets"] = budgets;
        assert!(Definition::parse(&envelope.to_string(), &engine).is_err());
    }
}

#[tokio::test]
async fn finished_driver_join_failure_is_sticky_even_after_reusable_was_set() {
    let fixture = Fixture::new().await;
    let completion = Arc::new(Completion {
        outcome: Mutex::new(None),
        notify: Notify::new(),
    });
    let task = tokio::spawn(async { panic!("injected driver failure after reusable publication") });
    while !task.is_finished() {
        tokio::task::yield_now().await;
    }
    fixture.service.shared.gate.lock().unwrap().entries.insert(
        "failed-driver".into(),
        Entry {
            cancel: CancellationToken::new(),
            task,
            reusable: Arc::new(AtomicBool::new(true)),
            completion: completion.clone(),
        },
    );
    let next = fixture.invoke("echo").await;
    next.join().await;
    {
        let gate = fixture.service.shared.gate.lock().unwrap();
        assert!(gate.failed);
        assert_eq!(gate.quarantined, 1);
        assert!(!gate.entries.contains_key("failed-driver"));
    }
    let outcome = completion.outcome.lock().unwrap().clone().unwrap();
    assert!(outcome.status.is_none());
    assert!(!outcome.cleanup_complete);
    assert!(matches!(
        fixture.service.shutdown().await,
        Err(PortError::Uncertain)
    ));
}

#[tokio::test]
async fn shutdown_join_failure_publishes_unknown_completion_without_claiming_cleanup() {
    let fixture = Fixture::new().await;
    let completion = Arc::new(Completion {
        outcome: Mutex::new(None),
        notify: Notify::new(),
    });
    let task = tokio::spawn(async {
        panic!("injected unjoined driver failure");
    });
    fixture.service.shared.gate.lock().unwrap().entries.insert(
        "shutdown-failed-driver".into(),
        Entry {
            cancel: CancellationToken::new(),
            task,
            reusable: Arc::new(AtomicBool::new(true)),
            completion: completion.clone(),
        },
    );
    assert!(matches!(
        fixture.service.shutdown().await,
        Err(PortError::Uncertain)
    ));
    let result = completion.outcome.lock().unwrap().clone().unwrap();
    assert!(result.status.is_none());
    assert!(!result.cleanup_complete);
}

#[tokio::test]
async fn script_preflight_failure_has_no_history_or_effects() {
    let fixture = Fixture::new().await;
    let definition = Definition::parse(
        &yaml("echo").replace("test.echo", "missing.capability"),
        fixture.engine.as_ref(),
    )
    .unwrap();
    let result = fixture
        .service
        .invoke(
            definition,
            "run".into(),
            caller(Origin::Manual),
            Value::String("x".into()),
        )
        .unwrap()
        .await;
    assert!(matches!(result, Err(PortError::UnsupportedCapability)));
    assert!(fixture.store.list(0, 100).await.unwrap().is_empty());
    assert_eq!(fixture.engine.starts.load(Ordering::SeqCst), 0);
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn script_pending_preflight_maintenance_and_shutdown_release_without_admission() {
    for shutdown in [false, true] {
        let fixture = Fixture::new().await;
        fixture.flags.preflight_block.store(true, Ordering::SeqCst);
        let acknowledgment = fixture
            .service
            .invoke(
                fixture.definition("echo"),
                "run".into(),
                caller(Origin::Manual),
                Value::String("x".into()),
            )
            .unwrap();
        fixture.flags.preflight_entered.notified().await;
        if shutdown {
            fixture.service.shutdown().await.unwrap();
        } else {
            fixture.service.set_maintenance(true).unwrap();
            fixture.flags.preflight_gate.notify_one();
        }
        assert!(acknowledgment.await.is_err());
        fixture.service.shutdown().await.unwrap();
        assert!(fixture.store.list(0, 100).await.unwrap().is_empty());
        assert_eq!(fixture.flags.drops.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.engine.starts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn script_bounded_active_owners_and_explicit_detached_cancel() {
    let fixture = Fixture::new().await;
    let mut handles = Vec::new();
    for _ in 0..4 {
        handles.push(fixture.invoke("wait").await);
    }
    assert!(matches!(
        fixture.service.invoke(
            fixture.definition("echo"),
            "run".into(),
            caller(Origin::Manual),
            Value::String("x".into()),
        ),
        Err(PortError::Unavailable)
    ));
    for handle in &handles {
        fixture.service.cancel(&handle.id).unwrap();
    }
    fixture.service.shutdown().await.unwrap();
    for handle in handles {
        assert_ne!(handle.join().await.status, Some(Status::Succeeded));
    }
    assert_eq!(fixture.flags.drops.load(Ordering::SeqCst), 4);
}

struct FixedClock;
impl Clock for FixedClock {
    fn now_ms(&self) -> i64 {
        1729
    }
}

#[tokio::test]
async fn script_dropped_shutdown_waiter_retains_join_responsibility() {
    for fail in [false, true] {
        let fixture = Fixture::new().await;
        fixture.flags.cleanup_block.store(true, Ordering::SeqCst);
        fixture.flags.cleanup_fail.store(fail, Ordering::SeqCst);
        let handle = fixture.invoke("echo").await;
        fixture.flags.cleanup_entered.notified().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(20), fixture.service.shutdown())
                .await
                .is_err()
        );
        let first = fixture.service.shutdown();
        let second = fixture.service.shutdown();
        tokio::pin!(first, second);
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut first)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );
        assert_eq!(fixture.flags.finalized.load(Ordering::SeqCst), 0);
        fixture.flags.cleanup_gate.notify_one();
        let expected = if fail {
            Err(PortError::Uncertain)
        } else {
            Ok(())
        };
        assert_eq!(first.await, expected);
        assert_eq!(second.await, expected);
        assert_eq!(fixture.service.shutdown().await, expected);
        assert!(handle.join().await.status.is_some());
        assert_eq!(
            fixture.flags.drops.load(Ordering::SeqCst),
            usize::from(!fail)
        );
    }
}

#[tokio::test]
async fn script_origin_trigger_matrix_precedes_preflight_and_pins_selected_descriptor() {
    let fixture = Fixture::new().await;
    let origins = [
        Origin::Manual,
        Origin::Event,
        Origin::Schedule,
        Origin::Lifecycle,
        Origin::Progress,
        Origin::Interactive,
        Origin::Tool,
        Origin::Inline,
        Origin::Mcp,
    ];
    for origin in origins {
        for wrong in origins {
            if entry(origin) == entry(wrong) {
                continue;
            }
            assert!(matches!(
                fixture.service.invoke(
                    fixture.definition("echo"),
                    entry(wrong),
                    caller(origin),
                    Value::String("x".into())
                ),
                Err(PortError::Invalid)
            ));
        }
        for ownership in [Ownership::Detached, Ownership::Leased, Ownership::Tool] {
            let allowed = match origin {
                Origin::Interactive => ownership == Ownership::Leased,
                Origin::Tool | Origin::Inline => ownership == Ownership::Tool,
                Origin::Mcp => ownership != Ownership::Detached,
                _ => ownership == Ownership::Detached,
            };
            if allowed {
                continue;
            }
            let mut wrong_owner = caller(origin);
            wrong_owner.ownership = ownership;
            assert!(matches!(
                fixture.service.invoke(
                    fixture.definition("echo"),
                    entry(origin),
                    wrong_owner,
                    Value::String("x".into())
                ),
                Err(PortError::Invalid)
            ));
        }
    }
    assert_eq!(fixture.flags.preflights.load(Ordering::SeqCst), 0);
    assert!(fixture.store.list(0, 100).await.unwrap().is_empty());
    for origin in origins {
        let mut envelope: Value = serde_json::from_str(&yaml("echo")).unwrap();
        envelope["spec"]["entrypoints"][entry(origin)]["selector"] =
            Value::String("pinned-selector".into());
        let definition = Definition::parse(&envelope.to_string(), fixture.engine.as_ref()).unwrap();
        let handle = fixture
            .service
            .invoke(
                definition,
                entry(origin),
                caller(origin),
                Value::String("x".into()),
            )
            .unwrap()
            .await
            .unwrap();
        assert_eq!(handle.join().await.status, Some(Status::Succeeded));
        let selected = fixture
            .flags
            .selected
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(selected.name, entry(origin));
        assert!(compatible(origin, selected.descriptor.trigger));
        assert_eq!(
            selected.descriptor.selector.as_deref(),
            Some("pinned-selector")
        );
    }
    fixture.service.shutdown().await.unwrap();
}

struct BarrierPolicy {
    entered: Arc<Notify>,
    gate: Arc<Notify>,
}
impl OperationPolicy for BarrierPolicy {
    fn before(&self, request: &HostRequest, _: &HostContext) -> PortFuture<Value> {
        let blocked = request.operation == "blockedPolicy";
        let entered = self.entered.clone();
        let gate = self.gate.clone();
        Box::pin(async move {
            if blocked {
                entered.notify_one();
                gate.notified().await;
            }
            Ok(Value::Null)
        })
    }
    fn project_result(&self, _: &str, _: &HostReply) -> Result<Option<Value>, PortError> {
        Ok(None)
    }
}

#[tokio::test]
async fn script_concurrent_journal_failure_fences_policy_and_intent_waiters() {
    for blocked in ["blockedPolicy", "blockedIntent"] {
        let fixture = Fixture::new().await;
        let definition = fixture.definition("echo");
        let id = "journal-race".to_string();
        fixture
            .store
            .admit(Admission {
                id: id.clone(),
                revision: Revision {
                    id: definition.revision().into(),
                    api_version: API_VERSION.into(),
                    engine: definition.engine().into(),
                    envelope: definition.canonical().into(),
                    name: "test".into(),
                },
                caller: caller(Origin::Manual),
                entrypoint: "run".into(),
                snapshot: None,
                admitted_ms: 1,
            })
            .await
            .unwrap();
        fixture.store.running(id.clone(), 2).await.unwrap();
        let entered = Arc::new(Notify::new());
        let gate = Arc::new(Notify::new());
        let cancel = CancellationToken::new();
        let host = Arc::new(JournalHost {
            id: id.clone(),
            trace: tracing::Span::none(),
            store: fixture.store.clone(),
            clock: Arc::new(SystemClock),
            inner: Arc::new(TestHost {
                flags: fixture.flags.clone(),
                store: fixture.store.clone(),
            }),
            policy: Arc::new(BarrierPolicy {
                entered: entered.clone(),
                gate: gate.clone(),
            }),
            failed: Arc::new(AtomicBool::new(false)),
            cancel: cancel.clone(),
        });
        let context = HostContext::new(Limits::default()).unwrap();
        let request = |name: &str| HostRequest {
            operation: name.into(),
            arguments: StructuredValue::new(Value::String("x".into()), &Limits::default()).unwrap(),
            bytes: None,
            callback: None,
            handle: None,
        };
        let pending = tokio::spawn(host.dispatch(request(blocked), context.clone()));
        if blocked == "blockedPolicy" {
            entered.notified().await;
        } else {
            fixture.store.intent_entered.notified().await;
        }
        *fixture.fault.lock().unwrap() = Fault::OperationDone;
        assert!(
            host.dispatch(request("echo"), context.clone())
                .await
                .is_err()
        );
        assert!(cancel.is_cancelled());
        gate.notify_one();
        fixture.store.intent_gate.notify_one();
        assert!(pending.await.unwrap().is_err());
        assert_eq!(fixture.flags.effects.load(Ordering::SeqCst), 1);
        *fixture.fault.lock().unwrap() = Fault::None;
        assert_eq!(
            fixture
                .store
                .terminal(id.clone(), Status::Failed, None, 3)
                .await
                .unwrap(),
            Status::Uncertain
        );
        assert!(
            fixture
                .store
                .operations(id, 0, 100)
                .await
                .unwrap()
                .iter()
                .all(|op| op.status == OperationStatus::Uncertain)
        );
        context.close().unwrap();
        fixture.service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn script_unpolled_tool_and_interactive_ack_drop_cancels_preflight() {
    for origin in [Origin::Tool, Origin::Interactive] {
        let fixture = Fixture::new().await;
        fixture.flags.preflight_block.store(true, Ordering::SeqCst);
        let acknowledgment = fixture
            .service
            .invoke(
                fixture.definition("echo"),
                entry(origin),
                caller(origin),
                Value::String("x".into()),
            )
            .unwrap();
        fixture.flags.preflight_entered.notified().await;
        drop(acknowledgment);
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.flags.drops.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(fixture.store.list(0, 100).await.unwrap().is_empty());
        assert_eq!(fixture.engine.starts.load(Ordering::SeqCst), 0);
        fixture.service.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn script_substitutable_clock_and_engine_start_failure_are_durable() {
    let fixture = Fixture::with_clock(Arc::new(FixedClock)).await;
    fixture.engine.start_fail.store(true, Ordering::SeqCst);
    let handle = fixture.invoke("echo").await;
    assert_eq!(handle.join().await.status, Some(Status::Failed));
    let row = fixture.store.get(handle.id.clone()).await.unwrap().unwrap();
    assert_eq!(row.admitted_ms, 1729);
    assert_eq!(row.updated_ms, 1729);
    assert_eq!(fixture.flags.effects.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.flags.finalized.load(Ordering::SeqCst), 1);
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn script_store_failure_releases_reservation_and_lost_commit_ack_never_executes() {
    for fault in [Fault::AdmitBefore, Fault::AdmitAfter] {
        let fixture = Fixture::new().await;
        *fixture.fault.lock().unwrap() = fault;
        assert!(
            fixture
                .service
                .invoke(
                    fixture.definition("echo"),
                    "run".into(),
                    caller(Origin::Manual),
                    Value::String("x".into())
                )
                .unwrap()
                .await
                .is_err()
        );
        fixture.service.shutdown().await.unwrap();
        assert_eq!(fixture.engine.starts.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.flags.drops.load(Ordering::SeqCst), 1);
        let history = fixture.store.list(0, 100).await.unwrap();
        if fault == Fault::AdmitBefore {
            assert!(history.is_empty());
        } else {
            assert_eq!(history[0].status, Status::Interrupted);
        }
    }
}

#[tokio::test]
async fn script_effects_have_durable_fence_and_safe_projection() {
    let fixture = Fixture::new().await;
    let handle = fixture.invoke("echo").await;
    let outcome = handle.join().await;
    assert_eq!(outcome.status, Some(Status::Succeeded));
    assert_eq!(outcome.result, Some(Value::String("private-value".into())));
    let operations = fixture
        .store
        .operations(handle.id.clone(), 0, 100)
        .await
        .unwrap();
    assert_eq!(operations[0].status, OperationStatus::Completed);
    assert_eq!(
        operations[0].arguments,
        serde_json::json!({"argumentType":"string"})
    );
    assert!(operations[0].result.is_none());
    assert_eq!(fixture.flags.finalized.load(Ordering::SeqCst), 1);
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn script_intent_failure_denies_effect_and_post_effect_failure_is_uncertain() {
    for fault in [Fault::Intent, Fault::OperationDone, Fault::Terminal] {
        let fixture = Fixture::new().await;
        *fixture.fault.lock().unwrap() = fault;
        let handle = fixture.invoke("echo").await;
        let outcome = handle.join().await;
        assert_ne!(outcome.status, Some(Status::Succeeded));
        assert_eq!(
            fixture.flags.effects.load(Ordering::SeqCst),
            usize::from(fault != Fault::Intent)
        );
        if fault == Fault::Terminal {
            assert!(outcome.status.is_none());
            assert_eq!(fixture.flags.drops.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.service.shutdown().await, Err(PortError::Uncertain));
        } else {
            assert_eq!(outcome.status, Some(Status::Uncertain));
            fixture.service.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn script_unknown_operation_and_schema_denial_do_not_dispatch() {
    let fixture = Fixture::new().await;
    assert!(matches!(
        fixture.service.invoke(
            fixture.definition("echo"),
            "run".into(),
            caller(Origin::Manual),
            serde_json::json!(42),
        ),
        Err(PortError::Invalid)
    ));
    assert!(fixture.store.list(0, 100).await.unwrap().is_empty());
    assert_eq!(fixture.engine.starts.load(Ordering::SeqCst), 0);
    let handle = fixture.invoke("unknown").await;
    assert_eq!(handle.join().await.status, Some(Status::Failed));
    assert!(
        fixture
            .store
            .operations(handle.id.clone(), 0, 100)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fixture.flags.effects.load(Ordering::SeqCst), 0);
    fixture.service.shutdown().await.unwrap();
}

#[tokio::test]
async fn script_detached_lost_ack_runs_pinned_revision_and_owner_drop_cancels_lease() {
    let fixture = Fixture::new().await;
    let pinned = fixture.definition("echo");
    let future = fixture
        .service
        .invoke(
            pinned.clone(),
            "run".into(),
            caller(Origin::Manual),
            Value::String("x".into()),
        )
        .unwrap();
    let replacement = fixture.definition("wait");
    assert_ne!(pinned.revision(), replacement.revision());
    drop(future);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let rows = fixture.store.list(0, 100).await.unwrap();
            if rows.first().is_some_and(|r| r.status.terminal()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(fixture.engine.sources.lock().unwrap().as_slice(), &["echo"]);
    let handle = fixture
        .service
        .invoke(
            fixture.definition("wait"),
            "interactive".into(),
            caller(Origin::Interactive),
            Value::String("x".into()),
        )
        .unwrap()
        .await
        .unwrap();
    let id = handle.id.clone();
    // Wait until the effect has actually entered, not just the admission ack.
    while fixture.flags.effects.load(Ordering::SeqCst) < 2 {
        tokio::task::yield_now().await;
    }
    drop(handle);
    fixture.service.shutdown().await.unwrap();
    assert_eq!(
        fixture.store.get(id).await.unwrap().unwrap().status,
        Status::Uncertain
    );
    assert_eq!(fixture.flags.cleanups.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn script_maintenance_covers_every_origin_and_shutdown_joins() {
    let fixture = Fixture::new().await;
    fixture.service.set_maintenance(true).unwrap();
    for origin in [
        Origin::Manual,
        Origin::Event,
        Origin::Schedule,
        Origin::Lifecycle,
        Origin::Interactive,
        Origin::Tool,
        Origin::Inline,
        Origin::Mcp,
        Origin::Progress,
    ] {
        assert!(matches!(
            fixture.service.invoke(
                fixture.definition("echo"),
                entry(origin),
                caller(origin),
                Value::String("x".into())
            ),
            Err(PortError::Denied)
        ));
    }
    fixture.service.set_maintenance(false).unwrap();
    let handle = fixture.invoke("wait").await;
    fixture.flags.entered.notified().await;
    let (first, second) = tokio::join!(fixture.service.shutdown(), fixture.service.shutdown());
    first.unwrap();
    second.unwrap();
    assert_eq!(handle.join().await.status, Some(Status::Uncertain));
    assert_eq!(fixture.flags.cleanups.load(Ordering::SeqCst), 1);
    assert!(
        fixture
            .service
            .invoke(
                fixture.definition("echo"),
                "run".into(),
                caller(Origin::Manual),
                Value::String("x".into())
            )
            .is_err()
    );
}

#[tokio::test]
async fn script_cleanup_or_finalization_failure_retains_reservation_and_never_succeeds() {
    for host_cleanup in [true, false] {
        let fixture = Fixture::new().await;
        if host_cleanup {
            fixture.flags.cleanup_fail.store(true, Ordering::SeqCst);
        } else {
            fixture.flags.finalize_fail.store(true, Ordering::SeqCst);
        }
        let handle = fixture.invoke("echo").await;
        assert_eq!(handle.join().await.status, Some(Status::Uncertain));
        assert_eq!(fixture.flags.drops.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.service.shutdown().await, Err(PortError::Uncertain));
        let flags = fixture.flags.clone();
        drop(fixture);
        assert_eq!(
            flags.drops.load(Ordering::SeqCst),
            0,
            "service Drop must not release an unverified reservation"
        );
    }
}

#[tokio::test]
async fn script_real_engine_cpu_cancel_after_pending_join_is_trusted_and_joined() {
    struct ObservedEngine {
        engine: Engine,
        pending: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
    }
    struct ObservedExecution {
        owner: Box<dyn ScriptExecution>,
        pending: Arc<Notify>,
        cancelled: Arc<AtomicBool>,
    }
    impl ScriptExecution for ObservedExecution {
        fn cancellation(&self) -> Arc<dyn ScriptCancellation> {
            self.owner.cancellation()
        }
        fn cancel(&self) {
            self.owner.cancel();
        }
        fn join(self: Box<Self>) -> ScriptFuture<ExecutionReport> {
            Box::pin(async move {
                let mut joined = self.owner.join();
                match futures_util::poll!(joined.as_mut()) {
                    std::task::Poll::Ready(report) => report,
                    std::task::Poll::Pending => {
                        self.pending.notify_one();
                        let report = joined.await;
                        if matches!(report.result, Err(Error::Cancelled)) {
                            self.cancelled.store(true, Ordering::SeqCst);
                        }
                        report
                    }
                }
            })
        }
    }
    impl ScriptEngine for ObservedEngine {
        fn info(&self) -> EngineInfo {
            self.engine.info()
        }
        fn validate(&self, source: &str, limits: &Limits) -> Result<(), Error> {
            self.engine.validate(source, limits)
        }
        fn start(
            &self,
            source: String,
            input: StructuredValue,
            limits: Limits,
            host: Arc<dyn HostDispatcher>,
        ) -> Result<Box<dyn ScriptExecution>, Error> {
            Ok(Box::new(ObservedExecution {
                owner: self.engine.start(source, input, limits, host)?,
                pending: self.pending.clone(),
                cancelled: self.cancelled.clone(),
            }))
        }
        fn shutdown(&self) -> ScriptFuture<Result<(), Error>> {
            self.engine.shutdown()
        }
    }
    let temporary = std::env::temp_dir();
    let preferred = temporary.join("opencode");
    let base = if preferred.is_dir() {
        preferred
    } else {
        temporary
    };
    let path = base.join(format!("script-s2-cancel-{}.sqlite", uuid::Uuid::new_v4()));
    let persistence = Persistence::initialize(
        DatabaseConfig::from_urls(
            &format!("sqlite:{}?mode=rwc", path.display()),
            "sqlite::memory:",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let store = persistence.script_invocation_store();
    let flags = Arc::new(Flags::default());
    let pending = Arc::new(Notify::new());
    let cancelled = Arc::new(AtomicBool::new(false));
    let engine = Arc::new(ObservedEngine {
        engine: Engine::new(1).unwrap(),
        pending: pending.clone(),
        cancelled: cancelled.clone(),
    });
    let service = InvocationService::new(
        engine.clone(),
        store.clone(),
        Arc::new(TestFactory {
            flags: flags.clone(),
            store: store.clone(),
        }),
        Arc::new(SystemClock),
        1,
    )
    .unwrap();
    let definition = Definition::parse(&yaml("export default async function(input) { await host.call('wait', input); host.call('echo', input); while (true) await Promise.resolve(); }"), engine.as_ref()).unwrap();
    let deadline = definition.limits().deadline;
    let handle = service
        .invoke(
            definition,
            "run".into(),
            caller(Origin::Manual),
            Value::String("cpu-marker".into()),
        )
        .unwrap()
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), pending.notified())
        .await
        .expect("join must be polled pending before cancellation");
    tokio::time::timeout(Duration::from_secs(5), flags.entered.notified())
        .await
        .unwrap();
    flags.gate.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let operations = store.operations(handle.id.clone(), 0, 100).await.unwrap();
            if operations.len() == 2
                && operations
                    .iter()
                    .all(|op| op.status == OperationStatus::Completed)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unawaited CPU-loop marker must complete without a pending host wait");
    assert_eq!(flags.cleanups.load(Ordering::SeqCst), 0);
    assert!(!cancelled.load(Ordering::SeqCst));
    let start = Instant::now();
    handle.cancel();
    let outcome = tokio::time::timeout(Duration::from_secs(5), handle.join())
        .await
        .expect("CPU cancellation must not wait for the engine deadline");
    assert!(start.elapsed() < deadline);
    assert!(
        cancelled.load(Ordering::SeqCst),
        "trusted engine report must be Cancelled, not fuel/deadline exhaustion"
    );
    assert_eq!(outcome.status, Some(Status::Interrupted));
    assert!(outcome.cleanup_complete);
    assert_eq!(flags.cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(flags.finalized.load(Ordering::SeqCst), 1);
    let next = Definition::parse(
        &yaml("export default async function(input) { return await host.call('echo', input); }"),
        engine.as_ref(),
    )
    .unwrap();
    let next = service
        .invoke(
            next,
            "run".into(),
            caller(Origin::Manual),
            Value::String("readmission".into()),
        )
        .unwrap()
        .await
        .unwrap();
    assert_eq!(next.join().await.status, Some(Status::Succeeded));
    assert_eq!(flags.cleanups.load(Ordering::SeqCst), 2);
    service.shutdown().await.unwrap();
    engine.shutdown().await.unwrap();
    persistence.sqlite_pool().unwrap().close().await;
    std::fs::remove_file(path).unwrap();
}

#[tokio::test]
async fn script_real_engine_recording_host_sql_vertical() {
    // Worker runtimes can recycle SQLx connections; durable history must survive
    // connection replacement, unlike an anonymous in-memory SQLite database.
    let temporary = std::env::temp_dir();
    let preferred = temporary.join("opencode");
    let base = if preferred.is_dir() {
        preferred
    } else {
        temporary
    };
    let path = base.join(format!("script-s2-real-{}.sqlite", uuid::Uuid::new_v4()));
    let config = DatabaseConfig::from_urls(
        &format!("sqlite:{}?mode=rwc", path.display()),
        "sqlite::memory:",
    )
    .unwrap();
    let persistence = Persistence::initialize(config).await.unwrap();
    let store = persistence.script_invocation_store();
    let flags = Arc::new(Flags::default());
    let engine = Arc::new(Engine::new(2).unwrap());
    let source = "export default async function(input) { return await host.call('echo', input); }";
    let definition = Definition::parse(&yaml(source), engine.as_ref()).unwrap();
    assert!(
        Definition::parse(
            &yaml("import x from 'ambient'; export default async function() { return x; }"),
            engine.as_ref()
        )
        .is_err()
    );
    assert!(Definition::parse(&yaml("export default async function( {"), engine.as_ref()).is_err());
    let factory = Arc::new(TestFactory {
        flags: flags.clone(),
        store: store.clone(),
    });
    let service = InvocationService::new(
        engine.clone(),
        store.clone(),
        factory,
        Arc::new(SystemClock),
        2,
    )
    .unwrap();
    let handle = service
        .invoke(
            definition,
            "run".into(),
            caller(Origin::Manual),
            Value::String("real-js".into()),
        )
        .unwrap()
        .await
        .unwrap();
    let outcome = handle.join().await;
    assert_eq!(
        outcome.status,
        Some(Status::Succeeded),
        "outcome={outcome:?}; durable={:?}",
        store.get(handle.id.clone()).await
    );
    assert_eq!(flags.effects.load(Ordering::SeqCst), 1);
    let unawaited = Definition::parse(&yaml("export default async function(input) { host.call('wait', input); await host.call('echo', input); return input; }"), engine.as_ref()).unwrap();
    let pending = service
        .invoke(
            unawaited,
            "run".into(),
            caller(Origin::Manual),
            Value::String("unawaited".into()),
        )
        .unwrap()
        .await
        .unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(3), pending.join())
        .await
        .unwrap();
    let operations = store.operations(pending.id.clone(), 0, 100).await.unwrap();
    assert!(
        operations
            .iter()
            .all(|op| op.status != OperationStatus::Dispatched)
    );
    if operations
        .iter()
        .any(|op| op.status == OperationStatus::Uncertain)
    {
        assert_eq!(outcome.status, Some(Status::Uncertain));
    }
    assert!(outcome.cleanup_complete);
    assert_eq!(flags.cleanups.load(Ordering::SeqCst), 2);
    service.shutdown().await.unwrap();
    engine.shutdown().await.unwrap();
    persistence.sqlite_pool().unwrap().close().await;
    std::fs::remove_file(path).unwrap();
}
