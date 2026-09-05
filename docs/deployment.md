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

Waltr and MCP use separate pools. SQLite keeps Waltr's standard `_sqlx_migrations` history, OIDC tables, hosted-OAuth login continuations, outbound OAuth attempts/credentials, and Routine history in `waltr.sqlite3`, while MCP history, inert session/event tables, clients, grants, token state, CIMD cache, and encrypted signing keys live in `mcp.sqlite3`. PostgreSQL uses `waltr` and `mcp` search paths for migration application, producing `waltr._sqlx_migrations` and `mcp._sqlx_migrations` in the default shared logical database. Waltr's immutable sequence is `0001_schema_ownership`, `0002_oidc_access_auth`, `0003_mcp_oauth_login_continuations`, `0004_outbound_oauth`, `0005_unified_oauth_credentials`, and `0006_routine_invocations`; MCP's is `0001_streamable_http_sessions`, `0002_oauth_authorization_server`, and `0003_oauth_client_registration`. V5 is a hard cutover that discards existing MCP SQL credentials and stops reading legacy provider credential files; all targets must authorize again after upgrade. Legacy `auth.json` files are not deleted automatically and should be securely removed after rollback is no longer required. V6 adds exactly the Routine aggregate and append-only event-journal tables, with no task table or pruning. Both owners use embedded `sqlx::migrate!()` migrators, so SQLx owns standard version and checksum validation. Applied migration files must not be edited. Both migrators complete before the server binds. Invalid URLs, unavailable databases, SQLx migration validation failures, migration application failures, missing wrapping keys, and unusable signing state fail startup; there is no memory fallback.

## Container Runtime And Local Compose

The repository `Dockerfile` produces the supported production `runtime` image as well as test-only stages. The runtime contains the release server with embedded web assets, runs as nonroot UID/GID `65532`, listens on `0.0.0.0:14333`, and uses `/data` as its home, working directory, and default writable persistence root. Mount writable storage at `/data` for filesystem-backed settings, or configure external PostgreSQL and S3 services.

The server's default Cargo feature set includes `shell` and `profiling`. The optional `profiling` feature adds Pyroscope initialization, but the standalone server binary remains available and functional when that feature is disabled.

Embedded Tauri builds use `waltr-server` with `default-features = false`. Desktop adds only `shell`; Android adds neither shell nor Pyroscope.

The build installs pnpm dependencies from workspace manifests before copying Rust and web sources. It then copies only the Cargo workspace files and `crates/` into the shared builder, so documentation, infrastructure, and repository tooling changes do not invalidate application compilation. BuildKit cache mounts retain the pnpm store and Cargo registry, Git, and target data under architecture-specific IDs. Those mutable caches are local to the selected BuildKit worker; the repository pipelines do not import or export a registry-backed build cache. Persistent cluster workers and daemon-configured package or registry mirrors can accelerate builds transparently, but their availability and retention are infrastructure concerns rather than repository guarantees.

`compose.yml` remains fixed local development and integration-test infrastructure. It intentionally exposes only static `dev`, `test`, and `test-auth` profiles; bare `docker compose up` starts nothing.

`docker compose --profile dev up` builds the embedded server and web assets for local use. The `waltr` service uses:

- `WALTR_DATABASE_URL=sqlite:///data/waltr.sqlite3`
- Empty `WALTR_MCP_DATABASE_URL`, which derives the MCP target
- Derived MCP database `/data/mcp.sqlite3`
- `WALTR_SESSION_STORE=filesystem`
- `WALTR_SESSION_FILESYSTEM_ROOT=/data/sessions`
- `WALTR_PUBLIC_URL=http://127.0.0.1:14333`
- Named volume `waltr-dev-data` mounted at `/data`

The static profile does not consume ambient `WALTR_*` deployment settings. Change server environment configuration in the actual process/container runtime used by a deployment, not through this Compose fixture.

The `dev` volume contains both SQLite files, including unified outbound credentials in `waltr.sqlite3`, and filesystem agent sessions. It is disposable local state, not a production backup contract.

