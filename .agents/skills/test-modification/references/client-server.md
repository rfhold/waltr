# Client And Server Tests

Client and server tests preserve separate layers of the same feature behavior.

# Scope

Use `crates/waltr-server/tests/` for server coordination, update, watch, validation, health, and proto contract behavior.

Use `crates/waltr-client/tests/` for UI-like interaction with the shared client state machine over the server contract.

# Expectations

Server tests should act like a frontend is using the server.

Client tests should act like UI code is interacting with the client.

Do not collapse client state-machine coverage into server tests or server contract coverage into UI tests.
