# Index

| Path | Info |
| --- | --- |
| [src/agent_host/](src/agent_host/) | Concrete definitions, shared capability dispatch, tools/templates/middleware/Skills, worker gateway, and MCP runtime. |
| [src/permission/](src/permission/) | Codified policy and private process-local approval state. |
| [src/interaction.rs](src/interaction.rs) | Typed live broker, exact identity, snapshot/watch, and owner cancellation. |
| [src/auth/](src/auth/) | Upstream MCP OAuth and provider-only inference credential adapter. |
| [../../docs/agent-host/README.md](../../docs/agent-host/README.md) | Concrete host guides moved from generic agent docs. |
| [../../docs/runbooks/server-capability-cutover.md](../../docs/runbooks/server-capability-cutover.md) | Coordinated upgrade and separately authorized exact reset boundaries. |
| [src/persistence/](src/persistence/) | SQLite/PostgreSQL selection, Waltr migrations, and MCP store composition. |
| [src/routine_objects.rs](src/routine_objects.rs) | Immutable filesystem/S3 program and text payloads; SQL publication references and retained orphan objects. |
| [src/agent_host/session_roles.rs](src/agent_host/session_roles.rs) | Trusted process-local helper/task provenance, keyed by actual session and incarnation. |
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
| [src/self_mcp.rs](src/self_mcp.rs) | Standalone private MCP-only listener, exact logical binding, and joined generation cleanup. |
| [src/access_auth/](src/access_auth/) | Waltr access OIDC config, discovery, principals, browser sessions, bearer validation, and SQL domain store. |
| [src/mcp_oauth.rs](src/mcp_oauth.rs) | Hosted MCP OAuth configuration, Authentik-session bridge, consent adapter, keyring loading, and lifecycle composition. |
| [src/persistence/outbound_oauth.rs](src/persistence/outbound_oauth.rs) | Durable outbound MCP callback, credential, and refresh-claim state. |
| [src/routine/](src/routine/) | Shared JavaScript invocation service, typed host adapters and transport ownership; no task DSL or v1 executor. |
| [src/routine/production.rs](src/routine/production.rs) | Standalone/embedded composition, explicit default-deny roles and code profiles, normal helpers, lifecycle callbacks, and joined shutdown. |
| [src/routine/README.md](src/routine/README.md) | Versioned script API, bounded contracts, joined cancellation, and SQL history. |
| [src/routine/code_mode/README.md](src/routine/code_mode/README.md) | Optional principal-scoped agent/MCP code API, exact policy, durable outer association, and non-replay rules. |
| [src/routine/rpc/](src/routine/rpc/) | Production Routine RPC adapter, principal-scoped observations, transport owners, and opaque blob grants. |
| [src/routine_service.rs](src/routine_service.rs) | Production Routine RPC service export. |
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

Server owns concrete capabilities; `waltr-agent` remains the generic provider/model runner, neutral hooks, history/coordinator, and durable fences. All entry points share exact-identity `CapabilityDispatcher`, final middleware arguments/claims, worker Prepare/authorize/Execute, and live operation routes. Never detour through a model or fabricate a session for external `capability__*` calls. Those authenticated sessionless calls have full access over enabled capabilities, not a code grant or sandbox/upstream-auth bypass.

Private pending interactions and grants belong to `InteractionBroker`/`ServerPermissionPolicy`, not metadata, journal, or timeline. Responses bind session/generation/request/operation and resume the original live future. Cancel/drop removes pending state; observer release does not decide work. Restart discards requests/grants without replay. URL consent is not navigation or completion. Ordinary admitted MCP create/send prompts survive later token expiry; explicit Routine/helper/code leases remain separate.

Server code should coordinate shared state without taking ownership of client-side UI state or reducer behavior.

