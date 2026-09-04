# Index

| Path | Info |
| --- | --- |
| [src/server_controller.rs](src/server_controller.rs) | Process-owned embedded server, app-private paths, ephemeral loopback endpoint, and generation bearer. |
| [src/bridge.rs](src/bridge.rs) | Rust-owned local client runtime, generation lifecycle, and bounded publication. |
| [src/observability.rs](src/observability.rs) | All-build native OTLP traces, logs, and metrics, fail-open lifecycle, test isolation, and privacy controls. |
| [src/policy.rs](src/policy.rs) | External URL and local navigation security policies. |
| [capabilities/](capabilities/) | Minimal local main-WebView IPC grants. |
| [tauri.conf.json](tauri.conf.json) | Desktop and Android shell configuration. |
| [gen/android/app/src/main/AndroidManifest.xml](gen/android/app/src/main/AndroidManifest.xml) | Notification permissions, private foreground service, and backup policy references. |
| [gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt](gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt) | Notification gate and activity-owned client lifecycle. |
| [gen/android/app/src/main/java/dev/waltr/app/WaltrForegroundService.kt](gen/android/app/src/main/java/dev/waltr/app/WaltrForegroundService.kt) | Same-process Android embedded-server service and persistent notification. |

# Boundaries

This crate owns the Tauri shell, embedded server lifecycle, local client transport, and IPC adapter. It never uses native Waltr access OAuth.

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

Use explicit app data and config roots for all embedded state. Resource overrides live exactly at app config `server/.waltr/resources`.

After the server prebinds loopback, atomically overwrite `server/.waltr/resources/mcp-servers/waltr.yaml` with the generation URL before ordinary catalog load. The endpoint is durable on disk and refreshed each generation; only the bearer remains memory-only.

Use the fixed bounded models.dev connect and request timeouts. On failure or timeout, prefer a valid app cache. If the cache is absent, explicitly select the curated built-in seed and mark it stale; malformed or unreadable cache state must still fail startup. Do not expose host-configurable catalog timeout policy.

Desktop and Android load the same ordinary bundled and persisted resource catalog. Desktop compiles the server-side shell tool; Android does not. Android still accepts and exposes shell-bearing definitions, but runtime construction omits the unavailable shell tool and feature-derived shell tool permissions. Stdio MCP resources materialize and run through the ordinary MCP runtime on both targets.

Android notification permission gates foreground-service startup. The non-exported same-process `specialUse` service owns the server and survives activity pause or task removal best effort.

Activity pause stops only the client runtime. Activity resume creates a fresh client generation against the current server generation.

Suspend may preserve one unexpired pending outbound authorization attempt for only the immediate replacement client generation against the same server generation. Keep that handoff process-local, single-use, stale-safe, and secret-safe; ordinary stop, disposal, terminal state, skipped generations, and server replacement must clear it.

Never join Rust client or server threads inside Android lifecycle callbacks. Signal stop there and complete joins on reaper threads.

For a nonblocking server stop, transition the exact current generation to `Stopping` and detach its thread under the controller lock. Its reaper must join that exact thread before publishing `Stopped` or `Failed`.

Make `reserve_start` wait through `Stopping`. Embedded server generations must never overlap their shared app-private roots, while Android callbacks remain nonblocking because the reaper owns the join.

Apply notification permission before a fresh generation uses an existing server. Keep permission-required generations disconnected until permission changes.

Publish permission transitions to active client generations.

Ticket every Android client start and bind successful ownership to its exact bridge generation.

Suspend must invalidate in-progress ownership. Resume must create a replacement ticket.

Stop stale or suspended completions by their exact generation. Never stop or claim a replacement generation.

Promote `WaltrForegroundService` before native library load. Start native server work only after foreground promotion and library load.

Do not promise service survival after force-stop, process kill, reboot, or user termination. Durable state handles reopen and Routine restart reconciliation.

Exclude all Android private files, databases, preferences, and root data from cloud backup and device transfer.

Only packaged local content may invoke IPC. Deny remote main-WebView navigation and all new windows; external opening must pass the Rust HTTPS URL policy.

If an outbound authorization URL contains the local endpoint, remove it from the snapshot. Retain it only in Rust for the current pending attempt.

Open that URL only from an opaque current attempt ID. Recheck generation, attempt identity, lifecycle, and external HTTPS policy.

Generate Android legacy and adaptive launcher resources from the web favicon with `scripts/android-launcher-icons.sh`. Keep both committed Android resource trees synchronized, isolate the adaptive foreground glyph from its full-bleed background, and leave the monochrome notification icon independent.

Use the callback derived from the prebound server origin. Do not read `WALTR_PUBLIC_URL` or target OAuth redirect variables for embedded composition.

Use no ambient private-test destination switch. Pass only explicit trusted private origins to embedded composition.

Treat outbound callback markers and DCR registration as server-generation state. Reuse a valid DCR registration for reauthorization within the same generation, but require fresh registration after a replacement generation rejects stale attempts and credentials. Stale attempt status and cancel return not found.

Reject self-MCP credential logout with failed precondition because its generation credential is host-owned. Keep the resource filesystem-owned and subject to ordinary inspection, replacement, watching, reset, and ETags. Exact URL binding must prevent the host bearer from being sent after an edit changes the endpoint. Reset may fail ordinary candidate validation when the bundled fallback needs `WALTR_PUBLIC_URL`; add no embedded reset rule.

Keep WebView Faro separate from native Rust telemetry. Faro retains its production/release gate. Native traces, logs, and metrics initialize in every non-test app build, including local and debug builds. Tauri enables `waltr-client/native-tracing`, so Rust owns gRPC spans and W3C propagation.

Initialize `rustls-platform-verifier` from the Android foreground-service JNI entry point with the service context before starting any embedded-server networking. Package its Cargo-bundled JVM artifact through Cargo metadata and retain its classes under release minification.

Use standard OpenTelemetry SDK providers: batched span and log exporters plus a periodic metric exporter. Keep the fixed OTLP/HTTP protobuf URLs at `/v1/traces`, `/v1/logs`, and `/v1/metrics` under `https://telemetry.holdenitdown.net:4318`. Preserve service name `waltr-app-native` and resource version, sanitized revision, and target. Keep the bounded `app.startup` log and `waltr.app.startups` metric.

Allow trace targets only from this app or `waltr_client::transport::native_trace`. Allow log targets only from this app. Use the shared blocking Rustls client for all exporters. Configure it explicitly with the ring provider, TLS 1.2/1.3, and bundled `webpki_roots`; retain normal chain and hostname verification. Do not add platform roots, private or user CAs, or `SSL_CERT_FILE` to that telemetry client; the process-level Android verifier initialization is only for ordinary reqwest users. Disable redirects and strip every inherited or environment-derived header except the canonical protobuf content type.

Telemetry must exclude credentials, local endpoints, query strings, commands, snapshots, content, arbitrary error or panic text, bodies, and stable user or device IDs. Keep provider setup and export fail-open. Bound trace and log shutdown requests to two seconds. Preserve the OpenTelemetry SDK 0.32 fixed five-second metric shutdown bound. Keep automated Rust tests isolated through disabled native initialization and deterministic in-memory exporters. Do not synchronously flush native telemetry from Android suspend callbacks.

On Android embedded-server startup failure, retain the server's sanitized build category through the controller and emit only the fixed startup-failure event, outcome, and bounded category. A caught JNI panic marks the startup operation span but relies on the global panic hook for its sole panic error event. Never attach inner error text or configuration values.
