# 06 Web Shared Reducer Path

Status: Complete

Every browser RPC flows through command -> reducer -> effect -> executor -> event -> snapshot. Remove all direct WASM transports.

## Findings Resolved

- `crates/waltr-client/src/wasm.rs` exposes three direct RPCs constructing `DefaultGrpcTransport`: `search_catalog_models_json`, `suggest_catalog_matches_json`, `search_session_templates_json`. Their 401s bypass shared access state; duplicate private DTO conversions exist only for them.
- Web `app/state.tsx` + provider model form consume the catalog helpers; `SessionsRoute.tsx` already uses the shared `SearchSessionTemplates` path, so the direct template helper is dead.

## Changes

### waltr-client coordination

- New catalog coordination following the session-template pattern (`coordination/session.rs` request-ID style):
  - `ClientCommand::SearchCatalogModels { query, ... }`, `ClientCommand::SuggestCatalogMatches { ... }`.
  - `CatalogEvent` requested/succeeded/failed variants keyed by `request_id`.
  - `CatalogEffect` executed through the existing shared executor + `WaltrTransport` catalog methods.
  - `CatalogClientState`: latest model search + latest suggestion operation only (request ID, loading/success/error, stale flag, results); stale request IDs and stale access generations ignored.
  - `CatalogSnapshot` exposed via `ClientSnapshot.catalog` and view models.
- Clear catalog state under the same transient/explicit access policies as other protected data (WS5 generation rules apply automatically since effects run through the shared executor).

### wasm.rs

- Delete the three direct methods, the `DefaultGrpcTransport` import, and the private catalog/session DTO conversion types.
- Catalog flows use `dispatch_json` + snapshot like everything else.
- Add a compile-time/source assertion (unit test) that `wasm.rs` does not reference `DefaultGrpcTransport`.

### Web

- `src/wasm/client.ts`: remove `searchCatalogModels`, `suggestCatalogMatches`, `searchSessionTemplates` wrappers.
- `app/state.tsx`: replace direct calls with command dispatch + snapshot selection for catalog data.
- Provider model form: local state limited to query text, selection, dialog visibility; loading/errors/results derive from `snapshot.catalog`.
- Regenerate ignored local WASM artifacts (`pnpm --filter @rfhold/waltr-web wasm:build`) into validated content-addressed versions and atomically switch the `generated/wasm-current` symlink consumed by the web build; never hand-edit generated output. Generated binding files are immutable, identical output reuses its existing version, and a version manifest records the latest publication time.
- Explicit retention cleanup (`pnpm --filter @rfhold/waltr-web wasm:cleanup`) takes the publication lock, validates every version before removing any, and preserves the current version, every version whose manifest publication time is no more than five minutes old, and the two most recently published non-current versions. Cleanup cannot remove a resolved target until its recorded publication time is over five minutes old and it is outside the history floor; a stable system wall clock therefore gives readers at least five minutes after the latest publication. Cleanup does not detect active readers and does not promise indefinite retention.
- The publisher derives its generated root from the repository script location, rejects redirected ancestors and pointers, and atomically claims dead owner, transaction, and temporary-pointer artifacts into owner-bound recovery namespaces before validating and removing them. A lost claim means another recovery owns the artifact; changed, ambiguous, linked, or hostile claimed artifacts are preserved and fail closed.
- Linux publication locks use boot ID plus process start time as strong owner identity. Platforms without that identity use a nonce-bound heartbeat refreshed atomically while publication or cleanup holds the lock; a fresh heartbeat remains active regardless of PID observations, while an expired heartbeat is reclaimable even if the PID is alive or reused. The heartbeat task stops and joins before release, and ownership checks prevent it from releasing a replacement lock.
- The filesystem boundary assumes cooperative same-UID local processes. Validation, atomic claims, non-following traversal, and replacement checks prevent accidental or competing-publisher deletion, but Node filesystem APIs cannot close every malicious same-UID check/use race against an attacker replacing validated ancestors during an operation.

### Tests

- Client: catalog reducer request/success/failure/stale-request/stale-generation transitions; snapshot shape.
- Web: provider form drives commands and renders snapshot-derived loading/error/results; template search still covered by existing sessions tests; a catalog 401 surfaces the access route via shared state.
- Build generation: subprocess fixtures cover every transaction and publication crash boundary, concurrent stale recovery and post-discovery replacement, Linux and portable lock identity, active/expired/malformed heartbeats, cleanup contention and waiters, concurrent writers, continuous readers, generation failure, content reuse, reader-safe cleanup retention, redirected roots, hostile names, and external-sentinel preservation.

## Exit Criteria

- No `DefaultGrpcTransport` construction outside the runtime executor.
- Catalog 401s produce the same access transition as any protected effect.
- Web builds and tests pass against regenerated bindings.

## Verification

```
cargo test -p waltr-client --lib
cargo test -p waltr-client --tests
pnpm --filter @rfhold/waltr-web wasm:build
pnpm --filter @rfhold/waltr-web test
pnpm --filter @rfhold/waltr-web build
```
