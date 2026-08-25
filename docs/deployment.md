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

The bundled self-MCP resource requires `WALTR_PUBLIC_URL` in every access-auth mode. It consumes the variable independently of OIDC, joins `/mcp` through URL parsing, and preserves a deployment path prefix. With access auth disabled, actual partial browser, API, or hosted-MCP OIDC settings still fail startup.

The server watches `.waltr/resources` after startup. An invalid reload preserves the last-known-good snapshot and exposes the reload error.

Routine resources use strict YAML and stay outside durable agent-definition bundles. The Routine service exposes definitions and durable invocation history globally.

Public `/mcp` actions provide resource administration:

| Action | Behavior |
| --- | --- |
| `resources.inspect` | Lists resource slots or inspects one canonical path. |
| `resources.replace` | Validates and atomically replaces one complete YAML resource. |
| `resources.reset` | Removes one persisted override and reveals any bundled fallback. |

Replace and reset require the latest persisted-slot ETag. They validate the complete candidate catalog before filesystem mutation.

`waltr.resource.v1.ResourceService` provides equivalent gRPC reads, watches, inspection, replace, and reset operations. Reads and watch events contain complete catalog snapshots.

Provider resources can materialize models.dev selectors with optional `keyPrefix` and aliases. MCP resources select `streamableHttp` or `stdio`.

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

`WALTR_PUBLIC_URL` is the externally reachable Waltr API resource identifier and may include a path prefix. It is required in every auth mode because the bundled self-MCP resource consumes it independently of access authentication. It must use HTTPS except for loopback development and is not inferred from the browser callback. Disabling access auth permits this variable but still rejects actual partial browser, API, or hosted-MCP OIDC settings. `WALTR_OIDC_AUDIENCE` is retained only as the API-audience fallback; new deployments should set `WALTR_API_OIDC_AUDIENCE` explicitly. `WALTR_API_OIDC_CLIENT_ID` enables provider-generic `JWT` access tokens by requiring their `azp` claim to exactly match the configured native client; RFC 9068 `at+jwt` access tokens remain supported without that claim.

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

Register a separate public native client at the effective API authorization server. It must allow Authorization Code with PKCE S256 and the selected loopback redirects. The TUI always sends `client_id` and PKCE without a client secret or HTTP Basic authentication. Discovery may advertise only confidential token methods because some providers publish endpoint-wide capabilities that omit `none`. Do not reuse or distribute the browser confidential-client secret.

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

## Authentik And Pulumi Deployment

The repository's supported infrastructure/workload definition is Pulumi under `infra/pulumi/`; there is no Kustomize deployment. Each `preview` and `prod` stack is defined to provision the Kubernetes workload, PostgreSQL, S3-compatible session/backup bucket, and two Authentik providers:

| Provider | Type | Use |
| --- | --- | --- |
| Browser | Confidential | Server-side browser Authorization Code flow and `/oidc/callback`. |
| Native API | Public | TUI Authorization Code with PKCE and loopback redirects; issues the API audience/scope. |
Authentik has no MCP provider, application, audience, scope mapping, callback, or token-issuance role. Waltr hosts MCP authorization, supports DCR and CIMD according to explicit stack policy, and retains `WALTR_MCP_REQUIRED_SCOPES` as local policy.

This is a clean cutover. Do not add `WALTR_MCP_OIDC_*`, `WALTR_MCP_SELF_OAUTH_*`, an Authentik MCP audience, a second advertised issuer, or a fallback validator. Existing native API tokens remain Authentik-issued but are not accepted at `/mcp`.

Pulumi creates one random 256-bit key per configured wrapping-key version and persists it as a secret output in stack state. The generated versioned ring is stored only in the `waltr-mcp-oauth-wrapping-keys` Secret, mounted read-only at `/var/run/secrets/waltr/mcp-oauth/keyring.json` with mode `0440`, and made readable to runtime UID/GID `65532` through pod `fsGroup`. A secret-derived SHA-256 rollout annotation changes when the ring changes; no key or keyring is exported. Stack outputs expose only issuer, resource, metadata, and JWKS URLs.

The keyring JSON requires top-level `schema_version: 1`, names one active version, and contains URL-safe base64 256-bit keys. Waltr rejects missing or unknown schema versions, unknown fields, non-files, files larger than 64 KiB, duplicate IDs, invalid key lengths, and Unix files writable by any class. Signing keys are generated in MCP, encrypted at rest under the active wrapping version, and rotated by the server-owned maintenance task every 24 hours. Activation is atomic; the prior public key remains in JWKS for the maximum configured access-token lifetime plus clock skew, and retained private material is rewrapped with the current wrapping key.

The Pulumi workload exports logs, traces, and metrics over OTLP/HTTP protobuf to `https://telemetry.holdenitdown.net:4318` and profiles to `https://telemetry.holdenitdown.net:4040`. Its `OTEL_RESOURCE_ATTRIBUTES` sets `deployment.environment.name` to the Pulumi stack name, distinguishing `preview` from `prod`; profiles retain the `waltr-server` application name.

