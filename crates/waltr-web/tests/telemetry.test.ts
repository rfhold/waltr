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
  sanitizeTransportItem,
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

  it("normalizes identifiers and strips URL details", () => {
    expect(normalizeApplicationView("/sessions/private-session?token=secret#turn-4")).toBe("/sessions/:session");
    expect(normalizeApplicationView("/config/provider/edit/private/models/edit/secret-model")).toBe("/config/provider/edit/:provider/models/edit/:model");
    expect(normalizeApplicationView("/unexpected/user-content")).toBe("/unknown");

    const item = {
      type: "exception" as const,
      payload: { timestamp: "now", type: "Error", value: "private command text", stacktrace: { frames: [] } },
      meta: {
        user: { id: "user-id" },
        session: { id: "session-id" },
        page: { id: "raw", url: "https://waltr.example/sessions/private?token=secret#fragment" },
        view: { name: "/routines/private-id" },
      },
    };
    expect(sanitizeTransportItem(item as never)).toEqual(expect.objectContaining({
      payload: { timestamp: "now", type: "unhandled_error", value: "Unhandled application error" },
      meta: expect.objectContaining({
        user: undefined,
        session: undefined,
        page: { id: "/sessions/:session", url: "/sessions/:session" },
        view: { name: "/routines/:invocation" },
      }),
    }));

    const trace = {
      type: "trace",
      payload: {
        resourceSpans: [{ scopeSpans: [{ spans: [{
          name: "GET https://waltr.example/sessions/private?token=secret",
          kind: 3,
          attributes: [
            { key: "url.full", value: { stringValue: "https://waltr.example/sessions/private?token=secret" } },
            { key: "http.request.header.authorization", value: { stringValue: "Bearer secret" } },
            { key: "http.request.body", value: { stringValue: "private command" } },
            { key: "http.request.method", value: { stringValue: "GET" } },
          ],
        }] }] }],
      },
      meta: {},
    };
    sanitizeTransportItem(trace as never);
    expect(trace.payload.resourceSpans[0].scopeSpans[0].spans[0]).toEqual(expect.objectContaining({
      name: "HTTP GET",
      attributes: [{ key: "http.request.method", value: { stringValue: "GET" } }],
    }));
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

  it("retains only bounded Web Vitals measurement fields", () => {
    const credential = "Bearer credential-hostile-marker";
    const callback = "dev.waltr.app:/oauth2redirect?code=callback-hostile-marker";
    const userContent = "private-user-content-hostile-marker";
    const item = {
      type: "measurement",
      payload: {
        type: "web-vitals",
        timestamp: "2026-09-01T00:00:00.000Z",
        values: {
          lcp: 1250,
          delta: 20,
          resource_load_duration: 300,
          generated_id: 42,
          hostile: userContent,
          infinite: Number.POSITIVE_INFINITY,
        },
        context: {
          rating: "good",
          navigation_type: "navigate",
          load_state: "complete",
          interaction_type: "pointer",
          id: credential,
          navigation_entry_id: callback,
          interaction_target: userContent,
          element: `<main data-secret="${credential}">`,
          resource_url: `https://example.invalid/private?callback=${callback}`,
          arbitrary: userContent,
        },
        trace: { trace_id: credential, span_id: callback },
        action: { name: userContent, id: credential },
      },
      meta: {},
    };

    const sanitized = sanitizeTransportItem(item as never);
    expect(sanitized).toEqual({
      type: "measurement",
      payload: {
        type: "web-vitals",
        timestamp: "2026-09-01T00:00:00.000Z",
        values: { lcp: 1250, delta: 20, resource_load_duration: 300 },
        context: {
          rating: "good",
          navigation_type: "navigate",
          load_state: "complete",
          interaction_type: "pointer",
        },
      },
      meta: { user: undefined, session: undefined },
    });
    const serialized = JSON.stringify(sanitized);
    expect(serialized).not.toContain(credential);
    expect(serialized).not.toContain(callback);
    expect(serialized).not.toContain(userContent);

    expect(sanitizeTransportItem({
      type: "measurement",
      payload: { type: "custom", values: { lcp: 1 }, timestamp: "2026-09-01T00:00:00.000Z" },
      meta: {},
    } as never)).toBeNull();
    expect(sanitizeTransportItem({
      type: "measurement",
      payload: { type: "web-vitals", values: { custom: 1 }, timestamp: "2026-09-01T00:00:00.000Z" },
      meta: {},
    } as never)).toBeNull();
    expect(sanitizeTransportItem({
      type: "measurement",
      payload: { type: "web-vitals", values: { lcp: 1 }, timestamp: "2026-09-01T01:00:00+01:00" },
      meta: {},
    } as never)?.payload).toMatchObject({ timestamp: "2026-09-01T00:00:00.000Z" });
  });

  it("initializes and reports each controlled failure once", () => {
    const pushEvent = vi.fn();
    const initialize = vi.fn((_config: BrowserConfig) => ({ api: { pushEvent, setPage: vi.fn(), setView: vi.fn() } }));
    initializeTelemetry({ enabled: true, platform: "tauri", version: "0.1.0", origin: "http://tauri.localhost", initialize: initialize as never });
    initializeTelemetry({ enabled: true, platform: "browser", version: "other", origin: "https://waltr.example", initialize: initialize as never });
    reportApplicationError("dispatch", "failed");
    reportApplicationError("dispatch", "failed");
    expect(initialize).toHaveBeenCalledOnce();
    expect(initialize.mock.calls[0][0].app.name).toBe("waltr-tauri");
    expect(pushEvent).toHaveBeenCalledOnce();
    expect(pushEvent).toHaveBeenCalledWith("waltr.application_error", { category: "dispatch", outcome: "failed" });
  });

  it("limits trace propagation to the hosted origin and disables it for Tauri", () => {
    const browser = createFaroConfig({ platform: "browser", version: "0.1.0", origin: "https://waltr.example" });
    const tauri = createFaroConfig({ platform: "tauri", version: "0.1.0", origin: "http://tauri.localhost" });
    const browserTracing = browser.instrumentations?.at(-1) as { options?: { instrumentationOptions?: { propagateTraceHeaderCorsUrls?: RegExp[] } } };
    const tauriTracing = tauri.instrumentations?.at(-1) as { options?: { instrumentationOptions?: { propagateTraceHeaderCorsUrls?: RegExp[] } } };
    expect(browserTracing.options?.instrumentationOptions?.propagateTraceHeaderCorsUrls?.[0].test("https://waltr.example/waltr.Client/Watch")).toBe(true);
    expect(browserTracing.options?.instrumentationOptions?.propagateTraceHeaderCorsUrls?.[0].test("https://remote.example/api")).toBe(false);
    expect(tauriTracing.options?.instrumentationOptions?.propagateTraceHeaderCorsUrls).toEqual([]);
    expect(browser.instrumentations?.map(({ name }) => name)).not.toEqual(expect.arrayContaining([
      "@grafana/faro-web-sdk:instrumentation-console",
      "@grafana/faro-web-sdk:instrumentation-navigation",
      "@grafana/faro-web-sdk:instrumentation-session",
      "@grafana/faro-web-sdk:instrumentation-user-action",
    ]));
  });
});
