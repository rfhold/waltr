# Boundaries

Waltr server is a coordination layer so multiple clients can stay on the same page about state.

# Index

| Path | Info |
| --- | --- |
| [src/persistence/](src/persistence/) | SQLite/PostgreSQL selection, Waltr migrations, and MCP store composition. |
| [src/embedded.rs](src/embedded.rs) | Ambient-free embedded composition with explicit paths, prebound loopback, host policy, and cache source. |
| [src/local_admission.rs](src/local_admission.rs) | Process-generation bearer admission and host-owned self-MCP credentials. |
| [src/access_auth/](src/access_auth/) | Waltr access OIDC config, discovery, principals, browser sessions, bearer validation, and SQL domain store. |
| [src/mcp_oauth.rs](src/mcp_oauth.rs) | Hosted MCP OAuth configuration, Authentik-session bridge, consent adapter, keyring loading, and lifecycle composition. |
| [src/persistence/outbound_oauth.rs](src/persistence/outbound_oauth.rs) | Durable outbound MCP callback, credential, and refresh-claim state. |
| [src/routine/](src/routine/) | Server-owned event adapters, host boundary, Routine engine lifecycle, invocation execution, and expressions. |
| [../../docs/architecture/routines.md](../../docs/architecture/routines.md) | Canonical Routine lifecycle, persistence, replay, restart, and client boundary. |
| [../../docs/architecture/persistence.md](../../docs/architecture/persistence.md) | Canonical database ownership, migration, and startup contract. |
| [../../docs/architecture/access-authentication.md](../../docs/architecture/access-authentication.md) | Canonical access-auth contract. |
| [../../docs/deployment.md](../../docs/deployment.md) | Environment and deployment contract. |

# Dependents

| Path | Info |
| --- | --- |
| [../waltr-client/](../waltr-client/) | Client depends on the server via the proto contract. |

# Contracts

Server code should coordinate shared state without taking ownership of client-side UI state or reducer behavior.

Keep Waltr access authentication, hosted MCP authorization, and provider/upstream-MCP credential OAuth separate. Authentik authenticates resource owners; the generic MCP server owns downstream grants/tokens, and Waltr's outbound flow is target-scoped.

`build_app()` owns environment loading and is async/fallible because persistence migrations and OIDC discovery precede binding. Explicit builders receive initialized `Persistence` and, where enabled, `AccessAuthenticator`.

`build_embedded_server()` must use only explicit absolute paths and host policy. It prebinds ephemeral IPv4 loopback and returns one owned lifecycle.

Keep embedded configuration and lifecycle `Debug` output free of endpoints, bearers, paths, and task or thread handles.

Local admission protects `/waltr.*` and `/mcp` with one exact process-generation bearer. Keep health, readiness, and state-bound outbound OAuth callback routes outside this bearer.

Keep the embedded self-MCP credential host-owned and memory-only. Other provider and upstream-MCP OAuth credentials remain durable Waltr SQL state.

After prebinding loopback, atomically overwrite the ordinary persisted `mcp-servers/waltr.yaml` slot with the generation URL before catalog load. Keep its filesystem origin and ordinary inspect, replace, watcher, reset, and ETag behavior. Reset may fail normal candidate validation when the bundled fallback needs `WALTR_PUBLIC_URL`; do not add an embedded reset rule.

Use fixed bounds for models.dev HTTP connection and total request time. Fall back only to a valid configured cache after failure or timeout; malformed cache content must still fail startup. Do not add public catalog-timeout policy.

Derive the embedded outbound callback from the prebound loopback origin. Pass it explicitly through registration, attempt mode, and completion.

Embedded composition must not read public or target OAuth redirect variables. Standalone startup snapshots its environment-owned OAuth configuration once.

Embedded composition must ignore `WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS`. Use only explicit trusted private origins.

Persist an opaque generation marker instead of the embedded callback endpoint. Rehydrate it only for the exact active generation.

Reuse a valid embedded DCR registration during same-generation reauthorization. A replacement generation rejects stale attempts and credentials, returns not found for stale attempt status/cancel, and performs fresh DCR rather than reusing the old generation's registration.

Reject embedded self-MCP logout with failed precondition because its credential is host-owned; do not revoke or delete that generation credential.

Configure no embedded CIMD metadata URI. If upstream offers CIMD and DCR, skip CIMD and preserve preregistration or DCR selection.

Retain only supported outbound OAuth environment names. Keep arbitrary environment and all retained values out of `Debug`.

Keep one ordinary bundled and persisted resource catalog for standalone and embedded hosts. A build without the `shell` feature still accepts and exposes shell-bearing definitions, but runtime construction omits the shell tool and feature-derived shell tool permissions. Stdio MCP resources materialize and use ordinary runtime behavior in every build.

Bind the host-owned self-MCP credential to the exact initialized resource URL. If the persisted resource is edited to another endpoint, never send the generation bearer there.

Keep Pyroscope behind the optional `profiling` feature, which remains enabled by default. The standalone binary must also compile and run without `profiling`; embedded consumers use `default-features = false`.

Server shutdown must call the session host shutdown path. `SessionCoordinator::shutdown` rejects new work, cancels active and queued work, waits for run ownership to drain, atomically drains MCP preparation/cache state under its lifecycle gate, and awaits every cached `McpClientRuntime::shutdown`. Do not add a stronger termination bound than the MCP shutdown contract provides.

Waltr and MCP use independent pools and standard embedded SQLx migrators. SQLite requires separate ownership files; PostgreSQL migration histories live under the `waltr` and `mcp` schemas. Waltr owns outbound OAuth attempts/credentials; MCP owns hosted clients, grants, tokens, authorization leases, and encrypted signing keys.

`spec.sourceSession.snapshot` supports manual raw capture. `spec.sourceSession.pause` supports manual admission and `sessionLifecycle.runCompleted` admission through an exclusive agent-owned pause; it captures mask-effective completed context at a pinned cursor. Manual admission rejects busy sources before durable admission. Lifecycle admission atomically claims the completion barrier and transfers pause ownership only after durable admission. Other event-triggered Routines cannot request source capture. Request IDs provide correlation only; do not add retry or deduplication semantics implicitly. Durable admission and lifecycle state belong to Waltr SQL.

Every source-backed `sessionTemplate` atomically creates a durable helper with completed rollback-aware source history followed by its task prompt. Inference-only templates start with only their task prompt. Provider requests derive only from durable helper state, and ordinary session retention, delete, and wipe rules apply.

The fork compact helper stores `<fork-direction>\n<exact direction>\n</fork-direction>` after copied history. Its replacement system prompt treats the entire user message as untrusted emphasis text. The ordinary destination stores only compaction plus the original direction, not source history.

`SessionCreateRequest` atomically persists a Routine destination and its exact validated first prompt. Successful creation is the prompt-durability boundary. After cancellation or start failure, retain the destination and its ID.

# Hints

Use the server-modification skill when modifying this crate.
