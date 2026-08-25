import { A, useParams } from "@solidjs/router";
import { Match, Switch, createEffect, createMemo, on, onCleanup, untrack } from "solid-js";

import { useAppState } from "../../app/context";
import { clientRequestId } from "../../app/requestId";
import { InlineAlert } from "../../components/ui";
import { RoutineDetail } from "./RoutineDetail";

export function RoutineInvocationRoute() {
  const app = useAppState();
  const params = useParams<{ invocationId: string }>();
  const ready = createMemo(() => app.clientLoaded() && app.systemReady());
  const detail = createMemo(() => app.snapshot()?.routines.details[params.invocationId]);
  let registered = false;

  createEffect(on(ready, (isReady) => {
    if (!isReady || registered) return;
    registered = true;
    untrack(() => void app.dispatch({ type: "registerRoutineInvocationWatchInterest" }).catch(() => undefined));
  }));

  createEffect(() => {
    params.invocationId;
    if (!ready()) return;
    untrack(loadDetail);
  });

  onCleanup(() => {
    if (registered) void app.dispatch({ type: "releaseRoutineInvocationWatchInterest" }).catch(() => undefined);
  });

  function loadDetail() {
    const current = detail();
    if (current?.status === "ready" || current?.status === "loading") return;
    void app.dispatch({
      type: "loadRoutineInvocationDetail",
      requestId: clientRequestId("routine-detail"),
      invocationId: params.invocationId,
    }).catch(() => undefined);
  }

  return (
    <section class="routines routine-invocation" aria-labelledby="routine-invocation-title">
      <A class="routine-invocation__back" href="/routines">Back to Routines</A>
      <Switch>
        <Match when={!detail() || detail()?.status === "loading"}>
          <div class="routine-invocation__state"><h1 id="routine-invocation-title">Routine invocation</h1><p>Loading invocation detail...</p></div>
        </Match>
        <Match when={detail()?.status === "failed"}>
          <div class="routine-invocation__state">
            <h1 id="routine-invocation-title">Routine invocation</h1>
            <InlineAlert tone="danger">{detail()?.error ?? "Unable to load invocation detail."}</InlineAlert>
            <button type="button" onClick={loadDetail}>Retry</button>
          </div>
        </Match>
        <Match when={detail()?.detail}>{(value) => <RoutineDetail detail={value()} headingId="routine-invocation-title" />}</Match>
      </Switch>
    </section>
  );
}
