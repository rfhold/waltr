# Server Permissions

The server owns policy evaluation and private approvals. The generic agent runs neutral tool callbacks; the worker prepares workspace claims without deciding policy. Internal concrete tool entry points use the server's `CapabilityDispatcher`, not a second permission path.

## Policy

| Concept | Purpose |
| --- | --- |
| `PermissionClaim` | Requested operation on a filesystem path, command, or typed tool input. |
| `PermissionPolicy` | Ordered rules and fallback effect. |
| `PermissionLevel` | Allow, ask, or deny. |
| `ServerPermissionPolicy` | Evaluates the policy and owns process-local approval receipts and patterns. |
| `InteractionBroker` | Delivers a typed request to the original live operation. |

A bare policy falls back to allow. Resolved agent definitions instead start deny-by-default. Enabled feature defaults add rules only for installed tools. Whole-tool denial also removes tools from provider discovery. More specific rules win; equally specific conflicts prefer deny over ask over allow.

Nested input rules use canonical RFC 6901 pointers and typed scalar equality or one terminal string wildcard. Conditions within a rule are ANDed. Missing pointers do not match; selected objects or arrays deny closed. Precedence is condition count, exact-condition count, total wildcard-prefix length, effect conflict rank, then later composed position. See [Agent definitions](../architecture/agent.md#durable-ownership).

## Dispatch

1. Resolve the exact installed capability, declaration revision, session incarnation, and operation binding.
2. Apply the whole-tool gate before concrete before-tool middleware. Middleware cannot redirect the tool name.
3. For worker tools, Prepare the final arguments and translate the worker's claims.
4. Authorize the final inputs and claims. Cancel an unused worker ticket on denial or cancellation.
5. Execute only after the applicable dispatch guard and durable host fence.

Allow runs the tool. Deny returns a tool error without the dependent effect. Ask awaits a typed interaction in the same live future; answering does not rerun the model, JavaScript, or tool setup. Unknown outcomes after dispatch are not automatically replayed. See [private workspace boundary](../architecture/agent.md#private-workspace-boundary).

## Approval Lifetime

Approvals are private server memory, never session metadata, middleware state, journal entries, or timeline messages.

- Allow once records an exact operation receipt binding capability identity, policy, final arguments, and claims. Completion clears it.
- Allow always stores selected suggested patterns for the same session incarnation, exact capability, and policy identity. It cannot override a deny and is not permanent across restart or runtime retirement.
- Deny returns an error; cancel-work also cancels the original operation.
- Remote [MCP Skills](mcp.md#mcp-skills) require separate manifest-bound and per-call consent. Generic remembered patterns do not satisfy that consent.

The broker exposes full-replacement snapshots with a process generation and revision. Responses bind the exact session, generation, request, and operation. Only one valid authorized response wins. Owner cancellation or drop removes pending requests; observer disconnect does not decide them. Restart discards pending requests and approvals, with no replay or compatibility resume API.

Public MCP no longer discovers or dispatches `capability__*` tools; no sessionless full-access capability policy applies. Internal agents retain native tools and normal definition permissions. Public management tools, resources, and Apps retain their own contracts. Explicit code execution grants and leases remain separate; public code cannot proxy upstream concrete MCP targets.

## Sources And Tests

| Path | Covers |
| --- | --- |
| [permission/](../../crates/waltr-server/src/permission/) | Rule matching, input rules, and private live approvals. |
| [dispatcher.rs](../../crates/waltr-server/src/agent_host/dispatcher.rs) | Shared admission, middleware, claims, and dispatch. |
| [interaction.rs](../../crates/waltr-server/src/interaction.rs) | Snapshot/watch, response identity, cancellation, and live waiters. |
| [interaction_service.proto](../../proto/waltr/session/v1/interaction_service.proto) | Typed native and browser contract. |
| [host permission tests](../../crates/waltr-server/tests/agent_host/permissions.rs) | Host permission flows. |

See [Testing](../testing.md) for commands and [coordinated upgrade](../runbooks/server-capability-cutover.md) for incompatible retained history.
