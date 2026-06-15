# TUI Server-Backed Tests

TUI server-backed tests validate real server flows from a terminal user's perspective.

# Scope

Use `crates/waltr-tui/tests/server_backed.rs` and `crates/waltr-tui/tests/server-backed/` for TUI coverage that requires a real Waltr server.

# Expectations

Mirror web k6 server-backed flows when the same user-facing feature exists in both clients.

Use shared TUI support for rendering, input, app driving, and server polling.

Keep terminal-only rendering and input transitions in non-server-backed TUI tests.
