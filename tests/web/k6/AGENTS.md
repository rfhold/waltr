# Audience

Web k6 tests should validate browser-level server-backed flows for deployed web behavior.

# Boundaries

K6 tests should cover end-to-end browser behavior against a running server, not component-level web UI state.

# Index

| Path | Info |
| --- | --- |
| [support/](support/) | Shared browser checks, text helpers, and k6 browser setup. |
| [access/](access/) | Real OIDC login, secure session cookie, UI logout, TLS trust, and signed-out behavior. |
| [config/](config/) | Server-backed config provider, MCP, defaults, and watch flows. |
| [oauth/](oauth/) | Provider/upstream-MCP credential OAuth flows, separate from Waltr access auth. |
| [routines/](routines/) | Session-launched Routine list, detail, task disclosure, and responsive screenshot flows. |
| [status/](status/) | Server-backed status landing flow. |

# Contracts

Web k6 tests are the parity reference for [../../../crates/waltr-tui/tests/server-backed/](../../../crates/waltr-tui/tests/server-backed/) flows.

Compose profiles, Dex users, MCP endpoints, TLS names, and credentials are static test fixtures. Browser-owned actions such as logout must use real visible UI interactions; do not replace them with page-evaluated fetches.

Screenshot snapshots are validated with `pnpm test:web:screenshots`. Actual PNGs are written to ignored `.dev/web-screenshots/current/` and compared against local snapshot baselines in ignored `tests/web/k6/snapshots/screenshots/`.

Access-disabled semantic screenshot states use the shared responsive matrix: desktop 1440x900, tablet 768x1024, phone 390x844, and compact phone 320x700. Capture them through `screenshotMatrix`; generated filenames end in `--desktop.png`, `--tablet.png`, `--phone.png`, and `--compact.png`.

Use `pnpm test:web:screenshots:update` after intentional visual changes to refresh those local snapshot baselines.

# Hints

Use the test-modification skill when modifying tests.
