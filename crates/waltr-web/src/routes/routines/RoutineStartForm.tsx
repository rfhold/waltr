import { A } from "@solidjs/router";
import { For, Show, createMemo, createSignal } from "solid-js";
import { useAppState } from "../../app/context";
import { clientRequestId } from "../../app/requestId";
import type { ClientManualRoutineDefinition } from "../../generated/waltr-client";
import { initialParameters, validateParameters } from "./parameters";
import { MediaInteractionPanel } from "./MediaInteractionPanel";

export function RoutineStartForm(props: { definition: ClientManualRoutineDefinition; sourceSession?: string }) {
  const app = useAppState();
  const entries = createMemo(() => props.definition.entrypoints.filter(e => e.trigger === "manual" && e.startContexts.includes(props.sourceSession ? "session" : "global")));
  const [entry, setEntry] = createSignal("");
  const [json, setJson] = createSignal(initialParameters(props.definition.parameters.schema));
  const [error, setError] = createSignal<string>();
  const [key, setKey] = createSignal("");
  const [pending, setPending] = createSignal(false);
  const receipt = createMemo(() => app.snapshot()?.routines.starts[key()]);
  const current = createMemo(() => Boolean(app.snapshot()?.routines.definitions.definitions.some(d => d.routineName === props.definition.routineName && d.routineRevision === props.definition.routineRevision)));
  async function submit(event: SubmitEvent) {
    event.preventDefault();
    const invalid = validateParameters(json(), props.definition.parameters.schema);
    setError(invalid); if (invalid || !current() || pending()) return;
    const operationKey = clientRequestId("routine-start"); setKey(operationKey); setPending(true);
    try { await app.dispatch({ type: "startManualRoutine", operationKey, requestId: clientRequestId("routine-request"), routineName: props.definition.routineName, routineRevision: props.definition.routineRevision, triggerName: entry() || entries()[0]!.name, sourceSessionId: props.sourceSession ?? null, inputs: json() }); }
    catch { setError("Unable to confirm admission. Do not retry an uncertain start automatically."); }
    finally { setPending(false); }
  }
  return <section><h3>{props.definition.routineName}</h3><p>{props.definition.description}</p><p>Revision {props.definition.routineRevision}</p>
    <label>Parameters (JSON)<textarea value={json()} onInput={e => setJson(e.currentTarget.value)} spellcheck={false} /></label>
    <details><summary>Parameter schema</summary><pre>{props.definition.parameters.schemaJson}</pre></details>
    <Show when={error() || receipt()?.error || receipt()?.validationFailure}><p role="alert">{error() || receipt()?.error || receipt()?.validationFailure?.message}</p><For each={receipt()?.validationFailure?.violations}>{v => <p>{v.fieldPath}: {v.message}</p>}</For></Show>
    <Show when={entries().length} fallback={<p>No manual entrypoint in this context.</p>}>
      <form aria-label={`${props.definition.routineName} command`} onSubmit={submit}>
        <label>Entrypoint<select value={entry() || entries()[0]?.name} onChange={e => setEntry(e.currentTarget.value)}><For each={entries()}>{e => <option value={e.name}>{e.name}</option>}</For></select></label>
        <Show when={!current()}><p role="alert">Definition changed. Select the current revision before starting.</p></Show>
        <button type="submit" disabled={!current() || pending() || receipt()?.status === "loading"}>Start {props.definition.routineName}</button>
        <Show when={receipt()?.invocationId}>{id => <A href={`/routines/${encodeURIComponent(id())}`}>View invocation</A>}</Show>
      </form>
    </Show>
    <MediaInteractionPanel definition={props.definition} parameters={json()} sourceSession={props.sourceSession} valid={() => { const invalid = validateParameters(json(), props.definition.parameters.schema); setError(invalid); return !invalid && current(); }} />
  </section>;
}
