# Waltr-Served Apps

This guide adds a static, tool-linked HTML app to Waltr's server package.
It describes Waltr repository policy, not a generic MCP asset API.
See [MCP Apps](apps.md) for protocol types and [Clients](../architecture/clients.md#session-mcp-apps) for browser isolation.

The production example is `status`, linked to `ui://waltr/status`.
Apps open from a complete recorded tool call/result pair in the current session.
This path adds no dynamic catalog, navigation entry, resource-first launch, Git/S3 source, cache, or app-specific renderer.
Normal Apple Silicon macOS builds use the same generic host without a diagnostic feature or separate profile.
Other native platforms remain unsupported; no Intel or universal package support follows from this target.
See [macOS Apps verification](../testing.md#macos-native-apps-verification) for live acceptance and the optional diagnostic harness.
Record a fresh `status` call after enablement; older text-only calls do not acquire an Open app action retroactively.

## Add The Source And Build Entry

1. Add `crates/waltr-server/assets/<name>/index.html` with an HTML5 document envelope and a module script entry.
2. Add the app's TypeScript and CSS beside that entry.
3. Add one `<name> assets/<name>` line to `crates/waltr-server/scripts/apps.entries`.

Use a unique name that matches `[a-z][a-z0-9_]*`.
The source root resolves relative to `crates/waltr-server`.
The entry list accepts exactly two whitespace-separated fields per line; it has no comment syntax.
The server `tsconfig.json` already includes `assets`; another app requires no per-app TypeScript configuration.

Use the current server SDK and shared Vite configuration.
See [status source](../../crates/waltr-server/assets/status/status.ts) and [its HTML entry](../../crates/waltr-server/assets/status/index.html).
Keep all scripts, styles, and other assets self-contained; do not depend on external chunks or network assets.

`pnpm --filter @rfhold/waltr-server build` typechecks assets and runs `scripts/build-apps.mjs`.
The script runs an independent standard Vite single-file build for each explicit entry.
Each `dist/<name>/` contains only a nonempty UTF-8 `index.html`, at most 1 MiB.
The output validator rejects external script and stylesheet references.

With `apps-served-assets`, `build.rs` copies each document into `OUT_DIR/<name>.html`.
It generates `served_app_assets.rs` with `<NAME>_HTML: Option<&str>` constants.
Without that feature, every generated constant equals `None`; Cargo runs no served-app package build.
Do not edit generated assets or constants.
See [Deployment](../deployment.md#mcp-apps-assets-and-app-container) for prerequisites and host assets.

## Declare The Resource And Ordinary Tool

1. Add `crates/waltr-server/src/mcp_service/apps/<name>.rs` with a stable `ui://waltr/<name>` URI.
2. Declare an `APP` constant through the shared `AppResource` type.
3. Declare the module in `apps/mod.rs`.
4. Add its `APP` to the explicit `RESOURCES` list.

For an illustrative `example` entry, the module declares:

```rust
pub(crate) const URI: &str = "ui://waltr/example";

pub(super) const APP: super::AppResource = super::AppResource {
    uri: URI,
    name: "Waltr Example",
    description: "An interactive view of the example tool result.",
    html: super::EXAMPLE_HTML,
};
```

`AppResource` owns capability checks, descriptors, content, Apps MIME type, deny-all external CSP, and empty permissions.
The shared server handlers advertise available resources and serve exact URI reads through `RESOURCES`.
Do not add per-app resource dispatch or host behavior.

Register the ordinary tool declaration in `WaltrMcpServer::list_tools` in `src/mcp_service.rs`.
Implement its ordinary execution in `WaltrMcpServer::call_tool`.
Attach `_meta.ui.resourceUri` only when `APP.supported(&context)` returns true:

```rust
if APP.supported(context) {
    definition["_meta"] = serde_json::json!({"ui": {"resourceUri": URI}});
}
```

Use [status.rs](../../crates/waltr-server/src/mcp_service/apps/status.rs) as the declaration example.
Keep tool execution useful without HTML assets or the current request's HTML Apps capability.
Without either prerequisite, omit the UI link; the resource remains unavailable.
The build entry includes bytes, while `RESOURCES` publishes resources; neither automatically registers a tool.

## Use The Host Data Path

Create an SDK `App` with the supported `inline` display mode.
Register callbacks before `app.connect()`.
Use `app.ontoolinput` for the original complete arguments when the app needs them.
Use `app.ontoolresult` for the original result, including `structuredContent`, content, error state, and `_meta`.
The host sends the original input and result once after the handshake.

For Refresh, call `app.callServerTool({ name, arguments })`.
Render its response directly; Refresh does not trigger another original-result notification.
Validate result fields before display.
Preserve the previous valid observation after malformed results or call failures.
Use text-safe DOM updates for tool data.

Handle `app.onteardown` with an idempotent cleanup callback that returns `{}`.
Disable controls and release app-owned listeners, timers, and pending work.
Ignore late responses after teardown.
The host bounds teardown and cancels calls; asset expiry covers lost unload cleanup.
The modal and container ignore `pagehide` with `persisted: true`; final unload releases their resources.

## Respect Current Host Limits

The host supports same-server `tools/call` and bounded display/lifecycle messages.
After the handshake, it also accepts `ping`, `ui/request-display-mode`, `ui/notifications/size-changed`, and `notifications/cancelled`.
It rejects resource reads, model-context updates, model messages, sampling, and external links.
Do not fetch Waltr APIs or upstream HTTP endpoints from the app.
The inner iframe has an opaque origin, no parent DOM access, no Waltr credentials, and no browser permission grants.
Resource CSP requests do not grant external origins; HTTP policy applies before script execution.

Calls retain current source authority, app-visible tool checks, permissions, middleware, and idle-session requirements.
Middleware result changes deny raw-result publication; denial does not undo a tool that already executed.
Approval-required calls have no app approval continuation.
Successful calls append separate audit results without inference or a new session.
Owner-bound Close revokes the handle without a session load, even after session deletion or wipe.

Static Waltr publication does not change external-source eligibility.
Legacy OAuth credentials without an authority identity, static bearers, and ordinary custom bearer providers remain Apps-ineligible.
Custom hosts need the atomic identity/bearer contract in [Agent](../architecture/agent.md#session-mcp-apps).
Native Apps display supports Apple Silicon macOS only.

## Verify The Addition

Use the commands and prerequisites in [MCP Apps tests](../testing.md#mcp-apps).
Add focused coverage for the new app's original input/result, Refresh, malformed results, and teardown.
Update the explicit production-entry expectation in `scripts/apps.contract.mjs` when the production entry list changes.
Check conditional tool linkage and exact resource reads with and without the HTML capability.
Check no-assets behavior: ordinary execution remains available, but UI linkage and resources disappear.
Check independent output, the one-MiB limit, and generated Rust content.

The test-only second app proves real independent Vite output and Rust embedding without production registration.
It does not replace the new app's tool/resource tests.
The status output contract uses a DOM/message stub; it proves neither browser isolation nor full-session authorization.
Browser and native proof limits remain separate from asset build contracts.
