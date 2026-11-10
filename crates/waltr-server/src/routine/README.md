# Versioned script Routine service

## Behavior

The API validates complete JavaScript modules and executes immutable revisions through explicit host ports.
Standalone and embedded production constructors install one shared service through [production](production.rs), using the normal auth, session, helper, tool-policy, and media boundaries. Only strict JS resources are accepted. The task parser/executors, v1 package and task-history decoder are removed. Additive migration 0014 drops only the three old task-history tables, preserving current script and unrelated stores. This source change is not a live deployment or conversion.

```yaml
apiVersion: waltr.routine/v1
kind: Routine
name: echo
description: Return the supplied text.
spec:
  source: |
    export default async function(input) {
      return input.text;
    }
  entrypoints:
    run:
      trigger: manual
  parameters:
    schemaVersion: waltr.value/v1
    schema:
      type: object
      properties:
        text: { type: string, maxLength: 128 }
      required: [text]
  result:
    schemaVersion: waltr.value/v1
    schema: { type: string, maxLength: 128 }
  capabilities: []
  ports: {}
```

`Definition::parse` checks the strict envelope, bounded contracts, and engine syntax without guest evaluation or host effects.
Unknown fields, old task definitions, unsupported versions, and unsupported schema keywords fail validation.
The engine rejects module imports; definitions contain inline source, never source paths.

`waltr.value/v1` is a closed dialect, not JSON Schema.
It supports null, boolean, finite bounded number, byte-bounded string, homogeneous bounded array, closed object, and a nullable wrapper.
`{type: nullable, schema: {type: string, maxLength: 128}}` accepts only null or a bounded string.
The wrapper and its child each consume a schema node; the child is one level deeper within the same depth-16/node-256 budget.
Nullable does not make an object property optional: `required` still applies. Unknown wrapper fields are rejected.
Number schemas require `min` and `max`; array schemas require `maxItems` and `items`.
Object schemas require `properties` and `required` and reject every undeclared key.
`maxLength` counts UTF-8 bytes, not characters.
Complete values also pass the engine's depth, node, byte, and exact-number checks.

## Session workflow adapters

### Configuration

[Configuration](../../tests/fixtures/routines/configuration.json) uses the normal named configuration agent.
The `configure-waltr` tool requires a source whose normal permission policy explicitly allows it.
The helper intersects its own policy with the caller restriction before execution.
Configuration remains independent of the removed permission sweep and deferred proposal/review feature.

Focused configuration and catalog coverage lives in [configuration tests](adapters/configuration_tests.rs).
See [repository testing guidance](../../../../docs/testing.md) for shared prerequisites.

`tests/fixtures/routines/` contains complete immutable workflow envelopes, plus native attachment examples.
The bundled catalog and matching eleven shipped examples use strict JS definitions. Fixture execution alone does not establish every production workflow or confer host capabilities.
`context.rs`, `children.rs`, and `triggers.rs` expose consumer ports; `adapters/context.rs`
forwards generic history, opaque metadata, atomic masks, destination and pause operations to the agent.
`manage_context_descriptor()` returns metadata only, with no tool registration or implicit permission.

Context pins include durable incarnation, completed boundary, agent/inference identity, privacy view,
and host authority. JavaScript sees lossless cursor strings, never authority tokens or numeric grants.
Legacy records without an incarnation fail read-only preflight; explicit authorized initialization is separate.
The context vocabulary is `context.snapshot`, `context.original`, `context.history`, `context.metadata`,
`context.transact`, `context.create`, `context.start` and `context.continue`.
Source handles remain invocation-local and provenance-bound. Original history requires its declared capability
and independent source-policy manage-context Allow, or private evidence from the exact approved normal tool dispatch.
Routine names and public metadata grant no original-history authority.

JavaScript defines `routine.categories.v1`, `routine.category.v1` and `category.<key>` conventions.
The host does not parse those schemas. Categorization validates coverage and descriptions before atomic metadata writes.
Management runs parallel plan-only children, then commits one aggregate predicate/mask transaction.
Generic transactions check revision, incarnation, source identity, metadata predicates and mask overlap atomically.
Absence differs from JSON null. Supported history copies retain canonical item annotations; raw replay remains unchanged.
Background policy lives in JavaScript, currently the last 16 items with 240-character excerpts.
`InvocationCoalescer` requires exact source/revision/entrypoint/execution identity, joined known success,
a context-owned generic commit and fresh revision/incarnation. It does not interpret category coverage.

