# 08 Integration, Verification, And Documentation

Status: In Progress

## Objective

Prove the complete Authentik-to-Waltr-to-MCP authorization system at every layer, verify security and failure invariants under concurrency and restart, and replace target-state plans with accurate canonical documentation once implementation is complete.

The canonical documentation and locally executable verification are complete. Generic MCP, memory/SQLite/PostgreSQL stores, Waltr server, outbound client, agent, shared client, TUI, web, Compose, Pulumi shape/policy, protobuf, aggregate workspace, pipeline-definition, and full unauthenticated/authenticated browser checks passed. The hosted browser path exercises Waltr-to-Waltr CIMD, consent, durable callback completion, persisted credential reuse, dynamic bearer delivery, and protected resource discovery. These are deterministic, container-backed, or browser checks, not evidence of a live Pulumi/Tekton deployment. This workstream remains in progress for independent third-party client OAuth interoperability, live multi-replica operational/failover/restore exercises, and performance/load measurements.

## Verification Record

| Result | Coverage |
| --- | --- |
| Passed | Deterministic generic MCP and Waltr suites; memory and SQLite OAuth contracts; container-backed PostgreSQL OAuth/session and Waltr persistence contracts. |
| Passed | Web/TUI/component checks and complete unauthenticated/authenticated Compose browser phases, including the Waltr-to-Waltr hosted OAuth path. |
| Passed | Pulumi TypeScript build and mock/policy tests, Compose contract tests, release/preview pipeline-definition checks, aggregate formatting/lint/build/test gates, and generated protobuf checks. |
| Not run | Live Pulumi preview/up or Tekton preview/release deployment. |
| Not run | Live multi-replica signing rotation/revocation, database failover, backup restoration, and rollback. |
| Not run | Independent third-party MCP client hosted-OAuth interoperability. Existing ecosystem tests do not establish this claim. |
| Not run | Authorization/token load, performance, capacity, and revocation fan-out measurement. |

No production deployment or successful backup restoration is claimed.

## Test Layers

### Generic MCP Unit And Spec Tests

Extend `crates/mcp` coverage for:

- OAuth request/response parsing and standard errors.
- RFC 9728 and RFC 8414 discovery.
- Authorization Code with PKCE `S256`.
- Exact redirect, resource, and scope enforcement.
- DCR and CIMD behavior.
- ES256 JWT issue and validation policy.
- Refresh rotation and family replay.
- Revocation and active authorization leases.
- Signing and wrapping-key rotation.
- Existing bearer-validator compatibility.

Keep MCP revision-scoped protocol tests under `tests/spec/v2025_11_25/`. Put backend behavioral parity in dedicated OAuth store targets rather than hiding it in server integration tests.

### Store Conformance

Run the same cases against memory, SQLite, and PostgreSQL:

- One-time code redemption.
- Concurrent refresh rotation.
- Replay-family revocation.
- Client/grant/token isolation.
- Admission quotas.
- Signing-key activation and overlap.
- Wrapping-key re-encryption.
- Cleanup and reopen persistence.
- Migration checksums and existing database upgrade.

PostgreSQL concurrency cases remain ignored only when they require an external container; CI and PAC must execute them in a provisioned phase.

### Waltr Server Integration

Add `crates/waltr-server/tests/mcp_oauth_server.rs` or equivalent coverage for:

- Existing Authentik browser login followed by OAuth transaction resumption.
- Consent approval, denial, remembered exact scope set, changed scope set, and CSRF.
- Local metadata and JWKS.
- DCR and CIMD client authorization.
- Code, access, refresh, and revocation lifecycle.
- Waltr-issued token use against `/mcp`.
- Direct Authentik token rejection.
- Stable owner identity across refresh and signing rotation.
- Subject isolation for existing MCP sessions.
- Authentik outage behavior.
- Startup, readiness, shutdown, and maintenance tasks.
- Restart and multi-replica behavior.

### Outbound Agent And Shared Client

Cover:

- Discovery-before-registration selection.
- Preregistered, CIMD, and DCR paths.
- Durable registration reuse.
- Refresh metadata preservation and singleflight.
- Dynamic bearer replacement on active transports.
- Server callback and manual completion.
- Typed failures remaining in the outbound MCP auth domain.
- No Waltr access logout on external MCP credential failure.
- No server-key-specific behavior.

### Web And TUI

Web tests cover shared authorization-attempt state, popup/callback completion, manual fallback, denial, expiry, and sanitized errors. TUI tests cover copied URL, manual completion, retry, cancellation, and target labeling.

Consent page tests cover accessible labels, untrusted text escaping, exact scopes/resource display, POST-only decisions, and CSRF failure.

Neither UI test should access a direct gRPC transport or token value.

### Browser Integration

Keep Dex as the Authentik-like upstream identity fixture. Change the protected-MCP scenario to this topology:

```text
browser or MCP client
    -> Waltr OAuth authorization
    -> Dex login
    -> Waltr consent
    -> Waltr token
    -> Waltr /mcp
```

Cover:

- Login-required authorization and return to the exact stored transaction.
- Consent approve and deny.
- Invalid client and attacker redirect rejection.
- Missing/wrong resource and excessive scope.
- Missing/plain/wrong PKCE.
- Code replay and expiry.
- DCR and CIMD clients.
- Refresh rotation and replay-family revocation.
- Active stream termination after revocation.
- Signing-key rotation overlap.
- Waltr outbound MCP authorization using the hosted server.
- Full teardown with no leaked containers, volumes, networks, or temporary roots.

