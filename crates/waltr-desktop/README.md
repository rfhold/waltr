# Native Routine consumer

`waltr-desktop` owns native capture, shortcuts, hold lifecycle, text typing, and local command effects.
The shared client owns v2 protocol validation, credentials, stream bounds, and media authority.
The app owns catalog discovery, source-context selection, platform playback, file destinations, and file pickers.

## App API handoff

1. Obtain the current definition and server limits through the shared client catalog.
2. Select an explicit entrypoint and source context.
3. Supply schema-valid `RoutineJson` parameters.
4. Construct `interaction::Invocation::new(definition, server, entrypoint, parameters, correlation, source_session)`.
5. Install the invocation in each `config::Keybind::resolved` before the actor starts.
6. Cancel the actor generation before any catalog, profile, access, or credential change.
7. Await the actor before its replacement starts.

`Invocation::new` checks entrypoint eligibility, parameters, and discovered limits.
`interaction::default_parameters(&definition.parameters)` creates schema-derived initial values, including nested objects, numbers, arrays, and nullable fields.
The host still chooses the context and reviews the parameters; defaults grant no capability.

```rust,ignore
let parameters = waltr_desktop::interaction::default_parameters(&definition.parameters)?;
binding.resolved = Some(waltr_desktop::interaction::Invocation::new(
    definition,
    discovered_limits,
    binding.routine.trigger.clone(),
    parameters,
    correlation,
    selected_source_session,
)?);
```

The local YAML format remains version 1.
`routine.trigger` now names the exact current entrypoint; it does not select a legacy wire decoder.
`resolved` never enters YAML.
The hold rejects absent or mismatched resolved selections before device access.
The shared transport validates the exact revision, declarations, descriptor formats, parameters, and source context again.
Both native runners first validate the complete open request through the shared client, before capture or transport effects, then reuse that exact request for admission.
No v1 fallback exists.

## Capture and text

`interaction::Selection` now contains `invocation: &Invocation`, `input`, `output`, and `max_capture_seconds`.
It no longer contains independent routine and trigger names.
`interaction::run` requires an audio input and a selected text output from that descriptor.
`interaction::run_with_inputs` has the same arguments plus `&mut [media::BinaryInput]` after `selection`.
Use it for a capture workflow with additional required file/image inputs.
The host waits for nonempty PCM before any RPC; empty capture produces no admission.
One 20-second deadline covers the open and Accepted response.
Accepted precedes every input event.
Normal release requests stop and converter-tail flush, even before native startup completes.
The capture and converter join precede stream close and input finish.
An early output EOF fails while input remains active.

The shared client applies narrowed Accepted limits.
Successful final text requires SUCCEEDED, clean EOF, cleanup proof, and exactly one closed committed selected stream.
The host awaits local capture cleanup before command effects.
The host rechecks the generation and credential guard immediately before effects.
Live text retains grapheme-safe revisions and final deduplication.
Partial typing failures never trigger retries or command fallback.

`maxCaptureSeconds` remains optional and bounded to 1–3600 seconds.
Server input and invocation limits remain independent.
The native source supports mono PCM16 at its current host-supported rates; the app must preflight the selected descriptor.
No new codec or transcode path exists.

## Cleanup ownership

Retain every active actor or hold future.
For shutdown, cancel its generation first.
Then await the future.
Future drop requests cancellation but proves neither local join nor remote cleanup.
Native device initialization has no portable hard deadline.

`Diagnostic::CaptureCleanup` reports the local capture-task join.
`Diagnostic::SourceCleanup` reports finite source joins and upload-cancel request results, not remote terminal proof.
`Diagnostic::RemoteCleanup(bool)` separately reports shared-client terminal and clean-EOF proof.
The host retains the session after a cancelled operation and invokes `cancel_and_join`.
A 30-second drain timeout reports false, never clean proof.
The shared cancellation lane bypasses a full input queue.

## Native output sink

