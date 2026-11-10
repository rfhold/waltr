# Storage

Agent storage persists event-sourced sessions: cursor-assigned timeline events and journal records for prompts, provider requests, neutral context state, and generic lifecycle. Private server interactions and approval grants are not stored here.

## Public Session APIs

| API | Purpose |
| --- | --- |
| `Agent::sessions` | Creates an `AgentSessions` factory from an agent and store. |
| `AgentSessions` | Creates, loads, and prompts durable sessions. |
| `AgentSessionRuntime` | Runtime wrapper around a session record and store. |
| `SessionStore` | Async persistence trait implemented by storage backends. |
| `ResumeInput` | Continue, prompt(s), or externally completed tool results for a loaded session. |
| `RuntimeResumeOutput` | Indicates whether resume produced a runnable record or remained suspended. |

## Session Data Model

| Type | Purpose |
| --- | --- |
| `SessionId` | Stable session identifier. |
| `Revision` | Public complete-record revision for observation and strict mutation CAS. |
| `ExecutionSequence` | Durable execution freshness sequence, separate from public metadata changes. |
| `ExecutionToken` | Session ID, durable incarnation and execution sequence for append preconditions; not authority. |
| `SessionCommitReceipt` | Coherent committed public revision, execution token, metadata, inference settings, definition and timeline cursor high-water. |
| `TimelineAppendReceipt` | Commit receipt plus store-assigned timeline events. |
| `SessionRecord` | Complete durable record with both revision domains. Its snapshot derives from journal records. |
| `AgentTimelineEvent` | Cursor-assigned history event for text, thinking, tool calls, progress, results, and status. |
| `SessionJournalEvent` | Internal journal entry for prompts, provider requests, provider metadata, middleware state, suspensions, turn finishes, and lifecycle records. |
| `SessionSnapshot` | Reconstructed turn state derived from the journal for resume decisions and provider replay. |
| `RequestRecord` | Canonical and projected provider request data recorded before dispatch. Inline tool-result images are redacted to safe metadata in these audit records. |
| `MiddlewareState` | Namespaced durable middleware state. |

## Store Contract

`SessionStore` implementations support:

- Creating a new empty session record by id.
- Loading a session by id.
- Appending timeline events with execution-token checks and store-assigned cursors.
- Appending internal journal records with execution-token checks.
- Reading timeline ranges after or before a cursor.
- Wiping every raw session record in the store namespace without requiring records to deserialize.

The agent loop depends on this trait rather than a concrete backend.

Metadata mutations, including inference settings and selected agent bundles, must not overwrite concurrent journal or timeline appends. Memory stores use process-wide per-session locks. Filesystem stores hold a shared global advisory lock plus an exclusive per-session advisory lock for mutations, while wipe holds the exclusive global lock. Unix targets, including Android, use a safe `flock` wrapper; other targets use the standard-library file-lock API. Filesystem safety still requires one process owner and no external file writer because readers do not participate in these mutation locks. Generic OpenDAL uses service ETags or an explicit compare-and-swap adapter. A conditional metadata update returns unsupported when neither exists; it never falls back to a non-atomic write. OpenDAL's process locks share an explicit durable mutation namespace, but they do not provide distributed atomicity. The PostgreSQL helper supplies database compare-and-swap. The dedicated S3 backend defaults to object ETag compare-and-swap across writers. It retries at most eight conflicting conditional PUTs before returning a backend error. Its explicit single-writer mode uses process-local serialization and unconditional PUTs. That mode requires one Waltr process and no external writer. Scylla uses lightweight transactions over revision and record content.

The coordinator also serializes inference and agent mutations with runtime record capture. Later captures use the committed configuration. An active runtime never swaps provider or policy in place; a concurrent configuration change invalidates its execution token.

A store wipe is idempotent and retry-safe. A successful result means no session records remain. Backends without multi-object transactions may delete some records before returning an error, and callers may safely retry. The operation deliberately does not inspect or coordinate running work; operators must ensure session activity is stopped before wiping or concurrent work may recreate records.

### Revision Domains And Append Receipts

`SessionRecord.revision` remains the public complete-record revision. `execution_sequence` tracks durable changes that invalidate execution assumptions.

| Mutation | Public revision | Execution sequence |
| --- | --- | --- |
| Journal/timeline append, middleware state, dispatch fence or settlement | Advance | Advance |
| Rollback or mask transaction | Advance | Advance |
| Opaque session/item metadata only | Advance | Unchanged |
| Metadata and masks in one transaction | Advance once | Advance once |
| Actual inference-settings or agent-definition change | Advance | Advance |
| Failed predicate or rejected transaction | Unchanged | Unchanged |
| Successful predicate-only `SessionMutation` | Advance | Unchanged |
| Field setter with no state change | Unchanged | Unchanged |

