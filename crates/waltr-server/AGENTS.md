# Boundaries

Waltr server is a coordination layer so multiple clients can stay on the same page about state.

# Dependents

| Path | Info |
| --- | --- |
| [../waltr-client/](../waltr-client/) | Client depends on the server via the proto contract. |

# Contracts

Server code should coordinate shared state without taking ownership of client-side UI state or reducer behavior.

# Hints

Use the server-modification skill when modifying this crate.
