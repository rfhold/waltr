//! Deadline-cap refinement without changes to fuel or resource ceilings.
#[path = "support/watchdog.rs"]
mod watchdog;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;
use waltr_script::{
    CleanupFuture, Engine, Error, ExecutionReport, HostContext, HostDispatcher, HostFuture,
    HostReply, HostRequest, InvocationId, Limits, ScriptEngine, StructuredValue,
};

fn long_limits() -> Limits {
    Limits {
        deadline: Duration::from_mins(90),
        ..Limits::default()
    }
}

#[test]
fn deadline_boundaries_and_default() {
    assert_eq!(Limits::default().deadline, Duration::from_secs(30));
    assert_eq!(long_limits().validate(), Ok(()));
    for deadline in [
        Duration::ZERO,
        Duration::from_mins(90) + Duration::from_nanos(1),
        Duration::MAX,
    ] {
        assert_eq!(
            Limits {
                deadline,
                ..Limits::default()
            }
            .validate(),
            Err(Error::Invalid)
        );
    }
}

type NumericField = (&'static str, fn(&mut Limits, usize), usize, usize);
const NUMERIC_FIELDS: [NumericField; 13] = [
    ("source", |l, v| l.source_bytes = v, 65_536, 1),
    ("value", |l, v| l.value_bytes = v, 65_536, 1),
    ("depth", |l, v| l.value_depth = v, 64, 1),
    ("nodes", |l, v| l.value_nodes = v, 8192, 1),
    ("string", |l, v| l.string_bytes = v, 65_536, 1),
    ("binary", |l, v| l.binary_bytes = v, 65_536, 1),
    (
        "heap",
        |l, v| l.heap_bytes = v,
        24 * 1024 * 1024,
        1024 * 1024,
    ),
    ("stack", |l, v| l.stack_bytes = v, 512 * 1024, 16 * 1024),
    (
        "memory",
        |l, v| l.memory_bytes = v,
        32 * 1024 * 1024,
        4 * 1024 * 1024,
    ),
    ("calls", |l, v| l.host_calls = v, 1024, 1),
    ("inflight", |l, v| l.in_flight = v, 32, 1),
    ("callbacks", |l, v| l.callbacks = v, 1024, 1),
    ("handles", |l, v| l.handles = v, 256, 1),
];

#[test]
fn other_numeric_bounds_and_defaults_do_not_change() {
    let d = Limits::default();
    assert_eq!(
        [
            d.source_bytes,
            d.value_bytes,
            d.value_depth,
            d.value_nodes,
            d.string_bytes,
            d.binary_bytes,
            d.heap_bytes,
            d.stack_bytes,
            d.memory_bytes,
            d.host_calls,
            d.in_flight,
            d.callbacks,
            d.handles
        ],
        [
            65_536,
            65_536,
            32,
            4096,
            16_384,
            65_536,
            16 * 1024 * 1024,
            256 * 1024,
            32 * 1024 * 1024,
            256,
            16,
            64,
            32
        ]
    );
    assert_eq!([d.fuel_slice, d.fuel_total], [20_000_000, 2_000_000_000]);
    for (name, set, cap, min) in NUMERIC_FIELDS {
        for (value, expected) in [
            (cap, Ok(())),
            (min, Ok(())),
            (0, Err(Error::Invalid)),
            (min - 1, Err(Error::Invalid)),
            (cap + 1, Err(Error::Invalid)),
            (usize::MAX, Err(Error::Invalid)),
        ] {
            let mut limits = long_limits();
            set(&mut limits, value);
            assert_eq!(limits.validate(), expected, "{name}={value}");
        }
    }
    for (slice, total, expected) in [
        (100_000_000, 100_000_000_000, Ok(())),
        (0, 2_000_000_000, Err(Error::Invalid)),
        (20_000_000, 0, Err(Error::Invalid)),
        (20_000_000, 19_999_999, Err(Error::Invalid)),
        (100_000_001, 100_000_000_000, Err(Error::Invalid)),
        (20_000_000, 100_000_000_001, Err(Error::Invalid)),
        (u64::MAX, u64::MAX, Err(Error::Invalid)),
    ] {
        assert_eq!(
            Limits {
                fuel_slice: slice,
                fuel_total: total,
                ..long_limits()
            }
            .validate(),
            expected
        );
    }
}

struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
#[derive(Default)]
struct Host {
    started: Mutex<Option<oneshot::Sender<()>>>,
    active: Arc<AtomicUsize>,
    cleanups: AtomicUsize,
}
impl HostDispatcher for Host {
    fn dispatch(&self, _request: HostRequest, _context: HostContext) -> HostFuture {
        self.active.fetch_add(1, Ordering::SeqCst);
        let active = Active(self.active.clone());
        let started = self.started.lock().expect("observer").take();
        Box::pin(async move {
            let _active = active;
            if let Some(started) = started {
                started.send(()).expect("start observer");
            }
            std::future::pending::<Result<HostReply, Error>>().await
        })
    }
    fn cleanup(&self, _invocation: InvocationId) -> CleanupFuture {
        assert_eq!(self.active.load(Ordering::SeqCst), 0);
        self.cleanups.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[test]
fn deadline_execution_watchdog() {
    let mut command = std::process::Command::new(std::env::current_exe().expect("exe"));
    command
        .args(["--exact", "deadline_child", "--ignored", "--nocapture"])
        .env("WALTR_SCRIPT_DEADLINE_CHILD", "1");
    let result = watchdog::run(command, Duration::from_secs(30), 16_384);
    assert!(
        result.success && !result.killed && !result.overflow,
        "{}",
        String::from_utf8_lossy(&result.output)
    );
}
#[test]
#[ignore = "disposable watchdog child"]
fn deadline_child() {
    assert!(std::env::var_os("WALTR_SCRIPT_DEADLINE_CHILD").is_some());
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
        .block_on(async {
            assert!(matches!(Engine::new(0), Err(Error::Invalid)));
            assert!(matches!(Engine::new(129), Err(Error::Invalid)));
            let maximum = Engine::new(128).expect("unchanged admission maximum");
            assert_eq!(maximum.info().admission_limit, 128);
            maximum.shutdown().await.expect("maximum shutdown");
            let engine = Engine::new(1).expect("engine");
            long_deadline_execution(&engine).await;
            cpu_deadline_with_owned_host_work(&engine).await;
            short_deadline_and_cancellation(&engine).await;
            engine.shutdown().await.expect("joined shutdown");
        });
}

const TRIVIAL: &str = "export default async function(){return 7}";
fn input(limits: &Limits) -> StructuredValue {
    StructuredValue::new(serde_json::Value::Null, limits).expect("input")
}
async fn execute(
    engine: &Engine,
    source: &str,
    limits: Limits,
    host: Arc<Host>,
) -> ExecutionReport {
    let execution = engine
        .start(source.into(), input(&limits), limits, host)
        .expect("start");
    tokio::time::timeout(Duration::from_secs(10), execution.join())
        .await
        .expect("terminal timeout")
}

async fn long_deadline_execution(engine: &Engine) {
    engine
        .validate(TRIVIAL, &long_limits())
        .expect("90-minute compile validation");
    let host = Arc::new(Host::default());
    let first = execute(engine, TRIVIAL, long_limits(), host.clone()).await;
    assert_eq!(
        first.result.expect("trivial output").as_json(),
        &serde_json::json!(7)
    );
    assert!(first.cleanup_complete);
    let limits = Limits {
        fuel_total: 40_000_000,
        ..long_limits()
    };
    let total = limits.fuel_total;
    let report = execute(
        engine,
        "export default async function(){while(true) await Promise.resolve()}",
        limits,
        host.clone(),
    )
    .await;
    assert_eq!(report.result, Err(Error::Fuel));
    assert!(
        report.fuel <= total && report.fuel > total - 100_000,
        "cumulative fuel ceiling: {}",
        report.fuel
    );
    assert!(report.elapsed < Duration::from_secs(10));
    assert!(report.cleanup_complete);
    let fresh = execute(engine, TRIVIAL, long_limits(), host.clone()).await;
    assert!(fresh.result.is_ok());
    assert!(fresh.cleanup_complete);
    assert_eq!(host.cleanups.load(Ordering::SeqCst), 3);
    assert_eq!(host.active.load(Ordering::SeqCst), 0);
}

async fn cpu_deadline_with_owned_host_work(engine: &Engine) {
    let (started, mut ready) = oneshot::channel();
    let host = Arc::new(Host {
        started: Mutex::new(Some(started)),
        ..Host::default()
    });
    let report = execute(
        engine,
        "export default async function(){host.call('wait',null);while(true) await Promise.resolve()}",
        Limits {
            deadline: Duration::from_secs(1),
            fuel_slice: 100_000_000,
            fuel_total: 100_000_000_000,
            ..Limits::default()
        },
        host.clone(),
    )
    .await;
    assert_eq!(report.result, Err(Error::Deadline));
    assert!(report.cleanup_complete);
    assert!(report.elapsed >= Duration::from_secs(1));
    assert!(report.elapsed < Duration::from_secs(10));
    ready.try_recv().expect("owned host wait was polled");
    assert_eq!(host.active.load(Ordering::SeqCst), 0);
    assert_eq!(host.cleanups.load(Ordering::SeqCst), 1);
    let fresh = execute(engine, TRIVIAL, long_limits(), Arc::new(Host::default())).await;
    assert_eq!(
        fresh.result.expect("joined re-admission").as_json(),
        &serde_json::json!(7)
    );
}

async fn short_deadline_and_cancellation(engine: &Engine) {
    let source = "export default async function(){return await host.call('wait',null)}";
    for cancel in [false, true] {
        let (started, ready) = oneshot::channel();
        let host = Arc::new(Host {
            started: Mutex::new(Some(started)),
            ..Host::default()
        });
        let limits = Limits {
            deadline: Duration::from_secs(if cancel { 2 } else { 1 }),
            ..Limits::default()
        };
        let deadline = limits.deadline;
        let execution = engine
            .start(source.into(), input(&limits), limits, host.clone())
            .expect("start");
        let control = execution.cancellation();
        let mut join = execution.join();
        assert!(futures_util::poll!(join.as_mut()).is_pending());
        ready.await.expect("host wait reached");
        if cancel {
            control.cancel();
        }
        let report = tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("short terminal");
        assert_eq!(
            report.result,
            Err(if cancel {
                Error::Cancelled
            } else {
                Error::Deadline
            })
        );
        assert!(report.cleanup_complete);
        if cancel {
            assert!(report.elapsed < deadline);
        } else {
            assert!(report.elapsed >= deadline);
        }
        assert_eq!(host.active.load(Ordering::SeqCst), 0);
        assert_eq!(host.cleanups.load(Ordering::SeqCst), 1);
        let fresh = execute(engine, TRIVIAL, long_limits(), Arc::new(Host::default())).await;
        assert!(fresh.result.is_ok());
    }
}
