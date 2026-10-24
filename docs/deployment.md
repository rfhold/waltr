# Deployment

## Persistence

`WALTR_DATABASE_URL` selects the Waltr-owned database used for access-auth, outbound OAuth, and Routine invocation history. Optional `WALTR_MCP_DATABASE_URL` independently selects the database used for the hosted MCP authorization server.

| Value | Behavior |
| --- | --- |
| Unset | Waltr SQLite at `$XDG_DATA_HOME/waltr/waltr.sqlite3`, or `$HOME/.local/share/waltr/waltr.sqlite3` when `XDG_DATA_HOME` is unset. MCP uses sibling `mcp.sqlite3`. Startup fails if no absolute data directory can be resolved. |
| `sqlite:<path>` | File-backed SQLite. Parent directories are created. Use an explicit SQLite memory URL only for ephemeral development or tests. |
| `postgres://...` or `postgresql://...` | PostgreSQL. A supplied `sslmode` is preserved; otherwise Waltr defaults to `sslmode=require`. |

When `WALTR_MCP_DATABASE_URL` is absent or empty, file-backed SQLite derives sibling `mcp.sqlite3` while preserving safe parsed SQLite options; PostgreSQL reuses the same logical database through a separate pool. An explicit MCP URL may override either default. Waltr rejects physical aliases for the same SQLite file and equal named shared in-memory databases. Separate private `sqlite::memory:` targets create independent Waltr and MCP databases.

These databases are not the agent session store configured by `WALTR_SESSION_STORE`. The Waltr database is the authoritative store for both provider and upstream-MCP OAuth credentials; the environment-owned server has no credential-file setting.

Waltr and MCP use separate pools. SQLite keeps Waltr's standard `_sqlx_migrations` history, OIDC tables, hosted-OAuth login continuations, outbound OAuth attempts/credentials, and Routine history in `waltr.sqlite3`, while MCP history, inert session/event tables, clients, grants, token state, CIMD cache, and encrypted signing keys live in `mcp.sqlite3`. PostgreSQL uses `waltr` and `mcp` search paths for migration application, producing `waltr._sqlx_migrations` and `mcp._sqlx_migrations` in the default shared logical database. Waltr's immutable sequence is `0001_schema_ownership`, `0002_oidc_access_auth`, `0003_mcp_oauth_login_continuations`, `0004_outbound_oauth`, `0005_unified_oauth_credentials`, `0006_routine_invocations`, and `0007_routine_interaction_trigger`; MCP's is `0001_streamable_http_sessions`, `0002_oauth_authorization_server`, and `0003_oauth_client_registration`. V5 is a hard cutover that discards existing MCP SQL credentials and stops reading legacy provider credential files; all targets must authorize again after upgrade. Legacy `auth.json` files are not deleted automatically and should be securely removed after rollback is no longer required. V6 adds exactly the Routine aggregate and append-only event-journal tables, with no task table or pruning. V7 extends the stored Routine trigger kind to include interactions and preserves prior invocation history. Both owners use embedded `sqlx::migrate!()` migrators, so SQLx owns standard version and checksum validation. Applied migration files must not be edited. Both migrators complete before the server binds. Invalid URLs, unavailable databases, SQLx migration validation failures, migration application failures, missing wrapping keys, and unusable signing state fail startup; there is no memory fallback.

## Container Runtime And Local Compose

The repository `Dockerfile` produces the production server `runtime` image, a separate `worker-runtime` image for standalone workspace operations, and test-only stages. The server runtime contains the release server with embedded web assets, runs as nonroot UID/GID `65532`, listens on `0.0.0.0:14333`, and uses `/data` as its home, working directory, and default writable persistence root. Mount writable storage at `/data` for filesystem-backed settings, or configure external PostgreSQL and S3 services. The worker image runs the sandbox executable; it is not a second public server and does not need the server's database, OAuth secrets, or `/data` mount.

The server's default Cargo feature set includes `shell` and `profiling`. The optional `profiling` feature adds Pyroscope initialization, but the standalone server binary remains available and functional when that feature is disabled.

Embedded Tauri builds use `waltr-server` with `default-features = false`. Desktop adds only `shell`; Android adds neither shell nor Pyroscope.

The build installs pnpm dependencies from workspace manifests before copying Rust and web sources. It then copies only the Cargo workspace files and `crates/` into the shared builder, so documentation, infrastructure, and repository tooling changes do not invalidate application compilation. BuildKit cache mounts retain the pnpm store and Cargo registry, Git, and target data under architecture-specific IDs. Those mutable caches are local to the selected BuildKit worker; the repository pipelines do not import or export a registry-backed build cache. Persistent cluster workers and daemon-configured package or registry mirrors can accelerate builds transparently, but their availability and retention are infrastructure concerns rather than repository guarantees.

`compose.yml` remains fixed local development and integration-test infrastructure. It intentionally exposes only static `dev`, `test`, and `test-auth` profiles; bare `docker compose up` starts nothing.

`docker compose --profile dev up` builds the server and web assets for local use, with the separate worker image alongside it. The `waltr` service uses:

- `WALTR_DATABASE_URL=sqlite:///data/waltr.sqlite3`
- Empty `WALTR_MCP_DATABASE_URL`, which derives the MCP target
- Derived MCP database `/data/mcp.sqlite3`
- `WALTR_SESSION_STORE=filesystem`
- `WALTR_SESSION_FILESYSTEM_ROOT=/data/sessions`
- `WALTR_PUBLIC_URL=http://127.0.0.1:14333`
- Named volume `waltr-dev-data` mounted at `/data`

The server and worker share the private socket directory; the worker mounts the writable workspace at `/workspace`, not the server's `/data` or secret mounts. Set the standalone socket and workspace environment as described in [Workspace Worker Deployment](#workspace-worker-deployment). The `test` and `test-auth` profiles use the same private-worker topology with ephemeral workspace and socket mounts. Do not add a public worker port or an ambient Compose profile switch.

The static profile does not consume ambient `WALTR_*` deployment settings. Change server environment configuration in the actual process/container runtime used by a deployment, not through this Compose fixture.

The `dev` volume contains both SQLite files, including unified outbound credentials in `waltr.sqlite3`, and filesystem agent sessions. It is disposable local state, not a production backup contract.

The `test` and `test-auth` profiles use tmpfs persistence, internal-only networking, health-gated startup, deterministic Dex/MCP fixtures, and an ephemeral TLS CA. Their `WALTR_OAUTH_*` and `WALTR_TEST_MCP_*` values are test-fixture settings, not server deployment variables. The authenticated Waltr fixture enables the hosted MCP issuer, generates an ephemeral keyring in tmpfs, and sets `WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS=true` only so HTTPS Waltr-to-Waltr CIMD and token requests can reach private Compose aliases. Production deployments must leave this deterministic test exception unset. The fixture does not configure a direct MCP issuer or a preregistered self client.

## Workspace Worker Deployment

Standalone Waltr uses one local worker for workspace filesystem, context, skills, patch, and optional shell operations. The server hosts the private sandbox gateway; the separate worker connects over a Unix domain socket (UDS). Configure both processes with absolute paths:

| Process | Setting | Standalone value | Purpose |
| --- | --- | --- | --- |
| Server | `WALTR_SANDBOX_SOCKET` | `/run/waltr-sandbox/gateway.sock` | Select the private UDS listener instead of the process-local worker. |
| Server | `WALTR_WORKSPACE_ROOT` | `/workspace` | Workspace identity passed to agent runtime construction and checked against the worker's reported canonical root. |
| Worker | `WALTR_SANDBOX_SOCKET` | `/run/waltr-sandbox/gateway.sock` | Connect to the server's gateway on the shared socket directory. |
| Worker | `WALTR_SANDBOX_WORKSPACE_ROOT` | `/workspace` | Workspace tree visible to workspace tools in the worker. |

Mount a writable socket directory at `/run/waltr-sandbox` in both processes with permissions allowing their runtime identities to connect; share the socket directory, not a TCP port. Mount the workspace at `/workspace` in the worker. Keep server persistence (`/data`), resource overrides, credentials, and projected secrets outside the worker mount. Server startup preflight checks known configured host-owned paths for overlap with the workspace, including canonicalized existing parents; it does not recursively audit arbitrary workspace content. The worker's canonical root must agree with the server's selected workspace root. Standalone deployments must set the socket to use the separate process; embedded Tauri hosts always use in-process duplex transport and explicit app-private workspace paths, ignoring these environment overrides.

