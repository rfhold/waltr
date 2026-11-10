import { A } from "@solidjs/router";
import { For, Show, createMemo, createSignal, onCleanup } from "solid-js";
import { useAppState } from "../../app/context";
import { clientRequestId } from "../../app/requestId";
import type { ClientRoutineInvocationDetail } from "../../generated/waltr-client";
import { TextResultBudget } from "../../platform/media-transfers";
import { TextResult } from "./TextResult";

export function RoutineDetail(props: { detail: ClientRoutineInvocationDetail; headingId?: string }) {
  const app = useAppState();
  const textBudget = new TextResultBudget();
  const key = clientRequestId("routine-action");
  const [requestId, setRequestId] = createSignal("");
  const receipt = createMemo(() => { const r = app.snapshot()?.routines.requests[key]; return r?.requestId === requestId() ? r : undefined; });
  function request(command: import("../../generated/waltr-client").RoutineRequest) {
    if (requestId()) void app.dispatch({ type: "releaseRoutineRequest", operationKey: key, requestId: requestId() }).catch(() => undefined);
    const id = clientRequestId("routine-action-request"); setRequestId(id);
    void app.dispatch({ type: "routineRequest", operationKey: key, requestId: id, command }).catch(() => undefined);
  }
  onCleanup(() => { if (requestId()) void app.dispatch({ type: "releaseRoutineRequest", operationKey: key, requestId: requestId() }).catch(() => undefined); });
  return <section class="routine-detail" aria-label="Routine invocation detail">
    <header><h1 id={props.headingId}>{props.detail.summary.routineName}</h1><p>{statusLabel(props.detail.summary.status)}</p></header>
    <dl><dt>Invocation</dt><dd>{props.detail.summary.invocationId}</dd><dt>Revision</dt><dd>{props.detail.summary.routineRevision}</dd><dt>Entrypoint</dt><dd>{props.detail.summary.triggerName} ({props.detail.summary.triggerKind})</dd>
      <Show when={props.detail.summary.sourceSession}>{id => <><dt>Source session</dt><dd><A href={`/sessions/${encodeURIComponent(id())}`}>{id()}</A></dd></>}</Show>
    </dl>
    <details><summary>Pinned historical definition</summary><p>{props.detail.pinnedDefinition.description}</p><pre>{props.detail.pinnedDefinition.parameters.schemaJson}</pre></details>
    <Show when={["admitted", "running"].includes(props.detail.summary.status)}><button disabled={receipt()?.status === "loading"} onClick={() => request({ type: "cancel", invocationId: props.detail.summary.invocationId })}>Request cancellation</button></Show>
    <Show when={receipt()}>{r => <p role="status">{r().status}{r().cancellationStatus ? `: ${r().cancellationStatus}` : ""} {r().error}</p>}</Show>
    <Show when={props.detail.summary.status === "uncertain"}><p role="alert">An effect may have occurred. This invocation will not be replayed automatically.</p></Show>
    <h2>Dynamic operations</h2><ol aria-label="Routine operations"><For each={props.detail.operations}>{operation => <li><details><summary>{operation.name} · {statusLabel(operation.status)}</summary><p>{operation.id}</p><Show when={operation.arguments}><pre>{operation.arguments}</pre></Show><Show when={operation.result}><pre>{operation.result}</pre></Show></details></li>}</For></ol>
    <Show when={props.detail.nextOperationCursor}>{cursor => <button onClick={() => request({ type: "operations", invocationId: props.detail.summary.invocationId, afterCursor: cursor(), limit: 50 })}>Load more operations</button>}</Show>
    <h2>Selected business results</h2><Show when={props.detail.summary.result}><pre>{props.detail.summary.result}</pre></Show>
    <For each={props.detail.summary.helpers}>{helper => <p><A href={`/sessions/${encodeURIComponent(helper.sessionId)}`}>Helper {helper.definitionIdentity}</A> {helper.operationId}</p>}</For>
    <For each={props.detail.summary.textResults}>{result => <TextResult invocation={props.detail.summary.invocationId} result={result} budget={textBudget} />}</For>
    <For each={props.detail.summary.binaryResults}>{result => <p>{result.displayName} · {result.bytes} bytes. Historical descriptor only; not a download grant.</p>}</For>
  </section>;
}

export function statusLabel(status: string) { return status.charAt(0).toUpperCase() + status.slice(1); }
