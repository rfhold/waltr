# Telemetry Smoke Checks

## Purpose

Use this runbook to confirm live Faro, OTLP trace, log, metric, and Pyroscope ingestion through Grafana Explore.

These checks complement deterministic tests. They verify one active runtime and its telemetry path, not source behavior or deployment provenance.

## Prerequisites

- Use Grafana access with Explore permissions for Loki, Tempo, the metrics data source, and Pyroscope.
- Select a recent time range that includes the test activity and exporter batch delay.
- Use a hosted production bundle or a Tauri release bundle for Faro.
- Use a non-test Tauri native build for `waltr-app-native` OTLP signals.
- Configure the standalone server OTLP exporter through standard OpenTelemetry environment variables.
- Build the standalone server with `profiling`, then set `WALTR_PYROSCOPE_URL` for profiles.
- Confirm that the active artifact contains the behavior under test. Repository changes alone do not prove deployment.

## Steps

1. Open the hosted app or Tauri release app. Navigate across views and trigger a safe, known application error if available.
2. Start or use the Tauri native app. Exercise one server-backed action to create native transport activity.
3. Send one request to the standalone server. Exercise an MCP or session action if trace depth matters.
4. In Loki Explore, run `{service_name=~"waltr-(spa|tauri)"}`. Confirm recent Faro logs, events, or measurements for the exercised client.
5. In Loki Explore, run `{service_name=~"waltr-(server|app-native)"}`. Confirm recent server and native logs from the active runtimes.
6. In Tempo Explore, run `{ resource.service.name =~ "waltr-(server|app-native)" }`. Confirm recent traces for each exercised service.
7. In the metrics data source, run `waltr_app_startups`. Confirm a recent sample for service `waltr-app-native`.
8. In Pyroscope Explore, select a process CPU profile and use `{service_name="waltr-server"}`. Confirm samples from the standalone server.

## Native Audio Diagnostics

Rebuild and relaunch the native app before these checks. An older packaged binary does not emit these events.
No exporter, endpoint, or logging configuration change is necessary.

The private `native_audio_telemetry` module preserves the app target `waltr_app_lib::dictation_telemetry` and emits these fixed events:

| Event | Fields and semantics |
| --- | --- |
| `native.audio.admission` | `entrypoint`: `hold` or `chat`; `outcome`: `admitted`, `busy`, `unavailable`, `stale`, or `duplicate`. Rejections do not create operation spans. |
| `native.audio.permission` | `entrypoint`, process-local `diagnostic_id`, `category`, and `elapsed_ms`. Categories: `granted`, `permission_denied`, `permission_unavailable`, `permission_busy`, `cancelled`. |
| `native.audio.milestone` | `entrypoint`, `diagnostic_id`, `stage`, and `elapsed_ms`. Each stage appears at most once per operation. |
| `native.audio.finished` | One summary after owned cleanup: `entrypoint`, `diagnostic_id`, `outcome`, primary `category`, `capture_cleanup`, `typing_cleanup`, `stage`, `failure_stage`, and `elapsed_ms`. |

The span `native.audio.operation` covers permission, capture, RPC, output actions, and owned cleanup.
It retains the summary fields and parents the native gRPC spans through async instrumentation.
The diagnostic identifier counts operations within this process; it never copies IPC, binding, routine, device, or client identifiers.

Milestone semantics:

- `capture_requested`: the host requests a blocking capture worker; the device is not ready yet.
- `capture_ready`: the capture factory returns a source on its worker; this does not measure callback time.
- `first_frame_observed`: the host dequeues its first nonempty PCM frame before RPC admission; this does not measure callback time.
- `rpc_open`: the host requests interaction admission.
- `accepted`: the host observes the server's Accepted event.
- `input_finished`: the input finish request completes.
- `output_finished`: the selected final output completes.
- `action`: the host requests a typing delivery or command action; this does not assert successful delivery.
- `cleanup`: the host starts an owned cleanup await. Capture cleanup can precede a command action.

