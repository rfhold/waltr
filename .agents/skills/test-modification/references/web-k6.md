# Web K6 Tests

Web k6 tests validate browser-level server-backed flows against a running server.

# Scope

Use `tests/web/k6/` for deployed-style browser behavior that needs the server, web app, and browser runtime together.

# Expectations

Keep component-level UI state, form transitions, and routing tests in `crates/waltr-web/tests/`.

Use k6 for end-to-end server-backed behavior and parity with TUI server-backed flows.

Reuse k6 support helpers before adding new browser setup.
