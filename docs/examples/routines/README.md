# Routine Examples

These Tekton-inspired files are Waltr Routine resources, not Tekton resources. The server executes [`title.yaml`](title.yaml), [`compact-and-continue.yaml`](compact-and-continue.yaml), and [`fork.yaml`](fork.yaml). Audio fixtures have implemented catalog, runtime, and RPC support. Other fixtures and unsupported syntax remain proposals.

| Fixture | Status | Purpose |
| --- | --- | --- |
| `title.yaml` | Implemented | Generate session title metadata from the first user message. |
| `compact-and-continue.yaml` | Implemented | Replace a session prefix with a continuation summary. |
| `context-categorization.yaml` | Proposed | Incrementally assign timeline items to semantic categories. |
| `context-management.yaml` | Proposed | Select category compaction changes in one agent turn. |
| `fork.yaml` | Implemented | Compact a durable source boundary into a new ordinary session. |
| `manage-category-context.yaml` | Proposed | Re-evaluate and optionally replace one category mask. |
| `permission-evaluation.yaml` | Proposed | Review replied permission prompts on a daily schedule. |
| `configuration.yaml` | Proposed | Run a named configuration agent from a session tool. |
| [`audio-transcribe.yaml`](audio-transcribe.yaml) | Implemented; requires compatible providers | Transcribe client audio with live revisions and committed transcript segments. |
| [`audio-transcribe-cleanup.yaml`](audio-transcribe-cleanup.yaml) | Implemented; requires compatible providers | Collect all committed transcript text, then stream one cleanup agent's text. |
| [`audio-conversation-notes.yaml`](audio-conversation-notes.yaml) | Implemented; requires diarization-capable speech provider | Retain all substantive topics, audit deletions, then produce notes with two helpers. |
| [`audio-synthesize.yaml`](audio-synthesize.yaml) | Implemented; requires compatible providers | Synthesize committed text into PCM audio. |

## Implemented Title Routine

- The unified catalog registers strict `Routine` resources at `routines/<name>.yaml`.
- The bundled [`title` resource](../../../crates/waltr-server/resources/routines/title.yaml) and this example use identical source.
- The exact `waltr.session.user-message.created` event fires best-effort after each durable user prompt.
- The title trigger selects the first message and excludes sessions whose declared `waltr.session.ephemeral` metadata is `true`.
- The task creates a visible durable auxiliary session. It copies only provider connection and model settings, then disables reasoning.
- The inference-only template creates a durable helper with only its task prompt. Empty feature, tool, and permission lists isolate source agent behavior.
- `maxTurns: 1` limits the task to one provider turn. The host uses the exact final text without normalization.
- The `sessionMetadata` action sets source metadata key `title` only when absent. It never overwrites a prior value.
- Source work does not await the routine and cannot fail because of routine delivery or execution.
- Title event delivery has no backfill, replay, recovery, retry, or enable flag.
- Duplicate-attempt suppression before admission is bounded and process-local. Admitted lifecycle history is durable.
- Shutdown interrupts auxiliary children and records unfinished invocation and task states as interrupted.

The delivery limits above apply to the title event path. Manual invocation uses the generic service described below.

## Implemented Compact-And-Continue Routine

- The server bundles [`compact-and-continue.yaml`](../../../crates/waltr-server/resources/routines/compact-and-continue.yaml) byte-for-byte from this example.
- Manual invocation rejects an active or already paused source before admission, creates no history on that rejection, and defaults omitted `continue` to `false`.
- The pause pins the latest completed source boundary and queues later submissions in a process-local FIFO. A successful continuation runs before that queue.
- The helper receives mask-effective completed-turn context through the pause cursor, uses no tools, and runs for at most one provider turn.
- The atomic `sessionMask` action removes active replacement masks that overlap the selected prefix, then adds one persistent replacement under key `context.compaction-prefix` without rewriting source events.
- Raw timeline APIs remain raw. Provider requests and Routine effective captures hide the selected prefix and insert the summary as one synthetic user message.
- Explicit subtraction can remove a mask without rolling back conversation content. A conversation rollback before a mask mutation removes that mutation from effective history.
- A successful mask remains committed if continuation or later Routine lifecycle work fails. The owned pause is still released.
- Automatic admission runs after a durable source run completes at 80 percent of a known effective model context limit or 180,000 latest-turn context tokens, whichever matches first. With an unknown limit, only the absolute threshold can match; missing usage matches neither.
- Automatic completion admission excludes `routine-task` helper sessions and fails open after two seconds or on callback, capture, shutdown, or durable-admission failure.
- Pauses and queued submissions disappear on process crash. Durable masks and admitted invocation history remain, but interrupted Routines are not resumed and continuation is not retried after restart.

