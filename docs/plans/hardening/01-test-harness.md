# 01 Test Harness

Status: Complete

Make Compose static dev/test infrastructure and make the integration runner isolated, failure-accurate, and self-cleaning.

## Findings Resolved

- Dev-mode integration runs inherit the caller's environment and durable XDG/HOME (or exported production) databases (`scripts/test-web-integration.mjs`).
- Compose interpolates ambient `${WALTR_*}` variables into the stack; hostile exported values reach containers.
- Runner exit path relies on an unreferenced timer calling `process.exit()`; caught failures can exit 0 and skip cleanup.
- `stopCompose()` ignores `spawnSync` status/error and clears `composeRunning` unconditionally; failed teardown contaminates the next phase.
- Test services publish host ports on all interfaces through a close-then-reuse TOCTOU window; parallel runs share the mutable `waltr-server-web:local` image tag.
- Startup readiness is timing-sensitive script sequencing rather than container health.
- k6 disables TLS verification (`K6_INSECURE_SKIP_TLS_VERIFY`, `ignore-certificate-errors`); the Caddy volume containing the CA private key is mounted into Waltr/MCP app containers.
- Docker build context includes `.envrc`, `.waltr/`, `.npmrc`, screenshots; validation runs after temp-dir allocation; failure screenshots can mask original errors.
- Package aliases use POSIX-only inline env and inherit ambient mode variables.

## Target Topology

| Profile | Services | Persistence | Host exposure |
| --- | --- | --- | --- |
| `dev` | `waltr` | named volume `waltr-dev-data` (disposable) | `127.0.0.1:14333` only |
| `test` | `waltr-test`, `dex`, `mcp`, one-shot `k6` | tmpfs `/data`; Dex memory storage | none |
| `test-auth` | `test-ca` (one-shot), `auth-proxy` (Caddy), `dex-auth`, `waltr-auth`, `mcp-auth`, one-shot `k6-auth` | tmpfs `/data` + cert volumes | none |

Every service carries an explicit profile; bare `docker compose up` starts nothing.

## Changes

### compose.yml

- Remove all `${...}` interpolation, both `env_file` entries, configurable image names, and dynamic ports.
- Fixed env per Waltr service: `WALTR_AUTH_MODE`, `WALTR_DATABASE_URL=sqlite:///data/waltr.sqlite3`, `WALTR_MCP_DATABASE_URL=sqlite:///data/mcp.sqlite3`, `WALTR_AUTH_PATH=/data/auth.json`, `WALTR_SESSION_STORE=filesystem`, `WALTR_SESSION_FILESYSTEM_ROOT=/data/sessions`, fixed `WALTR_MODELS_DEV_PATH` fixture mount.
- Keep existing static Dex client IDs/secrets/URLs in `test-auth`; network aliases `waltr.test`, `dex.test`.
- Let Compose derive project-scoped image names (no shared `waltr-server-web:local`).
- TLS fixture: one-shot `test-ca` service generates CA + leaf (SANs `waltr.test`, `dex.test`) into two volumes: `test-ca-public` (only `ca.crt`; mounted read-only into `waltr-auth`, `mcp-auth`, `k6-auth`) and `test-ca-caddy` (leaf cert/key for Caddy only). CA signing key is deleted after signing. Caddy uses explicit cert/key instead of `tls internal`.
- Remove `K6_INSECURE_SKIP_TLS_VERIFY` and Chromium `ignore-certificate-errors`; import `ca.crt` into k6 HTTP (`SSL_CERT_FILE`) and Chromium NSS store in the k6 entrypoint.
- Health checks: Waltr `curl --fail http://127.0.0.1:14333/health`; Dex discovery URL probe; MCP bounded request to `/mcp`; Caddy local health endpoint. Wire `depends_on.condition: service_healthy` end-to-end; k6 services depend on healthy Waltr/MCP/proxy.

### Dockerfile / .dockerignore

- Add `curl` to runtime stage for health checks.
- Extend `.dockerignore`: `.env*`, `.envrc`, `.waltr/`, `.npmrc`, `*.sqlite3*`, `*.key`, `tests/web/k6/snapshots/`, `.dev/`, `.worktrees/` (keep existing entries).

### scripts/test-web-integration.mjs

Rewrite control flow around an awaited `main()` + `finally`:

