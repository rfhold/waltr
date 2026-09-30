# Session Contract

All calls use tool `sessions`, an `action`, and action-specific `input`.

| Action | Input | Result |
| --- | --- | --- |
| `list` | None | `sessions` array, newest activity first |
| `create` | `prompt`, optional `agent_name` | Session id, agent name, run id, disposition, and queue position |
| `get` | `id` | Stable durable session projection |
| `send` | `id`, `prompt` | Session id, run id, disposition, and queue position |
| `wait` | `id`, `jq`, optional `timeout_seconds` | Reason, session projection, and evaluated jq output |
| `interrupt` | `id` | Session projection after idempotent interruption and queued-run cancellation |
| `wipe` | `confirm: true` | `wiped: true` after deleting every hosted session |

The projection includes `status`, `running`, `coordination`, `progress`,
`capabilities`, `active_run`, `current_prompt`, timestamps, inference, metrics,
and metadata. `get` also includes the durable turn, pending work, and pending
external requests. Use `running` rather than inferring activity from `status`.

Prompt disposition can be `started`, `replaced_active`, or `queued`. Queued
submission includes its queue position. Retain the returned run id for
correlation; submission does not imply completed inference.

`wait` evaluates jq against the same projection as `get`. It returns
`condition_met` when any output is neither false nor null,
`session_not_running` when the session has stopped without meeting the
condition, or `timeout` when the deadline expires. The default timeout is 60
seconds; explicit values must be between 1 and 300 seconds. Always check the
reason, failure, suspension, and pending external requests before claiming
success. Waiting does not resolve an external-input suspension.

```json
{"action":"wait","input":{"id":"exact-session-id","jq":".status == \"finished\"","timeout_seconds":60}}
```

`filter` optionally selects fields from a successful result with jq. The
`wait` input `jq` is instead the condition that controls waiting.
