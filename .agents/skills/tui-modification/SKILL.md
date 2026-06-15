---
name: tui-modification
description: Helps an agent modify Waltr TUI when the user asks to change terminal UI behavior, routing, forms, or golden paths.
---

# TUI Modification

Maintain feature equivalence with web while keeping TUI-specific code focused on terminal routing, input, and UI state.

## When To Use

Use this skill when changing `crates/waltr-tui/`, terminal routing, keyboard handling, app state, forms, focus, overlays, notices, rendering, snapshots, runtime wiring, or TUI golden paths.

Do not use this skill for shared server-data behavior that belongs in `waltr-client` unless the TUI change exposes where that shared behavior is needed.

## Core Boundary

`waltr-tui` owns terminal interaction and presentation. It consumes `ClientSnapshot` from `waltr-client`, dispatches `ClientCommand` for shared server-data behavior, and keeps terminal-only state local. TUI behavior should stay feature-equivalent with web for shared user-facing flows.

## Modification Patterns

1. Decide whether the change is terminal-specific or shared with web.
2. Keep terminal routing, key handling, leader commands, focus, notices, overlays, form drafts, and render state in TUI code.
3. Dispatch `ClientCommand` for server-data mutations and watch interest changes.
4. Render from `ClientSnapshot`; do not maintain a second server config/status cache in TUI app state.
5. Update `App` event handling when input changes, and keep `AppAction` as the boundary for runtime side effects.
6. Keep drawing code in UI/screen modules and state transitions in app/screen state modules.
7. Preserve golden paths with deterministic input helpers and stable snapshot rendering.
8. Compare web behavior when changing shared flows so labels, empty states, errors, and success paths remain equivalent where the platforms overlap.

## Test Selection

- Use `cargo test -p waltr-tui --lib` for crate-local app, input, routing, and rendering helper behavior.
- Use `cargo test -p waltr-tui --tests` for non-server-backed terminal routing, input handling, UI transitions, notices, forms, and stable render snapshots.
- Use `cargo test -p waltr-tui --test server_backed` for real-server TUI flows, client/server connectivity paths, config/status flows, and parity with browser k6 flows.
- Mirror the intent of `tests/web/k6/` when adding or changing server-backed user flows.

## Commands

Run TUI unit tests:

`cargo test -p waltr-tui --lib`

Run all TUI integration tests:

`cargo test -p waltr-tui --tests`

Run only TUI server-backed tests:

`cargo test -p waltr-tui --test server_backed`

## References

| Path | Info |
| --- | --- |
| [../../../docs/architecture/clients.md](../../../docs/architecture/clients.md) | Shared client state, command, snapshot, and UI implementation contract. |
| [../../../docs/testing.md](../../../docs/testing.md) | Canonical layered test structure and commands. |
| [../../../crates/waltr-tui/AGENTS.md](../../../crates/waltr-tui/AGENTS.md) | TUI crate boundary and parity guidance. |
| [../../../crates/waltr-tui/src/app.rs](../../../crates/waltr-tui/src/app.rs) | Terminal app state and input event handling. |
| [../../../crates/waltr-tui/src/runtime.rs](../../../crates/waltr-tui/src/runtime.rs) | Runtime wiring between TUI and shared client. |
| [../../../crates/waltr-tui/src/screens/](../../../crates/waltr-tui/src/screens/) | Screen-specific terminal state and forms. |
| [../../../crates/waltr-tui/src/ui.rs](../../../crates/waltr-tui/src/ui.rs) | Ratatui rendering entry point. |
| [../../../crates/waltr-tui/tests/AGENTS.md](../../../crates/waltr-tui/tests/AGENTS.md) | Non-server-backed TUI test boundaries. |
| [../../../crates/waltr-tui/tests/server-backed/AGENTS.md](../../../crates/waltr-tui/tests/server-backed/AGENTS.md) | Server-backed TUI parity contract. |
| [../../../tests/web/k6/](../../../tests/web/k6/) | Browser server-backed parity reference for shared flows. |
