# Routines

## Purpose

Waltr Routines define server-owned event and manual workflows. The Routine service separates live definitions from durable invocation history.

This document defines the Routine lifecycle, persistence, replay, restart, shared-client, and user-interface contracts.

## Responsibilities

| Layer | Owns |
| --- | --- |
| Resource catalog | Strict Routine source, validation, publication, revision identity, and complete manual-definition projections. |
| Routine engine | Event and manual admission, immutable invocation capture, task execution, helper sessions, and graceful shutdown. |
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

Waltr exposes no explicit Routine cancellation RPC. Interruption currently comes from shutdown or restart reconciliation.

The deployment contract assumes one active Routine engine for each Waltr database. Current traits permit a future external coordination implementation.

Waltr does not implement clustered leases or cross-engine execution ownership. Multiple active engines against one database remain unsupported.

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
| [../../crates/waltr-server/src/routine_service.rs](../../crates/waltr-server/src/routine_service.rs) | gRPC service and stream behavior. |
| [../../crates/waltr-server/src/persistence/routine_invocation.rs](../../crates/waltr-server/src/persistence/routine_invocation.rs) | Aggregate, journal, replay, and restart reconciliation store. |
| [../../crates/waltr-client/src/coordination/routine.rs](../../crates/waltr-client/src/coordination/routine.rs) | Shared watch, replay, history, detail, pagination, and start reducer. |
| [config.md](config.md) | Routine resource schema and catalog publication. |
| [persistence.md](persistence.md) | Waltr database ownership and migrations. |
| [clients.md](clients.md) | Shared-client and shell boundaries. |
| [../examples/routines/README.md](../examples/routines/README.md) | Implemented and proposed Routine examples. |
