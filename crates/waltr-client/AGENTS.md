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

`AccessStatus`, access events, and access commands model Waltr login independently from provider/upstream-MCP `AuthTarget` authorization. Expiry/logout must clear protected snapshots, invalidate watch epochs, and prevent reconnect until credentials change; login restores status before interested child watches.

Native bearer credentials are process-memory leases over verified TLS. Preserve generation checks so stale login or refresh tasks cannot overwrite newer credentials. Explicit logout performs an anonymous status reprobe so disabled-auth servers recover.

Keep one cancellation-safe in-flight effect across command preemption and park denied streams after work drains. Provider/MCP catalog state and commands belong to this reducer; web and TUI must not bypass it with direct RPC transports.

Outbound OAuth snapshots may expose opaque attempt/lifecycle, completion mode, registration method, URL, and sanitized failures. Tokens, PKCE verifiers, callback state, refresh claims, and registration credentials must remain behind server/agent boundaries.

# Hints

Use the client-modification skill when modifying this crate.
