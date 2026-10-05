# Testing

Waltr uses layered feature coverage. A feature should be tested at each layer it crosses, with assertions focused on that layer's responsibility.

# Structure

| Path | Intent |
| --- | --- |
| `crates/mcp/tests/spec.rs` and `crates/mcp/tests/spec/` | Generic MCP protocol conformance tests grouped by spec revision. |
| `crates/mcp/tests/ecosystem.rs` and `crates/mcp/tests/ecosystem/` | Generic MCP interoperability tests against ecosystem clients and servers. |
| `crates/mcp/tests/streamable_http_*_store.rs` | Feature-gated SQLite and container-backed PostgreSQL MCP store contracts, migrations, concurrency, and safeguards. |
| `crates/mcp/tests/oauth_authorization_server.rs` and `crates/mcp/tests/oauth_store.rs` | Hosted OAuth HTTP flow, DCR/CIMD security, token/revocation/key lifecycle, leases, and memory-store behavior. |
| `crates/mcp/tests/oauth_*_store.rs` | Feature-gated SQLite and container-backed PostgreSQL OAuth store parity, migrations, reopen, and concurrency. |
| `crates/waltr-agent/tests/agent.rs` and `crates/waltr-agent/tests/agent/` | Deterministic agent library tests using scripted/shared providers. |
| `crates/waltr-agent-sandbox/src/` tests | Worker file/patch/shell operations, tickets, workspace context, and snapshots. |
| `crates/waltr-agent/src/sandbox_gateway.rs` tests | Private RPC request correlation, cancellation, disconnect, and capacity. |
| `crates/waltr-server/src/sandbox.rs` tests | Private gateway startup, embedded duplex and standalone UDS worker readiness, reconnect, and shutdown. |
| `crates/waltr-agent/tests/live.rs` and `crates/waltr-agent/tests/live/` | Ignored live-provider and external-service agent tests. |
| `crates/waltr-server/tests/` | Server contract, coordination, watch, validation, health, persistence, access-auth, and system flows. |
| `crates/waltr-server/tests/embedded.rs` | Embedded composition, local admission, explicit callback metadata, public routes, and workspace isolation. |
| `crates/waltr-server/src/catalog/mod.rs` tests | Bounded network-first models.dev loading, disk-cache and explicit built-in precedence, stale semantics, and fallback validation. |
| `crates/waltr-server/src/resource_catalog/state/tests.rs` | Persisted self-MCP provisioning and ordinary resource administration. |
| `crates/waltr-server/tests/mcp_oauth_server.rs` | Hosted Waltr issuer, browser authentication bridge, consent, token lifecycle, direct-Authentik rejection, disabled-mode behavior, and restart. |
| `crates/waltr-server/tests/live.rs` and `crates/waltr-server/tests/live/session.rs` | Ignored server contract tests that run session commands through live providers. |
| `crates/waltr-client/tests/` | UI-like tests for the shared client state machine over the server contract. |
| `crates/waltr-client/tests/live.rs` and `crates/waltr-client/tests/live/session.rs` | Ignored client state-machine tests that run session commands through live providers. |
| `crates/waltr-web/tests/` | Browser UI state, routing, forms, transitions, and component-level behavior. |
| `crates/waltr-web/tests/telemetry.test.ts` | Faro enablement, stable identities, route normalization, standard instrumentation, repeated errors, and Tauri request exclusion. |
| `crates/waltr-web/tests/platform/` | Hosted WASM and Tauri adapter selection, lifecycle, stale-event handling, disposal, and external-navigation behavior. |
| `crates/waltr-app/src/server_controller.rs` tests | App-private paths, random local bearer generation, redaction, exact embedded-build error mapping, and process-owned server state. |
| `crates/waltr-app/src/bridge.rs` tests | Tauri generation order, permission gates, snapshot redaction, opaque authorization opening, and nonblocking replacement. |
| `crates/waltr-app/src/observability.rs` tests | Test-only disablement, all-build enablement, complete app-module target filters, fixed URLs and resources, startup failures, fixed panic exclusions, initialization diagnostics, hardened transport, shutdown bounds, and in-memory exporters. |
| `crates/waltr-app/src/policy.rs` tests | Packaged-only WebView navigation and credential-free external HTTPS URL validation. |
| `tests/web/k6/` | Browser-level server-backed end-to-end web flows. |
| `crates/waltr-tui/tests/app.rs` and `crates/waltr-tui/tests/snapshots/` | Non-server-backed TUI routing, input, rendering, and golden paths. |
| `crates/waltr-tui/tests/server_backed.rs` and `crates/waltr-tui/tests/server-backed/` | TUI flows against a real server, kept in parity with web k6 flows. |

Access-auth coverage is intentionally layered. Generic MCP tests own hosted OAuth parsing, DCR/CIMD, atomic token and key state, and authorization leases. Server tests own route partitioning, Authentik-to-Waltr browser resumption, Waltr consent, independent API/MCP issuers, direct-upstream rejection, persistence, and fail-closed startup. Agent/server tests own outbound discovery, registration selection, durable callback/refresh claims, and dynamic bearer replacement. Shared client tests own typed domain classification, native discovery/PKCE/callback/refresh behavior, and sanitized state. Browser, TUI, and Tauri tests own their host adapters and presentation without token access.

