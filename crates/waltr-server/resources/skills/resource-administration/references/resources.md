# Resource Contract

All calls use tool `exec`, an `action`, and action-specific `input`.

| Action | Input | Result |
| --- | --- | --- |
| `resources.inspect` | Optional `path` | Catalog entries, or one resource with source, origin, persisted state, and persisted-slot ETag |
| `resources.replace` | `path`, `source`, `expected_etag` | Inspected resource after validation and atomic replacement |
| `resources.reset` | `path`, `expected_etag` | Inspected resource after removal of the persisted override |

Paths are canonical relative paths, such as `agents/coding.yaml`. Use exactly
the `persisted_etag` returned by inspection as `expected_etag`; an effective
source hash is not a substitute. Even a slot without a persisted override has
an inspection ETag.

Replacement source must describe exactly one strict YAML resource. Validation
checks the candidate catalog before publishing it. Reset can fail validation
if the resulting fallback is invalid in the current environment. An ETag
mismatch rejects the mutation. Inspect the current state before rebuilding it.

Example inspection:

```json
{"action":"resources.inspect","input":{"path":"agents/coding.yaml"}}
```

`filter` may select fields from a successful tool result using jq. Preserve the
source and ETag needed for the next edit rather than filtering them away.
