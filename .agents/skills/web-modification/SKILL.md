---
name: web-modification
description: Helps an agent modify Waltr web when the user asks to change web UI behavior, routing, forms, or transitions.
---

# Web Modification

Maintain feature equivalence with the TUI while keeping web-specific code focused on routing, forms, and UI state.

## When To Use

Use this skill when changing `crates/waltr-web/`, browser routing, Solid components, forms, dialogs, tabs, transitions, app context, WASM client wiring, CSS, or web tests.

Do not use this skill for shared server-data behavior that belongs in `waltr-client` unless the web change exposes where that shared behavior is needed.

## Core Boundary

`waltr-web` owns browser interaction and presentation. It loads the WASM client bridge, consumes `ClientSnapshot`, dispatches `ClientCommand`, and keeps browser-only state local. Web behavior should stay feature-equivalent with TUI for shared user-facing flows while preserving browser-specific UX patterns.

## Modification Patterns

1. Decide whether the change is browser-specific or shared with TUI.
2. Keep route params, navigation, form drafts, dialogs, tabs, transitions, DOM state, and CSS in web code.
3. Dispatch `ClientCommand` for server-data mutations and watch interest changes.
4. Render from `ClientSnapshot`; do not duplicate shared config/status state machines in Solid state.
5. Use `AppStateProvider` and `useAppState` for client loading, snapshots, readiness, and dispatch.
6. Keep reusable UI primitives in `components/ui/` and route-specific behavior in `routes/`.
7. Keep browser tests focused on user-visible browser behavior rather than re-testing server internals.
8. Compare TUI behavior when changing shared flows so labels, empty states, errors, and success paths remain equivalent where the platforms overlap.

## Test Selection

- Use `pnpm --filter @rfhold/waltr-web test` for browser routing, forms, dialogs, tabs, transitions, app context wiring, component behavior, and UI-specific watch rendering.
- Use `pnpm test:web:integration` for browser-level server-backed k6 flows that need a real server, real web build, and end-to-end user navigation.
- Add or update `tests/web/k6/` when the browser flow depends on actual server coordination, gRPC-web/WASM wiring, readiness, config persistence, or watch behavior.
- Keep web unit tests for UI-state-heavy cases that can use test doubles instead of a real server.

## Commands

Run web tests:

`pnpm --filter @rfhold/waltr-web test`

Run web server-backed k6 tests:

`pnpm test:web:integration`

Build the web client:

`pnpm --filter @rfhold/waltr-web build`

## References

| Path | Info |
| --- | --- |
| [../../../docs/architecture/clients.md](../../../docs/architecture/clients.md) | Shared client state, command, snapshot, and UI implementation contract. |
| [../../../docs/testing.md](../../../docs/testing.md) | Canonical layered test structure and commands. |
| [../../../crates/waltr-web/AGENTS.md](../../../crates/waltr-web/AGENTS.md) | Web crate boundary and parity guidance. |
| [../../../crates/waltr-web/src/app/state.tsx](../../../crates/waltr-web/src/app/state.tsx) | WASM client loading, snapshot subscription, readiness, and dispatch provider. |
| [../../../crates/waltr-web/src/app/context.tsx](../../../crates/waltr-web/src/app/context.tsx) | App state context contract consumed by routes. |
| [../../../crates/waltr-web/src/routes/](../../../crates/waltr-web/src/routes/) | Browser routes and route-specific UI behavior. |
| [../../../crates/waltr-web/src/components/ui/](../../../crates/waltr-web/src/components/ui/) | Shared browser UI primitives. |
| [../../../crates/waltr-web/tests/AGENTS.md](../../../crates/waltr-web/tests/AGENTS.md) | Web test boundaries and structure. |
| [../../../tests/web/k6/](../../../tests/web/k6/) | Browser server-backed end-to-end flows. |
