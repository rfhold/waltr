# Server Capability Cutover

## Purpose

Coordinate the hard upgrade to server-owned tools, MCP, permissions, and live interactions. There are no compatibility aliases, dual history decoders, old external-response RPCs, or automatic wipes. This procedure does not authorize deletion, deployment, or credential reset.

## Prerequisites

- Identify every server, worker, browser, TUI, and native/embedded client sharing the deployment. Upgrade their contracts together.
- Stop admission and drain or cancel owned work before stopping writers. Pending interactions and private grants disappear at restart; do not replay approvals or uncertain effects.
- Record the actual resource directory, session backend/namespace, both SQL targets, and wrapping-key versions. Defaults do not establish deployed paths.
- Back up sessions, overrides, both databases, and required wrapping keys separately. Preserve workspace and Routine/media/code artifacts. Obtain separate approval for each reset or replacement target.

## Steps

1. Inventory overrides and stored agent bundles. Replace old tool selectors and prompt references (`mcp_<server>_<tool>`) with `<server>__<tool>` only after checking exact upstream declarations. Bundled self tools use `waltr__*`; aliases are not authority. Replace unsupported definitions and legacy Routine DSL with reviewed current-schema resources. No converter exists. Overrides replace complete resource bytes.
2. Inventory sessions for removed external-request/response, approval-suspension, legacy timeline identity, or other unsupported variants. Readers reject incompatible records; decode errors never authorize deletion. Stored bundles are part of session records, not live resource overrides. Replacing a resource does not rewrite them.
3. Choose a separately approved session action: retain compatible records, archive and retire exact incompatible records, or reset an enumerated namespace. Stop all writers first. The raw store wipe API does not need to decode records, but removes **every session in that store namespace**, including helpers and copied history. It is not a database or workspace reset. Never invoke it merely to make startup pass.
4. Apply the coordinated binaries/clients/resources and standard immutable SQLx migrations. Migration `0014_retire_routine_dsl.sql` drops only `routine_invocation_events`, `routine_invocation_metadata`, and `routine_invocations`, children first, without `CASCADE`. Archive those legacy tables before upgrading if their history is needed. Current JavaScript and credential stores are retained.
   Migration `0015_external_capability_dispatch.sql` remains immutable on SQLite and PostgreSQL. Public capability removal preserves its schema and historical rows and requires no new migration or reset.
   Migration `0016_routine_object_references.sql` requires empty incompatible Routine program, invocation and text state before active body columns disappear. Startup preflight and SQL guards refuse populated incompatible state. Preflight also refuses populated legacy `routine_invocations` before migration 0014. SQL guards check `script_program_revisions`, `script_invocations` and `script_text_results`. Stop for a separately approved archive/reset procedure for the exact affected Routine records and dependent journals. No backfill, SQL-to-object converter, automatic wipe or broad database reset exists. Configure and preserve the new immutable object namespace separately. Historical migrations remain immutable.
5. If a database applied the removed, unshipped migration 0012, stop for a separately authorized database-specific recovery plan. Do not delete `_sqlx_migrations` rows, rewrite checksums, renumber migrations, or wipe SQL to bypass validation.
6. Reconnect upgraded clients with fresh snapshots/watch epochs. Discard only transient cached timeline/interaction projections. Old-generation responses must fail. Restart requires fresh approval and, where applicable, normal embedded-generation upstream reauthorization.

## Session Revision Cutover

This JSON compatibility change preserves sessions, journals, metadata and IDs. It adds no SQL migration, historical migration edit, retained-store scan or automatic reset. Public revision wire semantics remain unchanged.

1. Inventory every complete-record writer for the exact session store or namespace.
2. Stop admission and drain or cancel owned work before the old writers stop.
3. Stop every old writer before any new writer starts; mixed old/new complete-record writers are unsupported.
4. Resume only one coordinated writer version per store, within the backend's current writer constraints.

Physical CAS support does not permit mixed versions. S3 `single-writer` still requires one process and no external writer. Tokenless OpenDAL process locks still provide no distributed atomicity. Uncertain acknowledgments never authorize provider or effect replay.

For records with a durable incarnation, reads seed absent `execution_sequence` from the stored public revision without a rewrite. The seed is opaque, not a historical execution count. The next authorized write persists the field. Scylla retains exact original JSON and row revision for LWT comparison.

