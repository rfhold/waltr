import { describe, expect, test, vi } from "vitest";

import type { ClientSnapshot } from "../../src/generated/waltr-client";
import { loadTauriClient } from "../../src/platform/tauri";
import { emptyConfig, snapshotWithConfig } from "../support/snapshots";

describe("Tauri client adapter", () => {
  test("keeps platform control events and access topology adapter-private", async () => {
    let emit!: (event: unknown) => void;
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(1, 0, namedSnapshot("initial")));
        return { generation: 1 };
      }
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });

    expect("platform" in adapter).toBe(false);
    expect(adapter.access).toBeUndefined();
    await adapter.start();
    const initial = adapter.snapshot();

    emit({ generation: 1, sequence: 1, payload: { type: "platform", startup: "firstCatalogDownload" } });
    emit({
      generation: 1,
      sequence: 2,
      payload: { type: "platform", startup: { type: "recoverableFailure", endpoint: "SECRET_ENDPOINT" } },
    });
    await Promise.resolve();
    expect(adapter.snapshot()).toBe(initial);
    expect(JSON.stringify(adapter)).not.toContain("SECRET_ENDPOINT");
  });

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
    expect(invoke).toHaveBeenCalledWith("client_dispatch", { command: { type: "requestAuthStatuses" }, generation: 2 });
    emit(bridgeState(2, 1, dispatched));
    await Promise.resolve();
    expect(seen).toEqual([initial, dispatched]);
    await expect(adapter.dispatch({ type: "requestAuthStatuses" })).rejects.toThrow("dispatch rejected");
  });

  test("accepts a sequence-zero state generation on native resume", async () => {
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
      if (command === "open_authorization_attempt") return;
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    await adapter.start();
    expect(adapter.snapshot()).toBe(initial);

    emit(bridgeState(7, 0, resumed));
    await vi.waitFor(() => expect(adapter.snapshot()).toBe(resumed));
    await adapter.openAuthorizationAttempt!("resumed-attempt");
    expect(invoke).toHaveBeenCalledWith("open_authorization_attempt", { attemptId: "resumed-attempt", generation: 7 });
    emit(bridgeState(6, 10, namedSnapshot("old-generation")));
    await Promise.resolve();
    expect(adapter.snapshot()).toBe(resumed);
  });

  test("accepts a bounded platform-first generation without fabricating client state", async () => {
    let emit!: (event: unknown) => void;
    const initial = namedSnapshot("initial");
    const resumed = namedSnapshot("resumed");
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(6, 0, initial));
        return { generation: 6 };
      }
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    await adapter.start();

    emit(bridgePlatform(7, 0, "starting"));
    await Promise.resolve();
    expect(adapter.snapshot()).toBe(initial);

    emit(bridgeState(7, 1, resumed));
    await vi.waitFor(() => expect(adapter.snapshot()).toBe(resumed));
    emit(bridgePlatform(6, 2, "recoverableFailure"));
    await Promise.resolve();
    expect(adapter.snapshot()).toBe(resumed);

    emit({ generation: 8, sequence: 0, payload: { type: "platform", startup: { endpoint: "not-bounded" } } });
    emit(bridgeState(8, 1, namedSnapshot("invalid-announcement")));
    await Promise.resolve();
    expect(adapter.snapshot()).toBe(resumed);
  });

  test("consumes current-generation platform events for state sequencing", async () => {
    let emit!: (event: unknown) => void;
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(3, 0, namedSnapshot("initial")));
        return { generation: 3 };
      }
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    await adapter.start();

    emit(bridgePlatform(3, 1, "notificationPermissionRequired"));
    const next = namedSnapshot("after-platform");
    emit(bridgeState(3, 2, next));
    await vi.waitFor(() => expect(adapter.snapshot()).toBe(next));
  });

  test("recovers private platform events when filling a state sequence gap", async () => {
    let emit!: (event: unknown) => void;
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(4, 0, namedSnapshot("initial")));
        return { generation: 4 };
      }
      if (command === "client_state") {
        return { generation: 4, events: [bridgePlatform(4, 1, "notificationPermissionRequired")] };
      }
      throw new Error(`unexpected command ${command}`);
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never });
    await adapter.start();

    const next = namedSnapshot("after-recovery");
    emit(bridgeState(4, 2, next));
    await vi.waitFor(() => expect(adapter.snapshot()).toBe(next));
    expect(invoke).toHaveBeenCalledWith("client_state", { afterSequence: 0 });
  });

  test("opens authorization attempts opaquely, opens external URLs, and unlistens before disposal", async () => {
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
    await adapter.openAuthorizationAttempt!("attempt-private");
    await adapter.openExternalUrl!("https://auth.example/authorize");
    await adapter.dispose();

    expect(adapter.access).toBeUndefined();
    expect(invoke).toHaveBeenCalledWith("dispose_runtime", { generation: 4 });
    expect(invoke.mock.calls).toContainEqual(["open_authorization_attempt", { attemptId: "attempt-private", generation: 4 }]);
    expect(invoke.mock.calls).toContainEqual(["open_external_url", { url: "https://auth.example/authorize", generation: 4 }]);
    expect(order).toEqual(["unlisten", "dispose"]);
    await adapter.dispose();
    expect(unlisten).toHaveBeenCalledOnce();
  });

  test("selects a server and keeps access login and logout native and generation-fenced", async () => {
    let emit!: (event: unknown) => void;
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(12, 0, namedSnapshot("remote")));
        return { generation: 12 };
      }
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never }, "remote");
    await adapter.start();
    await adapter.access!.openLogin("/oidc/login?return_to=SECRET");
    await expect(adapter.access!.logout()).resolves.toBe(true);
    expect(invoke).toHaveBeenCalledWith("start_runtime", { serverId: "remote" });
    expect(invoke).toHaveBeenCalledWith("start_access_login", { generation: 12 });
    expect(invoke).toHaveBeenCalledWith("logout_access", { generation: 12 });
    expect(JSON.stringify(invoke.mock.calls)).not.toContain("SECRET");
    await adapter.dispose();
    expect(invoke).toHaveBeenCalledWith("dispose_runtime", { generation: 12 });
    await expect(adapter.access!.logout()).rejects.toThrow("not ready");
  });

  test("built-in local admission does not expose user access operations", async () => {
    const api = { invoke: vi.fn() as never, listen: vi.fn() as never };
    expect((await loadTauriClient(api)).access).toBeUndefined();
    expect((await loadTauriClient(api, "local")).access).toBeUndefined();
    expect((await loadTauriClient(api, "remote")).access).toBeDefined();
  });

  test("dispatch and openers reject before IPC when unstarted or disposed", async () => {
    let emit!: (event: unknown) => void;
    const listen = vi.fn(async (_name, handler) => {
      emit = (payload) => handler({ payload });
      return vi.fn();
    });
    const invoke = vi.fn(async (command) => {
      if (command === "start_runtime") {
        emit(bridgeState(30, 0, namedSnapshot("active")));
        return { generation: 30 };
      }
    });
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never }, "remote");
    const rejectOperations = async () => {
      await expect(adapter.dispatch({ type: "requestAuthStatuses" })).rejects.toThrow("not ready");
      await expect(adapter.openAuthorizationAttempt!("stale-attempt")).rejects.toThrow("not ready");
      await expect(adapter.openExternalUrl!("https://example.com")).rejects.toThrow("not ready");
    };
    await rejectOperations();
    expect(invoke).not.toHaveBeenCalled();
    await adapter.start();
    await adapter.dispose();
    invoke.mockClear();
    await rejectOperations();
    expect(invoke).not.toHaveBeenCalled();
  });

  test("disposes a late startup by its exact generation, never an unqualified replacement", async () => {
    let complete!: (value: { generation: number }) => void;
    const runtime = new Promise<{ generation: number }>((resolve) => { complete = resolve; });
    const listen = vi.fn(async () => vi.fn());
    const invoke = vi.fn(async (command) => command === "start_runtime" ? runtime : undefined);
    const adapter = await loadTauriClient({ invoke: invoke as never, listen: listen as never }, "old");
    const starting = adapter.start();
    await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith("start_runtime", { serverId: "old" }));
    await adapter.dispose();
    expect(invoke).not.toHaveBeenCalledWith("dispose_runtime");
    complete({ generation: 20 });
    await starting;
    expect(invoke).toHaveBeenCalledWith("dispose_runtime", { generation: 20 });
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

function bridgePlatform(generation: number, sequence: number, startup: string) {
  return { generation, sequence, payload: { type: "platform", startup } };
}
