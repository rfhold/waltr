//! Consumer-side substitution needs no concrete interpreter or backend constructors.
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use waltr_script::{
    CleanupFuture, EngineInfo, Error, ExecutionReport, HostContext, HostDispatcher, HostFuture,
    HostReply, HostRequest, InvocationId, Limits, ScriptCancellation, ScriptEngine,
    ScriptExecution, ScriptFuture, StructuredValue,
};

struct FakeEngine;
struct FakeExecution {
    context: HostContext,
    host: Option<Arc<dyn HostDispatcher>>,
    cancelled: Arc<AtomicBool>,
}
struct FakeCancellation(Arc<AtomicBool>);
impl ScriptCancellation for FakeCancellation {
    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}
impl Drop for FakeExecution {
    fn drop(&mut self) {
        if self.host.is_some() {
            self.cancelled.store(true, Ordering::Release);
            self.context.close().expect("close");
        }
    }
}
impl ScriptExecution for FakeExecution {
    fn cancellation(&self) -> Arc<dyn ScriptCancellation> {
        Arc::new(FakeCancellation(self.cancelled.clone()))
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
    fn join(mut self: Box<Self>) -> ScriptFuture<ExecutionReport> {
        let host = self.host.take().expect("owner");
        let context = self.context.clone();
        let cancelled = self.cancelled.clone();
        Box::pin(async move {
            let input = StructuredValue::new(serde_json::Value::Bool(true), &Limits::default())
                .expect("input");
            let callback = context.callback(|value| Box::pin(async { Ok(value) }));
            let result = if cancelled.load(Ordering::Acquire) {
                Err(Error::Cancelled)
            } else {
                host.dispatch(
                    HostRequest {
                        operation: "test".into(),
                        arguments: input,
                        bytes: None,
                        callback: Some(callback),
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
            context.close().expect("close");
            let clean = host.cleanup(context.invocation()).await.is_ok();
            ExecutionReport {
                result,
                fuel: 0,
                elapsed: Duration::ZERO,
                cleanup_complete: clean,
            }
        })
    }
}
impl ScriptEngine for FakeEngine {
    fn validate(&self, _source: &str, limits: &Limits) -> Result<(), Error> {
        limits.validate()
    }
    fn start(
        &self,
        _source: String,
        _input: StructuredValue,
        limits: Limits,
        host: Arc<dyn HostDispatcher>,
    ) -> Result<Box<dyn ScriptExecution>, Error> {
        Ok(Box::new(FakeExecution {
            context: HostContext::new(limits)?,
            host: Some(host),
            cancelled: Arc::new(AtomicBool::new(false)),
        }))
    }
    fn info(&self) -> EngineInfo {
        EngineInfo {
            javascript: "fake",
            interpreter: "fake",
            api_version: 1,
            admission_limit: 1,
        }
    }
    fn shutdown(&self) -> ScriptFuture<Result<(), Error>> {
        Box::pin(async { Ok(()) })
    }
}
struct FakeHost;
impl HostDispatcher for FakeHost {
    fn dispatch(&self, request: HostRequest, _context: HostContext) -> HostFuture {
        Box::pin(async move {
            Ok(HostReply::Value(
                request
                    .callback
                    .ok_or(Error::Identity)?
                    .invoke(request.arguments)
                    .await?,
            ))
        })
    }
    fn cleanup(&self, _invocation: InvocationId) -> CleanupFuture {
        Box::pin(async { Ok(()) })
    }
}
#[tokio::test]
async fn public_contracts_are_object_safe_and_substitutable() {
    let engine: &dyn ScriptEngine = &FakeEngine;
    let limits = Limits::default();
    let input = StructuredValue::new(serde_json::Value::Null, &limits).expect("input");
    let execution: Box<dyn ScriptExecution> = engine
        .start(String::new(), input, limits, Arc::new(FakeHost))
        .expect("start");
    drop(execution.cancellation());
    let report = execution.join().await;
    assert_eq!(
        report.result.expect("callback output").as_json(),
        &serde_json::Value::Bool(true)
    );
    assert!(report.cleanup_complete);
    let execution = engine
        .start(
            String::new(),
            StructuredValue::new(serde_json::Value::Null, &Limits::default()).expect("input"),
            Limits::default(),
            Arc::new(FakeHost),
        )
        .expect("start");
    let cancellation: Arc<dyn ScriptCancellation> = execution.cancellation();
    let join = execution.join();
    let control = cancellation.clone();
    tokio::task::spawn_blocking(move || {
        control.cancel();
        control.cancel();
    })
    .await
    .expect("control thread");
    let report = join.await;
    assert_eq!(report.result, Err(Error::Cancelled));
    assert!(report.cleanup_complete);
    cancellation.cancel();
}