Manual compaction acquires the native reject-busy pause only in service-owned preparation, before history admission.
The opt-in `AutomaticCompactionHandler` adopts the coordinator's completion barrier lease and freezes host model limits.
JavaScript decides 80% of a known positive limit or 180000 context tokens; missing usage never qualifies.
Helpers are excluded. Production installs this handler through the actual originating gRPC, hosted MCP, or validated local-generation principal's run-scoped factory. Shutdown joins the owned handlers. Hosted MCP retains its original token expiry and revocation lease after the HTTP response. Embedded MCP retains the validated process-generation lifetime, not an OAuth subject. Missing or invalid admission proof fails closed; metadata and session membership supply no authority.
Context calls and completion admissions register retained host-runtime drivers before effects. Cleanup joins them
and releases the pause even if its only waiter drops. Cleanup uncertainty remains an error, not a successful release.
Compaction commits a mask before arming exact-cursor continuation; later failure does not undo that mask.
JavaScript chooses history copy, inheritance flags, exact destination message and start behavior.
Source freshness checks linearize the immutable source read immediately before creation; no cross-record transaction exists.
Destination and exact initial-message persistence are atomic. Start consumes its one-shot identity before the effect.
Failure or cancellation after creation retains the destination; uncertainty never permits start replay.
Native runs retain the ordinary session coordinator. Fork input does not change source masks.

Child calls use the same invocation service and engine, immutable definitions, principal, exact snapshot, capability
intersection, and absolute tree deadline. Limits are depth 4, total 16, parallel 4, and 65536 encoded input/output
bytes; deadlines are at most 30 seconds. No queue or second engine bypass exists. One-slot service/engine cases
fail before child history/effects. Failure cancels and joins every sibling, including pending admission.
Semantic consumers watch at most 32 incarnations, keep bounded counters/deduplication, and expose live ring lag.
They count durable completed turns and actual provenance-bound task completion, never provider text or accepted task results.
Lost events do not trigger replay or backfill.

Entrypoint descriptors support manual, event, interactive, tool, inline, MCP, schedule, progress, and lifecycle adapters.
Descriptors do not form a task graph or grant authority.
Audio, text, image, and file declarations contain metadata only. Constructor-injected adapters supply media I/O.

Each origin must select its own declared trigger before preflight or durable admission:

| Origin | Required trigger | Required ownership |
| --- | --- | --- |
| manual | manual | detached |
| event | event | detached |
| schedule | schedule | detached |
| lifecycle | lifecycle | detached |
| progress | progress | detached |
| interactive | interactive | leased |
| tool | tool | tool |
| inline | inline | tool |
| MCP (`mcp`) | mcp | leased or tool |

There are no wildcard triggers. `HostFactory::preflight` receives the selected name and exact descriptor, including its selector.
The supplied adapter must validate that semantic selector against its own authority; the descriptor grants none.

## Owner layer and integration points

The server owns admission, immutable program identity, durable observations, and execution ownership.
`InvocationService::new` accepts an engine, `InvocationStore`, `HostFactory`, `Clock`, and active-owner limit.
`HostFactory::preflight` receives the service-issued `InvocationBinding` before admission.
Its durable invocation ID and authenticated principal differ from the engine context ID and caller correlation.
Preflight supplies all required capability adapters, operation policy, opaque snapshot provenance, and an owned reservation.
The service supplies no implicit session, MCP, source-pause, or filesystem authority.
Later adapters must verify every required capability and required media port before durable admission.

The service registers one bounded owner before asynchronous preflight.
It commits the revision and admission before `engine.start`.
A detached invocation survives a lost acknowledgment or handle Drop.
Leased and tool acknowledgment futures and handles request cancellation on Drop; explicit cancellation also supports detached invocations.
Maintenance rejects all new origins; shutdown rejects new requests, cancels owners, and joins their tasks.
One service-owned shutdown worker retains join responsibility if any shutdown waiter is dropped.
Concurrent and later waiters observe the same completed shutdown result, including cleanup uncertainty.
The composition owner retains responsibility for engine shutdown because other consumers can share the engine.

