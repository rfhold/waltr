# Boundaries

Waltr TUI owns terminal routing, input, rendering, and UI-specific state.

# Dependencies

| Path | Info |
| --- | --- |
| [../waltr-client/](../waltr-client/) | TUI should defer server data state to the shared client state machine. |

# Contracts

TUI behavior should stay feature-equivalent with web while keeping terminal-specific state local.

Native Waltr login discovers only the API-root RFC 9728 document at `/.well-known/oauth-protected-resource`, verifies its resource against `WALTR_SERVER_ENDPOINT`, and sends that API resource in its public-client Authorization Code with PKCE request. It must not derive `/mcp` or consume path-specific MCP metadata. It also owns loopback callback validation, process-memory token refresh/expiry, browser launching, and copyable URL state. Keep it separate from provider/upstream-MCP OAuth and preserve credential-lease generation checks.

Transient expiry remembers and restores the protected route after reauthentication without releasing and reacquiring its ownership. Explicit logout clears protected drafts, overlays, notices, and route ownership before the anonymous access reprobe.

Provider/upstream-MCP authorization uses shared attempt state. Keep its copyable URL, manual completion, retry, cancellation, and target labeling separate from native Waltr access login; never place outbound token or PKCE material in TUI state.

`WALTR_OIDC_NATIVE_CLIENT_ID` is required at login; native scopes, audience request parameter, and loopback policy are optional deployment settings documented in [../../docs/deployment.md](../../docs/deployment.md).

# Hints

Use the tui-modification skill when modifying this crate.

Compare [../waltr-web/](../waltr-web/) behavior when changing shared user-facing flows.
