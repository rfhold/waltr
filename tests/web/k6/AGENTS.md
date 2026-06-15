# Audience

Web k6 tests should validate browser-level server-backed flows for deployed web behavior.

# Boundaries

K6 tests should cover end-to-end browser behavior against a running server, not component-level web UI state.

# Index

| Path | Info |
| --- | --- |
| [support/](support/) | Shared browser checks, text helpers, and k6 browser setup. |
| [config/](config/) | Server-backed config provider, MCP, and watch flows. |
| [status/](status/) | Server-backed status landing flow. |

# Contracts

Web k6 tests are the parity reference for [../../../crates/waltr-tui/tests/server-backed/](../../../crates/waltr-tui/tests/server-backed/) flows.

# Hints

Use the test-modification skill when modifying tests.
