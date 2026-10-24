# Index

| Path | Info |
| --- | --- |
| [src/server_controller.rs](src/server_controller.rs) | Process-owned embedded server, app-private paths, ephemeral loopback endpoint, and generation bearer. |
| [src/bridge.rs](src/bridge.rs) | Selected Local/remote client runtime, access coordinator, generation lifecycle, and bounded publication. |
| [src/native_audio/](src/native_audio/) | Shared microphone admission, audio runtime fences, and desktop audio hold adapter. |
| [src/dictation.rs](src/dictation.rs) | Chat dictation identities, status, preview, IPC, and shared native interaction consumer. |
| [src/native_audio_telemetry.rs](src/native_audio_telemetry.rs) | Shared audio diagnostics; preserves the literal historical telemetry target. |
| [src/shortcuts.rs](src/shortcuts.rs) | Process-owned desktop registration and ingress-epoch runtime routing. |
| [src/activity.rs](src/activity.rs) | Text-free client-operation leases through permission, effects, and cleanup. |
| [src/tray.rs](src/tray.rs) | Optional desktop status icon and minimal status/Open Waltr/Quit menu. |
| [../../docs/architecture/clients.md#client-routine-activity-tray](../../docs/architecture/clients.md#client-routine-activity-tray) | Client-owned activity scope and tray lifecycle. |
| [src/audio_permissions.rs](src/audio_permissions.rs) | Audio-only permission tickets, foreground policy, and stale-grant rejection. |
| [src/audio_permissions_macos.rs](src/audio_permissions_macos.rs) | Apple Silicon AVFoundation audio-only permission adapter. |
| [../../docs/architecture/clients.md#native-app-audio-composition](../../docs/architecture/clients.md#native-app-audio-composition) | Native audio ownership, IPC, lifecycle, and platform capability contract. |
| [src/server_profiles.rs](src/server_profiles.rs) | Nonsecret profile registry, strict endpoint normalization, and last-success persistence. |
| [src/native_auth.rs](src/native_auth.rs) | Verified discovery binding and native authorization browser policy. |
| [src/credential_store.rs](src/credential_store.rs) | Identity-bound Keychain/AndroidKeyStore refresh persistence and server-wide deletion fences. |
| [src/observability.rs](src/observability.rs) | All-build native OTLP traces, logs, and metrics, fail-open lifecycle, test isolation, and privacy controls. |
| [src/policy.rs](src/policy.rs) | External URL and local navigation security policies. |
| [capabilities/](capabilities/) | Minimal local main-WebView IPC grants. |
| [tauri.conf.json](tauri.conf.json) | Desktop and Android shell configuration. |
| [src/app_container.rs](src/app_container.rs) | Confined IPC and OS/architecture-specific runtime frame CSP policy. |
| [src/native_apps.rs](src/native_apps.rs) | Normal Apple Silicon macOS container lifecycle and optional diagnostic probe listener. |
| [tauri.native-apps-test.conf.json](tauri.native-apps-test.conf.json) | Optional diagnostic configuration and separate profile identifier. |
| [src/native_apps_navigation.rs](src/native_apps_navigation.rs) | Frame-aware WKNavigationDelegate for Apple Silicon macOS. |
| [../../docs/testing.md#macos-native-apps-verification](../../docs/testing.md#macos-native-apps-verification) | Normal-app live acceptance and optional sanitized native diagnostics. |
| [gen/android/app/src/main/AndroidManifest.xml](gen/android/app/src/main/AndroidManifest.xml) | Notification permissions, private foreground service, and backup policy references. |
| [gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt](gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt) | Local notification gate, client lifecycle, warm OAuth callbacks, and secure-store initialization. |
| [gen/android/app/src/main/java/dev/waltr/app/SecureTokenStore.kt](gen/android/app/src/main/java/dev/waltr/app/SecureTokenStore.kt) | Native-only AndroidKeyStore encryption and atomic refresh-token records. |
| [gen/android/app/src/main/java/dev/waltr/app/WaltrForegroundService.kt](gen/android/app/src/main/java/dev/waltr/app/WaltrForegroundService.kt) | Same-process Android embedded-server service and persistent notification. |

# Boundaries

This crate owns the Tauri shell, server profiles, lazy embedded server, native client transport, access host adapters, secure credentials, and IPC.

Local uses the private generation bearer. Remote profiles use shared native access after verified URL-only discovery. See [native access](../../docs/architecture/access-authentication.md#tauri-native-host-adapter) and [server selection](../../docs/architecture/clients.md#native-server-selection-frontend).

Keep the ephemeral loopback endpoint and random 256-bit local bearer in Rust. Never expose either value through IPC, logs, telemetry, Kotlin, or WebView state.

Keep embedded configuration and lifecycle `Debug` output free of endpoints, bearers, app-private paths, and task or thread handles.

Remove the exact endpoint and bearer from every snapshot before IPC publication. This projection must not mutate shared client state.

# Dependencies

| Path | Info |
| --- | --- |
| [../waltr-server/](../waltr-server/) | Ambient-free embedded server composition and local protocol admission. |
| [../waltr-client/](../waltr-client/) | Shared client reducer and `NativeGrpcTransport`. |
| [../waltr-web/dist](../waltr-web/dist) | Packaged local frontend assets. |

# Contracts

Keep one active client runtime generation. Preserve initial-before-update delivery, bounded latest-value snapshot coalescing, ordered command dispatch, stale-generation rejection, and complete disposal.

Fence starts with selection tickets. Require the current generation for dispatch, access, openers, and disposal effects.

Share one nonqueued audio reservation across chat and holds. Keep PCM and credentials outside IPC.
Use the desktop `interaction` driver for capture, Accepted-gated audio upload, concurrent output, cancellation, and native joins in both consumers.
Keep chat status/preview and desktop typing/commands as separate output consumers, not shortcut-shaped chat bindings.
Retain the app's process-owned shortcut registrar, ingress epochs, and permission tickets across runtime generations; the caller-driven desktop Actor is not its replacement.
Recheck generation, access, cancellation, and credential revision before capture, RPC, and output-action effects.
Require the same guards immediately before each actual native text batch.
Keep live external text with suffix revisions separate from native chat preview.
Preserve exactly one command or `typing` action; never add a fallback, retry, replay, clipboard path, or automatic submission.
Cancel audio before teardown or Android remote-pause preservation. Drain audio tasks before logout credential deletion and disposal completion.
Keep UI lifecycle retirement nonblocking. Publish bounded selected-port preview only through current identity, access, cancellation, and credential guards.
Clear preview on cancellation or terminal completion. Publish final text only after successful clean completion.

Keep generic client-hook activity in the bridge, independent of the audio host.
Keep microphone admission audio-only; do not gate unrelated hooks behind it.
The current shortcut adapter supports audio holds, not arbitrary device context.
Use `ActivityTracker::begin` for each admitted client hook operation.
Retain its lease through permission, RPC, output actions, and complete cleanup.
Mark failures explicitly; ordinary cancellation is not an error.
Keep activity text-free and independent of server-wide Routine history or WebView state.
Keep the tray optional and its menu limited to status, Open Waltr, and Quit.
Preserve normal window-close and exit behavior.
See [external text](../../docs/architecture/clients.md#live-external-text) and [platform prerequisites](../../docs/deployment.md#desktop-tray-and-external-text) for canonical constraints.

Start the embedded server only for Local selection. Retain an active local server across remote switches. Remote selection requests no Android service or notification.

Keep profiles nonsecret in app-data `servers.json`. Record last success only from an authenticated shared snapshot. Never remove Local.

Bind refresh storage to server ID, normalized endpoint, exact discovered issuer, client ID, effective scope set, and audience. Use Keychain on macOS and AndroidKeyStore-encrypted atomic files on Android. Other desktop targets report unavailable; add no plaintext fallback.

Stop auth tasks before server-wide credential deletion. Fence old store handles against detached writes. Delete secure credentials before profile removal; preserve the registry entry on deletion failure. Switch and ordinary disposal retain refresh credentials.

Forward only bounded warm Android intents to the current coordinator. Keep exact callback/state validation in shared native access. Do not restore cold-start OAuth attempts.

Keep native access progress private to IPC sequencing; web presentation follows shared snapshots.

Use explicit app data and config roots for all embedded state. Resource overrides live exactly at app config `server/.waltr/resources`.

After the server prebinds loopback, atomically overwrite `server/.waltr/resources/mcp-servers/waltr.yaml` with the generation URL before ordinary catalog load. The endpoint is durable on disk and refreshed each generation; only the bearer remains memory-only.

Use the fixed bounded models.dev connect and request timeouts. On failure or timeout, prefer a valid app cache. If the cache is absent, explicitly select the curated built-in seed and mark it stale; malformed or unreadable cache state must still fail startup. Do not expose host-configurable catalog timeout policy.

Desktop and Android load the same ordinary bundled and persisted resource catalog. Desktop compiles the server-side shell tool; Android does not. Android still accepts and exposes shell-bearing definitions, but runtime construction omits the unavailable shell tool and feature-derived shell tool permissions. Stdio MCP resources materialize and run through the ordinary MCP runtime on both targets.

Android notification permission gates foreground-service startup. The non-exported same-process `specialUse` service owns the server and survives activity pause or task removal best effort.

Local activity pause stops only the client runtime. Local resume creates a fresh client generation against the current server generation. Remote pause preserves its runtime and native OAuth attempt; remote resume requests no local server.

Suspend may preserve one unexpired pending outbound authorization attempt for only the immediate replacement client generation against the same server generation. Keep that handoff process-local, single-use, stale-safe, and secret-safe; ordinary stop, disposal, terminal state, skipped generations, and server replacement must clear it.

Never join Rust client or server threads inside Android lifecycle callbacks. Signal stop and enqueue client retirement there. Keep one managed `RetirementQueue` background reaper per bridge. Remove completed retirement records automatically. Await tracked completion barriers under `selection.operation` for IPC disposal and replacement. Preserve exact-generation server reaper threads separately.

For a nonblocking server stop, transition the exact current generation to `Stopping` and detach its thread under the controller lock. Its reaper must join that exact thread before publishing `Stopped` or `Failed`.

Make `reserve_start` wait through `Stopping`. Embedded server generations must never overlap their shared app-private roots, while Android callbacks remain nonblocking because the reaper owns the join.

Apply notification permission before a fresh Local generation uses the current server. Keep permission-required Local generations disconnected until permission changes. Remote generations bypass this gate.

Publish permission transitions to active client generations.

Ticket every Android client start and bind successful ownership to its exact bridge generation.

Suspend must invalidate in-progress ownership. Resume must create a replacement ticket.

Stop stale or suspended completions by their exact generation. Never stop or claim a replacement generation.

Promote `WaltrForegroundService` before native library load. Start native server work only after foreground promotion and library load.

Do not promise service survival after force-stop, process kill, reboot, or user termination. Durable state handles reopen and Routine restart reconciliation.

Exclude all Android private files, databases, preferences, and root data from cloud backup and device transfer.

Only packaged local content may invoke IPC. Deny remote main-WebView navigation and all new windows; external opening must pass the Rust HTTPS URL policy.

Normal Apple Silicon macOS builds enable Apps without a diagnostic feature, disposable profile, or prior isolation report.
Keep other native platforms unsupported; make no Intel or universal-package promise.
Preserve the Apple Silicon macOS container-only frame CSP and exact current-generation WKNavigationDelegate policy.
Apply `configure_apps_csp` to `context.config_mut()` before the Tauri builder consumes the context.
Keep the base configuration and unsupported targets, including Intel macOS, at `frame-src 'none'`.
Change only `frame-src` in configured normal and development CSP; optional diagnostics add only the probe frame URL.
Return `nativeApps.containerUrl` only after the WKNavigationDelegate installation acknowledges success.
Treat that acknowledgement as runtime security enforcement, not a release-validation gate.
Restrict optional `native-apps-test` diagnostics to Apple Silicon macOS and the disposable identifier/configuration.
Keep asset URLs separate from every selected API endpoint and bearer. Own trusted Apps assets per client runtime, independent of the embedded server. Detach the exact WK policy before asset shutdown on switch or disposal.
Preserve exact current-generation subframe policy, packaged main navigation, popup denial, and owned listener shutdown.
Pipe diagnostic-harness output directly through `scripts/native-apps-evidence.mjs`; never retain or `tee` raw invalid-key diagnostics.
Unit tests and sanitizer reports do not prove live WKWebView behavior; neither is a prerequisite for normal Apps enablement.

If an outbound authorization URL contains the local endpoint, remove it from the snapshot. Retain it only in Rust for the current pending attempt.

Open that URL only from an opaque current attempt ID. Recheck generation, attempt identity, lifecycle, and external HTTPS policy.

Generate Android legacy and adaptive launcher resources from the web favicon with `scripts/android-launcher-icons.sh`. Keep both committed Android resource trees synchronized, isolate the adaptive foreground glyph from its full-bleed background, and leave the monochrome notification icon independent.

Use the callback derived from the prebound server origin. Do not read `WALTR_PUBLIC_URL` or target OAuth redirect variables for embedded composition.

Use no ambient private-test destination switch. Pass only explicit trusted private origins to embedded composition.

Treat outbound callback markers and DCR registration as server-generation state. Reuse a valid DCR registration for reauthorization within the same generation, but require fresh registration after a replacement generation rejects stale attempts and credentials. Stale attempt status and cancel return not found.

Reject self-MCP credential logout with failed precondition because its generation credential is host-owned. Keep the resource filesystem-owned and subject to ordinary inspection, replacement, watching, reset, and ETags. Exact URL binding must prevent the host bearer from being sent after an edit changes the endpoint. Reset may fail ordinary candidate validation when the bundled fallback needs `WALTR_PUBLIC_URL`; add no embedded reset rule.

Keep WebView Faro separate from native Rust telemetry. Faro retains its production/release gate. Native traces, logs, and metrics initialize in every non-test app build, including local and debug builds. Tauri enables `waltr-client/native-tracing`, so Rust owns gRPC spans and W3C propagation.

Initialize `rustls-platform-verifier` from the Android foreground-service JNI entry point with the service context before starting any embedded-server networking. Package its Cargo-bundled JVM artifact through Cargo metadata and retain its classes under release minification. Keep this initialization for networking paths that still use platform verification.

On Android, use the ring provider, safe TLS 1.2/1.3, and bundled `webpki_roots` for outbound native Waltr access and OTLP; server-owned models.dev catalog refresh and provider and upstream-MCP OAuth discovery, registration, exchange, and revocation; agent provider and MCP OAuth refresh; OpenAI and Anthropic HTTP inference; and agent MCP Streamable HTTP. OpenAI websocket already uses explicit ring and bundled roots on every platform. Retain normal certificate-chain and hostname verification. Do not add Android platform, user, or private roots or `SSL_CERT_FILE` to these clients. Preserve default/platform reqwest behavior for non-Android paths that previously used it.

Use standard OpenTelemetry SDK providers: batched span and log exporters plus a periodic metric exporter. Keep the fixed OTLP/HTTP protobuf URLs at `/v1/traces`, `/v1/logs`, and `/v1/metrics` under `https://telemetry.holdenitdown.net:4318`. Preserve service name `waltr-app-native` and resource version, sanitized revision, and target. Keep the bounded `app.startup` log and `waltr.app.startups` metric.

Allow trace targets from this app and `waltr_client::transport::native_trace`, plus spans with the exact targets `waltr_server::routine::telemetry` and `waltr_audio::telemetry`. Those server/audio targets admit spans only, not logs. Allow log targets from this app. Allow INFO/WARN events from the exact target `waltr_client::native_access::telemetry` in both trace and log export. Keep those `native.auth` events limited to bounded `stage`, bounded `category`, and process-local numeric `generation` fields. This target exception excludes spans, ERROR/DEBUG events, nested targets, and lookalike targets. Exclude arbitrary client events and secret values. Use the shared blocking Rustls client for all exporters. Configure it explicitly with the ring provider, TLS 1.2/1.3, and bundled `webpki_roots`; retain normal chain and hostname verification. Do not add platform roots, private or user CAs, or `SSL_CERT_FILE` to that telemetry client. Disable redirects and strip every inherited or environment-derived header except the canonical protobuf content type.

Telemetry must exclude credentials, local endpoints, query strings, commands, snapshots, content, arbitrary error or panic text, bodies, and stable user or device IDs. Keep provider setup and export fail-open. Bound trace and log shutdown requests to two seconds. Preserve the OpenTelemetry SDK 0.32 fixed five-second metric shutdown bound. Keep automated Rust tests isolated through disabled native initialization and deterministic in-memory exporters. Do not synchronously flush native telemetry from Android suspend callbacks.

On Android embedded-server startup failure, retain the server's sanitized build category through the controller and emit only the fixed startup-failure event, outcome, and bounded category. A caught JNI panic marks the startup operation span but relies on the global panic hook for its sole panic error event. Never attach inner error text or configuration values.
