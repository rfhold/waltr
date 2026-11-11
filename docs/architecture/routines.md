# Routines

## Purpose

Routines are bounded JavaScript programs coordinated by the server. Standalone and embedded applications register the v2 Routine service. They do not start the legacy task executor or register v1 Routine routes.

## Responsibilities

| Layer | Owns |
| --- | --- |
| Resource catalog | Strict source validation, complete publication, immutable revisions and named agent definitions. |
| `waltr-script` | Isolated JavaScript execution; no ambient filesystem, network, imports or server policy. |
| `InvocationService` | Shared admission, durable execution identity, operation fencing, cancellation, cleanup and quarantine. |
| Host adapters | Normal helper/tool policy, source context, media resources and authenticated authority. |
| Waltr SQL persistence | Invocation/operation history, observations, object references, text ownership/quotas and code-dispatch associations. |
| Immutable object storage | Digest-addressed canonical program envelopes and selected UTF-8 text bodies through filesystem or S3. |
| Shared client and shells | Typed v2 state, independent request ownership, forms, media grants and presentation. |

## Definitions

Resources use `apiVersion: waltr.routine/v1`, `kind: Routine`, a top-level `name` and `description`, and a `spec` containing JavaScript `source`, typed entrypoints, parameter/result contracts, ports, capabilities and budgets. This resource version is distinct from the public RPC version.

The catalog accepts only JavaScript Routine envelopes and validates source and contracts before publication. Initial legacy overrides fail startup; later legacy replacements fail before writes or publication. No task DSL executor, converter, compatibility reader or fallback exists.

`WatchDefinitions` publishes complete source-free v2 snapshots. Revisions select exact programs; history remains interpretable after catalog changes. Capability discovery describes installed host families and configured exact targets, not caller consent. Admission independently checks the current revision, source, named bundles, permissions and applicable leases.

Bundled title, fork and compaction use ordinary named agents. Their prompts live in Agent resources, not inline Routine helper definitions. Configuration uses the ordinary named configuration agent. Permission-evaluation sweeps and deferred proposal/review are not shipped.

## Manual Admission

`StartInvocation` validates an exact manual entrypoint, typed parameters and any required source before durable admission. Missing sources, stale revisions and busy compaction pauses create no invocation history. Correlation is not idempotency: a lost admission acknowledgment must not trigger an automatic retry.

Successful manual admission detaches execution from the request. `WatchInvocations` and `GetInvocation` report durable outcomes; RPC success alone does not establish execution success. `CancelInvocation` is an authorized, idempotent cancellation request, independent of media credit.

One application-owned `InvocationService` serves public RPCs, semantic events, completion compaction and opt-in code calls. These paths do not create another JavaScript executor or bypass the normal MCP/tool pipeline.

## Automatic Completion Admission

Normal gRPC session runs capture the actual originating principal and access lease in their runtime factory. Queued runs retain that factory; different host bindings do not batch together. First-user-message title and completion compaction use this origin, excluding helper/ephemeral sessions.

The agent coordinator owns the atomic terminal barrier, pause, source cursor, continuation and ordinary queue. The reviewed completion adapter adopts the completed-run lease and joins admission, execution and pause cleanup. The JavaScript compaction program selects pressure at 180,000 tokens or 80 percent of a known context limit; missing limits remain unknown. It does not invent usage or retry interrupted work.

Manual compaction pins one completed, mask-effective source. Original history requires a declared capability and independent source-policy `manage-context` Allow. The exact normal manage-context handler can instead supply private evidence of its approved Allow/Ask dispatch. Routine names and public metadata grant no privilege. Later cancellation or persistence failure does not undo a committed mask. Raw timeline history remains separate from effective provider context.

Categorization, context management and category replacement use one immutable source pin and the shared invocation service. Child calls retain the reviewed depth, count, parallelism, capability and deadline limits. Each child owns its helper cleanup. Capture contracts accept only their exact normal named helper and typed capture tool; the caller's normal permission policy also applies.

### Generic Context And Routine Policy

The host exposes `context.snapshot`, `context.original`, `context.history`, `context.metadata`, `context.transact`, `context.create`, `context.start` and `context.continue`. Invocation-local source handles retain provenance and capability ceilings. Session IDs, hashes and metadata cannot fabricate those handles.

