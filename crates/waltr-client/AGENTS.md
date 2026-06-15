# Boundaries

Waltr client is the shared state machine for clients to talk to the server.

# Dependents

| Path | Info |
| --- | --- |
| [../waltr-tui/](../waltr-tui/) | TUI should defer server data state to this crate. |
| [../waltr-web/](../waltr-web/) | Web should defer server data state to this crate. |

# Dependencies

| Path | Info |
| --- | --- |
| [../waltr-server/](../waltr-server/) | Client talks to the server through the proto contract. |

# Contracts

Client interactivity should use reducer-style state transitions so TUI and web can share the same server data behavior.

# Hints

Use the client-modification skill when modifying this crate.
