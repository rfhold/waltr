import { A } from "@solidjs/router";
import { For, Match, Show, Switch, createEffect, createMemo, on, onCleanup, untrack } from "solid-js";

import { useAppState } from "../../app/context";
import { clientRequestId } from "../../app/requestId";
import { InlineAlert } from "../../components/ui";
import type { OperationSnapshot } from "../../generated/waltr-client";
import { statusLabel } from "./RoutineDetail";

const HISTORY_PAGE_SIZE = 50;

export function RoutinesRoute() {
  const app = useAppState();
  const snapshot = createMemo(() => app.snapshot()?.routines);
  const ready = createMemo(() => app.clientLoaded() && app.systemReady());
  let registered = false;

  createEffect(on(ready, (isReady) => {
    if (!isReady || registered) return;
    registered = true;
    untrack(() => {
      void app.dispatch({ type: "registerRoutineDefinitionWatchInterest" }).catch(() => undefined);
      void app.dispatch({ type: "registerRoutineInvocationWatchInterest" }).catch(() => undefined);
    });
  }));

  onCleanup(() => {
    if (!registered) return;
    void app.dispatch({ type: "releaseRoutineDefinitionWatchInterest" }).catch(() => undefined);
    void app.dispatch({ type: "releaseRoutineInvocationWatchInterest" }).catch(() => undefined);
  });

  const loadOlder = () => {
    const token = snapshot()?.history.olderPageToken;
    if (token === null || token === undefined || snapshot()?.history.pagination.status === "loading") return;
    void app.dispatch({
      type: "loadOlderRoutineInvocations",
      requestId: clientRequestId("routine-history"),
      pageSize: HISTORY_PAGE_SIZE,
    }).catch(() => undefined);
  };

  return (
    <section class="routines" aria-labelledby="routines-title">
      <header class="routines__header">
        <div><p class="eyebrow">Automation</p><h1 id="routines-title">Routines</h1></div>
        <p>Available definitions and durable execution history. Start a Routine from a session.</p>
      </header>

      <div class="routines__sections">
        <section class="routines__section" aria-labelledby="routine-definitions-title">
          <div class="routines__section-heading"><h2 id="routine-definitions-title">Manual definitions</h2><WatchState watch={snapshot()?.definitions.watch} hasData={Boolean(snapshot()?.definitions.digest)} /></div>
          <Switch>
            <Match when={!snapshot() || isInitial(snapshot()!.definitions.watch, Boolean(snapshot()!.definitions.digest))}><p>Loading Routine definitions...</p></Match>
            <Match when={isError(snapshot()!.definitions.watch) && !snapshot()!.definitions.digest}><InlineAlert tone="danger">Unable to load Routine definitions.</InlineAlert></Match>
            <Match when={snapshot()!.definitions.definitions.length === 0}><p class="routines__empty">No manual Routines are available.</p></Match>
            <Match when={true}>
              <ol class="routine-definitions">
                <For each={snapshot()!.definitions.definitions}>{(definition) => (
                  <li>
                    <div><h3>{definition.displayName}</h3><code>{definition.routineName}</code></div>
                    <p>{definition.description}</p>
                    <Show when={definition.inputs.length > 0} fallback={<p class="routine-definitions__none">No inputs</p>}>
                      <ul aria-label={`${definition.displayName} inputs`}>
                        <For each={definition.inputs}>{(input) => <li><strong>{input.name}</strong> <span>{input.type}{input.required ? ", required" : ", optional"}</span></li>}</For>
                      </ul>
                    </Show>
                  </li>
                )}</For>
              </ol>
            </Match>
          </Switch>
        </section>

        <section class="routines__section" aria-labelledby="routine-history-title">
          <div class="routines__section-heading"><h2 id="routine-history-title">Recent history</h2><WatchState watch={snapshot()?.history.watch} hasData={snapshot()?.history.throughSequence !== null && snapshot()?.history.throughSequence !== undefined} /></div>
          <Switch>
            <Match when={!snapshot() || isInitial(snapshot()!.history.watch, snapshot()!.history.throughSequence !== null)}><p>Loading Routine history...</p></Match>
            <Match when={isError(snapshot()!.history.watch) && snapshot()!.history.throughSequence === null}><InlineAlert tone="danger">Unable to load Routine history.</InlineAlert></Match>
            <Match when={snapshot()!.history.recent.length === 0}><p class="routines__empty">No Routine invocations yet.</p></Match>
            <Match when={true}>
              <ol class="routine-history" aria-label="Routine invocation history">
                <For each={snapshot()!.history.recent}>{(invocation) => (
                  <li><A aria-label={`View ${invocation.routineName} invocation ${invocation.invocationId}`} href={`/routines/${encodeURIComponent(invocation.invocationId)}`}>
                    <span><strong>{invocation.routineName}</strong><small>{invocation.triggerKind === "event" ? "Event" : "Manual"} invocation</small></span>
                    <span><span class={`routine-status routine-status--${invocation.status}`}>{statusLabel(invocation.status)}</span><time>{formatTime(invocation.admittedAtMs)}</time></span>
                  </A></li>
                )}</For>
              </ol>
              <Show when={snapshot()!.history.olderPageToken !== null}>
                <button type="button" class="routines__older" disabled={snapshot()!.history.pagination.status === "loading"} onClick={loadOlder}>
                  {snapshot()!.history.pagination.status === "loading" ? "Loading older invocations..." : "Load older invocations"}
                </button>
              </Show>
              <Show when={snapshot()!.history.pagination.status === "failed"}><InlineAlert tone="danger">Unable to load older invocations.</InlineAlert></Show>
            </Match>
          </Switch>
        </section>
      </div>

    </section>
  );
}

function WatchState(props: { watch: OperationSnapshot | undefined; hasData: boolean }) {
  return <Show when={(isError(props.watch) || props.watch?.status === "waiting") && props.hasData}><span class="routines__watch-state" role="status">Reconnecting...</span></Show>;
}

function isInitial(watch: OperationSnapshot | undefined, hasData: boolean) {
  return !watch || (["idle", "loading", "opened", "waiting"].includes(watch.status) && !hasData);
}

function isError(watch: OperationSnapshot | undefined) {
  return watch?.status === "failed" || Boolean(watch?.error);
}

function formatTime(milliseconds: bigint) {
  const value = Number(milliseconds);
  return Number.isFinite(value) ? new Date(value).toLocaleString() : "Unknown time";
}
