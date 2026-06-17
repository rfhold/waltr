# Middleware

Agent middleware is the internal extension system used by `crates/waltr-agent/` to shape prompts, tools, provider requests, tool execution, suspensions, and durable middleware state. Hosts configure middleware-backed features through public builder methods; the custom middleware trait itself is not a public extension API.

## Role

Middleware sits between the configured `Agent`, the provider-neutral request model, tool execution, and durable session storage.

It is responsible for:

- Building durable system prompt content.
- Listing, hiding, and annotating tools before a provider request.
- Adding permission claims before tool execution.
- Suspending a run for external questions or approvals.
- Recording middleware state needed for deterministic durable replay.

## Public Configuration Surface

Middleware behavior is exposed through agent configuration APIs, not through a public middleware trait.

| API | Middleware-backed Behavior |
| --- | --- |
| `AgentBuilder::with_base_system_prompt` | Captures and replays the base system prompt. |
| `AgentBuilder::with_system_prompt` | Adds host-provided prompt fragments. |
| `AgentBuilder::with_filesystem_system_prompt` | Adds filesystem guidance. |
| `AgentBuilder::with_agents_system_prompt` | Adds static `AGENTS.md` instructions. |
| `AgentBuilder::with_skills` | Adds skill catalog prompt context and `load_skill`. |
| `AgentBuilder::with_question_tool` | Adds question suspension behavior. |
| `AgentBuilder::with_permission_policy` | Enables policy-driven tool visibility and runtime decisions. |
| Run overrides | Can override tools, prompt content, permission policy, and session store for a run. |

## Built-In Middleware

| Middleware Area | Behavior |
| --- | --- |
| System prompt | Stores prompt fragments in durable middleware state and replays them on resume. |
| `AGENTS.md` | Loads configured instruction files and dynamic read-context reminders. |
| Skills | Adds a visible skill catalog, exposes `load_skill`, and reveals dynamic skill catalogs. |
| Tool visibility | Filters tools from provider requests when policy denies whole-tool access. |
| Tool metadata | Adds request-scoped filesystem root hints. |
| Claims | Adds filesystem and shell permission claims before tool execution. |
| Question suspension | Converts question tool calls into external requests for the host. |

Cost, pricing, usage, secrets, and OpenTelemetry middleware are not implemented agent middleware today.

## Hook Model

The internal `AgentMiddleware` trait defines the wired hooks for:

- Building system prompt fragments.
- Listing and editing tools/tool metadata.
- Inspecting provider requests.
- Running before and after tool calls.
- Handling suspensions.

Hook outcomes include continue, replace tool result, and suspend. Chain execution is deterministic and generally short-circuits on the first non-continue outcome for outcome-producing hooks.

## Ordering

Middleware order is part of the request-construction contract because it can affect tool visibility and provider request hashes.

Agent construction assembles middleware in this order:

1. System prompt middleware.
2. Feature and internal middleware in registration order.
3. `AGENTS.md` instruction middleware when configured.
4. Skills middleware when configured.

At run time, the permission-filtered tool registry is prepended before the agent's default chain. Provider request construction then builds the system prompt, lists tools, edits tool metadata, and runs request hooks before dispatching to the provider.

## Durable State

Middleware state is namespaced and journaled through the session store. State writes that influence provider request construction are persisted before the provider request is dispatched. This allows resumed sessions to reconstruct prompt and context decisions from persisted state instead of re-reading all ambient files.

## Boundaries

- `AgentMiddleware` is an internal crate trait, not a stable public plugin API.
- Hosts should use public builder methods and run configuration rather than importing middleware internals.
- Middleware should produce provider-neutral effects; provider-specific request encoding belongs in provider adapters.
- Middleware state should be durable when it affects replay correctness.

## Tests

See [../testing.md](../testing.md) for canonical test commands.

Focused coverage:

| Path | Covers |
| --- | --- |
| [../../crates/waltr-agent/src/middleware/chain.rs](../../crates/waltr-agent/src/middleware/chain.rs) | Chain order and hook outcomes. |
| [../../crates/waltr-agent/src/middleware/prompt.rs](../../crates/waltr-agent/src/middleware/prompt.rs) | Durable prompt state. |
| [../../crates/waltr-agent/src/middleware/tools.rs](../../crates/waltr-agent/src/middleware/tools.rs) | Permission-filtered tool visibility. |
| [../../crates/waltr-agent/src/middleware/tool_metadata.rs](../../crates/waltr-agent/src/middleware/tool_metadata.rs) | Request-scoped root hints. |
| [../../crates/waltr-agent/src/middleware/claims.rs](../../crates/waltr-agent/src/middleware/claims.rs) | Filesystem and shell claim construction. |
| [../../crates/waltr-agent/src/middleware/question_suspension.rs](../../crates/waltr-agent/src/middleware/question_suspension.rs) | Question external-request suspension. |
| [../../crates/waltr-agent/tests/agent/loop.rs](../../crates/waltr-agent/tests/agent/loop.rs) | Deterministic loop behavior that exercises middleware. |
| [../../crates/waltr-agent/tests/agent/session.rs](../../crates/waltr-agent/tests/agent/session.rs) | Durable middleware state and resume behavior. |

## References

| Path | Purpose |
| --- | --- |
| [../../crates/waltr-agent/src/middleware/mod.rs](../../crates/waltr-agent/src/middleware/mod.rs) | Middleware trait, hook outcomes, external requests, and public module boundary. |
| [../../crates/waltr-agent/src/middleware/chain.rs](../../crates/waltr-agent/src/middleware/chain.rs) | Chain execution. |
| [../../crates/waltr-agent/src/middleware/context.rs](../../crates/waltr-agent/src/middleware/context.rs) | Middleware context and durable state writes. |
| [../../crates/waltr-agent/src/agent/builder.rs](../../crates/waltr-agent/src/agent/builder.rs) | Builder wiring and middleware assembly order. |
| [../../crates/waltr-agent/src/agent/run.rs](../../crates/waltr-agent/src/agent/run.rs) | Run-time chain construction and overrides. |
| [../../crates/waltr-agent/src/agent/loop.rs](../../crates/waltr-agent/src/agent/loop.rs) | Middleware calls in provider request and tool execution flow. |
