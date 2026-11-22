import { screen, waitFor } from "@solidjs/testing-library";
import { afterEach, expect, test } from "vitest";

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
