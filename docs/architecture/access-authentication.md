# Access Authentication

## Purpose

Waltr access authentication decides whether a caller may use a Waltr server. It is separate from credential authorization, where an already admitted Waltr caller authorizes Waltr to call a model provider or an upstream MCP server.

| Boundary | Meaning | Owner |
| --- | --- | --- |
| Waltr access authentication | Admit a browser, native client, or MCP client to this Waltr server. | `waltr-server` access-auth module, shared client access state, and platform login runtimes. |
| Provider/upstream-MCP credential OAuth | Acquire credentials Waltr uses for outbound provider or MCP calls. | Existing server `AuthService`, `AuthTarget`, agent credential store, and provider OAuth UI. |
| Hosted MCP authorization | Authenticate the resource owner through the Waltr browser session, collect Waltr consent, and issue credentials accepted by Waltr `/mcp`. | Generic `mcp` authorization server and Waltr browser/consent adapters. |

An access failure never creates an `AuthTarget`. Every successfully authenticated issuer/subject enters one shared Waltr application authorization domain: workspace configuration, provider credentials, and agent sessions are not partitioned by OIDC subject. MCP Streamable HTTP transport sessions are a narrower security boundary: each opaque session is owner-bound to the issuer/subject-derived MCP principal so one authenticated subject cannot resume, append to, inspect, or delete another subject's transport session. This transport isolation does not create separate Waltr workspaces or application-data authorization domains.

## Modes And Configuration

`WALTR_AUTH_MODE` is exactly `disabled` or `oidc` and defaults to `disabled`.

- `disabled` admits a synthetic local principal. If any browser, API, MCP, or public-resource OIDC setting is present, startup fails instead of silently ignoring partial configuration.
- `oidc` requires valid configuration, successful provider discovery, an initial JWKS load, and initialized SQL persistence before the application is constructed. It never falls back to disabled mode.

| Variable | Required in OIDC mode | Contract |
| --- | --- | --- |
| `WALTR_AUTH_MODE` | Yes | Exactly `disabled` or `oidc`; omitted means `disabled`. |
| `WALTR_PUBLIC_URL` | Yes | Absolute public Waltr API resource URL. HTTPS is required except in explicit loopback mode; query, fragment, and userinfo are forbidden. |
| `WALTR_OIDC_ISSUER` | Yes | Absolute HTTP(S) issuer URL without query, fragment, or userinfo. Discovery must return this issuer exactly. |
| `WALTR_OIDC_CLIENT_ID` | Yes | Browser authorization-code client identifier. |
| `WALTR_OIDC_CLIENT_SECRET` | No | Confidential-client secret. When absent, provider metadata must support token endpoint auth method `none`. |
| `WALTR_OIDC_REDIRECT_URI` | Yes | Absolute registered HTTP(S) URL with exact path `/oidc/callback` and no query, fragment, or userinfo. |
| `WALTR_OIDC_AUDIENCE` | Fallback only | Legacy fallback when `WALTR_API_OIDC_AUDIENCE` is omitted. It does not configure browser ID-token validation. |
| `WALTR_OIDC_SCOPES` | Yes | Space-separated browser scopes; must include `openid`. |
| `WALTR_API_OIDC_ISSUER` | No | Native Waltr API bearer issuer; falls back to `WALTR_OIDC_ISSUER`. |
| `WALTR_API_OIDC_AUDIENCE` | Yes, unless legacy fallback is set | Native Waltr API bearer audience; falls back to `WALTR_OIDC_AUDIENCE`. |
| `WALTR_API_REQUIRED_SCOPES` | No | Space-separated native API bearer scopes; empty when omitted. |
| `WALTR_MCP_REQUIRED_SCOPES` | Yes | Non-empty space-separated scopes supported and required by the local MCP resource. |
| `WALTR_MCP_OAUTH_ISSUER` | No | Exact public deployment URL ending in `/oauth`; defaults to `WALTR_PUBLIC_URL` plus `/oauth`. |
| `WALTR_MCP_OAUTH_WRAPPING_KEYS_FILE` | Yes | Read-only JSON keyring used to encrypt MCP ES256 signing keys in the MCP database. |
| `WALTR_MCP_OAUTH_ACCESS_TOKEN_TTL` | No | Access-token seconds, 1 through 3,600; defaults to 300. |
| `WALTR_MCP_OAUTH_REFRESH_TOKEN_TTL` | No | Per-generation refresh-token seconds, 60 through 2,592,000; defaults to 86,400. |
| `WALTR_MCP_OAUTH_REFRESH_FAMILY_TTL` | No | Absolute refresh-family seconds, 60 through 31,536,000 and not shorter than the token TTL; defaults to 2,592,000. |
| `WALTR_MCP_OAUTH_CODE_TTL` | No | Authorization-code seconds, 1 through 600; defaults to 300. |
| `WALTR_MCP_OAUTH_ALLOW_DCR` | No | Exactly `true` or `false`; advertises and enables public Dynamic Client Registration when true. |
| `WALTR_MCP_OAUTH_ALLOW_CIMD` | No | Exactly `true` or `false`; enables hardened Client ID Metadata Document resolution when true. |
| `WALTR_AUTH_SESSION_TTL` | No | Browser session lifetime in seconds; defaults to 28,800 seconds and must be from 1 second through 30 days. |
| `WALTR_AUTH_ALLOW_INSECURE_COOKIE` | No | Exactly `true` or `false`; defaults to `false`. Allows non-`Secure` cookies only as part of loopback HTTP local mode. |
| `WALTR_OIDC_ALLOW_INSECURE_HTTP` | No | Exactly `true` or `false`; defaults to `false`. Allows HTTP discovery and OIDC endpoints only on loopback hosts. |

