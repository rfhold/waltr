# Permissions

Agent permissions decide whether host-affecting tool operations are allowed, denied, hidden, or suspended for host approval. The provider-neutral policy and decisions live in `crates/waltr-agent/`; `crates/waltr-agent-sandbox/` prepares workspace resource claims without deciding policy.

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
| `PermissionResponse` | Typed host decision for permission asks: allow once, allow always, or deny. |

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
| Tool execution | The agent loop collects claims after pre-tool middleware, evaluates policy, and applies the outcome before running the tool. For sandbox-backed tools it asks the worker to prepare claims and a ticket, then persists a dispatch fence before Execute. |

The worker generates detailed filesystem and shell claims for the operations it infers. Generic tools, including MCP-backed tools, can still be controlled at the tool-selector level. An ask or deny cancels an unused worker ticket. Once dispatched, an unknown result suspends rather than retrying a side effect; see [agent architecture](../architecture/agent.md#private-workspace-boundary).

## Outcomes

| Outcome | Runtime Behavior |
| --- | --- |
| Allow | The tool executes and its result is recorded. |
| Ask | The run suspends with an external permission request for the host. |
| Deny | The tool is not executed; the agent receives a permission error as the tool result. |

An ask outcome persists an external request in the session. Resuming with a permission response records the response, primes durable permission runtime state, and retries the originally suspended tool call through the normal permission and tool-execution path.

Permission responses support:

- `allow_once`: allow the suspended tool call once, then consume the approval.
- `allow_always`: persist reusable approval patterns and allow the suspended tool call.
- `deny`: either return a denial tool result to the model or cancel pending work, depending on the response mode.

## Defaults

Agent defaults are intentionally library-level defaults, not product policy:

- A bare `PermissionPolicy` falls back to allow.
- Enabling filesystem tools without an explicit policy merges a default filesystem policy for the configured root.
- Shell command execution should be controlled with explicit policy when a host needs stricter behavior.

Hosts that need safe-by-default user approvals should provide an explicit policy instead of relying on the generic fallback.

## Durable Behavior

Permission asks are part of the session model. The agent stores pending external requests, associated tool calls, responses, timeline events, and internal journal records through the configured `SessionStore`.

The agent persists allow-once approvals and allow-always patterns under the `permissions` middleware namespace.
One-shot approvals survive all pre-dispatch suspensions for the same request-bound call.
The agent clears them at final execution or result, denial, cancellation, or prompt supersession.
Reusable patterns apply to later matching permission claims in the same session.

Ordinary permission request IDs include a SHA-256 digest of the approval stage, policy identity, tool name, full arguments, and claim identities.
Changed arguments require fresh one-shot consent. Approvals with earlier request-ID formats cannot authorize a current request; the agent asks again.
Remote [MCP Skills](mcp.md#mcp-skills) require additional manifest-bound and per-call consent that generic reusable patterns cannot satisfy.

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
| [../../crates/waltr-agent-sandbox/src/tools.rs](../../crates/waltr-agent-sandbox/src/tools.rs) | Worker filesystem claim preparation. |
| [../../crates/waltr-agent/src/middleware/tools.rs](../../crates/waltr-agent/src/middleware/tools.rs) | Tool visibility filtering. |
| [../../crates/waltr-agent/tests/agent/permissions.rs](../../crates/waltr-agent/tests/agent/permissions.rs) | Deterministic permission integration, suspension, resume, and cancellation behavior. |
| [../../crates/waltr-agent/tests/live/agent/permissions.rs](../../crates/waltr-agent/tests/live/agent/permissions.rs) | Ignored live-provider permission coverage. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/permission/mod.rs](../../crates/waltr-agent/src/permission/mod.rs) | Public permission module exports. |
| [../../crates/waltr-agent/src/permission/policy.rs](../../crates/waltr-agent/src/permission/policy.rs) | Policy representation and evaluation. |
| [../../crates/waltr-agent/src/permission/rule.rs](../../crates/waltr-agent/src/permission/rule.rs) | Rule matching and specificity. |
| [../../crates/waltr-agent/src/tools/sandbox.rs](../../crates/waltr-agent/src/tools/sandbox.rs) | Worker claim translation into agent policy inputs. |
| [../../crates/waltr-agent/src/middleware/tools.rs](../../crates/waltr-agent/src/middleware/tools.rs) | Tool-list filtering. |
| [../../crates/waltr-agent/src/agent/loop.rs](../../crates/waltr-agent/src/agent/loop.rs) | Runtime permission outcomes and suspension behavior. |
| [../../crates/waltr-agent/src/agent/session/types.rs](../../crates/waltr-agent/src/agent/session/types.rs) | External request and response session types. |