## Implemented Resource And Task Subset

- Parameters use the closed string and boolean scalar types. A string parameter can require a value, set `minLength`, and use a bounded Rust regex pattern.
- Each trigger declares exactly one event, manual, or `sessionLifecycle.runCompleted` form. Implemented manual and lifecycle triggers use session scope and declared parameter assignments.
- Top-level `spec.sourceSession` supports manual or `runCompleted` triggers. A paused source requires manual `onBusy: reject`; pause-backed lifecycle admission claims the completion barrier.
- Inference-only event Routines remain supported, including the bundled title Routine.
- Event types are exact strings. Conditions support `In`, `NotIn`, numeric `GreaterThanOrEqual`, nested `any` OR groups, and top-level AND semantics.
- `runAfter` defines an acyclic task graph. The current executor processes ready tasks sequentially.
- A task contains either a generic agent template or one implemented action. The actions are `sessionMetadata`, the fork subset of `sessionCreate`, compaction-prefix `sessionMask`, and pause-owned `sessionContinue`.
- Agent tasks require replacement prompts, explicit features, tools, permissions, `maxTurns`, and text output.
- Each agent task creates a fresh durable auxiliary Routine session. Actions do not create an auxiliary agent session.
- Every source-backed `sessionTemplate` atomically stores copied inference history followed by its task prompt. An inference-only template stores only its task prompt.
- Scalar substitutions resolve event, manual, lifecycle pressure, source pause, task result, text output, and implemented action fields within fixed size bounds.
- Declared string and boolean results expose those scalar values to dependent tasks and routine results.

## Implemented Routine API

- `WatchManualRoutines` streams complete global typed definition snapshots with stable digests.
- `StartManualRoutine` returns a durable invocation ID or a structured pre-admission validation failure.
- Accepted execution detaches from the RPC. Manual and event invocations use the same durable lifecycle recorder.
- History supports snapshots, pagination, detail, and SQL event replay after an opaque durable cursor.
- Definitions preserve declared input and result order. Admission pins the Routine revision and ordered task descriptors.
- A request ID correlates traffic only. It provides no idempotency, retry, or duplicate suppression.
- See [Routines](../../architecture/routines.md) for restart, replay, persistence, client, and UI contracts.

## Implemented Audio Interactions