Each backend checks `ExecutionToken` inside its atomic complete-record mutation. The durable incarnation prevents stale append acceptance after session deletion and recreation. The process-local list incarnation cannot substitute for it.

Both append APIs return a bounded `SessionCommitReceipt`; timeline append also returns assigned events. The receipt contains coherent durable facts from the same commit, not another record load. The engine adopts these facts before ordered publication and preserves concurrent titles. Callers must not guess public revisions with `revision.next()` or copy the full journal for each delta.

`SessionMutation.expected_revision` and `ContextIdentity` retain strict public-record semantics. Context digests and metadata/mask predicates still inspect pre-update state atomically. Execution-token equality never replaces those checks or grants authority.

Only a definite physical CAS rejection permits reload and logical-token recheck. Unknown write outcomes propagate; they never authorize provider requests, tool effects or uncertain settlements to replay. Durable dispatch-before-effect fences and cancellation/join ownership remain unchanged. Tokenless OpenDAL and explicit S3 single-writer mode gain no distributed-safety guarantee.

### Stored Record Compatibility

For records with a durable incarnation, an absent `execution_sequence` starts from the stored public revision. This value is an opaque initial token, not zero or a historical execution count. Reads never rewrite records; the next authorized write persists the field. Scylla compares the exact original JSON and row revision through LWT, not a reserialized compatibility view.

Legacy records without a durable incarnation remain readable and listable. Token creation fails with `ExecutionCompatibility`; Waltr never invents an incarnation.

