import { beforeEach, describe, expect, test, vi } from "vitest";

const wasm = vi.hoisted(() => ({
  clients: [] as Array<{
    dispatchJson: ReturnType<typeof vi.fn>;
    dispose: ReturnType<typeof vi.fn>;
    nextStateJson: ReturnType<typeof vi.fn>;
    snapshotJson: ReturnType<typeof vi.fn>;
  }>,
}));

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
  return { config: { etag } };
}
