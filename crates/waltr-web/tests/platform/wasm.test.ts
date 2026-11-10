import { beforeEach, describe, expect, test, vi } from "vitest";
import { mediaBridge, routineDefinition, routineSnapshot } from "../support/routines";

const wasm = vi.hoisted(() => ({
  clients: [] as Array<{
    dispatchJson: ReturnType<typeof vi.fn>;
    dispose: ReturnType<typeof vi.fn>;
    nextStateJson: ReturnType<typeof vi.fn>;
    snapshotJson: ReturnType<typeof vi.fn>;
    openMediaInteraction: ReturnType<typeof vi.fn>;
    cancelMediaInteraction: ReturnType<typeof vi.fn>;
    mediaInteractionStatusJson: ReturnType<typeof vi.fn>;
  }>,
  cancelCapture: vi.fn(),
}));

vi.mock("../../src/audio/live", () => ({ startLiveCapture: (push: (bytes: Uint8Array) => Promise<void>) => ({
  ready: Promise.resolve(),
  stop: async () => { await push(new Uint8Array([0, 0])); },
  cancel: wasm.cancelCapture,
}) }));

vi.mock("../../generated/wasm-current/waltr_client", () => {
  class WaltrClient {
    static forCurrentOrigin() {
      let rejectNext: ((error: Error) => void) | undefined;
      const client = {
        ...mediaBridge(),
        dispatchJson: vi.fn(async () => JSON.stringify(snapshot("dispatched"))),
        dispose: vi.fn(() => rejectNext?.(new Error("client state stream closed"))),
        nextStateJson: vi.fn(() => new Promise<string>((_resolve, reject) => {
          rejectNext = reject;
        })),
        snapshotJson: vi.fn(async () => JSON.stringify(snapshot("initial"))),
      };
      wasm.clients.push(client);
      return client;
    }
  }

  return { default: vi.fn(async () => undefined), WaltrClient };
});

import { loadBrowserClient } from "../../src/wasm/client";

