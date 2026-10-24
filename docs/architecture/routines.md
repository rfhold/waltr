# Routines

## Purpose

Waltr Routines define server-owned event, manual, and interaction workflows. The Routine service separates live definitions from durable invocation history. The current server code implements interaction RPCs.

This document defines the Routine lifecycle, persistence, replay, restart, shared-client, and user-interface contracts.

## Responsibilities

| Layer | Owns |
| --- | --- |
| Resource catalog | Strict Routine source, validation, publication, revision identity, and complete manual-definition projections. |
| Routine engine | Event and manual admission, immutable invocation capture, task execution, helper sessions, and graceful shutdown. |
| Internal interaction runtime | Live components, speech adapters, collectors, standalone helpers, explicit groups, resource bounds, and owned cancellation. |
| Waltr SQL persistence | Invocation aggregates, append-only lifecycle events, pagination, replay, and restart reconciliation. |
| `waltr-client` | Global and source-session watch interests, independent replay state, history and detail reduction, pagination, start association, and stable views. |
| Web and TUI | Platform routes, forms, navigation, and presentation from shared client snapshots. |

## Definitions

Routine definitions remain catalog state, not invocation history. `WatchManualRoutines` provides one global stream of complete typed manual-definition snapshots.

Each snapshot preserves the catalog's Routine order, trigger order, input order, and result order. It includes a stable projection digest.

The server emits an initial snapshot. It then reprojects the current catalog after each publication or receiver lag event.

The server suppresses a projection when its digest equals the prior digest. Lag recovery therefore uses current catalog state, not missed deltas.

Definitions include each Routine revision. An admitted invocation pins its immutable Routine revision, trigger, source capture, and ordered task descriptors.

The bundled `compact-and-continue` Routine is the implemented compaction-prefix workflow. Its manual trigger exposes boolean `continue` with default `false`; its lifecycle trigger assigns `continue: true`.

Later catalog changes affect future admissions only. History and detail remain interpretable after the current definition changes or disappears.

### Agent Output Results

Agent task results accept `$(output.text)` for raw assistant text or `$(output.json.FIELD)` for a top-level JSON field. `FIELD` contains only ASCII letters, digits, and underscores. JSON extraction requires one strict object and selects only strings or booleans. Declared result types still apply. Nested paths, JSONPath, jq, Markdown fences, repair, retries, and fallback remain unsupported. Output references work only in task-output contexts; dependent tasks use `$(tasks.<task>.results.<name>)`.

The raw JSON limit is 512 KiB UTF-8 before parse. Expanded scalar strings and each expanded prompt also have 512 KiB bounds. Metadata keys and JSON-encoded metadata values retain separate 16 KiB limits. Extraction errors omit source payloads: `output_json_size` identifies raw overflow, `output_json` identifies invalid JSON or a non-object root, `output_json_field` identifies an absent field, and `output_json_type` identifies a non-string/non-boolean field. Unavailable output or unsupported selector syntax uses `substitution`. Invalid extraction fails the producer task before any dependent helper starts. The [conversation-notes example](../examples/routines/audio-conversation-notes.yaml) uses separate retained-transcript and deletion-audit results; only its final helper feeds the desktop text port.

## Interaction Catalog And Runtime

The catalog accepts strict `interaction` triggers, audio/text ports, speech tasks, collectors, stream routes, standalone model selection, and named work groups. The transport-independent `routine::interaction` runtime owns concurrent stream scheduling. It shares task completion policy and helper construction with the sequential event, manual, and completion-lifecycle executor.

The application registers both `RoutineInteractionService` methods under common gRPC/gRPC-web authentication. Native `Interact` accepts bidirectional input; `Run` accepts one bounded request and returns a response stream. Both methods share preflight, capacity admission, pinned catalog/provider settings, durable lifecycle records, and engine-owned shutdown.

