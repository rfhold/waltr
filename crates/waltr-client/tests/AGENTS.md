# Audience

Waltr client tests should act like the UI interacting with the shared client state machine.

# Boundaries

Client tests should validate reducer-style state transitions and server-facing flows without taking on TUI or web rendering concerns.

# Index

| Path | Info |
| --- | --- |
| [support/](support/) | Shared server and snapshot support for client integration tests. |
| [config/](config/) | Config read, watch, provider, and stale ETag flows. |
| [status/](status/) | Status and readiness flows. |
| [system/](system/) | Cross-feature client system flows. |

# Hints

Use the test-modification skill when modifying tests.
