# Boundaries

Waltr TUI owns terminal routing, input, rendering, and UI-specific state.

# Dependencies

| Path | Info |
| --- | --- |
| [../waltr-client/](../waltr-client/) | TUI should defer server data state to the shared client state machine. |

# Contracts

TUI behavior should stay feature-equivalent with web while keeping terminal-specific state local.

# Hints

Use the tui-modification skill when modifying this crate.

Compare [../waltr-web/](../waltr-web/) behavior when changing shared user-facing flows.