HTTP callback URLs and insecure cookies are accepted only when both insecure flags are `true` and the callback host is loopback. Otherwise the callback and discovered OIDC endpoints must use HTTPS and cookies use the `Secure` attribute.

## Public And Protected Surfaces

| Public | Protected in OIDC mode |
| --- | --- |
| `GET /health` | Embedded web assets and SPA routes; unauthenticated navigation redirects to `/oidc/login`. |
| `GET /oidc/login` | All native gRPC services via bearer token. |
| `GET /oidc/callback` | All gRPC-web services via the Waltr browser session cookie. |
| `POST /oidc/logout` | `/mcp` POST, GET, and DELETE via bearer token. |
| `GET /oidc/session` | |
| API-root RFC 9728 metadata, path-specific MCP metadata, local RFC 8414 metadata/JWKS, OAuth authorization/token/revocation/registration routes, and outbound callback/CIMD routes | |

API, gRPC, and MCP failures are protocol responses, not HTML redirects. Authentication runs before protected service handlers and before MCP session-store access.

## Browser Flow

1. The browser navigates to `/oidc/login?return_to=<local-path>`. The return value must be a local absolute path; protocol-relative paths, backslashes, control characters, and encoded bypasses are rejected.
2. The server rate-limits login admission, creates independent random state, browser correlation, nonce, and PKCE verifier values, and persists a ten-minute login attempt. Only state and correlation digests are stored for lookup; the PKCE verifier, nonce, and safe return path are persisted to complete the flow.
3. A short-lived `HttpOnly`, `SameSite=Lax`, path `/` correlation cookie binds the callback to the initiating browser. The authorization request uses code flow and PKCE S256.
4. The callback atomically consumes the unexpired attempt by state digest and correlation digest, exchanges the code with the exact redirect URI and verifier over the redirect-free bounded OIDC back channel, and validates ID-token signature, allowed asymmetric algorithm, exact issuer, client-ID audience, expiry, freshness, subject, nonce, and access-token hash when supplied.
5. The server creates a random opaque Waltr session token, stores only its SHA-256 digest with issuer, subject, timestamps, and expiry, and sends the token in an `HttpOnly`, `SameSite=Lax`, path `/` cookie. Secure deployments use the `__Host-waltr-session` cookie name and `Secure`; explicit local mode uses `waltr-session-local`.
6. Browser navigation and gRPC-web use only that same-origin cookie. The SPA never receives the OIDC client secret, code, provider tokens, or cookie value.

`GET /oidc/session` reports only whether the current browser session authenticates and is always `no-store`. Expired, revoked, unknown, or malformed sessions are unauthenticated.

ID tokens and access tokens have independent asymmetric signing-algorithm allowlists derived from provider metadata. An algorithm accepted for one token kind is not implicitly accepted for the other. Persisted browser sessions are bound to the configured issuer and browser client ID as well as subject; changing issuer or client ID invalidates and revokes old sessions.

Logout is `POST /oidc/logout`. In OIDC mode it requires exactly one valid `Origin` header whose scheme, host, and effective port exactly match `WALTR_PUBLIC_URL`; if `Sec-Fetch-Site` is present it must occur once and equal `same-origin`. The server revokes the persisted session before clearing the cookie. The web client calls logout with same-origin credentials and clears shared protected state only after a successful response.

