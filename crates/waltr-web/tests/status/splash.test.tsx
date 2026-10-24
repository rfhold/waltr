import { screen } from "@solidjs/testing-library";
import { describe, expect, test } from "vitest";

import { renderWithAppState } from "../support/render";
import { SplashRouteWithRouter } from "../support/routes";
import { snapshotWithConfig } from "../support/snapshots";

describe("splash status", () => {
  test("shows the bounded native discovery explanation", () => {
    renderWithAppState(() => <SplashRouteWithRouter />, {
      clientError: new Error("Native access discovery failed. Retry connection."),
      snapshot: undefined,
    });
    expect(screen.getByText("Unable to discover native sign-in configuration. Retry connection or switch server.")).toBeTruthy();
  });

  test("does not expose arbitrary startup failure details", () => {
    renderWithAppState(() => <SplashRouteWithRouter />, {
      clientError: new Error("SECRET https://secret.invalid/token"),
      snapshot: undefined,
    });
    expect(screen.getByText("Unable to connect to Waltr.")).toBeTruthy();
    expect(screen.queryByText(/SECRET/)).toBeNull();
  });
  test("renders the Waltr splash logo before the app is ready", () => {
    renderWithAppState(() => <SplashRouteWithRouter />, {
      routePath: "/",
      snapshot: snapshotWithConfig({ etag: "", providers: {}, mcpServers: {}, inferenceDefaults: null }, { ready: false }),
    });

    const logo = screen.getByLabelText("Waltr logo");

    expect(logo.textContent).toContain("W     W    A");
    expect(logo.textContent).toContain("LLLLLLL   T");
    expect(screen.getByText("Connecting to Waltr...")).toBeTruthy();
  });

  test("keeps startup presentation generic when no client snapshot is available", () => {
    renderWithAppState(() => <SplashRouteWithRouter />, {
      clientLoaded: false,
      snapshot: undefined,
    });

    expect(screen.getByText("Preparing client...")).toBeTruthy();
    expect(screen.queryByText(/embedded|local Waltr|server endpoint/i)).toBeNull();
  });
});
