# Permissions

Agent permissions decide whether host-affecting tool operations are allowed, denied, hidden, or suspended for host approval. The permission system is provider-neutral and lives entirely in `crates/waltr-agent/`.

## Concepts

| Concept | Purpose |
| --- | --- |
| `PermissionClaim` | A requested operation on a resource, produced before tool execution. |
| `PermissionOperation` | Operation class such as read, write, delete, execute, or tool access. |
| `PermissionResource` | Resource being accessed, such as a path, shell command, or tool. |
| `PermissionPolicy` | Ordered rule set plus fallback effect. |
| `PermissionEffect` | Resulting behavior: allow, ask, or deny. |
| `ToolSelector` | Rule target for an exact or patterned tool name. |
| Path and command patterns | Resource matchers used by filesystem and shell policies. |

## Policy Evaluation

Policy fallback defaults to allow unless a caller supplies stricter rules. Rules can target:

- Whole tools by selector.
- Operation classes.
- Filesystem directories or path patterns.
- Shell command patterns.
- Other resources represented by permission claims.

More specific matching rules win. When equally specific matching rules conflict, deny takes priority over ask, and ask takes priority over allow.

## Where Permissions Apply

Permissions are applied in two places during an agent turn.

| Phase | Behavior |
| --- | --- |
| Tool listing | Middleware can hide tools from provider requests when whole-tool access is denied. |
| Tool execution | Middleware collects claims, the policy evaluates them, and the agent loop applies the outcome before running the tool. |

Filesystem and shell tools generate detailed claims for the operations they infer. Generic tools, including MCP-backed tools, can still be controlled at the tool-selector level.

## Outcomes

| Outcome | Runtime Behavior |
| --- | --- |
| Allow | The tool executes and its result is recorded. |
| Ask | The run suspends with an external permission request for the host. |
| Deny | The tool is not executed; the agent receives a permission error as the tool result. |

An ask outcome persists an external request in the session. Resuming with an external response records the response and a synthetic permission tool result; it does not automatically re-run the originally suspended tool call.

## Defaults

Agent defaults are intentionally library-level defaults, not product policy:

- A bare `PermissionPolicy` falls back to allow.
- Enabling filesystem tools without an explicit policy merges a default filesystem policy for the configured root.
- Shell command execution should be controlled with explicit policy when a host needs stricter behavior.

Hosts that need safe-by-default user approvals should provide an explicit policy instead of relying on the generic fallback.

## Durable Behavior

Permission asks are part of the session model. The agent stores pending external requests, associated tool calls, responses, timeline events, and internal journal records through the configured `SessionStore`.

The current implementation does not have a persisted allow-once or allow-always grant store. Reusable command or path patterns may be suggested by lower-level permission helpers, but the agent loop does not yet consume durable approval patterns.

## Boundaries

- Permission decisions are runtime behavior; UI presentation belongs to the host layer.
- The agent permission model does not configure server, TUI, web, or shared client policy by itself.
- Denied whole tools can be hidden from provider requests, but denied runtime operations are returned as tool errors.
- External approval handling requires a host that can surface and answer external requests.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/src/permission/mod.rs](../../crates/waltr-agent/src/permission/mod.rs) | Permission rule and pattern unit coverage. |
| [../../crates/waltr-agent/src/middleware/claims.rs](../../crates/waltr-agent/src/middleware/claims.rs) | Claim construction before tool execution. |
| [../../crates/waltr-agent/src/middleware/tools.rs](../../crates/waltr-agent/src/middleware/tools.rs) | Tool visibility filtering. |
| [../../crates/waltr-agent/tests/agent/permissions.rs](../../crates/waltr-agent/tests/agent/permissions.rs) | Deterministic permission integration, suspension, resume, and cancellation behavior. |
| [../../crates/waltr-agent/tests/live/agent/permissions.rs](../../crates/waltr-agent/tests/live/agent/permissions.rs) | Ignored live-provider permission coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/permission/mod.rs](../../crates/waltr-agent/src/permission/mod.rs) | Public permission module exports. |
| [../../crates/waltr-agent/src/permission/policy.rs](../../crates/waltr-agent/src/permission/policy.rs) | Policy representation and evaluation. |
| [../../crates/waltr-agent/src/permission/rule.rs](../../crates/waltr-agent/src/permission/rule.rs) | Rule matching and specificity. |
| [../../crates/waltr-agent/src/middleware/claims.rs](../../crates/waltr-agent/src/middleware/claims.rs) | Middleware claim collection. |
| [../../crates/waltr-agent/src/middleware/tools.rs](../../crates/waltr-agent/src/middleware/tools.rs) | Tool-list filtering. |
| [../../crates/waltr-agent/src/agent/loop.rs](../../crates/waltr-agent/src/agent/loop.rs) | Runtime permission outcomes and suspension behavior. |
| [../../crates/waltr-agent/src/agent/session/types.rs](../../crates/waltr-agent/src/agent/session/types.rs) | External request and response session types. |
