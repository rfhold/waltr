# Audience

Waltr TUI tests should represent a few golden paths that validate client state connectivity from a terminal user's perspective.

# Boundaries

Non-server-backed TUI tests should focus on terminal routing, input, rendering, and UI transitions.

# Index

| Path | Info |
| --- | --- |
| [support/](support/) | Shared TUI rendering, input, and server support. |
| [snapshots/](snapshots/) | Golden render snapshots for stable UI paths. |
| [server-backed/](server-backed/) | Server-backed TUI flows that should mirror web k6 tests. |

# Hints

Use the test-modification skill when modifying tests.