### Deployment Tests

Add Pulumi mock/policy coverage for:

- No Authentik MCP token issuer.
- Secret wrapping-key propagation.
- Read-only secret volume and runtime permissions.
- Local issuer/resource settings.
- Nonsecret outputs only.
- Existing workload hardening.
- Preview/production stack differences limited to intended names, hosts, and capacities.

Keep Compose configuration tests exact and deterministic.

## Security Verification

Explicitly test:

- Duplicate OAuth parameters.
- Oversized form and JSON bodies.
- Redirect URI parsing edge cases.
- Unicode and control characters in client display metadata.
- No redirects during metadata, CIMD, registration, token, revocation, or JWKS requests.
- CIMD private-address, link-local, metadata-service, DNS-rebinding, and redirect attacks.
- Authorization code and refresh-token timing-safe digest checks.
- Unknown JWT `kid`, wrong `alg`, missing/wrong `typ`, wrong audience, and expired token.
- Concurrent code redemption and refresh.
- Revocation races with POST, GET, DELETE, and refresh.
- Signing-key and wrapping-key corruption.
- Database outage before and after mutation commit.
- No secrets in `Debug`, logs, traces, metrics, gRPC details, snapshots, SQL fixtures, Pulumi outputs, or container layers.

Use property or fuzz tests for challenge parsing, OAuth form parsing, redirect URI validation, scope normalization, and metadata decoding where they add meaningful malformed-input coverage.

## Interoperability

Add ecosystem tests using independent MCP clients when available. Verify:

- RFC 9728 path-specific metadata discovery.
- RFC 8414 path-bearing issuer discovery.
- DCR request compatibility.
- CIMD client ID handling.
- Resource parameter propagation.
- PKCE and token exchange.
- Refresh and revocation.

Keep protocol simulations for unsupported SDK features, but do not claim ecosystem interoperability from self-client tests alone.

## Performance And Capacity

Measure and bound:

- Authorization and token request body size.
- Active transaction, client, consent, token, and lease counts.
- JWT validation plus revocation-store lookup latency.
- Revocation fan-out latency across replicas.
- Signing-key rotation lock duration.
- Cleanup batch size and database impact.
- CIMD fetch concurrency and cache behavior.

Add load tests for token validation and refresh rotation before increasing deployment replica counts.

## Canonical Documentation

After behavior is implemented, update:

| Document | Required update |
| --- | --- |
| `docs/architecture/access-authentication.md` | Authentik authentication versus Waltr authorization/token issuance, consent, principal binding, and route ownership. |
| `docs/architecture/persistence.md` | OAuth schema ownership, encrypted keys, backup unit, cleanup, and multi-replica coordination. |
| `docs/mcp/protocol.md` | Generic OAuth authorization-server, DCR/CIMD, token, revocation, lease, and store contracts. |
| `docs/agent/mcp.md` | Outbound discovery, registration selection, durable state, callback modes, refresh, and revocation. |
| `docs/deployment.md` | Configuration, Pulumi topology, secrets, lifetimes, egress, backups, readiness, and rotation. |
| `docs/testing.md` | New test targets, container requirements, browser phases, and security checks. |
| `CONTRIBUTING.md` | Concise commands for the new focused gates. |
| Relevant `AGENTS.md` files | Updated boundaries, paths, and test guidance. |

Maintain `docs/runbooks/mcp-oauth-key-rotation-recovery.md` and link it from deployment documentation. Remove statements that Waltr is not an authorization server or that direct Authentik MCP tokens are accepted.

Plans remain implementation records. Mark each workstream complete only after its exit criteria and focused verification pass. Update the plan README and canonical docs in the same final documentation change.

## Verification Order

Run the repository's focused and aggregate gates, including:

1. MCP protocol and authorization-server unit/spec tests.
2. MCP memory and SQLite OAuth store conformance.
3. Container-backed PostgreSQL OAuth store conformance.
4. Existing MCP session-store and ecosystem tests.
5. Waltr server library and integration tests.
6. Agent library and deterministic MCP integration tests.
7. Shared-client tests.
8. TUI tests.
9. Web tests and build.
10. Compose contract and profile validation.
11. Full authenticated and unauthenticated browser integration.
12. Pulumi TypeScript build and policy/mock tests.
13. Production Docker build and runtime inspection.
14. Buf lint and generation when protobuf changes occur.
15. `cargo fmt --all -- --check`.
16. `cargo clippy -p mcp -p waltr-server -p waltr-client -p waltr-tui --all-targets -- -D warnings`.
17. `cargo clippy -p waltr-agent --features shell --all-targets -- -D warnings`.
18. MCP Clippy with every new authorization-server and SQL feature combination enabled.
19. `git diff --check`.

Record exact commands in `docs/testing.md` once feature names and test targets exist. Do not place speculative commands in `CONTRIBUTING.md` before they work.

## Final Acceptance

- Authentik authenticates users but never issues the token accepted by `/mcp`.
- Waltr advertises a local OAuth issuer and supports preregistration, CIMD, and DCR.
- Authorization Code with PKCE, consent, resource/scope binding, refresh rotation, revocation, and key rotation work end to end.
- Revocation terminates active streams across replicas within the documented bound.
- Waltr outbound MCP uses discovery and registration without `self` handling.
- All persisted security state survives restart and respects backend parity.
- Pulumi is the only production infrastructure/workload definition.
- Canonical docs describe implemented behavior without relying on these plans.
- Every verification gate passes from a clean test environment.

## Dependencies

This is the final workstream and depends on all previous plans.