Keep public session revision CAS and strict Routine/context metadata/mask predicates separate from execution-token append checks. Adopt coherent append receipts before publication; preserve concurrent titles and current idle Apps admission checks. Actual settings/definition changes invalidate execution, never hot-swap live policy. Each session store requires one coordinated writer version; no automatic retained-state reset applies. See [session storage](../../docs/agent/storage.md#revision-domains-and-append-receipts) and [cutover](../../docs/runbooks/server-capability-cutover.md#session-revision-cutover).

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

`build_app(BuildAppOptions)` owns environment loading and is async/fallible because persistence migrations and OIDC discovery precede binding. Explicit builders receive initialized `Persistence` and, where enabled, `AccessAuthenticator`.

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

For access-disabled non-hosted standalone self-MCP, keep the private transport separate from the public router and embedded callback credentials. Require the initialized logical self target, a generation bearer, proxy bypass, no redirects, and opt-in generic endpoint redaction. Keep resource YAML and durable OAuth credentials unchanged.

Trusted standalone composition enables shared anonymous public MCP admission only with access auth disabled, no hosted OAuth, and no embedded local admission. Bare MCP construction remains fail-closed for capabilities and prompt admission. Represent anonymous access as `AnonymousDisabled`, never validated process identity. Preserve normal enabled-capability policy and operation ownership; no-auth grants no optional code/helper/media authority. Private self-MCP still requires its exact generation bearer.

Keep Pyroscope behind the optional `profiling` feature, which remains enabled by default. The standalone binary must also compile and run without `profiling`; embedded consumers use `default-features = false`.

Server shutdown must drain generic session run ownership and retire the server host runtime registry/private policy, awaiting cached MCP client shutdown and stdio child reaping. Do not put MCP lifecycle ownership back in the generic coordinator or promise a stronger termination bound than the MCP shutdown contract.

Waltr and MCP use independent pools and standard embedded SQLx migrators. SQLite requires separate ownership files; PostgreSQL migration histories live under the `waltr` and `mcp` schemas. Waltr owns outbound OAuth attempts/credentials; MCP owns hosted clients, grants, tokens, authorization leases, and encrypted signing keys.

Reviewed JS capture APIs distinguish raw completed history from mask-effective paused capture. Manual pause rejects busy sources before admission; lifecycle admission claims the generic completion barrier and transfers owned pause authority after durable admission. Request IDs are correlation, not implicit retry/deduplication. Durable JS admission and lifecycle belong to Waltr SQL; no task DSL remains.

Source-backed helpers use normal named agent bundles and durable rollback-aware completed history followed by the task prompt. Inference-only helpers start with the task prompt. Ordinary session retention/delete/wipe applies; JS supplies no replacement system prompt.

The fork helper stores `<fork-direction>\n<exact direction>\n</fork-direction>` after copied history. Its named agent's prompt treats that message as untrusted emphasis text. The destination stores compaction plus direction, not source history.

Routine JavaScript owns category schemas, checkpoint keys, background excerpts, title/fork/compaction policy and media helper arguments. Keep context operations generic. Original history requires declared capability plus independent source authorization, never a Routine name or public metadata. Private approved manage-context evidence binds only its exact normal dispatch.

Resources declare exact capture selections/contracts; declarations grant no helper tool permission. Preflight rejects definite capture denial before speech effects. Normal live Ask consent remains separate. JavaScript selects inference and speech models from the pinned catalog or operation defaults; verified host encoders bound attachment support.

Persist and verify immutable payloads before SQL references. Keep owner/quota/state publication atomic and uncertainty non-replayable. Migration 0016 refuses populated incompatible Routine state; no converter, body fallback, automatic wipe or orphan collector exists. Embedded object roots remain explicit and outside the workspace. Preserve current namespaces unless exact deletion receives separate authorization.

`SessionCreateRequest` atomically persists a Routine destination and its exact validated first prompt. Successful creation is the prompt-durability boundary. After cancellation or start failure, retain the destination and its ID.

# Hints

Use the server-modification skill when modifying this crate.
Keep production JS composition on the shared Routine RPC service; do not restore the removed v1/task executor.
Reuse one invocation service and shared attachment/blob registries across native and browser transports.
Keep attachment credentials, binary grants, and reader credentials out of logs, `Debug`, and durable history.
Authorize observation reads inside SQL before page limits or payload allocation.
