# Audience

Web tests should validate UI-state-intensive flows like forms and UI transitions from a browser user's perspective.

# Boundaries

Web tests should cover browser routing, forms, transitions, and UI-specific state rather than server-backed browser flows.

# Index

| Path | Info |
| --- | --- |
| [support/](support/) | Shared web rendering, routing, and snapshot support. |
| [config/](config/) | Config form, routing, provider, model, MCP, read, and watch flows. |
| [status/](status/) | Status, splash, readiness, landing, and wiring flows. |
| [system/](system/) | Cross-feature web shell routing flows. |

# Contracts

Web tests should mirror non-server-backed TUI test intent for shared user-facing flows.

# Hints

Use the test-modification skill when modifying tests.

Use `tests/web/k6/` for server-backed browser flows. Screenshot actuals are written under ignored `.dev/web-screenshots/current/`; refresh local snapshot baselines under ignored `tests/web/k6/snapshots/screenshots/` with `pnpm test:web:screenshots:update` after intentional visual changes.
