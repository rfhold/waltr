# Native client

This crate owns the Tauri shell and native consumers of the shared client runtime. It does not own the Routine protocol or server admission policy. See [client architecture](../../docs/architecture/clients.md) and [testing](../../docs/testing.md).

## Native sign-in

Remote sign-in restores native refresh credentials without exposing tokens or storage paths to the WebView. macOS/Linux use owner-only local files, Android retains AndroidKeyStore encryption, and Windows persistence remains unavailable. Desktop cutover requires a fresh sign-in; no Keychain migration or fallback exists. Pending restoration announces “Restoring sign-in…” while ordinary shared snapshots continue to control access. See the [native access contract](../../docs/architecture/access-authentication.md#tauri-native-host-adapter) for filesystem protection and durable revocation limits.

## Routine consumers

Desktop holds accept `playback: {}` as an exclusive output action alongside `typing` and `command`.
The selected output must require `pcm16le-24000-mono` audio.
Playback prefers the exact source format on the default device.
For 24 kHz mono sources only, it can adapt to supported 48 kHz stereo output.
The shared CPAL owner checks hold cancellation, generation, access, and credential revision before physical output.
Release ends capture, then waits for the answer and verified playback-tail drain.
Cancellation joins capture, RPC cleanup, and playback before the hold releases its reservation.
See the [audio question example](../../docs/examples/routines/audio-interactions.md#audio-question-shortcut).

Chat dictation and desktop holds use the desktop interaction driver, the current shared definition catalog, and one nonqueued microphone reservation. Each operation pins its definition revision, limits, source session, and credential revision. Relevant catalog or access changes cancel and join the operation before replacement; unrelated snapshots do not cancel it. Chat preview is separate from shortcut typing and command effects.

The native media host uses `NativeGrpcTransport` and shared attachment, upload, output, and history readers. Admission validates the complete metadata before RPC. Binary uploads wait for Accepted, use negotiated chunks of at most 64 KiB, and commit quarantine before binding an input. Server validation remains authoritative.

## Native media IPC

`routine_media` accepts the current generation and an operation. Replies contain `{generation, value}`. The native frontend adapter uses these additive operations:

| Operation | Arguments | Result |
| --- | --- | --- |
| `pickInput` | `request: {port, kind, contentTypes, maxBytes}` | `{handle, port, name, contentType, bytes, purpose}` or null |
| `releaseInput` | `handle` | null |
| `pickOutput` | `request: {port, suggestedName}` | `{port, handle}` or null |
| `releaseOutput` | `handle` | null |
| `openNative` | `metadata`, `inputs: [{port, handle}]`, `outputs: [{port, kind: 'file', handle}]` or playback selections `{port, kind: 'playback'}` | operation ID |

`metadata` is the existing shared-client metadata JSON string, including an exact revision, entrypoint, parameters, correlation, declarations, and optional source session. Handles are generation-bound, single-use bindings; they convey no server grant authority. A picked name is a display basename, never a path. File and image limits are 64 MiB and 20 MiB respectively, further restricted by the request and descriptor. MIME hints do not replace quarantine validation.

The existing text, status, upload, download, cancellation, and durable-text operations share the same host. `routine_media_append` accepts raw file bytes with generation, owner, operation, and port headers. `routine_media_bytes` returns raw file bytes. Both bound chunks to 64 KiB. Neither carries PCM, JSON byte arrays, base64, credentials, paths, or private reader grants.

## Native ownership and cleanup

On macOS, Linux, and Windows, Rust owns the dialog backend, selected file handles, save destinations, and CPAL playback. The WebView has no dialog or filesystem plugin permission. Saves stage `create_new` content in a random exclusive sibling directory (0700 on Unix), then publish with an atomic no-overwrite hard link. Unsupported hard links fail closed. Cleanup touches only that private namespace, never the selected destination path. This assumes a trusted user-controlled parent and no hostile same-user modification of the private namespace or its ancestors; it is not a general pathname-race defense. Multi-file publication is not atomic: an already published file is never deleted to roll back another failure.

Playback has four credited source packets of at most 64 KiB and one stream-owner thread. An empty queue emits silence and returns to priming, allowing pauses between spoken progress updates without cancelling the operation. Playback resumes after two queued packets or an explicit end marker; the end marker also allows a final single packet to play. An explicit end marker drains queued data, then retains the stream through the last callback's backend playback timestamp plus its buffer duration. Invalid timing, a timing horizon above two seconds, or missing callbacks fail rather than claim a verified tail. These backend timestamps are not physical speaker proof. Access and cancellation checks precede sample effects.

Playback prefers the exact source format. Its only adaptation converts 24 kHz mono PCM to supported 48 kHz stereo output.
The owner uses causal linear interpolation across packet boundaries, then duplicates each sample into both channels.
It emits the midpoint and current sample for each source sample; the first midpoint repeats the first sample.
This first-order filter has limited image rejection; it is not the capture path's sinc filter.
No deferred samples remain at EOF. Four retained credits bound converted packets to 256 KiB each, plus one source packet during conversion.
Device-clock drain uses the output rate and channel count. Unsupported alternatives fail closed.

Bridge work is nonqueued beyond its bounded ingress: one raw-data credit, eight ordinary request credits, and a separate cancellation credit. JSON metadata is capped at 1 MiB before typed cloning, so those nine bridge requests retain at most 9 MiB of encoded metadata; the raw request retains at most 64 KiB of bridge payload. This is not a whole-process allocation bound. Credits remain with workers through cleanup even if callers drop their replies. One picker credit stays with the OS callback until it returns. Same-generation catalog and selection invalidation revokes picker tickets independently of active media pins.

Binary reader leases have an absolute, nonrenewable maximum lifetime of 15 minutes. The pinned expiry stops checkout publication and pending reads; operation deadlines and attachment renewal continue while checkout waits. Save-sink checkout cleanup waits up to 30 seconds; a missing response leaves cleanup unverified.

Each raw download has a runtime-owned task, retained in a `JoinSet`, that owns checkout, reads, and reader closure without holding the raw-state mutex during RPC. Cancellation, expiry, or a dropped waiter cannot discard a pending checkout: a late reader closes without publishing metadata or bytes. Raw close and drain wait against one fixed 30-second cleanup deadline, then report unverified if closure is unfinished. The actual task and request credits remain owned until closure finishes. A failed close retains the reader and credits in the retiring runtime without automatic retries; checkout errors and task panics never count as verified closure. Runtime teardown joins these tasks before its thread exits. An unanswered RPC or failed close can therefore retain background retirement work indefinitely, within the existing credit bound.

Logout revokes effects before waiting. Unverified cleanup returns a fixed failure without stopping native access, clearing the bearer, deleting stored credentials, or dispatching shared logout. Retained owners can use those credentials to finish late cleanup. An explicit logout retry rechecks retained receipts and joins actual download tasks before changing credentials; no automatic logout follows a failed attempt. Coupled access-command barriers also reject unverified cleanup.

Durable text readers have a separate history lifetime, a 1 MiB result bound, and UTF-8 byte offsets. Access, credential, generation, and relevant selection changes revoke all affected effects before cleanup waits. UI lifecycle callbacks retire work through the existing background queue rather than joining threads directly. Native OS dialogs cannot be forcibly joined; stale callbacks cannot open files or install handles. Admission and first Accepted share one absolute 20-second deadline.

Mobile native media is explicitly unsupported. Ordinary mobile chat dictation and hold policy remain separate. Automated tests use direct v2 fake services, temporary files, and a fake playback callback without activating physical devices. Backend compilation and these tests do not prove physical playback, microphone, typing, or live dialog behavior. Production service route composition belongs to the server, not this crate.
