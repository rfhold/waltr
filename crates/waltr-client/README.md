# Shared Routine client

Routine state uses the generated `waltr.routine.v2` contract. No v1 wire fallback exists in this crate.

## Shell API

| Shell action | Shared API or snapshot |
| --- | --- |
| Observe the current catalog | Register/release `RoutineDefinitionWatchInterest`; read `snapshot.routines.definitions`. |
| Choose an entrypoint | Use the exact definition revision, `entrypoints`, `startContexts`, and trigger family. |
| Render business parameters | Render `parameters.schema`; submit a `RoutineJson` string, distinct from media declarations. |
| Start a manual invocation | `Client::start_manual_routine` or `ClientCommand::StartManualRoutine`. |
| Observe all history | Register/release `RoutineInvocationWatchInterest`; read `snapshot.routines.history.recent`. |
| Observe one session | Register/release `SessionRoutineInvocationWatchInterest { session_id }`. |
| Browse history | `RoutineRequest::Invocations { after_cursor, limit, source_session }`. Start with cursor `0`; read `snapshot.routines.requests[key].selectedInvocations` and `nextCursor`. |
| Release a selected page | `Client::release_routine_request` or `ClientCommand::ReleaseRoutineRequest`, with the matching operation key and request identity. No RPC occurs. |
| Inspect an invocation | `LoadRoutineInvocationDetail`; read the historical `pinnedDefinition` and dynamic `operations`. |
| Browse operations | `RoutineRequest::Operations`; first load the invocation detail. |
| Cancel an invocation | `RoutineRequest::Cancel`; inspect the keyed receipt, not a speculative local status. |

`RoutineRequest` uses `ClientCommand::RoutineRequest` or `Client::request_routine`.
Each receipt has an operation key and request identity. Mutation receipts never replace authoritative history.

`RoutineRevision` serializes as a decimal string. It covers admission order, observation revisions, and wide byte counters.
Watch cursors remain opaque strings. They do not describe admission order or contiguous event numbers.
An expired watch cursor triggers an observation reset, not a new invocation.

The client retains 200 unpinned summaries, 32 start receipts, and 32 details.
Each detail retains its definition after catalog removal. At most 32 keyed request receipts retain selected authoritative pages: at most 100 rows per page and 200 rows in aggregate, separate from recent history.
Get/watch observations refresh selected rows by observation revision. When the selected-page budget is exceeded, the oldest completed page becomes `Expired`, with an error rather than a misleading empty `Ready` result.
Replacing or releasing a matching request drops its selected page. Late responses cannot restore a released request or replace a newer request identity.

`LoadOlderRoutineInvocations`, `Client::load_older_routine_invocations`, their `OlderPage*` events/effect, and `RoutinePageState` remain as compatibility shell scaffolding. They call only v2 ascending pagination despite their names. New shells must use `RoutineRequest::Invocations` instead. No v1 wire adapter remains.

## Native media

1. Build `InteractionLimits::for_definition` from the current catalog definition and server limits.
2. Declare every intended input before `RoutineInteractionTransport::interact`.
3. Poll `session.output` concurrently with capture and input.
4. Wait for `InteractionEvent::Accepted` before any input or upload.
5. Send exact `StreamOpen`, ordered chunks, and `StreamClose` values through `session.input`.
6. For files or images, call `input.begin_upload(&transport.media_transfers(), port)`.
7. Call `BlobUpload::begin`, bounded `append`, and `commit` before the returned `BlobInput` event.
8. On normal capture release, flush capture and call `finish_input`.
9. Keep the input owner alive while output continues.
10. Read `final_text(selected_port)` only after a successful terminal and clean EOF.

`InteractionControl::cancel` bypasses a full data queue. It wakes a blocked input writer.
The caller owns capture tasks and the output future. It must join those local tasks itself.
`cleanup_verified` proves the remote terminal/EOF contract. It does not prove that caller capture tasks stopped.
`cancel_and_join` drains the output contract; dropping a pending waiter does not create a cleanup proof.

## Browser media

`openMediaInteraction` accepts metadata JSON. It does not accept a complete audio recording.
Its promise returns a local operation ID before network admission. Status remains `opening` until Accepted; input is rejected until then. The retained driver owns Open through Accepted under one original 20-second monotonic deadline. Cancel can use that ID while Open is pending.
The metadata contains `routine`, `revision`, `entrypoint`, `parameters`, `correlation`, `sourceSession`, `inputs`, and `selectedTextOutput`.
`parameters` is an exact JSON string. The current catalog controls eligibility and schemas.

The browser uses unary admission, ordered unary input batches, one server-streamed output owner, and separate renewal/close calls.
It does not use HTTP request streams or WebSockets.

