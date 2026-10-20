import { cleanup, fireEvent, render, screen, waitFor } from "@solidjs/testing-library";
import { afterEach, expect, test, vi } from "vitest";
import { AppStateProvider } from "../../src/app/state";
import { ServerPicker } from "../../src/app/ServerPicker";
import { ServerControls } from "../../src/app/ServerControls";
import { useAppState } from "../../src/app/context";
import { AppRouter } from "../../src/routes";
import type { ClientAdapter } from "../../src/platform/client";
import type { ServerProfile } from "../../src/platform/tauri";
import { emptyConfig, snapshotWithConfig } from "../support/snapshots";

afterEach(() => { cleanup(); window.history.replaceState(null, "", "/"); });

function setup(lastConnectedId: string | null = null, fail = false, router = false, loginRequired = false) {
  let servers: ServerProfile[] = [{ id: "local", name: "This device" }, { id: "remote", name: "Work", url: "https://waltr.example" }];
  const registry = {
    list: vi.fn(async () => ({ servers: [...servers], lastConnectedId })),
    save: vi.fn(async (url: string, name?: string) => {
      const server = { id: "added", name: name ?? url, url };
      servers.push(server);
      return server;
    }),
    remove: vi.fn(async (id: string) => { servers = servers.filter((server) => server.id !== id); }),
  };
  const adapters: ClientAdapter[] = [];
  const loaderFor = vi.fn((id: string) => async () => {
    const snapshot = snapshotWithConfig(emptyConfig(id));
    if (loginRequired) snapshot.access.status = "loginRequired";
    const adapter: ClientAdapter = {
      snapshot: () => snapshot,
      start: vi.fn(async () => { if (fail) throw new Error("private native failure"); }),
      subscribe: () => () => undefined,
      dispatch: vi.fn(async () => snapshot),
      dispose: vi.fn(),
    };
    adapters.push(adapter);
    return adapter;
  });
  function Content() {
    const app = useAppState();
    return <><input aria-label="Draft" /><ServerControls failed={Boolean(app.clientError())} /></>;
  }
  render(() => <ServerPicker registry={registry} loaderFor={loaderFor}>{(loader) => (
    <AppStateProvider loadClientAdapter={loader}>{router ? <AppRouter /> : <Content />}</AppStateProvider>
  )}</ServerPicker>);
  return { registry, loaderFor, adapters };
}

test("first launch chooses explicitly; switching disposes and fully remounts drafts at root", async () => {
  const { loaderFor, adapters, registry } = setup();
  await screen.findByRole("button", { name: "This device" });
  expect(loaderFor).not.toHaveBeenCalled();
  expect(screen.queryByRole("button", { name: "Remove This device" })).toBeNull();
  fireEvent.click(screen.getByRole("button", { name: "This device" }));
  await screen.findByLabelText("Draft");
  fireEvent.input(screen.getByLabelText("Draft"), { target: { value: "private draft" } });
  window.history.replaceState(null, "", "/sessions/private?secret=1#draft");
  fireEvent.click(screen.getByRole("button", { name: "Switch server" }));
  expect(adapters[0].dispose).toHaveBeenCalledOnce();
  fireEvent.click(await screen.findByRole("button", { name: "Work" }));
  await waitFor(() => expect(adapters).toHaveLength(2));
  expect((screen.getByLabelText("Draft") as HTMLInputElement).value).toBe("");
  expect(window.location.pathname + window.location.search + window.location.hash).toBe("/");
  expect(loaderFor.mock.calls).toEqual([["local"], ["remote"]]);
  expect(registry.remove).not.toHaveBeenCalled();
});

test("auto reconnects last successful server without writing registry selection", async () => {
  const { loaderFor, registry } = setup("remote");
  await screen.findByLabelText("Draft");
  expect(loaderFor).toHaveBeenCalledWith("remote");
  expect(registry.save).not.toHaveBeenCalled();
});

