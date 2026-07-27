# MCP OAuth Authorization Server Execution Plans

Status: In Progress

These plans replace direct Authentik-issued MCP bearer tokens with a Waltr-hosted OAuth authorization server. Authentik remains the upstream identity provider that authenticates the resource owner. The generic `mcp` crate owns downstream OAuth protocol behavior, client registration, grants, token lifecycle, revocation, signing-key lifecycle, and persistence contracts.

Completed workstreams are implementation records; incomplete workstreams still describe target verification or operations. Use the canonical architecture and protocol documentation for current behavior.

## Objective

Provide a standards-based MCP authorization flow in which:

1. An MCP client discovers Waltr's `/mcp` protected-resource metadata.
2. The resource metadata identifies Waltr, not Authentik, as the OAuth authorization server.
3. The client uses preregistered information, a Client ID Metadata Document, or Dynamic Client Registration.
4. Waltr authenticates the resource owner through the existing Authentik-backed browser session.
5. Waltr obtains explicit consent for the client, resource, and scopes.
6. Waltr issues its own resource-bound access and refresh tokens.
7. The generic MCP server validates, refreshes, rotates, revokes, and leases those credentials.
8. Waltr acting as an outbound MCP client uses the same discovery and registration mechanisms without a `self` branch.

```text
MCP client
    |
    | RFC 9728 resource discovery
    v
Waltr /mcp
    |
    | Waltr OAuth issuer
    v
Waltr MCP OAuth authorization server
    |
    | existing Waltr browser login
    v
Authentik
    |
    | authenticated issuer + subject
    v
Waltr consent, code, access token, and refresh token
```

## Fixed Decisions

| Concern | Decision |
| --- | --- |
| Authorization-server owner | Generic `mcp` crate. |
| Upstream authentication | Reuse the existing Waltr browser session established through Authentik. |
| Access tokens | Short-lived ES256 JWTs with `typ=at+jwt`. |
| Refresh tokens | Opaque, rotating, digest-only persistence with family replay detection. |
| Signing-key custody | MCP-generated ES256 keys encrypted in SQL by a deployment-supplied wrapping-key ring. |
| Revocation | Token, grant, client, and family revocation terminates active MCP streams. |
| Client registration | Preregistered public clients, Client ID Metadata Documents, and RFC 7591 Dynamic Client Registration. |
| Consent | Waltr-owned consent, remembered only for the exact principal, client, resource, and scope set. |
| Rollout | Clean cutover; no direct-Authentik MCP token compatibility path. |
| Waltr `self` client | No special case and no deployment-owned `WALTR_MCP_SELF_OAUTH_*` variables. |

## Boundaries

| Layer | Owns | Must not own |
| --- | --- | --- |
| `mcp` | OAuth wire types, discovery, authorization server, registration, grants, tokens, revocation, key lifecycle, storage traits, and Streamable HTTP authorization leases. | Authentik, Waltr cookies, Waltr UI, Waltr configuration, or Waltr database types. |
| `waltr-server` | Browser-principal adapter, consent presentation, route composition, policy configuration, store selection, wrapping-key loading, lifecycle tasks, and deployment integration. | Generic OAuth parsing, token-family state transitions, or server-key-specific outbound registration logic. |
| `waltr-agent` | Target-scoped outbound MCP credential state and supplying current credentials to MCP transports. | Authorization-server policy or duplicated discovery/registration protocol logic. |
| `waltr-client` | Shared outbound authorization-attempt state and commands. | Token values, PKCE verifiers, registration secrets, or provider-specific protocol behavior. |
| Web and TUI | Authorization and consent presentation appropriate to each client. | Independent OAuth state machines or direct server transports. |
| Authentik | User authentication, MFA, and identity lifecycle. | MCP client registration, Waltr scopes, downstream refresh tokens, or bearer tokens accepted by `/mcp`. |

## Ground Rules

- Preserve existing protected-resource and external-validator APIs for generic `mcp` consumers while adding the hosted authorization-server path.
- Keep OAuth authorization state separate from Streamable HTTP transport-session state, even when both use one MCP-owned SQL backend.
- Require Authorization Code, PKCE `S256`, exact redirect matching, exact resource binding, and public-client token authentication method `none` in the first release.
- Support only public downstream MCP clients initially. Confidential clients and RFC 7592 registration management are later work.
- Never persist plaintext authorization codes, refresh tokens, registration tokens, or wrapping keys.
- Never place tokens, authorization codes, client metadata response bodies, or private keys in logs, tracing attributes, errors, snapshots, or status RPCs.
- Treat CIMD documents and DCR display metadata as untrusted input.
- Use no redirects, bounded bodies, bounded timeouts, and explicit transport policy for discovery, CIMD, registration, and token requests.
- Keep `WALTR_AUTH_MODE=disabled` behavior unauthenticated; the hosted authorization server is enabled only when OIDC access authentication is enabled.
- Do not add legacy dual validation because no direct-Authentik MCP consumers or durable credentials require migration.
- Every workstream leaves its affected crates compiling and its focused tests passing.

## Workstreams

