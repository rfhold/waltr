# Boundaries

Waltr web owns browser routing, forms, transitions, and UI-specific state.

# Dependencies

| Path | Info |
| --- | --- |
| [../waltr-client/](../waltr-client/) | Web should defer server data state to the shared client state machine. |

# Contracts

Web behavior should stay feature-equivalent with TUI while keeping browser-specific state local.

Web owns the Apps modal, `AppHost`, and trusted container implementation in `src/mcp-apps/container/`.
`src/mcp-apps/bridge-contract.ts` owns the shared bridge contract; `src/mcp-apps/policy.ts` reexports its constants and validator.
`app-container/container.html` joins the normal browser Vite build, which emits the UI plus `dist/app-container/container.html` and `container.js`.
Keep the container self-contained; Vite assertions reject shared chunks and external asset references.
`pnpm --filter @rfhold/waltr-web build:app-container` selects the container-only Vite mode without WASM; Tauri modes omit it.
Server host-only builds use this package command; server `web` builds reuse container output from one full browser build.
The separate `@rfhold/waltr-server` package owns static served-app sources and independent builds through its explicit `scripts/apps.entries` list.
The web package and generic MCP crate do not own those app builds.
See [Waltr-served Apps authoring](../../docs/mcp/apps-authoring.md) for the source, resource, and ordinary tool addition workflow.
Container tests and isolation fixtures belong in `tests/mcp-apps/`; `playwright.config.mjs` owns the browser suite.
See [Apps tests](../../docs/testing.md#mcp-apps) for focused commands and proof limits.
Prefer valid runtime HTML container metadata over the compiled Vite URL; reject invalid or duplicate metadata.
Use Rust's `nativeApps` capability and container bootstrap for normal Apple Silicon macOS Apps.
Do not require `nativeAppsTest`, a diagnostic feature, or a separate profile for normal Apps.
Keep other native platforms unsupported; do not infer support from browser-engine tests.
Asset URLs are nonsecret and must never expose the private API endpoint or generation bearer.
Older text-only calls do not acquire Open app retroactively; a fresh recorded call must carry linked Apps metadata.
See [macOS Apps verification](../../docs/testing.md#macos-native-apps-verification) for live acceptance and optional sanitized diagnostics.

Waltr access login navigates to server-owned `/oidc/login` with a safe local return path; the browser never handles provider tokens or the HttpOnly Waltr session cookie. Logout is same-origin POST before dispatching shared protected-state clearing. Keep this UI separate from provider/upstream-MCP `OAuthDialog`; server-callback MCP attempts poll shared lifecycle state and never expose token or PKCE material.

Route access and readiness only from authoritative shared snapshots; do not add an embedded or topology bypass. `ClientAdapter.access` is a generic optional capability: the browser adapter supplies `openLogin` and same-origin `logout`, while the Tauri adapter supplies neither. A login-required snapshot without access operations shows no sign-in action, and an authenticated shell without them shows no sign-out action.

Treat native `platform` startup events as adapter-private IPC sequencing inputs, not app context or route presentation. Never expose the loopback endpoint, local bearer, paths, or native failures through WebView state.

Use the generic optional `openAuthorizationAttempt(attemptId)` operation when Rust redacts an outbound authorization URL. Send only the current opaque attempt ID; never reconstruct or request the URL.

Non-persisted unload disposes the active adapter. Persisted `pagehide` keeps it alive for BFCache restoration on `pageshow`.

# Hints

Use the web-modification skill when modifying this crate.

Compare [../waltr-tui/](../waltr-tui/) behavior when changing shared user-facing flows.
