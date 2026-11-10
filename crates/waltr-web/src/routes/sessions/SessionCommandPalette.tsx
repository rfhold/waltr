import { A } from "@solidjs/router";
import { For, Show, createMemo, createSignal, onCleanup, onMount } from "solid-js";
import { useAppState } from "../../app/context";
import { Dialog } from "../../components/ui";
import { RoutineStartForm } from "../routines/RoutineStartForm";

export function SessionCommandPalette(props: { sessionId: string }) {
  const app = useAppState();
  const [open, setOpen] = createSignal(false);
  const [search, setSearch] = createSignal("");
  const definitions = createMemo(() => (app.snapshot()?.routines.definitions.definitions ?? []).filter(d =>
    d.entrypoints.some(e => (e.trigger === "manual" || e.trigger === "interactive") && e.startContexts.includes("session")) &&
    `${d.routineName} ${d.description}`.toLowerCase().includes(search().toLowerCase())));
  let trigger: HTMLButtonElement | undefined;
  function close() { setOpen(false); trigger?.focus(); }
  const shortcut = (event: KeyboardEvent) => {
    if (event.key.toLowerCase() !== "p" || !event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || open()) return;
    if (event.target instanceof HTMLElement && (event.target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(event.target.tagName))) return;
    if (document.querySelector('[role="dialog"][aria-modal="true"]') || !app.systemReady()) return;
    event.preventDefault(); setOpen(true);
  };
  onMount(() => document.addEventListener("keydown", shortcut));
  onCleanup(() => document.removeEventListener("keydown", shortcut));
  return <>
    <button ref={trigger} type="button" class="sessions__detail-status-button" onClick={() => setOpen(true)}>Commands</button>
    <Show when={open()}><SessionCommands onClose={close} sessionId={props.sessionId}>
      <label>Search commands<input value={search()} onInput={e => setSearch(e.currentTarget.value)} /></label>
      <For each={definitions().map(d => JSON.stringify([props.sessionId, d.routineName, d.routineRevision]))} fallback={<p>No session commands available.</p>}>{identity => {
        const definition = createMemo(() => definitions().find(d => JSON.stringify([props.sessionId, d.routineName, d.routineRevision]) === identity));
        return <Show when={definition()}>{d => <RoutineStartForm definition={d()} sourceSession={props.sessionId} />}</Show>;
      }}</For>
      <A href={`/routines?session=${encodeURIComponent(props.sessionId)}`} onClick={close}>Session Routine history</A>
    </SessionCommands></Show>
  </>;
}

function SessionCommands(props: import("solid-js").ParentProps<{ onClose: () => void; sessionId: string }>) {
  const app = useAppState();
  void app.dispatch({ type: "registerRoutineDefinitionWatchInterest" }).catch(() => undefined);
  onCleanup(() => { void app.dispatch({ type: "releaseRoutineDefinitionWatchInterest" }).catch(() => undefined); });
  return <Dialog title="Session commands" class="sessions__commands-dialog" onClose={props.onClose}>{props.children}</Dialog>;
}