The latest distinct milestone supplies `stage`; the first primary failure also preserves `failure_stage`.
A failure before Accepted can lack both `accepted` and `first_frame_observed`.
RPC admission requires a nonempty frame. `capture_ready` can occur after `cleanup` starts when teardown must await a late factory return.
That event order does not imply a new capture request or a cleanup failure.
Clean zero-input capture reports outcome and category `no_input`, after cleanup, without `first_frame_observed`, `rpc_open`, or `accepted`.
It creates no upstream speech request, final text, external text, or command action.
Zero frames with a capture failure still report that failure, not `no_input`.
Capture categories distinguish `capture_host_overflow`, `capture_budget`, `capture_unsupported`, `capture_backend`, `capture_callback_overflow`, `capture_callback_pool_exhausted`, `capture_callback_queue_full`, `capture_conversion`, `capture_resource_limit`, and `capture_worker`.
Host overflow includes host credit, frame-size, alignment, or closed-consumer failures; it does not identify a native callback overflow.
Native callbacks report `capture_callback_pool_exhausted` when no free block remains, including converter-owned blocks, or `capture_callback_queue_full` when a leased block cannot enter the raw queue. The generic `capture_callback_overflow` remains accepted for compatibility.
These fixed literals contain no device identity, audio, or backend error text. They identify the exhausted resource, not the physical cause of a scheduling delay or backpressure.
Conversion includes nonfinite samples or misalignment; resource limits cover native limits.
Worker panic or join failure reports `capture_worker`. True backend and device-resolution failures remain `capture_backend`.
These categories do not establish the physical cause of historical backend failures.
RPC categories distinguish `rpc_open`, `rpc_input`, `rpc_output`, and `rpc_protocol` where the current boundary preserves that distinction.
Validated remote FAILED and INTERRUPTED terminals report `rpc_remote_failed` and `rpc_remote_interrupted`; a typed admission rejection reports `rpc_rejected`.
These categories never include remote failure codes or messages.
The generic `interaction` category covers other collapsed interaction failures.
`rpc_protocol` identifies missing or invalid selected output rather than a validated remote failure terminal.
`ClientErrorKind::Other` merges stream protocol, transport, and remote failures; `rpc_output` cannot separate those causes without a broader typed error contract.
Typed client login failures preserve `rpc_login_required`; no authentication details enter telemetry.
Diagnostics never parse or export `ClientError` messages.
Typing categories distinguish unsupported, backend, control, limit, and revision failures with the `typing_` prefix.
The `typing_guard_or_cancelled` category preserves the typing boundary but does not distinguish individual guard or backend cancellation causes.
Command categories use the `command_` prefix with `invalid`, `input_limit`, `expanded_limit`, `timeout`, `output_limit`, `io`, `exit`, or `cleanup`.
Cancellation and invalidation report `cancelled` outcomes at INFO; ordinary failures report WARN.
A separate cleanup failure also reports WARN, even after cancellation.
Cleanup fields preserve failures independently; successful cleanup does not erase a primary error.
A capture worker error that fails the primary pipeline retains its capture category and observed failure stage.
The same error can also appear in `capture_cleanup`.
A cleanup-only failure leaves the primary category `none`. A worker join reports the whole worker result, not a precise teardown origin.
`none` means no primary failure; `not_needed` means no cleanup result exists for that resource.

In Loki, first inspect `{service_name="waltr-app-native"}` and the returned attribute labels.
Collectors can map `event.name` to `event_name`, with an empty log body, just like `app.startup`.
If that mapping exists, use label queries rather than body substring filters:

```logql
{service_name="waltr-app-native", event_name="native.audio.finished"}
{service_name="waltr-app-native", event_name="native.audio.milestone", entrypoint="hold"}
{service_name="waltr-app-native", event_name="native.audio.finished", category="capture_host_overflow"}
```

If the collector retains attributes as structured metadata, use its actual field names in pipeline filters instead.
In Tempo, use `{ resource.service.name = "waltr-app-native" && name = "native.audio.operation" }`.
Compare milestone elapsed times and child gRPC spans within the same trace.
Audio RPC children are named `routine_interaction.Interact` or `routine_interaction.Run`, with service `waltr.routine.v1.RoutineInteractionService` and the literal method name.
Unknown paths still report `grpc.unknown` without exporting the path. This does not add upstream server status precision.
Operation permission, milestone, and terminal logs carry the operation trace and span IDs.
This includes worker `capture_ready` logs and terminal logs after the async operation scope exits.
Deterministic in-memory exporter tests check those exact IDs. Confirm Loki-to-Tempo linking separately on the active artifact; these tests do not prove live collector mapping or ingestion.
Admission events without an operation do not require correlation.
These checks establish runtime evidence only after the new artifact emits the documented schema.