`OperationPolicy` validates complete typed requests and current authority before dispatch intent.
`JournalHost` commits intent before any supplied host effect, then commits a safe output projection before the guest response.
Required journal or projection failures cancel the invocation and fence concurrent dispatch after policy and intent awaits.
Unresolved intents force uncertain history; restart reconciliation never replays effects or starts a guest.
Success requires engine join, verified host cleanup, required reservation finalization, and a durable terminal write.
Unverified cleanup quarantines the reservation and consumes capacity.
Service destruction preserves that quarantine for the process lifetime instead of an unsafe reservation Drop.

The service retains `ScriptExecution::cancellation()` before consuming the owner through `join`.
Invocation cancellation signals this independent control during join, including CPU-only guest work.
The service still awaits the same join future to verify worker teardown and host cleanup; cancellation never substitutes for join.

## Data and bounds

The revision hash covers API identity, engine identity, and canonical JSON with the exact decoded source.
Immutable filesystem/S3 storage holds that envelope once per digest. SQL retains references and invocation foreign keys.
History stores provenance, status, dates, bounded results, and dynamic operation projections, never resumable heaps or retry tokens.
The invocation states are admitted, running, succeeded, failed, interrupted, and uncertain.
The operation states are dispatched, completed, failed, and uncertain.

| Surface | Limit |
| --- | --- |
| YAML and canonical envelope | 131072 bytes each |
| Source and complete encoded value | 65536 bytes each |
| Complete result | 65536 bytes; no greater than the declared value budget |
| Value depth / nodes / string | Defaults: 32 / 4096 / 16384 bytes |
| Contract depth / schema nodes | 16 / 256 |
| Object properties / array items / declared string | 128 / 4096 / 65536 bytes |
| Entrypoints / ports / capabilities | 32 / 32 / 64 |
| Catalog and descriptor identifiers | 128 ASCII bytes |
| Description | 4096 bytes |
| Media aggregate declarations | Audio 256 MiB; text 1 MiB; image 20 MiB; file 64 MiB |
| Active or quarantined owners | Configured 1..128 |
| Host calls / in-flight calls | Defaults 256 / 16; maxima 1024 / 32 |
| Deadline | Default 30000 milliseconds; explicit `media.execute` maximum 5400000 milliseconds |
| Operation projection | 4096 bytes, depth 8, 256 nodes, 1024-byte strings |
| Invocation / operation metadata record | 4096 / 8192 bytes |
| History page | 1..100 records, ascending immutable cursor |

