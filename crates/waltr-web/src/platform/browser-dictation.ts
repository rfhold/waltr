import { startLiveCapture } from "../audio/live";
import type { ClientSnapshot } from "../generated/waltr-client";
import { validSelection, type DictationHost, type DictationOperation, type DictationStatus } from "./dictation";
import { cancelOwnedMedia, waitAccepted, type MediaBridge, type MediaStatus } from "./media";
import { initialParameters } from "../routes/routines/parameters";
import { PcmPlayback } from "../audio/playback";

export function browserDictation(bridge: MediaBridge, snapshot: () => ClientSnapshot | undefined, capture = startLiveCapture): DictationHost & { reconcile(value: ClientSnapshot | undefined): void } {
  let disposed = false;
  let next = 0;
  let active: DictationOperation | undefined;
  let reconcileActive: ((value: ClientSnapshot | undefined) => void) | undefined;
  return {
    livePreview: true,
    reconcile(value) { reconcileActive?.(value); },
    start(selection, sessionId) {
      if (disposed || active) throw new Error("Microphone is busy or unavailable.");
      if (!validSelection(selection) || !sessionId) throw new Error("Configure a Routine first.");
      const definition = snapshot()?.routines.definitions.definitions.find(d => d.routineName === selection.routine);
      if (!definition || !definition.entrypoints.some(e => e.name === selection.trigger && e.trigger === "interactive" && e.startContexts.includes("session"))) throw new Error("Select a current interactive Routine.");
      function identity(value: ClientSnapshot | undefined) {
        const current = value?.routines.definitions.definitions.find(d => d.routineName === selection.routine);
        const entry = current?.entrypoints.find(e => e.name === selection.trigger && e.trigger === "interactive" && e.startContexts.includes("session"));
        if (!value?.status.ready || value.access.status !== "authenticated" || !current || !entry) return undefined;
        return JSON.stringify({ revision: current.routineRevision, entry, parameters: current.parameters, ports: current.ports.filter(p => p.name === selection.input || p.name === selection.output || (p.direction === "output" && p.media === "audio")), selection: value.config.current?.sessionDictation });
      }
      const pinnedIdentity = identity(snapshot());
      if (!pinnedIdentity) throw new Error("Select a current interactive Routine.");
      const id = ++next;
      let owner: number | undefined;
      let admission: Promise<number> | undefined;
      let sequence = 0;
      let cancelled = false;
      let joined = false;
      let finishing: Promise<void> | undefined;
      let cleanupJoin: Promise<void> | undefined;
      let state: DictationStatus = { operationId: id, phase: "starting", preview: "" };
      const playback = definition.ports.some(p => p.direction === "output" && p.media === "audio") ? new PcmPlayback(() => fail()) : undefined;
      const stream = `microphone-${id}`;
      async function cancelRemote() { const target = owner ?? await admission?.catch(() => undefined); return target === undefined || await cancelOwnedMedia(bridge, target); }
      const fail = () => {
        if (cancelled) return;
        cancelled = true; state = { operationId: id, phase: "error", preview: "" };
        cleanupJoin = Promise.all([handle.cancel(), cancelRemote(), playback?.dispose()]).then(() => { if (active === operation) active = undefined; }).catch(() => undefined);
      };
      function current() { if (identity(snapshot()) !== pinnedIdentity) operation.cancel(); return !cancelled && !disposed; }
      const handle = capture(async bytes => {
        try {
        if (!current()) throw new Error("Cancelled");
        if (owner === undefined) {
          const acceptanceDeadline = performance.now() + 20000;
          admission = bridge.openMediaInteraction(JSON.stringify({ routine: definition.routineName, revision: definition.routineRevision, entrypoint: selection.trigger, parameters: initialParameters(definition.parameters.schema), correlation: crypto.randomUUID(), sourceSession: sessionId, inputs: [{ format: "pcm16le", port: selection.input, sampleRate: 24000, channels: 1 }], selectedTextOutput: selection.output }));
          owner = await admission;
          if (!current()) throw new Error("Cancelled");
          await waitAccepted(bridge, owner, () => cancelled || disposed, acceptanceDeadline);
          if (!current()) throw new Error("Cancelled");
          await bridge.openMediaPcmStream(owner, selection.input, stream, JSON.stringify({ sampleRate: 24000, channels: 1 }));
        }
        if (!current()) throw new Error("Cancelled");
        // One worklet credit; no queued promises and no retry of an ambiguous send.
        await bridge.pushMediaPcm(owner, stream, sequence++, bytes);
        } catch (error) { if (!cancelled) fail(); throw error; }
      }, () => queueMicrotask(fail), 24000, () => queueMicrotask(() => { if (!cancelled && !finishing) void operation.finish(); }));
      const operation: DictationOperation = {
        operationId: id,
        async status() {
          try {
          if (current() && owner !== undefined) {
            for (let count = 0; count < 32; count++) {
               const metadata = await bridge.takeMediaOutputJson(owner); if (!metadata) break;
               if (!current()) return state;
              const event = JSON.parse(metadata);
              if (event.type === "open" && event.format?.type === "pcm16le") { if (!playback) throw new Error("Audio output sink unavailable."); playback.open(event.streamId, event.format); }
              else if (event.type === "audio") {
                 const bytes = await bridge.takeMediaOutputBytes(owner, event.chunkId);
                 if (!current()) return state;
                if (bytes.length !== event.byteLength || !playback) throw new Error("Invalid output chunk.");
                playback.push(event.streamId, bytes);
              } else if (event.type === "close") playback?.close(event.streamId);
            }
             const status = JSON.parse(await bridge.mediaInteractionStatusJson(owner)) as MediaStatus;
             if (!current()) return state;
            if (status.phase === "error") fail();
            else if (status.phase === "succeeded") {
              if (!joined) { await handle.stop(); joined = true; }
               if (joined && status.cleanupVerified) { await playback?.finish(); if (!current()) return state; state = { operationId: id, phase: "succeeded", preview: status.preview, finalText: status.finalText }; if (active === operation) active = undefined; }
            } else state = { ...state, preview: status.preview };
          }
          } catch { fail(); }
          return state;
        },
        finish() {
          if (finishing) return finishing;
          if (cancelled || state.phase === "error") return Promise.resolve();
          finishing = (async () => {
            try {
              state = { ...state, phase: "processing" };
              await handle.stop(); joined = true;
              if (!current()) return;
              if (owner === undefined) { operation.cancel(); await cleanupJoin; return; }
              await bridge.closeMediaStream(owner, stream, sequence);
              if (!current()) return;
              await bridge.finishMediaInput(owner);
            } catch { if (!cancelled) fail(); }
          })();
          return finishing;
        },
        cancel() {
          if (cancelled) return;
          cancelled = true; state = { operationId: id, phase: "cancelled", preview: "" };
          cleanupJoin = Promise.all([handle.cancel(), cancelRemote(), playback?.dispose()]).then(([, clean]) => { joined = true; if (!clean) state = { operationId: id, phase: "error", preview: "" }; if (active === operation) active = undefined; }).catch(() => { state = { operationId: id, phase: "error", preview: "" }; });
        },
      };
      active = operation;
      reconcileActive = value => { if (active === operation && identity(value) !== pinnedIdentity) operation.cancel(); };
      void handle.ready.then(() => { if (current() && !finishing) state = { ...state, phase: "recording" }; }, fail);
      return operation;
    },
    cancel() { active?.cancel(); },
    dispose() { disposed = true; active?.cancel(); },
  };
}