describe("browser WASM client adapter", () => {
  beforeEach(() => {
    wasm.clients.length = 0;
    wasm.cancelCapture.mockClear();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  test("provides hosted browser access operations", async () => {
    const assign = vi.spyOn(window.location, "assign").mockImplementation(() => undefined);
    const fetchMock = vi.fn()
      .mockResolvedValueOnce({ ok: true })
      .mockResolvedValueOnce({ ok: false });
    vi.stubGlobal("fetch", fetchMock);
    const adapter = await loadBrowserClient();

    await adapter.access!.openLogin("/oidc/login?return_to=%2Fsessions");
    await expect(adapter.access!.logout()).resolves.toBe(true);
    await expect(adapter.access!.logout()).resolves.toBe(false);

    expect(assign).toHaveBeenCalledWith("/oidc/login?return_to=%2Fsessions");
    expect(fetchMock).toHaveBeenNthCalledWith(1, "/oidc/logout", {
      method: "POST",
      credentials: "same-origin",
    });
    expect(fetchMock).toHaveBeenNthCalledWith(2, "/oidc/logout", {
      method: "POST",
      credentials: "same-origin",
    });
  });

  test.each(["logout", "accessLoss", "dispose"])("%s cancels capture and the same client's owned interaction", async exit => {
    vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true })));
    const adapter = await loadBrowserClient();
    const running = adapter.start();
    await vi.waitFor(() => expect(wasm.clients[0].nextStateJson).toHaveBeenCalledOnce());
    const op = adapter.dictation!.start({ routine: "fork", trigger: "t", input: "audio", output: "text" }, "s");
    await op.finish();
    const client = wasm.clients[0];
    expect(client.openMediaInteraction).toHaveBeenCalledOnce();
    if (exit === "logout") await adapter.access!.logout();
    if (exit === "accessLoss") {
      client.dispatchJson.mockResolvedValueOnce(JSON.stringify({ ...snapshot("lost"), status: { ready: false } }));
      await adapter.dispatch({ type: "requestAuthStatuses" });
    }
    if (exit === "dispose") adapter.dispose();
    expect(client.cancelMediaInteraction).toHaveBeenCalledWith(7);
    expect(wasm.cancelCapture).toHaveBeenCalledOnce();
    adapter.dispose();
    await running;
  });

  test("startup does not retain unused watches; disposal releases pending state delivery and is terminal", async () => {
    const adapter = await loadBrowserClient();
    const client = wasm.clients[0];
    const running = adapter.start();
    await vi.waitFor(() => expect(client.nextStateJson).toHaveBeenCalledOnce());
    await adapter.start();
    expect(client.dispatchJson).not.toHaveBeenCalled();

    adapter.dispose();

    await expect(running).resolves.toBeUndefined();
    expect(client.dispose).toHaveBeenCalledOnce();
    await expect(adapter.start()).rejects.toThrow("client adapter disposed");
    await expect(adapter.dispatch({ type: "requestAuthStatuses" })).rejects.toThrow("client adapter disposed");
    expect(client.dispatchJson).not.toHaveBeenCalled();
    adapter.dispose();
    expect(client.dispose).toHaveBeenCalledOnce();
  });
  test.each(["removed", "revision", "unrelated"])("published %s catalog reconciles the actual browser dictation before listeners", async change => {
    const adapter = await loadBrowserClient(), running = adapter.start();
    await vi.waitFor(() => expect(wasm.clients[0].nextStateJson).toHaveBeenCalledOnce());
    const operation = adapter.dictation!.start({ routine: "fork", trigger: "t", input: "audio", output: "text" }, "s");
    await operation.finish();
    const client = wasm.clients[0], next = snapshot("new-config");
    if (change === "removed") next.routines.definitions.definitions = [];
    if (change === "revision") next.routines.definitions.definitions[0].routineRevision = "new";
    if (change === "unrelated") next.routines.definitions.definitions.push(routineDefinition({ routineName: "other" }));
    client.dispatchJson.mockResolvedValueOnce(JSON.stringify(next));
    let notifiedAfterCancel = false;
    adapter.subscribe(value => { if (value.config.current?.etag === "new-config") notifiedAfterCancel = wasm.cancelCapture.mock.calls.length > 0; });
    await adapter.dispatch({ type: "requestAuthStatuses" });
    if (change === "unrelated") { expect(client.cancelMediaInteraction).not.toHaveBeenCalled(); expect((await operation.status()).preview).toBe("hypothesis"); }
    else { expect(notifiedAfterCancel).toBe(true); expect(client.cancelMediaInteraction).toHaveBeenCalledExactlyOnceWith(7); expect((await operation.status()).preview).toBe(""); }
    adapter.dispose(); await running;
  });
  test("replacement while admission is pending never cancels or publishes through the replacement client", async () => {
    const old = await loadBrowserClient(), running = old.start();
    await vi.waitFor(() => expect(wasm.clients[0].nextStateJson).toHaveBeenCalledOnce());
    wasm.clients[0].mediaInteractionStatusJson.mockReturnValue(JSON.stringify({ phase: "opening", cleanupVerified: false, preview: "" }));
    const operation = old.dictation!.start({ routine: "fork", trigger: "t", input: "audio", output: "text" }, "s");
    const finishing = operation.finish();
    await vi.waitFor(() => expect(wasm.clients[0].openMediaInteraction).toHaveBeenCalledOnce());
    old.dispose();
    const replacement = await loadBrowserClient(), replacementRunning = replacement.start();
    await vi.waitFor(() => expect(wasm.clients[1].nextStateJson).toHaveBeenCalledOnce());
    await finishing;
    expect(wasm.clients[0].cancelMediaInteraction).toHaveBeenCalledExactlyOnceWith(7);
    expect(wasm.clients[1].cancelMediaInteraction).not.toHaveBeenCalled(); expect((await operation.status()).preview).toBe("");
    replacement.dispose(); await Promise.all([running, replacementRunning]);
  });
});

function snapshot(etag: string) {
  const result = routineSnapshot(routineDefinition({ entrypoints: [{ name: "t", trigger: "interactive", selector: null, startContexts: ["session"] }] }));
  result.config.current!.etag = etag;
  return result;
}