- CLI flags replace ambient selection: `--mode compose|dev`, `--access both|disabled|oidc`, `--live`, `--screenshots`, `--update-screenshots`, `--script <path>`; validate everything before creating the temp root.
- `cleanEnvironment()` strips inherited `WALTR_*`, `K6_*`, `COMPOSE_*` from all children; pass `--project-name` explicitly; set `COMPOSE_DISABLE_ENV_FILE=1`.
- Dev mode sets explicit temp `WALTR_DATABASE_URL`/`WALTR_MCP_DATABASE_URL` under the run's temp root plus `WALTR_AUTH_MODE=disabled`.
- Track every child (build, up, server, vite, k6); await `close` not just `exit`.
- Exit-code precedence: k6/test failure > cleanup failure (1) > success; `SIGINT`=130, `SIGTERM`=143; set `process.exitCode`, never `process.exit()` from cleanup paths; no unref timers.
- `stopCompose()` checks `error`/`status`/`signal`; failed `down` keeps state, retries once after killing children, and fails the run if still unsuccessful. Final teardown: `docker compose --profile test --profile test-auth down --volumes --remove-orphans --timeout 5`.
- Capture `docker compose ps --all` + logs into `.dev/logs/` before teardown on failure.
- Bounded timeouts on Docker invocations and readiness HTTP requests; screenshots best-effort so they cannot replace the original error.
- Replace host-port waiting with `docker compose up --detach --wait --wait-timeout 120`; drop test-profile host port allocation entirely (dev mode still allocates loopback ports).
- Seed `waltr.toml` fixture into tmpfs via `docker compose cp` + atomic rename instead of host bind mounts.
- Keep detached process-group ownership after the leader closes, identify the original Linux process by boot ID plus `/proc` start time, and verify the PGID is gone after TERM/KILL before cleanup succeeds. Missing launch identity fails closed with direct leader containment and never permits negative-PGID signaling.
- Treat the screenshot tree as untrusted filesystem state: reject symlinked layout components, require marker-owned direct-child run directories, and bind recursive deletion to verified parent and run identities.
- Serialize screenshot runs by fsyncing complete owner metadata before atomic hard-link publication; stale-lock recovery and release recheck the exact lock inode before deletion. Replace `current` atomically with rollback, and update baselines through a validated same-filesystem staging directory with backup restoration on any failed swap.
- Run Compose, direct-container, child, screenshot-lock, and temporary-root cleanup independently so one teardown failure cannot suppress later cleanup stages.

### Review follow-up hardening

1. Copy snapshot PNGs through exclusive no-follow descriptors and verify regular-file identities before and after each copy.
2. Remove snapshot staging and quarantined run directories through verified directory descriptors so pathname replacement cannot redirect cleanup.
3. Bind temporary run-root cleanup to the allocated parent/root identities and traverse the owned tree through its open descriptor.
4. Track baseline transaction locations by verified inode, including renames that complete before reporting an error.
5. On a failed snapshot swap, move the verified new baseline aside with bounded retries before restoring the prior baseline; preserve failed output for recovery when needed.
6. Make `current` rollback recognize late completed renames and retain a valid new target if restoration cannot be proven safe.
7. Recover stale pending lock publications under verified parent/inode constraints and surface pending-artifact cleanup failures.
8. Keep process-group termination armed after the leader closes and wait for verified descendant-group disappearance before reporting command completion.
9. Scrub every direct child of the generated-CA volumes, including hidden files and directories, before writing exact output contents.
10. Prove Chromium trust with direct first-use navigation; certificate-verifier retries and `ERR_CERT_VERIFIER_CHANGED` handling are forbidden.

### Focused re-review hardening

1. Quarantine emptied owned directories under their verified parent, verify the moved inode through the still-open descriptor, and restore mismatched replacements instead of removing them.
2. Treat a fully validated new baseline that remains canonical after exhausted rollback as a committed publication rather than returning a false failure.
3. Read Linux mount IDs from `/proc/self/fdinfo` and refuse destructive descriptor traversal across mount boundaries or when mount identity is unavailable.
4. Keep snapshot staging private and require each staged file to have exactly one link before writing, after writing, and before publication.
5. Route missing process-group launch identity through bounded direct-leader containment so absent `close` events settle as cleanup failures without negative-PGID signaling.
6. Clear every record timer at terminal settlement and relinquish detached child/stdio handles when the completion deadline expires.
7. Require CA output roots to be existing absolute physical directories, rejecting symlink roots and symlinked parents before temporary material, cleanup, or output writes.

### Final focused re-review hardening

1. Quarantine directories with Linux `renameat2(RENAME_NOREPLACE)` so an occupied destination is never replaced, then recheck the quarantined inode immediately before removal. Nested cleanup passes the verified parent descriptor to the helper instead of relying on process-local `/proc/self/fd` paths.
2. Revalidate every staged snapshot file after moving the old baseline and every canonical published file before commit; reject changed identities or link counts other than one and restore the old baseline.
3. Retain settled child records while their verified process or process group remains live, allow later cleanup passes to retry termination, and release ownership only after verified disappearance or a late `close` event.