The `test` and `test-auth` profiles use tmpfs persistence, internal-only networking, health-gated startup, deterministic Dex/MCP fixtures, and an ephemeral TLS CA. Their `WALTR_OAUTH_*` and `WALTR_TEST_MCP_*` values are test-fixture settings, not server deployment variables. The authenticated Waltr fixture enables the hosted MCP issuer, generates an ephemeral keyring in tmpfs, and sets `WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS=true` only so HTTPS Waltr-to-Waltr CIMD and token requests can reach private Compose aliases. Production deployments must leave this deterministic test exception unset. The fixture does not configure a direct MCP issuer or a preregistered self client.

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

Public `/mcp` actions provide resource administration:

| Action | Behavior |
| --- | --- |
| `resources.inspect` | Lists resource slots or inspects one canonical path. |
| `resources.replace` | Validates and atomically replaces one complete YAML resource. |
| `resources.reset` | Removes one persisted override and reveals any bundled fallback. |

Replace and reset require the latest persisted-slot ETag. They validate the complete candidate catalog before filesystem mutation.

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

Register a separate public native client for remote native clients such as the TUI. It must allow Authorization Code with PKCE S256 and the selected loopback redirects. The shared native protocol sends `client_id` and PKCE without a client secret or HTTP Basic authentication. Discovery can advertise only confidential token methods because some providers publish endpoint-wide capabilities that omit `none`. Do not create an app secret or reuse the browser secret.

The bundled Tauri app does not use this registration. It admits its Rust client to the embedded server with a process-generation local bearer.

Configure the TUI process with:

```text
WALTR_SERVER_ENDPOINT=https://waltr.example
WALTR_OIDC_NATIVE_CLIENT_ID=waltr-native
WALTR_OIDC_NATIVE_SCOPES="openid offline_access waltr:api"
WALTR_OIDC_NATIVE_AUDIENCE=waltr-api
WALTR_OIDC_NATIVE_LOOPBACK_POLICY=ephemeral
```

Only the native client ID is required at login. The TUI discovers only the API-root metadata at `/.well-known/oauth-protected-resource`, verifies its resource exactly matches `WALTR_SERVER_ENDPOINT`, sends that API resource in the authorization request, and never derives or discovers `/mcp`. Omit scopes to request `openid` plus the API metadata scopes. `WALTR_OIDC_NATIVE_AUDIENCE` is an optional provider-specific authorization parameter; it does not replace the resource parameter or server-side API audience validation. The loopback policy defaults to `ephemeral`; a fixed value must be an exact `http://127.0.0.1:<port>` URL registered with the provider.

Native tokens and refresh tokens are process-memory only. Restarting the TUI requires login again. The TUI refreshes before expiry when a refresh token is returned and returns to login-required state when refresh fails or no refresh token remains after expiry.

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

Native Waltr access and native OTLP use rustls with the ring provider, safe TLS 1.2 or 1.3, and bundled `webpki_roots`. On Android, the same explicit TLS and root policy covers server-owned models.dev catalog refresh and provider and upstream-MCP OAuth discovery, registration, exchange, and revocation; agent provider and MCP OAuth refresh; OpenAI and Anthropic HTTP inference; and Waltr-agent MCP Streamable HTTP through a caller-supplied generic MCP reqwest client. OpenAI websocket already uses explicit ring and bundled roots on every platform. These Android clients retain certificate-chain and hostname checks but exclude Android platform, user, and private roots and `SSL_CERT_FILE`. Non-Android paths retain default/platform reqwest behavior where they previously used it. Other Android networking paths continue to use platform verification initialized by the foreground service. The Tauri Waltr client connects only to its host-created HTTP loopback endpoint. Hosted browser and WebView Faro trust remain platform-owned.

