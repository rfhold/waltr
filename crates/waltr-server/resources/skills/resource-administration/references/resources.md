# Resource Contract

Ordinary reads use MCP resources, not tools.

| URI | Content |
| --- | --- |
| `waltr://resources` | JSON catalog with entry `uri` and `source_uri` |
| `waltr://resources/{path}` | JSON slot metadata, exact sources, ETags, and guarded `revision` |
| `waltr://resources/{path}/source` | Exact effective source with MIME type `application/yaml` |

Encode the entire canonical path as one URI component: `agents/coding.yaml` becomes `agents%2Fcoding.yaml`.
Use uppercase percent escapes for every byte outside ASCII letters, digits, `-`, `.`, `_`, and `~`.
The server rejects URI aliases, query strings, fragments, malformed escapes, and control characters.
Catalog path validation rejects traversal and unknown resource kinds.

| Tool | Action | Input |
| --- | --- | --- |
| `create` | `resource.create` | Entry `uri`, complete strict YAML `source`, `expected_revision` |
| `edit` | None; direct arguments | Entry `uri`, `expected_revision`, ordered `edits` |
| `destroy` | `resource.destroy` | Entry `uri`, `expected_revision` |

Use the metadata entry URI, not the `/source` URI, for mutations.
Successful mutations return current slot metadata.
Create requires an absent persisted slot, including a slot with bundled fallback.
Destroy requires a persisted override; it restores bundled fallback or removes the unbundled slot.
Candidate catalog validation precedes each write or removal.
Invalid fallback or strict YAML rejects the mutation.

`revision` equals the persisted-slot ETag when a persisted file exists.
For absent slots, `revision` combines the absent ETag and effective-source hash with `@`.
The effective-source hash is `absent` when no effective source exists.
Copy the complete token into `expected_revision`; do not construct it yourself.
Edits use `source`, never `active_source`, and reject stale revisions.
The server rechecks the persisted ETag and exact effective source under the catalog mutation lock.
An edit to bundled source creates an override only if both guards match.
An absent source requires create instead of edit.
Rejected reloads can leave different effective and last-known-good active sources; metadata exposes both explicitly.

Text edits have this schema:

```json
{"uri":"waltr://resources/agents%2Fcoding.yaml","expected_revision":"<revision>","edits":[{"operation":"replace","old_text":"description: old","new_text":"description: new"}]}
```

Pass this object directly as the `edit` tool arguments, without an `action` or `input` envelope.
The edit tool rejects obsolete envelopes and extra fields, including `filter`.

Each `replace` requires a nonempty, unique `old_text` match.
Each `insert` requires nonempty `text` and `placement`: `start`, `end`, `before`, or `after`.
`before` and `after` require a nonempty, unique `anchor`.
`start` and `end` reject any `anchor` field, including explicit null.
`before` and `after` also reject a null anchor.
The list contains 1 through 256 edits.
Each edit operates on the previous edit's output.
No-op edits and a no-op final result fail.
All edits and candidate validation finish before one atomic replacement against the original revision.

For create and destroy, `filter` optionally selects fields from a successful tool result through jq.
Preserve the source and revision for the next edit.
