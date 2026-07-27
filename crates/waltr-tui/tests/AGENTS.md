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

# Contracts

Native OIDC protocol/runtime tests live with `src/native_auth.rs`; app tests own access-screen routing, copy/cancel/retry/expiry rendering, logout actions, and separation from provider OAuth.

# Hints

Use the test-modification skill when modifying tests.
