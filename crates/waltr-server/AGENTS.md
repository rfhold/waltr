# Boundaries

Waltr server is a coordination layer so multiple clients can stay on the same page about state.

# Index

| Path | Info |
| --- | --- |
| [src/persistence/](src/persistence/) | SQLite/PostgreSQL selection, Waltr migrations, and MCP store composition. |
| [../waltr-web/src/mcp-apps/container/](../waltr-web/src/mcp-apps/container/) | Web-owned trusted container source; web also owns the bridge contract and browser fixtures. |
| [assets/status/](assets/status/) | Server-owned MCP status app source with a Vite module entry. |
| [package.json](package.json) | `@rfhold/waltr-server` workspace build and served-app contracts. |
| [vite.config.ts](vite.config.ts) | Shared single-file Vite configuration for independent served-app builds. |
| [scripts/apps.entries](scripts/apps.entries) | Explicit served-app names and source roots. |
| [scripts/](scripts/) | Served-app build, actual output/embedding contracts, and mocked Cargo asset orchestration. |
| [src/mcp_apps_host/](src/mcp_apps_host/) | Consumer isolation, asset routes, view storage, origin policy, and container lifecycle. |
| [src/session/mcp_apps.rs](src/session/mcp_apps.rs) | Owner-bound session Apps RPC handle registry, independent of asset features. |
| [src/mcp_service/apps/mod.rs](src/mcp_service/apps/mod.rs) | Static AppResource list, capability checks, descriptors, resource content, and CSP. |
| [src/mcp_service/apps/status.rs](src/mcp_service/apps/status.rs) | Ordinary MCP status tool linkage, not a special renderer. |
| [../../docs/mcp/apps-authoring.md](../../docs/mcp/apps-authoring.md) | Waltr-specific static tool-linked app addition workflow. |
| [src/http_lifecycle.rs](src/http_lifecycle.rs) | Neutral HTTP connection task ownership and bounded shutdown. |
| [src/standalone_apps.rs](src/standalone_apps.rs) | All-or-none standalone Apps opt-in and exact origin validation. |
| [src/embedded.rs](src/embedded.rs) | Ambient-free embedded composition with explicit paths, prebound loopback, host policy, and cache source. |
| [src/local_admission.rs](src/local_admission.rs) | Process-generation bearer admission and host-owned self-MCP credentials. |
| [src/access_auth/](src/access_auth/) | Waltr access OIDC config, discovery, principals, browser sessions, bearer validation, and SQL domain store. |
| [src/mcp_oauth.rs](src/mcp_oauth.rs) | Hosted MCP OAuth configuration, Authentik-session bridge, consent adapter, keyring loading, and lifecycle composition. |
| [src/persistence/outbound_oauth.rs](src/persistence/outbound_oauth.rs) | Durable outbound MCP callback, credential, and refresh-claim state. |
| [src/routine/](src/routine/) | Server-owned event adapters, host boundary, Routine engine lifecycle, invocation execution, and expressions. |
| [src/sandbox.rs](src/sandbox.rs) | Private gateway, standalone UDS worker connection, embedded duplex worker, readiness and shutdown; never mount it on public routes. |
| [src/workspace_paths.rs](src/workspace_paths.rs) | Startup checks keeping host-owned storage and config paths out of the workspace worker root. |
| [../../docs/architecture/agent.md](../../docs/architecture/agent.md) | Canonical agent/worker private RPC and dispatch-fence contract. |
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

Keep container assets separate from MCP-served app assets. `apps-assets` selects `apps-host-assets` and `apps-served-assets`; defaults and `web` retain that umbrella.
For served assets, run `pnpm --filter @rfhold/waltr-server build` for the explicit `scripts/apps.entries` list.
Keep each `dist/<name>/index.html` self-contained, nonempty, UTF-8, and at most one MiB.
Generate optional `<NAME>_HTML` constants in `served_app_assets.rs`; no-assets builds use `None`.
Publish resources through `AppResource` and `RESOURCES`; register tool declarations and execution explicitly in `mcp_service.rs`.
Consume web Vite container output.
Without a full web build or an override, run `pnpm --filter @rfhold/waltr-web build:app-container`.
With `web`, consume container output from one full browser build.
Do not run a second host-assets build.
`WALTR_APP_CONTAINER_DIST` bypasses the host-assets Vite build, not the full web or served-app builds.
Keep exactly `container.html` and `container.js` as the container artifact pair.
Exclude `/app-container/` and legacy `/apps/` from API web assets.
Keep `app_container` as the public compatibility reexport; implementation belongs in `mcp_apps_host`.
Require Node, pnpm, and installed workspace dependencies for asset builds; Cargo installs no Node packages.
Use `pnpm test:status-app` for actual output, independent second-app embedding, and mocked orchestration checks.
Keep the second fixture test-only; it has no production build entry or resource registration.
The orchestration contract alone does not prove a real asset build.

