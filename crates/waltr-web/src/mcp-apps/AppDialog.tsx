import { createEffect, createSignal, onCleanup, onMount, Show } from "solid-js";
import { useAppState } from "../app/context";
import { Dialog } from "../components/ui/Dialog";
import { AppHost } from "./AppHost";
import { appPayload, createAppCommands } from "./commands";
import { appsAvailable, configuredContainerAddress } from "./policy";

export function AppDialog(props: { sessionId: string; callId: string; onClose: () => void }) {
  const app = useAppState();
  const commands = createAppCommands(app.snapshot, app.dispatch);
  const [status, setStatus] = createSignal("Loading app...");
  let container!: HTMLDivElement;
  let host: AppHost | undefined;
  let active = true;
  const sessionId = props.sessionId;
  const callId = props.callId;
  const cleanup = () => {
    if (!active) return;
    active = false;
    commands.dispose();
    const closing = host;
    host = undefined;
    if (closing) void closing.dispose().then(() => container?.replaceChildren(), () => container?.replaceChildren());
    else container?.replaceChildren();
  };
  const unload = (event: PageTransitionEvent) => {
    if (!event.persisted) { cleanup(); props.onClose(); }
  };
  onCleanup(() => { window.removeEventListener("pagehide", unload, true); cleanup(); });
  createEffect(() => {
    if (props.sessionId !== sessionId || props.callId !== callId || !app.clientLoaded() || !app.systemReady() || app.snapshot()?.access.status !== "authenticated") {
      cleanup();
      props.onClose();
    }
  });
  onMount(() => {
    window.addEventListener("pagehide", unload, true);
    if (!active) return;
    if (!appsAvailable()) { setStatus("Apps are unavailable on this platform."); return; }
    let containerUrl: string;
    try {
      containerUrl = configuredContainerAddress().href;
    } catch {
      setStatus("App container is not configured correctly. Check the server-provided app container URL. For external hosting, set VITE_WALTR_MCP_APP_CONTAINER_URL to a dedicated app container URL on a separate origin and rebuild the web client.");
      return;
    }
    let failureMessage = "App unavailable. Close this dialog and try opening it again.";
    void commands.open(sessionId, callId).then(async view => {
      if (!active) return;
      const payload = appPayload(view);
      host = new AppHost(container, {
        containerUrl, html: view.html, ...payload,
        theme: document.documentElement.dataset.theme === "light" ? "light" : "dark",
        platform: __WALTR_BUILD_PLATFORM__ === "browser" ? "web" : "desktop",
        containerDimensions: { width: Math.min(960, Math.max(240, window.innerWidth - 64)), height: Math.min(640, Math.max(200, window.innerHeight - 180)) },
        onCallTool: (name, args, signal) => commands.call(view.appId, name, args, signal),
      });
      await host.ready.catch(error => {
        if (error instanceof Error && error.message === "App initialization timed out") {
          failureMessage = "App container did not finish loading. Check that the dedicated app container service is reachable and configured for this site, then close this dialog and try again.";
        }
        throw new Error("App host unavailable");
      });
      if (active) setStatus("");
    }).catch(() => {
      if (!active) return;
      cleanup();
      setStatus(failureMessage);
    });
  });
  return <Dialog title="Tool app" onClose={props.onClose} class="sessions__app-dialog" style={{ width: "min(1000px, 100%)" }}>
    <div style={{ width: "min(960px, calc(100vw - 64px))", "max-width": "100%", "max-height": "calc(100dvh - 140px)", overflow: "auto" }}>
      <Show when={status()}><p role="status">{status()}</p></Show>
      <div ref={container} aria-label="Tool app content" />
    </div>
    <button type="button" onClick={props.onClose}>Close app</button>
  </Dialog>;
}