This section owns the runtime and admission contract. The [audio examples](../examples/routines/audio-interactions.md) explain YAML and the wire contract. [Speech providers](config.md#speech-provider-contract) owns upstream configuration. Local implementation does not establish release, deployment, or real upstream compatibility.

### Admission And Transports

Shared capability preflight rejects unsupported helpers, actions, source sessions, and speech options before durable history. Helpers require explicit standalone models, one turn, and empty feature, tool, and permission lists. Speech tasks require streaming capability even though `waltr-audio` also exposes unary methods.

Both methods commit durable admission before the service queues `accepted` with the invocation ID. Queue insertion does not prove that the client received acceptance. Disconnect during the transaction does not abandon its outcome. A committed invocation retains history even if cancellation prevents acknowledgment delivery.

`Run` validates the original complete request against its quota before admission. Independent per-port feeders move those bodies and split them into bounded frames. Request order does not block prerequisite inputs. Synthetic segment IDs and protocol metadata do not consume the original request quota again. Native ingress uses per-port workers with one finite aggregate byte/chunk budget, including in-flight sends. This permits cross-port progress within that budget without unbounded per-port storage.

Native request order limits in-band cancellation visibility. A `cancel` behind an entirely full ingress budget cannot reach the reader until capacity returns. Queue-wait, input, and invocation timeouts bound that wait. Transport drop and server shutdown cancel independently of request-reader progress.

`input_completed` closes the input phase, not the output phase. Native half-close implies input completion only after all streams close; an open stream makes half-close invalid. After input completion, clients can keep the request side open for explicit `cancel`. Wire cancellation, response drop, disconnect, and shutdown cancel owned work rather than detach execution. Broken transports cannot guarantee terminal delivery.

Compiler-facing catalog types live under `resource_catalog::routine`:

| Type or accessor | Contract |
| --- | --- |
| `RoutineInteractionTrigger { inputs, input_ports, output_ports }` | Scalar exposure separate from typed, single-stream ports. |
| `RoutineStreamFormat` | Tagged `Text` or `Audio { encoding, sample_rate_hz, channels }`; initial encoding is `PcmS16Le`. |
| `RoutineStreamRef` | `InteractionInput(String)` or `TaskOutput(String)`, never a scalar. |
| `RoutineModelRef { provider, model }` | Explicit literal keys checked against the complete snapshot. |
| `RoutineTaskSpec::{transcribe,synthesize,collect}` | Mutually exclusive with existing session-template/action kinds. |
| `RoutineTaskSpec::stream_input()` | Optional live dependency; consumer starts before producer completes. |
| `RoutineTaskSpec::stream_output_format()` | Typed stream output or no stream. |
| `RoutineTask::run_after` | Completion dependencies only. Both edge kinds must form one acyclic graph. |
| `RoutineWorkGroup { name, on_busy, max_queued }` | Explicit queue/reject/interrupt policy, with task opt-in via `work_group`. |
| `InteractionLimits::default()` and `validate()` | Server-owned limits passed explicitly to the runtime; global admission remains a service responsibility. |

### Execution And Ownership

A live-stream component includes task producers, their consumers, and virtual interaction-input producers. Every member waits for all external completion prerequisites before eligibility. The runtime reserves every queue-group resource atomically for the component through completion. Finite queue limits apply; completion-only successors acquire no resources before their prerequisites finish.

Named `queue`, `reject`, and `interrupt` policies apply only through explicit task group selection. Interruption fences suppress stale output; they cannot retract output that a consumer already received. A task starts once per invocation. The runtime adds no automatic ASR turns, debounce, or implicit supersession. Any interrupted task leaves a terminal `task_interrupted` outcome, even if later group work succeeds.

Both executors share task completion policy and helper request construction. `runAfter` requires completion, not success: succeeded and skipped prerequisites permit successors; failed or interrupted prerequisites never enable descendants. Live-component readiness and reservations remain interaction-owned. Sequential admission rejects speech and collector tasks before durable admission. Standalone model helpers in either executor use admission-pinned configuration; source-copy and inherited-inference helpers retain their sequential behavior.

Helper metadata records Routine name, revision, trigger, task, and execution identity. `waltr.routine.invocation_id` associates the helper with its public durable invocation when present; `waltr.routine.execution_id` remains distinct and determines the child ID. Source session metadata is included only when there is a source or inherited-inference session.

The runtime owns producer, consumer, adapter, and helper work through cancellation and join. Output-receiver drop cancels the interaction. Cleanup awaits durable helper creation even after cancellation, then interrupts the helper as required by the host durability contract. The interaction adapter rejects actions and source-backed helpers instead of claiming support. Standalone helpers require one turn and empty feature, tool, and permission lists.

### Speech And Text

Speech tasks use the selected endpoint and model settings from the pinned configuration. Interaction speech requires the model's streaming capability and never falls back to unary calls. Synthesis requires mono and rejects nonzero pitch. Device selection, capture, playback, and codec conversion belong to clients.
Native chat dictation and holds wait for nonempty PCM before interaction admission; clean zero-input capture creates no invocation or speech request.
This client safety rule does not change generic interaction empty-input semantics. See [native audio composition](clients.md#native-app-audio-composition).

Plain transcription maps upstream revisions and final results to request-local channel/ordinal segments. It ignores word and speaker attribution and preserves the upstream transcript. Segment IDs do not establish stable cross-request identity or speaker identity.

With `recognition.diarization: true`, transcription uses one request-local cumulative segment. Provisional plain text replaces earlier revisions; only the final cumulative response commits speaker-labeled text. Contiguous words with the same `WordInfo.speaker_label` form a line such as `S1: Hello, world!`. A speaker change starts a new line; empty or whitespace-only labels produce `unknown: ...`. Labels identify no real person and apply only within the recording. Channel tags never supply speaker labels. The adapter preserves punctuation within each whitespace word and separates words with one space. It rejects final words that disagree with the transcript, missing attribution, unsafe labels, and responses after the final result. Empty final transcripts remain valid. The text contract exposes no word offsets.

Diarization retains the finite interaction limits. The Google response decode bound equals `chunkBytes + 16 KiB`, or 1 MiB plus 16 KiB by default. The complete text segment, including its ID, must fit the default 1 MiB `chunkBytes` cap. The server collector cap remains 1 MiB. A long cumulative response can exceed these bounds before the upstream 4 MiB response ceiling. Operators must budget chunks, edges, collectors, deadlines, and helper context for the complete recording. Overflow fails explicitly, without truncation.

A collector starts with its producer and waits for successful stream close. It resolves revisions and joins committed segments in first-appearance order. Successful close requires every segment committed. One downstream cleanup helper receives the complete transcript, not one call per chunk. Synthesis consumes committed segments early, in first-appearance order, and never consumes speculative revisions.

The collector enforces both task `maxBytes` and the server collector cap. Cleanup compares UTF-8 prompt bytes plus a fixed allowance against known model context metadata. This conservative bound is not a tokenizer. Without context metadata, collector and upstream limits still apply. Oversize text fails; the runtime does not truncate, summarize, or split it.

### Interaction Trace Ownership

The interaction RPC captures trace context before the engine accepts task ownership.
Async instrumentation carries that context through engine work, the runtime owner, and each task.
The fixed span chain is `routine.interact` or `routine.run` → `routine.interaction` → `routine.interaction.owner` → `routine.interaction.task` → speech RPC.
The interaction span covers execution and owned cleanup, even after the RPC handler returns response headers.
Task spans cover adapter execution and cancellation cleanup.
Speech spans cover unary completion or the entire duplex operation, not just response headers.
Dropped or aborted work defaults to `incomplete`, never `success`.

The dedicated targets are exactly `waltr_server::routine::telemetry` and `waltr_audio::telemetry`.
Native telemetry admits only spans from these targets; it does not admit broader server/audio targets or their logs.
Embedded spans use the native host's `waltr-app-native` exporter.
Standalone spans use the server's `waltr-server` exporter.
Neither library installs another subscriber, exporter, or global propagator.

The RPC reuses an active middleware parent when its trace matches the validated request parent.
If that parent lacks context or belongs to another trace, the RPC uses the validated version-00 `traceparent` directly.
Absent or malformed parents create a new local trace.
Speech requests inject only a valid `traceparent`; they never forward `baggage` or `tracestate`.

Custom span fields contain only literal task kinds, finite outcomes, and numeric gRPC status.
They exclude identities, configuration, runtime paths, endpoints, content, arbitrary failure codes, and error messages.
Server admission spans distinguish `accepted`, `rejected`, `failure`, and unfinished work.
Execution outcomes distinguish `success`, `failure`, `cancelled`, `deadline`, and `incomplete`; speech validation also supports `invalid`.
These diagnostics do not change client error classification or recover precision that another boundary discards.
See the [trace smoke checks](../runbooks/telemetry-smoke.md#routine-interaction-trace-chain) for span names and runtime verification limits.

### Bounds And Durable State

| Limit | Default |
| --- | --- |
| Complete request and cumulative native request | 256 MiB |
| Chunk | 1 MiB |
| Buffered edge | 2 MiB |
| Queued chunks per edge | 8 |
| Queue wait (`queueWaitSeconds`) | 30 seconds |
| Active interactions | 16; service admission responsibility |
| Active tasks per interaction | 8; runtime rejects larger graphs |
| Input hard deadline | 3,900 seconds (65 minutes) |
| Invocation hard deadline | 5,400 seconds (90 minutes) |
| Collector | 1 MiB, also subject to task and context bounds |

These defaults apply to every interaction, including complete `Run` requests, not only conversation notes. One hour of 24 kHz mono PCM16 contains 172,800,000 raw bytes, below 256 MiB with room for protocol overhead. Native ingress counts cumulative encoded request bytes without a full-recording buffer. `Run` still requires one complete request in memory.

The native desktop closes capture normally at 3,600 seconds, then permits transcription finalization and helper output. Server input timeout remains a hard failure, not a normal close or automatic finalization. Input and invocation deadlines remain independent; input completion does not reset the total invocation deadline. The 90-minute total budget includes capture, transcription finalization, and both notes helpers. Live backend duration, diarization, and model behavior remain unverified.

The runtime and transport queues enforce cancellation-aware bounded backpressure. Each capacity wait respects the queue-wait bound and applicable input or invocation deadlines. Persistent queue-wait expiry produces `resource_exhausted`; the runtime never silently drops media or committed text. Byte and chunk limits apply together.

Hosts supply limits through `BuildAppOptions.interaction_limits` for `build_app` or `EmbeddedServerConfig.interaction_limits` for `build_embedded_server`. `BuildAppOptions::default()` supplies default limits; embedded hosts explicitly select `InteractionLimits::default()` or custom limits. Both primary builders validate limits before startup effects. The Rust field `queue_wait_seconds` serializes as `queueWaitSeconds`; this does not establish a YAML host-configuration path.

A skipped consumer explicitly detaches its edge, so it cannot stall an active producer.

The runtime accepts an optional lifecycle recorder and records ordinary invocation/task events through it. The wire enum assigns interaction trigger kind `3`; persistence stores `interaction`. Migration `0007_routine_interaction_trigger.sql` extends the database constraint. SQLite rebuilds the invocation table and preserves metadata, journal, indexes, and sequence high-water state. PostgreSQL migration behavior still requires live-database verification.

Waltr stores no audio or text stream chunks, stream events, or chunk replay. Declared scalar results follow ordinary Routine history rules and can contain final transcript text. Durable Routine result JSON envelopes have a separate 8 MiB cap for result arrays and JSON escapes. Other durable JSON envelopes retain their 64 KiB cap. Migration `0008_routine_result_budget.sql` raises only the result column constraint, preserving existing history and sequence high-water marks. This cap does not raise the 512 KiB expression limit or 16 KiB metadata limits. Cleanup helpers retain their prompts and replies through ordinary session storage. Durable lifecycle replay does not resume media execution. `OpenInteraction.request_id` remains optional correlation only, without deduplication or retry semantics. The service enforces admission, whole-interaction cancellation, and stream protocol validation.

## Manual Admission

`StartManualRoutine` validates the selected Routine, trigger, typed inputs, and any required source capture before durable admission. Omitted exposed inputs materialize their schema defaults before trigger conditions or tasks run; explicit request values take precedence. The request's `source_session_id` remains command input; admission records it as immutable string metadata under `waltr.routine.source_session_id`.

The response has one outcome:

- `accepted` returns a server-generated durable invocation ID.
- `validation_failure` returns a structured pre-admission message and field violations.

Other pre-admission failures use the gRPC error boundary. No invocation history exists unless the server commits admission.

Request IDs provide correlation only. They provide no idempotency, deduplication, or retry contract.

After admission, execution detaches from the RPC. RPC completion means that durable admission succeeded, not that task execution finished.

For a manual `sourceSession.pause`, admission first acquires an exclusive process-local pause with reject-on-busy behavior. The pause pins the source session and its latest completed cursor, queues new submissions, and captures mask-effective helper context through that cursor. Busy or capture failure occurs before durable admission and creates no invocation history. The durable admission stores the pinned source cursor; only after that commit does ownership move to the detached worker.

An invocation-owned `sessionMask` must address that exact paused session and cursor. It atomically subtracts masks overlapping the selected prefix and adds the declared replacement mask. Its result cursor is the committed administrative mask cursor. A following `sessionContinue` may arm continuation only from that cursor. The pause is released after invocation lifecycle finalization: an armed continuation runs first, then queued submissions drain normally; without an armed continuation, queued submissions drain immediately.

The worker explicitly attempts release after success, skip, helper or action failure, cancellation, shutdown, and lifecycle persistence failure. A non-blocking drop fallback covers an abruptly canceled worker. Once `sessionMask` commits, later continuation, lifecycle persistence, cancellation, or shutdown failure does not compensate or remove it.

Raw timeline reads remain an immutable source-history view. Compact helpers and provider requests use effective materialization: active prefix replacements hide selected completed prompt/turn context and insert one synthetic user replacement at the earliest selected position. Mask mutations are durable administrative events, not timeline items. Explicit mask subtraction reveals preserved source content without rolling back conversation content; rolling the conversation back before a mask mutation removes that mutation from rollback-effective mask history.

Manual and event admissions use the same executor and lifecycle recorder. Event invocations also receive durable public invocation IDs.

Session-originated event adapters record the originating session ID under the same canonical metadata key. Generic events may omit that metadata, and Waltr does not backfill metadata onto existing invocations.

## Automatic Completion Admission

The Routine engine registers one agent-boundary callback for `sessionLifecycle.runCompleted`. This synchronous path does not use the bounded semantic event bus. It considers active catalog Routines in canonical name order and triggers in declaration order, selects at most one eligible invocation for each completed run, and excludes sessions whose `waltr.session.purpose` is `routine-task`.

Runtime trigger values include the session and run identities, terminal cursor, latest completed provider turn's `context.tokens`, and `context.percent` when the completed run's effective configured provider/model resolves to a model with a nonzero context limit. Percent uses overflow-safe integer arithmetic. Unknown model limits produce a missing percent, so only an absolute-token condition can match; missing provider usage produces neither pressure value. The compact-and-continue trigger matches at 180,000 context tokens or 80 percent and materializes its lifecycle assignment as boolean `continue: true`.

Bounded preparation evaluates eligibility without pause authority. For an eligible trigger, the coordinator atomically converts the completion barrier into the invocation pause and invokes the prepared finalizer with that lease. The finalizer captures mask-effective source context and commits durable admission with the exact source session and completed source cursor. This post-claim phase has authoritative completion and is never aborted by the preparation deadline. Persistence failure creates no executable worker and explicitly fails open. Only after a successful commit does finalization return an already-created detached worker that owns the lease. The detached path is the same executor used by manual admission, including helper creation, mask mutation, privileged continuation, lifecycle recording, and pause release.

The coordinator bounds terminal snapshot loading, derivation, and pre-claim preparation to two seconds. No handler, no eligible trigger, pressure below threshold, preparation error or panic, timeout, or shutdown drains normally without ever claiming. Once claimed, delayed capture and durable admission keep queued work blocked until they resolve. Failed admission or finalizer panic removes the pause and drains; successful admission transfers the pause to detached execution. Automatic admission has no retry or persistent completion deduplication; a later completed run may trigger again. During shutdown the engine cancels automatic preparation and unregisters the handler before joining workers, so it starts no new automatic work. Existing admitted workers remain engine-owned and release their pauses through normal shutdown cleanup.

The agent coordinator owns pause, barrier, queue, continuation, and publication lifecycle state. The server supplies only validated semantic reason descriptors across `SessionHost` and `RoutineHost`: compact-and-continue uses code `context-compaction`, the selected Routine name as `source`, and `manual` or `run-completed` as `trigger`; other Routine pauses use `host-operation`. Automatic preparation returns that descriptor in the atomic completion claim, while manual admission supplies it when acquiring the pause. The server does not derive lifecycle state from Routine execution or author final UI labels.

## Durable Lifecycle

Migration `0006_routine_invocations.sql` adds three Waltr-owned tables:

| Table | Role |
| --- | --- |
| `routine_invocations` | Current aggregate, admission order, pinned identity, timestamps, status, terminal failure, and latest event sequence. |
| `routine_invocation_metadata` | Normalized exact-match metadata associated with each invocation. |
| `routine_invocation_events` | Append-only admission, invocation, and task lifecycle journal. |

Waltr stores no separate task table. Invocation summaries and admission records carry immutable normalized string metadata. Admission events are self-contained: they contain that metadata with the immutable Routine name, revision, trigger, correlation, and ordered task descriptors.

Task events contain task results, helper or destination session references, and public failures when applicable. Invocation success events preserve declared result order.

Invocation statuses are `admitted`, `running`, `succeeded`, `failed`, or `interrupted`. Task statuses also include `pending`, `skipped`, and `blocked`.

`ListRoutineInvocations` pages newest-first aggregates with an exclusive admission-sequence cursor. Its metadata map is an exact equality filter: every entry must match, entries compose with AND, and an empty map selects global history. The server applies this filter before pagination limits. `GetRoutineInvocation` reconstructs tasks and results from the aggregate and journal.

Waltr implements no Routine history pruning. Agent session deletion or wipe does not delete Waltr SQL invocation history.

## Watch And Replay

`WatchRoutineInvocations` accepts the same exact-AND metadata filter as listing. An empty map selects the global stream. The server applies the filter before snapshot and replay limits, then supports two modes:

- Without `after_sequence`, it returns a bounded newest-first aggregate snapshot and a durable high-water sequence.
- With `after_sequence`, it returns committed journal events after that cursor in ascending sequence order.

SQL event sequences are opaque, monotonic global durable cursors. PostgreSQL serializes event identity allocation with a database-wide transaction advisory lock, so a lower sequence cannot commit after a higher sequence. Clients must not require adjacent values: rolled-back allocations can leave gaps, and filtered streams normally have gaps because nonmatching events still consume positions in the global sequence.

Process-local wakeups only prompt another SQL query. SQL replay remains authoritative after missed wakeups, lag, disconnect, or reconnect.

The admission event contains enough immutable metadata and ordered task descriptors to create a new client item during replay.

## Restart And Coordination

Startup reconciles every unfinished invocation before the Routine engine reports ready. It records unfinished tasks and invocations as interrupted with `server_restart`. Pauses and queued submissions are process-local, so a crash drops both rather than recovering queued work. The durable mask and recorded source cursor remain.

Waltr does not resume interrupted Routine execution or retry an armed continuation after restart. Graceful shutdown stops admission, interrupts active helper sessions, joins owned work, and records `server_shutdown` interruptions.

Waltr exposes no separate Routine cancellation RPC. Detached event/manual invocations receive interruption through shutdown or restart reconciliation. Interactions also support in-band `cancel` and transport-owned cancellation.

The deployment contract assumes one active Routine engine for each Waltr database. Current traits permit a future external coordination implementation.

Waltr does not implement clustered leases or cross-engine execution ownership. Multiple active engines against one database remain unsupported.

Session list/watch gRPC responses and MCP session output consume the same agent `SessionSummary` progress and capabilities projection.
MCP uses `progress.running` for activity and tool `query` action `session.wait`.
It includes structured progress and capabilities without recombining durable status with legacy coordination.
Durable status and coordination remain present for diagnostics and older consumers.
These capabilities do not authorize Routine admission or determine Routine command availability.

## Shared Client

`waltr-client` owns an interest-counted definition watch, one singleton global invocation history, and independently keyed source-session histories. It preserves interests across recoverable access loss and reconnects interested watches after readiness.

Each invocation history has its own watch lifecycle, durable replay cursor, and health. Reconnect requests replay that history's server-filtered stream after its cursor.

If replay violates lifecycle or identity invariants, the reducer discards the cursor and requests a fresh authoritative snapshot.

The shared client also owns newest-first global history, older-page pagination, bounded detail state, and durable start-operation association. Its source-session histories are a convenience projection over generic server metadata filters, keyed by session ID and filtered with `waltr.routine.source_session_id`.

A manual start operation retains submitted inputs and structured validation failures. Acceptance associates its operation key with the durable invocation ID.

Stable Routine views expose definitions, history, details, watch health, pagination, and start state to both shells.

Session Context panels render `session_histories[session_id]` directly. No client-side selection, deduplication, or helper-owner fallback applies. A helper session therefore sees only invocations directly admitted with that helper as their source session.

## User Interfaces

The global web `/routines` page and TUI Routine list view display definitions, durable history, and pagination.

The web `/routines/:invocationId` page and TUI Routine detail view display one read-only invocation. Both detail surfaces remain separate from their list surfaces.

Neither global surface launches a Routine. Existing session command palettes remain the only web and TUI launch surfaces.

Session palettes render ordered forms and submit starts through `waltr-client`. They report pre-admission failure or durable admission success without execution progress.

After admission, each palette offers close or direct navigation to the accepted invocation's global read-only detail.

Routine helper sessions remain ordinary visible sessions. They provide durable agent execution context and links through recorded session references.

Helpers do not own Routine lifecycle status. The Waltr SQL aggregate and event journal remain authoritative.

Session Context panels show summaries from their server-filtered source-session history and link to global invocation details. They do not load task details, and their bounded snapshot does not represent exhaustive history. Opening a session detail registers its session-scoped watch; closing it releases that watch without affecting other source-session or global watches.

## Boundaries

- Manual triggers use session scope and declare inputs. They have no `completion` field.
- A proposed nested `routineRef` task can still use `completion: wait`; that field belongs to child invocation semantics.
- Waltr provides no resumption, pruning, global launch controls, or explicit cancellation RPC.
- Event delivery before durable admission remains best-effort and process-local. Admitted event invocation history remains durable.
- Automatic `sessionLifecycle.runCompleted` admission is process-local and fail-open; its durable invocation history begins only at successful SQL admission.

## References

| Path | Purpose |
| --- | --- |
| [../../proto/waltr/routine/v1/routine_service.proto](../../proto/waltr/routine/v1/routine_service.proto) | Routine service, definition, lifecycle, history, and replay contract. |
| [../../crates/waltr-server/src/routine/](../../crates/waltr-server/src/routine/) | Engine, canonical executor, lifecycle recorder, events, expressions, and host boundary. |
| [../../crates/waltr-server/src/routine/interaction/](../../crates/waltr-server/src/routine/interaction/) | Internal live-component runtime, stream state, groups, reservations, and adapters. |
| [../../crates/waltr-audio/](../../crates/waltr-audio/) | Google speech library, generated client reexports, and loopback contracts. |
| [../testing.md](../testing.md) | Audio library, strict example, runtime, and SQLite migration checks. |
| [../../crates/waltr-server/src/routine_service.rs](../../crates/waltr-server/src/routine_service.rs) | Durable Routine gRPC service and stream behavior. |
| [../../crates/waltr-server/src/routine_interaction_service.rs](../../crates/waltr-server/src/routine_interaction_service.rs) | Shared interaction preflight, durable admission, and transport ownership. |
| [../../crates/waltr-server/src/routine_interaction_service/](../../crates/waltr-server/src/routine_interaction_service/) | Stream protocol, per-port ingress, transport credits, and focused service tests. |
| [../../crates/waltr-server/src/app.rs](../../crates/waltr-server/src/app.rs) | Common authenticated route composition and explicit standalone limits injection. |
| [../../crates/waltr-server/src/embedded.rs](../../crates/waltr-server/src/embedded.rs) | Embedded composition and explicit limits injection. |
| [../../crates/waltr-server/src/persistence/routine_invocation.rs](../../crates/waltr-server/src/persistence/routine_invocation.rs) | Aggregate, journal, replay, and restart reconciliation store. |
| [../../crates/waltr-client/src/coordination/routine.rs](../../crates/waltr-client/src/coordination/routine.rs) | Shared watch, replay, history, detail, pagination, and start reducer. |
| [config.md](config.md) | Routine resource schema and catalog publication. |
| [persistence.md](persistence.md) | Waltr database ownership and migrations. |
| [clients.md](clients.md) | Shared-client and shell boundaries. |
| [../examples/routines/README.md](../examples/routines/README.md) | Implemented and proposed Routine examples. |
