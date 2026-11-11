# Routine Audio Interactions

These examples are strict JavaScript Routine envelopes, not task graphs. Resource validation does not establish live-provider compatibility or complete production workflow coverage. [Routines](../../architecture/routines.md) owns admission, cancellation, persistence and bounds.

| Example | Entrypoint | Inputs and outputs |
| --- | --- | --- |
| [Transcription](audio-transcribe.yaml) | `dictate` | PCM audio to transcript text |
| [Transcription and cleanup](audio-transcribe-cleanup.yaml) | `dictate` | Committed transcript to one normal named cleanup helper |
| [Conversation notes](audio-conversation-notes.yaml) | `record-notes` | Speaker-labeled transcript, explicit deletion audit and final notes |
| [Synthesis](audio-synthesize.yaml) | `speak` | Committed UTF-8 text to PCM audio |
| [Ask and speak](ask-and-speak.yaml) | `ask` | Question parameter to a completed answer, text output and PCM audio |

The [v2 contract](../../../proto/waltr/routine/v2/routine.proto) provides native `Interact` and finite browser attachment/input streams through the same invocation service. The v1 package, `Run` request and task-history decoder are removed.

## Resource And Host Configuration

Every resource uses `apiVersion: waltr.routine/v1`, a top-level `name`, and `spec.source`, `entrypoints`, typed parameter/result contracts, ports, capabilities and budgets. `host.call` uses the existing bounded media, helper and result APIs; no `tasks`, stream-expression parser, `runAfter` or work-group executor remains.

Ports select exact formats such as `pcm16le-24000-mono` and `utf8`. Each resource declares finite byte limits and a total deadline; input completion does not restart that deadline. Capture, workspace, stream, text-result and nonrenewing output-lease limits remain independently enforced by their owners. The guest cannot select native devices or paths, extend leases, or bypass bounded readers/writers.

Provider resources define speech endpoints, model options, and credentials. Defaults select `spec.speech.stt`, `diarize`, and `tts` independently. JavaScript calls use `speech: {from: 'defaults'}` or exact `{provider, model}` selectors. Exact synthesis selectors also accept `voice`, `language`, and `rate`. Omitted options use catalog model settings. The host permits no codec conversion or credential injection.

JavaScript calls `helper.run` with explicit selection, inference, input limits, task, capture, source, turns, deadline, and response/completion limits. Named agents include `routine-cleanup`, `routine-conversation-filter`, and `routine-conversation-notes`. These media helpers use `inference: {from: 'defaults'}` and `input: {text: true, maxBytes: 8388608}`. Payloads use the fifth `host.call` argument as committed invocation resources. Prompts and permissions remain normal Agent resources. Input declarations grant no image, PDF, or tool authority.

## Committed Text And Notes

`media.synthesize` accepts exactly one text source: a `port` name or a JavaScript `text` string. For example:

```js
await host.call('media.synthesize', {text: answer, output: 'audio', speech: {from: 'defaults'}});
```

Any JavaScript string can supply `text`, subject to host-call, string and text byte limits. Both sources use the same speech selection and PCM stream contracts.

`ask-and-speak` accepts a required, nonblank `question` parameter, bounded to 4096 bytes. The normal `routine-answer` Agent has no features or tool permissions. It answers from model knowledge through inference defaults, with one turn and a 60-second helper deadline. The Routine waits for the completed answer, reads its UTF-8 bytes through `media.read`, then decodes them in JavaScript. It retains an `answer` text result, emits text, and synthesizes speech through speech defaults. The result also returns `answer`. The response limit is 8192 bytes; the total deadline is 120 seconds. It copies no session history and speaks no token previews.

Cleanup consumes collected committed text, not successive interim hypotheses and not one helper per chunk. Conversation filtering returns exactly `retained_transcript` and `deleted_items`; the notes helper receives both fields. The named prompts preserve substantive topics, speaker labels, deletion reasons, decisions, todos and uncertainty while treating source content as untrusted data. Speaker labels are not known human identities.

The filter submits `_helper_capture` under the resource's `spec.captures` declaration for `conversation-filter`. Plain JSON prose does not satisfy that contract. Normal named-agent permissions and applicable caller/source restrictions control the capture tool. Declaration grants no permission. Definite denial fails before speech effects; Ask uses ordinary live consent. `media.select` uses a generic JSON pointer or entire-capture selection and retains helper provenance.

The JavaScript program validates the structured helper result before starting the next helper. It does not repair malformed JSON, retry an uncertain helper, truncate transcript content or invent a fallback summary. Provider context and output limits must fit the whole transcript, deletion audit and final notes; server byte bounds do not increase model capacity.

The [desktop conversation-notes example](../client-conversation-notes.yaml) selects the same resource, entrypoint and ports. Retained transcript, notes and deletion audit use separate durable text results. Sensitive transcript, helper and result data retain their ordinary persistence and access rules.

## Coordinated Cutover

Supply reviewed current-schema overrides; no converter or compatibility decoder exists. Back up matched resources, SQL, object namespaces, sessions and keys before a separately authorized deployment. Migration 0016 refuses populated incompatible Routine state; it does not migrate SQL bodies into objects. See the [cutover runbook](../../runbooks/server-capability-cutover.md) for exact archive/reset approval boundaries. Examples authorize no deployment or retained-state deletion.