## Native And TUI Flow

Native gRPC uses `Authorization: Bearer`, never the browser cookie. The TUI implements a public-client Authorization Code with PKCE loopback flow and uses these variables:

| Variable | Required | Contract |
| --- | --- | --- |
| `WALTR_OIDC_NATIVE_CLIENT_ID` | At login | Public native client registered at the provider; no client secret is used. |
| `WALTR_OIDC_NATIVE_SCOPES` | No | Space-separated requested scopes. If omitted, the TUI requests `openid` plus all resource-advertised scopes. If set, `openid` is added and all advertised required scopes must be included. |
| `WALTR_OIDC_NATIVE_AUDIENCE` | No | Adds an `audience` authorization parameter when the provider requires it. |
| `WALTR_OIDC_NATIVE_LOOPBACK_POLICY` | No | `ephemeral` by default, producing `http://127.0.0.1:<port>/callback`, or an exact `http://127.0.0.1:<nonzero-port>` URL without query or fragment. |

The TUI fetches only `/.well-known/oauth-protected-resource` from the configured server origin. It requires that document's resource to exactly match the normalized `WALTR_SERVER_ENDPOINT` scheme, host, effective port, and path before trusting its single advertised API authorization server. It sends the advertised API resource in the authorization request; it neither derives `/mcp` nor consumes MCP metadata. Server and provider metadata must use HTTPS, except HTTP on loopback for local development. Provider metadata must support code flow, PKCE S256, and public clients.

The TUI binds the loopback listener before generating state, nonce, and PKCE values; opens a browser when available; always displays a copyable URL; validates callback path, host, state, and duplicate parameters; and exchanges the code without a secret. Access and refresh tokens remain in process memory. A refresh token is used shortly before access-token expiry; without one, the client expires access at the declared lifetime. Missing usable expiry, failed refresh, cancellation, logout, or process exit clears credentials and requires login again.

Native provider metadata must advertise Authorization Code, PKCE `S256`, and token endpoint auth method `none`; the public native client never sends a client secret. HTTPS is required except for the single explicitly enabled loopback-HTTP mode described above.

`NativeCredentialLease` generations prevent a stale login or refresh task from replacing credentials installed by a newer flow. The transport reads the current credential for every newly sent unary request or opened stream, so reconnecting a watch receives the replacement token without rebuilding shared client state.

## MCP Flow

When OIDC mode is enabled, Authentik authenticates the human resource owner and the Waltr-hosted authorization server authorizes MCP access. Authentik never registers downstream MCP clients, grants Waltr MCP scopes, issues MCP refresh tokens, or issues a bearer accepted by `/mcp`.

The exact public surface, relative to an optional `WALTR_PUBLIC_URL` path prefix, is:

| Route | Contract |
| --- | --- |
| `GET /.well-known/oauth-protected-resource` | Independent native Waltr API resource metadata pointing at the API OIDC issuer. |
| `GET /.well-known/oauth-protected-resource/mcp` | MCP resource metadata pointing only at the local issuer. |
| `GET /.well-known/oauth-authorization-server/oauth` | RFC 8414 metadata for issuer `<public-url>/oauth`. |
| `GET /oauth/authorize` | Starts or resumes Authorization Code with exact redirect, resource, scopes, and PKCE S256. |
| `POST /oauth/authorize/decision` | Waltr consent approval or denial with browser-session and CSRF binding. |
| `GET /oauth/login` | Bridges an unauthenticated transaction through `/oidc/login` and back to its opaque transaction ID. |
| `POST /oauth/token` | Exchanges a one-time code or rotates a refresh token for a public client. |
| `POST /oauth/revoke` | Idempotently revokes an access token or refresh family for its public client. |
| `POST /oauth/register` | RFC 7591 public-client DCR, present only when enabled. |
| `GET /oauth/jwks.json` | Active and retained ES256 verification keys. |
| `GET /.well-known/oauth-client/waltr` | Waltr outbound public-client metadata used for CIMD. |
| `GET /outbound-oauth/callback` | Durable callback completion for Waltr acting as an outbound MCP client. |

Waltr configures the generic MCP Streamable HTTP authorization hooks as follows:

1. An unauthenticated `/mcp` request receives a bearer challenge that identifies Waltr's protected-resource metadata.
2. Path-specific RFC 9728 metadata identifies the exact public `/mcp` resource, advertises only the local `/oauth` issuer, and publishes `WALTR_MCP_REQUIRED_SCOPES`. The root document remains the independent API/Authentik domain.
3. The public client uses a preregistered client, a validated HTTPS CIMD client ID, or public DCR, then completes Authorization Code with PKCE S256 and Waltr consent.
4. Waltr verifies the ES256 `at+jwt`, exact issuer/resource audience, lifetime, subject, client, scopes, and durable token/grant/family/client authorization state before touching MCP session persistence. Direct Authentik access tokens are rejected.
5. Waltr derives the MCP session owner from a domain-separated SHA-256 of length-prefixed issuer and subject. Token bytes, scopes, expiry, and client secrets are excluded, so refresh and step-up tokens retain ownership while another subject receives HTTP `404` for the session.

Missing, invalid, expired, or revoked credentials return `401`; insufficient scopes return `403` with `insufficient_scope`; authorization-store failures fail closed and end active GET delivery. Expiry and same-process revocation terminate streams immediately at their observed boundary. Other replicas authoritatively recheck durable revocation at most every five seconds, plus scheduling and database latency. Waltr has no browser MCP client, so its Origin allowlist is empty: absent Origin is accepted and every present Origin is denied.

The cutover is intentionally clean: there is no direct-Authentik MCP validator, dual-issuer period, `WALTR_MCP_OIDC_*` compatibility surface, or special `self` client configuration. With access auth disabled, Waltr exposes neither the hosted OAuth routes nor an MCP challenge and keeps local `/mcp` unauthenticated. OIDC mode without an initialized hosted server fails `/mcp` closed.

## Shared Client Lifecycle

`waltr-client` keeps Waltr access in `AccessStatus`, independently of provider credential state:

- `UnknownChecking` permits the protected status probe.
- `LoginRequired`, `LoginInProgress`, and `LoginFailed` drive platform login UI.
- A successful protected status event for the current watch epoch establishes `Authenticated`.

Authentication failures use the versioned protobuf `AuthenticationFailure` status detail. Its domain is `WALTR_ACCESS`, `OUTBOUND_PROVIDER`, or `OUTBOUND_MCP`; its reason distinguishes required/rejected credentials, token exchange, network, invalid response, and missing configuration. Outbound failures include a typed `AuthTarget`; outbound MCP failures may additionally preserve the sanitized endpoint, HTTP status, resource-metadata URL, scope, bearer error fields, and realm from the structured challenge.

The shared client does not infer login from an ambiguous status code or missing target. Only `Unauthenticated` with a valid version-1 detail whose domain is explicitly `WALTR_ACCESS` triggers Waltr relogin and native bearer clearing. `OUTBOUND_PROVIDER` or `OUTBOUND_MCP` with a valid target and credential-required/rejected reason enters credential authorization; malformed, unknown, or untyped failures remain ordinary operation errors. Expiry invalidates the current status-watch epoch, clears protected snapshots and active streams, preserves watch interests and reload intent, and suppresses protected reconnects. Login completion starts a new status-watch epoch; only current-epoch events can authenticate, after which config/session watches reopen and authoritative collections reload. Explicit logout additionally drops watch interests and provider-auth pending state.

In-process browser-session revocation cancels registered API streams immediately after the durable revoke commits. Other processes revalidate browser leases at most once per second. Hosted MCP bearer streams stop at JWT `exp`, local revocation notification, or the five-second durable authorization recheck; these bounds do not depend on advisory cleanup.

## Persistence And Startup

OIDC migration V2 adds backend-equivalent login-attempt and browser-session tables to Waltr-owned persistence. Admission limits are enforced in the database across server instances: 1,024 active login attempts, 1,024 active browser sessions globally, and 32 active sessions per issuer/subject. Callback consumption is atomic, session tokens are stored only as digests, logout records revocation, and bounded opportunistic cleanup removes inactive records without being required for correctness.

OIDC back-channel requests do not follow redirects. Deployments behind a reverse proxy may configure `WALTR_TRUSTED_PROXIES` with exact CIDRs; only peers in that allowlist may supply forwarded client addresses, and the proxy must overwrite untrusted forwarding headers.

Environment-owned `build_app()` is intentionally async and fallible because database migrations, OIDC discovery, and initial key loading precede listener binding. Explicit app builders are synchronous composition APIs and require initialized `Persistence`; builders that enable access authentication also require an initialized `AccessAuthenticator`.

See [Server Persistence](persistence.md), [Clients](clients.md), and [Deployment](../deployment.md).
