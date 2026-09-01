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
| `crates/waltr-agent/tests/live.rs` and `crates/waltr-agent/tests/live/` | Ignored live-provider and external-service agent tests. |
| `crates/waltr-server/tests/` | Server contract, coordination, watch, validation, health, persistence, access-auth, and system flows. |
| `crates/waltr-server/tests/mcp_oauth_server.rs` | Hosted Waltr issuer, browser authentication bridge, consent, token lifecycle, direct-Authentik rejection, disabled-mode behavior, and restart. |
| `crates/waltr-server/tests/live.rs` and `crates/waltr-server/tests/live/session.rs` | Ignored server contract tests that run session commands through live providers. |
| `crates/waltr-client/tests/` | UI-like tests for the shared client state machine over the server contract. |
| `crates/waltr-client/tests/live.rs` and `crates/waltr-client/tests/live/session.rs` | Ignored client state-machine tests that run session commands through live providers. |
| `crates/waltr-web/tests/` | Browser UI state, routing, forms, transitions, and component-level behavior. |
| `crates/waltr-web/tests/telemetry.test.ts` | Faro enablement, stable identities, route normalization, privacy filtering, and platform-specific propagation. |
| `crates/waltr-web/tests/platform/` | Hosted WASM and Tauri adapter selection, lifecycle, stale-event handling, disposal, and external-navigation behavior. |
| `crates/waltr-app/src/bridge.rs` tests | Tauri generation ordering, bounded coalescing, FIFO dispatch, disposal, logout ordering, Android callback routing/handoff lifecycle, and secret-free serialization. |
| `crates/waltr-app/src/observability.rs` tests | Test-only initialization disablement, all-build enablement, target allowlists, fixed URLs, shared resources, startup signals, shutdown bounds, header privacy, redirect non-following, bundled-root loading, untrusted TLS rejection, and in-memory exporters. |
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

Run focused server access-auth integration tests:

`cargo test -p waltr-server --test access_auth`

Run the hosted MCP OAuth server integration target:

`cargo test -p waltr-server --test mcp_oauth_server`

Run access-auth tests including embedded-web navigation and restart coverage gated by the server `web` feature:

`cargo test -p waltr-server --features web --test access_auth`

Server library tests include SQLite persistence and OIDC store behavior. The container-backed PostgreSQL targets are ignored by default, so `cargo test -p waltr-server --lib` does not require Docker.

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

The web suite includes `tests/access/` coverage for generated access bindings, safe browser return paths, login states, expiry, provider-OAuth separation, and same-origin logout. `tests/platform/` covers hosted WASM disposal and Tauri generation/sequence handling, recovery, disposal, and external URL delegation. `tests/telemetry.test.ts` verifies the build-mode matrix, identities, propagation ownership, and hostile-payload filtering for routes, errors, traces, and Web Vitals measurements. It uses mocked browser/client boundaries; server cookie and OIDC validation belong to server access-auth tests.

Run only deterministic Faro telemetry tests:

`pnpm --filter @rfhold/waltr-web exec vitest run tests/telemetry.test.ts --environment happy-dom`

Run Tauri shell unit and policy tests:

`cargo test -p waltr-app`

This layer checks the explicit manifest callback shape, opaque Kotlin-to-JNI `Intent` transport, and absence of a callback Tauri event or WebView capability. Tests cover one-entry latest replacement, invalid-then-valid routing, generation fences, handoff-before-open order, one-shot pause preservation, callback release on resume, suspended terminal teardown, process-memory cleanup, secret-free IPC, CSP scope, and native observability privacy. Cold and warm callback tests exercise Rust routing and source contracts, not Android's installed-app intent resolver.

Run only native observability and exact Faro CSP tests:

`cargo test -p waltr-app observability`

`cargo test -p waltr-app csp_adds_only_the_exact_faro_collector_origin`

The observability tests use deterministic in-memory span, log, and metric exporters. They verify target allowlists, shared resource identity, fixed signal URLs, startup signals, shutdown bounds, header sanitization, redirect non-following, ring provider ownership, complete bundled-root loading with ISRG Root X1, rejection of an untrusted local TLS endpoint, and test isolation. They do not contact either collector. They do not prove collector TLS, CORS, reachability, ingestion, mobile process-exit delivery, or Android runtime connectivity.

Build the no-WASM Tauri web target:

`pnpm build:tauri`

Build the no-WASM Tauri development assets with WebView Faro disabled:

