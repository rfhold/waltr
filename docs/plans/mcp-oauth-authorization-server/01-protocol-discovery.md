# 01 OAuth Protocol And Discovery

Status: Complete

## Objective

Establish provider-neutral OAuth protocol types and hardened discovery primitives in `crates/mcp` before adding authorization-server state or Waltr integration.

## Baseline

`crates/mcp/src/oauth.rs` currently provides RFC 9728 protected-resource metadata, partial authorization-server metadata, bearer challenge parsing, and resource/authorization-server discovery. It models `registration_endpoint` and `client_id_metadata_document_supported` but does not use them. It does not provide authorization, token, revocation, DCR, CIMD, or hosted authorization-server contracts.

The existing discovery implementation validates exact resource and issuer strings but does not yet enforce the complete endpoint, transport, body-size, redirect, PKCE, grant, and public-client capability policy needed by a hosted server and automated outbound client.

## Scope

- Split `oauth.rs` into focused modules while preserving current public exports.
- Complete authorization-server metadata needed by MCP clients and the hosted server.
- Add typed requests, responses, and errors for authorization, token, revocation, DCR, and CIMD.
- Add strict metadata and endpoint validation for public Authorization Code clients.
- Add a redirect-disabled, bounded discovery transport abstraction.
- Preserve unknown extension fields only on metadata documents where interoperability requires them.
- Define deterministic clock and entropy seams needed by later state-machine tests.
- Introduce optional Cargo features for hosted OAuth behavior without forcing crypto or SQL dependencies on protocol-only consumers.

## Module Layout

Start with this structure and collapse files only when types remain genuinely small:

```text
crates/mcp/src/oauth/
├── mod.rs
├── challenge.rs
├── discovery.rs
├── metadata.rs
├── protocol.rs
├── client/
│   └── mod.rs
└── server/
    └── mod.rs
```

`oauth/mod.rs` re-exports the existing names so current imports continue compiling. Waltr-specific selection policy must not enter these modules.

## Protocol Types

Add typed models for:

| Area | Required types |
| --- | --- |
| Authorization | Request, successful response parameters, standard error parameters, response type, and PKCE challenge method. |
| Token | Authorization-code request, refresh-token request, public-client identity, token response, and token endpoint error. |
| Revocation | Revocation request, token type hint, and standard idempotent response behavior. |
| DCR | Public-client registration request, registration response, grant types, response types, and token endpoint auth method. |
| CIMD | Client metadata document, exact client ID, redirect URIs, grant types, response types, client name, and extension fields. |
| JWT | RFC 9068 access-token claims and JOSE header policy. |

Security-sensitive standard fields must be typed rather than passed through `serde_json::Value`. Metadata extension maps may remain where standards require forward-compatible unknown fields.

## Authorization-Server Metadata

Extend `McpAuthorizationServerMetadata` with the fields required to advertise and consume:

- Authorization and token endpoints.
- Revocation endpoint.
- Registration endpoint.
- Grant types.
- Response types.
- PKCE challenge methods.
- Token endpoint authentication methods.
- Client ID Metadata Document support.
- JWKS URI.
- Access-token signing algorithms.
- Authorization response issuer support where applicable.

Add a validation method for an MCP public Authorization Code client. It must verify:

- Exact issuer equality.
- Valid HTTPS endpoint URLs, except explicit loopback development policy.
- Authorization Code grant and `code` response support.
- PKCE `S256` support.
- Public-client token endpoint authentication method `none`.
- A usable token endpoint.
- Internally consistent registration and CIMD capability advertisement.
- No endpoint fragments, embedded credentials, or disallowed schemes.

An omitted metadata field may receive a standards-defined default only when the relevant RFC defines that default. Compatibility guesses must be explicit and tested.

## Discovery Transport

Introduce a reusable HTTP policy used by protected-resource, authorization-server, CIMD, DCR, token, and revocation calls:

- Redirect following disabled.
- Bounded connect and response timeout.
- Bounded response bytes before JSON decoding.
- Content-type validation with narrowly documented compatibility behavior.
- HTTPS required except explicit loopback HTTP development policy.
- Sanitized errors without response bodies or secrets.
- Caller-supplied destination policy for CIMD SSRF enforcement.

Resource discovery retains current ordering:

1. Challenge-provided `resource_metadata`, when present.
2. Path-specific RFC 9728 metadata.
3. Root RFC 9728 metadata.

Authorization-server discovery retains RFC 8414 first and OpenID Provider metadata fallback for upstream servers that expose OAuth through OIDC discovery. Waltr's hosted authorization server will publish RFC 8414 metadata and will not claim to be an OpenID Provider.

## Public API Compatibility

Preserve:

- `McpProtectedResourceMetadata`
- `McpAuthorizationServerMetadata`
- `McpBearerChallenge`
- `McpAccessToken`
- `McpPrincipalId`
- `McpTokenValidation`
- Existing discovery function names where their contracts remain safe
- Existing `StreamableHttpAuthorization::new` behavior

Prefer additive constructors and builders over making downstream callers initialize new public struct fields. If a type must become `#[non_exhaustive]`, document that compatibility decision before changing it.

## Cargo Features

Add narrowly scoped features such as:

```text
oauth-authorization-server
oauth-authorization-server-sqlite
oauth-authorization-server-postgres
```

The protocol metadata and client discovery surface should remain available without SQL. Crypto dependencies used only for issuance should remain behind the authorization-server feature.

## Tests

Extend `crates/mcp/tests/spec/v2025_11_25/basic/authorization.rs` and focused module tests to cover:

- Metadata serialization and extension preservation.
- Successful RFC 9728 and RFC 8414 discovery.
- Path-bearing issuer discovery locations.
- Exact resource and issuer mismatch rejection.
- Redirect refusal.
- HTTPS and loopback policy.
- Bounded and malformed responses.
- Authorization Code, PKCE, and public-client capability validation.
- Duplicate and malformed OAuth parameters.
- Secret-safe `Debug` and error output.
- Current bearer challenge behavior remaining unchanged.

## Exit Criteria

- The existing MCP authorization and transport tests pass unchanged.
- The new types can represent every request and response needed by later plans without Waltr-specific fields.
- Discovery fails closed before any authorization code, verifier, refresh token, or registration credential is sent to an unvalidated endpoint.
- `mcp` protocol-only consumers do not acquire SQL dependencies.
- Current public resource-server composition remains available.

## Dependencies

This is the first workstream. It enables every later plan.
