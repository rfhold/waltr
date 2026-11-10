# Waltr TUI

The terminal shell consumes shared `waltr-client` snapshots and commands. It does not own Routine server state.

## Routine controls

- `Ctrl-X u` opens the catalog and invocation history.
- From session detail, the same shortcut selects that source session's history.
- `n` opens exact-revision manual entrypoints for the current global or session context.
- The parameter editor accepts complete JSON for the displayed versioned schema. `Tab` or `Ctrl-Enter` submits the exact input bytes.
- `r` selects the first authoritative history page. `o` or `End` selects the next page.
- `Tab` selects history. `Enter` opens durable invocation detail.
- Detail shows the pinned definition, dynamic operations, uncertainty, helper references, and business results.
- `c` requests cancellation. The receipt does not replace authoritative invocation status.
- `o` requests the next operation page. `Esc` releases detail receipts and returns to history.

Selected pages remain separate from the bounded recent watch window. Expired receipts require a fresh request.

## Finite media and text results

- `m` opens exact interactive entrypoints, filtered by global or current-session authorization. Event entrypoints are not manual controls.
- Enter selects an entrypoint. `Tab` switches between business-parameter JSON and local declarations. `Ctrl-Enter` starts; `Esc` closes and cancels ownership. While running, `c` requests independent cancellation.
- Local declarations contain `inputs`, `saves`, and `selected_text`. Each input names its declared port and either literal `text` or an explicit trusted local `path`. File/image selections also supply a MIME hint. Required declarations and known file sizes are checked before admission.
- For example, a file selection is `{"port":"document","path":"/chosen/report.pdf","mime":"application/pdf"}`. It works only if that exact port accepts that MIME. The full path stays local; only the basename and declaration metadata go to the server. MIME does not validate content.
- Files are capped at 64 MiB and images at 20 MiB, with smaller server/port limits applied. Images accept PNG, JPEG, and WebP metadata; the server validates their bytes. Helper attachments have a separate 8 MiB cap. PDF and UTF-8 files remain ordinary agent inputs, not a terminal extraction service. Unknown files are storage-only.
- Audio files require explicit `pcm24k_mono:true`, no MIME field, and raw PCM16LE at 24 kHz mono. There is no recorder, codec conversion, or playback sink.
- `saves` maps output port names to explicit **new** local destinations. Existing destinations are never overwritten. Binary saves require private live-interaction grants, bounded chunks, exact size/SHA-256, clean EOF, reader closure, and successful terminal completion. Expiry is pinned and never renewed. Unverified cleanup is reported, not treated as success.
- Output text can be selected with `selected_text`. In durable invocation detail, `t` opens a separate UTF-8 result reader; choose its result ID and a destination string or `null` for preview only. Results are bounded to 1 MiB each and 8 MiB aggregate encoded charge. Preview is capped at 64 KiB with explicit truncation; `y` explicitly copies only the displayed preview through the existing clipboard helper. A save preserves the full verified text.

Saves use private sibling staging and no-overwrite publication. Errors remove only owned partial files, never the chosen destination. Credentials and grants remain private to the runtime owner; leaving the route, changing access generation, or canceling ends its use. No ambiguous upload is retried.

## Limits and integration

The terminal has no microphone recorder, audio playback sink, or bitmap decoder. Saving raw audio is not playback; image descriptors provide metadata, not bitmap preview. Durable binary descriptors do not grant download authority.

The server-backed Routine watch test runs against the production v2 composition without an integration ignore. Generated-v2 Tonic fixtures in [src/media/tests/rpc.rs](src/media/tests/rpc.rs) separately exercise the native client over local TCP: negotiated uploads, parallel output reads, commit-before-binding, exact text/PCM streams, image declarations, cancellation, generation changes, clean EOF, digest/length failures, absolute expiry, and durable UTF-8 byte offsets. Those fake services do not prove server image policy or the eleven end-to-end workflows.

[Snapshot disposition](tests/snapshot-disposition.md) records unrelated committed expectation mismatches separately from Routine changes; those goldens have not been accepted.

See [CONTRIBUTING.md](../../CONTRIBUTING.md) for build commands and [the testing guide](../../docs/testing.md) for test layers.
