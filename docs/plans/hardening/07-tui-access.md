# 07 TUI Access Ownership

Status: Complete

Transient expiry must preserve and restore route ownership; explicit logout must clear protected terminal-local state; native discovery must fail closed.

## Findings Resolved

- Access expiry forces the route to `Access` then always restores `Landing`, while reducer-preserved watch interests keep counting for the abandoned route: hidden watches leak (`crates/waltr-tui/src/app.rs` snapshot handling).
- `LoggedOut` is handled identically to login cancellation: provider forms (including cloned headers), composer/prompt drafts, and overlays survive an explicit logout.
- Logout is always dispatched even with no native credential; in disabled-auth mode this traps the TUI in `LoginRequired` (`native_auth.rs`).
- `validate_provider_metadata` accepts omitted/empty `token_endpoint_auth_methods_supported` as public-client support; OAuth metadata default is `client_secret_basic`.
- `fail_attempt`/`expire_access` compare-then-clear credentials across separate lock operations.

## Changes

### native_auth.rs

- Require explicit `"none"` in `token_endpoint_auth_methods_supported` (omitted/empty fails validation).
- Use the same path-issuer discovery order as the server (appended OIDC form primary, prefix fallback on 404).
- Replace compare-then-clear with the atomic `clear_bearer_token_if_current` API from WS5; only emit failure/expiry events when the atomic clear succeeds.
- Logout with no native credential is a no-op event (or hidden action) rather than dispatching `LogoutWaltr` into a disabled-auth trap; with credentials, clear bearer first then dispatch `LogoutWaltr` (which now reprobes anonymously per WS5).

### app.rs

- Add `access_return_screen: Option<AppScreen>`: record the current protected route on the first transition into access UI only; never overwrite while on `Access`; keep interests and drafts during transient loss; restore the exact route once authenticated + ready; clear after restoration; initial-startup login still lands on `Landing`.
- Add `clear_protected_ui_state()` invoked only for `NativeLoginEvent::LoggedOut` (not expiry/cancel/failure): reset provider/MCP/runtime-defaults/sessions sections, auth input, composer and prompt drafts, overlays/confirmations, protected notices, access return screen; drop to `Waiting` while the shared client probes.
- Hide/disable the logout leader action when no native credential exists and server auth is disabled.

## Tests

- `--tests` app suites: expiry from Config restores Config without release/re-register churn (interest counts stay balanced); expiry from session detail restores Sessions/detail ownership; explicit logout clears provider form drafts and composer text; stale login failure does not clear a newer credential (atomic clear); logout action hidden in disabled mode.
- `--test server_backed`: auth-disabled logout -> `UnknownChecking` -> anonymous probe -> back to Landing without native OIDC.

## Exit Criteria

- Transient expiry never orphans watch interests; the restored route owns them.
- Explicit logout leaves no protected terminal-local data.
- Public-client discovery fails closed on metadata.

## Verification

```
cargo test -p waltr-tui --lib
cargo test -p waltr-tui --tests
cargo test -p waltr-tui --test server_backed
```
