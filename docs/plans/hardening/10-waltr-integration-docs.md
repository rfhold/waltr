# 10 Waltr Integration And Docs

Status: Complete

Adapt Waltr to the new MCP contract, complete layered auth coverage, exercise real browser logout, and correct all documentation.

## Changes

### Waltr MCP integration

- `access_auth/principal.rs`: derive `McpPrincipalId` from a domain-separated SHA-256 over length-prefixed issuer + subject (exclude token bytes, scopes, expiry, client secret) so refresh/step-up retains the session.
- `app.rs`: return `McpTokenValidation::Authorized { principal_id, expires_at }`; keep the generic default-deny Origin policy (Waltr has no browser MCP client); wire `StreamableHttpSessionStoreConfig` defaults; schedule bounded periodic `cleanup_expired`.
- Update `waltr-agent-test-mcp-server` (static + JWT modes) to return principal IDs; JWT fixture validates and derives from `sub`.
- `waltr-agent/src/mcp/server.rs` and other shutdown callers handle fallible `shutdown()`.
- mcp-macros tests/examples updated for principal-bearing authorization results.

### Layered tests

- `waltr-server/tests/access_auth.rs`: two valid OIDC subjects; subject B cannot inspect/append/deliver/delete subject A's MCP session (HTTP 404 at the boundary); distinct browser client ID vs API audience fixtures where bearer behavior is exercised.
- `waltr-server/tests/config/mcp_server.rs` + persistence tests updated for the new store wiring.

### Browser logout test (`tests/web/k6/access/login.browser.js`)

- After login: assert `__Host-waltr-session` cookie attributes (`Secure`, `HttpOnly`, `SameSite=Lax`, path `/`).
- Start a response waiter for same-origin `POST /oidc/logout`, click the real `button.shell__logout`, assert 204, signed-out route (`#access-title`), navigation/Sign-out removal, `/oidc/session` reports unauthenticated, cookie absent.
- Use a fresh browser context for any login-after-logout assertion (Waltr logout does not end provider SSO).

### Documentation (use the documenting skill)

- `README.md` + `docs/deployment.md`: Compose remains static local development/test infrastructure, while the Dockerfile's nonroot `runtime` target supports production deployment through the repository's Pulumi workload.
- `docs/testing.md`: tmpfs isolation, internal-only networking, health-gated startup, teardown/exit-code behavior, TLS fixture with real verification, real logout click, skipped Dex consent.
- `docs/architecture/access-authentication.md`: redirect-free back channel, explicit S256/public `none`, split token algorithm policy, issuer+client session binding, stream revocation bounds (immediate in-process, <=1s cross-process, JWT `exp`), single insecure loopback mode, advisory cleanup, trusted proxies.
- `docs/architecture/clients.md` + `crates/waltr-client/AGENTS.md`: access generations, cancellation-safe in-flight effects, denied-state parking, anonymous logout reprobe, catalog reducer ownership, native TLS.
- `crates/waltr-tui/AGENTS.md`: transient route restoration vs explicit logout clearing.
- `docs/mcp/protocol.md` + `docs/plans/mcp-session-persistence.md`: acknowledgment-based at-least-once delivery, one resumable GET outbox, non-resumable POST SSE, owner binding, TTL/quotas, default-deny Origin, fallible shutdown; correct any claim that GET drains events resumably.
- `docs/architecture/persistence.md`: dev volume vs test tmpfs; nested `file:` URI rejection.
- `tests/web/k6/AGENTS.md`: add `access/` and `oauth/` rows; static fixture assumptions; require real UI interactions for browser-owned actions.
- `CONTRIBUTING.md`: dev profile command, non-production Compose warning, new focused test commands.

### Reviewer follow-up

- `catalog/mod.rs`: publish one generation-scoped outcome for concurrent cold and stale refreshes, including shared failures and cancellation-safe waiter release; only later callers retry.
- Add deterministic eight-caller catalog stress regressions.

## Exit Criteria

- Cross-subject MCP isolation proven at the HTTP boundary.
- Browser logout exercises the real UI path end to end.
- No document claims Compose deployability or destructive-yet-resumable GET semantics.

## Verification

```
cargo test -p waltr-server --tests
cargo test -p waltr-server --lib catalog::loader_tests
cargo clippy -p mcp -p waltr-server -p waltr-client -p waltr-tui --all-targets -- -D warnings
pnpm test:web:integration:auth
pnpm test:web:integration
```
