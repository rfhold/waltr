# Host Middleware

The server assembles concrete prompt, tool, workspace, Skills, and MCP middleware under `src/agent_host/`. The generic agent retains neutral request/context hooks and namespaced durable context state. Neither the generic runner nor its middleware owns permissions or interaction continuations.

## Configuration And Order

`HostRuntimeBuilder` configures base and additional system prompts, filesystem guidance, `AGENTS.md`, Skills, question tools, and tool policy. The custom concrete middleware trait is internal, not a stable public plugin API.

Assembly order is deterministic:

1. Base system prompt.
2. Feature and configured middleware in registration order.
3. `AGENTS.md` instructions when enabled.
4. Skills when enabled.

The tool registry applies whole-tool visibility before provider request assembly. Request construction builds the prompt, lists tools, edits metadata, and runs neutral request hooks. Ordering can affect provider request hashes and visibility.

## Concrete Behavior

| Area | Behavior |
| --- | --- |
| System prompt | Captures and replays prompt fragments. |
| `AGENTS.md` | Loads instructions and dynamic read reminders through the worker. |
| Skills | One loader routes explicit local/MCP selectors; remote activation adds manifest verification and consent. |
| MCP resources | Publishes raw reads with held-manifest verification and opt-in resource templates. |
| Tool visibility and metadata | Filters denied tools and supplies request-scoped root hints. |
| Workspace snapshots | Adds opt-in bounded worker snapshots to prompt context. |
| Questions | Awaits a typed server interaction in the original live tool operation. |

Cost, pricing, usage, secrets, and OpenTelemetry are not concrete agent middleware features.

## Hooks And State

Neutral hooks build prompts, list or annotate tools, inspect requests, and run before/after calls. `HookOutcome` supports continue and replacement results, not an external-request suspension. Concrete host hooks may await the shared `InteractionBroker`; their continuation stays in the live operation rather than a session resume record.

The dispatcher gates the whole capability before concrete before-tool hooks, prohibits name redirection, and evaluates final inputs and worker claims after those hooks. After-tool changes cannot publish an unfiltered MCP Apps raw result. See [Permissions](permissions.md) and [Agent architecture](../architecture/agent.md#session-mcp-apps).

Replay-relevant prompt and context writes are namespaced and persisted before provider dispatch. Captured prompt inputs, instruction text, skill catalogs, and loaded skill bodies survive reconstruction. Private pending interactions, approvals, and consent waiters do not belong in that state.

## Sources And Tests

| Path | Covers |
| --- | --- |
| [host builder](../../crates/waltr-server/src/agent_host/builder.rs) | Concrete middleware assembly. |
| [host middleware](../../crates/waltr-server/src/agent_host/middleware/) | Concrete feature behavior. |
| [dispatcher](../../crates/waltr-server/src/agent_host/dispatcher.rs) | Policy and effect boundary. |
| [neutral hooks](../../crates/waltr-agent/src/middleware/mod.rs) | Generic hook contract. |
| [neutral context](../../crates/waltr-agent/src/middleware/context.rs) | Durable context writes. |
| [host tests](../../crates/waltr-server/tests/agent_host/) | Concrete feature and replay coverage. |

See [Testing](../testing.md) for canonical commands.
