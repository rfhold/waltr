# Index

| Path | Info |
| --- | --- |
| [src/bridge.rs](src/bridge.rs) | Rust-owned client runtime, generation lifecycle, and bounded publication. |
| [src/observability.rs](src/observability.rs) | All-build native OTLP traces, logs, and metrics, fail-open lifecycle, test isolation, and privacy controls. |
| [src/policy.rs](src/policy.rs) | External URL and local navigation security policies. |
| [capabilities/](capabilities/) | Minimal local main-WebView IPC grants. |
| [tauri.conf.json](tauri.conf.json) | Desktop and Android shell configuration. |
| [gen/android/app/src/main/AndroidManifest.xml](gen/android/app/src/main/AndroidManifest.xml) | Explicit private-scheme callback filter. |
| [gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt](gen/android/app/src/main/java/dev/waltr/app/MainActivity.kt) | Opaque Android lifecycle and Intent forwarding to JNI. |

# Boundaries

This crate owns the native Tauri shell and IPC adapter. Authorization URLs, Android callback URLs/codes/state, PKCE material, tokens, and credential lifecycle stay in Rust; UI payloads must remain secret-free. Android passes each callback `Intent` opaquely from `MainActivity` to JNI. Rust reads and validates the URI without a Tauri event, JavaScript binding, capability, or callback IPC.

# Dependencies

| Path | Info |
| --- | --- |
| [../waltr-client/](../waltr-client/) | Shared client reducer, native transport, and native access coordinator. |
| [../waltr-web/dist](../waltr-web/dist) | Packaged local frontend assets. |

# Contracts

Keep one active runtime generation. Preserve initial-before-update delivery, bounded latest-value snapshot coalescing, uncoalesced logout control ordering, ordered command dispatch, stale-generation rejection, and complete disposal.

Keep Android callback delivery nonblocking, one-entry latest-value bounded, and fenced to the active handoff generation. Preserve the callback-policy split: Android uses exact external `dev.waltr.app:/oauth2redirect`, while desktop remains loopback.

Mark the Android browser handoff before opening the external browser. Only the first browser-away pause for a pending unauthenticated attempt preserves the generation. The first resume consumes that exception and releases any held callback. Every later pause, authenticated pause, or suspended terminal result clears the runtime and credentials. Process death fails closed.

Only packaged local content may invoke IPC. Deny remote main-WebView navigation and all new windows; external opening must pass the Rust HTTPS URL policy.

Keep WebView Faro separate from native Rust telemetry. Faro retains its production/release gate. Native traces, logs, and metrics initialize in every non-test app build, including local and debug builds. Tauri enables `waltr-client/native-tracing`, so Rust owns gRPC spans and W3C propagation.

Use standard OpenTelemetry SDK providers: batched span and log exporters plus a periodic metric exporter. Keep the fixed OTLP/HTTP protobuf URLs at `/v1/traces`, `/v1/logs`, and `/v1/metrics` under `https://telemetry.holdenitdown.net:4318`. Preserve service name `waltr-app-native` and resource version, sanitized revision, and target. Keep the bounded `app.startup` log and `waltr.app.startups` metric.

Allow trace targets only from this app or `waltr_client::transport::native_trace`. Allow log targets only from this app. Use the shared blocking Rustls client for all exporters. Disable redirects and strip every inherited or environment-derived header except the canonical protobuf content type.

Telemetry must exclude credentials, callbacks, query strings, commands, snapshots, content, arbitrary error or panic text, bodies, and stable user or device IDs. Keep provider setup and export fail-open. Bound trace and log shutdown requests to two seconds. Preserve the OpenTelemetry SDK 0.32 fixed five-second metric shutdown bound. Keep automated Rust tests isolated through disabled native initialization and deterministic in-memory exporters. Do not synchronously flush native telemetry from Android suspend callbacks.