Mixed old/new complete-record writers are unsupported. Each store requires one coordinated writer version, even with physical CAS support. This JSON compatibility change requires no SQL migration, retained-store scan, historical migration edit or automatic reset. Preserve app data and retained session records. See the [session revision cutover](../runbooks/server-capability-cutover.md#session-revision-cutover).

## Backends

| Backend | Purpose |
| --- | --- |
| Memory | In-process tests and ephemeral sessions. |
| Filesystem | JSON files on local disk. |
| OpenDAL | Generic JSON object storage through an OpenDAL operator. |
| S3 | JSON object storage in S3-compatible object stores. |
| Scylla | JSON rows with helper schema creation. |
| PostgreSQL | OpenDAL-backed PostgreSQL helper with migrations. |

The backend implementations store the same session model; they differ in persistence mechanics and setup requirements.

## Runtime Flow

Durable runs journal data before external side effects where possible:

1. Middleware state needed for request construction is written.
2. The provider request is recorded before dispatch.
3. Provider events are converted into timeline events or internal journal records as the stream is consumed.
4. Tool-call previews, progress, results, and status changes are appended to the timeline.
5. Provider requests, metadata, middleware state, suspensions, interruptions, and turn finishes are appended to the internal journal.
6. Loaded records derive snapshots from the journal before making resume decisions.

Crash recovery is retry-the-turn. The provider request is durable before dispatch, but parser progress inside an in-flight provider stream is not a durable checkpoint.

For the fork Routine, `SessionCreateRequest` atomically stores the ordinary destination and its byte-exact first prompt. Successful creation is the prompt-durability boundary and emits the normal user-message event.

After successful atomic creation, a start failure or caller cancellation retains the destination and its ID.

A source-backed Routine helper atomically stores rollback-aware provider-neutral inference history through the latest completed source turn, followed by its task prompt. The copied history includes provider-replay prompts, assistant text, tool calls and results, and inline images. It excludes source lifecycle events, middleware state, status, usage and cost, request audit records, provider metadata, provider-native thinking payloads, rollback barriers, and active partial work.

`SessionCopy` is an opaque validated projection through one exact completed-turn cursor. Materialization preserves copied cursors, item IDs, parent IDs, and tool-call IDs, rewrites each timeline event's session ID to the destination, and persists the copied cursor as the destination high-water before appending configured messages. One copy has one source; multi-source merging is unsupported.

Canonical item metadata survives supported copies and projections. Session/item values remain opaque to the agent and grant no authority. `SessionMutation` atomically checks revision, context, durable incarnation and metadata predicates with dependent mask changes. Absence differs from JSON null. Invalid keys, unknown items and exceeded bounds reject before mutation. See [agent metadata contracts](../architecture/agent.md#durable-sessions).

Provider requests derive only from the durable helper record. Provider adapters apply their normal encoding rules, so the record is not a byte-for-byte HTTP wire archive. The helper persists across restart and storage backends under ordinary session retention rules. Normal session delete and wipe operations apply. An inference-only task helper stores only its task prompt.

The ordinary fork destination stores compaction plus the original direction. It stores no copied source history.

Provider-neutral tool results may contain bounded inline images so a resumed session can replay them to the model. Request audit records do not duplicate that Base64: canonical records retain media type, decoded size, and SHA-256 digest, while projected provider requests replace native Base64 and data URLs with redacted metadata. Public server timeline conversion uses the same safe text projection and never returns inline image data.

## Resume Inputs

Loaded sessions resume with `ResumeInput`:

| Input | Behavior |
| --- | --- |
| `Continue` | Continues from the durable checkpoint without adding input. |
| `Prompt` / `Prompts` | Supersedes pending work and appends user prompt(s). |
| `ToolResults` | Records externally supplied tool results. |

Questions and permissions use the server's original live continuation through [Interactions](../architecture/agent.md#live-interactions), never a stored external-response resume input. Removed history variants reject without a fallback decoder or automatic wipe. See [coordinated upgrade](../runbooks/server-capability-cutover.md).

## Boundaries

- Session storage is an agent-library concern; server/client/UI persistence contracts are separate layers.
- Provider-specific metadata may be stored with turns, but replay correctness should rely on provider-neutral timeline/journal data. Snapshots are derived projections, not durable source-of-truth records.
- Session compression and automatic compaction remain unimplemented. The server implements manual fork compaction and automatic titles outside the agent storage layer.
- SQLite, Redis, and embedded key-value store backends are not implemented.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/src/agent/session/runtime.rs](../../crates/waltr-agent/src/agent/session/runtime.rs) | Resume behavior and runtime state transitions. |
| [../../crates/waltr-agent/src/agent/session/types.rs](../../crates/waltr-agent/src/agent/session/types.rs) | Session type serialization and behavior. |
| [../../crates/waltr-agent/src/agent/session/storage/memory.rs](../../crates/waltr-agent/src/agent/session/storage/memory.rs) | In-memory store behavior and trait expectations. |
| [../../crates/waltr-server/tests/agent_host/session.rs](../../crates/waltr-server/tests/agent_host/session.rs) | Host session integration and durable replay. |
| [../../crates/waltr-agent/tests/agent/session_storage.rs](../../crates/waltr-agent/tests/agent/session_storage.rs) | Deterministic S3 write-mode, conditional-conflict, and same-process serialization coverage. |
| [../../crates/waltr-agent/tests/live/agent/session_storage.rs](../../crates/waltr-agent/tests/live/agent/session_storage.rs) | Ignored live/container-backed storage coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/agent/session/mod.rs](../../crates/waltr-agent/src/agent/session/mod.rs) | Public durable session module boundary. |
| [../../crates/waltr-agent/src/agent/session/runtime_factory.rs](../../crates/waltr-agent/src/agent/session/runtime_factory.rs) | Session runtime factory. |
| [../../crates/waltr-agent/src/agent/session/runtime.rs](../../crates/waltr-agent/src/agent/session/runtime.rs) | Durable runtime and resume handling. |
| [../../crates/waltr-agent/src/agent/session/types.rs](../../crates/waltr-agent/src/agent/session/types.rs) | Session journal, snapshots, and records. |
| [../../crates/waltr-agent/src/timeline.rs](../../crates/waltr-agent/src/timeline.rs) | Public timeline event, cursor, channel, content, and status types. |
| [../../crates/waltr-agent/src/agent/session/storage/mod.rs](../../crates/waltr-agent/src/agent/session/storage/mod.rs) | `SessionStore` trait and storage exports. |
| [../../crates/waltr-agent/src/agent/session/storage/filesystem.rs](../../crates/waltr-agent/src/agent/session/storage/filesystem.rs) | Filesystem store. |
| [../../crates/waltr-agent/src/agent/session/storage/opendal.rs](../../crates/waltr-agent/src/agent/session/storage/opendal.rs) | Generic OpenDAL store. |
| [../../crates/waltr-agent/src/agent/session/storage/s3.rs](../../crates/waltr-agent/src/agent/session/storage/s3.rs) | S3 store. |
| [../../crates/waltr-agent/src/agent/session/storage/scylla.rs](../../crates/waltr-agent/src/agent/session/storage/scylla.rs) | Scylla store. |
| [../../crates/waltr-agent/src/agent/session/storage/postgresql.rs](../../crates/waltr-agent/src/agent/session/storage/postgresql.rs) | PostgreSQL helper. |
