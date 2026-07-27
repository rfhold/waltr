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
| `tests/web/k6/` | Browser-level server-backed end-to-end web flows. |
| `crates/waltr-tui/tests/app.rs` and `crates/waltr-tui/tests/snapshots/` | Non-server-backed TUI routing, input, rendering, and golden paths. |
| `crates/waltr-tui/tests/server_backed.rs` and `crates/waltr-tui/tests/server-backed/` | TUI flows against a real server, kept in parity with web k6 flows. |

Access-auth coverage is intentionally layered. Generic MCP tests own hosted OAuth parsing, DCR/CIMD, atomic token and key state, and authorization leases. Server tests own route partitioning, Authentik-to-Waltr browser resumption, Waltr consent, independent API/MCP issuers, direct-upstream rejection, persistence, and fail-closed startup. Agent/server tests own outbound discovery, registration selection, durable callback/refresh claims, and dynamic bearer replacement. Shared client tests own typed domain classification and sanitized attempt state. Web/TUI tests own callback polling/manual completion presentation without token access.

# Layering

Do not remove higher-layer coverage just because lower-layer coverage exists.

Generic protocol/library tests validate reusable crates such as `mcp`. Agent tests validate the agent library as a normal library boundary on top of generic dependencies. Server tests validate the server-facing contract. Client tests validate the shared state machine on top of that contract. Web and TUI tests validate user-facing behavior through their platform-specific UI layers.

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

The default MCP feature set does not compile either SQL store test. Run the feature-gated SQLite store contract:

`cargo test -p mcp --features streamable-http-sqlite --test streamable_http_sqlite_store`

Run the feature-gated PostgreSQL store contract, which starts `postgres:16-alpine` through testcontainers and therefore requires Docker:

`cargo test -p mcp --features streamable-http-postgres --test streamable_http_postgres_store -- --ignored`

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

Run client integration tests:

`cargo test -p waltr-client --tests`

Run ignored live client session tests:

`cargo test -p waltr-client --test live -- --ignored`

Run TUI unit tests:

`cargo test -p waltr-tui --lib`

Run all TUI integration tests:

`cargo test -p waltr-tui --tests`

Run native OIDC discovery, PKCE, loopback callback, token refresh/expiry, and redaction unit tests:

`cargo test -p waltr-tui --lib native_auth`

Run focused terminal access-screen and native-action integration tests:

`cargo test -p waltr-tui --test app access`

Run only TUI server-backed tests:

`cargo test -p waltr-tui --test server_backed`

Run web tests:

`pnpm --filter @rfhold/waltr-web test`

The web suite includes `tests/access/` coverage for generated access bindings, safe browser return paths, login states, expiry, provider-OAuth separation, and same-origin logout. It uses mocked browser/client boundaries; server cookie and OIDC validation belong to server access-auth tests.

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

Run the Pulumi deployment shape and secret-projection policy tests:

`bun --cwd infra/pulumi run build && bun --cwd infra/pulumi run test`

Run the static Compose deployment contract without starting services:

`node --test scripts/compose-config.test.mjs`
