//! Fake-host production conformance, always run in a disposable watched process.
#![allow(clippy::literal_string_with_formatting_args)] // JavaScript object literals.
mod support {
    pub mod watchdog;
}
use serde_json::{Value, json};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};
use waltr_script::{
    CallbackRef, CleanupFuture, Engine, Error, HostContext, HostDispatcher, HostFuture, HostHandle,
    HostReply, HostRequest, InvocationId, Limits, ScriptEngine, StructuredValue,
};

#[derive(Default)]
struct Host {
    calls: AtomicUsize,
    cleanups: AtomicUsize,
    active: Arc<AtomicUsize>,
    saved: Mutex<Vec<HostContext>>,
    saved_callbacks: Mutex<Vec<CallbackRef>>,
    resources: Arc<Mutex<Vec<(HostHandle, Active)>>>,
    live_resources: Arc<AtomicUsize>,
}
struct Active(Arc<AtomicUsize>);
impl Drop for Active {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
fn value(v: Value) -> StructuredValue {
    StructuredValue::new(v, &Limits::default()).expect("value")
}
impl HostDispatcher for Host {
    fn dispatch(&self, request: HostRequest, context: HostContext) -> HostFuture {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.active.fetch_add(1, Ordering::SeqCst);
        self.saved.lock().expect("contexts").push(context.clone());
        if let Some(callback) = &request.callback {
            self.saved_callbacks
                .lock()
                .expect("callbacks")
                .push(callback.clone());
        }
        let active = Active(self.active.clone());
        let resources = self.resources.clone();
        let live_resources = self.live_resources.clone();
        Box::pin(async move {
            let _active = active;
            match request.operation.as_str() {
                "echo" => Ok(HostReply::Value(request.arguments)),
                "bytes" => Ok(HostReply::Bytes(request.bytes.ok_or(Error::Invalid)?)),
                "never" => std::future::pending().await,
                "panic" => panic!("fake host panic"),
                "helper" => {
                    let callback = request.callback.ok_or(Error::Invalid)?;
                    for attempt in 0..3 {
                        let accepted = callback.invoke(value(json!(attempt))).await?;
                        match accepted.as_json().as_bool() {
                            Some(true) => {
                                return Ok(HostReply::Value(value(
                                    json!({"attempts": attempt + 1}),
                                )));
                            }
                            Some(false) => {}
                            None => return Err(Error::Value),
                        }
                    }
                    Err(Error::Denied)
                }
                "open" => {
                    let handle = context.issue_handle("reader")?;
                    live_resources.fetch_add(1, Ordering::SeqCst);
                    resources
                        .lock()
                        .expect("resources")
                        .push((handle.clone(), Active(live_resources)));
                    Ok(HostReply::Handle(handle))
                }
                "read" => {
                    context.validate_handle(
                        request.handle.as_ref().ok_or(Error::Identity)?,
                        "reader",
                    )?;
                    Ok(HostReply::Value(value(json!(42))))
                }
                "close" => {
                    let handle = request.handle.as_ref().ok_or(Error::Identity)?;
                    context.release_handle(handle)?;
                    resources
                        .lock()
                        .expect("resources")
                        .retain(|(resource, _)| resource != handle);
                    Ok(HostReply::Value(value(Value::Null)))
                }
                "wrongkind" => {
                    context.validate_handle(
                        request.handle.as_ref().ok_or(Error::Identity)?,
                        "writer",
                    )?;
                    unreachable!()
                }
                _ => Err(Error::Denied),
            }
        })
    }
    fn cleanup(&self, invocation: InvocationId) -> CleanupFuture {
        assert_eq!(self.active.load(Ordering::SeqCst), 0);
        self.resources
            .lock()
            .expect("resources")
            .retain(|(handle, _)| handle.invocation() != invocation);
        self.cleanups.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

async fn execute(
    engine: &dyn ScriptEngine,
    host: Arc<Host>,
    body: &str,
    limits: Limits,
) -> waltr_script::ExecutionReport {
    let source = format!("export default async function(input) {{ {body} }}");
    engine
        .start(source, value(json!({"input":7})), limits, host)
        .expect("start")
        .join()
        .await
}

async fn basic_module_and_host_contract() {
    let engine = Engine::new(2).expect("engine");
    let host = Arc::new(Host::default());
    let report = execute(
        &engine,
        host.clone(),
        "return await host.call('echo', input)",
        Limits::default(),
    )
    .await;
    assert_eq!(
        report.result.expect("output").as_json(),
        &json!({"input":7})
    );
    assert!(report.cleanup_complete);
    assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.cleanups.load(Ordering::SeqCst), 1);
    engine.shutdown().await.expect("shutdown");
}

#[test]
fn production_contracts_watchdog() {
    let mut command = std::process::Command::new(std::env::current_exe().expect("test exe"));
    command.args(["--exact", "child_suite", "--ignored", "--nocapture"]);
    command.env("WALTR_SCRIPT_CHILD", "suite");
    let result = support::watchdog::run(command, std::time::Duration::from_secs(90), 32_768);
    assert!(
        result.success && !result.killed && !result.overflow,
        "success={} killed={} overflow={}: {}",
        result.success,
        result.killed,
        result.overflow,
        String::from_utf8_lossy(&result.output)
    );
}

#[test]
#[ignore = "disposable watchdog child"]
fn child_suite() {
    assert_eq!(std::env::var("WALTR_SCRIPT_CHILD").as_deref(), Ok("suite"));
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
        .block_on(async {
            basic_module_and_host_contract().await;
            callback_and_resource_contracts().await;
            strict_value_contracts().await;
            array_key_regressions().await;
            limits_and_lifecycle_contracts().await;
            caught_native_quota_contracts().await;
            validation_and_authority_contracts().await;
            additional_ownership_contracts().await;
        });
}

async fn callback_and_resource_contracts() {
    let engine = Engine::new(2).expect("engine");
    for (body, expected, calls) in [
        (
            "return await host.call('helper', null, async n => n === 2)",
            json!({"attempts":3}),
            1,
        ),
        (
            "return await host.call('helper', null, async n => await host.call('echo', n === 1))",
            json!({"attempts":2}),
            3,
        ),
        (
            "const h = await host.call('open', null); return await host.call('read', null, null, null, h)",
            json!(42),
            2,
        ),
        (
            "const b = new Uint8Array(65536); b.fill(7); const r=await host.call('bytes', null, null, b.buffer); return [r.byteLength,new Uint8Array(r)[65535]]",
            json!([65536, 7]),
            1,
        ),
    ] {
        let host = Arc::new(Host::default());
        let report = execute(&engine, host.clone(), body, Limits::default()).await;
        assert_eq!(report.result.expect(body).as_json(), &expected);
        assert_eq!(host.calls.load(Ordering::SeqCst), calls);
        assert_eq!(host.cleanups.load(Ordering::SeqCst), 1);
        assert_eq!(host.live_resources.load(Ordering::SeqCst), 0);
        let callbacks = host.saved_callbacks.lock().expect("callbacks").clone();
        for callback in callbacks {
            assert_eq!(
                callback.invoke(value(Value::Null)).await,
                Err(Error::Identity)
            );
        }
        for context in host.saved.lock().expect("contexts").iter() {
            assert_eq!(context.issue_handle("late"), Err(Error::Identity));
        }
    }
    for body in [
        "return await host.call('helper', null, async () => { throw Error('Denied') })",
        "return await host.call('helper', null, () => true)",
        "const h=await host.call('open',null); await host.call('close',null,null,null,h); return await host.call('read',null,null,null,h)",
        "return await host.call('read',null,null,null,{id:1})",
        "const h=await host.call('open',null); return await host.call('wrongkind',null,null,null,h)",
    ] {
        let host = Arc::new(Host::default());
        let report = execute(&engine, host.clone(), body, Limits::default()).await;
        assert!(report.result.is_err(), "{body}");
        assert!(report.cleanup_complete);
        assert_eq!(host.live_resources.load(Ordering::SeqCst), 0);
    }
    let host = Arc::new(Host::default());
    let first = execute(
        &engine,
        host.clone(),
        "await host.call('echo',null); return null",
        Limits::default(),
    )
    .await;
    assert!(first.result.is_ok());
    let old = host.saved.lock().expect("contexts")[0].clone();
    assert_eq!(old.issue_handle("reader"), Err(Error::Identity));
    engine.shutdown().await.expect("shutdown");
}

async fn strict_value_contracts() {
    let engine = Engine::new(2).expect("engine");
    for expression in [
        "undefined",
        "NaN",
        "Infinity",
        "9007199254740992",
        "1n",
        "Symbol('s')",
        "() => 1",
        "new Date()",
        "new Map()",
        "new Uint8Array(2)",
        "new ArrayBuffer(2)",
        "[1,,3]",
        "Object.assign([1], {extra:2})",
        "{a:undefined}",
        "Object.defineProperty({}, 'x', {get(){ host.call('echo',null); return 1 }, enumerable:true})",
        "{toJSON(){ host.call('echo',null); return 1 }}",
        "{[Symbol('s')]:1}",
        "new Proxy({}, {ownKeys(){host.call('echo',null); return []}})",
        "Object.create({custom:true})",
        "(()=>{const x={}; x.x=x; return x})()",
        "Object.defineProperty({},'x',{value:1})",
    ] {
        for body in [
            format!("return ({expression})"),
            format!("return await host.call('echo', ({expression}))"),
        ] {
            let host = Arc::new(Host::default());
            let report = execute(&engine, host.clone(), &body, Limits::default()).await;
            assert_eq!(report.result, Err(Error::Value), "{body}");
            assert_eq!(
                host.calls.load(Ordering::SeqCst),
                0,
                "hooks must not execute: {body}"
            );
        }
    }
    for (field, body) in [
        ("depth", "let x=null; for(let i=0;i<40;i++) x={x}; return x"),
        ("nodes", "return new Array(5000).fill(1)"),
        ("strings", "return 'x'.repeat(17000)"),
        (
            "output",
            "return Array.from({length:8},()=> 'x'.repeat(16000))",
        ),
    ] {
        let report = execute(&engine, Arc::new(Host::default()), body, Limits::default()).await;
        assert_eq!(report.result, Err(Error::Value), "{field}");
    }
    let body = "Object.prototype.toJSON=()=>{throw Error('hook')}; Array.prototype.toJSON=()=>{throw Error('hook')}; return {a:[1,2],b:Object.assign(Object.create(null),{x:true})}";
    let report = execute(&engine, Arc::new(Host::default()), body, Limits::default()).await;
    assert_eq!(
        report.result.expect("pinned serializer").as_json(),
        &json!({"a":[1,2],"b":{"x":true}})
    );
    for v in [json!(9_007_199_254_740_992_u64), json!("x".repeat(17000))] {
        assert_eq!(
            StructuredValue::new(v, &Limits::default()),
            Err(Error::Value)
        );
    }
    engine.shutdown().await.expect("shutdown");
}

async fn array_key_regressions() {
    let engine = Engine::new(1).expect("engine");
    for key in [
        "+1",
        " 1",
        "1\0extra",
        "01",
        "1e0",
        "1.0",
        "-0",
        "4294967296",
        "1x",
        "length\0extra",
    ] {
        let key = serde_json::to_string(key).expect("key");
        for payload in [
            "3",
            "{valueOf(){globalThis.hooked++;host.call('echo','HOOK');return 1}}",
            "{[Symbol.toPrimitive](){globalThis.hooked++;host.call('echo','HOOK');return 1}}",
        ] {
            let setup =
                format!("globalThis.hooked=0;const a=[1,2];delete a[1];a[{key}]={payload};");
            for (body, calls) in [
                (format!("{setup}return a"), 0),
                (format!("{setup}return await host.call('echo',a)"), 0),
                (
                    format!("return await host.call('helper',null,async()=>{{{setup}return a}})"),
                    1,
                ),
            ] {
                let host = Arc::new(Host::default());
                let report = execute(&engine, host.clone(), &body, Limits::default()).await;
                assert_eq!(report.result, Err(Error::Value), "{body}");
                assert!(report.cleanup_complete);
                assert_eq!(
                    host.calls.load(Ordering::SeqCst),
                    calls,
                    "no hook effects: {body}"
                );
                assert_eq!(host.active.load(Ordering::SeqCst), 0);
            }
        }
    }
    assert!(
        execute(
            &engine,
            Arc::new(Host::default()),
            "return [1,2]",
            Limits::default()
        )
        .await
        .result
        .is_ok()
    );
    engine.shutdown().await.expect("shutdown");
}

#[allow(clippy::too_many_lines)] // Table-driven independent execution cases.
async fn limits_and_lifecycle_contracts() {
    use std::time::Duration;
    let engine = Engine::new(1).expect("engine");
    for (body, limits, expected) in [
        (
            "while(true) {}",
            Limits {
                fuel_total: 20_000_000,
                ..Limits::default()
            },
            Error::Fuel,
        ),
        (
            "function f(n){return n?1+f.call(null,n-1):0}return f(60)",
            Limits {
                stack_bytes: 16 * 1024,
                fuel_slice: 100_000_000,
                ..Limits::default()
            },
            Error::Limit,
        ),
        (
            "return new ArrayBuffer(2097152).byteLength",
            Limits {
                heap_bytes: 1024 * 1024,
                fuel_slice: 100_000_000,
                ..Limits::default()
            },
            Error::Memory,
        ),
        (
            "return 'a'.repeat(10000).match(/(a+)+b/)",
            Limits::default(),
            Error::Fuel,
        ),
        (
            "while(true) await Promise.resolve()",
            Limits {
                fuel_total: 20_000_000,
                ..Limits::default()
            },
            Error::Fuel,
        ),
    ] {
        let host = Arc::new(Host::default());
        let report = execute(&engine, host.clone(), body, limits).await;
        assert_eq!(report.result, Err(expected), "{body}");
        assert!(report.cleanup_complete);
        assert_eq!(host.calls.load(Ordering::SeqCst), 0);
        assert_eq!(host.active.load(Ordering::SeqCst), 0);
        let fresh = execute(
            &engine,
            Arc::new(Host::default()),
            "return 1",
            Limits::default(),
        )
        .await;
        assert!(fresh.result.is_ok());
    }
    for (body, expected) in [
        (
            "function f(n){return n?1+f.call(null,n-1):0}return f(60)",
            60,
        ),
        ("return new ArrayBuffer(2097152).byteLength", 2_097_152),
    ] {
        let report = execute(
            &engine,
            Arc::new(Host::default()),
            body,
            Limits {
                fuel_slice: 100_000_000,
                ..Limits::default()
            },
        )
        .await;
        assert_eq!(
            report.result.expect("positive budget control").as_json(),
            &json!(expected)
        );
    }
    for (body, expected) in [
        ("return await host.call('never',null)", Error::Deadline),
        (
            "await Promise.all(Array.from({length:3},()=>host.call('never',null))); return 1",
            Error::Limit,
        ),
        (
            "for(let i=0;i<3;i++) await host.call('echo',null); return 1",
            Error::Limit,
        ),
    ] {
        let limits = Limits {
            deadline: Duration::from_millis(250),
            in_flight: 2,
            host_calls: 2,
            ..Limits::default()
        };
        let report = execute(&engine, Arc::new(Host::default()), body, limits).await;
        assert_eq!(report.result, Err(expected), "{body}");
    }
    for body in [
        "await host.call('never',null); return 1",
        "return await host.call('helper',null,async()=>await host.call('never',null))",
        "await host.call('open',null); return await host.call('never',null)",
    ] {
        let host = Arc::new(Host::default());
        let execution = engine
            .start(
                format!("export default async function(){{ {body} }}"),
                value(Value::Null),
                Limits::default(),
                host.clone(),
            )
            .expect("start");
        assert!(matches!(
            engine.start(
                "export default async function(){return 1}".into(),
                value(Value::Null),
                Limits::default(),
                host.clone()
            ),
            Err(Error::Busy)
        ));
        let required_calls = if body.contains("helper") || body.contains("open") {
            2
        } else {
            1
        };
        while host.calls.load(Ordering::SeqCst) < required_calls {
            tokio::task::yield_now().await;
        }
        execution.cancel();
        let report = execution.join().await;
        assert_eq!(report.result, Err(Error::Cancelled));
        assert!(report.cleanup_complete);
        assert_eq!(host.live_resources.load(Ordering::SeqCst), 0);
    }
    let host = Arc::new(Host::default());
    let execution = engine
        .start(
            "export default async function(){ await host.call('never',null) }".into(),
            value(Value::Null),
            Limits::default(),
            host.clone(),
        )
        .expect("start");
    drop(execution);
    engine.shutdown().await.expect("drop cleanup and join");
    assert_eq!(host.cleanups.load(Ordering::SeqCst), 1);

    let engine = Engine::new(1).expect("engine");
    let report = execute(
        &engine,
        Arc::new(Host::default()),
        "return await host.call('panic',null)",
        Limits::default(),
    )
    .await;
    assert_eq!(report.result, Err(Error::Panic));
    assert!(report.cleanup_complete);
    let report = execute(
        &engine,
        Arc::new(Host::default()),
        "host.call('never',null); return 3",
        Limits::default(),
    )
    .await;
    assert_eq!(
        report.result.expect("unawaited cancelled").as_json(),
        &json!(3)
    );
    engine.shutdown().await.expect("shutdown");
}

async fn caught_native_quota_contracts() {
    let engine = Engine::new(1).expect("engine");
    for (probe, limits, expected) in [
        (
            "function f(n){return n?1+f.call(null,n-1):0}f(60)",
            Limits {
                stack_bytes: 16 * 1024,
                fuel_slice: 100_000_000,
                ..Limits::default()
            },
            Error::Limit,
        ),
        (
            "new ArrayBuffer(2097152)",
            Limits {
                heap_bytes: 1024 * 1024,
                fuel_slice: 100_000_000,
                ..Limits::default()
            },
            Error::Memory,
        ),
    ] {
        let body = format!(
            "await host.call('open',null);host.call('never',null);await host.call('echo',null);let caught=false;try{{{probe}}}catch{{caught=true}}return caught"
        );
        let host = Arc::new(Host::default());
        let report = execute(&engine, host.clone(), &body, limits).await;
        assert_eq!(report.result, Err(expected), "caught native quota: {probe}");
        assert!(report.cleanup_complete);
        assert_eq!(host.calls.load(Ordering::SeqCst), 3);
        assert_eq!(host.active.load(Ordering::SeqCst), 0);
        assert_eq!(host.live_resources.load(Ordering::SeqCst), 0);
        assert_eq!(host.cleanups.load(Ordering::SeqCst), 1);
        for context in host.saved.lock().expect("contexts").iter() {
            assert_eq!(context.issue_handle("late"), Err(Error::Identity));
        }
        // Re-admission requires the terminal worker to have been joined.
        let control = execute(
            &engine,
            Arc::new(Host::default()),
            &body,
            Limits {
                fuel_slice: 100_000_000,
                ..Limits::default()
            },
        )
        .await;
        assert_eq!(
            control.result.expect("positive control").as_json(),
            &json!(false)
        );
        assert!(control.cleanup_complete);
    }
    engine.shutdown().await.expect("joined shutdown");
}

async fn validation_and_authority_contracts() {
    let engine = Engine::new(2).expect("engine");
    assert_eq!(engine.info().api_version, 1);
    assert_eq!(
        engine.validate("export default async function(){", &Limits::default()),
        Err(Error::Syntax)
    );
    assert!(engine.validate("throw Error('top-level must not run'); export default async function(){ return 1 }", &Limits::default()).is_ok());
    assert!(engine.validate("Object.defineProperty(globalThis,'danger',{get(){throw 1}}); host.call('echo',null); export default async function(){}", &Limits::default()).is_ok());
    assert!(
        engine
            .validate(
                "import x from 'std'; export default async function(){return x}",
                &Limits::default()
            )
            .is_err()
    );
    for (body, expected) in [
        ("throw Error('Denied')", Error::Script),
        ("throw Error('out of memory')", Error::Script),
        (
            "throw RangeError('Maximum call stack size exceeded')",
            Error::Script,
        ),
        ("throw {category:'Denied'}", Error::Script),
        ("return await host.call('deny',null)", Error::Denied),
        (
            "return await host.call('helper',null,async()=>await host.call('deny',null))",
            Error::Denied,
        ),
        (
            "return await host.call('helper',null,async()=>42)",
            Error::Value,
        ),
        (
            "return await host.call('helper',null,async()=>false)",
            Error::Denied,
        ),
        ("return await import('std')", Error::Script),
        ("return await import('file:///etc/passwd')", Error::Script),
        (
            "return await host.call('filesystem.read', {path:'/etc/passwd'})",
            Error::Denied,
        ),
    ] {
        let report = execute(&engine, Arc::new(Host::default()), body, Limits::default()).await;
        assert_eq!(report.result, Err(expected), "{body}");
    }
    for body in [
        "return [typeof fetch,typeof require,typeof process,typeof std,typeof os,typeof WebSocket]",
        "try { host = {} } catch {} try { host.call = () => 0 } catch {} return [typeof host.call,Object.isFrozen(host),Object.isFrozen(host.call)]",
        "globalThis.secret=7; return 1",
        "return typeof secret",
    ] {
        let report = execute(&engine, Arc::new(Host::default()), body, Limits::default()).await;
        let expected = if body.starts_with("return [typeof fetch") {
            json!([
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined",
                "undefined"
            ])
        } else if body.starts_with("try") {
            json!(["function", true, true])
        } else if body.starts_with("global") {
            json!(1)
        } else {
            json!("undefined")
        };
        assert_eq!(report.result.expect(body).as_json(), &expected);
    }
    transport_and_fairness_contracts(&engine).await;
    engine.shutdown().await.expect("shutdown");
}

async fn transport_and_fairness_contracts(engine: &dyn ScriptEngine) {
    let host = Arc::new(Host::default());
    let body = "host.call('never',null); let done=false; host.call('echo',1).then(()=>done=true); let n=0; while(!done){if(++n>100000) throw Error('starved'); await Promise.resolve()} return n>0";
    let report = execute(engine, host, body, Limits::default()).await;
    assert_eq!(
        report
            .result
            .expect("microtasks must not starve completion")
            .as_json(),
        &json!(true)
    );
    for body in [
        "Object.defineProperty(Object.prototype,'then',{get(){host.call('echo','hook');return undefined}}); return await host.call('echo',{ok:1})",
        "Object.defineProperty(ArrayBuffer.prototype,'then',{get(){host.call('echo','hook');return undefined}}); const r=await host.call('bytes',null,null,new Uint8Array([3]).buffer); return [r.byteLength,new Uint8Array(r)[0]]",
    ] {
        let host = Arc::new(Host::default());
        let report = execute(engine, host.clone(), body, Limits::default()).await;
        assert!(report.result.is_ok(), "{body}: {report:?}");
        assert_eq!(
            host.calls.load(Ordering::SeqCst),
            1,
            "transport must not invoke prototype then hooks"
        );
    }
}

#[test]
fn watchdog_kills_reaps_and_caps_output() {
    use std::{process::Command, time::Duration};
    let mut command = Command::new(std::env::current_exe().expect("exe"));
    command
        .args(["--exact", "watchdog_fixture", "--ignored", "--nocapture"])
        .env("WALTR_SCRIPT_CHILD", "hang");
    let result = support::watchdog::run(command, Duration::from_millis(100), 4096);
    assert!(result.killed && !result.success);
    let mut command = Command::new(std::env::current_exe().expect("exe"));
    command
        .args(["--exact", "watchdog_fixture", "--ignored", "--nocapture"])
        .env("WALTR_SCRIPT_CHILD", "flood");
    let result = support::watchdog::run(command, Duration::from_secs(5), 4096);
    assert!(result.success && !result.killed && result.overflow);
    assert!(result.output.len() <= 4096);
    let mut command = Command::new(std::env::current_exe().expect("exe"));
    command
        .args([
            "--exact",
            "blocked_host_fixture",
            "--ignored",
            "--nocapture",
        ])
        .env("WALTR_SCRIPT_CHILD", "blocked");
    let result = support::watchdog::run(command, Duration::from_secs(3), 4096);
    assert!(result.killed && !result.success);
    assert!(
        String::from_utf8_lossy(&result.output).contains("NO_TERMINAL_PROOF"),
        "{result:?}"
    );
}

#[test]
#[ignore = "unsupported synchronous host work requires a disposable child"]
fn blocked_host_fixture() {
    use std::sync::atomic::AtomicBool;
    struct BlockedHost(Arc<AtomicBool>);
    impl HostDispatcher for BlockedHost {
        fn dispatch(&self, _request: HostRequest, _context: HostContext) -> HostFuture {
            self.0.store(true, Ordering::Release);
            loop {
                std::thread::park();
            }
        }
        fn cleanup(&self, _invocation: InvocationId) -> CleanupFuture {
            Box::pin(async { Ok(()) })
        }
    }
    assert_eq!(
        std::env::var("WALTR_SCRIPT_CHILD").as_deref(),
        Ok("blocked")
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("runtime")
        .block_on(async {
            let entered = Arc::new(AtomicBool::new(false));
            let engine = Engine::new(1).expect("engine");
            let execution = engine
                .start(
                    "export default async function(){return await host.call('blocked',null)}"
                        .into(),
                    value(Value::Null),
                    Limits::default(),
                    Arc::new(BlockedHost(entered.clone())),
                )
                .expect("start");
            while !entered.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
            execution.cancel();
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(100), execution.join())
                    .await
                    .is_err()
            );
            assert!(matches!(
                engine.start(
                    "export default async function(){return 1}".into(),
                    value(Value::Null),
                    Limits::default(),
                    Arc::new(Host::default())
                ),
                Err(Error::Busy)
            ));
            eprintln!("NO_TERMINAL_PROOF");
            std::future::pending::<()>().await;
        });
}

#[test]
#[ignore = "watchdog control fixture"]
fn watchdog_fixture() {
    match std::env::var("WALTR_SCRIPT_CHILD").as_deref() {
        Ok("hang") => loop {
            std::thread::park();
        },
        Ok("flood") => {
            for _ in 0..10000 {
                println!("stdout payload");
                eprintln!("stderr payload");
            }
        }
        _ => panic!("fixture requires parent"),
    }
}

#[allow(clippy::too_many_lines, clippy::items_after_statements)] // Cleanup-failure fixture is local to its case.
async fn additional_ownership_contracts() {
    use std::time::Duration;
    let engine = Engine::new(1).expect("engine");
    for (limits, body, expected) in [
        (
            Limits {
                callbacks: 1,
                ..Limits::default()
            },
            "return await host.call('helper',null,async n=>n===2)",
            Error::Limit,
        ),
        (
            Limits {
                handles: 1,
                ..Limits::default()
            },
            "await host.call('open',null); return await host.call('open',null)",
            Error::Limit,
        ),
        (
            Limits {
                binary_bytes: 1024,
                ..Limits::default()
            },
            "return await host.call('bytes',null,null,new ArrayBuffer(2048))",
            Error::Value,
        ),
        (
            Limits {
                fuel_total: 20_000_000,
                ..Limits::default()
            },
            "while(true) await Promise.resolve()",
            Error::Fuel,
        ),
    ] {
        let report = execute(&engine, Arc::new(Host::default()), body, limits).await;
        assert_eq!(report.result, Err(expected), "{body}");
        assert!(report.cleanup_complete);
    }
    assert_eq!(
        engine.validate(&" ".repeat(65_537), &Limits::default()),
        Err(Error::Limit)
    );
    assert_eq!(
        engine.validate(
            "export default async function(){}",
            &Limits {
                host_calls: 0,
                ..Limits::default()
            }
        ),
        Err(Error::Invalid)
    );

    // Dropping an unpolled join future must still cancel the worker.
    let host = Arc::new(Host::default());
    let execution = engine
        .start(
            "export default async function(){ await host.call('never',null) }".into(),
            value(Value::Null),
            Limits::default(),
            host.clone(),
        )
        .expect("start");
    drop(execution.join());
    let (one, two) = tokio::join!(engine.shutdown(), engine.shutdown());
    one.expect("first shutdown");
    two.expect("concurrent shutdown");
    assert_eq!(host.cleanups.load(Ordering::SeqCst), 1);

    #[derive(Debug)]
    struct CleanupFailure;
    impl HostDispatcher for CleanupFailure {
        fn dispatch(&self, _request: HostRequest, _context: HostContext) -> HostFuture {
            Box::pin(async { Err(Error::Denied) })
        }
        fn cleanup(&self, _invocation: InvocationId) -> CleanupFuture {
            Box::pin(std::future::pending())
        }
    }
    let engine = Engine::new(1).expect("engine");
    let report = engine
        .start(
            "export default async function(){return 1}".into(),
            value(Value::Null),
            Limits {
                deadline: Duration::from_millis(200),
                ..Limits::default()
            },
            Arc::new(CleanupFailure),
        )
        .expect("start")
        .join()
        .await;
    assert_eq!(report.result, Err(Error::Cleanup));
    assert!(!report.cleanup_complete);
    assert!(matches!(
        engine.start(
            "export default async function(){return 1}".into(),
            value(Value::Null),
            Limits::default(),
            Arc::new(Host::default())
        ),
        Err(Error::Busy)
    ));
    assert_eq!(engine.shutdown().await, Err(Error::Cleanup));

    let engine = Engine::new(2).expect("engine");
    let first = engine
        .start(
            "export default async function(){return await host.call('never',null)}".into(),
            value(Value::Null),
            Limits::default(),
            Arc::new(Host::default()),
        )
        .expect("first");
    let second = engine
        .start(
            "export default async function(){return 2}".into(),
            value(Value::Null),
            Limits::default(),
            Arc::new(Host::default()),
        )
        .expect("second");
    let report = tokio::time::timeout(Duration::from_secs(2), second.join())
        .await
        .expect("unrelated worker must not delay terminal join");
    assert_eq!(report.result.expect("second output").as_json(), &json!(2));
    first.cancel();
    first.join().await;
    engine.shutdown().await.expect("shutdown");

    struct CleanupBarrier {
        entered: Arc<std::sync::atomic::AtomicBool>,
        receive: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }
    impl HostDispatcher for CleanupBarrier {
        fn dispatch(&self, _request: HostRequest, _context: HostContext) -> HostFuture {
            Box::pin(async { Err(Error::Denied) })
        }
        fn cleanup(&self, _invocation: InvocationId) -> CleanupFuture {
            let receive = self
                .receive
                .lock()
                .expect("cleanup receive")
                .take()
                .expect("once");
            self.entered.store(true, Ordering::Release);
            Box::pin(async { receive.await.map_err(|_| Error::Cleanup) })
        }
    }
    let engine = Engine::new(1).expect("engine");
    let (release, receive) = tokio::sync::oneshot::channel();
    let entered = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let execution = engine
        .start(
            "export default async function(){return 1}".into(),
            value(Value::Null),
            Limits::default(),
            Arc::new(CleanupBarrier {
                entered: entered.clone(),
                receive: Mutex::new(Some(receive)),
            }),
        )
        .expect("start");
    while !entered.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        engine.start(
            "export default async function(){return 1}".into(),
            value(Value::Null),
            Limits::default(),
            Arc::new(Host::default())
        ),
        Err(Error::Busy)
    ));
    release.send(()).expect("release");
    assert!(execution.join().await.result.is_ok());
    assert!(
        execute(
            &engine,
            Arc::new(Host::default()),
            "return 2",
            Limits::default()
        )
        .await
        .result
        .is_ok()
    );
    engine.shutdown().await.expect("shutdown");
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut text, byte| {
            write!(text, "{byte:02x}").expect("hex");
            text
        })
}