Client telemetry follows the boundary in [Clients](architecture/clients.md#client-observability-boundary). Standard Faro data is high-detail personal-use telemetry, not a globally sanitized stream.

## Server Observability

The standalone server writes JSON logs and defaults to global `info` for every target. `RUST_LOG` overrides that default through `EnvFilter` syntax.

Set `WALTR_OTEL_EXPORT=1` or `WALTR_OTEL_EXPORT=true` to enable OTLP export. `OTEL_EXPORTER_OTLP_ENDPOINT` also enables export when present. Standard OpenTelemetry environment variables configure trace, log, and metric exporters. The service name is `waltr-server`.

Pyroscope remains available only in a standalone server build with the `profiling` feature. Set `WALTR_PYROSCOPE_URL` to start it with application name `waltr-server`. Embedded Tauri servers do not include Pyroscope.

Use [Telemetry Smoke Checks](runbooks/telemetry-smoke.md) after a build and runtime satisfy the signal prerequisites. Source changes alone do not establish deployment or ingestion.

## Tauri Application

`crates/waltr-app/` packages local `crates/waltr-web/` assets and an embedded `waltr-server`. It runs the shared client in Rust against an ephemeral IPv4 loopback endpoint.

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

The app has no Waltr endpoint selector or native Waltr access OAuth. Its web adapter supplies no generic Waltr access operations, so an unexpected login-required snapshot offers no sign-in action and the authenticated shell offers no sign-out action. Routes still follow authoritative shared access and readiness snapshots. Provider and upstream-MCP OAuth remain server-owned.

The main WebView loads only packaged local assets. Remote navigation and new windows remain denied. Rust validates credential-free HTTPS URLs before the operating system opens them.

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

First launch requires network access unless a valid cache already exists.

Later startup can use a valid cache when the network source fails. Startup fails when neither source supplies a valid catalog.

Desktop and Android use the same ordinary bundled and persisted resource catalog. Desktop retains server-side Bash shell execution; the shell has no operating-system sandbox.

Android is compiled without shell support. Shell-bearing definitions remain valid and inspectable, but runtime construction omits the shell tool and feature-derived shell tool permissions. Stdio MCP resources materialize and use the ordinary runtime path.

These server-side capabilities differ from WebView capabilities. The WebView keeps its minimal local IPC grants on every platform.

Run these commands from the repository root. They map to current workspace scripts and manifests:

| Target | Command | Purpose |
| --- | --- | --- |
| Tauri web | `pnpm build:tauri` | Type-check and build relative local assets with Vite `tauri` mode and no WASM build. |
| Tauri development web | `pnpm --filter @rfhold/waltr-web build:tauri:dev` | Build relative no-WASM Tauri assets with real Faro export disabled. |
| Linux desktop | `pnpm --filter @rfhold/waltr-app dev` | Build the Tauri web assets and start the desktop app. |
| Linux packages | `pnpm --filter @rfhold/waltr-app build` | Build the configured Linux Tauri bundles. |
| Android package | `pnpm --filter @rfhold/waltr-app android:build` | Build through the generated Android project under `crates/waltr-app/gen/android/`. |
| Android app | `pnpm --filter @rfhold/waltr-app exec cargo tauri android dev` | Build, install, and start on a configured emulator or device. |

Android launcher assets derive from `crates/waltr-web/public/favicon.svg`. Run `crates/waltr-app/scripts/android-launcher-icons.sh` after changing that artwork, and run the same command with `--check` to verify the committed legacy and adaptive resources. The launcher assets are separate from the monochrome foreground-service notification icon.

Focused deterministic checks are listed in [Testing](testing.md). Package creation does not prove installed-device lifecycle behavior.

### Android Foreground Service

Android requires notification permission before it starts the embedded server on Android 13 and later. Denial leaves startup at `notificationPermissionRequired`.

`WaltrForegroundService` is non-exported and runs in the app process. It uses foreground-service type `specialUse`, a persistent notification, and `android:stopWithTask="false"`.

Ordinary service startup returns `START_STICKY`. The notification exposes a Stop server action. That action requests the current nonblocking native server stop, removes the foreground notification, stops the service, and returns `START_NOT_STICKY` for that command.

The service enters foreground state before it loads the native library. Native server work starts only after both steps complete.

The current `specialUse` declaration targets sideload distribution. Play distribution requires separate policy review, declarations, and product work.

Activity pause stops only the client runtime. The service and embedded server survive background and task removal on a best-effort basis.

The Stop action does not persist a disabled setting. A later app open or resume starts a fresh embedded server generation after the prior generation stops.

Android lifecycle callbacks never wait for Rust client or server thread joins. Reaper threads complete those joins outside callbacks.

A nonblocking server stop transitions the exact current generation to `Stopping` and detaches its thread under the controller lock. Its reaper joins only that generation and publishes `Stopped` or `Failed` after the join.

New start reservation waits while the controller is `Stopping`, so a replacement starts only after the old reaper completes. Embedded generations never overlap their shared app-private roots, and Android callbacks remain nonblocking because they do not perform the join.

Notification permission changes publish an adapter-private startup control event to an active client generation. A later client generation reads the current permission state at startup.

A fresh generation applies permission state before it can use an existing server connection. Permission-required generations remain disconnected until permission changes.

Each client-start ticket owns the exact bridge generation that it creates. Suspend invalidates in-progress ownership, and resume creates a replacement ticket.

A stale or suspended completion stops only its own generation. It never stops a newer replacement or leaves an unowned background client.

Foreground-service APIs do not guarantee survival through force-stop, process kill, reboot, or user termination. Android can terminate the process at any time.

After process death, a later service or app start creates a new endpoint and bearer. Durable stores reopen, and Routine restart reconciliation handles unfinished invocations.

Android excludes all app-private files, databases, shared preferences, and root data from cloud backup and device transfer.

## Authentik And Pulumi Deployment

The repository's supported infrastructure/workload definition is Pulumi under `infra/pulumi/`; there is no Kustomize deployment. Each `preview` and `prod` stack is defined to provision the Kubernetes workload, PostgreSQL, S3-compatible session/backup bucket, and two Authentik providers:

| Provider | Type | Use |
| --- | --- | --- |
| Browser | Confidential | Server-side browser Authorization Code flow and `/oidc/callback`. |
| Native API | Public | Remote native clients such as the TUI; issues the API audience and scope. |
Authentik has no MCP provider, application, audience, scope mapping, callback, or token-issuance role. Waltr hosts MCP authorization, supports DCR and CIMD according to explicit stack policy, and retains `WALTR_MCP_REQUIRED_SCOPES` as local policy.

This is a clean cutover. Do not add `WALTR_MCP_OIDC_*`, `WALTR_MCP_SELF_OAUTH_*`, an Authentik MCP audience, a second advertised issuer, or a fallback validator. Existing native API tokens remain Authentik-issued but are not accepted at `/mcp`.

Pulumi creates one random 256-bit key per configured wrapping-key version and persists it as a secret output in stack state. The generated versioned ring is stored only in the `waltr-mcp-oauth-wrapping-keys` Secret, mounted read-only at `/var/run/secrets/waltr/mcp-oauth/keyring.json` with mode `0440`, and made readable to runtime UID/GID `65532` through pod `fsGroup`. A secret-derived SHA-256 rollout annotation changes when the ring changes; no key or keyring is exported. Stack outputs expose only issuer, resource, metadata, and JWKS URLs.

The keyring JSON requires top-level `schema_version: 1`, names one active version, and contains URL-safe base64 256-bit keys. Waltr rejects missing or unknown schema versions, unknown fields, non-files, files larger than 64 KiB, duplicate IDs, invalid key lengths, and Unix files writable by any class. Signing keys are generated in MCP, encrypted at rest under the active wrapping version, and rotated by the server-owned maintenance task every 24 hours. Activation is atomic; the prior public key remains in JWKS for the maximum configured access-token lifetime plus clock skew, and retained private material is rewrapped with the current wrapping key.

The Pulumi workload exports logs, traces, and metrics over OTLP/HTTP protobuf to `https://telemetry.holdenitdown.net:4318` and profiles to `https://telemetry.holdenitdown.net:4040`. Its `OTEL_RESOURCE_ATTRIBUTES` sets `deployment.environment.name` to the Pulumi stack name, distinguishing `preview` from `prod`; profiles retain the `waltr-server` application name.

The egress NetworkPolicy permits DNS, HTTPS discovery, OTLP on TCP `4318`, profiling on TCP `4040`, and PostgreSQL. Application destination policy rejects private, loopback, link-local, metadata-service, DNS-rebinding, and redirect destinations by default. The exact initialized hosted MCP HTTPS origin is trusted automatically for self-CIMD, exact-self authorization orchestration, and agent session refresh; additional private origins require the direction-specific allowlists above. Trust never admits another scheme, host, or port. Fetches pin approved DNS addresses, verify the connected peer, use bounded headers/body/timeouts, require JSON, and send no ambient credentials. Loopback, link-local, metadata, reserved, and other prohibited addresses remain blocked even for a trusted origin. NetworkPolicy port filtering does not replace those checks.

CloudNativePG writes compressed WAL and data backups under the generated S3 bucket's `database/` prefix, retains backups for 30 days, and runs a daily scheduled backup. Treat the MCP database and the matching wrapping-key Secret version as one recovery unit. A restored database without every wrapping key referenced by encrypted signing rows cannot issue or rotate credentials. See [MCP OAuth Key Rotation And Recovery](runbooks/mcp-oauth-key-rotation-recovery.md).

### Release Pipelines

Pipelines as Code definitions under `.tekton/` separate server deployment from application prereleases. A push to `main` targets only the `preview` server stack with an image tagged by the full commit SHA; it publishes no Tauri application release. The production server definition is triggered only by an exact stable `vX.Y.Z` tag, retains signed annotated-tag validation, builds and deploys that tag, and publishes the multi-architecture container tag as `latest`. RC tags cannot trigger this stable pipeline.

The Tauri prerelease definition is triggered only by an exact `vX.Y.Z-rc.N` tag. Before any build it requires the tag to be annotated, signed by a key in `waltr-release-trusted-signers`, resolved to the full webhook commit SHA, and reachable from `origin/main`. It strips the tag signature from the annotated message and uses the remaining message as the Forgejo release body. Version components have no leading zeros. The build additionally requires `X <= 20`, `Y <= 99`, `Z <= 999`, and `1 <= N <= 999`, and computes Android versionCode as `X*100000000 + Y*1000000 + Z*1000 + N` with an explicit signed 32-bit check.

The shared build Task publishes exactly `waltr-X.Y.Z-rc.N-android-universal.apk` and `waltr-X.Y.Z-rc.N-linux-amd64.AppImage`. It verifies the APK certificate against the PAC repository setting `WALTR_ANDROID_SIGNING_CERT_SHA256`. The release is reconciled as `prerelease=true` and `draft=false`; rerunning the same tag updates its metadata and replaces assets with those same names. Build and publication use separate `waltr-tauri-build-v1` and `waltr-forgejo-release-v1` ServiceAccounts and OpenBao policies. Their KV v2 paths are `kv/data/ci/waltr/android-signing` and `kv/data/ci/waltr/forgejo-release`, respectively.

The prerelease PipelineRun references the shared homelab Tasks from the homelab `main` branch so each run uses the current Task definitions. Deploy those Tasks and their ServiceAccounts, OpenBao roles/policies, and secret payloads before pushing an RC tag. Review homelab Task changes as part of release readiness because later changes apply to future Waltr release runs without a Waltr repository update. These definitions and their static checks do not establish that a stack or application release has been deployed successfully.

## Startup And Operation

The public `waltr_server::build_app()` reads environment-owned configuration and is async/fallible. It resolves both persistence targets and completes both standard migrators. It also validates access auth and hosted OAuth, performs OIDC discovery, and loads the initial JWKS. When hosted OAuth is active, it loads the wrapping ring and initializes the local active signer. It loads the unified resource catalog, starts its watcher and routine engine, and initializes agent session storage. The executable binds `WALTR_SERVER_ADDR` only after initialization succeeds.

Standalone startup retains only `WALTR_PUBLIC_URL` and supported `WALTR_MCP_*_OAUTH_{CLIENT_ID,ISSUER,REDIRECT_URI}` values. It snapshots `WALTR_OAUTH_ALLOW_PRIVATE_TEST_DESTINATIONS` once as a boolean.

Later environment changes do not alter active behavior. Embedded composition ignores the ambient private-test switch.

The snapshot excludes arbitrary process environment. Its redacted `Debug` representation reports configuration presence and count without values.

Embedded configuration and lifecycle `Debug` output omits endpoints, bearers, app-private paths, and task or thread handles.

Environment-owned servers expand required `${env:WALTR_PUBLIC_URL}` in bundled `mcp-servers/waltr.yaml` and join `/mcp`. Their self resource remains mutable through persisted overlays and administration.

Embedded Tauri composition prebinds loopback, atomically overwrites the ordinary persisted `mcp-servers/waltr.yaml` slot with that generation's exact URL, and then loads the same bundled-plus-persisted catalog. The filesystem resource remains ordinarily inspectable, replaceable, watchable, and resettable. Reset can fail normal candidate validation because the bundled fallback needs `WALTR_PUBLIC_URL`.

The host-owned generation credential remains memory-only and is available only when the effective resource URL exactly matches the initialized loopback endpoint, so an edited endpoint cannot receive it. Hosted servers retain their private-origin policy.

Embedded outbound OAuth uses the exact callback derived from that same prebound origin. It does not consult ambient public or target redirect variables.

Embedded configuration contains no client metadata URI. Therefore its local `/.well-known/oauth-client/waltr` route returns `404 Not Found`.

`/health` reports process liveness. `/ready` checks persistence, routine engine health, optional hosted OAuth maintenance, and definition watcher health. Hosted readiness also checks its store, active signer, key agreement, maintenance, and bounded revocation policy. Invalid candidate resource content alone does not fail readiness while the watcher and last-known-good snapshot remain healthy.

Startup interrupts unfinished Routine invocations before the engine reports ready. Waltr does not resume them.

Deploy one active Routine engine for each Waltr database. Clustered Routine leases and cross-engine execution coordination are not implemented.

`ServerApplication` owns the definition watcher, routine engine, session host, and hosted OAuth maintenance. Graceful shutdown stops Routine work, invokes `SessionCoordinator::shutdown`, and joins the other owned tasks. Coordinator shutdown rejects new work, cancels active and queued work, waits for run ownership to drain, atomically drains MCP caches under the lifecycle gate, and awaits each cached runtime's explicit shutdown. Stdio runtimes therefore use the generic MCP client's shutdown/reap path; no stronger bounded-termination guarantee is made. Pulumi uses `/ready` only for readiness and retains `/health` for startup and liveness.

Environment-owned startup loads models.dev from `WALTR_MODELS_DEV_PATH`, `WALTR_MODELS_DEV_URL`, or the default URL. The disk cache can satisfy an unavailable source.

Tauri always requests `https://models.dev/api.json` first. The connection has a five-second bound, and the complete request has a 15-second bound.

Failure or timeout uses the app-private cache when that cache contains a valid catalog. First launch requires network without a valid cache.

Every mode fails startup when neither source nor cache can materialize the complete catalog.

Local revocation wakes same-process streams after the database commit. Every hosted stream also performs an authoritative durable check at most every five seconds; another replica's revocation therefore closes it within five seconds plus scheduling and query latency. PostgreSQL `LISTEN/NOTIFY` is not implemented. Do not increase the current one-replica Pulumi workload until the documented container-backed concurrency and cross-replica operational checks have been exercised.

Explicit builders are intended for callers that own dependencies. They require already initialized `Persistence`; access-authenticated composition additionally requires an initialized `AccessAuthenticator`. They return the same `ServerApplication` lifecycle, which callers must serve or explicitly shut down rather than detaching its background work from server ownership.

The bundled TUI installed with `pnpm install:bundled` includes server and web features and starts its embedded server by default. The TUI retains the embedded server's shutdown signal and task, signals shutdown after the terminal runtime exits on either success or error, and joins server completion before the process returns; MCP cleanup therefore does not depend on dropping the Tokio runtime. Use `--no-server` with `WALTR_SERVER_ENDPOINT` to connect it to an external deployment. A plain `cargo run -p waltr-tui` build has no server feature and connects to `WALTR_SERVER_ENDPOINT`, defaulting to `http://127.0.0.1:14333`.

See [Access Authentication](architecture/access-authentication.md), [Server Persistence](architecture/persistence.md), [Routines](architecture/routines.md), and [Testing](testing.md).

No successful production deployment, live Pulumi/Tekton deployment, database failover, or backup restoration is asserted by this document.