Compose defines static `dev`, `test`, and `test-auth` profiles, not a dynamic worker-mode profile. The worker is an internal service in each profile; server/test health gates must wait for a working connection. The test fixtures keep `/data` and workspace state ephemeral. A lost UDS stream makes `/ready` fail until the same worker reconnects, while `/health` continues to report server liveness; a worker that cannot connect during startup fails startup rather than silently moving workspace operations into the server. See [Private Workspace Boundary](architecture/agent.md#private-workspace-boundary) for request deadlines, reconnect, and unknown-outcome handling.

## MCP Apps Assets And App Container

MCP Apps uses SDK assets and a separate-origin, asset-only listener.
The bundled TUI with web assets owns that listener in the same process.
Its normal loopback startup requires no separate command or Apps environment variables.
This app container differs from the private workspace worker.
It exposes no Waltr API, MCP endpoint, OAuth route, admission credentials, or filesystem access.
Normal Apple Silicon macOS builds also own a dedicated loopback Apps listener; other native platforms remain unsupported.
The host configuration below covers the hosted browser.
See [Clients](architecture/clients.md#session-mcp-apps) for host policy and native limitations.

### Build Prerequisites

Run these commands from the repository root after `pnpm install`:

```sh
cargo build -p waltr-server
```

The default server features include `apps-assets`; `web` also enables it, even without default features.
The umbrella feature selects both independent asset features:

| Feature | Assets |
| --- | --- |
| `apps-host-assets` | Web-owned trusted container HTML and JavaScript from `crates/waltr-web/dist/app-container/`. |
| `apps-served-assets` | Explicit server-owned app HTML from `crates/waltr-server/scripts/apps.entries`, currently `status`. |

Web owns the trusted container source in `src/mcp-apps/container/`, the shared `src/mcp-apps/bridge-contract.ts`, and the `app-container/container.html` entry.
These paths belong to `crates/waltr-web`.
The normal Vite browser build emits the UI plus `dist/app-container/container.html` and `dist/app-container/container.js`.
Vite assertions require a self-contained container script with no shared chunks, dynamic imports, or external asset references.
The container HTML references only `./container.js`.
The `app-container` Vite mode emits only those two files and requires no WASM build.
Tauri frontend modes omit the container; Apple Silicon macOS Rust builds embed it through the server host-assets feature.

Without a full web build or an override, the server runs `pnpm --filter @rfhold/waltr-web build:app-container` for host assets.
It copies the two output files into Cargo's `OUT_DIR`.
With the `web` feature, the server invokes one full web build and consumes its container output without a second host-assets build.
For served assets, the server runs `pnpm --filter @rfhold/waltr-server build`.
The `@rfhold/waltr-server` workspace package owns the served-app SDK and Vite toolchain.
It pins `@modelcontextprotocol/ext-apps` to `1.2.2`, Vite to `7.2.4`, and `vite-plugin-singlefile` to `2.3.0`.
`scripts/apps.entries` declares each app name and source root relative to the server package.
`scripts/build-apps.mjs` runs independent standard Vite single-file builds through the shared configuration.
Each `dist/<name>/` contains only a nonempty UTF-8 `index.html`, at most one MiB, with no external scripts or stylesheets.
Cargo validates each document and copies it into `OUT_DIR/<name>.html`.
Cargo generates `served_app_assets.rs` with optional `<NAME>_HTML` constants; builds without served assets generate `None`.
The explicit Rust `AppResource` and `RESOURCES` list owns publication, capability checks, descriptors, content, and deny-all external CSP.
Build entries do not create tools or a runtime app catalog.
See [Waltr-served Apps authoring](mcp/apps-authoring.md) for the static addition workflow.
Node and pnpm must exist on `PATH`; asset builds require installed workspace dependencies.
Root dependencies supply only Playwright; each asset package owns its build dependencies.
Cargo installs no Node packages; all asset builds use standard package commands.
The Docker build copies all current workspace package manifests before its frozen pnpm install.
Keep that manifest set complete when workspace packages change.
Production asset builds do not import browser fixtures.
The public MCP `status` declaration links the app only for requests advertising the HTML Apps MIME type; resource reads require the same capability.
The app requests no external CSP origins or browser permissions and refreshes through `tools/call`, not HTTP fetch or inference.
The main web asset collection excludes `/app-container/` and legacy `/apps/` files.
A server build with `--no-default-features` and no asset feature omits Apps assets.
Session Apps RPC support remains independent of both asset features.
Asset inclusion alone does not start an Apps listener in the standalone server.

`WALTR_APP_CONTAINER_DIST` optionally selects a prebuilt container artifact directory at compile time.
Use an absolute path for a portable artifact override.
The build script does not enforce absolute paths; it reads the supplied directory directly.
With `apps-host-assets`, the override bypasses the host-assets Vite build and copies its files into `OUT_DIR`.
It preserves the supplied directory; a `web` build still runs its full browser build.
With `apps-served-assets`, all explicit served apps still build through the server workspace package.
The override directory must contain exactly UTF-8 `container.html` and `container.js` files, each at most one MiB.
The hosted web build also requires the WASM prerequisites in [Contributing](../CONTRIBUTING.md#building).

### Bundled Startup

`pnpm install:bundled` installs `waltr` through `cargo install --features bundled`; the Rust builder supplies the required asset builds.
The `bundled` feature selects the embedded server, web assets, and default server startup.
Web-enabled TUI startup uses `ServerApplication::bind_with_apps` to own both listeners.
The Apps listener defaults to an ephemeral `127.0.0.1:0` address, separate from the API origin.
The host derives exact parent origins from the actual loopback API address and its `localhost` alias.
It injects the public app container URL into the HTML head through `waltr-mcp-apps-container-url` metadata.
Root and deep-link shell responses use `Cache-Control: no-store` so they retain no stale listener URL.
The API origin never serves Apps assets.

The browser prefers this runtime metadata over a stale compiled Vite URL.
Present but invalid or duplicate metadata fails closed; it does not fall back to the compiled URL.
Without runtime metadata, external browser hosts still use `VITE_WALTR_MCP_APP_CONTAINER_URL`.

The bundled host rolls back both listeners after startup failure.
Normal and error TUI exits cancel and join the owned HTTP tasks and Apps expiry task.
Unexpected Apps listener termination fails the server and enters the same cleanup path.
HTTP shutdown permits at most five seconds for request drain before cleanup proceeds.
Plain TUI mode displays no Apps UI and starts no Apps listener without the `web` feature.
The standalone `waltr-server` API entry point uses this owned lifecycle only after explicit configuration of all three Apps environment variables.

### Configuration

| Setting | Contract |
| --- | --- |
| `WALTR_APP_CONTAINER_ADDR` | Dedicated listener address. Bundled web startup defaults to `127.0.0.1:0`; the optional `app-container` CLI defaults to `127.0.0.1:14334`. Standalone opt-in requires an explicit address. |
| `WALTR_APP_CONTAINER_ORIGIN` | Optional exact HTTP(S) public origin. Loopback listeners derive an origin when absent; external binds require an explicit origin. |
| `WALTR_APP_CONTAINER_HOST_ORIGINS` | Comma-separated exact parent origins, with 1 through 20 entries. Bundled loopback startup adds its exact API origins; external startup requires explicit parents. |
| `VITE_WALTR_MCP_APP_CONTAINER_URL` | Optional web build/dev app container URL for an external host without runtime metadata, such as `http://127.0.0.1:5174/container.html`. |

The app container origin must differ from every allowed host origin.
For loopback public origins, the configured port must match the listener port.
Use the browser's exact parent origin in `WALTR_APP_CONTAINER_HOST_ORIGINS`.
`localhost` and `127.0.0.1` are different origins; manual hosts require exact parent configuration.
The browser permits HTTP app container URLs only on loopback.
Non-loopback bundled API binds require explicit public app container and parent origins; incomplete policy rejects startup.

### Optional External Host

The standalone asset CLI remains available for Vite or other external browser hosts.
It is not a prerequisite for normal bundled startup.
For a Vite host at `http://127.0.0.1:5173`, start a stable separate listener:

```sh
WALTR_APP_CONTAINER_ADDR=127.0.0.1:5174 \
WALTR_APP_CONTAINER_HOST_ORIGINS=http://127.0.0.1:5173 \
target/debug/waltr-server app-container
```

In another terminal, configure the browser bundle:

```sh
VITE_WALTR_MCP_APP_CONTAINER_URL=http://127.0.0.1:5174/container.html \
pnpm --filter @rfhold/waltr-web dev
```

This asset listener does not start the Waltr API server.
Start the ordinary API server separately with its usual persistence, workspace worker, and public URL configuration.
If Vite selects another port, update the exact parent origin before use.
For bundled web startup, configure runtime origins instead of a compiled Vite URL.

### Hosted Origin And Limits

Use a dedicated HTTPS, cookie-free asset origin for hosted deployments.
Configure its exact public origin and allowed Waltr parent origins.
Keep Waltr session cookies, API routes, authorization routes, and reverse-proxy credentials off that origin.
Do not merge the Apps router into the main API router or serve app container files from the Waltr origin.
Standalone startup leaves Apps disabled when all three `WALTR_APP_CONTAINER_*` runtime variables are absent.
Explicit opt-in requires `WALTR_APP_CONTAINER_ADDR`, `WALTR_APP_CONTAINER_ORIGIN`, and `WALTR_APP_CONTAINER_HOST_ORIGINS` together, plus the server `web` feature.
Partial configuration or an unsupported build fails startup; invalid, wildcard, or same-origin policy also fails closed.
The server uses `bind_with_apps_config` for both listeners, runtime HTML bootstrap, startup rollback, child-failure cleanup, and joined shutdown.
No compiled Vite URL or separate asset process is necessary for this topology.

The Pulumi source configures this topology for both stacks:

| Stack | Namespace | Main origin | Apps origin |
| --- | --- | --- | --- |
| `preview` | `waltr-preview` | `https://preview-waltr.holdenitdown.net` | `https://preview-waltr-apps.holdenitdown.net` |
| `prod` | `waltr` | `https://waltr.holdenitdown.net` | `https://waltr-apps.holdenitdown.net` |

Each stack sets the address to `0.0.0.0:14334`, the public origin to its Apps origin, and allowed parents to its main origin.
The same server process exposes container port `apps-http` at `14334`; no Apps sidecar exists.
A separate `waltr-apps` Service selects the server workload and declares `appProtocol: http`.
Its own `waltr-apps` HTTPRoute targets that Service through the external `ingress/default-gateway` Gateway.
The main Service and route retain port `14333`.
Pulumi source configuration does not prove live deployment, DNS resolution, Gateway admission, or certificate coverage.
Verify DNS and TLS for both Apps hostnames separately before rollout.
These instructions authorize no commit, push, or deployment.

The service accepts app-container-origin `POST /views` uploads of at most one MiB of UTF-8 HTML.
It stores at most 64 views and 16 MiB total, with 60 uploads per minute per service instance.
Each view receives a random 256-bit token and expires after ten minutes.
`DELETE /views/{token}` requires the same exact Origin check and removes the view.
Unknown or expired view tokens return 404.

The service returns `no-store`, `no-referrer`, `nosniff`, a restrictive Permissions-Policy, and HTTP CSP headers.
The view CSP denies network access, external scripts, nested frames, objects, base URLs, and forms.
It permits bundled inline scripts/styles and data images/media under `sandbox allow-scripts`.
The app container's `frame-src 'self'` also constrains view navigation; `navigate-to` alone is not portable enforcement.
`frame-ancestors` restricts both app container and view ancestor chains to configured origins.
The service grants no resource-declared external origins or browser permissions.

Sources: [server asset build](../crates/waltr-server/build.rs), [web Vite build](../crates/waltr-web/vite.config.ts),
[container entry](../crates/waltr-web/app-container/container.html), [server features](../crates/waltr-server/Cargo.toml),
[bundled install](../package.json), [TUI lifecycle](../crates/waltr-tui/src/main.rs),
[server lifecycle](../crates/waltr-server/src/app.rs), [HTML bootstrap](../crates/waltr-server/src/web_assets.rs),
[server package](../crates/waltr-server/package.json), [status Vite build](../crates/waltr-server/vite.config.ts),
[asset service](../crates/waltr-server/src/mcp_apps_host/container.rs), [CLI](../crates/waltr-server/src/main.rs),
and [web policy](../crates/waltr-web/src/mcp-apps/policy.ts).
Hosted opt-in sources: [standalone policy](../crates/waltr-server/src/standalone_apps.rs),
[Pulumi topology](../infra/pulumi/index.ts), [preview stack](../infra/pulumi/Pulumi.preview.yaml), and [production stack](../infra/pulumi/Pulumi.prod.yaml).

## Agent Session Storage

`WALTR_SESSION_STORE` selects agent session storage independently of the Waltr and MCP SQL databases:

| Value | Configuration |
| --- | --- |
| Unset or `filesystem` | Uses `WALTR_SESSION_FILESYSTEM_ROOT`, defaulting to `$HOME/.local/share/waltr/sessions`. |
| `memory` | Ephemeral process-local sessions; suitable only for development and tests. |
| `s3` | Requires the S3 connection and credential variables below; write mode is optional. |

| S3 variable | Contract |
| --- | --- |
| `WALTR_SESSION_S3_ENDPOINT` | HTTPS base URL without credentials, path, query, or fragment; HTTP is allowed only on loopback. |
| `WALTR_SESSION_S3_REGION` | S3 region. |
| `WALTR_SESSION_S3_BUCKET` | Existing bucket name. |
| `WALTR_SESSION_S3_PREFIX` | Object-key prefix for Waltr session data. |
| `WALTR_SESSION_S3_ACCESS_KEY_ID` | Static S3 access-key ID. |
| `WALTR_SESSION_S3_SECRET_ACCESS_KEY` | Static S3 secret access key. |
| `WALTR_SESSION_S3_FORCE_PATH_STYLE` | Exactly `true` or `false`. |
| `WALTR_SESSION_S3_WRITE_MODE` | Optional exact value `conditional-etag` or `single-writer`; defaults to `conditional-etag`. |

`conditional-etag` uses S3 object ETags to compare and swap updates across writers. A backend that does not support `If-Match` on `PutObject` fails after eight conditional attempts rather than retrying forever. `single-writer` uses process-local per-session serialization and unconditional PUTs. It is safe only when one Waltr process owns the session prefix and no external writer modifies those objects; overlapping processes can lose updates.

The Pulumi workload definition uses S3 session storage backed by Ceph RGW, which does not support the required conditional PUT behavior. Pulumi therefore explicitly selects `single-writer`, one Waltr replica, and a `Recreate` strategy. Do not increase the replica count, change to a strategy that overlaps old and new pods, or introduce another writer without first selecting and validating a session-store mode with cross-writer coordination.

The session-data wipe affects only records in `WALTR_SESSION_STORE`. It does not modify either Waltr or MCP SQL data, authentication sessions, OAuth credentials, or configuration. The operation is a blind delete without active-run or cross-replica coordination; stop session activity before invoking it or concurrent work may recreate records.

## Resource Catalog

The server embeds one YAML catalog under [`crates/waltr-server/resources`](../crates/waltr-server/resources/). Persisted overrides live under `.waltr/resources` at the server root.

The catalog uses `agents`, `features`, `permissions`, `providers`, `mcp-servers`, `defaults`, and `routines`. Only `defaults/default.yaml` is valid in the defaults category.

Each persisted file contains one strict YAML document. Its category, `kind`, filename stem, and `metadata.name` must identify the same resource.

Bundled resources supply defaults. A persisted exact-path file replaces its bundle. Reset removes the persisted override and reveals the bundle.

The server always registers bundled `routines/title.yaml` and `routines/fork.yaml`. A persisted same-path file overrides either resource, and reset restores its bundled definition.

Waltr validates the complete catalog before startup. Invalid YAML, identity mismatches, unresolved references, invalid models.dev selectors, and invalid environment templates fail startup.

Models.dev loading is network-first with fixed request bounds. A valid configured disk cache is the first fallback and is reported stale. Embedded hosts may explicitly supply the curated seed in `resources/models-dev-defaults.json` as a final stale fallback only when no cache file exists. Malformed or unreadable cache files fail closed, and standalone startup does not enable the built-in fallback.

The environment-owned bundled self-MCP resource requires `WALTR_PUBLIC_URL` in every access-auth mode. It consumes the variable independently of OIDC, joins `/mcp` through URL parsing, and preserves a deployment path prefix. With access auth disabled, actual partial browser, API, or hosted-MCP OIDC settings still fail startup.

The server watches `.waltr/resources` after startup. An invalid reload preserves the last-known-good snapshot and exposes the reload error.

Routine resources use strict YAML and stay outside durable agent-definition bundles. The Routine service exposes definitions and durable invocation history globally.

Public `/mcp` resources and uniform tools provide resource administration:

| Resource Or Tool/Action | Behavior |
| --- | --- |
| `waltr://resources` | Lists resource slots with canonical item URIs. |
| `waltr://resources/{path}` | Reads metadata and exact persisted, effective, and active sources. |
| `waltr://resources/{path}/source` | Reads effective YAML as `application/yaml`. |
| `create/resource.create` | Creates an absent persisted slot from complete strict YAML. |
| `edit` (direct arguments) | Applies guarded, ordered text edits through one atomic replacement. |
| `destroy/resource.destroy` | Removes one persisted override and reveals any bundled fallback. |

Mutations require the current `expected_revision` from item metadata.
Persisted revisions equal persisted-slot ETags; absent revisions also guard the effective source hash.
All mutations validate the complete candidate catalog before filesystem mutation.
See the embedded [resource contract](../crates/waltr-server/resources/skills/resource-administration/references/resources.md) for URI encoding and text edit inputs.

Hosted sessions use `waltr://sessions` and `waltr://sessions/{id}` for ordinary reads.
Tool `create` exposes `session.create`; tool `execute` exposes `session.send` and `session.interrupt`.
Tool `query` exposes bounded `session.wait`; tool `destroy` exposes `session.wipe` with `confirm: true` for ALL sessions.
The old `exec` and `sessions` tools have no compatibility aliases.

Embedded startup prebinds loopback and atomically overwrites app config `server/.waltr/resources/mcp-servers/waltr.yaml` with the generation URL before catalog load. This is an ordinary persisted resource with filesystem origin; the same inspection, replace, watcher, reset, and ETag behavior applies. Reset can fail candidate validation when the bundled fallback needs `WALTR_PUBLIC_URL`, without any embedded-specific reset rule.

`waltr.resource.v1.ResourceService` provides equivalent gRPC reads, watches, inspection, replace, and reset operations. Reads and watch events contain complete catalog snapshots.

Provider resources can materialize models.dev selectors with optional `keyPrefix` and aliases. MCP resources select `streamableHttp` or `stdio`; stdio resources materialize and use ordinary runtime semantics in standalone and embedded builds.

`defaults/default.yaml` stores inference defaults. Agent definitions own MCP selection.

Provider and MCP credentials remain in Waltr SQL. They never enter resource source, catalog snapshots, or resource reset behavior.

New sessions snapshot provider inference settings. They also store exact agent-definition source texts. Later resource changes do not rewrite those durable values.

After the first durable user prompt, the bundled title routine starts best-effort work. It has no enable setting, backfill, replay, recovery, or retry mechanism.

The routine creates a visible durable auxiliary session. It copies only provider connection and model settings, disables reasoning, and runs one provider turn. It uses replacement prompts with empty feature, tool, and permission lists.

The title task uses an inference-only template. Its durable helper starts with only the task prompt and receives no source history.

The routine writes the exact final text to source metadata key `title` only when that key is absent. Its declared ephemeral metadata condition excludes auxiliary recursion.

Event delivery and duplicate-attempt suppression before admission are process-local and bounded. Admitted event and manual invocations use one durable lifecycle recorder.

Graceful shutdown stops admission, interrupts owned auxiliary children, and records unfinished work as interrupted. Startup records leftover work with `server_restart`.

Fork compaction depends on one provider call. Caller or server failure can interrupt that call. Model output remains nondeterministic.

The source-backed compact helper atomically stores rollback-aware provider-neutral inference history through the latest completed turn, followed by its task prompt. The history persists across restart and storage backends under ordinary session retention rules. Normal session delete and wipe operations apply.

The durable helper history preserves provider-replay prompts, assistant text, tool calls and results, and inline images. It excludes lifecycle, middleware, status, usage and cost, request audit, provider metadata, provider-native thinking payloads, rollback barriers, and active partial work. Provider adapters apply their normal encoding rules. The history is not an HTTP wire archive.

Fork atomically persists the ordinary destination and its exact first prompt through `SessionCreateRequest`. Successful creation is the prompt-durability boundary. The first durable prompt makes the destination title-eligible.

After successful atomic creation, caller cancellation or start failure retains the destination and its ID.

Fork copies exact source inference and agent definition but clears `pending_switch_audit`. The ordinary destination copies no source history or general metadata. Destination metadata contains only `waltr.session.fork.source`.

Provider and upstream-MCP credentials remain host-resolved. Fork adds no database migration or new durable credential state.

See [Routines](architecture/routines.md) for the canonical lifecycle and replay contract. See [Resource Catalog](architecture/config.md) for resource syntax.

## OIDC Access Domains

Production OIDC deployment requires:

```text
WALTR_AUTH_MODE=oidc
WALTR_PUBLIC_URL=https://waltr.example
WALTR_OIDC_ISSUER=https://identity.example/issuer
WALTR_OIDC_CLIENT_ID=waltr-browser
WALTR_OIDC_CLIENT_SECRET=<provider-dependent secret>
WALTR_OIDC_REDIRECT_URI=https://waltr.example/oidc/callback
WALTR_OIDC_SCOPES=openid profile
WALTR_API_OIDC_ISSUER=https://identity.example/application/o/waltr-native/
WALTR_API_OIDC_AUDIENCE=waltr-api
WALTR_API_OIDC_CLIENT_ID=waltr-native
WALTR_API_REQUIRED_SCOPES=waltr:api
WALTR_MCP_REQUIRED_SCOPES=mcp:use
WALTR_MCP_OAUTH_ISSUER=https://waltr.example/oauth
WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE=/var/run/secrets/waltr/mcp-oauth/keyring.json
WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL=300
WALTR_MCP_OAUTH_REFRESH_TOKEN_TTL=86400
WALTR_MCP_OAUTH_REFRESH_FAMILY_TTL=2592000
WALTR_MCP_OAUTH_CODE_TTL=300
WALTR_MCP_OAUTH_ALLOW_DCR=true
WALTR_MCP_OAUTH_ALLOW_LOOPBACK_REDIRECTS=true
WALTR_MCP_OAUTH_ALLOW_CIMD=true
WALTR_TRUSTED_PROXIES=10.0.0.0/8,2001:db8::/32
```

The browser, native Waltr API, and inbound MCP endpoint are separate authorization domains:

| Domain | Issuer | Audience | Required scopes |
| --- | --- | --- | --- |
| Browser login | `WALTR_OIDC_ISSUER` | ID-token audience `WALTR_OIDC_CLIENT_ID` | Browser request `WALTR_OIDC_SCOPES` |
| Native Waltr API bearer | `WALTR_API_OIDC_ISSUER`, falling back to `WALTR_OIDC_ISSUER` | `WALTR_API_OIDC_AUDIENCE`, falling back to legacy `WALTR_OIDC_AUDIENCE`; generic `JWT` also requires `azp` equal to `WALTR_API_OIDC_CLIENT_ID` | `WALTR_API_REQUIRED_SCOPES`, empty when omitted |
| Inbound MCP bearer | Local `WALTR_MCP_OAUTH_ISSUER` | Exact public `/mcp` resource | `WALTR_MCP_REQUIRED_SCOPES` |

The hosted issuer is enabled only in OIDC mode. `WALTR_MCP_REQUIRED_SCOPES` and `WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE` are required. The issuer defaults to the exact `WALTR_PUBLIC_URL` path plus `/oauth`; an override must equal that same HTTPS deployment-relative URL. Token/code lifetime bounds and defaults are documented in [Access Authentication](architecture/access-authentication.md). DCR, native HTTP loopback redirects, and CIMD default off and must be enabled independently. Enabling loopback redirects permits public clients resolved through either DCR or CIMD to use them.

Private OAuth destinations are denied by default. Two optional, comma-separated allowlists admit RFC 1918 IPv4 and IPv6 unique-local addresses at exact HTTPS origins:

| Setting | Direction |
| --- | --- |
| `WALTR_MCP_OAUTH_CIMD_TRUSTED_PRIVATE_ORIGINS` | Client metadata documents fetched by Waltr's hosted authorization server. |
| `WALTR_MCP_OUTBOUND_OAUTH_TRUSTED_PRIVATE_ORIGINS` | MCP discovery, registration, exchange, revocation, and refresh performed by Waltr's outbound MCP client. |

Each entry must be only `https://host[:port]`, without credentials, a path, query, or fragment. Matching includes the effective port, duplicate origins are rejected, and the lists do not grant trust across directions. Waltr automatically trusts its initialized deployment origin for hosted self-CIMD. Server-owned outbound authorization gates automatic trust to the exact initialized self-MCP resource, and the server supplies that origin to agent session refresh. The Pulumi settings `mcpOAuthCimdTrustedPrivateOrigins` and `mcpOutboundOAuthTrustedPrivateOrigins` project nonempty lists into the corresponding environment variables.

`WALTR_PUBLIC_URL` is the externally reachable Waltr API resource identifier and may include a path prefix. Environment-owned servers require it in every auth mode because the bundled self-MCP resource consumes it independently. It must use HTTPS except for loopback development and is not inferred from the browser callback. Disabling access auth permits this variable but still rejects actual partial browser, API, or hosted-MCP OIDC settings. `WALTR_OIDC_AUDIENCE` is retained only as the API-audience fallback; new deployments should set `WALTR_API_OIDC_AUDIENCE` explicitly. `WALTR_API_OIDC_CLIENT_ID` enables provider-generic `JWT` access tokens by requiring their `azp` claim to exactly match the configured native client; RFC 9068 `at+jwt` access tokens remain supported without that claim.

`WALTR_OIDC_CLIENT_SECRET` is optional only when browser-provider metadata supports token endpoint auth method `none`. `WALTR_AUTH_SESSION_TTL` optionally sets browser-session seconds and defaults to eight hours. `WALTR_TRUSTED_PROXIES` is optional; when set it is a comma-separated CIDR allowlist for trusting forwarded client addresses. Omit it unless a known reverse proxy overwrites forwarding headers.

Register `WALTR_OIDC_REDIRECT_URI` exactly with the browser provider. It must use the exact `/oidc/callback` path and have no query or fragment. Browser ID tokens are validated against `WALTR_OIDC_CLIENT_ID`, native API tokens remain Authentik-issued, and MCP access tokens are issued and validated locally by Waltr.

Terminate TLS at Waltr or a reverse proxy and preserve the public origin. `WALTR_PUBLIC_URL` is the sole authority for advertised URLs; Waltr does not infer issuer or resource URLs from forwarding headers. Route `/oidc/*`, `/oauth/*`, `/outbound-oauth/callback`, `/.well-known/oauth-protected-resource`, `/.well-known/oauth-protected-resource/mcp`, `/.well-known/oauth-authorization-server/oauth`, `/.well-known/oauth-client/waltr`, `/mcp`, gRPC-web, web assets, and SPA paths without rewriting. `/outbound-oauth/callback` completes only outbound authorization explicitly started from Config; a runtime unauthenticated response does not start an attempt. The API-root RFC 9728 document remains the native API/Authentik domain. MCP metadata advertises only `WALTR_PUBLIC_URL` plus `/oauth` and the exact public `/mcp` resource.

Waltr has no browser MCP client. `/mcp` therefore keeps an empty Origin allowlist: requests without `Origin` are accepted, while every browser-originated request is denied.

OIDC discovery, authorization, token, and JWKS endpoints must use HTTPS. For loopback-only development, HTTP is accepted only when both settings are explicit:

```text
WALTR_AUTH_ALLOW_INSECURE_COOKIE=true
WALTR_OIDC_ALLOW_INSECURE_HTTP=true
```

The callback and every HTTP OIDC endpoint must then be loopback. These flags are not a reverse-proxy workaround and must not be used for non-loopback deployments.

## Native Client Registration

Register a separate public native client for the TUI and Tauri remote profiles. It must allow Authorization Code with PKCE S256 and each host's callback policy. The shared native protocol sends `client_id` and PKCE without a client secret or HTTP Basic authentication. Discovery can advertise only confidential token methods because some providers publish endpoint-wide capabilities that omit `none`. Do not create an app secret or reuse the browser secret.

Tauri remote profiles use this public native registration through URL-only bootstrap. Desktop requires ephemeral loopback callbacks; Android requires the exact `dev.waltr.app:/oauth2redirect` callback. Tauri Local instead uses its process-generation local bearer and no access OAuth.

For URL-only bootstrap, set server-side `WALTR_API_OIDC_CLIENT_ID` to the public native client identifier. The API-root protected-resource document then advertises the optional `waltr_native_client` extension with:

- `client_id` from `WALTR_API_OIDC_CLIENT_ID`;
- deduplicated `openid`, `offline_access`, and `WALTR_API_REQUIRED_SCOPES`;
- `audience` from `WALTR_API_OIDC_AUDIENCE` or its legacy fallback.

The shared client helper accepts the endpoint and host callback policy, validates discovery, and returns public configuration plus the exact issuer. This Waltr application extension provides neither universal OIDC client discovery nor DCR. It exposes no browser client secret. Older authenticated servers need an update or explicit native-client configuration. See [URL-only native bootstrap](architecture/access-authentication.md#url-only-native-bootstrap) for the contract.

The TUI retains explicit environment configuration; it does not use this helper. Configure the TUI process with:

```text
WALTR_SERVER_ENDPOINT=https://waltr.example
WALTR_OIDC_NATIVE_CLIENT_ID=waltr-native
WALTR_OIDC_NATIVE_SCOPES="openid offline_access waltr:api"
WALTR_OIDC_NATIVE_AUDIENCE=waltr-api
WALTR_OIDC_NATIVE_LOOPBACK_POLICY=ephemeral
```

Only the native client ID is required at login. The TUI discovers only the API-root metadata at `/.well-known/oauth-protected-resource`, verifies its resource exactly matches `WALTR_SERVER_ENDPOINT`, sends that API resource in the authorization request, and never derives or discovers `/mcp`. Omit scopes to request `openid` plus the API metadata scopes. `WALTR_OIDC_NATIVE_AUDIENCE` is an optional provider-specific authorization parameter; it does not replace the resource parameter or server-side API audience validation. The loopback policy defaults to `ephemeral`; a fixed value must be an exact `http://127.0.0.1:<port>` URL registered with the provider.

Native tokens and refresh tokens are process-memory only. Restarting the TUI requires login again. The TUI refreshes before expiry when a refresh token is returned and returns to login-required state when refresh fails or no refresh token remains after expiry.

## Client Audio Host Prerequisites

The native app composes shared capture and shortcut libraries through its Rust bridge, separate from upstream speech providers.
See [Clients](architecture/clients.md#routine-interaction-clients) for ownership and [desktop configuration](architecture/config.md#desktop-client-configuration) for startup YAML.
The [example client file](examples/client.yaml) does not install its server Routine or providers.
A compatible server must expose `RoutineInteractionService` and the exact configured PCM ports.

| Platform | Prerequisites and limits |
| --- | --- |
| macOS | CPAL 0.16; `global-hotkey` 0.9 requires a main-thread registrar and an active host run loop. |
| Wayland Linux | CPAL 0.15.3 requires ALSA development files and pkg-config; PipeWire requires ALSA compatibility. GlobalShortcuts requires a compatible portal and compositor. |
| Android | CPAL 0.15.3 uses Oboe 0.6 and Android API 24. The application must verify NDK/C++ linkage and package compatibility. |
| Hosted browser | Secure context, AudioWorklet support, microphone permission, and explicit user activation. Serve the static worklet with the browser assets. |

The native converter pins rubato 0.16.2 without default features.
Android requests explicit 48 kHz mono I16 source capture on the default device, with no Java-context discovery or application fallback.
The host owns `RECORD_AUDIO` permission and foreground policy; library compilation proves neither permission delivery nor device capture.
Native capture startup and teardown require host workers. OS driver calls have no portable hard deadline.
Wayland portal approval can alter or reject requested shortcuts; the host must expose actual effective triggers.
No X11 or media-key interception fallback exists.
Secure Linux credential-store unavailability does not authorize plaintext credential storage.

Native app dictation uses the current Rust transport and a shared chat/hold audio gate.
Apple Silicon macOS includes audio-only AVFoundation permission support, `NSMicrophoneUsageDescription`, and the hardened-runtime `audio-input` entitlement.
Intel macOS reports dictation unavailable; this implementation adds no Intel package promise.
Android includes `RECORD_AUDIO` permission and a dedicated launcher; it adds no microphone foreground service.
Android pause cancels audio even when the remote auth runtime survives. Resume never restarts audio automatically.
Desktop startup loads the strict XDG client file, including on macOS; Android chat loads no XDG client configuration.
Source and host tests establish no signed permission prompt, installed-device capture, Linux portal acceptance, or Android NDK/C++ package compatibility.
Browser capture uses bounded complete uploads, not native duplex capture or mobile XDG configuration.
Browser resampling uses linear interpolation without an anti-alias filter; native conversion uses a stateful sinc filter.
Hardware, portal, packaged-app, and browser acceptance remain separate [test gates](testing.md#client-audio-and-dictation).

### Desktop Tray And External Text

The desktop tray reports only [client-owned Routine activity](architecture/clients.md#client-routine-activity-tray), not server-wide work.
Its menu contains status, Open Waltr, and Quit; normal window-close and exit behavior do not change.
Tray creation is optional and nonfatal.
macOS requires main-thread tray ownership and uses template icons.
Android supplies neither a tray nor the desktop external-text action.

The pinned Tauri 2.10.1 and `tray-icon` 0.21.3 Linux path uses GTK/AppIndicator, not a native `ksni` backend.
At runtime, it loads `libayatana-appindicator3.so.1` or `libappindicator3.so.1` and requires a compatible panel host.
GNOME generally requires an AppIndicator extension; KDE requires a compatible panel configuration.
A headless session or absent panel host provides no tray acceptance evidence.
Wayland alone does not guarantee tray support.
Use the Open Waltr menu item; Linux icon-click behavior supplies no portable window-open contract.

`typing: {}` requires an authorized native backend; schema acceptance and fake-backend tests do not establish installed-platform support.
The core returns `Unsupported` when the host supplies no backend.
The following requirements define platform acceptance, not a compile or live-delivery claim.

| Platform | External-text requirements and limits |
| --- | --- |
| macOS | CoreGraphics Unicode events require Accessibility trust; trust checks must not trigger an automatic permission prompt. |
| Wayland Linux | RemoteDesktop portal keyboard consent, a ready/resumed EI device, and EI text capability are mandatory. |
| Android | No desktop external-text action. Native chat dictation remains separate. |

If macOS reports no trust, grant Waltr access manually under System Settings → Privacy & Security → Accessibility.
CoreGraphics event posts provide best-effort delivery; applications can ignore Unicode events or interpret keycodes differently.
The action provides no held-modifier release or exact insertion guarantee.
Live hypotheses arrive before provider commitment; macOS replaces revised suffixes with unmodified physical Backspace key pairs.
The host counts extended graphemes, but target applications need not delete exactly one grapheme per Backspace pair.
A focus or cursor change can redirect deletions away from this hold's prior text.

Wayland backend acceptance requires `ashpd` RemoteDesktop portal consent and `reis` EI text delivery.
Native EI text batches permit at most 254 UTF-8 bytes.
Wayland supports append-only live output; revisions return `Unsupported` before any deletion or replacement.
The EI text interface exposes keysyms but no backward-text deletion API; the backend adds no keysym fallback.
Absent EI text capability must return `Unsupported`; no X11, keysym, clipboard, or `uinput` fallback exists.
The portal session belongs to one hold and retains no persistent consent; the portal can request consent again on later holds.
The portal parent window identifies the consent request, not an injection target.
Neither backend pins keyboard focus; focus changes can redirect later batches.
No backend adds Enter, Tab, paste, shortcuts, or automatic submission.

Compile checks cannot prove Accessibility delivery, portal negotiation, panel behavior, or application insertion.
Perform live checks only with explicit authority for OS consent, input effects, capture, and any server traffic.
See [platform acceptance gates](testing.md#platform-acceptance-gates) for the separate checks.

## Client Observability

Production web bundles initialize Grafana Faro at `https://faro.holdenitdown.net/collect`. Hosted browser bundles use application name `waltr-spa`; Tauri WebView release bundles use `waltr-tauri`. Both report the version from `crates/waltr-web/package.json` and use the fixed `production` environment. Vite development, Tauri development, and test builds do not initialize Faro. Tauri development uses the explicit `tauri-dev` asset mode because a normal `vite build` is production-form even when launched by `cargo tauri dev`. A locally served release bundle still has release behavior and attempts Faro export. This WebView gate does not control native Rust telemetry.

Faro uses the standard web instrumentations and default tracing instrumentation without a global `beforeSend` sanitizer. It exports normal logs, exceptions, session, user-action, navigation, performance, resource, and trace data. This personal-use telemetry can include URLs, identifiers, exception text, stacks, console output, and browser or session metadata. Explicit app view reports still normalize known route identifiers. Repeated application errors remain separate events.

The Tauri CSP permits `connect-src` to exactly `https://faro.holdenitdown.net` in addition to the local IPC origins. It grants no other telemetry origin. Tauri configures Faro with `ignoreUrls: [/.*/]`, which excludes adapter-private loopback requests and request trace propagation. Rust keeps the private endpoint and bearer outside WebView state and telemetry. The collector must provide valid public HTTPS and accept Faro browser requests from each deployed hosted origin and the packaged Tauri WebView. Collector DNS, TLS, CORS, retention, and availability remain operator concerns; Waltr has no runtime Faro endpoint override. Faro failure never blocks application startup or use.

Every non-test native Rust app build exports traces, logs, and metrics as OTLP/HTTP protobuf. This includes local and debug app builds. Automated Rust tests disable native telemetry initialization. OpenTelemetry 0.32 requires programmatic endpoints for each signal, so Waltr fixes these URLs:

- Traces: `https://telemetry.holdenitdown.net:4318/v1/traces`
- Logs: `https://telemetry.holdenitdown.net:4318/v1/logs`
- Metrics: `https://telemetry.holdenitdown.net:4318/v1/metrics`

Native telemetry uses standard service-style OpenTelemetry SDK providers: a batched span exporter, a batched log exporter bridged from tracing, and a periodic metric exporter. Trace export accepts INFO+ data from the complete `waltr_app_lib` module tree plus `waltr_client::transport::native_trace`. Log export accepts INFO+ events from the complete `waltr_app_lib` module tree. All providers use service name `waltr-app-native`. Their shared resource includes the Cargo package version, sanitized app revision, and Rust target triple. Startup emits the `app.startup` log and increments `waltr.app.startups` with fixed lifecycle context. The native client enables `waltr-client`'s `native-tracing` feature, which adds bounded gRPC spans and W3C context propagation to native Waltr RPCs. Tauri Faro does not also propagate those RPCs.

`WALTR_APP_REVISION` can set the native build revision. The build accepts 7 through 40 ASCII hexadecimal characters and normalizes them to lowercase. If the value is absent or invalid, `build.rs` tries the current Git `HEAD` under the same rules. If neither source yields a valid revision, telemetry uses `unknown`. Release automation should pass the verified full commit SHA when its build environment cannot expose `.git` reliably.

Native provider setup and export remain fail-open. Initialization failure emits the fixed stderr line `waltr native telemetry initialization failed; continuing without telemetry`. All three exporters share a blocking Rustls client that disables redirects. The client strips inherited or environment-derived request headers and retains only the canonical OTLP protobuf content type. Trace and log shutdown requests use a two-second bound; OpenTelemetry SDK 0.32 metric shutdown uses its fixed five-second bound. Subscriber, TLS, exporter, request, or shutdown failures do not block app startup or use. Operators must not treat application success as proof of telemetry delivery. Local and debug app runs attempt export, but deterministic Rust tests use in-memory exporters and do not contact collectors. Android lifecycle suspension remains nonblocking and does not synchronously flush. Abrupt process termination can discard queued telemetry. Native panic events exclude panic payload text and retain fixed panic metadata.

Native Waltr access and native OTLP use rustls with the ring provider, safe TLS 1.2 or 1.3, and bundled `webpki_roots`. On Android, the same explicit TLS and root policy covers server-owned models.dev catalog refresh and provider and upstream-MCP OAuth discovery, registration, exchange, and revocation; agent provider and MCP OAuth refresh; OpenAI and Anthropic HTTP inference; and Waltr-agent MCP Streamable HTTP through a caller-supplied generic MCP reqwest client. OpenAI websocket already uses explicit ring and bundled roots on every platform. These Android clients retain certificate-chain and hostname checks but exclude Android platform, user, and private roots and `SSL_CERT_FILE`. Non-Android paths retain default/platform reqwest behavior where they previously used it. Other Android networking paths continue to use platform verification initialized by the foreground service. Tauri remote profiles use native HTTPS; only Local uses its host-created HTTP loopback endpoint. Hosted browser and WebView Faro trust remain platform-owned.

Client telemetry follows the boundary in [Clients](architecture/clients.md#client-observability-boundary). Standard Faro data is high-detail personal-use telemetry, not a globally sanitized stream.

## Server Observability

The standalone server writes JSON logs and defaults to global `info` for every target. `RUST_LOG` overrides that default through `EnvFilter` syntax.

Set `WALTR_OTEL_EXPORT=1` or `WALTR_OTEL_EXPORT=true` to enable OTLP export. `OTEL_EXPORTER_OTLP_ENDPOINT` also enables export when present. Standard OpenTelemetry environment variables configure trace, log, and metric exporters. The service name is `waltr-server`.

Pyroscope remains available only in a standalone server build with the `profiling` feature. Set `WALTR_PYROSCOPE_URL` to start it with application name `waltr-server`. Embedded Tauri servers do not include Pyroscope.

Use [Telemetry Smoke Checks](runbooks/telemetry-smoke.md) after a build and runtime satisfy the signal prerequisites. Source changes alone do not establish deployment or ingestion.

## Tauri Application

`crates/waltr-app/` always loads packaged local `crates/waltr-web/` assets and runs the shared client in Rust. The picker selects Local or a saved remote profile. First launch requires a choice; later launches reconnect the last server that reached an authenticated shared snapshot.

App-data `servers.json` stores only nonsecret profiles and the last successful ID. Rust requires HTTPS except for exact loopback hosts and rejects URL credentials, queries, and fragments. Profile saves do not connect automatically. Switch server disposes the selected client and clears server-specific routes and drafts. Retry connection remounts the same selection.

Local starts the embedded `waltr-server` lazily on an ephemeral IPv4 loopback endpoint. Switching away retains an active local server and its state. Remote selection starts no embedded server or Android foreground service and requests no notification permission.

Each server generation receives a random 256-bit local bearer. Rust keeps the endpoint and bearer out of IPC, WebView state, logs, and telemetry.

After prebinding that endpoint, startup atomically writes its `/mcp` URL to app config `server/.waltr/resources/mcp-servers/waltr.yaml`, then loads the ordinary bundled-plus-persisted catalog. The endpoint is therefore durable on disk and refreshed each generation; only the bearer remains memory-only.

Rust removes the exact endpoint and bearer from every Tauri snapshot before publication. Shared client state retains its original values inside Rust.

If an outbound authorization URL contains the local endpoint, Rust also removes that URL from the snapshot. WebView opens it through the current opaque attempt ID.

The embedded server derives `http://127.0.0.1:<port>/outbound-oauth/callback` from its prebound origin. It passes this callback through all outbound MCP OAuth stages.

Registration, completion-mode selection, and callback completion use the same exact URL. The app requires no `WALTR_PUBLIC_URL` or per-target OAuth redirect variable.

Embedded mode publishes no CIMD client metadata URI. Its local metadata endpoint returns `404 Not Found` because remote authorization servers cannot fetch device loopback.

If upstream advertises CIMD and DCR, embedded mode skips CIMD. It can use preregistration or DCR. Reauthorization in the same server generation reuses a valid DCR registration; a replacement generation performs fresh DCR because the previous generation's registration marker is stale.

Embedded composition ignores `WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS`. Only its explicit trusted-origin list can admit private HTTPS OAuth destinations.

Embedded attempts and MCP credentials persist only an opaque per-generation callback marker. They never persist the loopback callback endpoint.

Every embedded server restart rejects stale markers and credentials. Status and cancel return not found for stale attempts, and self-MCP credential logout returns failed precondition because that credential is host-owned. Affected outbound MCP targets require reauthorization and cannot reuse generation-specific DCR registration.

Remote profiles use [URL-only native bootstrap](architecture/access-authentication.md#url-only-native-bootstrap) and public-client PKCE. The server must advertise compatible native client metadata for sign-in. Local exposes no user access operations. Remote sign-in opens the system browser from Rust; remote sign-out clears access and all secure credentials for that profile.

macOS stores refresh tokens in Keychain. Android uses AndroidKeyStore AES-256-GCM keys and atomic encrypted files under `noBackupFilesDir`. Other desktop targets return secure-storage unavailable; no plaintext fallback exists. Access tokens remain in memory. Switching or runtime disposal retains refresh credentials; profile removal deletes all credential identities before its registry entry. See [Tauri native host adapter](architecture/access-authentication.md#tauri-native-host-adapter) for namespace, refresh, deletion, and callback contracts.

Android warm `onNewIntent` callbacks target the current remote runtime. Cold-start intents do not recover an OAuth attempt; process death requires a new login. Routes follow authoritative shared access and readiness snapshots. The web adapter ignores native access progress for presentation. Provider and upstream-MCP OAuth remain server-owned.

The main WebView loads only packaged local assets. Remote navigation and new windows remain denied. Rust validates credential-free HTTPS URLs before the operating system opens them.

Normal Apple Silicon macOS builds enable Apps without `native-apps-test`, a disposable profile, or a prior diagnostic report.
Target-specific Cargo dependencies select host assets, served assets, and the WKWebView adapter.
Rust owns a dedicated trusted loopback asset listener per client runtime, independent of the selected API server.
Local and remote profiles use the same local Apps assets; the listener serves no API and receives no API bearer.
Switch or disposal detaches its WK navigation policy and shuts down its listener.
The base `tauri.conf.json` retains `frame-src 'none'`; no automatic macOS configuration relaxes that policy.
At startup, `app_container.rs` applies compile-target-aware CSP through `context.config_mut()` before the Tauri builder consumes the context.
Only macOS/aarch64 permits `http://127.0.0.1:*/container.html`; other targets, including Intel macOS, retain `frame-src 'none'`.
The function changes only `frame-src` in configured normal and development CSP; optional diagnostics also permit the probe frame URL.
Rust further restricts subframe navigation to exact current-generation ports and asset paths.
Empty `about:blank` and `about:srcdoc` frames require a packaged-page or trusted current-generation asset source.
The `start_runtime` response exposes `nativeApps.containerUrl` only after the WKNavigationDelegate installation acknowledges success.
That acknowledgement enforces runtime security; it is not a release-validation gate.
The private Local API endpoint and bearer remain Rust-only. Every application IPC command retains its packaged-top-frame boundary.
Other native platforms remain unsupported; no Intel or universal package support follows from this target.

The optional `native-apps-test` diagnostic feature requires `tauri.native-apps-test.conf.json` and identifier `dev.waltr.app.native-apps-test`.
Its separate identifier selects disposable app-private roots and adds the attack-fixture listener and `nativeAppsTest` helper.
Normal Apps require neither the helper nor its evidence pipeline.
See [macOS Apps verification](testing.md#macos-native-apps-verification) for normal-app live acceptance and optional sanitized diagnostics.
That procedure supplies no runtime success claim or commit, push, or deployment authority.

The embedded server uses explicit Tauri app-private roots:

| State | Location under Tauri roots |
| --- | --- |
| Waltr database | App data `server/waltr.sqlite3` |
| MCP database | App data `server/mcp.sqlite3` |
| Sessions | App data `server/sessions/` |
| Workspace | App data `workspace/` |
| models.dev cache | App data `server/models/api.json` |
| Resource overrides | App config `server/.waltr/resources` |

Startup fetches `https://models.dev/api.json` before it reads the app cache fallback. HTTP connect time has a five-second bound.

The complete request and response body have a 15-second bound. Failure or timeout falls back to a valid app cache.

When the app cache is absent, embedded startup uses its explicit curated built-in seed as a final stale fallback.
A malformed or unreadable cache still fails startup; it does not select the built-in seed.

Desktop and Android use the same ordinary bundled and persisted resource catalog. Both require the process-local workspace worker at startup. The embedded server runs the private Tonic service and worker in-process over `tokio::io::duplex`; it does not bind a worker socket or expose a public worker route. Workspace tools and context I/O have no host-side fallback. This separation does not provide operating-system isolation; desktop Bash commands have the app process's OS authority. See [agent architecture](architecture/agent.md#private-workspace-boundary).

Workspace path preflight checks known configured host-owned paths against the worker root (including canonicalized existing parents of missing files). It does not recursively scan the workspace; ordinary repository files can remain in the workspace.

Android is compiled without shell support. Shell-bearing definitions remain valid and inspectable, but runtime construction omits the shell tool and feature-derived shell tool permissions. Stdio MCP resources materialize and use the ordinary runtime path.

These server-side capabilities differ from WebView capabilities. The WebView keeps its minimal local IPC grants on every platform.

Run these commands from the repository root. They map to current workspace scripts and manifests:

| Target | Command | Purpose |
| --- | --- | --- |
| Tauri web | `pnpm build:tauri` | Generate shared WASM prerequisites, type-check, and build relative Tauri assets with no WASM output. |
| Tauri development web | `pnpm --filter @rfhold/waltr-web build:tauri:dev` | Generate shared WASM prerequisites and build relative Tauri assets with no WASM output and real Faro export disabled. |
| Linux desktop | `pnpm --filter @rfhold/waltr-app dev` | Build the Tauri web assets and start the desktop app. |
| Linux packages | `pnpm --filter @rfhold/waltr-app build` | Build the configured Linux Tauri bundles. |
| Android package | `pnpm --filter @rfhold/waltr-app android:build` | Build through the generated Android project under `crates/waltr-app/gen/android/`. |
| Android app | `pnpm --filter @rfhold/waltr-app exec cargo tauri android dev` | Build, install, and start on a configured emulator or device. |

Android launcher assets derive from `crates/waltr-web/public/favicon.svg`. Run `crates/waltr-app/scripts/android-launcher-icons.sh` after changing that artwork, and run the same command with `--check` to verify the committed legacy and adaptive resources. The launcher assets are separate from the monochrome foreground-service notification icon.

Focused deterministic checks are listed in [Testing](testing.md). Package creation does not prove installed-device lifecycle behavior.

### Android Foreground Service

Local selection requires notification permission before embedded-server startup on Android 13 and later. Denial leaves local startup at `notificationPermissionRequired`. Remote mode does not require that permission.

`WaltrForegroundService` is non-exported and runs in the app process. It uses foreground-service type `specialUse`, a persistent notification, and `android:stopWithTask="false"`.

Ordinary service startup returns `START_STICKY`. The notification exposes a Stop server action. That action requests the current nonblocking native server stop, removes the foreground notification, stops the service, and returns `START_NOT_STICKY` for that command.

The service enters foreground state before it loads the native library. Native server work starts only after both steps complete.

The current `specialUse` declaration targets sideload distribution. Play distribution requires separate policy review, declarations, and product work.

In Local mode, activity pause stops only the client runtime. Local resume creates a fresh client generation. The service and embedded server survive background and task removal on a best-effort basis. Remote mode preserves the client across activity pause, including an active native login attempt.

The Stop action persists no disabled setting. A later Local selection or Local activity resume can start a fresh embedded server generation after the prior generation stops. Remote resume never requests a local server.

Android Local lifecycle callbacks signal client stop and enqueue retirement without a thread join. Each bridge owns one managed background reaper that joins retired clients and automatically removes completed records. IPC disposal awaits tracked completion barriers off-callback under `selection.operation`; IPC replacement waits for those barriers before a new runtime starts. Server-controller reaper threads retain their separate exact-generation server shutdown contract.

A nonblocking server stop transitions the exact current generation to `Stopping` and detaches its thread under the controller lock. Its reaper joins only that generation and publishes `Stopped` or `Failed` after the join.

New start reservation waits while the controller is `Stopping`, so a replacement starts only after the old reaper completes. Embedded generations never overlap their shared app-private roots, and Android callbacks remain nonblocking because they do not perform the join.

Notification permission changes publish an adapter-private startup control event to an active client generation. A later client generation reads the current permission state at startup.

A fresh Local generation applies permission state before it can use the current server connection. Permission-required Local generations remain disconnected until permission changes. Remote generations bypass the local notification gate.

Each client-start ticket owns the exact bridge generation that it creates. Suspend invalidates in-progress ownership, and resume creates a replacement ticket.

A stale or suspended completion stops only its own generation. It never stops a newer replacement or leaves an unowned background client.

Foreground-service APIs do not guarantee survival through force-stop, process kill, reboot, or user termination. Android can terminate the process at any time.

After process death, a later local service start creates a new endpoint and bearer. Durable stores reopen, and Routine restart reconciliation handles unfinished invocations.

Android excludes all app-private files, databases, shared preferences, and root data from cloud backup and device transfer.

## Authentik And Pulumi Deployment

The repository's supported infrastructure/workload definition is Pulumi under `infra/pulumi/`; there is no Kustomize deployment. Each `preview` and `prod` stack is defined to provision the Kubernetes workload, PostgreSQL, S3-compatible session/backup bucket, and two Authentik providers:

| Provider | Type | Use |
| --- | --- | --- |
| Browser | Confidential | Server-side browser Authorization Code flow and `/oidc/callback`. |
| Native API | Public | Remote native clients such as the TUI; issues the API audience and scope. |
Authentik has no MCP provider, application, audience, scope mapping, callback, or token-issuance role. Waltr hosts MCP authorization, supports DCR and CIMD according to explicit stack policy, and retains `WALTR_MCP_REQUIRED_SCOPES` as local policy.

Pulumi keeps one Waltr replica with one workspace worker sidecar in the same pod. A protected workspace PVC is mounted at `/workspace` in the worker, separately from the server's `/data` PVC and secret projections; the server does not mount the workspace PVC, and the worker does not mount `/data`, the PostgreSQL CA, or the OAuth keyring. An ephemeral volume shared only by these containers holds `/run/waltr-sandbox/gateway.sock`. Give both containers the socket path and their respective workspace-root variables from [Workspace Worker Deployment](#workspace-worker-deployment). The main Service/route exposes server port `14333`; the separate Apps Service/route exposes the same server process's asset-only port `14334`. Never create a sandbox Service, ingress, or published socket. The containers share one pod network namespace, so this layout separates mounts and processes, **not** network access. Restrict worker egress and runtime authority separately if stronger isolation is required; the pod NetworkPolicy by itself cannot isolate a sidecar from the server's network.

After first rollout, confirm the storage driver grants UID/GID `65532` write access to the protected PVC and the worker has connected. Replace `YOUR_NAMESPACE` in these checks; the temporary probe file is removed after the read:

```bash
kubectl -n YOUR_NAMESPACE exec deployment/waltr -c waltr-sandbox -- /bin/sh -c 'printf ok > /workspace/.waltr-worker-smoke && test "$(cat /workspace/.waltr-worker-smoke)" = ok && rm /workspace/.waltr-worker-smoke'
kubectl -n YOUR_NAMESPACE exec deployment/waltr -c waltr -- curl --fail --silent http://127.0.0.1:14333/ready
```

If the PVC cannot be written by the worker, fix storage provisioning or group ownership rather than mounting server-private storage or running the worker as root.

This is a clean cutover. Do not add `WALTR_MCP_OIDC_*`, `WALTR_MCP_SELF_OAUTH_*`, an Authentik MCP audience, a second advertised issuer, or a fallback validator. Existing native API tokens remain Authentik-issued but are not accepted at `/mcp`.

Pulumi creates one random 256-bit key per configured wrapping-key version and persists it as a secret output in stack state. The generated versioned ring is stored only in the `waltr-mcp-oauth-wrapping-keys` Secret, mounted read-only at `/var/run/secrets/waltr/mcp-oauth/keyring.json` with mode `0440`, and made readable to runtime UID/GID `65532` through pod `fsGroup`. A secret-derived SHA-256 rollout annotation changes when the ring changes; no key or keyring is exported. Stack outputs expose only issuer, resource, metadata, and JWKS URLs.

The keyring JSON requires top-level `schema_version: 1`, names one active version, and contains URL-safe base64 256-bit keys. Waltr rejects missing or unknown schema versions, unknown fields, non-files, files larger than 64 KiB, duplicate IDs, invalid key lengths, and Unix files writable by any class. Signing keys are generated in MCP, encrypted at rest under the active wrapping version, and rotated by the server-owned maintenance task every 24 hours. Activation is atomic; the prior public key remains in JWKS for the maximum configured access-token lifetime plus clock skew, and retained private material is rewrapped with the current wrapping key.

The Pulumi workload exports logs, traces, and metrics over OTLP/HTTP protobuf to `https://telemetry.holdenitdown.net:4318` and profiles to `https://telemetry.holdenitdown.net:4040`. Its `OTEL_RESOURCE_ATTRIBUTES` sets `deployment.environment.name` to the Pulumi stack name, distinguishing `preview` from `prod`; profiles retain the `waltr-server` application name.

The egress NetworkPolicy permits DNS, HTTPS discovery, OTLP on TCP `4318`, profiling on TCP `4040`, and PostgreSQL. The sidecar shares the pod's egress policy; its shell commands are not subject to the server's application HTTP destination policy. Application destination policy rejects private, loopback, link-local, metadata-service, DNS-rebinding, and redirect destinations by default. The exact initialized hosted MCP HTTPS origin is trusted automatically for self-CIMD, exact-self authorization orchestration, and agent session refresh; additional private origins require the direction-specific allowlists above. Trust never admits another scheme, host, or port. Fetches pin approved DNS addresses, verify the connected peer, use bounded headers/body/timeouts, require JSON, and send no ambient credentials. Loopback, link-local, metadata, reserved, and other prohibited addresses remain blocked even for a trusted origin. NetworkPolicy port filtering does not replace those checks.

CloudNativePG writes compressed WAL and data backups under the generated S3 bucket's `database/` prefix, retains backups for 30 days, and runs a daily scheduled backup. Treat the MCP database and the matching wrapping-key Secret version as one recovery unit. A restored database without every wrapping key referenced by encrypted signing rows cannot issue or rotate credentials. See [MCP OAuth Key Rotation And Recovery](runbooks/mcp-oauth-key-rotation-recovery.md).

### Release Pipelines

Pipelines as Code definitions under `.tekton/` separate server deployment from application prereleases. A push to `main` targets only the `preview` stack: it builds multi-architecture server and worker images from the full commit SHA and passes their verified immutable digests to Pulumi. It publishes no Tauri application release. The production pipeline is triggered only by an exact stable `vX.Y.Z` tag and retains signed annotated-tag validation. It builds both images, verifies that neither version tag conflicts with an existing digest, promotes both tags, and deploys their immutable digests. Registry writes to the two image repositories are not atomic; a failed partial promotion can be retried when the published digest matches. RC tags cannot trigger this stable pipeline.

The Tauri prerelease definition is triggered only by an exact `vX.Y.Z-rc.N` tag. Before any build it requires the tag to be annotated, signed by a key in `waltr-release-trusted-signers`, resolved to the full webhook commit SHA, and reachable from `origin/main`. It strips the tag signature from the annotated message and uses the remaining message as the Forgejo release body. Version components have no leading zeros. The build additionally requires `X <= 20`, `Y <= 99`, `Z <= 999`, and `1 <= N <= 999`, and computes Android versionCode as `X*100000000 + Y*1000000 + Z*1000 + N` with an explicit signed 32-bit check.

The shared build Task publishes exactly `waltr-X.Y.Z-rc.N-android-universal.apk` and `waltr-X.Y.Z-rc.N-linux-amd64.AppImage`. It verifies the APK certificate against the PAC repository setting `WALTR_ANDROID_SIGNING_CERT_SHA256`. The release is reconciled as `prerelease=true` and `draft=false`; rerunning the same tag updates its metadata and replaces assets with those same names. Build and publication use separate `waltr-tauri-build-v1` and `waltr-forgejo-release-v1` ServiceAccounts and OpenBao policies. Their KV v2 paths are `kv/data/ci/waltr/android-signing` and `kv/data/ci/waltr/forgejo-release`, respectively.

The prerelease PipelineRun references the shared homelab Tasks from the homelab `main` branch so each run uses the current Task definitions. Deploy those Tasks and their ServiceAccounts, OpenBao roles/policies, and secret payloads before pushing an RC tag. Review homelab Task changes as part of release readiness because later changes apply to future Waltr release runs without a Waltr repository update. These definitions and their static checks do not establish that a stack or application release has been deployed successfully.

#### CI Image Adoption Boundaries

Maintained CI tool images use `latest` with `imagePullPolicy: Always`; the deployment steps select `cr.holdenitdown.net/rfhold/general-ci:latest`. Third-party tool images retain their exact version and digest pins, including the Alpine manifest-tool image and its shell scripts. Dockerfile bases, application runtime parameters, and verified deployment digests remain unchanged. The maintained Tauri builder also selects `latest`, subject to the publication prerequisite below.

The homelab Rust/web contract provides Bookworm, Rust 1.96.0, Node.js 24.18.0, pnpm 11.5.0, and wasm-bindgen CLI 0.2.122. Its pipeline defines native amd64/arm64 verification before a unique multi-architecture manifest and an immutable `image-reference` result. After digest validation, the pipeline copies that verified manifest digest to `latest`; the unique tag and immutable result remain intact.

Waltr's Dockerfile pins `build-base` to `cr.holdenitdown.net/rfhold/rust-web-ci@sha256:472959b480ef0f974ba75be2ee5aef4b6e20fd3f5fd1491b03559826ca1f30ce` after successful native amd64/arm64 verification and manifest publication. The server and test builds inherit Node, system packages, pnpm, the WASM target, and wasm-bindgen CLI from this image. The image supplies `/pnpm` on `PATH` and retains Cargo home at `/usr/local/cargo`, compatible with the application cache mounts. Its Cargo configuration replaces crates.io with the internal sparse mirror at `https://mirrors.holdenitdown.net/cargo/api/v1/cratesio/`; npm uses `https://mirrors.holdenitdown.net/npm/`. Dependency builds require access to these mirrors. The worker build retains its separate Rust base; Bookworm runtime images and application cache mounts remain unchanged. Offline contract tests check this adoption, but do not prove a Waltr image build or deployment.

The separate browser image supports Linux amd64 only. It supplies exact Playwright 1.63.0 Chromium, not application dependencies or an arm64 manifest. Its pipeline verifies a unique candidate under nonroot sandbox restrictions before digest promotion to a unique final tag. See [dedicated browser image checks](testing.md#dedicated-browser-image) for the opt-in consumer and offline prerequisites. Current Waltr CI does not adopt it; kernel sandbox compatibility remains unverified.

#### Tauri Release Builder Contract

[`.tekton/waltr-app-prerelease.yaml`](../.tekton/waltr-app-prerelease.yaml) selects `cr.holdenitdown.net/rfhold/tauri-release-ci:latest`. The shared homelab Task retains `imagePullPolicy: Always` for this maintained CI image.

Prerelease execution requires Homelab to publish the `latest` tag first. The Homelab publication change adds that tag; the local Waltr change neither publishes it nor proves registry availability. Until publication completes, the builder image cannot resolve.

The source contract includes Rust 1.96.0, Clippy, rustfmt, Java 17, Android API 36, build-tools 36.0.0, and NDK 29.0.14206865. It also includes Tauri CLI 2.10.1, pnpm 11.5.0, wasm-bindgen CLI 0.2.122, and the required Android, WebAssembly, and Linux Rust targets.

The release image adds `libasound2-dev` and `pkg-config` for native audio dependencies. Its final Dockerfile runs a verifier that compiles, links, and executes an ALSA C probe without audio hardware. This does not prove device access or an application release build.

The shared Task also extracts the actual AppImage without FUSE or application startup before APK signature operations and final asset installation. It resolves the main amd64 ELF64 executable from the root desktop entry and checks its direct, transitive, and version dependencies. Resolution uses AppDir `usr/lib`, ELF RPATH/RUNPATH, and builder-host defaults; any unresolved dependency fails the check.

If the main dependency closure includes ALSA, the Task requires resolution and reports `bundled` or `builder-host`. A valid non-audio application reports `not-linked` and passes. The Task does not inspect unrelated plugins as part of this closure.

The check requires coreutils `timeout`/`realpath`, binutils `readelf`, and libc-bin `ldd`. Bounded private output, time limits, clean probe environments, and cleanup constrain inspection. These commands still require verification inside the actual builder.

This proves builder dependency resolution only. It does not prove target-OS portability, startup, dynamic plugin loads, device access, Wayland, permissions, or audio acceptance.

Local source and mock-backed tests do not establish image contents or registry availability. Publication and adoption require separate approval, a verified image digest, and consumer validation. The current builder tag remains unchanged; no release retry follows from these checks.

## Startup And Operation

The public `waltr_server::build_app(BuildAppOptions)` reads environment-owned configuration and is async/fallible. It resolves both persistence targets and completes both standard migrators. It also validates access auth and hosted OAuth, performs OIDC discovery, and loads the initial JWKS. When hosted OAuth is active, it loads the wrapping ring and initializes the local active signer. It loads the unified resource catalog, starts its watcher and routine engine, and initializes agent session storage. The executable binds `WALTR_SERVER_ADDR` only after initialization succeeds.

`BuildAppOptions::default()` supplies default interaction limits; hosts select custom bounds through its `interaction_limits` field. Embedded hosts set the same `InteractionLimits` type through `EmbeddedServerConfig.interaction_limits`. Both primary builders reject invalid limits before initialization or runtime effects.

Standalone startup retains only `WALTR_PUBLIC_URL` and supported `WALTR_MCP_*_OAUTH_{CLIENT_ID,ISSUER,REDIRECT_URI}` values. It snapshots `WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS` once as a boolean.

Later environment changes do not alter active behavior. Embedded composition ignores the ambient private-test switch.

The snapshot excludes arbitrary process environment. Its redacted `Debug` representation reports configuration presence and count without values.

Embedded configuration and lifecycle `Debug` output omits endpoints, bearers, app-private paths, and task or thread handles.

Environment-owned servers expand required `${env:WALTR_PUBLIC_URL}` in bundled `mcp-servers/waltr.yaml` and join `/mcp`. Their self resource remains mutable through persisted overlays and administration.

Embedded Tauri composition prebinds loopback, atomically overwrites the ordinary persisted `mcp-servers/waltr.yaml` slot with that generation's exact URL, and then loads the same bundled-plus-persisted catalog. The filesystem resource remains ordinarily inspectable, replaceable, watchable, and resettable. Reset can fail normal candidate validation because the bundled fallback needs `WALTR_PUBLIC_URL`.

The host-owned generation credential remains memory-only and is available only when the effective resource URL exactly matches the initialized loopback endpoint, so an edited endpoint cannot receive it. Hosted servers retain their private-origin policy.

Embedded outbound OAuth uses the exact callback derived from that same prebound origin. It does not consult ambient public or target redirect variables.

Embedded configuration contains no client metadata URI. Therefore its local `/.well-known/oauth-client/waltr` route returns `404 Not Found`.

`/health` reports process liveness. `/ready` checks worker connection in addition to persistence, routine engine health, optional hosted OAuth maintenance, and definition watcher health. Hosted readiness also checks its store, active signer, key agreement, maintenance, and bounded revocation policy. Invalid candidate resource content alone does not fail readiness while the watcher and last-known-good snapshot remain healthy. Startup requires a connected worker with a usable canonical root; after disconnect readiness fails until reconnection, without retrying an uncertain tool execution.

Startup interrupts unfinished Routine invocations before the engine reports ready. Waltr does not resume them.

Deploy one active Routine engine for each Waltr database. Clustered Routine leases and cross-engine execution coordination are not implemented.

`ServerApplication` owns the definition watcher, routine engine, session host, and hosted OAuth maintenance. Graceful shutdown stops Routine work, invokes `SessionCoordinator::shutdown`, and joins the other owned tasks. Coordinator shutdown rejects new work, cancels active and queued work, waits for run ownership to drain, atomically drains MCP caches under the lifecycle gate, and awaits each cached runtime's explicit shutdown. Stdio runtimes therefore use the generic MCP client's shutdown/reap path; no stronger bounded-termination guarantee is made. Pulumi uses `/ready` only for readiness and retains `/health` for startup and liveness.

Environment-owned startup loads models.dev from `WALTR_MODELS_DEV_PATH`, `WALTR_MODELS_DEV_URL`, or the default URL. The disk cache can satisfy an unavailable source.

Tauri always requests `https://models.dev/api.json` first. The connection has a five-second bound, and the complete request has a 15-second bound.

Failure or timeout uses a valid app-private cache, or the explicit built-in seed only when that cache is absent.
Both fallback sources report stale catalog state; malformed or unreadable cache state fails closed.
Standalone startup has no built-in fallback and fails when neither source nor cache supplies a valid catalog.

Local revocation wakes same-process streams after the database commit. Every hosted stream also performs an authoritative durable check at most every five seconds; another replica's revocation therefore closes it within five seconds plus scheduling and query latency. PostgreSQL `LISTEN/NOTIFY` is not implemented. Do not increase the current one-replica Pulumi workload until the documented container-backed concurrency and cross-replica operational checks have been exercised.

Explicit builders are intended for callers that own dependencies. They require already initialized `Persistence`; access-authenticated composition additionally requires an initialized `AccessAuthenticator`. They return the same `ServerApplication` lifecycle, which callers must serve or explicitly shut down rather than detaching its background work from server ownership.

The bundled TUI installed with `pnpm install:bundled` includes server and web features and starts its embedded server by default. The TUI retains the embedded server's shutdown signal and task, signals shutdown after the terminal runtime exits on either success or error, and joins server completion before the process returns; MCP cleanup therefore does not depend on dropping the Tokio runtime. Use `--no-server` with `WALTR_SERVER_ENDPOINT` to connect it to an external deployment. A plain `cargo run -p waltr-tui` build has no server feature and connects to `WALTR_SERVER_ENDPOINT`, defaulting to `http://127.0.0.1:14333`.

See [Access Authentication](architecture/access-authentication.md), [Server Persistence](architecture/persistence.md), [Routines](architecture/routines.md), and [Testing](testing.md).

No successful production deployment, live Pulumi/Tekton deployment, database failover, or backup restoration is asserted by this document.
