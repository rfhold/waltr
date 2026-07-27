# OIDC Client And UI Flows Plan

## Status

Implemented across `2c01447`, `f203e90`, and `7c6a8f8`. This is the fourth historical plan in the persistence and authentication sequence. Use [Access Authentication](../architecture/access-authentication.md) and [Client Architecture](../architecture/clients.md) for the current contract.

At this plan's completion, external MCP clients discovered and obtained tokens from the deployment's external OIDC provider. The later [MCP OAuth Authorization Server](mcp-oauth-authorization-server/README.md) work replaced that MCP-specific flow with Waltr-hosted authorization; the flow and non-goals below preserve the original implementation boundary.

## Objective

Implement Waltr access-login behavior in the shared client, web application, and TUI while preserving the separate provider/MCP credential authorization flows already exposed by `AuthService`.

This phase also verifies the complete MCP authorization experience against the OIDC-protected Waltr MCP endpoint.

## Preimplementation Baseline

Before this plan was implemented, the shared client interpreted a gRPC `Unauthenticated` status as provider or upstream MCP authorization only when the status details decoded to an `AuthTarget`. Other unauthenticated responses became generic connection errors.

The web and TUI both rendered provider/MCP OAuth requirements from `snapshot.auth.pending`. Neither UI had a Waltr login state. Native gRPC transports did not attach bearer credentials, and the browser transport had no explicit session-expiry behavior.

## Scope

- Represent Waltr login-required state separately in `waltr-client`.
- Preserve existing provider/MCP `AuthTarget` behavior.
- Add credential injection and refresh/reconnect hooks to native transport orchestration.
- Add browser signed-out, login, session-expired, and logout behavior.
- Add a TUI login screen and native Authorization Code with PKCE flow.
- Reconnect watches and reload server state after successful login.
- Clear protected client state after logout or terminal authentication failure.
- Validate the OAuth discovery and bearer-token path for external MCP clients.
- Keep platform-specific browser, loopback listener, and token handling out of deterministic reducers.

## Shared Authentication Model

The client must model two distinct auth states:

| State | Trigger | UI behavior |
| --- | --- | --- |
| Waltr login required | gRPC `Unauthenticated` without provider/MCP `AuthTarget` details. | Show server login flow. |
| Credential authorization required | Existing event containing a provider or upstream MCP `AuthTarget`. | Show the existing OAuth dialog or modal. |

Do not create a synthetic `AuthTarget` for Waltr login. `AuthTarget` identifies a credential Waltr needs for an outbound provider or MCP call; it is not a user identity or server-access target.

Add an explicit client error kind, event, and snapshot state for Waltr login. The reducer remains deterministic: it records login facts and emits effects, while transports and platform runtimes perform redirects, open browsers, run loopback listeners, and exchange tokens.

## Shared Client State

The shared client should expose enough stable state for both UIs:

| State | Meaning |
| --- | --- |
| Unknown/checking | The client has not established whether Waltr access is authenticated. |
| Login required | The server rejected access without an `AuthTarget`. |
| Login in progress | A platform login effect is active. |
| Authenticated | A protected status request succeeded. |
| Login failed | The platform flow failed and can be retried. |

Commands should cover starting login, completing login, canceling login, and logout at the shared intent level. Platform runtimes may reject commands they cannot support, but web and TUI should expose equivalent user outcomes.

Provider/MCP authorization state in `AuthSnapshot` remains independent. A user may be authenticated to Waltr and still need to authorize a configured model provider.

## Protected State And Reconnection

When Waltr authentication expires or is revoked:

- Stop or suspend protected status, config, catalog, session, and timeline watches.
- Mark Waltr login required.
- Do not translate the failure into a provider OAuth requirement.
- Avoid reconnect loops that repeatedly call protected endpoints with known-invalid credentials.
- Remove protected snapshots that should not remain visible after logout.

After login succeeds:

- Re-establish the status watch first.
- Wait for readiness using the existing client coordination rules.
- Reopen child watches according to current watch interest.
- Reload authoritative server collections rather than assuming data remained unchanged.

## Web Flow

