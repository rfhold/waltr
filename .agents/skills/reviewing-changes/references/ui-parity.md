# UI Parity

## Invariant

Web and TUI stay feature-equivalent for shared user-facing flows while preserving platform-specific presentation patterns.

## Why It Matters

Waltr has multiple shells over the same client contract; users should not get different capabilities or meanings by changing UI.

## Source Files To Inspect

- `crates/waltr-web/AGENTS.md`
- `crates/waltr-tui/AGENTS.md`
- `crates/waltr-web/src/routes/`
- `crates/waltr-tui/src/`
- `crates/waltr-client/src/view.rs`
- `docs/architecture/clients.md`

## Common Drift Signs

- One UI exposes a shared action that the other cannot perform.
- Labels, empty states, validation, or errors communicate different meanings.
- One UI handles readiness, conflicts, or retries and the other silently ignores them.
- View-model changes are consumed by only one shell without a reason.

## Suggested Tests

Add shell-specific tests for the same flow and client view tests for shared labels, state, and operation availability when possible.

## Finding Format

`[ui-parity] <path>:<line> makes <flow> differ from <other shell> in <capability/meaning/state>.`
