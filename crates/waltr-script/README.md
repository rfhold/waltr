# waltr-script

`waltr-script` executes one fresh JavaScript module per invocation.
The sole backend embeds QuickJS-NG 0.16.2 as WASM and interprets it through Wasmi 0.46.0.
No server, agent, audio, MCP, or generated RPC dependency enters this crate.

## Rust boundary

```rust,ignore
pub trait ScriptEngine: Send + Sync {
    fn validate(&self, source: &str, limits: &Limits) -> Result<(), Error>;
    fn start(
        &self,
        source: String,
        input: StructuredValue,
        limits: Limits,
        host: Arc<dyn HostDispatcher>,
    ) -> Result<Box<dyn ScriptExecution>, Error>;
    fn info(&self) -> EngineInfo;
    fn shutdown(&self) -> ScriptFuture<Result<(), Error>>;
}

pub trait ScriptExecution: Send {
    fn cancellation(&self) -> Arc<dyn ScriptCancellation>;
    fn cancel(&self);
    fn join(self: Box<Self>) -> ScriptFuture<ExecutionReport>;
}

pub trait ScriptCancellation: Send + Sync {
    fn cancel(&self);
}

pub trait HostDispatcher: Send + Sync {
    fn dispatch(&self, request: HostRequest, context: HostContext) -> HostFuture;
    fn cleanup(&self, invocation: InvocationId) -> CleanupFuture;
}
```

`Engine::new(admission_limit)` constructs the backend and its shared admission gate.
Clones share that gate; independent engine instances have independent gates.
Production composition must share one configured engine across callers.
`HostFuture`, `CleanupFuture`, and `ScriptFuture<T>` are owned `Send` boxed futures.
`HostReply` carries one `StructuredValue`, byte vector, or `HostHandle`.
`ExecutionReport` contains the result, fuel count, elapsed time, and host cleanup status.
Reports contain no source text, guest exception messages, or host error messages.

`HostContext::new`, `HostContext::callback`, and `HostContext::close` support engine substitutes.
These Rust-only constructors grant no effect authority.
Substitutes own callback lifetime, quotas, cancellation, and terminal cleanup.
The production callback transport uses a bounded channel into the engine pump.
No interpreter value crosses that channel.

## JavaScript ABI 1

```js
export default async function (input) {
  const result = await host.call("example.operation", {input});
  return result;
}
```

The source is a complete ES module, not text inside a generated function wrapper.
`validate` compiles and resolves the module without evaluation.
`start` repeats syntax validation before any execution or host effect.
Module bodies execute only inside the invocation.
The default export must return an actual engine promise.
Top-level await follows the same execution budgets.
Static imports fail validation; dynamic imports reject at runtime.
No module loader, environment, filesystem, network, device, or process API exists in the guest.

The generic bridge has one method:

```js
host.call(operation, arguments, callback = null, bytes = null, handle = null)
```

- `operation`: 1–128 ASCII letters, digits, dots, underscores, or hyphens.
- `arguments`: one strict structured value, never omitted.
- `callback`: optional function that returns an actual engine promise.
- `bytes`: optional `ArrayBuffer`, copied through guest memory, outside JSON.
- `handle`: optional frozen opaque object from a host reply.

The host object and its method have fixed identities and frozen properties.
Operation names remain untrusted input; the dispatcher always decides authority.
The backend contains no application operation table.
Host replies resolve to structured data, an `ArrayBuffer`, or an opaque handle.
All structured host inputs use null prototypes, including arrays.
Returned binary buffers also use null prototypes and expose an own, read-only `byteLength`.
Their prototype methods require explicit calls through `ArrayBuffer.prototype`.
Private host reply resolution never reads a user prototype.
Host errors reject with frozen private error objects.
An uncaught private error preserves its Rust category.
A script-created error with the same text or properties remains a script error.

### Callbacks and resources