Migrations `0009_script_invocations.sql` add three tables in each backend: `script_program_revisions`, `script_invocations`, and `script_operations`.
PostgreSQL uses the `waltr` schema.
Migration 0014 removes only legacy task-history tables. Current script records, sessions, masks, OAuth state and configuration are preserved.
Migration 0016 refuses populated incompatible Routine state before active SQL program/text body columns disappear.
No backfill, converter or automatic wipe exists. Object persistence and verification precede SQL publication.
SQL atomically commits text ownership, quota and reference after an invocation-state check.
Missing/corrupt objects fail explicitly; no catalog or SQL body fallback exists.
Unreferenced objects remain retained without automatic collection. See [payload storage](../../../../docs/architecture/persistence.md#routine-payload-objects).
Earlier migrations remain unchanged.
Startup reconciliation requires exclusive database ownership and only terminalizes unfinished observations.
The invocation service exposes no watch stream, deletion API, retry queue, or automatic reconciliation lifecycle.
The RPC adapter supplies SQL-backed observations.

## Production Routine RPC adapter

`RoutineRpcService::new` accepts a `ReadAdapter` and shared `AttachmentRegistry` and `BlobRegistry` instances.
`into_server()` constructs all 21 Routine RPCs directly.
Standalone and embedded production constructors register this shared Routine RPC service under `waltr.routine.v2`.

`Catalog` publishes complete bounded replacements through an injected watch receiver.
`MemoryCatalog` supports explicit shared offers for tests and host composition.
Offers describe current request eligibility, not capabilities or historical authority.
`StartPolicy` authorizes the actual source session and supplies the normal host factory.
Admission pins the source-free public definition and its offered start contexts.
Historical detail reads preserve that projection after catalog replacement or deletion.

The adapter derives a Routine-specific principal from authenticated issuer and subject.
Client correlations and source-session claims grant no ownership.
Every public history, cancellation, text, and blob operation authorizes against durable principal scope first.
SQL applies principal and optional exact source filters before page limits.

Additive migration `0011_script_observation.sql` supplies a shared durable observation journal in both backends.
Transactional triggers capture admission, status, operation completion, text descriptors, and selected helper/binary metadata.
Historical records and earlier migrations remain intact.
Observation revisions differ from immutable admission cursors.
Initial watch pages reconstruct one fixed high-water snapshot across invocations and operations.
Later pages deliver strict journal-order upserts.
Opaque cursors bind the principal, exact source filter, snapshot position, high-water, and durable database key.
No automatic journal pruning, effect replay, or guest restart exists.
Binary descriptors freeze observation-time validity; a descriptor never grants byte access.

Native `Interact` and browser Open/Send/Watch/Renew/Close share one admitted owner and the same media driver.
The host associates cold input declarations before preflight and SQL admission.
Required declarations, immutable formats, finite binary sizes, purposes, and digest presence fail before admission on mismatch.
Blob upload metadata must match the exact admitted port declaration.
Only a complete successful blob commit supplies input to the normal helper path.

Transport limits include 64 KiB chunks, 128 KiB batches, 32 events per batch, eight queued batches, and 16 output frames.
Shared transport credits cap process retention at 16 MiB and principal retention at 4 MiB.
Typed stream credits remain separate and also apply.
The registry caps live or quarantined attachments at 64 process-wide and four per principal.
The output subscriber acknowledges each frame before the driver proves required output closure.
Control and cancellation bypass full data queues.
RPC output consumers await deferred output under cancellation and the original absolute media deadline, not the producer idle limit.
Capture, finalization, and helper work can complete before the first output packet.
Explicit stream close remains necessary for output EOF.
Input and ordinary internal stream idle limits, transport credits, queue waits, and output acknowledgment backpressure remain unchanged.

An attachment requires an output subscriber within five seconds.
Its renewable owner lease lasts 30 seconds; its absolute deadline never exceeds the admitted deadline or 90 minutes.
Browser callers renew explicitly.
A native duplex call renews only while its concrete output owner remains attached.
Authentication loss, lease expiry, output Drop, and lost admission acknowledgment cancel and join the original owner.
The access monitor survives the Open unary response, including browser logout before Watch.
Normal input finish preserves output ownership.
Explicit native input finish requires clean input EOF before success.
Cleanup uncertainty retains the actual reservation and its capacity quarantine.

Binary output transfer creates a port-bound, process-generation grant before invocation teardown.
The fixed 15-minute lease does not renew.
The blob registry permits one sequential reader admission per output grant and no seek or resume.
Reads use ordered chunks of at most 64 KiB and one explicit final EOF.
Reader Drop, authentication loss, and expiry revoke the actual blob reader, even without a later read request.
History and process restart never reconstruct a grant.
The registry caps output grants at 128 globally and 32 per principal; readers at 256 globally and 16 per principal.
Read buffers use a shared 16 MiB budget.
Durable text results retain SQL ownership/references, immutable object bodies, byte-offset reads and independent result quotas.

## Agent adapters

`adapters/` supplies constructor-injected helper and concrete-tool ports. Production routes and catalog registration remain unchanged.
`InvocationAdapterFactory::bind` receives the definition, selected entrypoint, and caller. The host verifies selector authority and supplies fresh private session owners.
Required media ports fail preflight because these adapters provide no media I/O.

| JavaScript operation | Strict argument shape | Required capabilities |
| --- | --- | --- |
| `helper.run` | `{selection,task,capture?,source?,maxTurns,deadlineMs,responseBytes,completionBytes}`, optional callback and fifth-argument payload resource | `helper.run` and exact helper identity |
| `tools.execute` | `{target:{server,tool},callId,arguments}` | `tools.execute` and constructor-mapped `tool.<id>` |
| `tools.schemas` | `{targets:[{server,tool}]}` | `tools.schemas` and each mapped target |

Helper selections use `{kind:"named",name}` or `{kind:"restricted",profile}`.
Named helpers pin the current catalog bundle. Restricted profiles supply explicit inference, prompt, tools, and total turn limits.
`spec.captures` maps each capture name to `{selection:{kind:"named",name},contract:{schemaVersion:"waltr.value/v1",schema}}`.
Capture names require `helper.capture.<id>`. `_helper_capture` validates the resource-declared closed contract through normal policy.
Declaration grants no capture permission. Definite denial fails media preflight before driver or speech effects.
Ask remains eligible for ordinary live consent; preflight does not mint approval.
Semantic callbacks require `helper.validate`; capture validation does not replace semantic validation.

Callbacks receive `{sessionId,proposalId,turn,response,evidence}`. Evidence retains effective calls, post-policy results, errors, and timeline links.
Return `{decision:"accept"}` or `{decision:"reject",feedback:{proposalId,callId?,message}}`.
Rejection appends durable feedback to the same helper and preserves original results. Exceptions, invalid verdicts, cancellation, and exhausted budgets fail closed.
Provider deltas remain previews. Only accepted helper text becomes committed output. Restart never resumes callback heaps or repeats completed effects.

Direct operations return `completed`, `authorizationRequired`, or `denied` status. Successful results preserve the complete provider-neutral tool result.
Ask returns an actionable authorization requirement before concrete dispatch. No automatic resume exists. Cancellation or dispatch uncertainty quarantines unverified cleanup.
Discovery exposes requested, whole-tool-permitted schemas only; it grants no execution authority.
Both direct and model calls use the same agent pre-tool, permission, dispatch, post-tool, projection, and durable result path.
Canonical exact identity and legacy alias policy both constrain direct calls. Model exposure does not grant authority; app-only tools remain unavailable.
Preflight pins every permitted named agent bundle. Catalog replacement or deletion cannot change its prompt, tools, or policy during dispatch.
Hidden remote identities do not receive feature-default execution grants. Applicable explicit permissions still apply.

Deadlines bound provider, tool, and observer work, not cleanup completion. Persistence, admission, interruption, and joins use cooperative waits.
An agent-owned driver starts before admission and continues admission, interruption, and joining even when every request or cleanup waiter is dropped. Coordinator shutdown tracks the driver through its run drain; no later cleanup caller is required to poll it.
Construct adapters on the host's long-lived Tokio runtime. Helper owners retain that runtime explicitly rather than spawning on the guest engine's temporary runtime. The host runtime must outlive coordinator shutdown. Dropping the last owner requests cancellation but does not abort its tracked driver.
Deadline expiry alone does not prove cleanup. The invocation reservation remains quarantined unless every child joins successfully.

| Adapter surface | Bound |
| --- | --- |
| Total helper turns | 1..64, including corrections; restricted profile limits also apply |
| JavaScript helper deadline | 1..30000 milliseconds, within invocation lifetime |
| Neutral validation deadline | Greater than zero, at most 300 seconds |
| Task / restricted prompt / capture contract | 16384 bytes each |
| Restricted profiles / capture contracts / requested schemas | 32 each |
| Helpers per invocation | 64 |
| Cumulative evidence | 256 calls and 262144 encoded bytes |
| Feedback | Nonempty, at most 4096 bytes, with valid proposal and optional call link |
| Helper result / concrete arguments / schema list | 65536 encoded bytes each; engine limits also apply |
| Exact server / remote tool / call identifier | 256 bytes each |

## Media composition

`adapters/media.rs` supplies the production `MediaHostFactory` over the same service, SQL journal and WASM engine.
Hosts select speech providers/models and exact inference attachment capabilities. JavaScript selects helper agents, tasks and budgets.
`MediaBindingFactory` supplies authenticated streams, cold transport owners, speech, normal helpers, blob storage, and selected text storage.
Preflight validates principal, port identity, format, aggregate limits, model/API profiles, and speech configuration without transport effects.
The service commits admission before the first operation starts the transport owner.

The host injects shared process and principal pools across invocations; an invocation never creates replacement global credit.
`MediaDriver::finish` proves explicit input completion and required output EOF.
`MediaDriver::join` proves cleanup, including deliberate cancellation. Drop, timeout, and cancellation alone prove neither condition.
Every operation retains its worker until join. Cleanup cancels and joins transports, speech, helpers, readers, and blob ownership.

| JavaScript operation | Behavior |
| --- | --- |
| `media.transcribe` | Host-owned STT with ordered append/replace updates and explicit final commitment |
| `media.collect` | One bounded collection after successful STT; optional host-pinned diarization snapshot |
| `media.synthesize` | PCM16LE, 24 kHz, mono from committed text and explicit host-approved speech options |
| `media.tee` | Host-owned fanout with required-consumer backpressure |
| `media.upload` | Sequential finite upload; no readable handle before integrity and fixed signature checks |
| `media.select` | Entire committed capture or generic JSON-pointer selection, with helper provenance |
| `media.read` / `media.proposal.read` | At most 64 KiB through an invocation-local resource reference |
| `media.publish` | Immutable selected UTF-8 object with SQL owner/quota/reference; descriptor-only reply |
| `media.emit` | Committed text output after helper acceptance |
| `media.lease` | Authenticated binary output transfer before teardown, with a declared output name |
| `media.finish` | Seal dispatch and prove required transport completion |

JS receives opaque, engine-bound resources, not payload-sized JSON or ownership grants.
Helper callbacks receive proposal descriptors and temporary chunk-reader references; the normal agent retains original responses and evidence.
Rejection keeps the same helper, cumulative evidence, provider-turn limit, and absolute deadline.
JavaScript supplies explicit response/completion ceilings within host bounds; the ordinary agent default remains 262144 bytes.
The 256-evidence limit also remains unchanged. A cumulative ceiling can reject otherwise valid large proposals.

Helpers await permission and Skills consent through the server-owned interaction broker and typed `InteractionService` responses.
The original loop retains attachments, evidence, deadline, and pending-call ownership throughout consent.
Normal policy, middleware, MCP provenance, pattern validation, and denial semantics still apply.
Duplicate, stale, wrong-owner, and ownerless responses cannot create a replacement run.
Restart never restores transient input or a suspended heap. Direct JS concrete-tool calls retain fail-before-dispatch authorization behavior.

Uploads allow files through 64 MiB and PNG/JPEG/WebP images through 20 MiB with exact MIME and fixed signature checks.
Providers own full decoding; malformed bodies with allowed signatures can pass unchanged.
The host reports no dimensions and enforces no decoded-pixel limit or decompression-bomb protection.
Helpers accept native images, opaque PDF, and exact UTF-8 through 8 MiB each and 16 MiB source per run.
Other files remain storage-only. PDF input does not imply PDF sanitization.
The normal agent retains shared source credit and bounded request encoders through teardown; this is not a whole-agent RSS bound.

Stream chunks remain at most 64 KiB, independently of aggregate declarations.
Authoritative diarization snapshots wait for attributed revisions and provider EOF under cancellation and the original absolute invocation deadline.
Wordless interim responses and silent finalization do not trigger a snapshot response or revision idle timeout.
Input idle limits, publication queue waits, quotas, workspace credits, and ordinary STT/TTS response idle limits remain unchanged.
Only explicit input close, provider EOF, and worker joins permit the final snapshot commit.
Audio capture allows 256 MiB and 65 minutes; the invocation allows at most 90 minutes without fuel renewal.
Default text-state retention remains 1 MiB. A host can select 2 MiB for a 1 MiB output plus segment metadata.
Stream payload credit remains 8 MiB per invocation, 16 MiB per principal, and 64 MiB per process.
Server result credit is 64 MiB per invocation, 64 MiB per principal, and 128 MiB per process.

E5 collects at most 1 MiB before one cleanup helper. E6 collects at most 256 KiB before its two helpers.
The second E6 helper receives both validated `retained_transcript` and `deleted_items` fields.
Retained transcript, notes and deletion audit enter durable history as three separate selected text results.
Selected results allow 1 MiB each, 8 MiB total encoded charge, and authenticated reads through 64 KiB.
Those results survive later failures and restart. Binary leases expire after 15 minutes and do not grant history-reader authority.
The operation journal retains safe metadata, not recordings, attachments or raw tool arguments. Selected text results retain their explicit payloads.
The invocation journal conservatively marks dispatched host errors uncertain, independently of verified cleanup.

## Adapter tests

Focused `script_` unit tests cover service ports, strict contracts, SQL transitions, effect fences, failure paths, ownership, and the real engine.
SQLite and PostgreSQL share the durable history contract. Migration tests distinguish historical task-table retirement from migration 0016's populated-state refusal.
Object-cutover tests cover immutable references, missing/corrupt payloads and preservation of unrelated credentials and state.
The PostgreSQL fixture requires an explicitly supplied fresh local disposable database and refuses ordinary database targets.
Tests need no provider credentials, LLM, or production startup.
See [repository test guidance](../../../../docs/testing.md) for shared test expectations.
