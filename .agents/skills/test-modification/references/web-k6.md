# Web K6 Tests

Web k6 tests validate browser-level server-backed flows against a running server.

# Scope

Use `tests/web/k6/` for deployed-style browser behavior that needs the server, web app, and browser runtime together.

# Expectations

Keep component-level UI state, form transitions, and routing tests in `crates/waltr-web/tests/`.

Use k6 for end-to-end server-backed behavior and parity with TUI server-backed flows.

Reuse k6 support helpers before adding new browser setup.

Use `pnpm test:web:screenshots` when visual validation is useful. This runs the same server-backed k6 browser flows, writes actual PNGs under ignored `.dev/web-screenshots/current/`, and validates them against static snapshots in `tests/web/k6/snapshots/screenshots/`.

Use `pnpm test:web:screenshots:update` after an intentional visual change to refresh the static snapshots.