Legacy records without a durable incarnation remain readable and listable. Token creation fails with `ExecutionCompatibility`; this failure authorizes no fabricated incarnation, record deletion or reset. Preserve native app data, session namespaces, both SQL stores, credentials and workspace files. Any incompatible-record disposition requires separate exact-target approval. See [storage compatibility](../agent/storage.md#stored-record-compatibility).

## Exact Reset And Preservation Boundaries

| Surface | Exact boundary |
| --- | --- |
| Standalone filesystem sessions | `WALTR_SESSION_FILESYSTEM_ROOT/<id>.json`; absent override, `$HOME/.local/share/waltr/sessions/<id>.json`. Resolve the actual root, not the SQL directory. |
| S3 sessions | `WALTR_SESSION_S3_BUCKET` and `WALTR_SESSION_S3_PREFIX/<id>.json`. Enumerate exact keys and any retained versions under separate storage authority; never delete a bucket or unrelated prefix. |
| Embedded sessions | Host-supplied absolute app data `server/sessions/<id>.json`. Do not remove app data or sibling SQL files. |
| Other library stores | Explicit backend-specific namespace and records. Standalone environment selection supports filesystem, memory, and S3 only; memory retains no records. |
| Resource overrides | Exact slots under configured `.waltr/resources/`; embedded app config uses `server/.waltr/resources/`. Replace approved full resources and preserve other slots. |
| Incompatible Routine SQL | Inventory `script_program_revisions`, `script_invocations`, dependent `script_operations`, text results, observation/publication state and code-dispatch associations. Migration 0016 refuses populated incompatible state. Archive/reset requires separate exact-target approval; do not erase SQL to bypass refusal. |
| Compatible Routine SQL and objects | Preserve reference-format control state and its matching immutable payloads. Filesystem uses the configured root's `program/` and `text/`; S3 uses the exact configured `<prefix>/routine-objects/` namespace. No collector exists. A session reset never authorizes object deletion. |
| Routine-owned category metadata | Supported records use the current opaque session/item schemas. No legacy category reader or reinterpretation exists. Inventory incompatible records for separately approved disposition. |
| External capability SQL | Retain historical `external_capability_dispatch` associations and completion receipts in Waltr SQL. They include exact principal/input and known text/image results, can contain sensitive data, and have no store-provided payload encryption or automatic GC. Public capability removal and session reset do not remove them; retention deletion or database reset needs separate explicit approval. See [persistence](../architecture/persistence.md#sessionless-external-dispatch). |
| Credentials/access state | Retain Waltr provider/upstream OAuth and access state; MCP hosted clients/grants/tokens/signing keys; and referenced wrapping keys. A session reset does not require standalone reauthorization. |
| Workspace/artifacts | Retain all files, eleven Routine definitions/examples, media, code, and user artifacts. Do not delete a volume, workspace, database/schema, or app-private root as a session reset. |

No deletion commands are provided: deployment and destructive scope must first be explicit. The historical V5 credential migration has an earlier reauthorization consequence; it does not justify another credential wipe here.

## Expected Result

Clients use typed `InteractionService` snapshot/watch/respond. Requests have one live owner and exact generation/session/request/operation identity; one authorized response wins. They are not metadata or history. URL acceptance is consent only, not navigation or completion. Ordinary admitted MCP create/send prompts survive later access-token expiry; explicit Routine/helper/code leases remain enforced.

Internal concrete tool entry points share exact-identity server dispatch, middleware, policy, and worker preparation/execution. Public MCP neither discovers nor dispatches `capability__*` tools; self-MCP exposes no `waltr__capability__*` aliases. Native agent tools, management/administration tools, resources, and served Apps retain their contracts. Optional code mode retains separate grants and its public no-upstream-proxy restriction. Missing interaction routes fail before dependent effects.

No new public capability call enters the retained external dispatch journal. Historical uncertain associations and known completion receipts remain sensitive SQL data, not replay opportunities. Durable receipts do not restore live waiters, reconstruct effects, or provide usable restart continuations.

## Troubleshooting

- Decode failures require an authorized record disposition, not fallback decoding or automatic deletion.
- Missing tools can mean stale aliases, disabled capabilities, unavailable build features, or deny policy. Do not broaden permissions to fix a name.
- Removed public `capability__*` names fail as unknown tools. Use native agent tools, not self-MCP capability aliases.
- Stale responses after restart are expected. Never replay an effect to reconstruct its consent prompt.
- Migration-history mismatches require targeted recovery; preserve credential stores and their backup/key relationship.
- Missing or corrupt referenced payloads fail explicitly. Restore the exact approved backup unit; catalog bytes and SQL bodies supply no fallback.

## References

- [Agent and host boundaries](../architecture/agent.md)
- [Persistence](../architecture/persistence.md) and [Deployment](../deployment.md)
- [Session backend selection](../../crates/waltr-server/src/session/storage.rs)
- [Session stores](../../crates/waltr-agent/src/agent/session/storage/)
- [SQLite migrations](../../crates/waltr-server/migrations/sqlite/) and [PostgreSQL migrations](../../crates/waltr-server/migrations/postgresql/)
