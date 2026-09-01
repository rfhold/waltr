import { fireEvent, screen, waitFor } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { renderWithAppState } from "../support/render";
import { ShellRouteWithChildren } from "../support/routes";
import { snapshotWithConfig } from "../support/snapshots";

describe("shell routing", () => {
  test("marks nested config routes active in the shell", () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/config/provider/edit/claude",
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, inferenceDefaults: null }),
    });

    expect(screen.getByRole("link", { name: "Config" }).classList.contains("active")).toBe(true);
  });

  test("marks session routes active in the shell", () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/sessions/session-a",
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, inferenceDefaults: null }),
    });

    expect(screen.getAllByRole("link", { name: "Sessions" })[0]!.classList.contains("active")).toBe(true);
  });

  test("navigates to and marks the Routines route active", () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/routines",
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, inferenceDefaults: null }),
    });

    const link = screen.getByRole("link", { name: "Routines" });
    expect(link.classList.contains("active")).toBe(true);
    expect(screen.getByRole("heading", { name: "Routines" })).toBeTruthy();
  });

  test("renders one global status surface and keeps sign out in the navigation drawer", () => {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/",
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, inferenceDefaults: null }),
    });

    expect(screen.getAllByLabelText("Global system status")).toHaveLength(1);
    expect(screen.getAllByRole("status")).toHaveLength(1);
    expect(screen.getByLabelText("Global system status").textContent).toContain("Status watch");
    expect(screen.getByRole("button", { name: "Sign out" })).toBeTruthy();
  });

  test("opens the drawer, moves focus inside, and closes from its close button", async () => {
    renderShell();
    const trigger = screen.getByRole("button", { name: "Open navigation menu" });

    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(trigger.getAttribute("aria-controls")).toBe("primary-navigation-drawer");
    expect(document.querySelector(".shell__sidebar")?.classList.contains("shell__drawer--open")).toBe(false);

    fireEvent.click(trigger);

    expect(trigger.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByLabelText("Primary navigation").getAttribute("aria-modal")).toBe("true");
    expect(document.querySelector(".shell__app-bar")?.hasAttribute("inert")).toBe(true);
    expect(document.querySelector(".shell__content")?.hasAttribute("inert")).toBe(true);
    await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("button", { name: "Close navigation menu" })));

    fireEvent.click(screen.getByRole("button", { name: "Close navigation menu" }));
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    expect(document.querySelector(".shell__app-bar")?.hasAttribute("inert")).toBe(false);
    expect(document.querySelector(".shell__content")?.hasAttribute("inert")).toBe(false);
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });

  test("closes from Escape and the backdrop while restoring trigger focus", async () => {
    renderShell();
    const trigger = screen.getByRole("button", { name: "Open navigation menu" });

    fireEvent.click(trigger);
    fireEvent.keyDown(document, { key: "Escape" });
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    await waitFor(() => expect(document.activeElement).toBe(trigger));

    fireEvent.click(trigger);
    fireEvent.click(screen.getByRole("button", { name: "Close navigation drawer" }));
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });

  test("traps focus in the open drawer", async () => {
    renderShell();
    fireEvent.click(screen.getByRole("button", { name: "Open navigation menu" }));
    const close = screen.getByRole("button", { name: "Close navigation menu" });
    const brand = screen.getByRole("link", { name: "Waltr" });
    const signOut = screen.getByRole("button", { name: "Sign out" });
    await waitFor(() => expect(document.activeElement).toBe(close));

    brand.focus();
    fireEvent.keyDown(document, { key: "Tab", shiftKey: true });
    expect(document.activeElement).toBe(signOut);

    signOut.focus();
    fireEvent.keyDown(document, { key: "Tab" });
    expect(document.activeElement).toBe(brand);
  });

  test("closes after route activation and preserves active-link semantics", async () => {
    renderShell();
    const trigger = screen.getByRole("button", { name: "Open navigation menu" });
    fireEvent.click(trigger);

    fireEvent.click(screen.getByRole("link", { name: "Sessions" }));

    await waitFor(() => expect(screen.getByRole("link", { name: "Sessions" }).getAttribute("aria-current")).toBe("page"));
    expect(screen.getByRole("link", { name: "Sessions" }).classList.contains("active")).toBe(true);
    expect(trigger.getAttribute("aria-expanded")).toBe("false");
    await waitFor(() => expect(document.activeElement).toBe(trigger));
  });

  test("leaves modal mode and focuses active navigation after resizing to desktop", async () => {
    const originalInnerWidth = window.innerWidth;
    Object.defineProperty(window, "innerWidth", { configurable: true, value: 390 });
    try {
      renderShell();
      const trigger = screen.getByRole("button", { name: "Open navigation menu" });
      fireEvent.click(trigger);
      await waitFor(() => expect(screen.getByLabelText("Primary navigation").getAttribute("aria-modal")).toBe("true"));

      Object.defineProperty(window, "innerWidth", { configurable: true, value: 701 });
      fireEvent(window, new Event("resize"));

      await waitFor(() => expect(trigger.getAttribute("aria-expanded")).toBe("false"));
      expect(screen.getByLabelText("Primary navigation").getAttribute("aria-modal")).toBeNull();
      expect(document.querySelector(".shell__content")?.hasAttribute("inert")).toBe(false);
      await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("link", { name: "Home" })));
    } finally {
      Object.defineProperty(window, "innerWidth", { configurable: true, value: originalInnerWidth });
    }
  });

  function renderShell() {
    renderWithAppState(() => <ShellRouteWithChildren />, {
      routePath: "/",
      snapshot: snapshotWithConfig({ etag: "v1", providers: {}, mcpServers: {}, inferenceDefaults: null }),
    });
  }
});