test("startup failure exposes retry and switch without deleting credentials", async () => {
  const { adapters, registry } = setup("remote", true, true);
  fireEvent.click(await screen.findByRole("button", { name: "Retry connection" }));
  await waitFor(() => expect(adapters).toHaveLength(2));
  expect(adapters[0].dispose).toHaveBeenCalledOnce();
  expect(screen.queryByText("private native failure")).toBeNull();
  fireEvent.click(await screen.findByRole("button", { name: "Switch server" }));
  await screen.findByRole("heading", { name: "Choose a server" });
  expect(registry.remove).not.toHaveBeenCalled();
});

test("switch is available in the authenticated sidebar and resets the real router", async () => {
  const { adapters } = setup("remote", false, true);
  const sidebar = await screen.findByLabelText("Primary navigation");
  expect(sidebar.textContent).toContain("Switch server");
  window.history.replaceState(null, "", "/sessions/other-server-session");
  fireEvent.click(sidebar.querySelector<HTMLButtonElement>("button.shell__logout")!);
  fireEvent.click(await screen.findByRole("button", { name: "This device" }));
  await waitFor(() => expect(adapters).toHaveLength(2));
  expect(window.location.pathname).toBe("/");
  expect(adapters[0].dispose).toHaveBeenCalledOnce();
});

test("switch remains accessible when the server requires sign-in", async () => {
  setup("remote", false, true, true);
  await screen.findByRole("heading", { name: "Sign in to continue" });
  fireEvent.click(screen.getByRole("button", { name: "Switch server" }));
  await screen.findByRole("heading", { name: "Choose a server" });
});

test("validates URL, adds an optional name, removes saved server and reports errors", async () => {
  const { registry } = setup();
  await screen.findByRole("button", { name: "Work" });
  fireEvent.input(screen.getByLabelText("Server URL"), { target: { value: "https://user:password@waltr.example" } });
  fireEvent.submit(screen.getByRole("button", { name: "Add server" }).closest("form")!);
  expect((await screen.findByRole("alert")).textContent).toContain("without credentials");
  expect(registry.save).not.toHaveBeenCalled();
  fireEvent.input(screen.getByLabelText("Server URL"), { target: { value: "https://other.example" } });
  fireEvent.input(screen.getByLabelText("Name (optional)"), { target: { value: "Personal" } });
  registry.save.mockRejectedValueOnce(new Error("private storage path"));
  fireEvent.submit(screen.getByRole("button", { name: "Add server" }).closest("form")!);
  expect((await screen.findByRole("alert")).textContent).toContain("Unable to save");
  fireEvent.submit(screen.getByRole("button", { name: "Add server" }).closest("form")!);
  await screen.findByRole("button", { name: "Personal" });
  expect(registry.save).toHaveBeenCalledWith("https://other.example/", "Personal");
  registry.remove.mockRejectedValueOnce(new Error("private path"));
  fireEvent.click(screen.getByRole("button", { name: "Remove Personal" }));
  expect((await screen.findByRole("alert")).textContent).toContain("Unable to remove");
  fireEvent.click(screen.getByRole("button", { name: "Remove Personal" }));
  await waitFor(() => expect(screen.queryByRole("button", { name: "Personal" })).toBeNull());
});

test("list failure stays recoverable and does not expose native failure details", async () => {
  const registry = {
    list: vi.fn().mockRejectedValueOnce(new Error("private path")).mockResolvedValue({ servers: [{ id: "local", name: "This device" }], lastConnectedId: null }),
    save: vi.fn(), remove: vi.fn(),
  };
  render(() => <ServerPicker registry={registry} loaderFor={vi.fn()}>{() => <div />}</ServerPicker>);
  expect((await screen.findByRole("alert")).textContent).toBe("Unable to load saved servers.");
  fireEvent.click(screen.getByRole("button", { name: "Retry loading servers" }));
  await screen.findByRole("button", { name: "This device" });
});
