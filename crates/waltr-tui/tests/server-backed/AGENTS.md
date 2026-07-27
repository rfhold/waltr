# Audience

TUI server-backed tests should validate the same server-backed user flows as browser k6 tests from a terminal user's perspective.

# Boundaries

Server-backed TUI tests should cover client/server connectivity paths, not terminal-only rendering details.

Native OIDC discovery, loopback, and refresh mechanics are deterministic library tests. Add server-backed access coverage here only when it requires a real Waltr server contract and has a corresponding browser outcome.

# Contracts

| Path | Info |
| --- | --- |
| [../../../../tests/web/k6/](../../../../tests/web/k6/) | Web k6 tests are the parity reference for server-backed flows. |

# Hints

Use the test-modification skill when modifying tests.
