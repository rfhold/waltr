# Audience

Waltr server tests should act like a frontend is using the server through the client-facing contract.

# Boundaries

Server tests should validate shared state coordination, updates, watches, and status flows without asserting UI rendering behavior.

# Index

| Path | Info |
| --- | --- |
| [support/](support/) | Shared gRPC and server setup for integration tests. |
| [config/](config/) | Config read, watch, validation, provider, and stale ETag flows. |
| [status/](status/) | Status watch and health flows. |
| [system/](system/) | Process boot and broader server system flows. |

# Hints

Use the test-modification skill when modifying tests.