`pnpm --filter @rfhold/waltr-web build:tauri:dev`

Both commands run the Tauri output contract. It requires release assets to enable Faro, development assets to disable Faro, relative asset paths in both modes, and no WASM output. This asset-build gate does not disable native Rust telemetry in a non-test app build.

Build the Android package through the generated project:

`pnpm --filter @rfhold/waltr-app android:build`

Validate the Pulumi source registration and deployment shape without applying a stack:

`bun run --cwd infra/pulumi build && bun run --cwd infra/pulumi test`

The client and app tests prove protocol behavior, Rust ownership, bounded routing, source/manifest configuration, and deterministic native TLS trust. The Pulumi test proves both stack files declare the exact private URI and the program retains strict Android plus regex desktop registration. The Android package command proves the generated project builds. None proves Android runtime connectivity; RC8 device verification must exercise the live public-root Waltr, OIDC, and OTLP endpoints before this fix is considered runtime-verified. These checks also do not prove that Authentik has been reconciled, that an emulator/device routes cold or warm intents to Waltr, or that a live provider accepts the redirect.

Installed-app testing belongs at the Android host layer. Exercise warm and cold intents, browser pause/resume, cancel and five-minute timeout while suspended, ordinary and authenticated backgrounding, callback after process death, and private-scheme contention. A cold callback after process death must fail closed and a new login must be required. Authorized live-provider smoke testing additionally covers login, a protected request, refresh or expiry, logout, and subsequent credential rejection. These checks require explicit deployment authority and must not be folded into routine tests that mutate preview. Android app start and Linux desktop commands are documented once in [Deployment](deployment.md#tauri-application). macOS and iOS require macOS and were not verified.

Run web server-backed k6 tests:

`pnpm test:web:integration`

This builds the repository server+web image through the static local Compose test infrastructure and runs every browser scenario in both access modes. The `test` profile exercises access-disabled Waltr. The `test-auth` profile uses an HTTPS proxy with Waltr access OIDC enabled and an ephemeral hosted-MCP wrapping ring, then adds a real browser login, protected-route return, session-cookie checks, a real Sign out button click, and signed-out stream/state verification against Dex. Dex is the authentication-only fixture. The hosted MCP scenario exercises Waltr-to-Waltr CIMD registration, Waltr consent, durable callback completion, persisted credential reuse in a fresh session, dynamic bearer delivery, and protected resource discovery. Deterministic refresh, replay, revocation, and key-rotation behavior remains covered at the Rust protocol, store, agent, and server layers.

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

The screenshot workflow runs the unauthenticated phase so each visual baseline is produced once. Actual screenshot PNGs are written under `.dev/web-screenshots/current/`, which is ignored by git, and compared against local snapshot baselines under ignored `tests/web/k6/snapshots/screenshots/`.

Update the local screenshot snapshots after an intentional visual change:

`pnpm test:web:screenshots:update`

Run agent unit tests:

`cargo test -p waltr-agent --features shell --lib`

Run deterministic agent integration tests:

`cargo test -p waltr-agent --features shell --test agent`

Run ignored live agent tests:

`cargo test -p waltr-agent --features shell --test live -- --ignored`

Run ignored live server session tests:

`cargo test -p waltr-server --test live -- --ignored`

Run ignored live client session tests:

`cargo test -p waltr-client --test live -- --ignored`

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

The commands above are the implemented focused gates. Deterministic and SQLite suites, container-backed PostgreSQL conformance, aggregate workspace checks, complete hosted browser OAuth verification, Pulumi shape/policy tests, and static pipeline-definition checks have passed. Live Pulumi/Tekton deployment, multi-replica revocation/rotation, database failover, backup restoration, Authentik-outage, independent third-party client hosted-OAuth interoperability, and performance/load exercises were not run and remain workstream 07/08 gaps. Do not infer production deployment or successful restoration from deterministic, container-backed, browser, Pulumi-mock, or pipeline-definition checks.

Web server-backed integration tests require Docker Compose. Every test Waltr service uses tmpfs `/data`; test services communicate on an internal-only network and publish no host port. Long-running dependencies have health checks and startup uses `docker compose up --wait`. The runner preserves the first failure exit code, captures diagnostics, and treats failed teardown as a test failure while retrying cleanup. Caddy terminates HTTPS for the browser and OIDC issuer, and its ephemeral CA is trusted only inside the test stack.

Run the static Compose deployment contract without starting services:

`node --test scripts/compose-config.test.mjs`