The server owns browser Authorization Code exchange and the HttpOnly session cookie. The SPA must never read the cookie, handle the client secret, or exchange the OIDC code itself.

The web behavior will be:

1. A protected gRPC-web call returns unauthenticated, or the initial session check reports signed out.
2. The shared snapshot enters Waltr login-required state.
3. The signed-out route offers a login action that navigates to `/oidc/login` with a safe local return path.
4. The server completes callback validation, sets the cookie, and redirects to the return path.
5. The app restarts protected status coordination and enters its normal shell after readiness.

If the server protects the initial SPA navigation with a redirect, the same signed-out state still handles session expiry while the SPA is already loaded and development deployments where assets remain public.

Logout calls the server logout endpoint, clears local protected state, and returns to the signed-out route. The existing `OAuthDialog` remains provider/MCP-specific and must not appear for Waltr login.

The browser transport must send same-origin credentials on gRPC-web requests. If the current transport already relies on browser defaults, add a contract test rather than adding redundant token plumbing.

## TUI And Native Flow

Use Authorization Code with PKCE and a loopback redirect as the baseline native login flow. Device Authorization Grant may be added later for providers that support it, but it is not required for the first implementation.

The runtime flow will be:

1. Discover Waltr's protected-resource metadata and the configured OIDC authorization server.
2. Load the deployment's public native client ID and requested scopes from explicit client configuration.
3. Bind an ephemeral loopback listener before creating the authorization URL.
4. Generate state, nonce, and a PKCE verifier/challenge.
5. Open the system browser when available and always render a copyable URL.
6. Receive and validate the loopback callback state.
7. Exchange the code at the provider token endpoint using the PKCE verifier.
8. Attach the access token to native gRPC requests.
9. Reconnect the shared client runtime and resume watches.

The TUI login screen must show progress, a copyable authorization URL, cancellation, retryable errors, and clear expiration. It must remain separate from the existing provider/MCP OAuth modal.

Browser launching, loopback HTTP, and token exchange belong in the TUI/native runtime boundary. The reducer receives only started, completed, canceled, expired, and failed events.

## Native Token Handling

`NativeGrpcTransport` needs a credential provider or request interceptor that can attach the current bearer token to every service client and long-lived watch request. Creating a fresh channel or service client must not drop authentication metadata.

The runtime should refresh an access token before expiry when a refresh token is available. A failed refresh transitions to login-required and stops protected reconnect loops.

Persistent refresh-token storage requires a deliberate secret-storage design. The initial implementation may keep native tokens in process memory and require login after restart; it must not introduce a plaintext token file as an incidental convenience. Durable native credentials can be added later through an OS credential store or an explicitly secured Waltr credential contract.

## Native Client Configuration

Native OIDC is a public-client flow and must not reuse the server's confidential client secret.

Define explicit deployment settings for:

- Native OIDC client ID.
- Loopback redirect policy or fixed redirect URI where required by the provider.
- Requested scopes.
- Optional native authorization-request audience. The server's `WALTR_OIDC_AUDIENCE` validation setting remains required.

Issuer and authorization-server metadata should be discovered from the Waltr protected-resource contract so the client does not need a second independently configured issuer. Client ID is not resource-server metadata and may remain a TUI environment/config setting unless Waltr introduces a small public client-configuration endpoint.

## MCP Authorization Flow

The Waltr MCP endpoint remains bearer-only. External MCP clients should be able to:

1. Receive a bearer challenge from `/mcp`.
2. Discover Waltr's OAuth protected-resource metadata.
3. Discover the configured authorization server.
4. Complete an authorization flow supported by that client and deployment.
5. Retry MCP initialization with a valid access token.
6. Use the same token for POST, GET, and DELETE requests.

Waltr does not need to become an authorization server or implement dynamic client registration for this flow. Client registration remains a deployment concern at the configured OIDC provider.

The generic `mcp` client already supports supplying a bearer token and reporting authorization failure. Automatic interactive OAuth orchestration should be added to the generic crate only if Waltr or another concrete consumer needs it as a reusable library contract; the Waltr server gate does not depend on that addition.

## Implementation Steps

