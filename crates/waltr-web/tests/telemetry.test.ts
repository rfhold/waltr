import { beforeEach, describe, expect, it, vi } from "vitest";
import type { BrowserConfig } from "@grafana/faro-web-sdk";

import { telemetryEnabledForBuild } from "../src/build";
import { telemetryAppName } from "../src/platform/client";
import {
  createFaroConfig,
  initializeTelemetry,
  normalizeApplicationView,
  reportApplicationError,
  resetTelemetryForTests,
} from "../src/telemetry";

describe("telemetry", () => {
  beforeEach(() => resetTelemetryForTests());

  it("uses stable build-time platform identities", () => {
    expect(telemetryAppName("browser")).toBe("waltr-spa");
    expect(telemetryAppName("tauri")).toBe("waltr-tauri");
  });

  it("enables telemetry only for explicit release build modes", () => {
    expect([
      ["build", "production", telemetryEnabledForBuild("build", "production")],
      ["build", "tauri", telemetryEnabledForBuild("build", "tauri")],
      ["build", "development", telemetryEnabledForBuild("build", "development")],
      ["build", "test", telemetryEnabledForBuild("build", "test")],
      ["build", "tauri-dev", telemetryEnabledForBuild("build", "tauri-dev")],
      ["build", "custom-preview", telemetryEnabledForBuild("build", "custom-preview")],
      ["serve", "production", telemetryEnabledForBuild("serve", "production")],
      ["serve", "tauri", telemetryEnabledForBuild("serve", "tauri")],
    ]).toEqual([
      ["build", "production", true],
      ["build", "tauri", true],
      ["build", "development", false],
      ["build", "test", false],
      ["build", "tauri-dev", false],
      ["build", "custom-preview", false],
      ["serve", "production", false],
      ["serve", "tauri", false],
    ]);
  });

  it("normalizes application views", () => {
    expect(normalizeApplicationView("/sessions/private-session?token=secret#turn-4")).toBe("/sessions/:session");
    expect(normalizeApplicationView("/config/provider/edit/private/models/edit/secret-model")).toBe("/config/provider/edit/:provider/models/edit/:model");
    expect(normalizeApplicationView("/unexpected/user-content")).toBe("/unknown");
  });

  it("does not initialize or send in tests when production is disabled", () => {
    const initialize = vi.fn();
    const fetchSpy = vi.spyOn(globalThis, "fetch");
    initializeTelemetry({ enabled: false, initialize });
    reportApplicationError("startup", "load_failed");
    expect(initialize).not.toHaveBeenCalled();
    expect(fetchSpy).not.toHaveBeenCalled();
    fetchSpy.mockRestore();
  });

  it("initializes once and reports repeated controlled failures", () => {
    const pushEvent = vi.fn();
    const initialize = vi.fn((_config: BrowserConfig) => ({ api: { pushEvent, setPage: vi.fn(), setView: vi.fn() } }));
    initializeTelemetry({ enabled: true, platform: "tauri", version: "0.1.0", origin: "http://tauri.localhost", initialize: initialize as never });
    initializeTelemetry({ enabled: true, platform: "browser", version: "other", origin: "https://waltr.example", initialize: initialize as never });
    reportApplicationError("dispatch", "failed");
    reportApplicationError("dispatch", "failed");
    expect(initialize).toHaveBeenCalledOnce();
    expect(initialize.mock.calls[0][0].app.name).toBe("waltr-tauri");
    expect(pushEvent).toHaveBeenCalledTimes(2);
    expect(pushEvent).toHaveBeenNthCalledWith(1, "waltr.application_error", { category: "dispatch", outcome: "failed" });
    expect(pushEvent).toHaveBeenNthCalledWith(2, "waltr.application_error", { category: "dispatch", outcome: "failed" });
  });

  it("uses standard Faro instrumentation while excluding native requests", () => {
    const browser = createFaroConfig({ platform: "browser", version: "0.1.0", origin: "https://waltr.example" });
    const tauri = createFaroConfig({ platform: "tauri", version: "0.1.0", origin: "http://tauri.localhost" });
    expect(browser.beforeSend).toBeUndefined();
    expect(browser.ignoreUrls).toBeUndefined();
    expect(browser.experimental?.trackNavigation).toBe(true);
    expect(tauri.ignoreUrls).toHaveLength(1);
    expect(tauri.experimental?.trackNavigation).toBe(true);
    expect((tauri.ignoreUrls?.[0] as RegExp).test("http://127.0.0.1:43123/waltr.Client/Watch")).toBe(true);
    expect(browser.instrumentations?.map(({ name }) => name)).toEqual(expect.arrayContaining([
      "@grafana/faro-web-sdk:instrumentation-console",
      "@grafana/faro-web-sdk:instrumentation-navigation",
      "@grafana/faro-web-sdk:instrumentation-session",
      "@grafana/faro-web-sdk:instrumentation-user-action",
    ]));
  });
});
