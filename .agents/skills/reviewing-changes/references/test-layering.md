# Test Layering

## Invariant

Tests assert behavior at the owning layer and add integration coverage only where contracts cross layers.

## Why It Matters

Layered tests catch drift without making UI tests re-prove server internals or unit tests depend on external services.

## Source Files To Inspect

- `docs/testing.md`
- `crates/waltr-agent/tests/AGENTS.md`
- `crates/waltr-agent/tests/live/AGENTS.md`
- `crates/waltr-server/tests/`
- `crates/waltr-client/tests/`
- `crates/waltr-web/tests/AGENTS.md`
- `tests/web/k6/`

## Common Drift Signs

- UI tests assert server validation details instead of visible behavior.
- Reducer behavior is only covered through slow end-to-end tests.
- Live provider or external service tests are not ignored or isolated.
- Contract changes lack server/client integration tests.

## Suggested Tests

Choose unit tests for pure logic, crate integration tests for contracts and orchestration, UI tests for presentation behavior, and live/ignored tests for real external dependencies.

## Finding Format

`[test-layering] <path>:<line> tests <behavior> at <wrong/missing> layer; expected <layer> coverage because <owner/contract>.`