JavaScript owns title absence rules, fork messages, inheritance/history choices, compaction thresholds, replacement keys and overlap policy. Creation checks source freshness against the immutable capture immediately before the destination transaction. That check linearizes the source read; no cross-record transaction exists. Destination creation and exact initial-message persistence are atomic. Start consumes its one-shot identity before the effect; uncertainty never permits replay. Pause ownership and exact committed-cursor continuation remain coordinator responsibilities. Continuation precedes queued work; cleanup joins the owner.

The bundled category programs define `routine.categories.v1` session checkpoints, `routine.category.v1` item annotations and `category.<key>` masks. These names and schemas are JavaScript policy, not Rust contracts. Categorization validates semantic coverage and merged descriptions before one metadata transaction. Context management runs parallel plan-only children, then applies one aggregate metadata-predicate/mask CAS. JavaScript selects background excerpts, currently the last 16 items at 240 characters each. Generic stores enforce identity, revision, incarnation, metadata predicates and masks atomically; they do not parse category checkpoints.

Routine source CAS retains the public complete-record revision, strict context digest, durable incarnation and pre-state metadata/mask predicates. Execution-token equality cannot replace these checks. Metadata-only transactions advance the public revision without invalidation of live execution; a concurrent title remains compatible with the parent's first response. Combined metadata/mask transactions advance both domains once. The completed-source matcher retains its explicit mask-commit allowance; pause ownership, continuation cursors and committed receipts remain mandatory. Routine program revisions and observation cursors remain separate namespaces. See [agent revision domains](../agent/storage.md#revision-domains-and-append-receipts).

Origin-bound semantic consumers use post-commit generic turn, tool-result and run-finished facts plus host-confirmed idle state. Trusted server-local `SessionRoles` binds helper, ephemeral and task provenance to actual session IDs and incarnations. Public `task.*` metadata remains audit data. A private creation sink prevents automatic helper triggers before classification. Twenty turns or two tasks request categorization; idle uses live boundary markers. Lag resets counters without hydration replay. `InvocationCoalescer` binds exact source identity, Routine revision, entrypoint and execution ID. Reuse requires joined known success, a context-owned generic commit and current revision/incarnation, never category proof interpretation. Each consumer stops on its original lease and joins its admitted work on shutdown.

`manage-context` and `configure-waltr` are normal permission-controlled session tools. Direct configuration requires a source whose current normal policy explicitly allows `configure-waltr`; no global manual offer exists. Both paths intersect the named helper's own permissions with the actual caller policy. Authentication and the configuration agent's own grants do not supply caller authority.

## Interaction Catalog And Runtime

### Admission And Transports

Native `Interact` and browser split-lane attachment RPCs use the same service, policy, media factory, SQL admission and resource registries. Native duplex is not a browser transport. Browser input/control and live output remain separate bounded lanes.

### Speech And Text

Media adapters own readers, writers, helper input, blobs, text and speech work through cancellation and join. Speech resolves each operation's JavaScript selector or catalog default; an unconfigured required speech direction fails closed. Non-speech media does not select a cloud provider implicitly. JavaScript selects normal helpers through explicit `helper.run` arguments: selection, task, capture, source, turns, deadline and response/completion budgets.

`spec.captures` maps names to exact named selections and bounded `waltr.value/v1` contracts. Declaration grants no `_helper_capture` permission. Preflight rejects definite capture denial before media or speech effects; Ask requires normal live consent. Named-agent permissions and applicable caller/source restrictions control capture. Payloads use the fifth `host.call` argument: an invocation-local committed resource. `media.select` selects the whole capture or a JSON pointer and retains helper provenance.

Conversation notes require diarization-capable recognition and publish retained transcript, notes and deletion audit separately.

Each invocation pins one catalog snapshot. Speech calls accept `speech: {from: 'defaults'}` or exact `{provider, model}` selections. Omission selects the operation default: `spec.speech.stt`, `diarize` for `snapshot: true`, or `tts`. Exact TTS selections also accept `voice`, `language`, and `rate`; absent options use catalog synthesis settings. The host validates model kind, streaming support, snapshot diarization, and PCM before provider effects. Missing defaults fail only operations that require them. No fallback joins the ordinary and snapshot STT defaults.

