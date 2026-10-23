import { beforeEach, describe, expect, test, vi } from "vitest";

const wasm = vi.hoisted(() => ({
  clients: [] as Array<{
    dispatchJson: ReturnType<typeof vi.fn>;
    dispose: ReturnType<typeof vi.fn>;
    nextStateJson: ReturnType<typeof vi.fn>;
    snapshotJson: ReturnType<typeof vi.fn>;
    startAudioRun: ReturnType<typeof vi.fn>;
    cancelAudioRun: ReturnType<typeof vi.fn>;
  }>,
  cancelCapture: vi.fn(),
}));

vi.mock("../../src/audio", () => ({ start: () => ({
  ready: Promise.resolve(),
  stop: async () => ({ bytes: new Uint8Array([0, 0]), format: { encoding: "PCM_S16LE", sampleRate: 24000, channels: 1 } }),
  cancel: wasm.cancelCapture,
}) }));

vi.mock("../../generated/wasm-current/waltr_client", () => {
  class WaltrClient {
    static forCurrentOrigin() {
      let rejectNext: ((error: Error) => void) | undefined;
      const client = {
        dispatchJson: vi.fn(async () => JSON.stringify(snapshot("dispatched"))),
        dispose: vi.fn(() => rejectNext?.(new Error("client state stream closed"))),
        nextStateJson: vi.fn(() => new Promise<string>((_resolve, reject) => {
          rejectNext = reject;
        })),
        snapshotJson: vi.fn(async () => JSON.stringify(snapshot("initial"))),
        startAudioRun: vi.fn(() => 1),
        audioRunStatusJson: vi.fn(() => JSON.stringify({ operationId: 1, phase: "processing", preview: "" })),
        cancelAudioRun: vi.fn(),
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

  test.each(["logout", "accessLoss", "dispose"])("%s cancels capture and the same client's owned Run", async exit => {
    vi.stubGlobal("fetch", vi.fn(async () => ({ ok: true })));
    const adapter = await loadBrowserClient();
    const op = adapter.dictation!.start({ routine: "r", trigger: "t", input: "audio", output: "text" }, "s");
    await op.finish();
    const client = wasm.clients[0];
    expect(client.startAudioRun).toHaveBeenCalledOnce();
    if (exit === "logout") await adapter.access!.logout();
    if (exit === "accessLoss") {
      client.dispatchJson.mockResolvedValueOnce(JSON.stringify({ ...snapshot("lost"), status: { ready: false } }));
      await adapter.dispatch({ type: "requestAuthStatuses" });
    }
    if (exit === "dispose") adapter.dispose();
    expect(client.cancelAudioRun).toHaveBeenCalledWith(1);
    expect(wasm.cancelCapture).toHaveBeenCalledOnce();
    adapter.dispose();
  });

  test("disposal releases pending state delivery and is terminal", async () => {
    const adapter = await loadBrowserClient();
    const client = wasm.clients[0];
    const running = adapter.start();
    await vi.waitFor(() => expect(client.nextStateJson).toHaveBeenCalledOnce());

    adapter.dispose();

    await expect(running).resolves.toBeUndefined();
    expect(client.dispose).toHaveBeenCalledOnce();
    await expect(adapter.start()).rejects.toThrow("client adapter disposed");
    await expect(adapter.dispatch({ type: "requestAuthStatuses" })).rejects.toThrow("client adapter disposed");
    expect(client.dispatchJson).not.toHaveBeenCalled();
    adapter.dispose();
    expect(client.dispose).toHaveBeenCalledOnce();
  });
});

function snapshot(etag: string) {
  return { config: { etag }, status: { ready: true } };
}
