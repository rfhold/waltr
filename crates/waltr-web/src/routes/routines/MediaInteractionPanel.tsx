import { For, Show, createEffect, createMemo, createSignal, onCleanup, untrack } from "solid-js";
import { useAppState } from "../../app/context";
import type { ClientManualRoutineDefinition, RoutineBlobDescriptor, RoutinePort } from "../../generated/waltr-client";
import { cancelOwnedMedia, waitAccepted, type MediaBridge, type MediaStatus, type NativeFileInput, type NativeFileOutput, type NativeFiles, type NativeOutputBinding } from "../../platform/media";
import { downloadBlob, fileDeclaration, MediaDownloadCleanupError, uploadFile } from "../../platform/media-transfers";
import { PcmPlayback } from "../../audio/playback";

export function MediaInteractionPanel(props: { definition: ClientManualRoutineDefinition; parameters: string; sourceSession?: string; valid: () => boolean }) {
  const app = useAppState();
  const bridge = () => app.media?.();
  const entries = createMemo(() => props.definition.entrypoints.filter(e => e.trigger === "interactive" && e.startContexts.includes(props.sourceSession ? "session" : "global")));
  const ports = createMemo(() => props.definition.ports.filter(p => p.direction === "input"));
  const eligible = createMemo(() => {
    const snapshot = app.snapshot();
    return snapshot?.status.ready && snapshot.access.status === "authenticated"
      && snapshot.routines.definitions.definitions.some(definition => definition.routineName === props.definition.routineName && definition.routineRevision === props.definition.routineRevision);
  });
  const [entry, setEntry] = createSignal("");
  const [files, setFiles] = createSignal<Record<string, File>>({});
  const [nativeInputs, setNativeInputs] = createSignal<Record<string, NativeFileInput>>({});
  const [nativeOutputs, setNativeOutputs] = createSignal<Record<string, NativeFileOutput>>({});
  const [playOutputs, setPlayOutputs] = createSignal<Record<string, boolean>>({});
  const [picking, setPicking] = createSignal(false);
  const [downloading, setDownloading] = createSignal(false);
  const [downloadCleanupFailed, setDownloadCleanupFailed] = createSignal(false);
  const [interactionCleanupFailed, setInteractionCleanupFailed] = createSignal(false);
  const [texts, setTexts] = createSignal<Record<string, string>>({});
  const [phase, setPhase] = createSignal("idle");
  const [owned, setOwned] = createSignal(false);
  const [preview, setPreview] = createSignal("");
  const [error, setError] = createSignal("");
  const [outputs, setOutputs] = createSignal<Array<{ port: string; descriptor: RoutineBlobDescriptor }>>([]);
  const [downloads, setDownloads] = createSignal<Record<string, { url: string; name: string }>>({});
  const usedDownloads = new Set<string>();
  let owner: number | undefined;
  let interactionMedia: MediaBridge | undefined;
  let selectionMedia: NativeFiles | undefined;
  let failedDownload: { media: MediaBridge; id: number; port: string } | undefined;
  let abort = new AbortController();
  let playback: PcmPlayback | undefined;
  let disposed = false;
  let polling: Promise<void> | undefined;
  let cancellation: Promise<void> | undefined;
  let expiryTimers: ReturnType<typeof setTimeout>[] = [];
  let pickerEpoch = 0;
  const busy = () => owned() || picking() || downloading() || downloadCleanupFailed() || interactionCleanupFailed();
  async function releaseSelections() {
    pickerEpoch++;
    const native = selectionMedia;
    const inputs = Object.values(nativeInputs()), outputs = Object.values(nativeOutputs());
    setNativeInputs({}); setNativeOutputs({});
    await Promise.all([...inputs.map(input => native?.releaseInput(input.handle).catch(() => undefined)), ...outputs.map(output => native?.releaseOutput(output.handle).catch(() => undefined))]);
  }
  async function pickInput(port: RoutinePort) {
    const native = bridge()?.nativeFiles; if (!native || !eligible() || busy()) return;
    const epoch = ++pickerEpoch; selectionMedia = native; setPicking(true); setError(""); cancellation = undefined;
    try {
      const selected = await native.pickInput({ port: port.name, kind: port.media === "image" ? "image" : "file", contentTypes: port.media === "image" ? ["image/png", "image/jpeg", "image/webp"] : port.formats, maxBytes: Math.min(port.media === "image" ? 20971520 : 67108864, Number(BigInt(port.maxBytes))) });
      if (!selected) return;
      if (disposed || epoch !== pickerEpoch || bridge()?.nativeFiles !== native) { await native.releaseInput(selected.handle); return; }
      if (selected.port !== port.name || selected.purpose !== port.media) { await native.releaseInput(selected.handle); throw new Error("Invalid picker binding."); }
      const previous = nativeInputs()[port.name];
      setNativeInputs(current => ({ ...current, [port.name]: selected }));
      if (previous) await native.releaseInput(previous.handle);
    } catch { if (!disposed) setError("Native file selection unavailable."); }
    finally { setPicking(false); }
  }
  async function pickOutput(port: RoutinePort) {
    const native = bridge()?.nativeFiles; if (!native || !eligible() || busy()) return;
    const epoch = ++pickerEpoch; selectionMedia = native; setPicking(true); setError(""); cancellation = undefined;
    try {
      const selected = await native.pickOutput({ port: port.name, suggestedName: port.name });
      if (!selected) return;
      if (disposed || epoch !== pickerEpoch || bridge()?.nativeFiles !== native) { await native.releaseOutput(selected.handle); return; }
      if (selected.port !== port.name) { await native.releaseOutput(selected.handle); throw new Error("Invalid output binding."); }
      const previous = nativeOutputs()[port.name];
      setNativeOutputs(current => ({ ...current, [port.name]: selected }));
      if (previous) await native.releaseOutput(previous.handle);
    } catch { if (!disposed) setError("Native save destination unavailable."); }
    finally { setPicking(false); }
  }
  function cancel(): Promise<void> {
    if (cancellation) return cancellation;
    abort.abort();
    revoke();
    const id = owner;
    setPhase("cancelled");
    cancellation = (async () => {
      const [joined, playbackJoined] = await Promise.all([id === undefined ? Promise.resolve(true) : interactionMedia ? cancelOwnedMedia(interactionMedia, id).catch(() => undefined) : undefined, playback ? playback.dispose().then(() => true, () => false) : true, releaseSelections()]);
      if (joined === undefined || !playbackJoined || (!joined && (interactionMedia?.nativeFiles || interactionMedia?.nativePlayback))) { setInteractionCleanupFailed(true); setPhase("error"); if (!disposed) setError("Media cleanup could not be verified. Replacement is blocked."); }
      else if (!joined) { setPhase("error"); if (!disposed) setError("Stopped locally; the remote outcome is uncertain. No automatic retry was made."); }
    })();
    return cancellation;
  }
  function revoke() { for (const value of Object.values(downloads())) URL.revokeObjectURL(value.url); setDownloads({}); for (const timer of expiryTimers) clearTimeout(timer); expiryTimers = []; }
  onCleanup(() => { disposed = true; revoke(); void cancel(); });
  createEffect(() => {
    const media = bridge(), allowed = eligible();
    untrack(() => {
      if ((!allowed || (interactionMedia && media !== interactionMedia)) && (owned() || owner !== undefined)) void cancel();
      else if ((!allowed || (selectionMedia && media?.nativeFiles !== selectionMedia)) && (picking() || Object.keys(nativeInputs()).length || Object.keys(nativeOutputs()).length)) void releaseSelections();
    });
  });
  async function poll(id: number) {
    const media = interactionMedia!;
    try {
      for (;;) {
        abort.signal.throwIfAborted();
        // Drain metadata and consume each checked-out typed chunk exactly once.
        for (let count = 0; count < 32; count++) {
          const json = await media.takeMediaOutputJson(id); if (!json) break;
          const event = JSON.parse(json);
          if (event.type === "open" && event.format?.type === "pcm16le") playback!.open(event.streamId, event.format);
          else if (event.type === "audio") {
            const bytes = await media.takeMediaOutputBytes(id, event.chunkId);
            if (bytes.length !== event.byteLength) throw new Error("Output chunk identity mismatch.");
            playback!.push(event.streamId, bytes);
          } else if (event.type === "close") playback?.close(event.streamId);
          else if (event.type === "blob") {
            setOutputs(rows => [...rows, { port: event.port, descriptor: event.descriptor }]);
          }
        }
        const status = JSON.parse(await media.mediaInteractionStatusJson(id)) as MediaStatus;
        if (disposed || abort.signal.aborted) return;
        setPreview(status.preview);
        if (["succeeded", "error"].includes(status.phase)) {
          if (!status.cleanupVerified || status.phase !== "succeeded") throw new Error("Interaction failed or remote cleanup could not be verified.");
          await playback?.finish();
          if (disposed || abort.signal.aborted) return;
          setPhase("succeeded"); return;
        }
        await new Promise(resolve => setTimeout(resolve, 10));
      }
    } catch {
      if (!abort.signal.aborted && !disposed) { setError("Media interaction failed. It was cancelled; no automatic retry was made."); await cancel(); }
    }
  }
  async function start() {
    if (!props.valid() || !eligible() || busy()) return;
    const media = bridge(); if (!media) return;
    interactionMedia = media;
    setOwned(true);
    setError(""); revoke(); setOutputs([]); usedDownloads.clear(); abort = new AbortController(); owner = undefined; cancellation = undefined;
    try {
      const inputs = ports().flatMap(port => {
        if (port.media === "audio") { if (port.required) throw new Error("Use session dictation for microphone input."); return []; }
        if (port.media === "text") return texts()[port.name] !== undefined || port.required ? [{ format: "text", port: port.name }] : [];
        if (media.nativeFiles) {
          const selected = nativeInputs()[port.name];
          if (!selected) { if (port.required) throw new Error("Choose a required native file."); return []; }
          return [fileDeclaration(port, { name: selected.name, size: selected.bytes, type: selected.contentType })];
        }
        const file = files()[port.name];
        if (!file) { if (port.required) throw new Error(`Choose a file for ${port.name}.`); return []; }
        return [fileDeclaration(port, file)];
      });
      const nativeBindings: NativeOutputBinding[] = [];
      if (media.nativeFiles) for (const port of props.definition.ports.filter(p => p.direction === "output")) {
        if (port.media === "audio") {
          if (playOutputs()[port.name]) nativeBindings.push({ port: port.name, kind: "playback" });
          else if (port.required) throw new Error("Select playback for the required audio output.");
        } else if (port.media === "file" || port.media === "image") {
          const output = nativeOutputs()[port.name];
          if (output) nativeBindings.push({ port: port.name, kind: "file", handle: output.handle });
          else if (port.required) throw new Error("Choose a destination for the required file output.");
        }
      }
      // Create the sink inside the user gesture, not on the first remote audio event.
      if (!media.nativePlayback && props.definition.ports.some(p => p.direction === "output" && p.media === "audio")) playback = new PcmPlayback(() => { setError("Active PCM output failed. The interaction was cancelled."); void cancel(); });
      setPhase("opening");
      const metadata = JSON.stringify({ routine: props.definition.routineName, revision: props.definition.routineRevision, entrypoint: entry() || entries()[0]!.name, parameters: props.parameters, correlation: crypto.randomUUID(), sourceSession: props.sourceSession ?? null, inputs, selectedTextOutput: props.definition.ports.find(p => p.direction === "output" && p.media === "text")?.name ?? null });
      const acceptanceDeadline = performance.now() + 20000;
      const id = media.nativeFiles ? await media.nativeFiles.open(metadata, Object.values(nativeInputs()).map(({ port, handle }) => ({ port, handle })), nativeBindings) : await media.openMediaInteraction(metadata);
      if (media.nativeFiles) { setNativeInputs({}); setNativeOutputs({}); }
      owner = id;
      if (abort.signal.aborted || disposed) {
        cancellation = undefined;
        await cancel();
        return;
      }
      polling = poll(id);
      await waitAccepted(media, id, () => abort.signal.aborted || disposed, acceptanceDeadline);
      setPhase("live");
      for (const port of ports()) {
        abort.signal.throwIfAborted();
        const file = files()[port.name];
        if (file && !media.nativeFiles) await uploadFile(media, id, port.name, file, abort.signal);
        else if (port.media === "text" && (texts()[port.name] !== undefined || port.required)) {
          const text = texts()[port.name] ?? "";
          if (new TextEncoder().encode(text).length > Math.min(65536, Number(BigInt(port.maxBytes)))) throw new Error("Text input exceeds its limit.");
          const stream = `text-${crypto.randomUUID()}`;
          await media.openMediaTextStream(id, port.name, stream);
          await media.pushMediaTextRevision(id, stream, "0", JSON.stringify({ segment: "text", text, replace: false, committed: true }));
          await media.closeMediaStream(id, stream, 1);
        }
      }
      setPhase("finishing"); await media.finishMediaInput(id);
      await polling;
    } catch {
      if (!disposed && !abort.signal.aborted) { setError("Unable to complete media interaction. Check required files, declared formats, and input limits. No automatic retry was made."); await cancel(); }
    } finally { await cancellation; setOwned(false); }
  }
  async function download(port: string) {
    if (owner === undefined || usedDownloads.has(port) || downloading() || downloadCleanupFailed() || interactionCleanupFailed()) return;
    const id = owner, media = interactionMedia!, signal = abort.signal;
    usedDownloads.add(port);
    setDownloading(true);
    try {
      const result = await downloadBlob(media, id, port, signal);
      if (disposed || signal.aborted || owner !== id || interactionMedia !== media) return;
      const url = URL.createObjectURL(result.blob);
      setDownloads(current => ({ ...current, [port]: { url, name: result.name } }));
      expiryTimers.push(setTimeout(() => { URL.revokeObjectURL(url); setDownloads(current => { const next = { ...current }; delete next[port]; return next; }); }, 60000));
    } catch (error) {
      if (error instanceof MediaDownloadCleanupError) {
        failedDownload = { media, id, port }; setDownloadCleanupFailed(true); setPhase("error");
      }
      if (!disposed) setError(error instanceof MediaDownloadCleanupError ? "Download cleanup could not be verified. Replacement is blocked until explicit cleanup verification succeeds." : "Download unavailable, expired, or interrupted. Grants are single-use; no resume or automatic retry.");
    }
    finally { setDownloading(false); }
  }
  async function verifyDownloadCleanup() {
    const failed = failedDownload; if (!failed || downloading()) return;
    setDownloading(true);
    try { await failed.media.closeMediaDownload(failed.id, failed.port); failedDownload = undefined; setDownloadCleanupFailed(false); setError(""); }
    catch { setError("Download cleanup remains unverified. No automatic retry was made."); }
    finally { setDownloading(false); }
  }
  async function verifyInteractionCleanup() {
    if (owner === undefined || !interactionMedia || owned()) return;
    setOwned(true);
    try {
      if (!await interactionMedia.cancelMediaInteraction(owner)) throw new Error("Unverified cleanup.");
      await playback?.dispose();
      setInteractionCleanupFailed(false); setError("");
    } catch { setError("Remote cleanup remains unverified. No automatic retry was made."); }
    finally { setOwned(false); }
  }
  return <Show when={entries().length}><section aria-label="Media interaction"><h4>Interactive inputs</h4>
    <Show when={bridge()} fallback={<p>This platform has not supplied the native media capability.</p>}>
      <label>Interactive entrypoint<select disabled={busy()} value={entry() || entries()[0]?.name} onChange={e => { setEntry(e.currentTarget.value); void releaseSelections(); setPlayOutputs({}); }}><For each={entries()}>{e => <option value={e.name}>{e.name}</option>}</For></select></label>
      <For each={ports()}>{port => <label>{port.name}{port.required ? " (required)" : ""}
        <Show when={port.media === "file" || port.media === "image"}><Show when={bridge()?.nativeFiles} fallback={<input type="file" disabled={busy()} accept={port.media === "image" ? "image/png,image/jpeg,image/webp" : undefined} onChange={e => { const file = e.currentTarget.files?.[0]; setFiles(current => { const next = { ...current }; if (file) next[port.name] = file; else delete next[port.name]; return next; }); }} />}>
          <button type="button" aria-label={`Choose native file for ${port.name}`} disabled={busy()} onClick={() => void pickInput(port)}>Choose native file for {port.name}</button>
          <Show when={nativeInputs()[port.name]}>{selected => <span>{selected().name} ({selected().bytes} bytes)</span>}</Show>
        </Show></Show>
        <Show when={port.media === "text"}><textarea disabled={busy()} value={texts()[port.name] ?? ""} onInput={e => setTexts(current => ({ ...current, [port.name]: e.currentTarget.value }))} /></Show>
        <Show when={port.media === "audio"}><p>Session microphone uses the configured dictation selection.</p></Show>
      </label>}</For>
      <Show when={bridge()?.nativeFiles}><section aria-label="Native output destinations"><h4>Output destinations</h4>
        <For each={props.definition.ports.filter(port => port.direction === "output")}>{port => <>
          <Show when={port.media === "audio"}><label><input type="checkbox" disabled={busy()} checked={Boolean(playOutputs()[port.name])} onChange={event => setPlayOutputs(current => ({ ...current, [port.name]: event.currentTarget.checked }))} />Play {port.name}{port.required ? " (required)" : ""}</label></Show>
          <Show when={port.media === "file" || port.media === "image"}><button type="button" disabled={busy()} onClick={() => void pickOutput(port)}>Choose save destination for {port.name}</button><Show when={nativeOutputs()[port.name]}><span>Destination selected</span></Show></Show>
        </>}</For>
      </section></Show>
      <Show when={bridge()?.nativeFiles}><button type="button" disabled={busy()} onClick={() => void releaseSelections()}>Clear selected files</button></Show>
      <p>Files: 64 MiB storage limit; images: 20 MiB, PNG/JPEG/WebP signatures only. Providers validate image contents; no local dimension or pixel limits apply. Helper limits are separate (8 MiB each / 16 MiB source / 32 MiB request); no PDF extraction or URL fetch.</p>
      <button type="button" disabled={busy()} onClick={() => void start()}>Start interaction</button><button type="button" disabled={!busy()} onClick={() => void cancel()}>Cancel interaction</button>
      <button type="button" onClick={() => { if (owner !== undefined && interactionMedia?.nativePlayback) void interactionMedia.nativePlayback.drop(owner).catch(() => { setError("Native playback could not be stopped."); void cancel(); }); else void playback?.dispose().catch(() => { setError("Playback cleanup could not be verified."); void cancel(); }); }}>Drop audio playback</button>
      <p role="status">{phase()}</p><p aria-live="polite">{preview()}</p><Show when={error()}><p role="alert">{error()}</p></Show>
      <Show when={downloadCleanupFailed()}><button type="button" disabled={downloading()} onClick={() => void verifyDownloadCleanup()}>Verify download cleanup</button></Show>
      <Show when={interactionCleanupFailed()}><button type="button" disabled={owned()} onClick={() => void verifyInteractionCleanup()}>Verify interaction cleanup</button></Show>
      <For each={outputs()}>{output => <p>{output.descriptor.displayName} ({output.descriptor.bytes} bytes) <Show when={!bridge()?.nativeFiles} fallback={<span>Native output destination belongs to Rust.</span>}><Show when={downloads()[output.port]} fallback={<button disabled={usedDownloads.has(output.port) || downloading() || downloadCleanupFailed() || interactionCleanupFailed()} onClick={() => void download(output.port)}>Prepare download (up to 64 MiB)</button>}>{value => <a href={value().url} download={value().name}>Save file</a>}</Show></Show></p>}</For>
    </Show>
  </section></Show>;
}
