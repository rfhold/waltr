# Routine web adapter

The web consumes Routine v2 snapshots and commands from `waltr-client`.
The shared client owns catalog watches, recent history, explicit request pages, historical detail, and cancellation receipts.
The web owns JSON drafts, file pickers, microphone devices, playback, and bounded download presentation.

`RoutineJson`, revisions, IDs, and cursors retain their public string representation.
The JSON editor validates the closed schema dialect and submits the original JSON text.
Historical detail uses the pinned definition, not current catalog authority.

## Browser media

`src/platform/media.ts` mirrors actual WASM exports.
`src/audio/live.worklet.js` emits incremental 24 kHz mono PCM16LE chunks.
One transport acknowledgment returns one worklet credit.
The worklet retains at most one additional 64 KiB buffer, then reports overflow.
Normal release flushes the tail and closes input; output remains live.
Open returns a local owner ID before network admission; one original monotonic 20-second deadline covers Open through Accepted.
Cancellation interrupts pending admission and joins the local owner before releasing browser devices and busy state.
Unknown remote admission effects remain an error with unverified cleanup, not success; only an explicit new gesture can start another owner.
Success requires remote terminal, clean EOF, remote cleanup proof, and local device teardown.
Each interaction pins its adapter capability; access loss, catalog invalidation, and adapter replacement cancel that owner, never a replacement generation.
Active downloads block replacement through reader closure, and cancelled reads cannot publish late object URLs.
Dictation reconciles its pinned revision, entrypoint, selected ports, parameters, and selection configuration before snapshot publication and further media effects.
Unrelated catalog and configuration changes preserve that owner.

File declarations precede admission and include the exact port, purpose, MIME, name, and finite size.
Uploads use sequential `File.slice` chunks of at most 64 KiB.
The server validates quarantine, digest, and image content; the browser does not assert content safety.
Other files remain storage-only, without PDF extraction, URL fetch, or transcoding.

Output metadata contains no PCM arrays or encoded binary payloads.
Typed output chunks feed bounded PCM playback at the declared channel count and sample rate.
Priming may remain idle; an active open stream that exhausts its scheduled audio fails and cancels its owner.
Explicit stream close permits the scheduled final tail to drain before success; intentional drop does not report an underrun.
The Blob-save fallback requires an explicit gesture and caps each file at 64 MiB.
Download grants remain private, single-use, and nonrenewable; descriptors do not grant historical downloads.
Abort and the monotonic grant deadline invoke the owned reader close control even during checkout or a blocked read.
A failed close prevents Blob publication and blocks replacement until explicit cleanup verification succeeds; no automatic retry occurs.
Durable text readers have separate 1 MiB per-result and 8 MiB aggregate presentation budgets.

## Native IPC contract: opaque file bindings

`src/platform/native-media.ts` supplies the optional `MediaBridge` capability.
Rust must advertise `routineMedia: true` in its runtime reply only after it implements this contract.
Without that flag, the frontend exposes no media capability and uses no HTTP or WASM fallback.
Native microphone capture remains in Rust through the separate dictation capability.
Native PCM capture and playback stay in Rust; neither input nor output PCM crosses IPC.
The generic native media capability supports file chunks and text, not browser microphone exports.
Its optional `nativeFiles` capability owns native pickers, one-use source handles, and explicit output destinations.
The web uses native picker display metadata, never browser file inputs or local paths, when this capability exists.
Rust consumes audio output and includes its playback teardown in the final cleanup proof.
Rust omits audio stream opens, chunks, and closes from `outputMetadata`.
`dropPlayback` cancels and joins the same native output owner independently of producer credit.

| Command | Request | Reply |
| --- | --- | --- |
| `routine_media` | `{generation, operation, ...arguments}` | `{generation, value}` |
| `routine_media_bytes` | `{generation, operation: "download", id, port}` | Raw file `ArrayBuffer`, at most 65536 bytes; empty means EOF |
| `routine_media_append` | Raw file `Uint8Array`, 1–65536 bytes | Acknowledgment after producer credit |

Append headers contain `x-waltr-generation`, `x-waltr-operation`, `x-waltr-owner`, and URI-encoded `x-waltr-port`.
The append operation is `upload`; no PCM append operation exists.
`native-media.ts` defines the exact operation names and argument keys.
`media.ts` defines each value shape and lifecycle operation.
The native `open` operation allocates its numeric owner before remote admission and returns without network waits.
Its `status` remains `opening` until the remote Accepted event.
The native owner supports cancellation during admission; its generation disposal also cancels that admission.

