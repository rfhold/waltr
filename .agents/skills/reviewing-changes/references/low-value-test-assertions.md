# Low-Value Test Assertions

## Invariant

Assert observable behavior, not incidental inventory. Require exact counts, membership, or snapshots only when the contract depends on them.

## Why It Matters

Routine additions should not force unrelated fixture updates. A failing test should explain which guarantee broke.

## Source Files To Inspect

- `crates/waltr-server/src/persistence/tests.rs`
- `crates/waltr-server/src/config_service.rs`
- `crates/waltr-server/tests/session/agents.rs`

## Common Drift Signs

- Tests pin the current total of migrations, config resources, or bundled agents.
- Tests list every current entry to prove sorting, presence, or default selection.
- A snapshot fails on an unrelated addition, and the fix only updates the expected inventory.

## Suggested Tests

- Exercise database initialization, upgrade, and reopen with data that must survive or change. Do not count migrations.
- Verify catalog read and watch agree and return a known edit. Do not pin the size of the bundled catalog.
- Check sorting and default selection directly. Use exact membership for a controlled fixture when exclusion or authorization is the contract.

## Finding Format

`[low-value-test-assertion] <path>:<line> pins <inventory> but should verify <observable behavior>.`
