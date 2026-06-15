# Boundaries

Waltr web owns browser routing, forms, transitions, and UI-specific state.

# Dependencies

| Path | Info |
| --- | --- |
| [../waltr-client/](../waltr-client/) | Web should defer server data state to the shared client state machine. |

# Contracts

Web behavior should stay feature-equivalent with TUI while keeping browser-specific state local.

# Hints

Use the web-modification skill when modifying this crate.

Compare [../waltr-tui/](../waltr-tui/) behavior when changing shared user-facing flows.
