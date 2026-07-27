# 04 Client Registration

Status: Complete

## Objective

Allow standards-compatible public MCP clients to use Waltr without a deployment-created client ID by supporting Client ID Metadata Documents and RFC 7591 Dynamic Client Registration, while retaining generic preregistration for controlled integrations.

## Registration Resolution

The hosted authorization server resolves a submitted client ID as follows:

1. Look up an active preregistered or dynamically registered client.
2. If the client ID is an eligible HTTPS URL and CIMD is enabled, resolve and validate its metadata document.
3. Reject unknown opaque client IDs; DCR clients must register before authorization.

Waltr acting as an outbound client uses a related but distinct selection order defined in workstream 6: reuse persisted registration, configured preregistration, CIMD, then DCR.

## Public Clients Only

The first implementation accepts only:

```text
token_endpoint_auth_method = none
response_types = ["code"]
grant_types = ["authorization_code", "refresh_token"]
code_challenge_methods = ["S256"]
```

Do not issue or accept downstream client secrets. Confidential clients, signed client assertions, software statements, and RFC 7592 registration management are phase-local non-goals.

## Client ID Metadata Documents

Advertise:

```json
{
  "client_id_metadata_document_supported": true
}
```

For a URL-shaped client ID:

- Require HTTPS in production.
- Require the document's `client_id` to equal the requested URL exactly.
- Require exact redirect URI membership.
- Validate public-client auth method, response types, grant types, and PKCE behavior.
- Treat client name and links as untrusted display data.
- Bind authorization transactions to an immutable, versioned snapshot and normalized document digest used for validation.
- Cache successful documents for a bounded duration based on safe response metadata and configured maxima.
- Fail closed when no unexpired validated snapshot exists and refresh fails.

Retain every referenced snapshot through the authorization transaction and code lifetime. Code exchange uses the exact snapshot selected at authorization time and does not silently switch to a newly fetched document. New authorization requests use the current validated snapshot. Cleanup removes an old snapshot only after its cache expiry and every referencing transaction/code retention deadline.

The authorization server does not fetch client logos or arbitrary display URLs. Only the client ID metadata document itself is fetched.

## CIMD Network Policy

CIMD introduces server-side request forgery risk. Add a destination-policy interface and production implementation that:

- Resolves DNS before connecting.
- Rejects loopback, private, link-local, multicast, unspecified, and metadata-service addresses.
- Pins each connection to an address from the approved resolution set and verifies the connected peer. If the HTTP stack cannot expose or pin the peer, it is not suitable for production CIMD fetching.
- Refuses redirects.
- Limits DNS results, connect time, total time, headers, and body bytes.
- Requires an expected JSON media type.
- Does not forward cookies, authorization headers, or Waltr request headers.
- Records only sanitized hostname and outcome metrics.

Explicit test policy may permit loopback HTTP for deterministic integration fixtures. Production policy must not infer exceptions from hostname strings alone.

## Dynamic Client Registration

Serve:

```text
POST /oauth/register
```

Implement RFC 7591 registration for public clients:

- Require one or more redirect URIs.
- Reject fragments, embedded credentials, wildcard hosts, and unsupported schemes.
- Require exact redirects during authorization and code exchange.
- Permit native loopback redirects only under an explicit policy.
- Normalize and deduplicate metadata before persistence.
- Generate a high-entropy opaque client ID.
- Do not return a client secret.
- Ignore unknown display extensions only where RFC 7591 permits.
- Return standards-shaped registration errors without reflecting unsafe input.

Do not automatically fetch `client_uri`, `logo_uri`, or other display resources.

## Abuse Controls

Registration is unauthenticated and requires explicit controls:

- Bounded JSON request body.
- Global active-client capacity.
- Maximum redirect URIs per client.
- Maximum metadata field lengths.
- Global and trusted-source registration rate limits.
- Mandatory expiry and cleanup for dynamic registrations that have never completed an authorization.
- Per-principal promotion limits for dynamically registered clients.
- Reserved capacity so dynamic clients cannot consume preregistered/CIMD admission capacity.
- Inactivity reclamation for dynamic clients with no active grants or refresh families.
- Metrics for accepted, rejected, limited, and capacity-blocked requests without client IDs as labels.

An unauthenticated DCR registration starts in a provisional state with a bounded lifetime. Its first successful authorization promotes it to an active client only when the authenticated principal remains below its promotion quota. Expired provisional registrations are reclaimable and do not permanently consume active-client capacity. Dynamic clients with no active grants or refresh families expire after a bounded inactivity period. Active dynamic clients have a separate capacity pool so they cannot starve preregistered or CIMD clients; administrative revocation remains available for abuse response.

Proxy-derived source identity is used only when the existing trusted-proxy configuration establishes it. Otherwise use a global limiter rather than trusting forwarded headers.

## Preregistered Clients

Expose a store/service API for deployment or administrative bootstrap without adding a public administrative HTTP endpoint in this phase.

Preregistered clients obey the same redirect, public-client, resource, scope, disablement, consent, and revocation rules as dynamic clients. They differ only in registration source and lifecycle policy.

## Generic Client Helpers

Add reusable `mcp` client helpers for:

- Validating CIMD support from authorization-server metadata.
- Building and validating a Waltr client metadata document.
- Sending a public DCR request.
- Validating DCR responses and rejecting unexpected client secrets or auth methods.
- Representing registration method, issuer, redirect URI, creation time, and optional expiry.

Selection and persistence policy remain with the integrating host. The generic crate must not write Waltr credentials or inspect MCP server keys.

## Tests

Cover:

- CIMD success and exact client-ID binding.
- Redirect mismatch and metadata mutation between authorization and token exchange.
- Unsupported grant, response, PKCE, or token auth method.
- Redirect refusal and body-size enforcement.
- Public, private, loopback, link-local, DNS-rebinding, and metadata-service destination policy.
- CIMD cache hit, expiry, failed refresh, and changed digest.
- DCR request/response interoperability.
- Duplicate redirect normalization.
- Invalid URI schemes and wildcard redirects.
- Public loopback redirect policy.
- Registration quotas, rate limits, and concurrent capacity admission.
- Capacity exhaustion followed by provisional-registration expiry and recovery.
- Per-principal promotion exhaustion, inactive-client reclamation, and reserved-capacity enforcement.
- Disabled or revoked client behavior.
- Preregistered, CIMD, and DCR clients completing the same authorization flow.
- No client secret or registration credential leaking through logs or errors.

Add ecosystem cases using real MCP SDK clients when they support CIMD or DCR. Keep simulated protocol cases for edge conditions SDKs cannot induce.

## Exit Criteria

- A new public client can register through DCR and complete the full MCP authorization flow.
- A valid CIMD client can authorize without a stored client registration.
- CIMD cannot reach prohibited network destinations under production policy.
- DCR cannot create unbounded database growth.
- Preregistered, CIMD, and DCR clients share one grant and token policy.
- No Waltr-specific registration behavior exists in `crates/mcp`.

## Dependencies

Depends on [Authorization, Tokens, And Revocation](03-authorization-tokens.md). Waltr server and outbound client integration consume these registration mechanisms.