| Order | Status | Plan | Covers |
| --- | --- | --- | --- |
| 1 | Complete | [OAuth Protocol And Discovery](01-protocol-discovery.md) | Typed OAuth contracts, module boundaries, metadata validation, secure discovery, and compatibility surface. |
| 2 | Complete | [OAuth Persistence And Signing Keys](02-persistence-signing.md) | Store contract, SQL migrations, encrypted ES256 key custody, rotation, and backend conformance. |
| 3 | Complete | [Authorization, Tokens, And Revocation](03-authorization-tokens.md) | Authorization, token, refresh, revocation, lease, and signing-overlap invariants pass across the supported stores. |
| 4 | Complete | [Client Registration](04-client-registration.md) | Preregistration, CIMD, DCR, SSRF controls, quotas, and generic client helpers. |
| 5 | Complete | [Waltr Server Integration](05-waltr-server-integration.md) | Existing browser-session authentication, consent UI, router composition, configuration, principal mapping, and clean cutover. |
| 6 | Complete | [Outbound MCP Client](06-outbound-client.md) | Standard registration selection, durable credentials, refresh serialization, dynamic bearer supply, callback completion, and removal of `self`. |
| 7 | In Progress | [Deployment And Operations](07-deployment-operations.md) | Pulumi/Authentik changes, key deployment, readiness, policy tests, and recovery procedures are implemented; live multi-replica, failover, restore, and rotation exercises remain. |
| 8 | In Progress | [Integration, Verification, And Documentation](08-verification-documentation.md) | Canonical documentation, aggregate gates, PostgreSQL conformance, and full hosted browser OAuth pass; live operational, independent-client OAuth, and load exercises remain. |

## Dependency Order

Workstream 1 defines contracts consumed by every later workstream. Workstream 2 provides durable atomic state and signing keys. Workstream 3 builds the authorization server on those foundations. Workstream 4 extends the working server with standards-based registration. Workstream 5 composes the generic server into Waltr. Workstream 6 then migrates Waltr's outbound MCP client to the same generic protocol primitives. Workstream 7 changes deployed identity and secret wiring only after the server and client paths work in tests. Workstream 8 closes cross-layer coverage and canonical documentation.

Within that sequence, in-memory protocol work in workstream 3 may proceed alongside SQL backend completion in workstream 2, but no Waltr integration should begin until the store contract and security invariants are stable.

## Global Acceptance

- `/mcp` advertises a Waltr-owned OAuth issuer and never accepts an Authentik-issued MCP access token.
- Authentik authenticates the resource owner without becoming the downstream token issuer.
- A standards-compatible public client can register through DCR, authorize with PKCE, refresh, revoke, and call `/mcp` without administrator-created client credentials.
- A CIMD client can authorize without DCR when its metadata document is valid and reachable under production transport policy.
- Access tokens are exact-resource and exact-scope bound ES256 JWTs.
- Refresh tokens rotate atomically; replay revokes the family and all related access leases.
- Revoking a token, grant, family, or client ends active Streamable HTTP GET delivery across replicas within the documented bound.
- Signing keys rotate without invalidating unexpired access tokens and survive restart and replica changes.
- Waltr's outbound MCP flow discovers resource and authorization metadata before registration selection and has no `self` behavior.
- No `WALTR_MCP_SELF_OAUTH_*`, direct MCP Authentik verifier, or Authentik MCP audience/scope mapping remains.
- Memory, SQLite, and PostgreSQL stores pass one behavioral contract.
- Full Rust, web, Compose, Pulumi, browser-integration, formatting, lint, and generated-code gates pass.

## References

- [MCP Authorization](https://modelcontextprotocol.io/specification/2025-11-25/basic/authorization)
- [RFC 9728: OAuth 2.0 Protected Resource Metadata](https://www.rfc-editor.org/rfc/rfc9728)
- [RFC 8414: OAuth 2.0 Authorization Server Metadata](https://www.rfc-editor.org/rfc/rfc8414)
- [RFC 7591: OAuth 2.0 Dynamic Client Registration](https://www.rfc-editor.org/rfc/rfc7591)
- [OAuth Client ID Metadata Document](https://datatracker.ietf.org/doc/draft-ietf-oauth-client-id-metadata-document/)
- [RFC 7636: Proof Key for Code Exchange](https://www.rfc-editor.org/rfc/rfc7636)
- [RFC 8707: Resource Indicators for OAuth 2.0](https://www.rfc-editor.org/rfc/rfc8707)
- [RFC 7009: OAuth 2.0 Token Revocation](https://www.rfc-editor.org/rfc/rfc7009)
- [RFC 9068: JWT Profile for OAuth 2.0 Access Tokens](https://www.rfc-editor.org/rfc/rfc9068)
- [RFC 9700: Best Current Practice for OAuth 2.0 Security](https://www.rfc-editor.org/rfc/rfc9700)
- [Access Authentication](../../architecture/access-authentication.md)
- [MCP Protocol](../../mcp/protocol.md)
- [Deployment](../../deployment.md)
- [Testing](../../testing.md)
