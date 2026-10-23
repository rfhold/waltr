import { Show, createEffect, createMemo, createSignal, onCleanup, untrack } from "solid-js";
import type { DictationDraft } from "./dictation-draft";
import { validSelection, type DictationHost, type DictationOperation, type RoutineSelection } from "../platform/dictation";

export function DictationControl(props: {
  host: DictationHost; scope: string; sessionId: string; invalidation: number; ready: boolean;
  configuredSelection?: RoutineSelection | null; configurationVersion?: string;
  revision: number; selection: { start: number; end: number };
  beginDraft?: () => DictationDraft;
  onActive?: (active: boolean) => void;
  insert: (text: string, selection: { start: number; end: number }) => void;
}) {
  const [phase, setPhase] = createSignal("idle");
  const [result, setResult] = createSignal<string>();
  const [error, setError] = createSignal<string>();
  let draft: DictationDraft | undefined;
  let operation: DictationOperation | undefined;
  let held: number | string | undefined;
  let finishing = false;
  let epoch = 0;
  let timer: ReturnType<typeof setTimeout> | undefined;
  const cancel = () => {
    epoch++;
    held = undefined;
    finishing = false;
    clearTimeout(timer);
    operation?.cancel();
    operation = undefined;
    draft?.rollback(); draft = undefined;
    props.onActive?.(false);
    setPhase("idle"); setResult(undefined);
  };
  const operationKey = createMemo(() => JSON.stringify([
    props.scope, props.sessionId, props.invalidation, props.ready,
    props.configurationVersion, props.configuredSelection,
  ]));
  const host = createMemo(() => props.host);
  createEffect(() => {
    host(); operationKey();
    untrack(cancel);
  });
  window.addEventListener("blur", cancel);
  const visibility = () => { if (document.hidden) cancel(); };
  document.addEventListener("visibilitychange", visibility);
  onCleanup(() => {
    cancel();
    window.removeEventListener("blur", cancel);
    document.removeEventListener("visibilitychange", visibility);
  });
  const begin = (owner: number | string) => {
    if (!props.ready || operation || held !== undefined || !validSelection(props.configuredSelection)) return;
    setError(undefined); setResult(undefined);
    const revision = props.revision;
    const captured = { ...props.selection };
    const generation = ++epoch;
    held = owner;
    finishing = false;
    try {
      draft = props.beginDraft?.();
      props.onActive?.(true);
      operation = props.host.start({ ...props.configuredSelection }, props.sessionId);
      setPhase("starting");
      let previewed = false;
      const poll = async () => {
        const owned = operation;
        if (!owned || epoch !== generation) return;
        try {
          const state = await owned.status();
          if (epoch !== generation || operation !== owned) return;
          setPhase(finishing && ["starting", "recording"].includes(state.phase) ? "processing" : state.phase);
          if (state.phase === "succeeded") {
            operation = undefined; held = undefined;
            if (state.finalText == null) throw new Error("Missing final text");
            if (draft) {
              if (!draft.update(state.finalText)) setResult(state.finalText);
              draft = undefined;
            } else if (props.revision === revision && props.selection.start === captured.start && props.selection.end === captured.end) props.insert(state.finalText, captured);
            else setResult(state.finalText);
            props.onActive?.(false);
            return;
          }
          if (state.phase === "error" || state.phase === "cancelled") {
            operation = undefined; held = undefined;
            draft?.rollback(); draft = undefined;
            props.onActive?.(false);
            if (state.phase === "error") setError("Dictation failed. Check microphone access and configured Routine.");
            return;
          }
          if (props.host.livePreview === true && (state.preview || previewed)) {
            previewed = true;
            draft?.update(state.preview ?? "");
          }
          timer = setTimeout(() => void poll(), 80);
        } catch {
          if (epoch === generation) { cancel(); setError("Dictation failed. Your edits remain intact."); }
        }
      };
      void poll();
    } catch {
      cancel(); setError("Unable to start dictation. Check configuration and microphone access.");
    }
  };
  const finish = (owner: number | string) => {
    if (held !== owner) return;
    held = undefined;
    const owned = operation;
    const generation = epoch;
    if (!owned || finishing) return;
    finishing = true;
    setPhase("processing");
    void owned.finish().catch(() => {
      if (operation !== owned || epoch !== generation) return;
      cancel(); setError("Unable to finish dictation.");
    });
  };
  const key = (value: string) => value === " " || value === "Enter";
  return <div class="sessions__dictation">
    <button type="button" aria-label="Hold to dictate" aria-pressed={phase() === "starting" || phase() === "recording"}
      style={{ "touch-action": "none", "user-select": "none" }}
      onContextMenu={event => event.preventDefault()}
      title={props.configuredSelection ? "Hold to dictate" : "Session dictation is not configured"}
      disabled={!props.ready || !validSelection(props.configuredSelection) || phase() === "processing"}
      onPointerDown={event => {
        if (event.button !== 0) return;
        event.preventDefault();
        begin(event.pointerId);
        if (held === event.pointerId) event.currentTarget.setPointerCapture?.(event.pointerId);
      }}
      onPointerUp={event => { event.preventDefault(); finish(event.pointerId); }}
      onPointerCancel={cancel}
      onLostPointerCapture={() => { if (typeof held === "number") cancel(); }}
      onKeyDown={event => {
        if (!key(event.key)) return;
        event.preventDefault();
        if (!event.repeat) begin(event.key);
      }}
      onKeyUp={event => { if (key(event.key)) { event.preventDefault(); finish(event.key); } }}
      onBlur={() => { if (held !== undefined) cancel(); }}
      onClick={event => event.preventDefault()}
    >Microphone</button>
    <div role="status" aria-live="polite"><Show when={["starting", "recording", "processing"].includes(phase())}>{phase() === "processing" ? "Transcribing…" : phase() === "starting" ? "Starting microphone…" : "Recording…"}</Show></div>
    <Show when={result() !== undefined}><p>Your draft changed. Dictation is ready to insert.</p><button type="button" onClick={() => { const text = result(); if (text !== undefined) props.insert(text, props.selection); setResult(undefined); }}>Insert dictation</button><button type="button" onClick={cancel}>Discard dictation</button></Show>
    <Show when={error()}><p role="alert">{error()}</p></Show>
  </div>;
}
