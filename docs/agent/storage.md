# Storage

Agent storage persists durable sessions for `crates/waltr-agent/`. A session is event-sourced: the agent stores cursor-assigned timeline events and internal journal records for prompts, provider requests, external requests, and middleware state so a run can be loaded, ranged, or resumed through a `SessionStore`.

## Public Session APIs

| API | Purpose |
| --- | --- |
| `Agent::sessions` | Creates an `AgentSessions` factory from an agent and store. |
| `AgentSessions` | Creates, loads, and prompts durable sessions. |
| `AgentSessionRuntime` | Runtime wrapper around a session record and store. |
| `SessionStore` | Async persistence trait implemented by storage backends. |
| `ResumeInput` | Prompt, external response, tool results, or interrupt input for a loaded session. |
| `RuntimeResumeOutput` | Indicates whether resume produced a runnable record or remained suspended. |

## Session Data Model

| Type | Purpose |
| --- | --- |
| `SessionId` | Stable session identifier. |
| `Revision` | Optimistic revision for appended timeline and journal records. |
| `SessionRecord` | Loaded journal and revision. Its snapshot is derived from journal records. |
| `AgentTimelineEvent` | Cursor-assigned public display/history event for text, thinking, tool calls, progress, results, external requests, and status. |
| `SessionJournalEvent` | Internal journal entry for prompts, provider requests, provider metadata, middleware state, suspensions, turn finishes, and lifecycle records. |
| `SessionSnapshot` | Reconstructed turn state derived from the journal for resume decisions and provider replay. |
| `ExternalRequest` | Pending host interaction such as a permission decision or question answer. |
| `RequestRecord` | Canonical and projected provider request data recorded before dispatch. Inline tool-result images are redacted to safe metadata in these audit records. |
| `MiddlewareState` | Namespaced durable middleware state. |

## Store Contract

`SessionStore` implementations support:

- Creating a new empty session record by id.
- Loading a session by id.
- Appending timeline events with revision checks and store-assigned cursors.
- Appending internal journal records with revision checks.
- Reading timeline ranges after or before a cursor.
- Wiping every raw session record in the store namespace without requiring records to deserialize.

The agent loop depends on this trait rather than a concrete backend.

A store wipe is idempotent and retry-safe. A successful result means no session records remain. Backends without multi-object transactions may delete some records before returning an error, and callers may safely retry. The operation deliberately does not inspect or coordinate running work; operators must ensure session activity is stopped before wiping or concurrent work may recreate records.

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
4. Tool-call previews, progress, results, external requests, and status changes are appended to the timeline.
5. Provider requests, metadata, middleware state, suspensions, interruptions, and turn finishes are appended to the internal journal.
6. Loaded records derive snapshots from the journal before making resume decisions.

Crash recovery is retry-the-turn. The provider request is durable before dispatch, but parser progress inside an in-flight provider stream is not a durable checkpoint.

Provider-neutral tool results may contain bounded inline images so a resumed session can replay them to the model. Request audit records do not duplicate that Base64: canonical records retain media type, decoded size, and SHA-256 digest, while projected provider requests replace native Base64 and data URLs with redacted metadata. Public server timeline conversion uses the same safe text projection and never returns inline image data.

## Resume Inputs

Loaded sessions resume with `ResumeInput`:

| Input | Behavior |
| --- | --- |
| `Prompt` | Cancels pending external requests/work and appends a new user prompt. |
| `ExternalResponse` | Validates and records a host response to a pending external request. |
| `ToolResults` | Records externally supplied tool results. |
| `Interrupt` | Records interruption and keeps the session suspended. |

Question responses record the response as the linked tool result. Permission responses are typed: allow decisions prime durable permission state and retry the original suspended tool call, while deny decisions either return a denial result or cancel the pending work.

## Boundaries

- Session storage is an agent-library concern; server/client/UI persistence contracts are separate layers.
- Provider-specific metadata may be stored with turns, but replay correctness should rely on provider-neutral timeline/journal data. Snapshots are derived projections, not durable source-of-truth records.
- Session compression, automatic compaction, and auto-title generation are placeholder modules and are not implemented.
- SQLite, Redis, and embedded key-value store backends are not implemented.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/src/agent/session/runtime.rs](../../crates/waltr-agent/src/agent/session/runtime.rs) | Resume behavior and runtime state transitions. |
| [../../crates/waltr-agent/src/agent/session/types.rs](../../crates/waltr-agent/src/agent/session/types.rs) | Session type serialization and behavior. |
| [../../crates/waltr-agent/src/agent/session/storage/memory.rs](../../crates/waltr-agent/src/agent/session/storage/memory.rs) | In-memory store behavior and trait expectations. |
| [../../crates/waltr-agent/tests/agent/session.rs](../../crates/waltr-agent/tests/agent/session.rs) | Deterministic session integration and durable replay. |
| [../../crates/waltr-agent/tests/live/agent/session_storage.rs](../../crates/waltr-agent/tests/live/agent/session_storage.rs) | Ignored live/container-backed storage coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/agent/session/mod.rs](../../crates/waltr-agent/src/agent/session/mod.rs) | Public durable session module boundary. |
| [../../crates/waltr-agent/src/agent/session/factory.rs](../../crates/waltr-agent/src/agent/session/factory.rs) | `AgentSessions` factory. |
| [../../crates/waltr-agent/src/agent/session/runtime.rs](../../crates/waltr-agent/src/agent/session/runtime.rs) | Durable runtime and resume handling. |
| [../../crates/waltr-agent/src/agent/session/types.rs](../../crates/waltr-agent/src/agent/session/types.rs) | Session journal, snapshots, records, and external requests. |
| [../../crates/waltr-agent/src/timeline.rs](../../crates/waltr-agent/src/timeline.rs) | Public timeline event, cursor, channel, content, and status types. |
| [../../crates/waltr-agent/src/agent/session/storage/mod.rs](../../crates/waltr-agent/src/agent/session/storage/mod.rs) | `SessionStore` trait and storage exports. |
| [../../crates/waltr-agent/src/agent/session/storage/filesystem.rs](../../crates/waltr-agent/src/agent/session/storage/filesystem.rs) | Filesystem store. |
| [../../crates/waltr-agent/src/agent/session/storage/opendal.rs](../../crates/waltr-agent/src/agent/session/storage/opendal.rs) | Generic OpenDAL store. |
| [../../crates/waltr-agent/src/agent/session/storage/s3.rs](../../crates/waltr-agent/src/agent/session/storage/s3.rs) | S3 store. |
| [../../crates/waltr-agent/src/agent/session/storage/scylla.rs](../../crates/waltr-agent/src/agent/session/storage/scylla.rs) | Scylla store. |
| [../../crates/waltr-agent/src/agent/session/storage/postgresql.rs](../../crates/waltr-agent/src/agent/session/storage/postgresql.rs) | PostgreSQL helper. |