1. Add Waltr login-required error, event, reducer state, effects, and snapshots.
2. Split generic unauthenticated handling from `AuthTarget` credential requirements.
3. Suspend and resume shared watches around authentication state changes.
4. Add native transport bearer injection and token refresh hooks.
5. Implement web signed-out, login, expiry, and logout behavior.
6. Implement native OIDC discovery, loopback PKCE, and token exchange in the TUI runtime.
7. Add TUI login rendering, keyboard behavior, cancellation, and retry.
8. Verify MCP challenge, discovery, token acquisition, and retry with a conforming client fixture.
9. Add cross-layer tests and update user-facing configuration documentation.

## Tests

Follow the client and UI layering in [testing.md](../testing.md).

Shared client tests must cover:

- Unauthenticated without details becoming Waltr login required.
- Unauthenticated with `AuthTarget` remaining provider/MCP authorization required.
- Protected watches stopping after session expiry.
- Successful login restarting status before child watches.
- Logout clearing protected snapshots and preserving deterministic reducer behavior.
- Provider authorization still working after Waltr login.

Native transport and runtime tests must cover:

- Bearer metadata on unary and streaming RPCs.
- Token replacement applying to newly opened requests.
- State and PKCE validation.
- Loopback bind, callback, timeout, cancellation, and port cleanup.
- Refresh success and refresh failure transitions.
- No token values appearing in errors or logs.

Web tests must cover:

- Signed-out rendering and login navigation.
- Callback return to the intended local route.
- Session expiry while the app is loaded.
- Logout and protected-state clearing.
- Provider/MCP `OAuthDialog` remaining distinct.
- Same-origin cookies being sent with gRPC-web calls.

TUI tests must cover:

- Login screen routing and rendering.
- Copyable URL, progress, cancellation, expiry, and retry.
- Successful login reconnecting to the normal landing screen.
- Existing provider/MCP OAuth modal behavior remaining unchanged.
- Server-backed parity with the equivalent web login outcomes.

MCP end-to-end tests must cover:

- Challenge and protected-resource discovery.
- A valid acquired token completing initialization.
- The token being attached to POST, GET, and DELETE.
- Expiry or revocation surfacing as authorization failure rather than session-not-found.

## Acceptance Criteria

- Web and TUI can authenticate to an OIDC-protected Waltr server.
- The shared client distinguishes Waltr login from provider/MCP credential authorization.
- Browser code never handles the OIDC client secret or session-cookie value.
- Native gRPC attaches bearer tokens to unary and streaming requests.
- Auth expiry pauses reconnect attempts until credentials change.
- Login success restores status and interested child watches in the established order.
- External MCP clients receive enough standards-compatible metadata and challenges to obtain and use an access token.
- Existing provider and upstream MCP OAuth UI continues to work independently.

## Phase-Local Non-Goals

These items were outside this client/UI plan, not a repository-wide prohibition on future work.

- User profiles, roles, ownership, or tenant-specific UI.
- Sharing provider credentials by OIDC subject.
- Storing native refresh tokens in plaintext.
- Requiring Device Authorization Grant support.
- Turning Waltr into an OAuth authorization server.
- Implementing automatic OAuth orchestration in every generic MCP client.

## Dependencies

This plan depends on [OIDC Server Gate](oidc-server-gate.md). It also relies on the reconnect and watch contracts described in [Client Architecture](../architecture/clients.md).

## References

- [`crates/waltr-client/src/transport/mod.rs`](../../crates/waltr-client/src/transport/mod.rs)
- [`crates/waltr-client/src/coordination/`](../../crates/waltr-client/src/coordination)
- [`crates/waltr-web/src/app/state.tsx`](../../crates/waltr-web/src/app/state.tsx)
- [`crates/waltr-web/src/routes/`](../../crates/waltr-web/src/routes)
- [`crates/waltr-tui/src/app.rs`](../../crates/waltr-tui/src/app.rs)
- [`crates/waltr-tui/src/runtime.rs`](../../crates/waltr-tui/src/runtime.rs)
- [`crates/mcp/src/client/transport/http.rs`](../../crates/mcp/src/client/transport/http.rs)
- [Testing](../testing.md)
