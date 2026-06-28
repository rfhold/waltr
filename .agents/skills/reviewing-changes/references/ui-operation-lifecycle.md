# UI Operation Lifecycle

## Invariant

UI operations expose clear pending, success, error, conflict, retry, and reset behavior while dispatching shared server-data commands through `waltr-client`.

## Why It Matters

Users need trustworthy feedback, and UI shells should not invent separate operation state machines for shared server data.

## Source Files To Inspect

- `crates/waltr-web/src/routes/`
- `crates/waltr-web/src/app/`
- `crates/waltr-tui/src/`
- `crates/waltr-client/src/view.rs`
- `crates/waltr-client/src/client.rs`
- `docs/architecture/clients.md`

## Common Drift Signs

- Submit buttons do not reflect pending state or allow duplicate submission accidentally.
- Errors persist after successful retry or disappear before the user can act.
- Conflict states are treated as generic success or generic failure.
- UI operation state is disconnected from client command completion.

## Suggested Tests

Add web/TUI tests for visible operation states and client tests for operation result transitions when the behavior is shared.

## Finding Format

`[ui-operation-lifecycle] <path>:<line> loses <pending/success/error/conflict/retry> state for <operation>.`