| Purpose | WASM exports |
| --- | --- |
| Incremental PCM | `openMediaPcmStream`, `pushMediaPcm`, `closeMediaStream` |
| Incremental text revisions | `openMediaTextStream`, `pushMediaTextRevision`, `closeMediaStream` |
| Normal input completion | `finishMediaInput` |
| Preview and final text | `mediaInteractionStatusJson` |
| Output event metadata | `takeMediaOutputJson` |
| Typed output PCM bytes | `takeMediaOutputBytes(id, chunkId)` returns `Uint8Array` once, after metadata checkout. |
| MIME-aware file/image input | `beginMediaUpload`, `appendMediaUpload`, `commitMediaUpload` |
| Single-consumer output download | `beginMediaDownload`, `readMediaDownloadChunk`, `closeMediaDownload` |
| Durable UTF-8 text results | `beginRoutineTextResult`, `readRoutineTextResultChunk`, `closeRoutineTextResult` |
| Explicit cancellation | `cancelMediaInteraction` |

PCM and upload calls accept chunks of at most 64 KiB. Producer calls take one immediate credit before copying payloads or awaiting transport; competing calls fail with `media producer busy` instead of waiting in a queue. Retry a rejected Busy call only after the previous call completes; do not retry an ambiguous transport failure.
Input streams and upload mutations share that producer credit. Reader and durable-text mutations also fail immediately when their lane is busy. Cancellation does not take a producer lock.
The Rust-owned output queue holds at most 32 events and 256 KiB of PCM, with at most 64 KiB per chunk. Audio JSON contains stream/sequence/chunk identity and byte length, never byte arrays or base64.
Metadata checkout retains the matching PCM until `takeMediaOutputBytes` consumes it. Checked-out chunks still count against both bounds; duplicate, unknown, or stale-owner access fails. Teardown releases retained bytes.
These bounds cover Rust-owned queues and copies, not the wasm-bindgen slice glue allocation before export admission or browser RSS.
`openMediaPcmStream(id, port, streamId, formatJson)` takes `formatJson` as `{"sampleRate": 16000, "channels": 1}`.
If the shell abandons that queue, the owner cancels instead of accumulating output.
Admission failure or owner termination releases only its own pending identity. Ignoring the returned promise does not abandon the retained driver; cancel by ID or dispose the client. A new explicit start is allowed after local termination; admission is never retried automatically.
An aborted Open with no acknowledgement has unverified remote cleanup. Local ownership is released only after its actual RPC future stops; it never produces final text. Known attachment authority is closed before the owner finishes, with a bounded cleanup wait.
A terminal stops renewal and begins a bounded EOF drain. Final text still requires valid closures, success, and clean EOF; lease ticks during that drain do not turn completion into cancellation. The drain ends after at most 30 seconds or the absolute expiry, whichever comes first.
The shell owns devices, file pickers, native audio sinks, and text insertion. This crate contains none of those UI policies.

## Blob and text results

`MediaTransfers::read_output` consumes a private output grant exactly once.
Browser downloads register an independent private owner before checkout. `closeMediaDownload(id, port)` interrupts pending checkout or read without taking a reader lock, including before a descriptor is returned. Downloads can outlive the invocation and its output owner.
The original observation-time grant expiry remains pinned across checkout and reads. Close waits at most 30 seconds for a real acknowledgement. An uncertain close rejects, retains private close authority and the advertised reader credit in quarantine until the original expiry, and never reports verified cleanup. Terminating a Rust waiter cancels its retained driver rather than discarding close authority; ignoring a JS promise does not remove the owner or its expiry deadline. At most 32 records per client are retained; live/quarantined drivers also share the advertised process-local `blobReaders` ceiling.
`read_input` uses a durable invocation/blob identity. Output history descriptors do not grant output access.
`BlobReader` validates ordered chunks, the exact byte count, one EOF marker, and clean stream closure.
Browser descriptor and chunk publication rechecks the original monotonic expiry even when a successful reply is already queued. A zero-TTL descriptor is rejected; expiry signals the existing owner without claiming cleanup.
It offers no offset resume. Binary output leases expire after at most 15 minutes and never renew.

`MediaTransfers::text_reader` uses durable text descriptors and UTF-8 byte offsets.
`DurableTextReader::next_chunk` carries incomplete code points across chunk boundaries.
Text limits remain separate from binary chunk and lease limits.

Attachment and reader credentials never enter snapshots, generated TypeScript types, history descriptors, or debug output.
Native principal-generation changes wake media waits and reject stale operations. Token refresh within one generation preserves the owner.

See [client architecture](../../docs/architecture/clients.md) and [test guidance](../../docs/testing.md).
