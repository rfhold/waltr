# Contributing

Contributions should preserve clear layer boundaries, minimum-assumption documentation, and a working codebase.

# Running

Run the server:

`cargo run -p waltr-server`

Run web without starting the server:

`pnpm --filter @rfhold/waltr-web dev`

Run the server, Vite web client, and TUI together with `pnpm dev`.
For the server and web client without the TUI, run `pnpm dev:web`.
Both commands manage a separate loopback Apps container at `http://127.0.0.1:14334/container.html` and configure Vite automatically.
Neither command requires a manual Apps service or environment setup.
Vite binds `127.0.0.1:5173` with strict port selection so the container can trust exactly the `127.0.0.1` and `localhost` web origins.
If a port has another listener, startup fails instead of silently changing origins.
Set `WALTR_WEB_DEV_PORT` or `WALTR_APP_CONTAINER_ADDR` (an explicit `127.0.0.1:PORT` or `[::1]:PORT`) to choose unused ports.
`pnpm dev` keeps its API at `127.0.0.1:14333` and writes background logs, including `app-container.log`, under `.dev/logs/`.
`pnpm dev:web` keeps its API at `127.0.0.1:8080` and streams logs.
All managed children stop when the runner stops or a background child fails.
An explicit `VITE_WALTR_MCP_APP_CONTAINER_URL` instead uses a current separate-origin container; the runner starts no additional container.
Its operator must configure the exact Vite parent origins and the [Apps asset security policy](docs/deployment.md#mcp-apps-assets-and-app-container).
For a reverse proxy over the managed loopback container, `WALTR_APP_CONTAINER_ORIGIN` sets its exact public HTTPS origin.
`WALTR_APP_CONTAINER_HOST_ORIGINS` adds exact parent origins (comma-separated, no wildcards).

Run the static local Compose development profile:

`docker compose --profile dev up`

Repository Compose profiles are local development/test infrastructure. The Dockerfile's `runtime` target supports production containers; repository infrastructure and workload deployment are defined only in `infra/pulumi/`.

Run TUI without starting the server:

`cargo run -p waltr-tui`

Run the Linux Tauri app with its packaged local web assets:

`pnpm --filter @rfhold/waltr-app dev`

The Tauri picker selects Local or a saved remote server. Local starts the embedded server on demand; remote selections use native Waltr access OAuth when available. See [docs/deployment.md](docs/deployment.md#tauri-application).

Install and run the bundled TUI with an embedded server and web assets:

`pnpm install:bundled`

The bundled web host starts its separate-origin Apps listener automatically on loopback.
Normal bundled startup requires no separate Apps command or URL environment variable.
Plain TUI mode displays no Apps UI.

The server uses separate durable `waltr.sqlite3` and `mcp.sqlite3` files under `XDG_DATA_HOME` or `$HOME/.local/share` when database URLs are unset. `WALTR_DATABASE_URL` selects Waltr persistence; optional `WALTR_MCP_DATABASE_URL` overrides the derived MCP target. Access auth defaults to disabled. OIDC mode uses Authentik for browser/native authentication and requires a local MCP OAuth wrapping-key ring; Waltr itself issues the only bearer accepted by `/mcp`. See [docs/deployment.md](docs/deployment.md).

# Building

Web builds require workspace dependencies, the `wasm32-unknown-unknown` Rust target, and a compatible `wasm-bindgen` CLI on `PATH`.
Install the target with `rustup target add wasm32-unknown-unknown`.
Install `wasm-bindgen-cli` at the exact `wasm-bindgen` version in `Cargo.lock`.
The CLI and Rust dependency versions must match.
Browser and Tauri build scripts both generate shared WASM artifacts as a prerequisite.
Tauri bundles still contain no WASM output and use the Rust client runtime.
Web owns the trusted Apps container and its shared bridge contract.
Normal browser Vite builds emit the UI plus the self-contained `dist/app-container/container.html` and `container.js` pair.
Tauri modes omit the container.
Default server builds run `pnpm --filter @rfhold/waltr-web build:app-container`, without a WASM build.
With `web`, the server consumes container output from one full web build instead.
For served assets, Cargo runs `pnpm --filter @rfhold/waltr-server build` for the explicit `scripts/apps.entries` list.
Each independent Vite build emits self-contained `dist/<name>/index.html`; Cargo generates optional Rust asset constants in `OUT_DIR`.
The server workspace package owns the served-app SDK and Vite toolchain; root dependencies supply only Playwright.
Asset builds require Node, pnpm, and installed workspace dependencies; Cargo installs no Node packages.
`apps-assets` selects independent `apps-host-assets` and `apps-served-assets` features.
`--no-default-features` with no asset feature omits the assets, not the session Apps RPCs.
`WALTR_APP_CONTAINER_DIST` selects prebuilt container assets and bypasses the host-assets Vite build.
The full `web` build and served-app builds remain independent of this override.
See [Waltr-served Apps authoring](docs/mcp/apps-authoring.md) for the static source, resource, and ordinary tool addition path.
See [MCP Apps deployment](docs/deployment.md#mcp-apps-assets-and-app-container) for prerequisites, bundled startup, and external host configuration.
Normal Apple Silicon macOS builds enable Apps without a diagnostic feature, separate profile, or prior isolation report.
Other native platforms remain unsupported; this support makes no Intel or universal-package promise.
See [macOS Apps verification](docs/testing.md#macos-native-apps-verification) for live acceptance and the optional diagnostic harness.

From `crates/waltr-app`, build the normal unsigned Apple Silicon debug app:

```sh
CARGO_TARGET_DIR=../../target/macos-default cargo tauri build --debug --target aarch64-apple-darwin --bundles app --no-sign
```

From the repository root, open that bundle:

```sh
open target/macos-default/aarch64-apple-darwin/debug/bundle/macos/Waltr.app
```

These commands describe a procedure, not a successful build or live test.

To build and install the local debug app at the stable `~/Applications/Waltr.app` path:

```sh
pnpm dev:install
```

Quit Waltr before each install, including any copy open from the build directory.
The installer supports native Apple Silicon macOS only.
It requires the build prerequisites above, Xcode command-line tools, and the Cargo Tauri CLI.
It uses the normal unsigned debug build, packaged web assets, and `--locked` Cargo dependencies.
It sets `CARGO_TARGET_DIR` to the repository's `target/macos-default`, regardless of your shell's current directory or target-directory override.
It stages a complete bundle with `ditto`, then validates the bundle identity and executable.
It signs the staged bundle with identifier `dev.waltr.app` and the microphone entitlement from `crates/waltr-app/Entitlements.plist`.
It verifies the complete staged bundle with `codesign --verify --deep --strict` before replacement.
The default signature is ad-hoc (`codesign --sign -`).
To use an available stable codesign identity, set `WALTR_DEV_SIGNING_IDENTITY` to its name or certificate hash.
The installer rejects blank values and does not discover identities or create certificates.
A supplied identity can trigger a macOS Keychain prompt when you run the installer.
Build, copy, signature, or verification failure preserves the current app; a failed replacement attempts rollback.
An unsuccessful rollback reports the backup path for manual recovery.
If cleanup fails after installation, the command exits nonzero and reports that the new app occupies the installed path.
Cleanup errors also report the transaction path and possible backup location; a concurrent installation error retains its original cause.
The installer changes only the staged bundle's signature, not the build output or current installed app.
It never stops or launches Waltr, requests app permissions, resets TCC grants, or registers the app with LaunchServices.

After installation, open the stable path manually:

```sh
open "$HOME/Applications/Waltr.app"
```

The app keeps the `dev.waltr.app` identity and the same app data, profiles, and configuration.
This command creates no separate development profile and requires no Vite server or hot reload.
Ad-hoc signatures cover one code version, so rebuilds do not guarantee that macOS retains Accessibility or other TCC grants.
A consistent certificate identity matters across rebuilds, but neither a stable path nor successful signature verification guarantees permission retention.
`pnpm dev` continues to start the server, web client, and TUI; its behavior does not change.

Use `pnpm dev:install --dry-run` to print paths and the build/sign/verify plan without filesystem changes or subprocesses.
Use `pnpm dev:install --help` for usage.
Run deterministic installer tests with `node --test scripts/dev-install.test.mjs`; these tests use temporary bundles and fake subprocesses.

Build a Rust crate:

`cargo build -p <crate>`

Build the MCP crate:

`cargo build -p mcp`

Build the server:

`cargo build -p waltr-server`

Build all explicit server-owned apps independently:

`pnpm --filter @rfhold/waltr-server build`

Build only the trusted Apps container, without WASM:

`pnpm --filter @rfhold/waltr-web build:app-container`

Build the TUI:

`cargo build -p waltr-tui`

Build the web client:

`pnpm --filter @rfhold/waltr-web build`

Build the no-WASM web assets selected by Tauri:

`pnpm build:tauri`

Build the Linux Tauri packages:

`pnpm --filter @rfhold/waltr-app build`

Build the Android package:

`pnpm --filter @rfhold/waltr-app android:build`

Application prereleases are produced only from signed annotated tags exactly matching `vX.Y.Z-rc.N` whose commit is reachable from `origin/main`. Stable `vX.Y.Z` tags remain reserved for the production server pipeline. See [docs/deployment.md](docs/deployment.md#release-pipelines) for release artifacts, versionCode bounds, signing, secret, and rerun behavior.

# Protocol Buffers

Proto contracts live under `proto/`, Buf configuration lives at the repository root, and generated Rust is committed under `crates/waltr/src/grpc/`.

After proto changes, run these commands from the repository root:

```sh
sh scripts/check-protobuf-tools.sh
buf lint
buf generate
```

The preflight requires `protoc-gen-prost` 0.5.0 and `protoc-gen-tonic` 0.5.0. Ordinary application builds use committed generated Rust and never regenerate it. See [protobuf toolchain checks](docs/testing.md#protobuf-toolchain) for installation commands and verification limits.

# Testing

See [docs/testing.md](docs/testing.md) for test structure, commands, layering expectations, live-test setup, and container-backed test notes.

The canonical guide also covers Tauri host checks, Android feature checks, Gradle checks, and installed-device requirements.

For the opt-in sandboxed Chromium launcher, see [dedicated browser image checks](docs/testing.md#dedicated-browser-image). Current CI does not adopt that image; dependency preparation and image publication require separate authorization.

Run MCP spec tests:

`cargo test -p mcp --test spec`

Run all MCP tests:

`cargo test -p mcp --tests`

Run hosted MCP OAuth and SQL-store tests:

`cargo test -p mcp --features oauth-authorization-server --test oauth_authorization_server --test oauth_store`

`cargo test -p mcp --features oauth-authorization-server-sqlite --test oauth_sqlite_store`

`cargo test -p mcp --features oauth-authorization-server-postgres --test oauth_postgres_store -- --ignored`

Run server access-auth tests:

`cargo test -p waltr-server --test access_auth`

`cargo test -p waltr-server --test mcp_oauth_server`

Run deterministic agent MCP integration tests:

`cargo test -p waltr-agent --features shell --test agent mcp`

Compile the exact shared-client integration targets:

`cargo test -p waltr-client --tests --no-run`

Run the authenticated browser integration phase:

`pnpm test:web:integration:auth`

Run the web-owned app container unit/output and browser contracts:

`pnpm test:app-container:unit`

`pnpm test:app-container:browser`

These browser contracts use the actual host-only Cargo service and a fake host callback, not the full session RPC path.
See [MCP Apps tests](docs/testing.md#mcp-apps) for browser prerequisites and proof limits.

Build and test served-app output, independent second-app embedding, and Cargo asset orchestration:

`pnpm test:status-app`

# Commits

Use Conventional Commits.

Examples:

- `feat(agent): add shell permission flow`
- `fix(client): preserve reducer state on reconnect`
- `docs(architecture): describe config watch flow`
- `test(web): cover provider form transition`

Suggested scopes:

- `agent`
- `mcp`
- `client`
- `server`
- `tui`
- `web`
- `docs`
- `tests`
- `proto`
- `config`
- `status`
- `release`