`helper.run` accepts `inference: {from: 'defaults'}`, `{from: 'source-session'}`, or exact `{provider, model}`. Omission uses `spec.inference`. Source selection copies the authorized session's inference snapshot and fails without a source. Source and exact selections accept `reasoningEffort`; `null` explicitly disables reasoning. The title Routine uses this override. Context copy and fork destination inference remain separate contracts.

Helpers accept `input: {text: true, maxBytes: 8388608}`. The server enforces this 8 MiB maximum and validates model implementation capabilities. JavaScript declarations grant no image or PDF support, tools, credentials, or capture permission. The host resolves credentials from the pinned catalog. Catalog reloads affect subsequent invocations.

Capture, playback and codec conversion belong to clients. Opaque upload/read grants do not expose native paths. Supported image/document inputs remain PNG, JPEG, WebP, PDF and UTF-8; video is excluded. Output leases are bounded, non-renewable and do not authorize replay. Durable selected text uses the accepted separate text-result budget, not an unbounded protobuf result.

### Bounds And Durable State

The accepted transport, stream, workspace, result and lease budgets remain enforced by their owning registries. The obsolete server `interaction_limits` constructor field is removed; v2 registries and typed resource contracts own these bounds.

Routine blob storage receives an explicit workspace topology. Local and embedded workers require a canonical local workspace and reject directory identity overlap. The private worker handshake verifies the socket worker's absolute workspace identity. That directory need not exist on the server. Host-side exclusion resolves present path prefixes and rejects storage path overlap or aliases even when the workspace is absent. Private blob operations remain descriptor-rooted. Both modes enforce ownership, ancestor modes, no-follow opens, exclusive locks, empty-root checks and quotas.

### Interaction Trace Ownership

Invocation and operation history is authoritative for execution and cleanup. V2 admission uses `routine.v2.start` and `routine.v2.interact` spans. The original owned driver retains its `routine.invocation` span after admission returns; `routine.host_call` spans carry that explicit parent across the script boundary. Incoming trace context is validated and tracestate is cleared at the Routine boundary. Library composition installs no additional global subscriber or exporter. Never include credentials, source, helper inputs/results, native paths or media payloads in diagnostics.

## Code

`spec.features: [{codeMode: true}]` opts a normal agent into the optional code tool. The host additionally configures the exact principal, durable session incarnation, profile and normal permission intersection. An absent feature adds no tool. No principal-scoped code adapter is shared globally across callers. Public MCP code exposure remains optional and is not enabled by default.

For hosted MCP, the generic context carries the validated opaque subject and original token lifetime proof. Host configuration matches that subject and configured issuer; metadata cannot supply authority. `RoutineCodeGrant.public_mcp_session` may select one explicitly granted, opted-in durable session for public code. Discovery is principal-specific and calls bind a fresh adapter through that session's normal runtime. No guest session or profile selector is accepted. Ordinary admitted session create/send prompts are durable and do not inherit access-token expiry cancellation. Explicit code/helper/Routine leases retain exact cancellation and join ownership; concurrent origins do not replace each other's leases. Interrupted code retains sticky uncertainty, not success or replay.

Reviewed concrete tool calls use the server's shared exact-identity `CapabilityDispatcher`, including normal policy, middleware, worker Prepare/authorize/Execute, and operation-bound interactions. They do not detour through a model. Private approval waiters are not JavaScript journal state and cannot be replayed after restart. Authenticated public MCP `capability__*` tools are instead sessionless full-access dispatch over enabled capabilities; this does not grant optional code execution or bypass upstream auth and sandbox checks.

Code execution uses the same service and normal authorized executor. Its durable outer dispatch association precedes effects. Lost acknowledgments, restart and template execution do not replay source. Helper consent remains attached to the original live helper and current origin guard.

## Durable Lifecycle

Embedded MCP session creation and submission use a typed origin installed only after exact local bearer validation. Its lifetime is the current application generation; shutdown cancels and joins its normal run owners. No hosted OAuth identity is fabricated, and default code grants remain denied.

The `script_*` stores own JavaScript invocation, operation, observation, text-result and code-dispatch state. Additive migration 0014 drops only the legacy `routine_invocation_events`, `routine_invocation_metadata` and `routine_invocations` tables, children first and without `CASCADE`. New JavaScript history remains intact, as do session files and masks, configuration, browser/outbound OAuth state and the separate MCP store. No legacy task-history decoder remains. This source change does not authorize a live deployment or conversion. The unshipped deferred migration 0012 is removed without renumbering; retained local databases that applied it need separate migration-history handling.