`interaction::Consumer::sink()` defaults to no native media capability.
The app can return an explicit `media::OutputSink` for PCM and file/image outputs.
The host preflights `OutputSink::supports` for each nontext output port before capture.
`OutputSink::write` receives validated typed stream events or a single-use `BinaryOutputGrant`.
The sink uses its pinned `MediaTransfers::read_output(grant)` for blobs.
It never reconstructs authority from history IDs, paths, or URLs.

The sink owns bounded device queues and blob readers.
The sink retains its tasks outside each write future.
The sink rechecks the guard immediately before effects.
`OutputSink::finish(success, guard)` stops producers, closes readers, drains or discards queues, and joins local tasks.
It rechecks the guard before each drain effect; invalidation discards output but must not skip cleanup.
The host awaits finish after capture and remote cleanup, including after cancellation.
This interface does not implement actual playback, a file picker, or a destination dialog.
The app must supply those concrete platform effects.

## Finite file/image and no-input workflows

`media::run(transport, invocation, inputs, text_output, guard, observer, consumer)` runs finite or no-input workflows without activating a microphone.
It returns `media::Output { invocation, text: Option<String> }` after successful terminal, clean EOF, and cleanup.
Pass `Some(exact_text_port)` for final text or `None` for a media-only result.
Text effects remain consumer-owned; this runner never selects a command, typing backend, or fallback.

Construct each `media::BinaryInput { port, declaration, source }` from a trusted user selection.
`declaration` is the v2 `BinaryInputDeclaration`, including exact finite size, display-only name, content type, optional digest, and file/image purpose.
The source is `Box<dyn media::Source>` backed by an already authorized handle, not a server path or fetched URL.
The app implements `Source::read(max_bytes)` and `Source::finish(success)`.
Reads return at most the requested bytes; empty means EOF.
Finish closes the handle, stops producers, and joins local workers, including after a dropped blocked read.
The host must cancel and await the runner rather than drop it.

All declarations precede open; no source read or upload occurs before Accepted.
One 20-second deadline covers open through Accepted.
The producer uses `BlobUpload::chunk_bytes()` after begin, capped at 64 KiB, and waits for each append before the next read.
It detects shorter or longer sources, commits before `BlobInput`, and never retries uncertain begin, append, or commit calls.
Output polling runs concurrently with upload; early output EOF fails unfinished input.
Failures cancel the shared input lane, request upload cancellation, join sources, drain remote cleanup, and finish the sink.
Empty files commit without an empty append; empty images fail preflight.
The server remains the authoritative image validator and quarantine owner.
The limits remain 64 MiB per file, 20 MiB per image, and 8 MiB per helper attachment.
Descriptor and discovered server limits can narrow file/image bounds further.
This API does not create helper attachments or raise their separate 8 MiB limit.
The capture runner still requires nonempty PCM before admission, even when finite inputs are selected.
An empty capture closes those sources without upload.

The app must supply concrete picker/handle implementations and explicit PCM/file/image destinations.
No default directory, overwrite policy, arbitrary-path input, or native playback implementation is installed by these APIs.

## Verification boundary

`tests/hold.rs` uses a native fake v2 service, not production routes.
It covers hold release, acceptance deadlines, capture bounds, converter tails, cleanup, text safety, and the typed PCM sink interface.
`tests/parameters.rs` covers schema-derived structured defaults.
`tests/media.rs` covers negotiated 7-byte uploads, empty files, size changes, file/image limits, fake quarantine rejection, no-input and mixed capture/file workflows, acceptance deadlines, upload/read failures, backpressure, generation and credential invalidation, source joins, and grant-based blob reader cleanup.
The image fixture exercises protocol metadata and fake quarantine behavior; it does not validate an image decoder.
The PCM sink test counts validated bytes; it does not play audio.
No physical capture, actual platform playback, file picker, or production v2 route passes verification here.
The app Rust consumer requires the API migration above and independent review before integration.

See [shared-client contracts](../waltr-client/README.md) and [layered tests](../../docs/testing.md).
