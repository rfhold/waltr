# Routine Audio Interactions

## Status And Scope

The catalog, runtime, and `RoutineInteractionService` implement these audio examples in current local code. This status establishes no release, deployment, or real upstream compatibility.

[Routines](../../architecture/routines.md#interaction-catalog-and-runtime) owns runtime semantics, admission, limits, cancellation, and persistence. This document explains audio YAML and the wire contract.

| Source | Purpose |
| --- | --- |
| [Interaction proto](../../../proto/waltr/routine/v1/routine_interaction_service.proto) | Native and gRPC-web interaction contract. |
| [Audio transcription](audio-transcribe.yaml) | Audio input to live transcript revisions and committed segments. |
| [Audio transcription and cleanup](audio-transcribe-cleanup.yaml) | All committed transcript text, then one inference-only cleanup helper. |
| [Conversation notes](audio-conversation-notes.yaml) | Speaker-labeled transcript, topic selection with deletion audit, then notes from both retained and deleted content. |
| [Text synthesis](audio-synthesize.yaml) | Committed text input to PCM audio output. |

Examples require explicit provider/model keys. See [speech providers](../../architecture/config.md#speech-provider-contract) for matching Provider resources. Routines infer no endpoint, voice, device, credentials, codec conversion, or deployment.

## Accepted Catalog Syntax

- `trigger.interaction.inputs` exposes scalar parameters by name, like manual inputs. String constraints belong under `params[].validation.minLength`.
- `inputPorts` declares `{name, required, format}`. `outputPorts` declares `{name, format}`. Each port carries one stream per interaction. Clients must supply required inputs; optional inputs cannot feed unconditional stream tasks.
- A text format is `{type: text}`. An audio format is `{type: audio, encoding: pcmS16le, sampleRateHz: 24000, channels: 1}`. Catalog PCM validation accepts 8–48 kHz and 1–8 channels. Models must match the exact rate and channel count. TTS requires mono.
- `taskSpec.transcribe` contains `model: {provider, model}` and `input`.
- `taskSpec.synthesize` contains `model`, `input`, and an audio `format`. It consumes committed segments only, never interim hypotheses.
- `taskSpec.collect` contains `stream`, positive `maxBytes`, and `separator`. It waits for successful close, resolves revisions, and joins committed segments in first-appearance order. Its scalar result expression is `$(output.text)`.
- `sessionTemplate.model: {provider, model}` selects an LLM without a source session. It is exclusive with `source` and inherited `inference`. The runtime disables reasoning. Interaction helpers require one turn and empty feature, tool, and permission lists.
- `output: {type: text, stream: true}` exposes assistant text, not reasoning or tools.
- `spec.outputs` routes `{port, streamFrom}` to declared output ports. Every output requires exactly one route.

Stream handles use exactly `$(interaction.inputs.<port>)` or `$(tasks.<task>.output)`. They use the dedicated `RoutineStreamRef` type, not scalar expressions. A collector provides the explicit stream-to-string boundary.

A stream reference creates a live dependency; its consumer starts before producer completion. `runAfter` remains a completion dependency. Connected live-stream components require concurrent eligibility, including siblings with a shared interaction input. The validator rejects completion edges within components, contracted completion cycles, and directed stream/completion cycles.

Cleanup uses `runAfter: [collect-transcript]`, so one helper receives the entire committed transcript after successful collection. It does not concatenate successive hypotheses or invoke a helper per chunk.

Named groups are explicit:

```yaml
workGroups:
  - name: cleanup
    onBusy: queue
    maxQueued: 8
```

A task opts in with `workGroup: cleanup`. `onBusy` accepts exactly `queue`, `reject`, or `interrupt`. Only `queue` accepts `maxQueued`, which must be 1–64. Distinct tasks within one live component cannot share a queue group. Separate components can share one, including a collector and its cleanup task.

The runtime reserves queue-group resources atomically per live component after all external completion prerequisites. Tasks start once; groups add no automatic ASR turns, debounce, or implicit supersession.

## Conversation Notes

[Conversation notes](audio-conversation-notes.yaml) requires a separate `speech/diarized-dictation` model and a compatible backend.
Add a model with streaming speech capability and `recognition.diarization: true`.
Keep `speech/dictation` unchanged with diarization disabled, so the cleanup example retains its current behavior.
The recognition rate must match PCM16 little-endian, 24 kHz, mono audio.
Configure `inference/cleanup` for both standalone helpers.
These examples do not prove backend availability, diarization quality, or deployed model compatibility.
Speaker labels identify voices within one recognition request, not known people.

The first helper infers all substantive topics, retains their full speaker-labeled transcript, and removes only clearly unrelated noise.
It returns exact JSON string fields `retained_transcript` and `deleted_items`.
The deletion audit includes each removed passage, its topic, and its reason.
The second helper receives both fields explicitly.
It formats the retained conversation and summarizes topic output items, todos/action items, and decisions.
It consults deleted content for context and mistaken exclusions, but does not blindly restore it.
Both prompts treat all source content as untrusted data.
Model judgment remains nondeterministic; the runtime validates syntax and declared scalar types, not semantic fidelity.

Task results support `$(output.json.FIELD)` alongside unchanged `$(output.text)`.
`FIELD` is one nonempty top-level key with ASCII letters, digits, or underscores.
The runtime strictly parses one JSON object and selects only a string or boolean field.
Malformed JSON, fences, trailing commentary, missing fields, and other value types fail the task before dependent helpers run.
Nested paths, arrays, JSONPath, jq, repair, retries, and native structured result types are unsupported.
The expression works only in task result contexts where output references are valid.
Raw JSON has a 512 KiB UTF-8 bound before parse; selected strings have the same 512 KiB scalar bound.
Each expanded system or user prompt also has a 512 KiB bound.
The notes fixture uses a 256 KiB collector bound (`maxBytes: 262144`).
This leaves headroom for JSON, deletion reasons, prompt labels, summaries, and final notes within the 512 KiB response and prompt budgets.
JSON escapes and additional commentary still consume that headroom; 256 KiB of source does not guarantee a successful model response.
Metadata keys and JSON-encoded metadata values retain separate 16 KiB caps.
Durable Routine result JSON envelopes have a separate 8 MiB cap; this does not raise expression or metadata limits.
Oversize content fails explicitly; the runtime never truncates or silently substitutes a fallback.
It adds no retry, fence repair, fallback summary, or automatic extra helper calls.

Choose models whose context and output limits fit the complete transcript, both JSON fields, and final notes.
The first helper must reproduce retained content plus deletion passages and reasons, not just a short summary.
The second helper must accept both fields plus labels and produce the full notes within its output allowance.
Known model context metadata still enforces the conservative UTF-8-byte prompt check; this check is not tokenization.
Server byte ceilings do not increase provider context or output limits.
No live backend or model test establishes one-hour support or final-output completeness.

The native desktop automatically closes capture normally at 3,600 seconds, then waits for transcription finalization and notes.
The server input hard deadline is 3,900 seconds (65 minutes); the total invocation hard deadline is 5,400 seconds (90 minutes).
A server timeout fails the interaction; it does not act as the desktop's normal close.
The total deadline includes capture and both helpers and does not reset after input completion.
The default 256 MiB request cap covers 172,800,000 raw bytes for one hour of 24 kHz mono PCM16 plus protocol overhead.
Native requests count cumulative bytes through bounded queues; they do not require a full-hour audio buffer.
Complete `Run` requests share the 256 MiB cap but still require one complete request.
The server retains 1 MiB chunk and 2 MiB edge caps, eight queued chunks, and a 30-second queue-wait limit.

Only `write-notes` feeds the final `notes` text port.
The Routine exposes `deleted_items` separately from its final `text` result.
Declared results and durable helper messages can contain sensitive conversation content under ordinary persistence rules.

The [desktop clipboard example](../client-conversation-notes.yaml) selects Routine `audio-conversation-notes`, trigger `record-notes`, input `audio`, and output `notes`.
Its example chord, `Ctrl+Alt+Shift+Space`, differs from the cleanup example.
Check local shortcut conflicts before use.
The command receives only final notes through stdin and invokes exactly `/usr/bin/pbcopy`.
It has no typing or paste action and never sends the deletion audit to the clipboard command.

## Admission And Transports

The separate `RoutineInteractionService` shares `waltr.routine.v1` with the durable Routine API. Both methods use common authentication and shared admission. They preserve the detached `StartManualRoutine` contract.

`OpenInteraction` selects `routine_name`, `trigger_name`, scalar `params`, and optional `request_id`. The service rejects `source_session_id`, source-backed helpers, unsupported actions, and unsupported speech options before durable history. Admission pins the catalog revision and provider/model settings.

The server commits admission before it queues `accepted` with an invocation ID. That durable boundary does not prove client receipt of the acknowledgment. Request IDs provide correlation only, without idempotency, deduplication, reconnect, replay, or retry guarantees.

- Native clients use bidirectional `Interact`. The first request contains the only `open` event. The first application response is `accepted` or `validation_failure`. Clients wait for acceptance before input events. Validation failure ends the RPC without admission.
- gRPC-web clients use unary-request, server-streamed `Run`. The complete request contains at most one input per declared port. Each input contains audio format plus bytes, or one complete committed text value. The server closes those inputs and completes input. This method provides no incremental browser microphone upload.
- RPC cancellation, disconnect, response drop, or wire `cancel` cancels owned interaction work instead of detached execution. Cancellation cannot retract output that the client already received.

See [admission and transports](../../architecture/routines.md#admission-and-transports) for aggregate ingress budgets and in-band cancellation visibility under full capacity.

## Stream And Text Rules

A port name selects a declared boundary. A stream ID identifies its one stream instance. IDs are nonempty and unique within each direction; input and output can share an ID. Each stream has one open and one close event. Every event, content, and format oneof requires a member. Unspecified enums and duplicate scalar parameters are invalid.

Chunks occur between open and close and match the declared format. Transport order is authoritative; streams can interleave. Audio uses signed interleaved headerless PCM16 little-endian with complete sample frames, not WAV or browser-native compressed audio. Clients own device selection, capture, playback, and format conversion.

Text streams carry `TextUpdate`:

- Nonempty segment IDs are stream-local; first appearance fixes segment order.
- `APPEND` adds text to a segment, initially empty. `REPLACE` replaces its entire text. Either operation can create the segment.
- `committed: true` freezes the segment after that update, not its durable persistence. No later update can name it.
- Successful close requires every segment committed. Failed or cancelled streams can retain uncommitted segments, but those segments do not enter final transcript text.

Transcription emits segments and revisions. Collectors resolve hypotheses instead of concatenation. Synthesis forwards only committed segments, in first-appearance order.
Ordinary transcription omits word timing and speaker attribution.
An explicitly diarized model renders request-local speaker labels in text; it provides no durable speaker identity.
Segment IDs establish no cross-request or speaker identity.

Close status is `COMPLETED`, `CANCELLED`, or `FAILED`; only failure includes `RoutineFailure`. No chunk follows close. `input_completed` requires every input stream closed and forbids later input opens or chunks. It does not cancel output or transcription finalization. Native half-close requires every stream closed; otherwise it causes protocol failure. Clients can send `cancel` after input completion if the request side remains open.

On a usable transport, the server closes every output stream before exactly one `completed` response. No response follows it. Terminal status is `SUCCEEDED`, `FAILED`, or `INTERRUPTED`; cancellation maps to interrupted. Failed and interrupted outcomes include `RoutineFailure`. Broken transports cannot guarantee close or terminal delivery.

## Errors, Bounds, And Persistence

Before admission, invalid selection, parameters, complete inputs, formats, or adapter capabilities produce `validation_failure`. Capacity rejection uses gRPC `RESOURCE_EXHAUSTED`, without durable admission. After admission, protocol violations and execution failures produce one failed terminal event. Protocol violations use `invalid_argument`; persistent queue saturation uses `resource_exhausted`. Explicit cancellation produces interrupted with `cancelled` when transport permits. Authentication, service failure, deadlines, and broken transport can prevent application responses.

The [canonical bounds](../../architecture/routines.md#bounds-and-durable-state) apply to runtime edges and transport queues. `Run` charges its original request quota, not synthetic feeder metadata. Cancellation-aware bounded backpressure preserves finite byte/chunk budgets without silent loss.

The cleanup fixture enforces collector bounds before one helper invocation. Known model context metadata adds a conservative UTF-8-byte prompt bound, not tokenization. Without that metadata, collector and upstream limits still apply. Oversize text fails without truncation, summary, or separate helper calls.

Persistence records the distinct interaction trigger kind and ordinary invocation/task lifecycle events. SQLite migration tests cover history and sequence preservation. PostgreSQL migration execution still requires live verification. No audio or text stream chunks enter durable replay. Declared scalar results follow ordinary Routine history rules and can contain final transcript text.

The implementation adds no client device integration, upstream authentication, ADC, OAuth, resampling, automatic ASR turns, or debounce.
