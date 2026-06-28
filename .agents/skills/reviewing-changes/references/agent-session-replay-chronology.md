# Agent Session Replay Chronology

## Invariant

Durable session replay is based on persisted timeline and journal records in chronological order, not transient provider parser state.

## Why It Matters

Replay must reconstruct the user's session, suspensions, tool results, and model turns deterministically across storage backends.

## Source Files To Inspect

- `crates/waltr-agent/src/agent/session/`
- `crates/waltr-agent/src/agent/loop.rs`
- `crates/waltr-agent/src/timeline.rs`
- `crates/waltr-agent/src/stream.rs`
- `crates/waltr-agent/tests/agent/`
- `docs/architecture/agent.md`

## Common Drift Signs

- Replay depends on provider-specific continuation internals.
- Timeline events are persisted after derived state that assumes them.
- Suspensions or resume inputs appear out of order.
- A storage backend returns records without stable chronological ordering.

## Suggested Tests

Add deterministic agent tests that persist, replay, resume, and compare timeline order across memory and durable storage paths when feasible.

## Finding Format

`[agent-session-replay-chronology] <path>:<line> can replay <event> out of order because <record/source> is not chronological.`
