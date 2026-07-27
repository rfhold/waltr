# Boundaries

Waltr web owns browser routing, forms, transitions, and UI-specific state.

# Dependencies

| Path | Info |
| --- | --- |
| [../waltr-client/](../waltr-client/) | Web should defer server data state to the shared client state machine. |

# Contracts

Web behavior should stay feature-equivalent with TUI while keeping browser-specific state local.

Waltr access login navigates to server-owned `/oidc/login` with a safe local return path; the browser never handles provider tokens or the HttpOnly Waltr session cookie. Logout is same-origin POST before dispatching shared protected-state clearing. Keep this UI separate from provider/upstream-MCP `OAuthDialog`; server-callback MCP attempts poll shared lifecycle state and never expose token or PKCE material.

# Hints

Use the web-modification skill when modifying this crate.

Compare [../waltr-tui/](../waltr-tui/) behavior when changing shared user-facing flows.
