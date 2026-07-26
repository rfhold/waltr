# 02 Persistence And OIDC Hardening

Status: Complete

Close persistence target-identity gaps and make every OIDC back-channel interaction fail closed.

## Findings Resolved

- Nested SQLite `file:` URIs (`sqlite://file:/tmp/db`) bypass same-file rejection and parent-path handling (`crates/waltr-server/src/persistence/mod.rs` `parse_target()`).
- One shared reqwest client follows up to 3 redirects for discovery, JWKS, and token exchange; a 307/308 can replay code, PKCE verifier, Basic header, or `client_secret_post` body (`crates/waltr-server/src/app.rs` `build_app()`).
- Absent `code_challenge_methods_supported` passes PKCE validation (`access_auth/oidc.rs` `discover()`).
- Discovery uses only the RFC 8414 prefix form; standard path-issuer form (`<issuer>/.well-known/openid-configuration`) is not attempted.
- `id_token_signing_alg_values_supported` constrains both ID tokens and RFC 9068 access tokens, and filters JWKS for both (`oidc.rs`).
- `allow_insecure_http=true` accepted alongside HTTPS callback + secure cookies (`access_auth/config.rs`), drifting from the documented single loopback development mode.
- Post-commit expired-record cleanup failure turns a committed login/session into `503` (`access_auth/handlers.rs` `start_login()`, `complete_callback()`).
- Login source limiting uses the raw TCP peer; deployments behind a reverse proxy share one bucket.

## Changes

### Persistence (`crates/waltr-server/src/persistence/`)

- In `parse_target()`: after SQLx parses options and in-memory mode is ruled out, reject a decoded filename that case-insensitively starts with `file:` with a sanitized config error (`WALTR_DATABASE_URL must use a direct SQLite path rather than a nested file: URI`). Preserve named shared-memory URLs.
- Keep collision checks operating on created/canonicalized files; verify tests cover `sqlite:file:/tmp/db`, `sqlite://file:/tmp/db`, percent-encoded `file%3A`, relative/absolute alias pairs, and Waltr/MCP cross-position aliases.
- Before either migration, create/open both configured file targets without truncation and reject equal filesystem identities (Unix device/inode; Windows volume/file ID). SQLx continues to use the configured paths so journal, WAL, reconnect, and restart behavior remains standard.
- This is a single-process startup configuration check, not a defense against concurrent malicious directory-entry replacement. Deployment filesystem ownership and permissions must prevent same-UID mutation of database directories during startup and operation.

### Redirect-free back channel (`access_auth/oidc.rs`, `mod.rs`, `service.rs`, `app.rs`, `test_support.rs`)

- Add `AccessAuthHttpClient` newtype wrapping `reqwest::Client`; constructor applies `redirect::Policy::none()` unconditionally after caller timeouts. `AccessAuthenticator::initialize()` accepts only this type.
- Discovery, JWKS fetch/refresh, and token exchange treat any 3xx as a provider error. Browser authorization redirects are unaffected.
- Tests: discovery/JWKS redirect targets never contacted; token endpoint 307 never receives code/verifier/secret/Basic header (assert via second mock server receiving zero requests).

### Discovery and metadata (`oidc.rs`)

- `discovery_urls()` ordered: primary `<issuer>/.well-known/openid-configuration`, fallback prefix form only on explicit 404; dedupe for root issuers; no double-encoding of issuer path segments. Malformed docs, issuer mismatch, redirects, transport errors stay fatal (no fallback).
- Require `code_challenge_methods_supported` present and containing `S256`; absent or empty fails startup.
- Token auth semantics: secret + omitted metadata -> `client_secret_basic`; no secret requires explicit `none`; never silently downgrade a configured secret.

### Token algorithm separation (`oidc.rs`, `jwks.rs`)

- Rename current set to `id_token_signing_algorithms`; add optional `access_token_signing_alg_values_supported` deserialization.
- Access-token policy: explicit metadata -> intersection with Waltr's asymmetric allowlist (empty intersection fails discovery); absent -> full allowlist.
- `JwksCache` filters by the union; `header_and_key()` selects per `TokenKind`. Keep `kid`, key `use`/ops, `typ`, and `at_hash` (ID algorithm) checks.
- Tests: mixed RS256 ID / RS384 access provider validates both; explicit access `RS384` rejects RS256 access tokens; symmetric-only access metadata fails discovery.

### Insecure-mode policy (`config.rs`)

- Collapse the two booleans into one validated internal policy: `HttpsOnly` (both flags false, HTTPS callback) or `LoopbackHttp` (both flags true, HTTP loopback callback). Every other combination is a startup error naming the flags without echoing secrets.
- JWKS/endpoint HTTP permission derives from the policy, not per-URI re-inference.
- Test the full flag/callback matrix.

### Advisory cleanup (`store.rs`, `handlers.rs`)

- Add best-effort bounded cleanup that logs sanitized warnings and returns `()`. `start_login()` and `complete_callback()` call it after their inserts commit; the cookie/redirect is returned regardless. Insert/capacity/exchange failures stay strict.
- Test: cleanup failure (closed pool / poisoned table) after committed insert still returns success and the committed row remains usable.

### Trusted proxy source addresses (`config.rs`, `handlers.rs` or middleware)

- Add optional `WALTR_TRUSTED_PROXIES` (comma-separated CIDRs). When the TCP peer is inside a trusted CIDR, derive the login source from the nearest untrusted hop in `X-Forwarded-For`; otherwise use the TCP peer. Malformed headers from trusted peers fail closed to the TCP peer.
- Tests: untrusted peer spoofing XFF is ignored; trusted proxy chain resolves the client hop; rate-limit buckets key on the derived source.

## Exit Criteria

- No OIDC back-channel request follows a redirect; secrets cannot reach redirect targets.
- Missing PKCE metadata, missing public `none`, and partial insecure flags all fail startup with sanitized errors.
- Path-based issuers discover via the standard location with RFC 8414 fallback.
- Mixed-algorithm providers work; access tokens are not constrained by ID-token metadata.
- Committed logins survive cleanup failures.
- SQLite nested `file:` aliases are rejected in both Waltr and MCP positions.

## Verification

```
cargo test -p waltr-server --lib
cargo test -p waltr-server --tests
cargo test -p waltr-server --test access_auth
```