Standalone Apps startup requires all three runtime `WALTR_APP_CONTAINER_*` variables and the `web` feature.
Keep ordinary unset startup unchanged; reject partial or unsupported configuration.
Use `bind_with_apps_config` for owned listeners, HTML bootstrap, rollback, and joined shutdown.
Treat preview/prod Pulumi Apps routes as source configuration, not proof of live DNS, TLS, or deployment.

Keep Waltr access authentication, hosted MCP authorization, and provider/upstream-MCP credential OAuth separate. Authentik authenticates resource owners; the generic MCP server owns downstream grants/tokens, and Waltr's outbound flow is target-scoped.

API-root protected-resource metadata advertises `waltr_native_client` only when `WALTR_API_OIDC_CLIENT_ID` is configured. Keep public client ID, deduplicated `openid`/`offline_access`/required API scopes, and API audience separate from browser secrets and hosted MCP metadata. See [native bootstrap](../../docs/architecture/access-authentication.md#url-only-native-bootstrap).

`build_app()` owns environment loading and is async/fallible because persistence migrations and OIDC discovery precede binding. Explicit builders receive initialized `Persistence` and, where enabled, `AccessAuthenticator`.

`build_embedded_server()` must use only explicit absolute paths and host policy. It prebinds ephemeral IPv4 loopback and returns one owned lifecycle.

Keep embedded configuration and lifecycle `Debug` output free of endpoints, bearers, paths, and task or thread handles.

Local admission protects `/waltr.*` and `/mcp` with one exact process-generation bearer. Keep health, readiness, and state-bound outbound OAuth callback routes outside this bearer.

Keep the embedded self-MCP credential host-owned and memory-only. Other provider and upstream-MCP OAuth credentials remain durable Waltr SQL state.

After prebinding loopback, atomically overwrite the ordinary persisted `mcp-servers/waltr.yaml` slot with the generation URL before catalog load. Keep its filesystem origin and ordinary inspect, replace, watcher, reset, and ETag behavior. Reset may fail normal candidate validation when the bundled fallback needs `WALTR_PUBLIC_URL`; do not add an embedded reset rule.

Use fixed bounds for models.dev HTTP connection and total request time. After failure or timeout, prefer a valid configured cache. Only when that cache is absent may an embedding host's explicit built-in seed satisfy startup, marked stale. A malformed or unreadable configured cache must still fail startup. Standalone mode has no built-in fallback. Do not add public catalog-timeout policy.

Keep the production fallback at `resources/models-dev-defaults.json` limited to verified current model IDs needed by bundled providers. Keep synthetic selector edge cases in `tests/fixtures/models-dev-defaults.json`; do not promote them into shipped fallback data.

Derive the embedded outbound callback from the prebound loopback origin. Pass it explicitly through registration, attempt mode, and completion.

Embedded composition must not read public or target OAuth redirect variables. Standalone startup snapshots its environment-owned OAuth configuration once.

Embedded composition must ignore `WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS`. Use only explicit trusted private origins.

Persist an opaque generation marker instead of the embedded callback endpoint. Rehydrate it only for the exact active generation.

Reuse a valid embedded DCR registration during same-generation reauthorization. A replacement generation rejects stale attempts and credentials, returns not found for stale attempt status/cancel, and performs fresh DCR rather than reusing the old generation's registration.

Reject embedded self-MCP logout with failed precondition because its credential is host-owned; do not revoke or delete that generation credential.

Configure no embedded CIMD metadata URI. If upstream offers CIMD and DCR, skip CIMD and preserve preregistration or DCR selection.

Retain only supported outbound OAuth environment names. Keep arbitrary environment and all retained values out of `Debug`.

Keep one ordinary bundled and persisted resource catalog for standalone and embedded hosts. A build without the `shell` feature still accepts and exposes shell-bearing definitions, but runtime construction omits the shell tool and feature-derived shell tool permissions. Stdio MCP resources materialize and use ordinary runtime behavior in every build.

Standalone mode uses `WALTR_SANDBOX_SOCKET=/run/waltr-sandbox/gateway.sock` for a local Unix domain socket and `WALTR_WORKSPACE_ROOT=/workspace` for the workspace identity. Connect a separate worker with the same socket path and `WALTR_SANDBOX_WORKSPACE_ROOT=/workspace`. Require a connected worker and its canonical root before startup completes; report disconnect through `/ready` without host-side execution fallback. Keep the socket private and off public routes/TCP.

Embedded desktop and Android always start the process-local duplex worker using explicit app-private roots; ignore standalone environment overrides. Shut down the owned worker/gateway with the host. A sidecar's separate mounts do not imply network isolation in a shared pod.

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