# Layering

Do not remove higher-layer coverage just because lower-layer coverage exists.

Generic protocol/library tests validate reusable crates such as `mcp`. Agent tests validate the agent library as a normal library boundary on top of generic dependencies. Server tests validate the server-facing contract. Client tests validate the shared state machine and native access protocol on top of that contract. Browser, TUI, and Tauri tests validate user-facing behavior and lifecycle through their platform-specific adapters.

# Commands

Run MCP spec tests:

`cargo test -p mcp --test spec`

Run all MCP integration tests:

`cargo test -p mcp --tests`

Run the generic hosted authorization server and memory-store contracts:

`cargo test -p mcp --features oauth-authorization-server --test oauth_authorization_server --test oauth_store`

Run the feature-gated SQLite OAuth store contract:

`cargo test -p mcp --features oauth-authorization-server-sqlite --test oauth_sqlite_store`

Run the ignored PostgreSQL OAuth store contract, which requires Docker:

`cargo test -p mcp --features oauth-authorization-server-postgres --test oauth_postgres_store -- --ignored`

Run server unit tests:

`cargo test -p waltr-server --lib`

Run server integration tests:

`cargo test -p waltr-server --tests`

Run the embedded server contract:

`cargo test -p waltr-server --test embedded`

This target requires exact local bearer admission on `/waltr.*` and `/mcp`. It leaves health and outbound callback routes outside that bearer.

Without a public metadata URI, the local client metadata route must return `404`.

Run focused server access-auth integration tests:

`cargo test -p waltr-server --test access_auth`

Run the hosted MCP OAuth server integration target:

`cargo test -p waltr-server --test mcp_oauth_server`

Run access-auth tests including embedded-web navigation and restart coverage gated by the server `web` feature:

`cargo test -p waltr-server --features web --test access_auth`

Server library tests include SQLite persistence and OIDC store behavior. The container-backed PostgreSQL targets are ignored by default, so `cargo test -p waltr-server --lib` does not require Docker.

Server library tests require network, valid-cache, and explicitly configured built-in precedence after fixed bounded models.dev request failure or timeout. Both disk and built-in fallbacks are stale. A malformed or unreadable configured cache must fail rather than reaching the built-in seed, and malformed seed content must fail validation.

Embedded resource coverage must prove that startup prebinds loopback, atomically overwrites `mcp-servers/waltr.yaml` with the generation URL before catalog load, reports filesystem origin, and refreshes the durable file for each generation. Inspection, replace, watcher, reset, and persisted-slot ETag assertions must use the ordinary resource contract. Reset must exercise ordinary candidate validation, including failure when the bundled fallback cannot resolve `WALTR_PUBLIC_URL`; no embedded immutability or reset-special-case assertion belongs in this coverage.

Embedded fallback coverage must parse the packaged production seed, reject known synthetic fixture IDs, and materialize the complete ordinary bundled catalog including its default agent. General server tests retain their separate synthetic models.dev fixture for selector edge cases.

Outbound OAuth unit coverage must verify explicit callback propagation and one-time standalone environment snapshots, including the private-test boolean.

It must verify that embedded mode skips CIMD when upstream also offers DCR, then selects DCR.

It must verify that embedded policy ignores the ambient private-test switch and honors only explicit trusted origins.

Persistence assertions must prove that attempts and credentials contain a per-generation marker, not the callback endpoint.

OAuth assertions must count one DCR across initial authorization and same-generation reauthorization, then a second DCR for a replacement generation. Restart assertions must reject stale markers and credentials; stale attempt status and cancel must return not found without mutating the old attempt. Self-MCP credential logout must return failed precondition and preserve the host-owned credential. A resource edit away from the initialized URL must prove that the generation bearer is not sent to the edited endpoint.

Environment tests must retain only supported names. They must exclude unrelated secrets and redact all retained values from `Debug`.

Embedded `Debug` tests must exclude endpoints, bearers, app-private paths, and task or thread handles.

Run the ignored server PostgreSQL migration and OIDC store targets, which require Docker:

`cargo test -p waltr-server --lib postgres_uses_independent_pools_and_component_schemas -- --ignored`

`cargo test -p waltr-server --lib postgres_store_core_behavior_and_v2_migration -- --ignored`

Run client unit tests:

`cargo test -p waltr-client --lib`

This includes shared Waltr access state transitions, protected-state clearing, status-watch epoch rejection, and native bearer credential lease coverage.

Run shared native access protocol tests:

`cargo test -p waltr-client --features native-access --lib native_access`

This layer owns callback-policy separation, exact redirect/state/code/error validation, wrong-state or otherwise invalid callback followed by a valid callback, timeout/cancellation, PKCE exchange, credential leases, protected bearer delivery, refresh/expiry, and logout cleanup. It does not prove Android intent delivery.

The same command checks that native OAuth builds its bundled Mozilla root store with ISRG Root X1. Run the native transport trust rejection directly with:

`cargo test -p waltr-client --features native-access --lib native_https_ignores_controlled_native_root`

This test serves a local certificate trusted only through `SSL_CERT_FILE` and requires both tonic and native OAuth reqwest to reject it. It uses a local listener and does not contact a Waltr deployment or external collector.

Run feature-gated native gRPC telemetry tests:

`cargo test -p waltr-client --features native-tracing --lib transport::native_trace`

This focused target checks bounded RPC classification, gRPC status handling, and W3C context injection without network export.

Run client integration tests:

`cargo test -p waltr-client --tests`

Run ignored live client session tests:

`cargo test -p waltr-client --test live -- --ignored`

Run TUI unit tests:

`cargo test -p waltr-tui --lib`

Run all TUI integration tests:

`cargo test -p waltr-tui --tests`

Run TUI environment, loopback-policy, and terminal-effect adapter tests:

`cargo test -p waltr-tui --lib native_auth`

Run focused terminal access-screen and native-action integration tests:

`cargo test -p waltr-tui --test app access`

Run only TUI server-backed tests:

`cargo test -p waltr-tui --test server_backed`

Run hosted browser and Tauri web-adapter tests:

`pnpm --filter @rfhold/waltr-web test`

The web suite covers hosted access, provider-OAuth separation, same-origin logout, and capability-driven access controls. It verifies that authoritative login-required and authenticated snapshots remain authoritative when an adapter supplies no access operations. Platform tests cover browser-provided access operations, hosted WASM disposal, and Tauri adapter-private startup event sequencing, sequence recovery, disposal, and opaque authorization or external URL delegation. Telemetry tests cover build modes, identities, standard instrumentation, repeated error events, route normalization, and Tauri request exclusion. Server tests own cookie and OIDC validation.

Build the hosted web client:

`pnpm --filter @rfhold/waltr-web build`

This command must complete TypeScript checks, the WASM build, and the hosted Vite bundle.

Run only deterministic Faro telemetry tests:

`pnpm --filter @rfhold/waltr-web exec vitest run tests/telemetry.test.ts --environment happy-dom`

Run Tauri host tests:

`cargo test -p waltr-app`

This layer checks embedded paths, source and manifest policy, bridge generations, and native observability privacy.

It must remove endpoint and bearer values from every serialized snapshot. An authorization URL that contains the private callback endpoint must also disappear.

The full authorization URL must remain Rust-only. The opener must require the active generation's current opaque attempt ID.

Android checks must cover off-callback joins, foreground promotion, and permission updates for active and fresh generations.

Fresh permission-required generations must ignore an existing server connection. Permission transitions must release the same generation.

Ticket tests must cover pause/start, resume/start, cancellation, failed starts, and overlapping starts.

Each stale completion must stop only its own generation. It must not stop or claim a newer generation.

Server lifecycle tests must leave nonblocking stop in `Stopping`, detach the exact current generation under lock, and prove replacement reservation waits for the old thread/reaper to finish. They must also prove a delayed old-generation exit cannot affect a later running generation.

Check the host Rust graph without packaging:

`cargo check -p waltr-app`

The host check must compile desktop embedded composition with server-side shell support.

Inspect the Android feature graph without compiling Android Rust code:

`cargo tree -p waltr-app --target aarch64-linux-android -e features`

The tree must omit `waltr-server/shell`, `waltr-agent/shell`, `waltr-agent-sandbox/shell`, and `waltr-server/profiling`. It must contain no Pyroscope dependency.

Check the standalone binary without profiling:

`cargo check -p waltr-server --no-default-features --features shell --bin waltr-server`

This check proves profiling is optional for the executable; default builds still enable it.

No-shell runtime-factory coverage must keep shell-bearing definitions loadable while omitting the shell tool and feature-derived shell permissions. Unified catalog coverage must materialize ordinary shell-bearing and stdio resources for embedded composition; stdio uses the normal session runtime rather than a host-specific rejection path. Server coverage must keep the worker off public routes and fail readiness if it stops; agent/session coverage must keep unknown dispatched workspace calls suspended rather than executing them twice. Standalone tests must check UDS startup failure, canonical-root agreement, loss and reconnection of the same worker, worker process replacement, and failure without host-side tool execution. Embedded tests must verify that ambient socket and workspace overrides cannot switch it out of duplex mode.

Coordinator tests must prove shutdown rejects new work, cancels active and queued work, waits for active run ownership to drain, atomically empties MCP preparation/cache state, and awaits each cached runtime's explicit shutdown. Existing deterministic stdio coverage does not constitute a real child-process shutdown/reap integration test; do not claim that test exists.

Run only native observability tests:

`cargo test -p waltr-app observability`

The observability tests use deterministic in-memory span, log, and metric exporters. They verify the complete app-module filters, native transport trace target, shared resource identity, fixed signal URLs, startup signals, fixed panic payload exclusion, initialization diagnostics, shutdown bounds, header sanitization, redirect non-following, ring provider ownership, complete bundled-root loading with ISRG Root X1, rejection of an untrusted local TLS endpoint, and test isolation. They do not contact either collector. They do not prove collector TLS, CORS, reachability, ingestion, mobile process-exit delivery, or Android runtime connectivity.

Use [Telemetry Smoke Checks](runbooks/telemetry-smoke.md) for live Faro, OTLP, and Pyroscope ingestion. Those checks require a built and active runtime. They complement deterministic tests and do not prove that uncommitted source changes reached a deployment.

Build the no-WASM Tauri web target:

`pnpm build:tauri`

Build the no-WASM Tauri development assets with WebView Faro disabled:

`pnpm --filter @rfhold/waltr-web build:tauri:dev`

Both commands run the Tauri output contract. It requires release assets to enable Faro, development assets to disable Faro, relative asset paths in both modes, and no WASM output. This asset-build gate does not disable native Rust telemetry in a non-test app build.

Build the Android package through the generated project:

`pnpm --filter @rfhold/waltr-app android:build`

This command requires the Android NDK Rust linker, including `aarch64-linux-android-clang`, for the selected targets. A missing linker blocks the Rust package build.

Build the configured desktop packages:

`pnpm --filter @rfhold/waltr-app build`

On Linux, this command requests the configured bundle formats. Each format requires its host packaging tools, including AppImage `linuxdeploy` support.

Run Android JVM tests from `crates/waltr-app/gen/android/`:

`./gradlew :app:testUniversalDebugUnitTest -x rustBuildUniversalDebug`

Compile the Android manifest and Kotlin host layer without the Rust cross-build:

`./gradlew :app:processUniversalDebugMainManifest :app:compileUniversalDebugKotlin -x rustBuildUniversalDebug`

These Gradle checks validate Android source and manifest integration. They do not validate native linking or installed-device behavior. Static Android checks cannot prove notification rendering or runtime Stop-intent delivery.

Verify that committed launcher resources are deterministically generated from the web favicon, include legacy and adaptive regular/round assets, and keep the duplicate Android resource trees synchronized:

`crates/waltr-app/scripts/android-launcher-icons.sh --check`

This check requires `rsvg-convert`, ImageMagick, and `perl`. It validates dimensions, adaptive foreground transparency, resource identity, and removal of the superseded green/lime palette. It does not validate launcher masking or icon caching on a device.

Validate the Pulumi source registration and deployment shape without applying a stack:

`bun run --cwd infra/pulumi build && bun run --cwd infra/pulumi test`

Package, feature-tree, and Gradle checks do not prove Android runtime behavior. Installed-device validation remains a separate required gate.

On an installed Android build, verify these outcomes:

1. Deny notification permission. Confirm that no server starts and the adapter-private startup sequence remains at the permission gate without changing route presentation.
2. Grant notification permission. Confirm the foreground notification and local client readiness.
3. Create durable session and Routine state. Confirm that activity pause stops only the client runtime.
4. Resume the activity. Confirm a fresh client generation reconnects to the same server state.
5. Remove the task. Confirm best-effort service continuity without treating survival as guaranteed.
6. Kill the process, then reopen the app. Confirm durable state reopens and unfinished Routines receive restart reconciliation.
7. Force-stop the app. Confirm that documentation and UI make no survival guarantee.
8. Begin provider and upstream-MCP OAuth, switch to the external browser, then return before completion. Confirm the fresh client generation restores the same pending attempt, dialog, manual draft when applicable, and opaque Rust-owned URL opener.
9. Complete the restored authorization attempt. Confirm callback attempts resume polling, manual attempts submit normally, and terminal or canceled attempts do not reopen.
10. Inspect the installed launcher icon after uninstalling the prior build to avoid launcher caching. Confirm the purple lowercase-k artwork under supported adaptive masks and confirm the foreground-service notification remains the separate monochrome server icon.
11. Load persisted shell-bearing resources. Confirm that they remain inspectable while no shell tool or feature-derived shell permission is installed.
12. Load and select a stdio MCP resource. Confirm that it materializes and follows ordinary runtime behavior.
13. Inspect app backup and transfer behavior. Confirm that private databases, files, preferences, and root data remain excluded.
14. Confirm that the foreground notification displays the Stop server action while the server runs.
15. Tap Stop after readiness. Confirm nonblocking server shutdown, notification removal, service stop, and no automatic sticky restart.
16. Tap Stop during server startup. Confirm the same shutdown, notification removal, service stop, and no automatic sticky restart.
17. Start Waltr normally, then remove its task without Stop. Confirm the documented best-effort service continuity remains unchanged.
18. After Stop completes, reopen or resume Waltr. Confirm that it starts a fresh server generation and does not retain a disabled setting.

Installed-device validation must cover a supported Android version before release. Current local Rust and Gradle checks do not satisfy this gate.

Run web server-backed k6 tests:

`pnpm test:web:integration`

This builds the repository server+web image through the static local Compose test infrastructure and runs every browser scenario in both access modes. The host runner supports Linux and macOS with Python 3 and Docker Compose; it uses platform-specific process identity and descriptor-bound cleanup rather than assuming Linux `/proc` is available on macOS. Windows is not supported. The `test` profile exercises access-disabled Waltr. The `test-auth` profile uses an HTTPS proxy with Waltr access OIDC enabled and an ephemeral hosted-MCP wrapping ring, then adds a real browser login, protected-route return, session-cookie checks, a real Sign out button click, and signed-out stream/state verification against Dex. Dex is the authentication-only fixture. The hosted MCP scenario exercises Waltr-to-Waltr CIMD registration, Waltr consent, durable callback completion, persisted credential reuse in a fresh session, dynamic bearer delivery, and protected resource discovery. Deterministic refresh, replay, revocation, and key-rotation behavior remains covered at the Rust protocol, store, agent, and server layers.

Run only the unauthenticated Compose phase:

`pnpm test:web:integration:no-auth`

