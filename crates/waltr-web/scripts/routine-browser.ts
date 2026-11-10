import init, { WaltrClient } from "../generated/wasm-current/waltr_client";
import { browserDictation } from "../src/platform/browser-dictation";
import type { ClientSnapshot } from "../src/generated/waltr-client";
import { downloadBlob, fileDeclaration, uploadFile } from "../src/platform/media-transfers";
import { waitAccepted } from "../src/platform/media";

await init();
const client = WaltrClient.forCurrentOrigin();
await client.dispatchJson(JSON.stringify({ type: "registerRoutineDefinitionWatchInterest" }));
const snapshot = () => JSON.parse(clientSnapshot) as ClientSnapshot;
let clientSnapshot = await client.snapshotJson();
const dictation = browserDictation(client, snapshot);
const tracks: MediaStreamTrack[] = [];
const getUserMedia = navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
navigator.mediaDevices.getUserMedia = async constraints => { const stream = await getUserMedia(constraints); tracks.push(...stream.getTracks()); return stream; };
const timer = setInterval(async () => { clientSnapshot = await client.snapshotJson(); dictation.reconcile(snapshot()); }, 10);
let operation: ReturnType<typeof dictation.start> | undefined;
let released = false;
let liveBeforeRelease = false;
let status: unknown;
let fileResult: unknown;
let fileAbort: AbortController | undefined;
let fileUrls = 0;
const poll = setInterval(async () => {
  if (!operation) return;
  try {
    const current = await operation.status();
    if (!released && current.preview) liveBeforeRelease = true;
    status = current;
    document.querySelector("#state")!.textContent = JSON.stringify({ status, released, liveBeforeRelease });
  } catch (error) { document.querySelector("#state")!.textContent = String(error); }
}, 20);
document.querySelector<HTMLButtonElement>("#start")!.onclick = () => {
  released = false; liveBeforeRelease = false; status = undefined;
  operation = dictation.start({ routine: "routine", trigger: "live", input: "audio", output: "text" }, "source");
};
document.querySelector<HTMLButtonElement>("#finish")!.onclick = () => { released = true; void operation?.finish(); };
document.querySelector<HTMLButtonElement>("#cancel")!.onclick = () => operation?.cancel();
document.querySelector<HTMLButtonElement>("#files")!.onclick = () => { void (async () => {
  const abort = new AbortController(); fileAbort = abort; fileResult = undefined;
  let owner: number | undefined;
  try {
    const definition = snapshot().routines.definitions.definitions[0];
    const selected = ["document", "image"].map(port => ({ port, file: document.querySelector<HTMLInputElement>(`#${port}`)!.files![0] }));
    owner = await client.openMediaInteraction(JSON.stringify({ routine: definition.routineName, revision: definition.routineRevision, entrypoint: "live", parameters: "{}", correlation: crypto.randomUUID(), sourceSession: null, inputs: selected.map(({ port, file }) => fileDeclaration(definition.ports.find(p => p.name === port)!, file)), selectedTextOutput: "text" }));
    await waitAccepted(client, owner, () => false);
    for (const { port, file } of selected) await uploadFile(client, owner, port, file, abort.signal);
    await client.finishMediaInput(owner);
    const events: unknown[] = [];
    for (;;) {
      for (;;) { const json = client.takeMediaOutputJson(owner); if (!json) break; events.push(JSON.parse(json)); }
      const media = JSON.parse(client.mediaInteractionStatusJson(owner));
      if (media.phase === "error") throw new Error("File interaction failed.");
      if (media.phase === "succeeded") {
        if (!media.cleanupVerified) throw new Error("Cleanup unverified.");
        fileResult = { phase: "downloading" };
        const saved = await downloadBlob(client, owner, "result", abort.signal);
        const url = URL.createObjectURL(saved.blob); fileUrls++; URL.revokeObjectURL(url);
        fileResult = { phase: "succeeded", size: saved.blob.size, name: saved.name, contents: await saved.blob.text(), events, snapshot: await client.snapshotJson() }; return;
      }
      await new Promise(resolve => setTimeout(resolve, 10));
    }
   } catch { fileResult = { phase: "error" }; if (owner !== undefined) await client.cancelMediaInteraction(owner).catch(() => false); }
})(); };
Object.assign(window, { routineProof: {
  ready: () => snapshot().routines.definitions.definitions.length === 1,
  snapshot,
  state: () => ({ status, released, liveBeforeRelease }),
  mediaStatus: () => client.mediaInteractionStatusJson(1),
  fileResult: () => fileResult,
  cancelFiles: () => fileAbort?.abort(),
  fileUrls: () => fileUrls,
  tracksEnded: () => tracks.length > 0 && tracks.every(track => track.readyState === "ended"),
  openOnly: () => client.openMediaInteraction(JSON.stringify({ routine: "routine", revision: "revision", entrypoint: "live", parameters: "{}", correlation: crypto.randomUUID(), sourceSession: "source", inputs: [{ format: "pcm16le", port: "audio", sampleRate: 24000, channels: 1 }], selectedTextOutput: "text" })),
  dispose: () => { clearInterval(timer); clearInterval(poll); dictation.dispose(); client.dispose(); },
} });
