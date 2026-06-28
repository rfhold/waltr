# Layer Ownership

## Invariant

Each crate owns only its layer: agent owns hostable model/tool/session primitives, server owns coordination and RPC contracts, client owns shared server-data state, and UI crates own presentation.

## Why It Matters

Cross-layer ownership creates duplicated state machines, circular assumptions, and behavior that cannot be reused by both UI shells.

## Source Files To Inspect

- `crates/waltr-agent/AGENTS.md`
- `crates/waltr-server/AGENTS.md`
- `crates/waltr-client/AGENTS.md`
- `crates/waltr-web/AGENTS.md`
- `crates/waltr-tui/AGENTS.md`
- `docs/architecture/agent.md`
- `docs/architecture/clients.md`

## Common Drift Signs

- UI code performs server-data coordination instead of dispatching client commands.
- Server code encodes browser or terminal presentation policy.
- Agent code depends on server, client, web, or TUI crates.
- Shared behavior is implemented separately in web and TUI.

## Suggested Tests

Use tests at the owning layer, then add UI tests only for presentation differences and integration tests only for cross-layer contracts.

## Finding Format

`[layer-ownership] <path>:<line> moves <behavior> into <wrong layer>; expected owner is <owning layer> because <contract>.`
