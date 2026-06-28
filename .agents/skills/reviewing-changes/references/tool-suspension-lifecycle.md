# Tool Suspension Lifecycle

## Invariant

Tool execution, permission checks, suspensions, resume inputs, middleware hooks, and tool results flow through the agent loop.

## Why It Matters

Centralizing tool lifecycle preserves permission policy, durable session records, and host-visible suspension semantics.

## Source Files To Inspect

- `crates/waltr-agent/src/agent/loop.rs`
- `crates/waltr-agent/src/agent/session/`
- `crates/waltr-agent/src/tools/mod.rs`
- `crates/waltr-agent/src/permission/mod.rs`
- `crates/waltr-agent/src/middleware/mod.rs`
- `docs/architecture/agent.md`

## Common Drift Signs

- Tool execution bypasses permission claims.
- Suspensions are represented only in provider adapter state.
- Resume input is accepted without matching the suspended tool call.
- Middleware cannot observe tool request, result, or suspension transitions.

## Suggested Tests

Add deterministic agent tests for denied permissions, suspension persistence, resume matching, tool result ordering, and middleware observation.

## Finding Format

`[tool-suspension-lifecycle] <path>:<line> bypasses <permission/session/middleware> during <tool lifecycle step>.`
