use crate::{Error, HostContext, HostReply, Limits, StructuredValue};
use std::{sync::OnceLock, time::Instant};
use tokio_util::sync::CancellationToken;
use wasmi::{
    Caller, Config, Engine, Instance, Linker, Memory, Module, Store, StoreLimits,
    StoreLimitsBuilder, WasmParams, WasmResults,
};

struct State {
    limits: StoreLimits,
}
pub(super) struct Vm {
    store: Store<State>,
    instance: Instance,
    memory: Memory,
    limits: Limits,
    cancel: CancellationToken,
    start: Instant,
    spent: u64,
    available: u64,
    #[cfg(test)]
    last_trap: Option<wasmi::core::TrapCode>,
}

#[allow(clippy::needless_pass_by_value)] // Used directly by Result::map_err.
fn wasm_error(error: wasmi::Error) -> Error {
    if error.as_trap_code() == Some(wasmi::core::TrapCode::OutOfFuel) {
        Error::Fuel
    } else {
        Error::Protocol
    }
}

impl Vm {
    #[allow(clippy::too_many_lines)] // Audit the complete import allowlist and initialization together.
    pub fn new(limits: Limits, cancel: CancellationToken, start: Instant) -> Result<Self, Error> {
        static MODULE: OnceLock<Result<(Engine, Module), Error>> = OnceLock::new();
        let (engine, module) = MODULE
            .get_or_init(|| {
                let mut config = Config::default();
                config.consume_fuel(true);
                let engine = Engine::new(&config);
                let module =
                    Module::new(&engine, &include_bytes!("../../artifacts/quickjs.wasm")[..])
                        .map_err(wasm_error)?;
                Ok((engine, module))
            })
            .as_ref()
            .map_err(|e| *e)?;
        let mut store = Store::new(
            engine,
            State {
                limits: StoreLimitsBuilder::new()
                    .memory_size(limits.memory_bytes)
                    .memories(1)
                    .tables(1)
                    .build(),
            },
        );
        store.limiter(|s| &mut s.limits);
        store.set_fuel(limits.fuel_slice).map_err(wasm_error)?;
        let mut linker = Linker::new(engine);
        for import in module.imports() {
            if import.module() != "wasi_snapshot_preview1"
                || !matches!(
                    import.name(),
                    "clock_time_get" | "fd_close" | "fd_fdstat_get" | "fd_seek" | "fd_write"
                )
            {
                return Err(Error::Protocol);
            }
            let ty = import.ty().func().ok_or(Error::Protocol)?.clone();
            let clock = import.name() == "clock_time_get";
            linker
                .func_new(
                    import.module(),
                    import.name(),
                    ty,
                    move |mut caller: Caller<'_, State>, params, results| {
                        if clock {
                            let memory = caller
                                .get_export("memory")
                                .and_then(wasmi::Extern::into_memory)
                                .ok_or_else(|| wasmi::Error::new("memory"))?;
                            let ptr = params
                                .get(2)
                                .and_then(wasmi::Val::i32)
                                .ok_or_else(|| wasmi::Error::new("signature"))?;
                            memory
                                .write(&mut caller, ptr.cast_unsigned() as usize, &[0; 8])
                                .map_err(|_| wasmi::Error::new("bounds"))?;
                        }
                        if let Some(result) = results.first_mut() {
                            *result = wasmi::Val::I32(if clock { 0 } else { 76 });
                        }
                        Ok(())
                    },
                )
                .map_err(|_| Error::Protocol)?;
        }
        let instance = linker
            .instantiate(&mut store, module)
            .map_err(wasm_error)?
            .start(&mut store)
            .map_err(wasm_error)?;
        let memory = instance
            .get_memory(&store, "memory")
            .ok_or(Error::Protocol)?;
        let available = limits.fuel_slice;
        let mut vm = Self {
            store,
            instance,
            memory,
            limits,
            cancel,
            start,
            spent: 0,
            available,
            #[cfg(test)]
            last_trap: None,
        };
        vm.call::<(), ()>("_initialize", ())?;
        let l = &vm.limits;
        let config = (
            integer(l.heap_bytes)?,
            integer(l.stack_bytes)?,
            integer(l.value_bytes)?,
            integer(l.value_depth)?,
            integer(l.value_nodes)?,
            integer(l.string_bytes)?,
            integer(l.binary_bytes)?,
            integer(l.host_calls)?,
            integer(l.in_flight)?,
        );
        if vm.call::<_, i32>("init", config)? != 0 {
            return Err(Error::Memory);
        }
        vm.failure()?;
        Ok(vm)
    }
    pub fn check(&self) -> Result<(), Error> {
        if self.cancel.is_cancelled() {
            Err(Error::Cancelled)
        } else if self.start.elapsed() >= self.limits.deadline {
            Err(Error::Deadline)
        } else {
            Ok(())
        }
    }
    fn budget(&mut self) -> Result<(), Error> {
        self.check()?;
        let left = self.store.get_fuel().map_err(wasm_error)?;
        self.spent += self.available - left;
        if self.spent >= self.limits.fuel_total {
            return Err(Error::Fuel);
        }
        self.available = self
            .limits
            .fuel_slice
            .min(self.limits.fuel_total - self.spent);
        self.store.set_fuel(self.available).map_err(wasm_error)
    }
    fn call<P: WasmParams, R: WasmResults>(&mut self, name: &str, params: P) -> Result<R, Error> {
        self.budget()?;
        self.instance
            .get_typed_func::<P, R>(&self.store, name)
            .map_err(wasm_error)?
            .call(&mut self.store, params)
            .map_err(|error| {
                #[cfg(test)]
                {
                    self.last_trap = error.as_trap_code();
                }
                wasm_error(error)
            })
    }
    pub fn fuel(&self) -> u64 {
        self.spent + self.available - self.store.get_fuel().unwrap_or(0)
    }
    pub fn zero(&mut self, name: &str) -> Result<i32, Error> {
        self.call(name, ())
    }
    pub fn one(&mut self, name: &str, id: u32) -> Result<i32, Error> {
        self.call(name, i32::try_from(id).map_err(|_| Error::Protocol)?)
    }
    pub fn failure(&mut self) -> Result<(), Error> {
        match self.zero("failure")? {
            0 => Ok(()),
            1 => Err(Error::Value),
            2 => Err(Error::Limit),
            3 => Err(Error::Memory),
            _ => Err(Error::Protocol),
        }
    }
    pub fn script_error(&mut self) -> Result<Error, Error> {
        if let Err(error) = self.failure() {
            return Ok(error);
        }
        Ok(Self::rejection(self.zero("rejection_code")?))
    }
    pub fn callback_error(&mut self, id: u32) -> Result<Error, Error> {
        Ok(Self::rejection(self.one("callback_error", id)?))
    }
    const fn rejection(code: i32) -> Error {
        match code {
            1 => Error::Denied,
            2 => Error::Limit,
            3 => Error::Identity,
            4 => Error::Cancelled,
            5 => Error::Deadline,
            6 => Error::Value,
            7 => Error::Invalid,
            8 => Error::Memory,
            9 => Error::Fuel,
            10 => Error::Cleanup,
            11 => Error::Shutdown,
            12 => Error::Panic,
            13 => Error::Protocol,
            14 => Error::Syntax,
            15 => Error::Busy,
            _ => Error::Script,
        }
    }
    fn input(&mut self, bytes: &[u8]) -> Result<i32, Error> {
        if bytes.len() > 65_536 {
            return Err(Error::Limit);
        }
        let ptr: i32 = self.call("allocate", integer(bytes.len() + 1)?)?;
        if ptr <= 0 {
            return Err(Error::Memory);
        }
        let offset = usize::try_from(ptr).map_err(|_| Error::Protocol)?;
        self.memory
            .write(&mut self.store, offset, bytes)
            .map_err(|_| Error::Protocol)?;
        self.memory
            .write(&mut self.store, offset + bytes.len(), &[0])
            .map_err(|_| Error::Protocol)?;
        Ok(ptr)
    }
    fn data_call(&mut self, name: &str, bytes: &[u8]) -> Result<i32, Error> {
        let ptr = self.input(bytes)?;
        let result = self.call(name, (ptr, integer(bytes.len())?));
        let _freed = self.call::<_, ()>("deallocate", ptr);
        result
    }
    pub fn compile(&mut self, source: &str) -> Result<(), Error> {
        if source.len() > self.limits.source_bytes {
            return Err(Error::Limit);
        }
        let status = self.data_call("compile", source.as_bytes())?;
        self.failure()?;
        if status != 0 {
            return Err(Error::Syntax);
        }
        Ok(())
    }
    pub fn start(&mut self, value: &StructuredValue) -> Result<(), Error> {
        let bytes = value.encode(&self.limits)?;
        let status = self.data_call("start", &bytes)?;
        self.failure()?;
        if status != 0 {
            return Err(Error::Script);
        }
        Ok(())
    }
    fn copy(&self, ptr: i32, length: i32, cap: usize) -> Result<Vec<u8>, Error> {
        let length = usize::try_from(length).map_err(|_| Error::Protocol)?;
        if ptr < 0 || length > cap {
            return Err(Error::Protocol);
        }
        let mut bytes = vec![0; length];
        self.memory
            .read(
                &self.store,
                usize::try_from(ptr).map_err(|_| Error::Protocol)?,
                &mut bytes,
            )
            .map_err(|_| Error::Protocol)?;
        Ok(bytes)
    }
    pub fn output(&mut self) -> Result<StructuredValue, Error> {
        let ptr = self.zero("output_ptr")?;
        let len = self.zero("output_len")?;
        StructuredValue::decode(&self.copy(ptr, len, self.limits.value_bytes)?, &self.limits)
    }
    pub fn operation(&mut self, id: u32) -> Result<String, Error> {
        let ptr = self.one("event_op", id)?;
        let bytes = self.copy(ptr, 129, 129)?;
        let length = bytes.iter().position(|b| *b == 0).ok_or(Error::Protocol)?;
        if length == 0 {
            return Err(Error::Protocol);
        }
        let operation = std::str::from_utf8(&bytes[..length]).map_err(|_| Error::Protocol)?;
        if !operation
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        {
            return Err(Error::Protocol);
        }
        Ok(operation.into())
    }
    pub fn arguments(&mut self, id: u32) -> Result<StructuredValue, Error> {
        let ptr = self.one("event_args", id)?;
        let len = self.one("event_args_len", id)?;
        StructuredValue::decode(&self.copy(ptr, len, self.limits.value_bytes)?, &self.limits)
    }
    pub fn bytes(&mut self, id: u32) -> Result<Option<Vec<u8>>, Error> {
        let len = self.one("event_bytes_len", id)?;
        if len == -1 {
            return Ok(None);
        }
        let ptr = self.one("event_bytes", id)?;
        Ok(Some(self.copy(ptr, len, self.limits.binary_bytes)?))
    }
    pub fn settle(
        &mut self,
        id: u32,
        reply: Result<HostReply, Error>,
        context: &HostContext,
    ) -> Result<(), Error> {
        let (kind, bytes, handle) = match reply {
            Err(error) => (
                0,
                Vec::new(),
                match error {
                    Error::Denied => 1,
                    Error::Limit => 2,
                    Error::Identity => 3,
                    Error::Cancelled => 4,
                    Error::Deadline => 5,
                    Error::Value => 6,
                    Error::Invalid => 7,
                    Error::Memory => 8,
                    Error::Fuel => 9,
                    Error::Cleanup => 10,
                    Error::Shutdown => 11,
                    Error::Panic => 12,
                    Error::Protocol => 13,
                    Error::Syntax => 14,
                    Error::Busy => 15,
                    Error::Script => 0,
                },
            ),
            Ok(HostReply::Value(value)) => (1, value.encode(&self.limits)?, 0),
            Ok(HostReply::Bytes(bytes)) => {
                if bytes.len() > self.limits.binary_bytes {
                    return Err(Error::Limit);
                }
                (2, bytes, 0)
            }
            Ok(HostReply::Handle(handle)) => {
                context.validate_handle(&handle, handle.kind())?;
                (
                    3,
                    Vec::new(),
                    i32::try_from(handle.generation).map_err(|_| Error::Limit)?,
                )
            }
        };
        let ptr = self.input(&bytes)?;
        let status: i32 = self.call(
            "settle",
            (
                integer(id as usize)?,
                kind,
                ptr,
                integer(bytes.len())?,
                handle,
            ),
        )?;
        self.call::<_, ()>("deallocate", ptr)?;
        if status != 0 {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    pub fn callback(&mut self, id: u32, value: &StructuredValue) -> Result<bool, Error> {
        if self.one("event_callback", id)? != 1 {
            return Ok(false);
        }
        let bytes = value.encode(&self.limits)?;
        let ptr = self.input(&bytes)?;
        let status: i32 = self.call(
            "callback",
            (integer(id as usize)?, ptr, integer(bytes.len())?),
        )?;
        self.call::<_, ()>("deallocate", ptr)?;
        if status != 0 {
            return Err(Error::Script);
        }
        Ok(true)
    }
    pub fn cleanup(&mut self) -> Result<(), Error> {
        // Teardown has a separate finite fuel allowance and cannot run user jobs.
        self.store.set_fuel(100_000_000).map_err(wasm_error)?;
        self.instance
            .get_typed_func::<(), i32>(&self.store, "cleanup")
            .map_err(wasm_error)?
            .call(&mut self.store, ())
            .map_err(wasm_error)?;
        Ok(())
    }
}
fn integer(value: usize) -> Result<i32, Error> {
    i32::try_from(value).map_err(|_| Error::Limit)
}

#[cfg(test)]
mod tests {
    use super::*;
    mod watchdog {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/watchdog.rs"
        ));
    }
    #[test]
    fn linear_memory_growth_is_bounded() {
        if std::env::var_os("WALTR_SCRIPT_MEMORY_CHILD").is_none() {
            let mut command =
                std::process::Command::new(std::env::current_exe().expect("test exe"));
            command
                .args([
                    "--exact",
                    "backend::vm::tests::linear_memory_growth_is_bounded",
                    "--nocapture",
                ])
                .env("WALTR_SCRIPT_MEMORY_CHILD", "1");
            let result = watchdog::run(command, std::time::Duration::from_secs(10), 4096);
            assert!(
                result.success && !result.killed && !result.overflow,
                "{}",
                String::from_utf8_lossy(&result.output)
            );
            return;
        }
        let mut vm =
            Vm::new(Limits::default(), CancellationToken::new(), Instant::now()).expect("vm");
        let before = vm.memory.data_size(&vm.store);
        assert!(vm.memory.grow(&mut vm.store, 513).is_err());
        assert_eq!(before, vm.memory.data_size(&vm.store));
        vm.cleanup().expect("cleanup");
    }

    fn watched(name: &str) -> bool {
        if std::env::var("WALTR_SCRIPT_PRIVATE_CHILD").as_deref() == Ok(name) {
            return false;
        }
        let mut command = std::process::Command::new(std::env::current_exe().expect("exe"));
        command
            .args(["--exact", name, "--nocapture"])
            .env("WALTR_SCRIPT_PRIVATE_CHILD", name);
        let result = watchdog::run(command, std::time::Duration::from_mins(1), 16_384);
        assert!(
            result.success && !result.killed && !result.overflow,
            "{}",
            String::from_utf8_lossy(&result.output)
        );
        true
    }
    fn vm(limits: Limits) -> Vm {
        Vm::new(limits, CancellationToken::new(), Instant::now()).expect("vm")
    }
    fn generous() -> Limits {
        Limits {
            fuel_slice: 100_000_000,
            fuel_total: 10_000_000_000,
            deadline: std::time::Duration::from_mins(1),
            ..Limits::default()
        }
    }
    fn evaluate(vm: &mut Vm, source: &str) -> Result<StructuredValue, Error> {
        vm.compile(source)?;
        vm.start(&StructuredValue::new(serde_json::Value::Null, &vm.limits)?)?;
        loop {
            for _ in 0..8 {
                if vm.zero("step")? <= 0 {
                    break;
                }
            }
            vm.failure()?;
            assert_eq!(
                vm.zero("event")?,
                0,
                "budget probes must not dispatch effects"
            );
            match vm.zero("status")? {
                1 => {
                    if vm.zero("result")? != 0 {
                        return Err(vm.script_error()?);
                    }
                    return vm.output();
                }
                n if n < 0 => return Err(vm.script_error()?),
                _ => {}
            }
        }
    }
    #[test]
    fn native_budget_evidence() {
        if watched("backend::vm::tests::native_budget_evidence") {
            return;
        }
        runtime_budget_evidence();
        parser_budget_evidence();
    }
    fn runtime_budget_evidence() {
        for (label, source, low, expected, flag, output) in [
            (
                "heap",
                "export default async function(){return new ArrayBuffer(2097152).byteLength}",
                Limits {
                    heap_bytes: 1024 * 1024,
                    ..generous()
                },
                Error::Memory,
                1,
                2_097_152,
            ),
            (
                "stack",
                "export default async function(){function f(n){return n?1+f.call(null,n-1):0}return f(60)}",
                Limits {
                    stack_bytes: 16 * 1024,
                    ..generous()
                },
                Error::Limit,
                2,
                60,
            ),
        ] {
            let mut low = vm(low);
            let result = evaluate(&mut low, source);
            let events = low.zero("budget_events").expect("events");
            eprintln!(
                "{label} result={result:?} events={events} fuel={}",
                low.fuel()
            );
            assert_eq!(result, Err(expected), "{label}");
            assert_eq!(
                events,
                if flag == 1 { 1 | 4 } else { 2 },
                "must observe the actual native limiter"
            );
            let mut control = vm(generous());
            assert_eq!(
                evaluate(&mut control, source)
                    .expect("positive control")
                    .as_json(),
                &serde_json::json!(output),
                "control: {label}"
            );
            assert_eq!(control.zero("budget_events").expect("events"), 0);
        }
    }
    fn parser_budget_evidence() {
        let nested = format!(
            "host.call('must-not-run',null);export default async function(){{{}return 0;{}return 1;}}",
            "function f(){".repeat(80),
            "}".repeat(80)
        );
        let mut low = vm(Limits {
            stack_bytes: 16 * 1024,
            ..generous()
        });
        let result = low.compile(&nested);
        eprintln!(
            "compile stack={result:?} events={}",
            low.zero("budget_events").expect("events")
        );
        assert_eq!(result, Err(Error::Limit));
        assert_eq!(low.zero("budget_events").expect("events"), 2);
        assert_eq!(low.zero("event").expect("no compile effects"), 0);
        let mut control = vm(generous());
        control.compile(&nested).expect("compile control");
        assert_eq!(control.zero("budget_events").expect("events"), 0);
        assert_eq!(control.zero("event").expect("no compile effects"), 0);
        let large = format!(
            "host.call('must-not-run',null);export default async function(){{{}return 0}}",
            "function f(){}".repeat(4500)
        );
        let mut low = vm(Limits {
            heap_bytes: 1024 * 1024,
            ..generous()
        });
        let result = low.compile(&large);
        eprintln!(
            "compile heap={result:?} events={}",
            low.zero("budget_events").expect("events")
        );
        assert_eq!(result, Err(Error::Memory));
        assert_eq!(low.zero("budget_events").expect("events"), 1 | 4);
        assert_eq!(low.zero("event").expect("no compile effects"), 0);
        let mut control = vm(generous());
        control.compile(&large).expect("compile heap control");
        assert_eq!(control.zero("budget_events").expect("events"), 0);
        assert_eq!(control.zero("event").expect("no compile effects"), 0);
        let source = format!(
            "export default async function(){{return [{}0]}}",
            "0,".repeat(1000)
        );
        let mut limited = vm(generous());
        let pointer = limited.input(source.as_bytes()).expect("source copy");
        limited.limits.fuel_slice = 50_000;
        assert_eq!(
            limited.call::<_, i32>("compile", (pointer, integer(source.len()).expect("length"))),
            Err(Error::Fuel)
        );
        let mut control = vm(generous());
        control.compile(&source).expect("parser fuel control");
        assert_eq!(control.zero("budget_events").expect("events"), 0);
        assert_eq!(limited.last_trap, Some(wasmi::core::TrapCode::OutOfFuel));
        let nested = format!(
            "export default async function(){{return {}0{}}}",
            "(".repeat(200),
            ")".repeat(200)
        );
        let mut interpreter = vm(generous());
        assert_eq!(interpreter.compile(&nested), Err(Error::Protocol));
        assert_eq!(
            interpreter.last_trap,
            Some(wasmi::core::TrapCode::StackOverflow)
        );
    }

    #[test]
    fn array_coercion_hooks_never_execute() {
        if watched("backend::vm::tests::array_coercion_hooks_never_execute") {
            return;
        }
        for key in ["+1", " 1", "1\0extra", "length\0extra"] {
            let key = serde_json::to_string(key).expect("key");
            for method in ["valueOf", "[Symbol.toPrimitive]"] {
                let setup = format!(
                    "const a=[1,2];delete a[1];a[{key}]={{{method}(){{hooked++;host.call('tripwire',null);return 1}}}};"
                );
                for mode in ["output", "argument", "callback"] {
                    let body = match mode {
                        "output" => format!("{setup}return a"),
                        "argument" => format!("{setup}return await host.call('echo',a)"),
                        _ => format!(
                            "return await host.call('helper',null,async()=>{{{setup}return a}})"
                        ),
                    };
                    let source = format!(
                        "export default async function(){{globalThis.hooked=0;host.call('probe',null,async()=>hooked);{body}}}"
                    );
                    let mut vm = vm(generous());
                    vm.compile(&source).expect("compile");
                    vm.start(
                        &StructuredValue::new(serde_json::Value::Null, &generous()).expect("input"),
                    )
                    .expect("start");
                    vm.zero("status").expect("entry");
                    assert_eq!(vm.zero("event").expect("probe"), 1);
                    assert_eq!(vm.one("event_pop", 1).expect("pop"), 0);
                    if mode == "callback" {
                        assert_eq!(vm.zero("event").expect("helper"), 2);
                        vm.one("event_pop", 2).expect("pop");
                        vm.callback(
                            2,
                            &StructuredValue::new(serde_json::Value::Null, &generous())
                                .expect("input"),
                        )
                        .expect("callback");
                        for _ in 0..8 {
                            vm.zero("step").expect("job");
                        }
                        assert_eq!(vm.one("callback_result", 2).expect("encode callback"), -1);
                    } else if mode == "output" {
                        assert_eq!(vm.zero("result").expect("encode output"), -1);
                    }
                    assert_eq!(vm.failure(), Err(Error::Value), "{mode}: {key}");
                    assert_eq!(vm.zero("event").expect("no effect"), 0);
                    vm.callback(
                        1,
                        &StructuredValue::new(serde_json::Value::Null, &generous()).expect("input"),
                    )
                    .expect("probe callback");
                    for _ in 0..8 {
                        vm.zero("step").expect("probe job");
                    }
                    assert_eq!(vm.one("callback_result", 1).expect("probe encode"), 0);
                    assert_eq!(
                        vm.output().expect("hook counter").as_json(),
                        &serde_json::json!(0),
                        "{mode}: {key}"
                    );
                }
            }
        }
    }
}
