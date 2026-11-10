# Routine Examples

The eleven shipped examples below are strict JavaScript Routine resources. Each uses
`apiVersion: waltr.routine/v1`, a top-level `name`, an inline module, and typed
entrypoint, parameter, result, port, capability, and budget declarations. Each
example is byte-identical to its bundled resource in
[`crates/waltr-server/resources/routines`](../../../crates/waltr-server/resources/routines/).

The task DSL, substitutions, task graph, work groups and v1 RPC package are absent.
Unsupported overrides require reviewed current-schema replacements.
There is no converter, runtime decoder or fallback.

| Example | Purpose | Host requirements |
| --- | --- | --- |
| [title](title.yaml) | Set an absent title from the first durable user message. | First-message origin and normal named `routine-title` helper. |
| [compact-and-continue](compact-and-continue.yaml) | Summarize a completed source prefix and optionally continue. | Exact source snapshot, atomic pause/completion barrier, named `routine-compaction` helper. |
| [fork](fork.yaml) | Create an ordinary destination from a completed source boundary. | Original-context fork authority and named `routine-fork` helper. |
| [audio-transcribe](audio-transcribe.yaml) | Stream speech recognition revisions and committed text. | Explicit compatible recognition model and admitted audio/text ports. |
| [audio-transcribe-cleanup](audio-transcribe-cleanup.yaml) | Collect all committed text, then run one cleanup helper. | Recognition model, named `routine-cleanup` agent and exact helper input capability. |
| [audio-conversation-notes](audio-conversation-notes.yaml) | Retain substantive conversation, audit deletions, then produce notes. | Diarization-capable recognition, named filter/notes agents and declared capture contracts. |
| [audio-synthesize](audio-synthesize.yaml) | Synthesize committed text into PCM audio. | Explicit compatible synthesis model and admitted text/audio ports. |
| [context-categorization](context-categorization.yaml) | Categorize an immutable source snapshot. | Generic metadata/history transactions, original-source authority and named `routine-categorization` helper. |
| [context-management](context-management.yaml) | Plan category edits and await category-specific children. | Exact categorization coverage, child binding, named `routine-context-strategy` helper. |
| [manage-category-context](manage-category-context.yaml) | Keep or replace one category mask with compare-and-set. | Exact category context/mask authority and named `routine-category-context` helper. |
| [configuration](configuration.yaml) | Run the normal named configuration agent. | Explicit target authority, current resource pins, normal `configuration` agent and policy. |

## Admission and authority

The frozen [v2 service](../../../proto/waltr/routine/v2/routine.proto) provides
source-free discovery, typed admission, observation, cancellation, interaction,
and result access through one shared invocation service. The catalog revision
selected by a caller is pinned before durable admission. Correlation is not
idempotency; uncertain effects are not retried or replayed.

Catalog preflight validates strict resources and exact named-helper selections.
The three context helpers reference `routine-helper-capture`, a normal permission
set allowing only `_helper_capture`. The host must still bind the exact typed
capture contract; this permission adds no general tools or caller authority.
Resources declare capture schemas; JavaScript selects helper tasks, budgets and source handles.
Hosts bind generic context/child adapters, model ceilings and trigger authority. Unsupported authority fails closed.

The historical [permission-evaluation proposal](permission-evaluation.yaml) remains
documentation only. It uses the removed task DSL and is not accepted by the
current catalog. No permission sweep or deferred proposal/review service is shipped.

## Source and helper boundaries

Title uses only the first durable user message and an absent-title compare-and-set.
Compaction owns the completed-source pause and preserves committed masks even if
later continuation fails. Fork captures one immutable completed boundary, treats
the direction as untrusted data, and retains an atomically created destination
if later start or cancellation fails. Raw timeline history remains separate from
effective context. See [Routine architecture](../../architecture/routines.md)
for lifecycle, source identity, ownership, restart, and shutdown contracts.

Helpers are ordinary named agent resources; prompts and permissions are not
embedded in a task executor. Captures use bounded resource-declared schemas. Declaring
a helper or capability does not grant access to it.

## Audio setup

See [audio interactions](audio-interactions.md) for exact ports, committed-text
collection, deletion audit, limits, and explicit `RoutineAuthority` speech/model
and exact helper-input configuration. Old inline DSL model references are not
silently translated into provider choices. The desktop examples select exact
routine entrypoint and port identifiers:
[dictation](../client.yaml) and [conversation notes](../client-conversation-notes.yaml).

No example establishes live provider compatibility, deployment, or permission
to convert an existing database.