The host invokes `CallbackRef::invoke(value).await` through the owner pump.
Callbacks support nested awaited host calls without an engine lock across the await.
A host helper can reject a proposal repeatedly within the callback budget.
The helper call remains the same call; callback rejection causes no engine replay.
Callback exceptions fail execution.
Callbacks expire when their host call settles or invocation cleanup begins.
Concurrent evaluations of the same callback fail with `Identity`.

`HostContext::issue_handle(kind)` creates a random, invocation-bound identity with a monotonic generation.
The guest sees neither its registry index nor its token.
`validate_handle(handle, expected_kind)` checks kind, generation, token, invocation, and lifetime.
`release_handle` revokes the identity; the dispatcher separately closes the actual resource.
Final cleanup invalidates all identities before the dispatcher releases its actual resources.

### Structured values

The bridge accepts null, booleans, strings, finite numbers, dense arrays, and plain objects.
Integers must fit the exact JavaScript safe range.
Plain objects use the original object prototype or a null prototype.
Arrays use the original array prototype or a null prototype.
The bridge rejects accessors, symbol keys, non-enumerable data, proxies, cycles, and exotic objects.
Arrays also reject holes and extra properties.
Functions, promises, bigint, undefined, typed arrays, and handles are not structured values.
Only the separate binary and handle fields transport those supported resource forms.

The C bridge reads own data descriptors without user hooks.
It clones data into prototype-free containers before the private JSON serializer runs.
User `toJSON` hooks and prototype changes do not control serialization.
Both Rust and C validate data budgets at every boundary.
Rust serialization stops at the byte ceiling instead of allocating an unbounded JSON buffer.

## Budgets

Every limit is nonzero and has a hard cap.
`Limits::validate` rejects invalid configurations before admission.

| Limit | Default | Hard cap |
| --- | ---: | ---: |
| Source | 64 KiB | 64 KiB |
| Structured value per boundary | 64 KiB | 64 KiB |
| Depth | 32 | 64 |
| Nodes, including object keys | 4,096 | 8,192 |
| String or key | 16 KiB | 64 KiB |
| Binary payload | 64 KiB | 64 KiB |
| QuickJS heap | 16 MiB | 24 MiB |
| QuickJS C stack | 256 KiB | 512 KiB |
| WASM linear memory | 32 MiB | 32 MiB |
| Fuel per engine operation | 20 million | 100 million |
| Cumulative fuel | 2 billion | 100 billion |
| Host calls | 256 | 1,024 |
| Concurrent host calls | 16 | 32 |
| Callback evaluations | 64 | 1,024 |
| Live handles | 32 | 256 |
| Execution deadline | 30 seconds | 90 minutes |
| Engine-wide reservations | Constructor argument | 128 |

The guest heap minimum is 1 MiB; the stack minimum is 16 KiB.
The linear memory minimum is 4 MiB.
Cumulative fuel must cover at least one fuel slice.
The 90-minute deadline cap does not change the 30-second default or replenish fuel, heap, or host quotas.
Fuel exhaustion terminates the invocation; the backend never resumes a trapped operation.
Guest teardown receives a separate fixed allowance of 100 million fuel units.
Host awaits consume wall time, not interpreter fuel.
The pump checks deadlines and cancellation between interpreter operations and before host dispatch.
It processes at most eight microtasks per batch before host completion and callback work.
One synchronous guest operation remains bounded by its fuel slice, not a preemptive wall-clock interrupt.

The [tracked QuickJS patch](wasm/quickjs-budget.patch) enables C-stack checks that upstream disables under WASI.
Public allocator callbacks cannot observe every internal heap-limit refusal. Wasmi traps alone do not preserve native quota provenance.
The patch therefore remains necessary for these guarantees; fuel and memory limits do not replace its native budget hooks.
It also records native heap-limit denials, C-stack denials, and guest allocation failures in private C state.
These events terminate the invocation even if JavaScript catches the resulting exception.
`Memory` denotes observed guest allocation failure or heap denial, not process RSS exhaustion.
`Limit` includes native C-stack denial and host quotas.
`Fuel` denotes Wasmi fuel exhaustion or the Rust cumulative fuel ceiling.
Other interpreter traps, including Wasmi's separate interpreter-stack overflow, fail closed as `Protocol`.
Compile-only rejection without a native budget event is `Syntax`; ordinary guest rejection is `Script`.
No category comes from guest error text or user properties.
The C-stack quota is not a count of JavaScript frames; heap and fuel also bound execution.