See [Routine Audio Interactions](audio-interactions.md) for catalog syntax and the wire contract.
See [Routines](../../architecture/routines.md#interaction-catalog-and-runtime) for runtime semantics, shared admission, and bounds.
`RoutineInteractionService` implements native bidirectional `Interact` and bounded-upload, server-streamed `Run` for gRPC-web.
The detached `StartManualRoutine` contract stays unchanged.
The [desktop client example](../client.yaml) selects the cleanup fixture's exact identifiers and PCM format.
See [desktop configuration](../../architecture/config.md#desktop-client-configuration) for the separate local schema and trust boundary.

The [conversation notes fixture](audio-conversation-notes.yaml) adds two sequential one-turn helpers after transcript collection.
The first returns the full retained transcript and a separate deletion audit as JSON string fields.
The second receives both values and checks deleted content for context or mistaken exclusions.
Only the second helper feeds the `notes` text port; `deleted_items` remains a separate top-level Routine result.
The [clipboard-only desktop example](../client-conversation-notes.yaml) copies final notes with `/usr/bin/pbcopy`; it never types or pastes.
See [conversation notes prerequisites and limits](audio-interactions.md#conversation-notes) before use.

All four audio fixtures pass the strict Routine parser. Complete catalog publication also requires their explicit provider/model references: `speech/dictation`, `speech/diarized-dictation`, `inference/cleanup`, and `voice/reader`, as applicable. Speech settings must match the declared PCM format; the catalog does not probe an upstream endpoint.
The cleanup fixture explicitly collects **all final committed transcript text** before **one inference-only agent invocation**.
It does not concatenate hypotheses or invoke an agent per chunk.
Its prompt preserves language and tone, removes clear STT noise, and returns only clean text.
Local code support establishes no release, deployment, or real upstream-model compatibility.

## Proposed Resource And Task Model

The bullets below describe fixture syntax beyond the implemented subset.

- Proposed `$(...)` substitutions add captures, loop values, and default behavior beyond the implemented scalar references.
- `spec.taskDefaults` supplies routine-wide session, feature, tool, and
  permission settings. Ordinary tools default to none, and a task's `tools`
  value is an exact allowlist in addition to tools generated by features.
- `routineTools` registers routines with `tool` triggers as eligible tool
  targets. `defaultPermissions: false` keeps them out of the ordinary model tool
  surface unless another feature or permission exposes them.
- `deferredTools` exposes a `defer` tool whose validated input names one target
  tool and its arguments. Permissions on `defer` constrain eligible targets. A
  valid call is persisted for user review instead of being executed or opening a
  permission prompt. The agent receives a deferred-request receipt and continues
  immediately.
- A task with `taskSpec.script` performs a deterministic tool call without
  creating an agent session. Script tasks use the routine's task-default tool
  surface and permissions, expose typed results, and fail when the call fails.
- A task with `taskSpec.routineRef` invokes a named routine with supplied trigger
  context and parameters. `completion: wait` keeps the parent task pending until
  the child invocation, including any coalesced follow-up needed to cover its
  requested snapshot, completes. Declared task results select child outputs
  from `$(routineRef.results.<name>)`.
- A declared capture is added as a task-local structured result tool. Its schema
  validates arguments, and `finishAfter` can complete the task after an accepted
  call. Capture calls count toward `limits.maxToolCalls`.
- `limits.maxToolCalls` and `$(captures.<name>)` extend the implemented `maxTurns` and text-output subset.
- Proposed `forEach` creates one task run per array item. Downstream tasks
  can inspect those runs through `$(tasks.<name>.taskRuns)` or select all run
  results with `$(tasks.<name>.taskRuns[*].results)`. A gated fan-out has no task
  runs; callers that always expose the selected results declare an empty-array
  default.
- `valueFrom` derives a typed result from ordinary structured data and leaves
  session context unchanged. Its array form filters objects by one field and
  selects one field from every match. Category resolution uses this generic
  transform to turn assignments into explicit timeline item IDs.

## Proposed Triggers And Concurrency

- Event names beyond `waltr.session.user-message.created`, schedules, progress counters, and concurrency policies remain proposed.
- Manual trigger scopes beyond session scope remain proposed.
- Permission evaluation proposes global scope.
- Additional `sessionLifecycle` events remain proposed. Categorization uses the
  proposed `idle` event to flush uncategorized items immediately after the source
  run becomes idle.
- `sessionProgress` evaluates counters since a named checkpoint. Categorization
  can therefore run during a long source run after 20 completed turns or two
  completed calls to the `task` delegation tool. The immediate idle flush handles
  any remainder rather than replacing those mid-run thresholds.
- The categorization concurrency key permits one run per source session.
  `coalesce` folds triggers that arrive during that run into a later evaluation.
  A waiting `routineRef` invocation remains pending until the coalesced run has
  categorized through the cursor that invocation supplied.
- Categorization triggers require a non-ephemeral source session, preventing
  auxiliary routine-task sessions from recursively categorizing themselves.
- A `cron` trigger includes an IANA timezone. Permission evaluation uses
  `0 0 * * *` in UTC, a one-day timeframe, an empty-run preflight check, and
  forbids overlapping global runs.
- A routine exposed as a session tool blocks that tool call until completion.
  Its invocation cursor is a stable source snapshot boundary. Access to the
  generated tool is governed by the calling agent's effective permissions.
- The session-aware `CursorBefore` operator remains proposed. A null cursor is
  proposed to sort before the first session item. Missing values and evaluation
  errors make implemented conditions false.

## Proposed Session Materialization And Categories

- Snapshot selection beyond fork's raw latest-completed capture and compaction's
  effective paused-prefix capture remains proposed.
- Category-oriented `sessionTemplate.source` mask overrides and provenance remain proposed.
- The implemented persistent-mask subset is replacement masks selected by a
  completed-turn prefix (`throughCursor`), stable keys and opaque IDs, overlap
  subtraction, overlap rejection, effective replay, and explicit removal.
- Filter, truncation, annotation, exact item-set selectors, category selectors,
  and per-template additive/subtractive mask composition remain proposed. Safety
  policy remains separate from persistent masks.
- Timeline item IDs use Waltr's canonical `iX` form, such as `i1` or `i42`, and
  must match `i[1-9][0-9]*`. The number is the cursor of the item's first durable
  event. An item can span later events, so its item ID is stable identity while a
  cursor remains a timeline boundary; the two are not interchangeable.
- Categorization subtracts all inherited masks, filters to items after its
  previous checkpoint, and annotates each item with its canonical `iX` ID for
  structured assignment. Categorization output and item selectors consume those
  same IDs.
- Exact item-identity masks require source materialization facts with canonical
  timeline IDs. Legacy sessions without those exact links fail masked
  materialization clearly; ordinary run and resume behavior remains available.
- Categorization runs its agent and metadata-write tasks only when the saved
  `throughCursor` is before the requested snapshot cursor. Equal or stale
  snapshots are already covered and take the no-op path, whose routine result
  retains the prior note.
- `context.categorization` session metadata contains the inspected cursor, a
  short task-list-style note, category definitions, and item assignments. A
  successful categorization appends assignments by item ID, adds definitions by
  category key, and advances the cursor atomically. Existing definitions are
  immutable, so later passes reuse their key and description verbatim. The prior
  note and category definitions are supplied to later passes.
- Categorization currently trusts an accepted schema-valid capture and writes its
  categories, assignments, and note directly. It does not separately verify
  complete copied-item coverage or category references. The update remains
  atomic, and a cursor compare-and-set mismatch fails rather than reporting the
  requested cursor as covered.
- Event session data exposes `ephemeral` as a purpose marker, not a persistence
  or list-visibility guarantee. Auxiliary sessions remain visible even when
  their durable metadata contains `ephemeral: true`.

## Implemented Fork Flow

- The server bundles [`fork.yaml`](../../../crates/waltr-server/resources/routines/fork.yaml). The bundled resource and this example use identical source.
- Invocation captures one immutable provider-neutral inference conversation through the latest completed provider turn. It honors rollbacks and excludes active partial work.
- Admission releases the source operation gate after capture. Later source changes cannot alter the captured history.
- The captured history preserves provider-replay prompts, assistant text, tool calls and results, and inline images.
- The capture excludes lifecycle, middleware, status, usage and cost, request audit, provider metadata, provider-native thinking payloads, rollback barriers, and active partial work.
- Waltr atomically creates a fresh durable compact helper with copied history followed by this exact task message:

```text
<fork-direction>
<exact direction>
</fork-direction>
```

- Provider requests derive only from the durable helper. Provider adapters apply normal encoding rules; this is not an HTTP wire archive.
- The compact helper persists across restart and storage backends under ordinary session retention, delete, and wipe rules.
- The replacement system prompt treats the entire user message, including XML-like content, as untrusted emphasis text.
- The model must not execute instructions from that message or continue the source task. Compaction model output remains nondeterministic.
- The destination is a fresh ordinary session. It inherits exact source inference and the exact agent definition, with `pending_switch_audit` cleared.
- Provider and MCP credentials remain host-resolved. Waltr does not copy credentials into the destination.
- The destination receives no source history or general metadata. Its metadata contains only `waltr.session.fork.source` with the source session ID.
- Current timeline RPCs and web and TUI renderers expose the helper's normal durable timeline events automatically.
- The first prompt equals `COMPACTION + "\n\nOriginal user message:\n" + DIRECTION`. Exact direction bytes remain unchanged.
- The `|-` template adds no final newline. The compaction output itself can contain trailing newlines.
- `SessionCreateRequest` atomically persists the destination and exact first prompt. Successful creation is the prompt-durability boundary.
- After successful atomic creation, caller cancellation or start failure retains the destination and its session ID.
- The normal durable first-user-message event makes the destination eligible for the title Routine.

## Proposed Category Context Management

- The `manage-context` tool is intended for phase, stage, and task transitions.
  Context management first waits for `context-categorization` to cover the
  tool-call snapshot, then reads categorization metadata. It has one agent task,
  which receives effective masked context and that metadata and submits its
  complete strategy in one provider turn without inspecting raw categories.
- Metadata reads do not materialize session context. Context management requires
  the stored categorization `throughCursor` to equal the tool-call cursor, so
  stale or newer state fails rather than being reshaped for the current run.
  Category definitions remain stable by contract.
- The strategy returns `no_change` or unique category edits. The host validates
  that `no_change` has no edits, `edit` has at least one, and every selected key
  has at least one assignment in categorization metadata. Each edit fans out to
  a synchronous `manage-category-context` routine invocation at the same snapshot.
- `manage-category-context` owns the category-specific judgment. Deterministic
  tasks resolve its category key to explicit item IDs and read the active keyed
  mask. Its materialization subtracts masks that cover those IDs and adds a
  truncation mask for every other item; it does not read category metadata. The
  child agent receives the selected original items, truncated context from other
  items, and the active mask separately. The named truncation profile is resolved
  by the executor to concrete values before durable materialization. The child
  independently returns `keep` or `replace`; `keep` is the
  no-op escape hatch when an existing active summary already has the requested
  detail and is invalid when no active mask exists. Category resolution requires
  at least one matching item ID, so an empty mask cannot be created.
- A child `replace` applies the keyed mask, while `keep` returns `applied: false`
  and a null mask ID. Context management collects every child routine result so
  callers can distinguish selected categories from masks actually changed.
- A category replacement mask is keyed by category and selects an explicit
  item-ID set. It preserves source items. Persistent replacement-mask overlap is
  rejected. A same-key replacement requires the expected current mask identity.
  The replacement then atomically supersedes that mask.
- Raw gRPC timeline APIs remain raw and are outside these fixtures. Hosted MCP
  currently has no timeline action. Future access options compile into
  request-local masks and default to effective safe context.

## Proposed Permission And Configuration Flow

- The `waltr` harness action `sessions.permission-prompts` returns permission
  prompts across sessions. Its filters select replied prompts in a duration
  window ending at an explicit `through` timestamp.
- Permission evaluation's preflight script task calls that action with
  `filter: length`. Cron skips the agent session when the count is zero. Manual
  runs expose the same check but default it to false, allowing deliberate empty
  evaluations.
- The evaluation prompt uses an `@tool(waltr, {...})` template with the same
  timeframe and `$(routine.startedAt)` boundary to insert the full prompt list
  before the first provider request. Template-owned tool calls use the same
  input-validated permission path as ordinary calls. The evaluator's permission
  matches the action, replied status, timeframe, and boundary exactly, so it
  cannot widen the cross-session query.
- The evaluator can call `defer` with `tool: configure-waltr` when a narrow
  configuration change is warranted. Its input permission allows only that
  target and queues the exact validated call for separate user review without
  opening a permission prompt. Approval executes that call once; rejection
  leaves it unexecuted. The eventual status and result remain on the queue record
  and do not reopen the completed evaluation session.
- Permission evaluation returns only the evaluator's final text report. It
  includes a deferred-request ID when configuration was queued. Skipped runs
  synthesize a text report without creating an agent session. Evaluations are
  not checkpointed or otherwise recorded by this fixture.
- `configuration.yaml` is exposed only as the `configure-waltr` session tool. Its
  single `prompt` input is an abstract description of the desired change.
- The configuration task references the named `configuration` agent definition
  instead of embedding that agent's system prompt, tools, limits, or policy in
  the routine. The definition is resolved when the routine task session starts.
- Permission evaluation keeps its narrow evaluator prompt, scoped `waltr`
  permission, `defer` input permission for `configure-waltr`, and twelve-turn
  limit in the routine task.
- The routine returns the configuration agent's final assistant text directly.

The proposed examples intentionally leave `PipelineRun`-style invocation resources,
event deduplication, retries, approval presentation, category and metadata size
limits, schedule recovery, and durable routine-run recovery unspecified. Compact-
and-continue mask retention, explicit removal, rollback, and restart behavior are
implemented as documented above.
