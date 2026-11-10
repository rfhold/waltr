import { A, useSearchParams } from "@solidjs/router";
import { For, Show, createEffect, createMemo, createSignal, on, onCleanup, untrack } from "solid-js";
import { useAppState } from "../../app/context";
import { clientRequestId } from "../../app/requestId";
import { RoutineStartForm } from "./RoutineStartForm";
import { statusLabel } from "./RoutineDetail";

export function RoutinesRoute() {
  const app = useAppState();
  const [params] = useSearchParams();
  const session = createMemo(() => typeof params.session === "string" && params.session ? params.session : null);
  const routines = createMemo(() => app.snapshot()?.routines);
  const history = createMemo(() => session() ? routines()?.sessionHistories[session()!] : routines()?.history);
  const operationKey = clientRequestId("routine-history");
  const [requestId, setRequestId] = createSignal("");
  const receipt = createMemo(() => { const r = routines()?.requests[operationKey]; return r?.requestId === requestId() ? r : undefined; });
  createEffect(() => {
    if (!app.clientLoaded() || !app.systemReady()) return;
    const id = session();
    void app.dispatch({ type: "registerRoutineDefinitionWatchInterest" }).catch(() => undefined);
    void app.dispatch(id ? { type: "registerSessionRoutineInvocationWatchInterest", sessionId: id } : { type: "registerRoutineInvocationWatchInterest" }).catch(() => undefined);
    onCleanup(() => {
      void app.dispatch({ type: "releaseRoutineDefinitionWatchInterest" }).catch(() => undefined);
      void app.dispatch(id ? { type: "releaseSessionRoutineInvocationWatchInterest", sessionId: id } : { type: "releaseRoutineInvocationWatchInterest" }).catch(() => undefined);
    });
  });
  function release() { const id = requestId(); if (id) void app.dispatch({ type: "releaseRoutineRequest", operationKey, requestId: id }).catch(() => undefined); }
  createEffect(on(session, () => { untrack(release); setRequestId(""); }));
  onCleanup(release);
  function page(cursor = "0") {
    release();
    const id = clientRequestId("history-page"); setRequestId(id);
    void app.dispatch({ type: "routineRequest", operationKey, requestId: id, command: { type: "invocations", afterCursor: cursor, limit: 50, sourceSession: session() } }).catch(() => undefined);
  }
  return <section class="routines" aria-labelledby="routines-title">
    <header><h1 id="routines-title">Routines</h1><p>{session() ? `Source session: ${session()}` : "All authorized invocations"}</p><Show when={session()}><A href="/routines">All Routine history</A></Show></header>
    <section><h2>Definitions</h2><For each={routines()?.definitions.definitions.map(d => JSON.stringify([session(), d.routineName, d.routineRevision]))} fallback={<p>No Routines are available.</p>}>{identity => {
      const definition = createMemo(() => routines()?.definitions.definitions.find(d => JSON.stringify([session(), d.routineName, d.routineRevision]) === identity));
      return <Show when={definition()}>{d => <RoutineStartForm definition={d()} sourceSession={session() ?? undefined} />}</Show>;
    }}</For></section>
    <section><h2>Recent history</h2><HistoryRows rows={history()?.recent ?? []} />
      <Show when={history()?.watch.error}><p role="status">History observation interrupted; retained results may be stale.</p></Show>
      <button type="button" onClick={() => page()} disabled={receipt()?.status === "loading"}>Browse history from beginning</button>
      <Show when={requestId()}><section aria-label="Selected history page">
        <p role="status">{receipt()?.status ?? "loading"}</p>
        <Show when={receipt()?.status === "expired"}><p role="alert">This history page expired. Load it again explicitly.</p></Show>
        <Show when={receipt()?.error}><p role="alert">{receipt()?.error}</p></Show>
        <HistoryRows rows={receipt()?.selectedInvocations ?? []} />
        <Show when={receipt()?.nextCursor}>{cursor => <button disabled={receipt()?.status !== "ready"} onClick={() => page(cursor())}>Next history page</button>}</Show>
        <button onClick={() => { release(); setRequestId(""); }}>Close page</button>
      </section></Show>
    </section>
  </section>;
}

function HistoryRows(props: { rows: import("../../generated/waltr-client").ClientRoutineInvocationSummary[] }) {
  return <ol class="routine-history" aria-label="Routine invocation history"><For each={props.rows} fallback={<li>No Routine invocations.</li>}>{row => <li><A href={`/routines/${encodeURIComponent(row.invocationId)}`}><strong>{row.routineName}</strong> {row.triggerKind} · {statusLabel(row.status)} · {row.invocationId}</A></li>}</For></ol>;
}