#[test]
fn bundled_artifact_checksum_matches_manifest() {
    let hash = sha256_hex(include_bytes!("../artifacts/quickjs.wasm"));
    assert_eq!(
        include_str!("../artifacts/SHA256SUMS")
            .split_whitespace()
            .next(),
        Some(hash.as_str())
    );
    let provenance = include_str!("../PROVENANCE.md");
    for bytes in [
        &include_bytes!("../artifacts/quickjs.wasm")[..],
        &include_bytes!("../wasm/reactor.c")[..],
        &include_bytes!("../wasm/budgets.h")[..],
        &include_bytes!("../wasm/quickjs-budget.patch")[..],
        &include_bytes!("../scripts/rebuild.sh")[..],
    ] {
        let hash = sha256_hex(bytes);
        assert!(
            provenance.contains(&hash),
            "provenance omits current input or output digest: {hash}"
        );
    }
}

#[test]
fn packaged_unicode_license_is_verbatim() {
    let notice = include_str!("../THIRD-PARTY-NOTICES.txt");
    let start = notice.find("UNICODE LICENSE V3\n").expect("Unicode notice");
    let hash = sha256_hex(notice[start..].trim_end().as_bytes());
    // Exact notice text from the checksum-pinned upstream amalgam.
    assert_eq!(
        hash,
        "8fa201617d2c10c64205a88903dfac046417e6b95748e36b492026fbc38c5b94"
    );
}
