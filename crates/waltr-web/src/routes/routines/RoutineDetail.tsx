import { A } from "@solidjs/router";
import { For, Show } from "solid-js";

import type { ClientRoutineInvocationDetail, ClientRoutineResult } from "../../generated/waltr-client";

export function RoutineDetail(props: { detail: ClientRoutineInvocationDetail; headingId?: string }) {
  return (
    <section class="routine-detail" aria-label="Routine invocation detail" aria-live="polite">
      <header class="routine-detail__header">
        <div>
          <p class="eyebrow">{triggerLabel(props.detail.summary.triggerKind)} invocation</p>
          <h1 id={props.headingId}>{props.detail.summary.routineName}</h1>
        </div>
        <span class={`routine-status routine-status--${props.detail.summary.status}`}>{statusLabel(props.detail.summary.status)}</span>
      </header>

      <dl class="routine-detail__metadata">
        <div><dt>Invocation</dt><dd>{props.detail.summary.invocationId}</dd></div>
        <div><dt>Revision</dt><dd>{props.detail.summary.routineRevision}</dd></div>
        <Show when={props.detail.summary.metadata["waltr.routine.source_session_id"]}>
          {(id) => <div><dt>Source session</dt><dd><A href={`/sessions/${encodeURIComponent(id())}`}>{id()}</A></dd></div>}
        </Show>
        <Show when={props.detail.summary.sourceCursor}>
          {(cursor) => <div><dt>Source cursor</dt><dd>{cursor()}</dd></div>}
        </Show>
      </dl>

      <Show when={props.detail.summary.terminalError}>
        {(failure) => <p class="routine-failure" role="alert"><strong>{failure().code}</strong> {failure().message}</p>}
      </Show>

      <ol class="routine-tasks" aria-label="Routine tasks">
        <For each={props.detail.tasks}>{(task) => (
          <li class={`routine-task routine-task--${task.status}`}>
            <details>
              <summary>
                <span class="routine-task__identity"><strong>{task.name}</strong><small>{task.type}</small></span>
                <span class={`routine-status routine-status--${task.status}`}>{statusLabel(task.status)}</span>
              </summary>
              <div class="routine-task__content">
                <Show when={task.error}>
                  {(failure) => <p class="routine-failure"><strong>{failure().code}</strong> {failure().message}</p>}
                </Show>
                <RoutineResults results={task.results} label={`${task.name} results`} />
                <Show when={Object.entries(task.sessionReferences).length > 0}>
                  <dl class="routine-session-references" aria-label={`${task.name} session references`}>
                    <For each={Object.entries(task.sessionReferences)}>{([name, id]) => (
                      <div><dt>{name}</dt><dd><A href={`/sessions/${encodeURIComponent(id!)}`}>{id}</A></dd></div>
                    )}</For>
                  </dl>
                </Show>
              </div>
            </details>
          </li>
        )}</For>
      </ol>
      <RoutineResults results={props.detail.results} label="Routine results" />
    </section>
  );
}

function RoutineResults(props: { results: ClientRoutineResult[]; label: string }) {
  return (
    <Show when={props.results.length > 0}>
      <dl class="routine-results" aria-label={props.label}>
        <For each={props.results}>{(result) => (
          <div><dt>{result.name}</dt><dd>{String(result.value.value)}</dd></div>
        )}</For>
      </dl>
    </Show>
  );
}

export function statusLabel(status: string) {
  return status.charAt(0).toLocaleUpperCase() + status.slice(1);
}

function triggerLabel(kind: string) {
  return kind === "event" ? "Event" : "Manual";
}
