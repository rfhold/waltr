# Client And Server Tests

Client and server tests preserve separate layers of the same feature behavior.

# Scope

Use `crates/waltr-server/tests/` for server coordination, update, watch, validation, health, and proto contract behavior.

Use `crates/waltr-client/tests/` for UI-like interaction with the shared client state machine over the server contract.

Use `crates/waltr-client/tests/live.rs` and `crates/waltr-client/tests/live/` for ignored live-provider client tests. These tests should validate live provider interaction through `ClientRuntime`, `ClientCommand`, and `ClientSnapshot` only.

# Expectations

Server tests should act like a frontend is using the server.

Client tests should act like UI code is interacting with the client.

Client test bodies should not call server gRPC or mutate server internals directly after launch. If a deterministic client test needs existing server state, seed it before starting the server, then exercise behavior only through the client reducer/runtime surface.

Do not collapse client state-machine coverage into server tests or server contract coverage into UI tests.
