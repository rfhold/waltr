# Testing

Waltr uses layered feature coverage. A feature should be tested at each layer it crosses, with assertions focused on that layer's responsibility.

# Structure

| Path | Intent |
| --- | --- |
| `crates/waltr-agent/tests/agent.rs` and `crates/waltr-agent/tests/agent/` | Deterministic agent library tests using scripted/shared providers. |
| `crates/waltr-agent/tests/live.rs` and `crates/waltr-agent/tests/live/` | Ignored live-provider and external-service agent tests. |
| `crates/waltr-server/tests/` | Server contract, coordination, watch, validation, health, and system flows. |
| `crates/waltr-client/tests/` | UI-like tests for the shared client state machine over the server contract. |
| `crates/waltr-web/tests/` | Browser UI state, routing, forms, transitions, and component-level behavior. |
| `tests/web/k6/` | Browser-level server-backed end-to-end web flows. |
| `crates/waltr-tui/tests/app.rs` and `crates/waltr-tui/tests/snapshots/` | Non-server-backed TUI routing, input, rendering, and golden paths. |
| `crates/waltr-tui/tests/server_backed.rs` and `crates/waltr-tui/tests/server-backed/` | TUI flows against a real server, kept in parity with web k6 flows. |

# Layering

Do not remove higher-layer coverage just because lower-layer coverage exists.

Server tests validate the server-facing contract. Client tests validate the shared state machine on top of that contract. Web and TUI tests validate user-facing behavior through their platform-specific UI layers. Agent tests validate the agent library as a normal library boundary.

# Commands

Run server unit tests:

`cargo test -p waltr-server --lib`

Run server integration tests:

`cargo test -p waltr-server --tests`

Run client unit tests:

`cargo test -p waltr-client --lib`

Run client integration tests:

`cargo test -p waltr-client --tests`

Run TUI unit tests:

`cargo test -p waltr-tui --lib`

Run all TUI integration tests:

`cargo test -p waltr-tui --tests`

Run only TUI server-backed tests:

`cargo test -p waltr-tui --test server_backed`

Run web tests:

`pnpm --filter @rfhold/waltr-web test`

Run web server-backed k6 tests:

`pnpm test:web:integration`

Run web server-backed k6 tests and validate screenshot snapshots:

`pnpm test:web:screenshots`

Actual screenshot PNGs are written under `.dev/web-screenshots/current/`, which is ignored by git, and compared against static snapshots under `tests/web/k6/snapshots/screenshots/`.

Update the static screenshot snapshots after an intentional visual change:

`pnpm test:web:screenshots:update`

Run agent unit tests:

`cargo test -p waltr-agent --features shell --lib`

Run deterministic agent integration tests:

`cargo test -p waltr-agent --features shell --test agent`

Run ignored live agent tests:

`cargo test -p waltr-agent --features shell --test live -- --ignored`

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

# Containers

Some ignored live session storage tests require Docker because they start container-backed storage services. Prefer in-memory, filesystem, or scripted fixtures for behavior that does not require external services.
