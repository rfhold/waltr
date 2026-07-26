# 03 Browser Session Binding

Status: Complete

Persisted browser sessions must only authenticate against the OIDC configuration that created them.

## Findings Resolved

- `authenticate_browser_session_token()` trusts the persisted issuer/subject without comparing to the currently configured issuer; swapping identity providers while keeping the database leaves old cookies valid (`crates/waltr-server/src/access_auth/service.rs`).
- Sessions carry no client ID, so a client-ID change under the same issuer also cannot invalidate them.

## Changes

Migrations are undeployed; edit them in place (no forward migration):

### Migrations (edit in place)

- `crates/waltr-server/migrations/sqlite/0002_oidc_access_auth.sql` and `crates/waltr-server/migrations/postgresql/0002_oidc_access_auth.sql`: add `client_id TEXT NOT NULL CHECK (length(client_id) > 0)` to the browser sessions table. Developers with applied checksums recreate their local databases.

### Store (`access_auth/store.rs`)

- Add `client_id` to `BrowserSession`/`ActiveBrowserSession`; validate nonempty on insert for both backends.
- `lookup_browser_session()` takes expected issuer + client ID and filters in the `WHERE` clause.
- Add non-mutating `is_browser_session_active(digest, now)` for later stream revalidation (WS4).
- Add `revoke_browser_sessions_outside_configuration(issuer, client_id, now)`; derive `Hash` for `BrowserSessionTokenDigest`.

### Service and handlers

- `service.rs`: after successful discovery, revoke sessions bound to a different issuer/client; persistence failure is startup-fatal. Lookup still filters exactly even after startup revocation (correctness does not depend on cleanup).
- `handlers.rs`: persist configured issuer + client ID on session creation.

## Tests (`crates/waltr-server/tests/access_auth.rs`, store unit tests)

- Same issuer/client survives server reconstruction.
- Changed client ID: old cookie rejected; row revoked at startup.
- Changed issuer: old cookie rejected; row revoked.
- Client-secret rotation (same issuer/client) preserves sessions.
- Revoked/mismatched rows stop consuming the 32/subject and 1024/global admission capacity.
- SQLite + ignored PostgreSQL migration column presence; backend-equivalent store behavior.

## Exit Criteria

- Restart against a different issuer or browser client immediately invalidates prior cookies.
- Ordinary restart and secret rotation preserve valid sessions.

## Verification

```
cargo test -p waltr-server --lib
cargo test -p waltr-server --test access_auth
```
