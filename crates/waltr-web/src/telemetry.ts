import {
  getWebInstrumentations,
  initializeFaro,
  type BrowserConfig,
  type Faro,
} from "@grafana/faro-web-sdk";
import { TracingInstrumentation } from "@grafana/faro-web-tracing";

import { telemetryAppName, type BuildPlatform } from "./platform/client";

const COLLECTOR_URL = "https://faro.holdenitdown.net/collect";

type TelemetryApi = Pick<Faro["api"], "pushEvent" | "setPage" | "setView">;
type FaroInitializer = (config: BrowserConfig) => Faro;

let initializationAttempted = false;
let telemetryApi: TelemetryApi | undefined;
let lastView: string | undefined;

export function initializeTelemetry(options: {
  enabled?: boolean;
  platform?: BuildPlatform;
  version?: string;
  origin?: string;
  initialize?: FaroInitializer;
} = {}): void {
  if (initializationAttempted || !(options.enabled ?? __WALTR_TELEMETRY_ENABLED__)) return;
  initializationAttempted = true;

  const platform = options.platform ?? __WALTR_BUILD_PLATFORM__;
  const origin = options.origin ?? window.location.origin;

  try {
    const faro = (options.initialize ?? initializeFaro)(createFaroConfig({
      platform,
      version: boundedBuildValue(options.version ?? __WALTR_BUILD_VERSION__),
      origin,
      pathname: window.location.pathname,
    }));
    telemetryApi = faro.api;
  } catch {
    telemetryApi = undefined;
  }
}

export function createFaroConfig(options: {
  platform: BuildPlatform;
  version: string;
  origin: string;
  pathname?: string;
}): BrowserConfig {
  const initialView = normalizeApplicationView(options.pathname ?? "/");

  return {
    url: COLLECTOR_URL,
    app: {
      name: telemetryAppName(options.platform),
      version: boundedBuildValue(options.version),
      environment: "production",
    },
    instrumentations: [
      ...getWebInstrumentations(),
      new TracingInstrumentation(),
    ],
    experimental: { trackNavigation: true },
    // Native requests carry adapter-private loopback coordinates and bearer credentials.
    ignoreUrls: options.platform === "tauri" ? [/.*/] : undefined,
    pageTracking: {
      page: { id: initialView, url: initialView },
      generatePageId: (location) => normalizeApplicationView(location.pathname),
    },
    view: { name: initialView },
  };
}

export function reportApplicationView(pathname: string): void {
  const view = normalizeApplicationView(pathname);
  if (!telemetryApi || view === lastView) return;
  lastView = view;
  try {
    telemetryApi.setPage({ id: view, url: view });
    telemetryApi.setView({ name: view });
  } catch {
    // Telemetry must not participate in routing or rendering.
  }
}

export function reportApplicationError(category: "startup" | "dispatch" | "recovery", outcome: string): void {
  if (!telemetryApi) return;
  const safeOutcome = controlledOutcome(outcome);
  try {
    telemetryApi.pushEvent("waltr.application_error", { category, outcome: safeOutcome });
  } catch {
    // Reporting failures are deliberately ignored by product logic.
  }
}

export function normalizeApplicationView(pathname: string): string {
  const path = applicationPathname(pathname).split(/[?#]/, 1)[0].replace(/\/{2,}/g, "/");
  if (path === "/" || path === "/sessions" || path === "/sessions/data" || path === "/routines") return path;
  if (/^\/sessions\/[^/]+\/?$/.test(path)) return "/sessions/:session";
  if (/^\/routines\/[^/]+\/?$/.test(path)) return "/routines/:invocation";
  if (path === "/config" || path === "/config/provider" || path === "/config/provider/create") return path;
  if (/^\/config\/provider\/edit\/[^/]+\/?$/.test(path)) return "/config/provider/edit/:provider";
  if (path === "/config/provider/create/models/create") return path;
  if (/^\/config\/provider\/create\/models\/edit\/[^/]+\/?$/.test(path)) return "/config/provider/create/models/edit/:model";
  if (/^\/config\/provider\/edit\/[^/]+\/models\/create\/?$/.test(path)) return "/config/provider/edit/:provider/models/create";
  if (/^\/config\/provider\/edit\/[^/]+\/models\/edit\/[^/]+\/?$/.test(path)) return "/config/provider/edit/:provider/models/edit/:model";
  if (path === "/config/mcp" || path === "/config/mcp/create" || path === "/config/defaults") return path;
  if (/^\/config\/mcp\/edit\/[^/]+\/?$/.test(path)) return "/config/mcp/edit/:server";
  return "/unknown";
}

export function resetTelemetryForTests(): void {
  initializationAttempted = false;
  telemetryApi = undefined;
  lastView = undefined;
}

function boundedBuildValue(value: string): string {
  const bounded = value.trim().slice(0, 64);
  return /^[0-9A-Za-z._+-]+$/.test(bounded) ? bounded : "unknown";
}

function applicationPathname(value: string): string {
  try {
    return new URL(value, "https://waltr.invalid").pathname;
  } catch {
    return "/unknown";
  }
}

function controlledOutcome(value: string): string {
  const bounded = value.slice(0, 32);
  return /^[a-z][a-z0-9_]*$/.test(bounded) ? bounded : "unknown";
}
