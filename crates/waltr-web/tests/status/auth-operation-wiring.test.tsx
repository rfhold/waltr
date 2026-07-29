import { cleanup, fireEvent, render, screen, waitFor } from "@solidjs/testing-library";
import { createSignal } from "solid-js";
import { afterEach, describe, expect, test, vi } from "vitest";

vi.mock("../../src/wasm/client", () => ({
  loadWasmClient: vi.fn(),
}));

import { useAppState, type AppState } from "../../src/app/context";
import { AppStateProvider } from "../../src/app/state";
import type { ClientCommand, ClientOperationView, ClientSnapshot } from "../../src/generated/waltr-client";
import type { ClientSnapshotListener, WasmClient } from "../../src/wasm/client";
import { claudeConfig, snapshotWithConfig } from "../support/snapshots";

type AuthOperationCase = {
  name: string;
  commandType: ClientCommand["type"];
  kind: Extract<ClientOperationView["kind"], `auth${string}`>;
  field: "begin" | "complete" | "cancel" | "refresh" | "logout";
  run: (app: AppState) => Promise<ClientSnapshot>;
};

const AUTH_OPERATIONS: AuthOperationCase[] = [
  { name: "explicit begin", commandType: "beginAuth", kind: "authBegin", field: "begin", run: (app) => app.dispatchBeginExplicitAuth(target()) },
  { name: "complete", commandType: "completeAuth", kind: "authComplete", field: "complete", run: (app) => app.dispatchCompleteAuth("attempt-1", "callback") },
  { name: "cancel", commandType: "cancelAuth", kind: "authCancel", field: "cancel", run: (app) => app.dispatchCancelAuth("attempt-1") },
  { name: "refresh", commandType: "refreshAuth", kind: "authRefresh", field: "refresh", run: (app) => app.dispatchRefreshAuth(target()) },
  { name: "logout", commandType: "logoutAuth", kind: "authLogout", field: "logout", run: (app) => app.dispatchLogoutAuth(target()) },
];

afterEach(cleanup);

describe("auth operation wiring", () => {
  test.each(AUTH_OPERATIONS)("$name settles from its matching ready operation", async (operation) => {
    const harness = renderOperation(operation);
    fireEvent.click(await screen.findByRole("button", { name: `Run ${operation.name}` }));
    await waitFor(() => expect(screen.getByText("waiting")).toBeTruthy());

    harness.publish(operationSnapshot(harness.current(), operation, `${operation.name}-1`, "ready"));
    await waitFor(() => expect(screen.getByText("complete")).toBeTruthy());
    if (operation.name === "explicit begin") expect(screen.getByTestId("explicit-target").textContent).toBe("provider:claude");
  });

  test.each(AUTH_OPERATIONS)("$name rejects from its matching failed operation", async (operation) => {
    const harness = renderOperation(operation);
    fireEvent.click(await screen.findByRole("button", { name: `Run ${operation.name}` }));
    await waitFor(() => expect(screen.getByText("waiting")).toBeTruthy());

    harness.publish(operationSnapshot(harness.current(), operation, `${operation.name}-1`, "failed", `${operation.name} denied`));
    await waitFor(() => expect(screen.getByText(`failed:${operation.name} denied`)).toBeTruthy());
    if (operation.name === "explicit begin") expect(screen.getByTestId("explicit-target").textContent).toBe("none");
  });

  test("a ready foreign key cannot settle the active helper", async () => {
    const operation = AUTH_OPERATIONS.find((item) => item.name === "refresh")!;
    const harness = renderOperation(operation);
    fireEvent.click(await screen.findByRole("button", { name: "Run refresh" }));
    await waitFor(() => expect(screen.getByText("waiting")).toBeTruthy());

    const active = harness.current();
    const foreign = operationSnapshot(active, operation, "foreign-key", "ready");
    foreign.auth.refresh = active.auth.refresh;
    harness.publish(foreign);
    await Promise.resolve();
    expect(screen.getByText("waiting")).toBeTruthy();

    harness.publish(operationSnapshot(foreign, operation, "refresh-1", "ready"));
    await waitFor(() => expect(screen.getByText("complete")).toBeTruthy());
  });

  test("a successful server-callback attempt clears its explicit target", async () => {
    const operation = AUTH_OPERATIONS[0];
    const harness = renderOperation(operation);
    fireEvent.click(await screen.findByRole("button", { name: "Run explicit begin" }));
    harness.publish(operationSnapshot(harness.current(), operation, "explicit begin-1", "ready"));
    await waitFor(() => expect(screen.getByTestId("explicit-target").textContent).toBe("provider:claude"));

    harness.publish({
      ...harness.current(),
      auth: {
        ...harness.current().auth,
        activeAttempt: {
          target: target(),
          attemptId: "attempt-1",
          authUrl: "https://auth.example/authorize",
          verificationUrl: "",
          userCode: "",
          expiresAtMs: 1_800_000_000_000n,
          pollIntervalSeconds: 1,
          messages: [],
          completionMode: "serverCallback",
          registrationMethod: "dynamic",
          lifecycle: "succeeded",
          failureReason: "",
        },
      },
    });

    await waitFor(() => expect(screen.getByTestId("explicit-target").textContent).toBe("none"));
  });

  test("a stale succeeded attempt does not clear a new explicit reauthorization", async () => {
    const operation = AUTH_OPERATIONS[0];
    const initial = snapshotWithConfig(claudeConfig());
    initial.auth.activeAttempt = authAttempt("old-attempt", "succeeded");
    const harness = renderOperation(operation, initial);

    fireEvent.click(await screen.findByRole("button", { name: "Run explicit begin" }));
    await waitFor(() => expect(screen.getByTestId("explicit-target").textContent).toBe("provider:claude"));
    harness.publish(operationSnapshot(harness.current(), operation, "explicit begin-1", "ready"));
    expect(screen.getByTestId("explicit-target").textContent).toBe("provider:claude");

    const pending = harness.current();
    pending.auth.activeAttempt = authAttempt("new-attempt", "pending");
    harness.publish({ ...pending, auth: { ...pending.auth } });
    expect(screen.getByTestId("explicit-target").textContent).toBe("provider:claude");

    const succeeded = harness.current();
    succeeded.auth.activeAttempt = authAttempt("new-attempt", "succeeded");
    harness.publish({ ...succeeded, auth: { ...succeeded.auth } });
    await waitFor(() => expect(screen.getByTestId("explicit-target").textContent).toBe("none"));
  });
});

