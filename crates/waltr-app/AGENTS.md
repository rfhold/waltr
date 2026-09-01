# Index

| Path | Info |
| --- | --- |
| [src/bridge.rs](src/bridge.rs) | Rust-owned client runtime, generation lifecycle, and bounded publication. |
| [src/observability.rs](src/observability.rs) | Release-only native OTLP tracing, fail-open lifecycle, and privacy controls. |
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

Keep WebView Faro separate from native Rust tracing. Tauri enables `waltr-client/native-tracing`, so Rust owns gRPC spans and W3C propagation. Telemetry must exclude credentials, callbacks, query strings, commands, snapshots, content, arbitrary error or panic text, bodies, and stable user or device IDs. Keep exporter setup fail-open and bounded. Do not synchronously flush native telemetry from Android suspend callbacks.