### Final post-completion hardening

1. Quarantine stale screenshot runs with helper-backed `renameat2(RENAME_NOREPLACE)`, preserving source identity checks and propagating `EEXIST` without inspecting or removing an unverified destination.
2. Keep at most one `waitForRecords()` poll armed, clear its stored handle when the callback starts, and let timeout completion evaluate records without scheduling another poll.
3. Route every absent-destination runner move, including failed-baseline, current-rollback, and lock quarantines, through `renameat2(RENAME_NOREPLACE)`. Keep replacing renames only for validated `current` symlink and dev-config publication.

### Residual Boundary

Linux has no atomic compare-inode-and-remove-directory operation. The helper verifies device and inode immediately before descriptor-relative `rmdir`, which is sufficient for this local single-user test harness; continuous malicious replacement by another same-UID process in the final verification/removal window is outside the harness threat model.

### package.json

- `test:web:integration` -> `--mode compose --access both`; `:no-auth` -> `--access disabled`; `:auth` -> `--access oidc`; `:dev` -> `--mode dev --access disabled`; screenshots/live variants pass explicit flags. No inline `VAR=` prefixes.

### New contract tests

- `scripts/compose-config.test.mjs` (node:test): exact profile set; no unprofiled services; no `${...}` in raw YAML; no `env_file`; only `dev` publishes a port and it binds `127.0.0.1`; tmpfs `/data` on both test Waltr services; static DB/auth/session values; health checks present on long-running services; no TLS-bypass settings; `.dockerignore` covers keys/screenshots.
- `scripts/test-web-integration.test.mjs`: CLI parsing/defaults, `cleanEnvironment` stripping, phase->profile mapping, exit-code precedence, cleanup-once semantics, process-group identity reuse, symlink containment, atomic lock contention, partial initialization, independent cleanup, `current` rollback, and transactional screenshot-baseline failures via injected runtimes.
- `tests/web/k6/access/trust.browser.js`: `pnpm test:web:trust` starts the strict authenticated stack, proves `SSL_CERT_FILE` through a k6 HTTP request, and proves the custom entrypoint's NSS import through Chromium. The required healthy `waltr-auth` startup also proves the repository server's reqwest OIDC discovery can trust `https://dex.test`; no insecure client option is used.

## Explicit Non-Goals

Production Compose configurability, external OIDC providers in Compose, Dex consent automation (`skipApprovalScreen: true` stays), ACME/rotation, registry publication, non-root runtime hardening, digest-pinning every base image.

## Exit Criteria

- Hostile exported `WALTR_*`/`COMPOSE_*` values cannot influence any test run (dev or compose).
- Simulated k6 failure, startup failure, and teardown failure all produce nonzero exit codes.
- `SIGINT` leaves no containers, networks, volumes, or temp roots; exit 130.
- `test-auth` browser flows pass with real certificate verification; no CA private key is reachable from app/k6 containers.

## Verification

```
node --check scripts/test-web-integration.mjs
node --check scripts/test-web-integration.test.mjs
node --check scripts/compose-config.test.mjs
bash -n tests/web/k6/support/generate-test-ca.sh tests/web/k6/support/k6-entrypoint.sh
node --test scripts/compose-config.test.mjs scripts/test-web-integration.test.mjs
docker compose config --quiet
docker compose --profile dev config --quiet
docker compose --profile test config --quiet
docker compose --profile test-auth config --quiet
pnpm test:web:trust
pnpm test:web:integration:no-auth
pnpm test:web:integration:auth
pnpm test:web:integration
env WALTR_DATABASE_URL=postgres://ambient.invalid/waltr WALTR_MCP_DATABASE_URL=postgres://ambient.invalid/mcp WALTR_AUTH_MODE=oidc WALTR_OIDC_ISSUER=https://ambient.invalid COMPOSE_PROFILES=dev pnpm test:web:integration
pnpm test:web:screenshots:update
pnpm test:web:screenshots
```

Controlled `SIGINT` checks cover dev, authenticated Compose, and screenshot runs. Each must exit 130 and leave no project containers, networks, volumes, temporary roots, screenshot locks, or transaction artifacts.

Final verification passed with 71 Node contracts. Strict trust, disabled-access, OIDC, hostile-environment dual-profile, 30-snapshot update, and 30-snapshot validation runs passed. Controlled dev, authenticated Compose, and screenshot interruptions each exited 130 after managed resources started, with no matching project containers, networks, volumes, temporary roots, screenshot locks, quarantine paths, or snapshot transaction artifacts left behind.
