# Routine Audio Interactions

These examples are strict JavaScript Routine envelopes, not task graphs. Resource validation does not establish live-provider compatibility or complete production workflow coverage. [Routines](../../architecture/routines.md) owns admission, cancellation, persistence and bounds.

| Example | Entrypoint | Inputs and outputs |
| --- | --- | --- |
| [Transcription](audio-transcribe.yaml) | `dictate` | PCM audio to transcript text |
| [Transcription and cleanup](audio-transcribe-cleanup.yaml) | `dictate` | Committed transcript to one normal named cleanup helper |
| [Conversation notes](audio-conversation-notes.yaml) | `record-notes` | Speaker-labeled transcript, explicit deletion audit and final notes |
| [Synthesis](audio-synthesize.yaml) | `speak` | Committed UTF-8 text to PCM audio |

The [v2 contract](../../../proto/waltr/routine/v2/routine.proto) provides native `Interact` and finite browser attachment/input streams through the same invocation service. The v1 package, `Run` request and task-history decoder are removed.

## Resource And Host Configuration

Every resource uses `apiVersion: waltr.routine/v1`, a top-level `name`, and `spec.source`, `entrypoints`, typed parameter/result contracts, ports, capabilities and budgets. `host.call` uses the existing bounded media, helper and result APIs; no `tasks`, stream-expression parser, `runAfter` or work-group executor remains.

Ports select exact formats such as `pcm16le-24000-mono` and `utf8`. These examples retain their 256 MiB audio and 1 MiB text bounds and 90-minute total deadline; the deadline does not restart after input completion. Capture, workspace, stream, text-result and nonrenewing output-lease limits remain independently enforced by their owners. The guest cannot select native devices or paths, extend leases, or bypass bounded readers/writers.

Hosts configure speech endpoints, provider/model keys and credentials through `RoutineAuthority.recognition` / `synthesis`. Synthesis input requires `{speech:{model,voice,language,rate}}` from explicit host `synthesis_choices`. No ambient default, fallback model, codec conversion or credential choice exists.

JavaScript calls `helper.run` with explicit selection, task, capture, source, turns, deadline and response/completion limits. Named agents include `routine-cleanup`, `routine-conversation-filter` and `routine-conversation-notes`. `RoutineAuthority.helper_inputs` pins exact inference provider/model attachment capabilities; absent entries deny attachments. Payloads use the fifth `host.call` argument as committed invocation resources, never paths or URLs. Prompts and permissions remain normal Agent resources. Declared capabilities grant no authority.

## Committed Text And Notes

Cleanup consumes collected committed text, not successive interim hypotheses and not one helper per chunk. Conversation filtering returns exactly `retained_transcript` and `deleted_items`; the notes helper receives both fields. The named prompts preserve substantive topics, speaker labels, deletion reasons, decisions, todos and uncertainty while treating source content as untrusted data. Speaker labels are not known human identities.

The filter submits `_helper_capture` under the resource's `spec.captures` declaration for `conversation-filter`. Plain JSON prose does not satisfy that contract. Its named agent and `RoutineAuthority.media_restriction` ceiling must both permit the capture tool. Declaration grants no permission. Definite denial fails before speech effects; Ask uses ordinary live consent. An absent ceiling grants no tools. Source policy also applies. `media.select` uses a generic JSON pointer or entire-capture selection and retains helper provenance.

The JavaScript program validates the structured helper result before starting the next helper. It does not repair malformed JSON, retry an uncertain helper, truncate transcript content or invent a fallback summary. Provider context and output limits must fit the whole transcript, deletion audit and final notes; server byte bounds do not increase model capacity.

The [desktop conversation-notes example](../client-conversation-notes.yaml) selects the same resource, entrypoint and ports. Retained transcript, notes and deletion audit use separate durable text results. Sensitive transcript, helper and result data retain their ordinary persistence and access rules.

## Coordinated Cutover

Supply reviewed current-schema overrides; no converter or compatibility decoder exists. Back up matched resources, SQL, object namespaces, sessions and keys before a separately authorized deployment. Migration 0016 refuses populated incompatible Routine state; it does not migrate SQL bodies into objects. See the [cutover runbook](../../runbooks/server-capability-cutover.md) for exact archive/reset approval boundaries. Examples authorize no deployment or retained-state deletion.
