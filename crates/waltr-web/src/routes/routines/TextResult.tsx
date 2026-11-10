import { Show, createSignal, onCleanup } from "solid-js";
import { useAppState } from "../../app/context";
import type { RoutineTextResult } from "../../generated/waltr-client";
import { readTextResult, type TextResultBudget } from "../../platform/media-transfers";

export function TextResult(props: { invocation: string; result: RoutineTextResult; budget: TextResultBudget }) {
  const app = useAppState();
  const [text, setText] = createSignal<string>();
  const [error, setError] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  let release: (() => void) | undefined;
  const abort = new AbortController();
  onCleanup(() => { abort.abort(); release?.(); });
  async function load() {
    const media = app.media?.(); if (!media || busy()) return;
    setBusy(true); setError("");
    try {
      release?.(); release = props.budget.reserve(props.result);
      const result = await readTextResult(media, props.invocation, props.result, abort.signal);
      if (!abort.signal.aborted) setText(result);
    } catch { release?.(); release = undefined; if (!abort.signal.aborted) setError("Text result unavailable or exceeds its declared limit."); }
    finally { setBusy(false); }
  }
  return <section><h3>{props.result.name}</h3><p>{props.result.utf8Bytes} UTF-8 bytes</p>
    <Show when={app.media?.()} fallback={<p>This platform has not supplied text result reading.</p>}><button disabled={busy() || text() !== undefined} onClick={() => void load()}>Read text result</button></Show>
    <Show when={error()}><p role="alert">{error()}</p></Show><Show when={text() !== undefined}><pre>{text()}</pre><button onClick={() => { setText(undefined); release?.(); release = undefined; }}>Close text result</button></Show>
  </section>;
}