Run only the access-authenticated Compose phase:

`pnpm test:web:integration:auth`

Run the focused generated-CA trust contract:

`pnpm test:web:trust`

This starts the strict authenticated Compose stack. It generates an ephemeral CA and leaf certificate inside Compose volumes, exercises real certificate verification through `SSL_CERT_FILE` for k6 HTTP and NSS import for Chromium, and covers Waltr's reqwest OIDC and CIMD discovery during health-gated startup. The static `waltr-auth` fixture alone sets `WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS=true` so its HTTPS-only application policy can reach deterministic private Compose aliases; production deployments leave this test-only exception unset. The stack does not enable TLS bypass flags and commits no test private key.

Run the older local server plus Vite development path for debugging and fixture-only prompt checks:

`pnpm test:web:integration:dev`

Run web server-backed k6 tests and validate screenshot snapshots:

`pnpm test:web:screenshots`

The screenshot workflow runs the unauthenticated phase so each semantic visual state is captured at desktop (1440x900), tablet (768x1024), phone (390x844), and compact phone (320x700) sizes. Filenames end in `--desktop.png`, `--tablet.png`, `--phone.png`, or `--compact.png`. Actual screenshot PNGs are written under `.dev/web-screenshots/current/`, which is ignored by git, and compared against local snapshot baselines under ignored `tests/web/k6/snapshots/screenshots/`.

Update the local screenshot snapshots after an intentional visual change:

`pnpm test:web:screenshots:update`

Run agent unit tests:

`cargo test -p waltr-agent --features shell --lib`

Run worker unit tests, including shell execution:

`cargo test -p waltr-agent-sandbox --features shell --lib`

Run the worker without shell (the Android-style capability split, on the host target):

`cargo test -p waltr-agent-sandbox --no-default-features --features rpc --lib`

Run the server's focused sandbox transport tests (including UDS behavior on Unix hosts):

`cargo test -p waltr-server --lib sandbox`

For end-to-end standalone image/sidecar coverage, use Docker Compose on Linux or macOS with Python 3 on the host. Docker Desktop runs Linux worker and server containers on macOS, so the Compose smoke and browser phases exercise the Linux UDS and container mount contract there. A Kubernetes PVC still requires a deployed-pod write check; a macOS Docker run does not prove production CSI ownership. Run from the repository root after the worker image and static Compose services are present:

```sh
docker build --target worker-runtime -t waltr-worker:local .
node --test scripts/compose-config.test.mjs
node scripts/test-worker-compose.mjs
pnpm test:web:integration
pnpm test:web:trust
```