Migration 0016 removes active SQL program/text body columns only after startup preflight and SQL empty-state guards reject populated incompatible state. No backfill, converter or automatic wipe exists. Retained deployments require separately approved archive/reset targets before this transition. Credentials, access state, hosted MCP state, sessions, workspace files, current object namespaces and unrelated dispatch receipts stay outside that scope. See [persistence](persistence.md#routine-payload-objects) and the [cutover runbook](../runbooks/server-capability-cutover.md).

Admission captures immutable identity and public definition data. Results and operation outcomes are bounded; raw media chunks are not durable execution replay. Ambiguous effects or cleanup remain uncertain/quarantined rather than being labeled successful.

## Watch And Replay

Observation cursors are opaque durable identities. Initial history is bounded; selected history/detail requests are independent of the recent watch window. Wakeups prompt authoritative SQL reads rather than replacing them. Reconnection replays observations, not JavaScript, helper work or media.

## Restart And Coordination

Startup reconciles unfinished work before readiness. Readiness also checks the shared invocation gate, catalog coordination and sandbox health. Catalog coordination failure closes admission and reports failure rather than continuing under a stale executable catalog.

Shutdown first stops admission and semantic/code scheduling. It cancels and joins attachment/read/media owners and the shared invocation service, then joins completion/pause coordination before normal sessions and sandbox teardown. Cleanup uncertainty remains an error. The accepted filesystem blob registry permits one process owner; independent application replicas need separate processes.

## Shared Client

`waltr-client` consumes frozen v2 definitions, history, operations and attachments. Source-session selection is an explicit server filter, not a client-side helper-owner guess. Recent and selected pages have separate request identities. Access loss fences commands and local media effects without turning observation reconnect into execution retry.

## User Interfaces

Web and TUI render shared snapshots and dispatch shared commands. Parameter drafts retain their JSON bytes; schemas own validation. Native media brokers own trusted local selection, digest/length checks and no-overwrite staged saves. Durable text reads remain separate from live output.

## Interpreter And Image Isolation

`waltr-script` retains its QuickJS budget patch. Upstream WASI disables configured C-stack checks; public allocator callbacks miss internal heap-limit refusals. Wasmi traps do not supply equivalent native quota provenance. Cumulative fuel, bounded memory, owned host futures, worker join and no native fallback remain mandatory. See the [script budget contract](../../crates/waltr-script/README.md#budgets).

Image blobs retain a 20 MiB encoded-byte cap and exact PNG/JPEG/WebP MIME allowlist. Commit checks only the fixed PNG signature, JPEG SOI, or WebP RIFF/WEBP signature. The host preserves the original bytes; providers own full decoding and report encoding errors through ordinary provider failures. A malformed body with an allowed signature can pass. The host reports no dimensions and provides no decoded-pixel limit or decompression-bomb protection. Optional wire image metadata remains unset. Blob ownership, quotas, cancellation, and cleanup still apply.

## Boundaries

- Application-domain membership is not concrete tool consent.
- Default-off code mode does not create service identities.
- No automatic replay, backend fallback, video support or live deployment/reset is provided by composition.
- All eleven bundled Routine resources and matching examples use strict JS envelopes and normal named helpers. No permission-evaluation sweep is shipped. Resources own helper selection and capture schemas; hosts own model ceilings and context grants. An example grants no authority.

## References

| Path | Purpose |
| --- | --- |
| [v2 protocol](../../proto/waltr/routine/v2/routine.proto) | Public discovery, execution and transport contracts. |
| [Production composition](../../crates/waltr-server/src/routine/production/) | Shared constructors, authority and adapters. |
| [RPC adapters](../../crates/waltr-server/src/routine/rpc/) | Native/browser authority and observation boundary. |
| [Application](../../crates/waltr-server/src/app.rs) / [embedded](../../crates/waltr-server/src/embedded.rs) | Production route registration and lifecycle. |
| [Agent](agent.md) / [clients](clients.md) / [persistence](persistence.md) | Adjacent ownership contracts. |
| [Testing](../testing.md) | Layered verification and disposable fixtures. |