function renderOperation(operation: AuthOperationCase, initial = snapshotWithConfig(claudeConfig())) {
  let current = initial;
  let listener: ClientSnapshotListener | undefined;
  const publish = (snapshot: ClientSnapshot) => {
    current = snapshot;
    listener?.(snapshot);
  };
  const client: WasmClient = {
    endpoint: "https://waltr.example",
    snapshot: () => current,
    start: async () => undefined,
    subscribe: (next) => {
      listener = next;
      return () => { listener = undefined; };
    },
    dispatch: async (command) => {
      if (command.type === operation.commandType) {
        publish(operationSnapshot(current, operation, `${operation.name}-1`, "loading"));
      }
      return current;
    },
  };

  render(() => (
    <AppStateProvider loadWasmClient={async () => client}>
      <OperationHarness operation={operation} />
    </AppStateProvider>
  ));

  return { current: () => current, publish };
}

function authAttempt(attemptId: string, lifecycle: "pending" | "succeeded") {
  return {
    target: target(),
    attemptId,
    authUrl: "https://auth.example/authorize",
    verificationUrl: "",
    userCode: "",
    expiresAtMs: 1_800_000_000_000n,
    pollIntervalSeconds: 1,
    messages: [],
    completionMode: "serverCallback" as const,
    registrationMethod: "dynamic" as const,
    lifecycle,
    failureReason: "",
  };
}

function OperationHarness(props: { operation: AuthOperationCase }) {
  const app = useAppState();
  const [result, setResult] = createSignal("idle");
  return (
    <div>
      <button
        type="button"
        onClick={() => {
          setResult("waiting");
          void props.operation.run(app)
            .then(() => setResult("complete"))
            .catch((error: Error) => setResult(`failed:${error.message}`));
        }}
      >
        Run {props.operation.name}
      </button>
      <span>{result()}</span>
      <span data-testid="explicit-target">{app.explicitAuthTarget() ? `${app.explicitAuthTarget()!.kind}:${app.explicitAuthTarget()!.key}` : "none"}</span>
    </div>
  );
}

function operationSnapshot(
  snapshot: ClientSnapshot,
  operation: AuthOperationCase,
  key: string,
  status: "loading" | "ready" | "failed",
  error: string | null = null,
): ClientSnapshot {
  return {
    ...snapshot,
    auth: { ...snapshot.auth, [operation.field]: { status, error, key } },
    operations: {
      current: {
        ...snapshot.operations.current,
        [key]: operationView(operation.kind, status, error),
      },
    },
  };
}

function operationView(kind: AuthOperationCase["kind"], status: string, error: string | null): ClientOperationView {
  switch (kind) {
    case "authBegin": return { kind, status, error, target: target(), attempt: null };
    case "authRefresh": return { kind, status, error, target: target() };
    case "authLogout": return { kind, status, error, target: target() };
    case "authComplete": return { kind, status, error };
    case "authCancel": return { kind, status, error };
  }
}

function target() {
  return { kind: "provider", key: "claude" };
}