| `routine_media` operation | Arguments beyond `generation` and `operation` | `value` |
| --- | --- | --- |
| `open` | `metadata`: JSON string | Numeric owner ID |
| `openText` | `id`, `port`, `stream` | Unit |
| `text` | `id`, `stream`, decimal-string `sequence`, JSON-string `revision` | Unit |
| `closeStream` | `id`, `stream`, numeric `nextSequence` | Unit |
| `finishInput` | `id` | Unit; preserves output ownership |
| `status` | `id` | JSON-string `MediaStatus` |
| `outputMetadata` | `id` | Metadata JSON string or null; no audio stream events |
| `beginUpload` | `id`, `port` | Unit |
| `commitUpload` | `id`, `port` | Public descriptor JSON string; also sends BlobInput |
| `beginDownload` | `id`, `port` | Public descriptor JSON string; acquires the private single-use reader |
| `closeDownload` | `id`, `port` | Unit |
| `beginTextResult` | `invocation`, `selected` | Numeric reader ID |
| `readTextResult` | Reader `id` | Incrementally decoded UTF-8 string or null at EOF |
| `closeTextResult` | Reader `id` | Unit |
| `dropPlayback` | `id` | Boolean cleanup proof after native output cancellation and join |
| `cancel` | `id` | Boolean remote cleanup proof after local and remote join |

The native UI uses `openNative`, not the browser-shaped `open`, for file bindings and explicit playback selection.
Rust owns bound file upload and save work after `openNative`.
`finishInput` waits for bound uploads and BlobInput before it closes input; it does not cancel output.
Rust serializes text input with its bound uploads and preserves independent cancellation credit.

| Native file operation | Arguments beyond `generation` and `operation` | `value` |
| --- | --- | --- |
| `pickInput` | `{request: {port, kind: "file" | "image", contentTypes: string[], maxBytes: number}}` | `{handle, port, name, contentType, bytes, purpose: "file" | "image"}` or absent |
| `releaseInput` | `{handle}` | Unit |
| `pickOutput` | `{request: {port, suggestedName}}` | `{port, handle}` or absent |
| `releaseOutput` | `{handle}` | Unit |
| `openNative` | `{metadata, inputs: {port, handle}[], outputs: ({port, kind: "playback"} | {port, kind: "file", handle})[]}` | Numeric operation ID |

The adapter normalizes nullable picker and reader replies to undefined.
Picker cancellation is not an error; false playback cleanup proof rejects.
Picker metadata exposes a display basename and finite byte count; content type remains a hint.
The server retains authority over image validation and quarantine.
Source handles bind one use to a generation and port.
The web releases unused handles on replacement, entrypoint switch, cancellation, or disposal.
Rust also releases handles on generation disposal and rejects stale or mismatched bindings before RPC or device effects.
Output handles require an explicit user Save operation and `create_new`; no overwrite occurs.
Playback requires an explicit output selection; the web never selects it automatically.

Open metadata contains `routine`, `revision`, `entrypoint`, JSON-string `parameters`, `correlation`, nullable `sourceSession`, input declarations, and nullable `selectedTextOutput`.
Native declarations support text, file, and image inputs; native microphone selection uses the separate dictation API.
Status contains `operationId`, `phase`, `preview`, nullable `finalText`, and `cleanupVerified`.
The native owner reports success only after terminal, clean EOF, file/input teardown, native playback drain, and verified cleanup.
All frontend calls reject stale generations before dispatch and after completion.
Rust must fence the same generation before effects and cancel its owners on adapter disposal or access loss.
Rust retains attachment credentials, binary grants, reader handles, and transport endpoints.
No reply exposes a private endpoint, bearer, local path, or opaque authorization URL.

This contract has mock IPC tests, not native-platform media proof.
The accepted Rust app implementation owns native execution; these frontend tests do not prove packaged v2 routing or physical devices.

## Focused browser proof

Run `node scripts/routine-browser.contract.mjs` from this directory with the locked dependencies and installed Playwright Chromium.
The fixture uses the real WASM, browser microphone AudioWorklet, and checked-in v2 protobuf shapes.
Its fake gRPC-web service demonstrates live output before capture release and clean final completion.
The fixture also checks lease renewal, expiry during capture, blocked-input cancellation, lost acknowledgment without retry, failure on EOF without terminal, active playback underrun, and final-tail drain after explicit close.
Unanswered Open cancellation and deadline cases verify actual fetch termination, unverified remote cleanup, and a new owner only after an explicit gesture.
Output PCM passes through typed WASM bytes and the real browser playback sink.
The file scenario uses real browser File inputs, required PDF/PNG declarations, sequential 64 KiB uploads, commit/BlobInput, and a typed output download.
Additional file cases interrupt an actual blocked ReadBlob fetch on abort or pinned expiry, reject failed close without a Blob URL, and reject a reply queued beyond its original grant expiry.
The fake server checks protocol order and metadata, not authoritative image content validation.
It does not prove production route registration, server policy, real microphone hardware, or native WebView behavior.

These fourteen scenarios use the accepted shared-client ownership controls without changing the public MediaBridge DTOs or method signatures.