The static contract checks profile and mount topology. The worker Compose smoke starts an isolated project, checks worker-only workspace write access and a live socket, stops and restarts the worker, and verifies `/ready` fails and recovers while `/health` stays live. Compose browser integration runs both access modes through the server/worker fixture. The Rust socket tests exercise actual worker tool execution and in-flight reply delivery without re-executing it. Worker-process death leaves uncertain dispatched work unresolved rather than replaying it. `/health` is liveness, not proof of the worker connection. A Kubernetes PVC requires a separate deployed-pod write/read smoke check (see [Workspace Worker Deployment](deployment.md#workspace-worker-deployment)).

Run deterministic agent integration tests:

`cargo test -p waltr-agent --features shell --test agent`

Run ignored live agent tests:

`cargo test -p waltr-agent --features shell --test live -- --ignored`

Run ignored live server session tests:

`cargo test -p waltr-server --test live -- --ignored`

Run ignored live client session tests:

`cargo test -p waltr-client --test live -- --ignored`

## MCP Apps

Apps tests separate generic protocol, session authority, shared client state, browser isolation, and native IPC prerequisites.
Install the [asset build prerequisites](deployment.md#mcp-apps-assets-and-app-container) before server tests.
Default server builds consume web-owned Vite container output and build each explicit served app independently.
`WALTR_APP_CONTAINER_DIST` bypasses the host-assets Vite build, not a full `web` build or enabled served-app builds.
See [Waltr-served Apps authoring](mcp/apps-authoring.md) for the static addition workflow and required per-app coverage.
Run these focused commands from the repository root:

```sh
cargo test -p mcp --test spec apps
cargo test -p waltr-agent --features shell --lib mcp_apps
cargo test -p waltr-agent --features shell --test agent mcp
cargo test -p waltr-client --test apps
cargo test -p waltr-client --lib apps_cancellation
cargo test -p waltr-server --lib apps
cargo test -p waltr-server --lib mcp_apps_host
cargo test -p waltr-server --no-default-features --features web --lib apps
cargo test -p waltr-server --no-default-features --features web --lib bootstrap
cargo test -p waltr-tui --features bundled --bin waltr
pnpm --filter @rfhold/waltr-web exec vitest run tests/mcp-apps --environment happy-dom
pnpm test:app-container:unit
pnpm test:status-app
cargo test -p waltr-app app_container
node --test crates/waltr-app/scripts/confined-ipc.test.mjs
```

| Layer | Required Contract |
| --- | --- |
| Generic MCP | Default-off capability, typed declarations/read errors, exact metadata/visibility, raw fields, bounded resource decode, and independent lifecycle payloads. |
| Asset builds | Actual self-contained status output, independent second-fixture Vite output/Rust embedding, and separate mocked Cargo feature/override orchestration. |
| Agent | App-only exclusion, exact provenance, refresh-stable credential authority, account-replacement rejection, middleware mutation denial, and raw snapshots outside model context. |
| Server session | Owner-bound handles, complete Effective pairs, stale/busy rejection, no inference, expiry/cancellation, audit results, and Close without session load. |
| Shared client | Keyed state, original snapshot preservation, matching-call close preemption, unrelated-effect retention, stale completions, access invalidation, and late canceled-open cleanup. |
| Web host | SDK handshake, original input/result exactly once, response-only refresh, message validation, unsupported methods, persisted pagehide retention, and final-unload cleanup. |
| Asset service | Exact origins, HTTP CSP, no API/auth routes, byte-identical HTML, bounded uploads/storage, expiry, and deletion. |
| Bundled host | Automatic distinct-origin listener, exact loopback parents, runtime URL precedence, no-store shells, startup rollback, child-failure cleanup, and joined shutdown. |
| Native security | Explicit main-WebView permissions, confined invoke key, missing/guessed-key denial, and exact current-generation macOS frame policy. |

Run the real-browser isolation contract:

```sh
pnpm test:app-container:browser
```

The web-owned Playwright configuration owns two temporary loopback servers: a fixture host and the actual Cargo asset service.
The service command uses `--no-default-features --features apps-host-assets`, with no served status asset or full web/WASM build.
Its host-assets build consumes the web container-only Vite output.
The fixture host bundles its test-only SDK demo with esbuild and uses a fake tool callback.
It does not exercise the session RPC path.
The suite checks fixture HTML, unrelated tool names, network and navigation denial, parent DOM denial, protocol order, headers, and cleanup.
`pnpm test:app-container:unit` runs web-owned container tests through the focused Vitest configuration and checks the Vite output contract.
The output contract checks fixed self-contained assets, browser entry isolation, and container omission from Tauri builds.
Its full browser output check requires current generated WASM artifacts; otherwise that check skips.
The container implementation belongs in web `src/mcp-apps/container/`; container tests and browser fixtures belong in web `tests/mcp-apps/`.
The web suite also covers `AppHost` and the modal.
The default engine is Chromium; supply an installed Playwright browser or an explicit executable.
For macOS Google Chrome, use:

```sh
WALTR_APPS_BROWSER_EXECUTABLE='/Applications/Google Chrome.app/Contents/MacOS/Google Chrome' \
pnpm test:app-container:browser
```

`WALTR_APPS_BROWSER` selects the Playwright engine, such as `chromium` or `webkit`; that engine must exist locally.
`WALTR_APPS_TEST_BINARY` optionally selects a prebuilt asset-service binary and skips the Cargo service command.
The browser contract independently bundles its test-only SDK fixture; it does not require any shipped app HTML.
The production status app sources belong to `crates/waltr-server`; `build.rs` invokes its workspace package independently of the browser fixture.
Server MCP tests verify conditional status linkage, current-request HTML capability checks, self-contained SDK resource content, deny-all external CSP, and readiness refresh with no session or inference use.
Run `cargo test -p waltr-server --lib mcp_service::tests` and repeat with `--no-default-features` to check both linked-app and text-only builds.
Run `pnpm test:status-app` to build the server package, check its actual output, and test Cargo asset orchestration.
The package also runs `scripts/apps.contract.mjs` with the test-only `tests/fixtures/apps/second/` source.
It builds independent real Vite documents and compiles generated Rust constants against those documents.
The temporary pipeline stubs pnpm only after Vite creates the real output.
The fixture has no production build entry or `RESOURCES` registration.
The contract rejects shared output files, external script references, and oversized HTML.
The status output contract checks the SDK handshake, original observation, refresh, malformed-result preservation, and teardown.
It reads the actual Vite-built `dist/status/index.html` and requires one self-contained document within the one-MiB limit.
This contract uses a local message/DOM stub; it does not prove browser isolation or full-session authorization.
The separate `scripts/build-assets.contract.mjs` contract compiles `build.rs` and mocks pnpm commands and asset output.
It checks feature selection, override behavior, full-web output reuse, and exclusion of Apps assets from the API asset collection.
Run `node --test crates/waltr-server/scripts/build-assets.contract.mjs` for those orchestration checks alone.
Mocked orchestration does not prove a real package or asset build.

A Chrome result establishes only that engine's browser isolation behavior.
It does not prove a full session-RPC browser flow, WebKit behavior, or native Tauri WebView isolation.
MockRuntime and Node IPC tests also do not prove runtime native frame isolation.
Normal Apple Silicon macOS builds enable Apps without a prior isolation report or diagnostic feature.
These proof limits do not impose an enablement gate; runtime security enforcement remains mandatory.
Other native platforms remain unsupported; browser results establish no additional native support.

Sources: [Playwright configuration](../crates/waltr-web/playwright.config.mjs),
[browser contract](../crates/waltr-web/tests/mcp-apps/browser/isolation.spec.mjs),
[container unit configuration](../crates/waltr-web/tests/mcp-apps/container.config.ts),
[container output contract](../crates/waltr-web/scripts/container-output.contract.mjs),
[status output contract](../crates/waltr-server/scripts/status-app.contract.mjs),
[independent served-app contract](../crates/waltr-server/scripts/apps.contract.mjs),
[Cargo asset orchestration contract](../crates/waltr-server/scripts/build-assets.contract.mjs),
[session RPC contract](../crates/waltr-server/src/session/apps_tests.rs),
[shared client contracts](../crates/waltr-client/tests/apps.rs),
[shared-lane cancellation](../crates/waltr-client/src/coordination/runtime/tests/apps_cancellation.rs),
[bundled server contracts](../crates/waltr-server/src/app.rs), [HTML bootstrap contracts](../crates/waltr-server/src/web_assets.rs),
[TUI lifecycle contracts](../crates/waltr-tui/src/main.rs),
and [native prerequisites](../crates/waltr-app/src/app_container.rs).

### macOS Native Apps Verification

Normal Apple Silicon macOS builds enable Apps without `native-apps-test`, a disposable profile, or prior diagnostic proof.
Actual macOS WKWebView observations remain distinct from Playwright WebKit and MockRuntime results.
The normal local build targets unsigned `aarch64-apple-darwin`.
This procedure covers no Intel/universal package, code signature, notarization, Android, Linux WebView, or Windows WebView.
Future Git sources, navigation entries, and resource-first Apps launch remain outside this procedure.

Install workspace dependencies and the [web/WASM prerequisites](../CONTRIBUTING.md#building) before the build.
Use a compatible `cargo-tauri` CLI; the app pins Tauri `2.10.1`.
See [the normal build and launch commands](../CONTRIBUTING.md#building) for the separate `target/macos-default` output.
From the repository root, open the normal bundle:

```sh
open target/macos-default/aarch64-apple-darwin/debug/bundle/macos/Waltr.app
```

These commands describe a procedure, not a successful build or live test.
For the current local acceptance, the user tests the normal app before any separately authorized commit, push, or deployment.
This acceptance order does not disable Apps or require the optional diagnostic harness.
This procedure grants none of those permissions.

1. Record a fresh ordinary self-MCP `status` call/result pair in a session.
2. Select **Open app** on that complete pair.
3. Check the initial observation, Refresh, Close, and reopen through the ordinary session RPC path.
4. Quit the app normally to check listener shutdown.

Older text-only calls do not acquire an Open app action retroactively.
The normal startup returns the container URL only after the WKNavigationDelegate installation acknowledges success.
That acknowledgement enforces runtime security, not a release-validation gate.

#### Optional Diagnostic Harness

The `native-apps-test` feature adds a disposable attack fixture and the `nativeAppsTest` helper.
It requires `tauri.native-apps-test.conf.json` and identifier `dev.waltr.app.native-apps-test` only for this optional diagnostic build.
The separate configuration builds and stages packaged frontend assets plus the native attack page.
It does not remove the current shared WASM prerequisite from the Tauri asset build.

From the repository root, build the optional diagnostic candidate:

```sh
pnpm --filter @rfhold/waltr-app build:native-apps-test
```

The package script selects `--debug --no-bundle --target aarch64-apple-darwin --features native-apps-test --config tauri.native-apps-test.conf.json`.
Build completion proves no runtime isolation.
If the build fails, stop here; do not report the candidate as verified.

For a diagnostic report, inspect both the attack page and the real App in actual WKWebView.
The report does not replace normal-app live acceptance or authorize a commit, push, or deployment.

From the repository root, launch with the diagnostic sanitizer directly attached:

```sh
target/aarch64-apple-darwin/debug/waltr-app 2>&1 | node crates/waltr-app/scripts/native-apps-evidence.mjs
```

If `CARGO_TARGET_DIR` changes the output location, use the corresponding candidate binary path.
Never retain, print, copy, or `tee` raw stdout/stderr before the sanitizer.
Tauri's invalid-key diagnostic includes the expected real invoke key; raw output is secret-bearing evidence.
The sanitizer drops arbitrary output and emits only fixed probe IDs, bounded native gate markers, and a final report.
Capture only sanitizer output when a retained report is necessary.

1. Confirm that the trusted positive control succeeds on the initial WKWebView attack page.
2. Inspect both the separate-origin outer fixture and opaque inner fixture for ancestor access or invoke-key leaks.
3. Reject any `SUCCEEDED`, `LEAKED`, or `EXPOSED` outcome; absent APIs and timeouts do not prove native denial.
4. Wait for the subframe navigation probe to execute.
5. Select **Attempt Denied Main Navigation And Popup**.
6. Confirm that the packaged page remains current and no popup opens.
7. Select **Continue To Disposable Waltr UI**.
8. Obtain a complete ordinary self-MCP `status` tool call/result pair in a session.
9. Select **Open app** on that recorded pair.
10. Check the real status App's initial observation, Refresh, Close, and reopen through the ordinary session RPC path.
11. Quit the candidate normally so the sanitizer can emit its final report.

The native key-fence report requires 28 explicit rejections: seven commands through raw IPC and `ipc:` from each attacker frame.
The commands cover runtime start, dispatch, state, disposal, external URL, authorization-attempt opening, and channel fetch.
The fixture also attempts `http://ipc.localhost`; macOS does not provide that protocol, so absence supplies no extra native proof.
The report also requires the positive control and installed/main/subframe/popup native navigation markers.
The sanitizer's `NATIVE KEY-FENCE CHECK PASS` covers only those checks, not the complete manual isolation procedure.
Independently confirm ancestor/key isolation, native navigation denial, real status behavior, and listener shutdown.
An incomplete report establishes no successful isolation check; a forbidden capability requires investigation.
The report does not control normal Apps enablement or establish other native platform support.

Focused source contracts, without a GUI launch:

```sh
node --test crates/waltr-app/scripts/native-apps-evidence.test.mjs
cargo test -p waltr-app native_apps
cargo test -p waltr-app --features native-apps-test native_apps
```

These tests check sanitizer behavior, normal container lifecycle, optional probe ownership, diagnostic profile policy, and native navigation decisions.
The `app_container` source contract checks the same CSP function across six OS values, five architectures, and both diagnostic states.
It checks normal and development CSP, exact frame sources, and unchanged other directives; unsupported targets retain `frame-src 'none'`.
They do not replace the user boot or actual WKWebView observations.

Sources: [candidate package script](../crates/waltr-app/package.json),
[disposable configuration](../crates/waltr-app/tauri.native-apps-test.conf.json),
[fixture staging](../crates/waltr-app/scripts/stage-native-apps-test.mjs),
[Rust native container and optional probe](../crates/waltr-app/src/native_apps.rs),
[WK navigation adapter](../crates/waltr-app/src/native_apps_navigation.rs),
[attack page](../crates/waltr-app/tests/native-apps/native-apps-test.html),
[frame probes](../crates/waltr-app/tests/native-apps/probe.js), and
[diagnostic sanitizer](../crates/waltr-app/scripts/native-apps-evidence.mjs).

# Live Agent Tests

Live agent tests are ignored by default. Running them explicitly without provider credentials should complete successfully and print skip messages for missing API keys.

Use the shared gateway:

`WALTR_AGENT_LIVE_GATEWAY=1 cargo test -p waltr-agent --features shell --test live -- --ignored`

Use direct provider credentials:

`OPENAI_API_KEY=<key> cargo test -p waltr-agent --features shell --test live -- --ignored`

`ANTHROPIC_API_KEY=<key> cargo test -p waltr-agent --features shell --test live -- --ignored`

Select the live provider for selected-provider tests with `WALTR_AGENT_LIVE_PROVIDER`.

Supported values:

- `openai_responses_websocket`
- `openai_responses`
- `openai_chat_completions`
- `anthropic_messages`

# Live Server Tests

Live server tests are ignored by default and live under `crates/waltr-server/tests/live.rs` and `crates/waltr-server/tests/live/session.rs`. They validate the server gRPC contract, runtime default resolution, background session runs, and timeline reads/watches against real provider traffic.

Server live session tests follow the agent live-test pattern: each provider path has its own ignored test and skips when that provider's configuration is unavailable. Current coverage includes direct OpenAI chat completions, direct Anthropic Messages, gateway OpenAI-format, and gateway Anthropic-format runs. The server runtime maps OpenAI providers to the chat completions API for now.

Use the shared gateway:

`WALTR_AGENT_LIVE_GATEWAY=1 cargo test -p waltr-server --test live -- --ignored`

Use direct provider credentials:

`OPENAI_API_KEY=<key> cargo test -p waltr-server --test live -- --ignored`

`ANTHROPIC_API_KEY=<key> cargo test -p waltr-server --test live -- --ignored`

# Live Client Tests

Live client tests are ignored by default and live under `crates/waltr-client/tests/live.rs` and `crates/waltr-client/tests/live/session.rs`. They validate that the shared client reducer/runtime can create sessions, submit user messages, maintain timeline watches, and observe live provider output through client snapshots.

Client live tests use the same provider paths as server live tests: direct OpenAI chat completions, direct Anthropic Messages, gateway OpenAI-format, and gateway Anthropic-format. Each provider path has its own ignored test and skips when that provider's configuration is unavailable.

Use the shared gateway:

`WALTR_AGENT_LIVE_GATEWAY=1 cargo test -p waltr-client --test live -- --ignored`

Use direct provider credentials:

`OPENAI_API_KEY=<key> cargo test -p waltr-client --test live -- --ignored`

`ANTHROPIC_API_KEY=<key> cargo test -p waltr-client --test live -- --ignored`

# Containers

MCP and server PostgreSQL store tests require Docker because they start `postgres:16-alpine` through testcontainers. These targets are ignored and must be selected with `-- --ignored`. Some ignored live session storage tests also start container-backed services. Prefer in-memory, filesystem, SQLite, or scripted fixtures for behavior that does not require an external service.

The commands above define reproducible gates and expected outcomes. They do not record one local run or establish production readiness.

Web server-backed integration tests require Docker Compose. Every test Waltr service uses tmpfs `/data`; the standalone worker uses an ephemeral `/workspace` and a private shared socket directory rather than the server's `/data` or secrets. Test services communicate on an internal-only network and publish no host port. Long-running dependencies have health checks and startup uses `docker compose up --wait`. The runner preserves the first failure exit code, captures diagnostics, and treats failed teardown as a test failure while retrying cleanup. Caddy terminates HTTPS for the browser and OIDC issuer, and its ephemeral CA is trusted only inside the test stack.

Run the static Compose deployment contract without starting services:

`node --test scripts/compose-config.test.mjs`
