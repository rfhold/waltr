import { describe, expect, test, vi } from "vitest";

import type { ClientSnapshot } from "../../src/generated/waltr-client";
import { loadTauriClient } from "../../src/platform/tauri";
import { emptyConfig, snapshotWithConfig } from "../support/snapshots";

describe("Tauri client adapter", () => {
  test("listens before start, accepts the returned generation, and recovers sequence gaps", async () => {
    const calls: string[] = [];
    let emit!: (event: unknown) => void;
    const initial = namedSnapshot("initial");
    const next = namedSnapshot("next");
    const recovered = namedSnapshot("recovered");
    const listen = vi.fn(async (_name, handler) => {
      calls.push("listen");
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      calls.push(command);
      if (command === "start_runtime") {
        emit(bridgeState(99, 0, namedSnapshot("stale")));
        emit(bridgeState(7, 0, initial));
        return { generation: 7 };
      }
      if (command === "client_state") return { generation: 7, events: [bridgeState(7, 3, recovered)] };
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    const seen: ClientSnapshot[] = [];
    adapter.subscribe((snapshot) => seen.push(snapshot));

    await adapter.start();
    expect(calls.slice(0, 2)).toEqual(["listen", "start_runtime"]);
    expect(adapter.snapshot()).toBe(initial);

    emit(bridgeState(7, 1, next));
    await vi.waitFor(() => expect(adapter.snapshot()).toBe(next));
    emit(bridgeState(8, 2, namedSnapshot("wrong generation")));
    emit(bridgeState(7, 3, namedSnapshot("gap event")));
    await vi.waitFor(() => expect(adapter.snapshot()).toBe(recovered));
    expect(invoke.mock.calls.filter(([command]) => command === "client_state")).toHaveLength(1);
    expect(seen).toEqual([initial, next, recovered]);
  });

  test("returns command state, propagates failures, and deduplicates emitted command events", async () => {
    let emit!: (event: unknown) => void;
    const initial = namedSnapshot("initial");
    const dispatched = namedSnapshot("dispatched");
    let dispatches = 0;
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(2, 0, initial));
        return { generation: 2 };
      }
      if (command === "client_dispatch" && dispatches++ === 0) return bridgeState(2, 1, dispatched);
      throw new Error("dispatch rejected");
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    const seen: ClientSnapshot[] = [];
    adapter.subscribe((snapshot) => seen.push(snapshot));
    await adapter.start();

    await expect(adapter.dispatch({ type: "requestAuthStatuses" })).resolves.toBe(dispatched);
    emit(bridgeState(2, 1, dispatched));
    await Promise.resolve();
    expect(seen).toEqual([initial, dispatched]);
    await expect(adapter.dispatch({ type: "requestAuthStatuses" })).rejects.toThrow("dispatch rejected");
  });

  test("accepts only a post-start fresh generation snapshot on native resume", async () => {
    let emit!: (event: unknown) => void;
    const initial = namedSnapshot("initial");
    const resumed = namedSnapshot("resumed");
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(50, 0, namedSnapshot("startup-stale")));
        emit(bridgeState(6, 0, initial));
        return { generation: 6 };
      }
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    await adapter.start();
    expect(adapter.snapshot()).toBe(initial);

    emit(bridgeState(7, 0, resumed));
    await vi.waitFor(() => expect(adapter.snapshot()).toBe(resumed));
    emit(bridgeState(6, 10, namedSnapshot("old-generation")));
    await Promise.resolve();
    expect(adapter.snapshot()).toBe(resumed);
  });

  test("invokes native access/navigation and unlistens before disposal", async () => {
    const order: string[] = [];
    let emit!: (event: unknown) => void;
    const unlisten = vi.fn(() => order.push("unlisten"));
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return unlisten;
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(4, 0, namedSnapshot("initial")));
        return { generation: 4 };
      }
      if (command === "dispose_runtime") order.push("dispose");
      return undefined;
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    await adapter.start();
    await adapter.access!.start();
    await adapter.access!.cancel();
    await adapter.access!.logout();
    await adapter.openExternalUrl!("https://auth.example/authorize");
    await adapter.dispose();

    expect(invoke.mock.calls).toContainEqual(["access_start"]);
    expect(invoke.mock.calls).toContainEqual(["access_cancel"]);
    expect(invoke.mock.calls).toContainEqual(["access_logout"]);
    expect(invoke.mock.calls).toContainEqual(["open_external_url", { url: "https://auth.example/authorize" }]);
    expect(order).toEqual(["unlisten", "dispose"]);
    await adapter.dispose();
    expect(unlisten).toHaveBeenCalledOnce();
  });

  test("native logout resolves only after its cleared state is accepted", async () => {
    let emit!: (event: unknown) => void;
    let completeLogout!: () => void;
    const logoutGate = new Promise<void>((resolve) => { completeLogout = resolve; });
    const initial = namedSnapshot("initial");
    const signedOut = {
      ...namedSnapshot("signed-out"),
      access: { status: "loginRequired", error: null, activeAttemptId: null },
    } satisfies ClientSnapshot;
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(5, 0, initial));
        return { generation: 5 };
      }
      if (command === "access_logout") {
        await logoutGate;
        return bridgeState(5, 2, signedOut);
      }
      if (command === "client_state") return { generation: 5, events: [bridgeState(5, 2, signedOut)] };
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    await adapter.start();

    let settled = false;
    const logout = adapter.access!.logout().then(() => { settled = true; });
    await Promise.resolve();
    expect(settled).toBe(false);
    expect(adapter.snapshot()).toBe(initial);
    completeLogout();
    await logout;
    expect(adapter.snapshot()).toBe(signedOut);
  });

  test("gap recovery presents logout control before authoritative signed-out state", async () => {
    let emit!: (event: unknown) => void;
    const presentation: string[] = [];
    const initial = namedSnapshot("initial");
    const signedOut = {
      ...namedSnapshot("signed-out"),
      access: { status: "loginRequired", error: null, activeAttemptId: null },
    } satisfies ClientSnapshot;
    const barrier = {
      generation: 8,
      sequence: 2,
      payload: { type: "access", progress: { type: "loggingOut" } },
    };
    const cleared = bridgeState(8, 3, signedOut);
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(8, 0, initial));
        return { generation: 8 };
      }
      if (command === "client_state") return { generation: 8, events: [barrier, cleared] };
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    adapter.access!.subscribe((progress) => presentation.push(progress.type));
    adapter.subscribe((snapshot) => presentation.push(snapshot.access.status));
    await adapter.start();

    emit(barrier);
    await vi.waitFor(() => expect(adapter.snapshot()).toBe(signedOut));
    expect(presentation.slice(-2)).toEqual(["loggingOut", "loginRequired"]);
    expect(invoke).toHaveBeenCalledWith("client_state", { afterSequence: 0 });
  });

  test("replacement during recovery rejects the triggering stale-generation event", async () => {
    let emit!: (event: unknown) => void;
    let resolveRecovery!: (recovery: { generation: number; events: unknown[] }) => void;
    const recovery = new Promise<{ generation: number; events: unknown[] }>((resolve) => {
      resolveRecovery = resolve;
    });
    const initial = namedSnapshot("generation-6");
    const stale = namedSnapshot("stale-gap");
    const fresh = namedSnapshot("generation-7");
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(6, 0, initial));
        return { generation: 6 };
      }
      if (command === "client_state") return recovery;
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    const seen: ClientSnapshot[] = [];
    adapter.subscribe((snapshot) => seen.push(snapshot));
    await adapter.start();

    emit(bridgeState(6, 2, stale));
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("client_state", { afterSequence: 0 }));
    emit(bridgeState(7, 0, fresh));
    resolveRecovery({ generation: 7, events: [] });

    await vi.waitFor(() => expect(adapter.snapshot()).toBe(fresh));
    expect(seen).toEqual([initial, fresh]);
  });
});

function namedSnapshot(etag: string) {
  return snapshotWithConfig({ ...emptyConfig(etag), etag });
}

function bridgeState(generation: number, sequence: number, snapshot: ClientSnapshot) {
  return { generation, sequence, payload: { type: "state", snapshot } };
}
