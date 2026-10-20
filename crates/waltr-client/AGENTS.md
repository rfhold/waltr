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

Native bearer credentials are process-memory leases over verified TLS. The shell-neutral native-access persistence boundary may durably store only opaque refresh-token bytes; discovery and all endpoint/client metadata remain current configuration. Persist rotation before bearer installation, retain durable credentials across ordinary stop and transient failures, and delete them on logout, malformed storage, exact OAuth `invalid_grant`, or terminal Waltr credential rejection. Tonic API transport and native-access reqwest must use rustls with the ring provider, TLS 1.2/1.3, bundled `webpki_roots`, and normal chain and hostname verification. Do not add platform roots, private or user CAs, `SSL_CERT_FILE`, Unix certificate-file discovery, or Android Java platform-verifier initialization. Preserve the native-access HTTP exception only for exact loopback hosts `127.0.0.1`, `localhost`, and `::1`. Preserve generation checks so stale login or refresh tasks cannot overwrite newer credentials. Explicit logout performs an anonymous status reprobe so disabled-auth servers recover.

`discover_native_access_config(endpoint, callback)` returns public configuration and the exact validated issuer from API-root discovery. Its `waltr_native_client` metadata is a Waltr extension, not universal OIDC client discovery or DCR. Preserve exact API resource/issuer validation and explicit TUI environment behavior. See [native bootstrap](../../docs/architecture/access-authentication.md#url-only-native-bootstrap).

Keep loopback and external native callback policies separate. External callback data is opaque outside the native access coordinator; exact redirect, state, code/error, duplicate-parameter, and PKCE validation remain in Rust. Deliver queued external callbacks serially into the bounded attempt lane so invalid input cannot drop a later valid callback. Callback delivery must stay bounded and stale-safe so stale input cannot displace current credentials or complete another attempt.

Keep one cancellation-safe in-flight effect across command preemption and park denied streams after work drains. Provider/MCP catalog state and commands belong to this reducer; web and TUI must not bypass it with direct RPC transports.

Outbound OAuth snapshots may expose opaque attempt/lifecycle, completion mode, registration method, URL, and sanitized failures. Tokens, PKCE verifiers, callback state, refresh claims, and registration credentials must remain behind server/agent boundaries.

# Hints

Use the client-modification skill when modifying this crate.