The egress NetworkPolicy permits DNS, HTTPS discovery, OTLP on TCP `4318`, profiling on TCP `4040`, and PostgreSQL. Application destination policy rejects private, loopback, link-local, metadata-service, DNS-rebinding, and redirect destinations by default. The exact initialized hosted MCP HTTPS origin is trusted automatically for self-CIMD, exact-self authorization orchestration, and agent session refresh; additional private origins require the direction-specific allowlists above. Trust never admits another scheme, host, or port. Fetches pin approved DNS addresses, verify the connected peer, use bounded headers/body/timeouts, require JSON, and send no ambient credentials. Loopback, link-local, metadata, reserved, and other prohibited addresses remain blocked even for a trusted origin. NetworkPolicy port filtering does not replace those checks.

CloudNativePG writes compressed WAL and data backups under the generated S3 bucket's `database/` prefix, retains backups for 30 days, and runs a daily scheduled backup. Treat the MCP database and the matching wrapping-key Secret version as one recovery unit. A restored database without every wrapping key referenced by encrypted signing rows cannot issue or rotate credentials. See [MCP OAuth Key Rotation And Recovery](runbooks/mcp-oauth-key-rotation-recovery.md).

Pipelines as Code definitions under `.tekton/` are configured to build the Dockerfile's multi-architecture `runtime` target and deploy only through Pulumi. A push to `main` targets the `preview` stack with an image tagged by the full commit SHA. The production definition accepts only a tag that strictly matches `vX.Y.Z`; it builds and deploys that exact tag and publishes the multi-architecture tag as `latest`. A tag matching the broad trigger but not the strict version expression fails before production provisioning or deployment. These definitions and their static checks do not establish that either stack has been deployed successfully.

## Startup And Operation

The public `waltr_server::build_app()` reads environment-owned configuration and is async/fallible. It resolves both persistence targets and completes both standard migrators. It also validates access auth and hosted OAuth, performs OIDC discovery, and loads the initial JWKS. When hosted OAuth is active, it loads the wrapping ring and initializes the local active signer. It loads the unified resource catalog, starts its watcher and routine engine, and initializes agent session storage. The executable binds `WALTR_SERVER_ADDR` only after initialization succeeds.

The bundled `mcp-servers/waltr.yaml` resource expands required `${env:WALTR_PUBLIC_URL}` and joins `/mcp`. The resource uses the normal target-scoped outbound OAuth lifecycle. When hosted MCP OAuth starts, Waltr admits its private HTTPS origin only for that exact self resource. Other private origins require explicit configuration.

`/health` reports process liveness. `/ready` checks persistence, routine engine health, optional hosted OAuth maintenance, and definition watcher health. Hosted readiness also checks its store, active signer, key agreement, maintenance, and bounded revocation policy. Invalid candidate resource content alone does not fail readiness while the watcher and last-known-good snapshot remain healthy.

Startup interrupts unfinished Routine invocations before the engine reports ready. Waltr does not resume them.

Deploy one active Routine engine for each Waltr database. Clustered Routine leases and cross-engine execution coordination are not implemented.

`ServerApplication` owns the definition watcher, routine engine, and hosted OAuth maintenance. Graceful shutdown cancels and joins each owned task. Pulumi uses `/ready` only for readiness and retains `/health` for startup and liveness.

Startup loads models.dev to materialize provider resource selectors. The loader uses `WALTR_MODELS_DEV_PATH` or `WALTR_MODELS_DEV_URL`, then its configured default. The disk cache can satisfy an unavailable source. Startup fails when no source or cache can materialize the complete catalog.

Local revocation wakes same-process streams after the database commit. Every hosted stream also performs an authoritative durable check at most every five seconds; another replica's revocation therefore closes it within five seconds plus scheduling and query latency. PostgreSQL `LISTEN/NOTIFY` is not implemented. Do not increase the current one-replica Pulumi workload until the documented container-backed concurrency and cross-replica operational checks have been exercised.

Explicit builders are intended for callers that own dependencies. They require already initialized `Persistence`; access-authenticated composition additionally requires an initialized `AccessAuthenticator`. They return the same `ServerApplication` lifecycle, which callers must serve or explicitly shut down rather than detaching its background work from server ownership.

The bundled TUI installed with `pnpm install:bundled` includes server and web features and starts its embedded server by default. The TUI retains the embedded server's shutdown signal and task, signals shutdown after the terminal runtime exits on either success or error, and joins server completion before the process returns; MCP cleanup therefore does not depend on dropping the Tokio runtime. Use `--no-server` with `WALTR_SERVER_ENDPOINT` to connect it to an external deployment. A plain `cargo run -p waltr-tui` build has no server feature and connects to `WALTR_SERVER_ENDPOINT`, defaulting to `http://127.0.0.1:14333`.

See [Access Authentication](architecture/access-authentication.md), [Server Persistence](architecture/persistence.md), [Routines](architecture/routines.md), and [Testing](testing.md).

No successful production deployment, live Pulumi/Tekton deployment, database failover, or backup restoration is asserted by this document.
