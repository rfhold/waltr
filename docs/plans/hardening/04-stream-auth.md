# 04 Stream-Lifetime Authentication

Status: Complete

Authenticated gRPC streams must end when the credential that opened them ends.

## Findings Resolved

- `GrpcAccessAuthService::call()` authenticates only while opening the RPC; config/session/timeline/status watches keep delivering protected data after logout, browser-session expiry, or JWT expiry (`crates/waltr-server/src/access_auth/middleware.rs`).

## Design

### Principal lease (`principal.rs`, new `lease.rs`)

`AuthenticatedPrincipal` gains a private lease:

- `Unbounded` (auth disabled; OIDC ID-token login principals need none).
- `Bearer { expires_at }` from the validated raw JWT `exp` (add explicit post-decode `exp > now` check).
- `Browser { token_digest, issuer, client_id, expires_at, store, revocation_signal }`; never retains plaintext tokens; custom `Debug` omits digest/store/signal.

### Revocation registry (`lease.rs`, authenticator)

- Weak in-memory registry keyed by `BrowserSessionTokenDigest`, owned by the authenticator.
- Browser authentication registers/obtains the shared cancellation signal before the DB lookup (closes lookup/logout race).
- `logout()` routes through the authenticator: commit `revoked_at_ms`, then cancel the local signal only after successful commit. Database stays authoritative across instances.
- Monitors are owned by response bodies (no detached tasks): cancel on signal, end exactly at persisted expiry, revalidate via primary-key `is_browser_session_active()` every 1s for cross-process revocation, fail closed on store error.

### Guarded body (`middleware.rs`)

- Wrap successful authenticated responses in `AuthenticatedGrpcBody` implementing `http_body::Body`:
  - Poll the lease monitor before polling source frames.
  - On expiry/revocation: stop the source; emit one gRPC trailer `UNAUTHENTICATED` ("Waltr access authentication required"), built via `tonic::Status::into_http()` headers without `content-type`.
  - On revalidation store failure: `UNAVAILABLE`.
  - Completed unary bodies pass through unchanged.
- Layer order stays `GrpcTransportLayer -> GrpcWebLayer -> GrpcAccessAuthLayer -> routes`, so gRPC-web trailer conversion happens outside the guard. No changes inside StatusService/ConfigService/SessionService.
- Add `http-body` 1.x as a direct dependency (workspace + waltr-server).

Bearer streams end at `exp`; provider-side early JWT revocation stays unsupported and documented.

## Tests (`crates/waltr-server/tests/access_auth.rs`)

- Native bearer `WatchStatus`: initial item, then `Unauthenticated` at JWT expiry (short-lived token fixture).
- gRPC-web browser stream ends with encoded status 16 after session expiry.
- Same-instance logout cancels an open stream immediately.
- Two servers sharing one SQLite file: logout on A ends B's stream within the revalidation bound.
- Logout of one browser session does not cancel another's stream.
- Store failure during revalidation ends with `Unavailable` (14).
- Disabled-mode streams unaffected; wrapper emits exactly one terminal trailer and never polls the source afterward.
- Empty error details so the shared client maps it to Waltr access loss (`transport status_error()` contract).

## Exit Criteria

- No protected stream outlives its lease: logout immediate in-process, <=1s cross-process, exact expiry timers otherwise.

## Verification

```
cargo test -p waltr-server --lib
cargo test -p waltr-server --test access_auth
cargo test -p waltr-server --tests
```