## Routine Interaction Trace Chain

Use an artifact that contains the interaction trace instrumentation.
Live checks require separate approval for the actual interaction and any device or external-text effects.
Deterministic tests use fake loopback services and in-memory exporters; they do not prove collector ingestion or Whispers deployment.

The expected parent chain follows the native client RPC into these server-owned spans:

| Span | Lifetime and fields |
| --- | --- |
| `routine.interact` / `routine.run` | RPC admission and context ownership; `outcome` and numeric `grpc_status`. Engine work retains the RPC span after handler return. |
| `routine.interaction` | Durable admission, execution, and owned cleanup; finite `outcome`. |
| `routine.interaction.owner` | Runtime execution and child-task drain; finite `outcome`. |
| `routine.interaction.task` | Task prerequisites, adapter execution, and cancellation cleanup; literal `task_kind` and finite `outcome`. |
| `speech.streaming_recognize` / `speech.streaming_synthesize` | Whole duplex operation through EOF, failure, cancellation, or drop; finite `outcome` and numeric upstream `grpc_status` when available. |
| `speech.recognize` / `speech.synthesize` | Generic unary audio-library calls; the interaction adapters still require duplex speech. |

Task kinds are `transcribe`, `synthesize`, `collect`, and `session_template`.
Admission outcomes are `accepted`, `rejected`, `failure`, or `incomplete`.
Execution outcomes are `success`, `failure`, `cancelled`, `deadline`, or `incomplete`; speech validation also supports `invalid`.
`incomplete` never implies success: response drop or task abort can prevent an explicit terminal result.
A numeric upstream status such as `8` preserves admission-failure precision without an upstream message or arbitrary failure code.
A local deadline or cancellation does not invent an upstream status.

Embedded server and speech spans use service `waltr-app-native`; standalone spans use `waltr-server`.
Native trace admission adds only the exact targets `waltr_server::routine::telemetry` and `waltr_audio::telemetry`.
It excludes their logs, lookalike targets, nested targets, and other server/audio modules.
Exporter ownership and native log admission stay unchanged.

In Tempo, locate an interaction span:

```traceql
{ resource.service.name =~ "waltr-(server|app-native)" && name = "routine.interaction" }
```

Inspect its trace ID and parent span IDs through the task and speech RPC.
For duplex speech, confirm that the span extends beyond response headers through terminal completion or cancellation.
The outbound `traceparent` identifies the speech RPC span, not its task parent.
Whispers can continue that trace only when its active artifact supports parent extraction and exports to the same trace backend.
Waltr forwards no `baggage` or `tracestate`.
Absent or malformed version-00 parents create a local trace; native host filters do not require the generic middleware span.

These fields contain no Routine, task, invocation, session, or provider identities, configuration, runtime paths, endpoints, PCM, transcripts, prompts, or error messages.
The client-side failed-stream classification limit remains separate; server and speech spans do not change that client contract.
See [interaction trace ownership](../architecture/routines.md#interaction-trace-ownership) for lifecycle boundaries.

## Expected Result

Each enabled signal returns recent data from the runtime that produced the test activity. Missing disabled signals do not indicate failure. Tauri Faro contains no request telemetry; native traces contain its gRPC activity.

Read-only Grafana checks on 2026-09-04 found prior ingestion for both trace services, both Faro identities, `waltr_app_startups`, and server profiles. That historical result does not show that current working-tree changes are deployed.

## Troubleshooting

- If Faro data is absent, confirm a production or Tauri release build and collector CORS access.
- If native data is absent, inspect stderr for `waltr native telemetry initialization failed; continuing without telemetry`.
- If server OTLP data is absent, confirm `WALTR_OTEL_EXPORT` or `OTEL_EXPORTER_OTLP_ENDPOINT` and standard exporter settings.
- If profiles are absent, confirm a standalone `profiling` build and a valid `WALTR_PYROSCOPE_URL`.
- If queries stay empty, widen the time range and confirm the selected data source.

## References

- [Client observability boundary](../architecture/clients.md#client-observability-boundary)
- [Deployment observability](../deployment.md#client-observability)
- [Deterministic telemetry tests](../testing.md)
