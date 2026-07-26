# 05 Client Transport And Lifecycle

Status: Complete

Fix native HTTPS, make effect execution cancellation-safe, scope results to credential/access generations, and stop denied-state busy-looping.

## Findings Resolved

- `NativeGrpcTransport::channel()` builds plain tonic endpoints; documented `WALTR_SERVER_ENDPOINT=https://...` cannot connect (`crates/waltr-client/src/transport/mod.rs`; tonic has only `transport` feature).
- `ClientStateStream::next()` pops an effect before awaiting; `next_or_dispatch()` drops the pending future when a command wins, losing the effect while its operation stays `Loading` (`coordination/runtime.rs`).
- `LoginRequired`/`LoginInProgress`/`LoginFailed` branches return identical snapshots immediately: hot loop in web/TUI actors.
- `waltr_login_error` (`client.rs`) intercepts any login-required failure before request/epoch guards; a late 401 from token A tears down replacement token B; stale completions can repopulate protected state after user changes.
- `LoggedOut` enters `LoginRequired` directly; auth-disabled deployments are trapped without an anonymous reprobe.
- Native credential clear paths are compare-then-clear across separate lock operations.

## Changes

### Native TLS (`Cargo.toml`, `transport/mod.rs`)

- Non-WASM tonic features: add `tls-native-roots`, `tls-ring`.
- Split endpoint construction from connection; HTTPS endpoints get `ClientTlsConfig::new().with_native_roots()`; HTTP stays plaintext for loopback/embedded.
- Tests: scheme-based endpoint construction plus a subprocess-isolated controlled-root HTTPS connection; the child starts without a process crypto provider and verifies production endpoint initialization installs one before connecting.

### Credential revisions (`transport/mod.rs`, `transport/auth.rs`)

- Native credential state: `lease_generation` + `credential_revision` + authorization metadata. Lease start/clear bumps both; every token replacement (including refresh under the same lease) bumps revision.
- Interceptor records the exact revision attached to each request; RPC error mapping carries it.
- Add `clear_bearer_token_if_current(...)` semantics: compare-and-clear under one write lock, keyed by revision (and lease for TUI-side calls).
- 401 rules: valid `AuthTarget` -> provider/MCP authorization (never clears Waltr bearer); Waltr-401 with current revision -> atomic clear + `WaltrLoginRequired`; superseded revision -> retryable `Other`, replacement credentials untouched. No automatic replay of unary mutations.

### Access generation (`coordination/state.rs`, `event.rs`, `runtime.rs`, `client.rs`)

- Add `access_generation: u64` to shared state; bump on access loss, explicit logout, and login completion.
- Stamp protected effects with the generation that created them; completions carry it back; reducer ignores stale-generation completions entirely (success and failure), keeping status-epoch checks as-is.
- Move `waltr_login_error` interception after request-identity/epoch/generation validity checks; duplicate `WaltrAccessExpired` reports are no-ops once access is already lost or a newer login attempt is active.

### Cancellation-safe effects (`runtime.rs`)

- `InFlightEffect { effect, future: LocalBoxFuture<'static, ExecutedEffect> }` slot on `ClientStateStream`; `ExecutedEffect` enum for events + each watch-open result.
- `execute_effect` split into `start_effect` (clones executor/sleep; returns owned `'static` future) and `complete_effect` (installs streams, produces events).
- `next()`: move one queued effect into the slot, await `future.as_mut()`, then take + complete. Command preemption in `next_or_dispatch` leaves the slot intact; the effect resumes on the next `next()` call. Effects complete exactly once, never restart.
- Intentional cancellation (drop slot + queue + watches) only on access loss/logout/generation replacement; post-login reload reconciles possibly-committed mutations.

### Denied-state parking (`runtime.rs`)

- After queue/in-flight drain, if access is `LoginRequired`/`LoginInProgress`/`LoginFailed`, await `future::pending()` instead of returning an unchanged snapshot; `next_or_dispatch` still wakes for commands.

### Logout reprobe (`client.rs`, `coordination/access.rs`)

- `logout_waltr`: clear protected state/interests/operations, enter `UnknownChecking`, start a fresh status-watch epoch; `LoggedOut` emits `StatusEffect::Watch { epoch }`.
- Auth-disabled servers recover to authenticated; OIDC servers get a current-epoch 401 -> `LoginRequired`. Platform ordering unchanged (web revokes cookie first; TUI clears bearer first).

## Tests

- `cargo test -p waltr-client --lib`: revision/lease atomicity, stale-401 classification, generation-stamped reducer guards, logout->UnknownChecking+single probe, stale completions ignored.
- `cargo test -p waltr-client --tests`: command-wins race preserves in-flight effect (controllable sleep + signal); parked stream stays pending under timeout while `next_or_dispatch(StartWaltrLogin)` completes promptly; explicit logout cancels old work and leaves exactly one status-watch effect; post-logout success authenticates; post-logout 401 requires login.

## Exit Criteria

- HTTPS endpoints connect with system roots.
- No effect is lost or replayed on command arrival; denied states consume no CPU.
- Stale results/401s can neither clear new credentials nor repopulate protected state.
- Auth-disabled logout recovers without client restart.

## Verification

```
cargo test -p waltr-client --lib
cargo test -p waltr-client --tests
```