## Ownership and teardown

Admission covers compile validation, worker execution, and cleanup.
The engine reserves capacity before each invocation thread starts.
Each worker owns its thread-affine store and polls host futures on a private Tokio runtime.
The runtime supports trusted host I/O and timers; neither creates guest authority.
No host task detaches through the engine.

Script return, failure, cancellation, and panic all drop owned host futures first.
The backend destroys guest memory before host cleanup.
A separate reaper joins finished worker threads without head-of-line waits on unrelated workers.
Only then does it release admission and publish the terminal report.
`join` awaits a notification; it never joins a thread on the async executor.
Before `join` consumes the execution owner, `cancellation()` returns an independent `Arc<dyn ScriptCancellation>`.
Clones support cancellation from other tasks or threads while `join` awaits teardown.
Repeated requests and requests after termination have no additional effect.
The control handle grants no ownership: its drop does not cancel execution or prove teardown.
`Drop` requests cancellation and returns without a thread join.
An unpolled or cancelled join future also requests cancellation.
`shutdown` stops admission, cancels all executions, and joins the reaper through a Tokio blocking worker.
Explicit shutdown requires a Tokio runtime.

Host cleanup has a separate deadline: the smaller of ten seconds and the invocation deadline.
Cleanup error, panic, or timeout returns `Cleanup` and permanently retains that admission reservation.
Shutdown reports the failed cleanup instead of claiming successful resource release.
No timeout substitutes for a thread join.

Host futures and cleanup must cooperate with cancellation through drop.
Arbitrary synchronous Rust stalls, host-spawned detached tasks, and host resource leaks violate this contract.
The interpreter cannot preempt such host code.
Cancellation does not undo completed external effects.
The engine never retries or replays an operation or module.

## Evidence and limits

```sh
cargo test -p waltr-script --locked
cargo test -p waltr-script --locked --release
cargo clippy -p waltr-script --all-targets --locked -- -D warnings
cargo check -p waltr-script --locked
cargo fmt --all -- --check
```

The contract parent runs the real backend in a disposable child with a watchdog.
The parent drains both output pipes concurrently, caps retained output, and kills/reaps timeouts.
Separate fixtures exercise actual watchdog termination and output overflow.
The child suite covers callbacks, binary transport, strict values, quotas, cancellation, panic recovery, isolation, authority, and teardown.
The linear-memory, native-budget, and array-hook probes have their own disposable watchdogs.
Native-budget probes compare the same source with only the relevant heap or C-stack limit changed.
They observe native event bits and successful controls for both execution and compile-only parser stress.
An isolated parser call proves fuel exhaustion; a separate deep-parser probe identifies Wasmi's interpreter-stack trap.
Array regressions cover argument, result, and callback boundaries, with private rooted callbacks checking that coercion-hook counters stay zero.
The public substitution test uses no real interpreter.
The cancellation watchdog verifies independent control after the first `join` future poll returns `Pending`.
It covers CPU-only microtasks and a nested host wait.
It also verifies harmless control-handle drop, repeated cancellation, host cleanup, and admission after joined teardown.

Wasmi bounds guest memory, not process RSS or OS isolation.
This crate provides no permission policy, durable invocation service, effect compensation, or durable heap recovery.
Current platform evidence covers only the local Apple Silicon macOS host.
Linux, mobile link, simulator, device, and package execution remain unverified.
See [PROVENANCE.md](PROVENANCE.md) for artifact inputs and rebuild instructions.
