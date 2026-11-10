# Agent Session Replay Chronology

## Invariant

Durable session replay is based on persisted timeline and journal records in chronological order, not transient provider parser state.

Public complete-record revisions remain distinct from incarnation-bound execution tokens. Atomic append receipts supply coherent metadata and cursor facts before publication. Strict public CAS/context predicates remain independent. See [session storage](../../../../docs/agent/storage.md#revision-domains-and-append-receipts).

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
- Metadata-only writes invalidate execution, or execution writes leave stale tokens valid.
- Callers guess public revisions, discard receipt metadata, or substitute execution tokens for strict source predicates.
- Compatibility reads rewrite stores, seed absent sequences with zero, or invent durable incarnations.
- Physical CAS retries include uncertain outcomes, or mixed old/new complete-record writers share a store.

## Suggested Tests

Add deterministic agent tests that persist, replay, resume, and compare timeline order across memory and durable storage paths when feasible.

## Finding Format

`[agent-session-replay-chronology] <path>:<line> can replay <event> out of order because <record/source> is not chronological.`
