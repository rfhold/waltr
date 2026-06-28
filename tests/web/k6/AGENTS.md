# Audience

Web k6 tests should validate browser-level server-backed flows for deployed web behavior.

# Boundaries

K6 tests should cover end-to-end browser behavior against a running server, not component-level web UI state.

# Index

| Path | Info |
| --- | --- |
| [support/](support/) | Shared browser checks, text helpers, and k6 browser setup. |
| [config/](config/) | Server-backed config provider, MCP, defaults, and watch flows. |
| [status/](status/) | Server-backed status landing flow. |

# Contracts

Web k6 tests are the parity reference for [../../../crates/waltr-tui/tests/server-backed/](../../../crates/waltr-tui/tests/server-backed/) flows.

Screenshot snapshots are validated with `pnpm test:web:screenshots`. Actual PNGs are written to ignored `.dev/web-screenshots/current/` and compared against local snapshot baselines in ignored `tests/web/k6/snapshots/screenshots/`.

Use `pnpm test:web:screenshots:update` after intentional visual changes to refresh those local snapshot baselines.

# Hints

Use the test-modification skill when modifying tests.
