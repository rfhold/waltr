# Agent Live Tests

Agent live tests validate agent behavior that requires real provider or external service integration.

# Scope

Use `crates/waltr-agent/tests/live.rs` and `crates/waltr-agent/tests/live/` for ignored live-provider coverage.

# Expectations

Use the shared live provider setup instead of local provider wiring.

Prefer deterministic agent tests for behavior that does not require a real provider.

Keep ignored live tests explicit about required gateway, API key, Docker, or external service setup.
