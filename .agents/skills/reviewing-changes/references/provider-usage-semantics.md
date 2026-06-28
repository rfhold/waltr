# Provider Usage Semantics

## Invariant

Provider adapters translate provider-specific protocol details into provider-neutral requests, stream events, turns, tool calls, thinking, and metadata.

## Why It Matters

Host layers and tests should reason about Waltr's neutral agent contract, not each provider's transient wire format.

## Source Files To Inspect

- `crates/waltr-agent/src/provider/`
- `crates/waltr-agent/src/stream.rs`
- `crates/waltr-agent/src/agent/loop.rs`
- `crates/waltr-agent/tests/agent/`
- `crates/waltr-agent/tests/live/`
- `docs/architecture/agent.md`

## Common Drift Signs

- Provider-specific enum variants leak into host-facing APIs without a neutral abstraction.
- Stream parsing emits events that cannot be replayed or persisted neutrally.
- Reasoning, thinking, or continuation metadata is lost or attached to the wrong turn.
- Scripted-provider tests no longer exercise the neutral contract.

## Suggested Tests

Add scripted provider tests for neutral event shape and adapter unit tests for provider-specific parsing; reserve real-provider checks for ignored live tests.

## Finding Format

`[provider-usage-semantics] <path>:<line> leaks or loses <provider detail> instead of mapping it to <neutral type/event>.`
