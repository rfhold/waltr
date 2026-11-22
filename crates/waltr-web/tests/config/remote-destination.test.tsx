import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { afterEach, expect, test, vi } from "vitest";

import { AppRouter } from "../../src/routes";
import { renderWithAppState } from "../support/render";
import { emptyConfig, snapshotWithConfig } from "../support/snapshots";

afterEach(() => window.history.replaceState(null, "", "/"));

const id = "0123456789abcdef0123456789abcdef";
const base = `/remote/home~${id}`;

test("remote config and session links stay on the named destination", async () => {
  window.history.replaceState(null, "", `${base}/config/provider`);
  renderWithAppState(() => <AppRouter base={base} />, {
    snapshot: snapshotWithConfig(emptyConfig("test")),
  });

  await waitFor(() => expect(screen.getByRole("link", { name: "Sessions" }).getAttribute("href"))
    .toBe(`${base}/sessions`));
  expect(screen.getByRole("link", { name: "Routines" }).getAttribute("href")).toBe(`${base}/routines`);
  expect(screen.queryByRole("tab", { name: "Remotes" })).toBeNull();
});

test("opening a Remote leaves the entry router and carries the current authorization", async () => {
  window.history.replaceState(null, "", "/config/remotes");
  const snapshot = snapshotWithConfig(emptyConfig("test"));
  snapshot.config.remotes.home = { endpoint: "https://waltr.example", machineId: null };
  snapshot.auth.statuses["remote:home"] = {
    target: { kind: "remote", key: "home" }, kind: "oauthConfigured",
    supportedMethods: ["oauthDevice"], expiresAtMs: 0n, message: "",
    registrationMethod: "", canRefresh: false, metadata: { remoteRouteId: id },
  };
  renderWithAppState(() => <AppRouter />, { snapshot });

  const link = await screen.findByRole("link", { name: "Open home" });
  expect(link.getAttribute("href")).toBe(`${base}/`);
  expect(link.getAttribute("rel")).toBe("external");
});

test("the rail uses the entry directory from a Remote view", async () => {
  const read = vi.fn(async () => [
    { name: "home", routeId: id },
    { name: "office", routeId: "a".repeat(32) },
    { name: "pending", routeId: null },
  ]);
  window.history.replaceState(null, "", `${base}/config/provider`);
  renderWithAppState(() => <AppRouter base={base} />, {
    routePath: `${base}/config/provider`,
    snapshot: snapshotWithConfig(emptyConfig("test")),
    entryDestinations: read,
  });
  const office = await screen.findByRole("link", { name: "office" });
  expect(read).toHaveBeenCalledOnce();
  expect(office.getAttribute("rel")).toBe("external");
  expect(office.getAttribute("href")).toBe(`/remote/office~${"a".repeat(32)}/`);
  expect(screen.getByRole("link", { name: "home" }).getAttribute("aria-current")).toBe("true");
  expect(screen.getByRole("link", { name: "Manage pending" }).getAttribute("href")).toBe("/config/remotes");
  expect(screen.getByRole("link", { name: "Current server" }).getAttribute("href")).toBe("/");
});

test("stalled Remote can return to the entry even without a client", () => {
  window.history.replaceState(null, "", `${base}/sessions/old`);
  renderWithAppState(() => <AppRouter base={base} />, {
    routePath: `${base}/sessions/old`,
    clientLoaded: false,
  });
  const exit = screen.getByRole("link", { name: "Return to current server" });
  expect(screen.getByRole("main", { name: "Waltr splash screen" }).contains(exit)).toBe(true);
  expect(exit.getAttribute("href")).toBe("/");
  expect(exit.getAttribute("rel")).toBe("external");
});

test("entry directory failure leaves Local available and offers retry", async () => {
  const read = vi.fn().mockRejectedValueOnce(new Error("offline")).mockResolvedValueOnce([{ name: "home", routeId: id }]);
  renderWithAppState(() => <AppRouter />, {
    routePath: "/config/provider",
    snapshot: snapshotWithConfig(emptyConfig("test")),
    entryDestinations: read,
  });
  expect(screen.getByRole("link", { name: "Current server" }).getAttribute("aria-current")).toBe("true");
  fireEvent.click(await screen.findByRole("button", { name: "Retry server list" }));
  expect((await screen.findByRole("link", { name: "home" })).getAttribute("href")).toBe(`${base}/`);
});
