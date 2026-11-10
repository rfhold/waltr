//! Independent cancellation control after the execution owner enters join.
#[path = "support/watchdog.rs"]
mod watchdog;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::oneshot;
use waltr_script::{
    CleanupFuture, Engine, Error, ExecutionReport, HostContext, HostDispatcher, HostFuture,
    HostReply, HostRequest, InvocationId, Limits, ScriptCancellation, ScriptEngine,
    StructuredValue,
};

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
struct Host {
    started: Mutex<Option<oneshot::Sender<()>>>,
    cpu_entered: Mutex<Option<oneshot::Sender<()>>>,
    release: Mutex<Option<oneshot::Receiver<()>>>,
    active: Arc<AtomicUsize>,
    cleanups: Arc<AtomicUsize>,
}
impl HostDispatcher for Host {
    fn dispatch(&self, request: HostRequest, _context: HostContext) -> HostFuture {
        self.active.fetch_add(1, Ordering::SeqCst);
        let active = Active(self.active.clone());
        let started = if request.operation == "helper" {
            None
        } else {
            self.started.lock().expect("started").take()
        };
        let release = if request.operation == "barrier" {
            self.release.lock().expect("release").take()
        } else {
            None
        };
        let cpu_entered = if request.operation == "cpu_started" {
            self.cpu_entered.lock().expect("CPU observer").take()
        } else {
            None
        };
        Box::pin(async move {
            let _active = active;
            if let Some(started) = started {
                started.send(()).expect("start observer");
            }
            if let Some(cpu_entered) = cpu_entered {
                cpu_entered.send(()).expect("CPU observer");
            }
            match request.operation.as_str() {
                "barrier" => {
                    release
                        .ok_or(Error::Invalid)?
                        .await
                        .map_err(|_| Error::Invalid)?;
                    Ok(HostReply::Value(request.arguments))
                }
                "helper" => Ok(HostReply::Value(
                    request
                        .callback
                        .ok_or(Error::Invalid)?
                        .invoke(request.arguments)
                        .await?,
                )),
                "nested" => std::future::pending().await,
                "cpu_started" => Ok(HostReply::Value(request.arguments)),
                _ => Err(Error::Denied),
            }
        })
    }
    fn cleanup(&self, _invocation: InvocationId) -> CleanupFuture {
        assert_eq!(
            self.active.load(Ordering::SeqCst),
            0,
            "host futures must drop before cleanup"
        );
        self.cleanups.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn independent_cancellation_watchdog() {
    let mut command = std::process::Command::new(std::env::current_exe().expect("exe"));
    command
        .args(["--exact", "cancellation_child", "--ignored", "--nocapture"])
        .env("WALTR_SCRIPT_CANCELLATION_CHILD", "1");
    let result = watchdog::run(command, Duration::from_secs(30), 16_384);
    assert!(
        result.success && !result.killed && !result.overflow,
        "{}",
        String::from_utf8_lossy(&result.output)
    );
}

#[test]
#[ignore = "disposable watchdog child"]
fn cancellation_child() {
    assert!(std::env::var_os("WALTR_SCRIPT_CANCELLATION_CHILD").is_some());
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
        .block_on(async {
            for mode in ["cpu", "nested", "drop"] {
                exercise(mode).await;
            }
        });
}

async fn exercise(mode: &str) {
    let engine = Engine::new(1).expect("engine");
    let (started, ready) = oneshot::channel();
    let (release, wait) = oneshot::channel();
    let (cpu_entered, cpu_ready) = oneshot::channel();
    let host = Arc::new(Host {
        started: Mutex::new(Some(started)),
        cpu_entered: Mutex::new(Some(cpu_entered)),
        release: Mutex::new(Some(wait)),
        active: Arc::new(AtomicUsize::new(0)),
        cleanups: Arc::new(AtomicUsize::new(0)),
    });
    let body = match mode {
        "cpu" => {
            // The unawaited marker proves CPU-loop entry without a host wait.
            "await host.call('barrier',null);host.call('cpu_started',null);while(true) await Promise.resolve()"
        }
        "nested" => "return await host.call('helper',null,async()=>await host.call('nested',null))",
        _ => "await host.call('barrier',null);return 7",
    };
    let limits = Limits {
        fuel_slice: 100_000_000,
        fuel_total: 100_000_000_000,
        deadline: Duration::from_mins(1),
        ..Limits::default()
    };
    let execution = engine
        .start(
            format!("export default async function(){{{body}}}"),
            StructuredValue::new(serde_json::Value::Null, &limits).expect("input"),
            limits,
            host.clone(),
        )
        .expect("start");
    let cancellation = execution.cancellation();
    drop(execution.cancellation());
    let (polled, observed) = oneshot::channel();
    let terminal = tokio::spawn(async move {
        let mut join = execution.join();
        assert!(
            futures_util::poll!(join.as_mut()).is_pending(),
            "join must enter Pending before control cancellation"
        );
        polled.send(()).expect("poll observer");
        join.await
    });
    observed.await.expect("join polled");
    ready.await.expect("host start barrier");
    assert_eq!(host.cleanups.load(Ordering::SeqCst), 0);
    finish_pending_case(mode, cancellation, terminal, (release, cpu_ready), &host).await;
    assert_eq!(host.active.load(Ordering::SeqCst), 0);
    assert_eq!(host.cleanups.load(Ordering::SeqCst), 1);
    // Re-admission after terminal proves the reaper completed joined teardown.
    let next = engine
        .start(
            "export default async function(){return 1}".into(),
            StructuredValue::new(serde_json::Value::Null, &Limits::default()).expect("input"),
            Limits::default(),
            host.clone(),
        )
        .expect("joined admission released");
    assert!(next.join().await.result.is_ok());
    engine.shutdown().await.expect("shutdown join");
}

async fn finish_pending_case(
    mode: &str,
    cancellation: Arc<dyn ScriptCancellation>,
    mut terminal: tokio::task::JoinHandle<ExecutionReport>,
    signals: (oneshot::Sender<()>, oneshot::Receiver<()>),
    host: &Host,
) {
    let (release, cpu_ready) = signals;
    if mode == "drop" {
        drop(cancellation);
        release.send(()).expect("release");
        let report = tokio::time::timeout(Duration::from_secs(10), &mut terminal)
            .await
            .expect("terminal timeout")
            .expect("join task");
        assert_eq!(
            report
                .result
                .expect("handle drop must not cancel")
                .as_json(),
            &serde_json::json!(7)
        );
        assert!(report.cleanup_complete);
    } else {
        if mode == "cpu" {
            release.send(()).expect("release CPU loop");
            cpu_ready.await.expect("CPU loop entered");
            while host.active.load(Ordering::SeqCst) != 0 {
                tokio::task::yield_now().await;
            }
        } else {
            assert_eq!(
                host.active.load(Ordering::SeqCst),
                2,
                "nested callback and host wait"
            );
        }
        assert!(
            !terminal.is_finished(),
            "no incidental fuel/deadline termination"
        );
        let start = Instant::now();
        let control = cancellation.clone();
        tokio::spawn(async move {
            control.cancel();
            control.cancel();
        })
        .await
        .expect("control task");
        let report = tokio::time::timeout(Duration::from_secs(10), &mut terminal)
            .await
            .expect("cancel timeout")
            .expect("join task");
        assert_eq!(report.result, Err(Error::Cancelled), "{mode}");
        assert!(report.cleanup_complete);
        assert!(start.elapsed() < Duration::from_secs(10));
        assert!(
            report.elapsed < Duration::from_secs(15),
            "well before the one-minute deadline"
        );
        cancellation.cancel();
        cancellation.clone().cancel();
    }
}
