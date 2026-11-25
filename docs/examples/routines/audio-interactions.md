# Routine Audio Interactions

These examples are strict JavaScript Routine envelopes, not task graphs. Resource validation does not establish live-provider compatibility or complete production workflow coverage. [Routines](../../architecture/routines.md) owns admission, cancellation, persistence and bounds.

| Example | Entrypoint | Inputs and outputs |
| --- | --- | --- |
| [Transcription](audio-transcribe.yaml) | `dictate` | Type the final transcript through Computer Use |
| [Transcription and cleanup](audio-transcribe-cleanup.yaml) | `dictate` | Copy the cleanup helper's final text through Computer Use |
| [Conversation notes](audio-conversation-notes.yaml) | `record-notes` | Retain transcript and audit, then copy only final notes through Computer Use |
| [Synthesis](audio-synthesize.yaml) | `speak` | Committed UTF-8 text to PCM audio |
| [Ask and speak](ask-and-speak.yaml) | `ask` | Question parameter to a completed answer, text output and PCM audio |
| [Audio ask and speak](audio-ask-and-speak.yaml) | `ask` | Recorded question to a completed answer and PCM audio |

The [v2 contract](../../../proto/waltr/routine/v2/routine.proto) provides native `Interact` and finite browser attachment/input streams through the same invocation service. The v1 package, `Run` request and task-history decoder are removed.

## Resource And Host Configuration

Every resource uses `apiVersion: waltr.routine/v1`, a top-level `name`, and `spec.source`, `entrypoints`, typed parameter/result contracts, ports, capabilities and budgets. `host.call` uses the existing bounded media, helper and result APIs; no `tasks`, stream-expression parser, `runAfter` or work-group executor remains.

Ports select exact formats such as `pcm16le-24000-mono` and `utf8`. Each resource declares finite byte limits and a total deadline; input completion does not restart that deadline. Capture, workspace, stream, text-result and nonrenewing output-lease limits remain independently enforced by their owners. The guest cannot select native devices or paths, extend leases, or bypass bounded readers/writers.

Provider resources define speech endpoints, model options, and credentials. Defaults select `spec.speech.stt`, `diarize`, and `tts` independently. JavaScript calls use `speech: {from: 'defaults'}` or exact `{provider, model}` selectors. Exact synthesis selectors also accept `voice`, `language`, and `rate`. Omitted options use catalog model settings. The host permits no codec conversion or credential injection.

JavaScript calls `helper.run` with an explicit selection, inference, input limits, ordered `content`, capture, source, turns, deadline, and response/completion limits. Named agents include `routine-cleanup`, `routine-conversation-filter`, and `routine-conversation-notes`. These helpers use `inference: {from: 'defaults'}` and `input: {text: true, maxBytes: 8388608}`. They put the prompt in one text part and pass the committed transcript resource as the fifth `host.call` argument. Prompts and permissions remain normal Agent resources. Input declarations grant no image, PDF, or tool authority.

## Committed Text And Notes

`media.synthesize` accepts exactly one text source: a `port` name or a JavaScript `text` string. For example:

```js
await host.call('media.synthesize', {text: answer, output: 'audio', speech: {from: 'defaults'}});
```

Any JavaScript string can supply `text`, subject to host-call, string and text byte limits. Both sources use the same speech selection and PCM stream contracts.

`ask-and-speak` accepts a required, nonblank `question` parameter, bounded to 4096 bytes. The normal `routine-answer` Agent has no features or tool permissions. It answers from model knowledge through inference defaults, with one turn and a 60-second helper deadline. The Routine waits for the completed answer, reads its UTF-8 bytes through `media.read`, then decodes them in JavaScript. It retains an `answer` text result, emits text, and synthesizes speech through speech defaults. The result also returns `answer`. The response limit is 8192 bytes; the total deadline is 120 seconds. It copies no session history and speaks no token previews.

## Audio question shortcut

Add this binding to the `keybinds` list in `~/.config/waltr/client.yaml` (or `$XDG_CONFIG_HOME/waltr/client.yaml`):

```yaml
- id: audio-question
  key: Ctrl+Alt+A
  mode: press-to-talk
  maxCaptureSeconds: 120
  routine: {name: audio-ask-and-speak, trigger: ask, input: question, output: audio}
  audio: {device: default, sampleRateHz: 24000, channels: 1}
  playback: {}
```

The shortcut uses Local by default. For a Remote named `home`, add `destination: home` inside `routine`. Authorize `home` in **Config → Remotes** first. The shortcut does not start a login or select a workspace.

Tap Ctrl+Alt+A, ask a question, then pause for about one second to finish recording. Playback starts when the answer is ready.
If the mic detects no speech within five seconds, the shortcut stops without sending a request.
Use `mode: toggle` to tap once to start capture and tap again to finish it. Silence does not stop toggle capture.
Use `mode: hold` instead if you prefer to release the keys to finish recording.
The Routine collects one transcript through STT defaults, answers through inference defaults, and synthesizes the complete answer through TTS defaults.
It retains the answer in Routine history without a required text stream.
The transcript limit is 4096 bytes, the answer limit is 8192 bytes, and the invocation deadline is 240 seconds.
The output limit is 16 MiB of 24 kHz mono PCM.
The default output device must support that exact format or the bounded 48 kHz stereo playback adaptation.
The player prefers exact output and permits no other rate or channel conversion. No automatic replay exists.
The capture reservation remains occupied until playback and cleanup finish. Other capture shortcuts do not queue.
Restart the client after configuration changes. This action requires a client build with native playback support.

Cleanup consumes collected committed text, not successive interim hypotheses and not one helper per chunk. Conversation filtering returns exactly `retained_transcript` and `deleted_items`; the notes helper receives both fields. The named prompts preserve substantive topics, speaker labels, deletion reasons, decisions, todos and uncertainty while treating source content as untrusted data. Speaker labels are not known human identities.

The filter submits `_helper_capture` under the resource's `spec.captures` declaration for `conversation-filter`. Plain JSON prose does not satisfy that contract. Normal named-agent permissions and applicable caller/source restrictions control the capture tool. Declaration grants no permission. Definite denial fails before speech effects; Ask uses ordinary live consent. `media.select` uses a generic JSON pointer or entire-capture selection and retains helper provenance.

The JavaScript program validates the structured helper result before starting the next helper. It does not repair malformed JSON, retry an uncertain helper, truncate transcript content or invent a fallback summary. Provider context and output limits must fit the whole transcript, deletion audit and final notes; server byte bounds do not increase model capacity.

Retained transcript, notes and deletion audit use separate durable text results. Sensitive transcript, helper and result data retain their ordinary persistence and access rules.

## Desktop Text Shortcuts

Use the [dictation and cleanup example](../client.yaml) and the [conversation-notes example](../client-conversation-notes.yaml).
They route through Remote `home`; the Routines select Computer Use MCP on worker `preview`.
The shortcuts omit `routine.output` and any local output action.
The Routines send at most 16 KiB of final UTF-8 text, subject to the encoded request limit.
Dictation makes one `type_text` call. Cleanup and notes each make one `clipboard_copy` call.
There is no interim insertion, clipboard paste, local fallback, or automatic retry.

## Coordinated Cutover

Supply reviewed current-schema overrides; no converter or compatibility decoder exists. Back up matched resources, SQL, object namespaces, sessions and keys before a separately authorized deployment. Migration 0016 refuses populated incompatible Routine state; it does not migrate SQL bodies into objects. See the [cutover runbook](../../runbooks/server-capability-cutover.md) for exact archive/reset approval boundaries. Examples authorize no deployment or retained-state deletion.
