import {
  getWebInstrumentations,
  initializeFaro,
  type BrowserConfig,
  type Faro,
  type TransportItem,
} from "@grafana/faro-web-sdk";
import { TracingInstrumentation } from "@grafana/faro-web-tracing";

import { telemetryAppName, type BuildPlatform } from "./platform/client";

const COLLECTOR_URL = "https://faro.holdenitdown.net/collect";
const SAFE_TRACE_ATTRIBUTE_KEYS = new Set([
  "browser.mobile",
  "browser.platform",
  "deployment.environment",
  "deployment.environment.name",
  "error.type",
  "http.request.method",
  "http.response.status_code",
  "http.status_code",
  "http.status_text",
  "http.method",
  "network.protocol.version",
  "server.address",
  "service.name",
  "service.namespace",
  "service.version",
  "telemetry.distro.name",
  "telemetry.distro.version",
]);
const WEB_VITAL_NAMES = new Set(["cls", "fcp", "inp", "lcp", "ttfb"]);
const WEB_VITAL_VALUE_NAMES = new Set([
  ...WEB_VITAL_NAMES,
  "cache_duration",
  "connection_duration",
  "delta",
  "dns_duration",
  "element_render_delay",
  "first_byte_to_fcp",
  "input_delay",
  "interaction_time",
  "largest_shift_time",
  "largest_shift_value",
  "longest_script_intersecting_duration",
  "next_paint_time",
  "presentation_delay",
  "processing_duration",
  "request_duration",
  "resource_load_delay",
  "resource_load_duration",
  "time_to_first_byte",
  "total_paint_duration",
  "total_script_duration",
  "total_style_and_layout_duration",
  "total_unattributed_duration",
  "waiting_duration",
]);
const WEB_VITAL_CONTEXT_ENUMS: Record<string, ReadonlySet<string>> = {
  interaction_type: new Set(["keyboard", "pointer"]),
  load_state: new Set(["complete", "dom-content-loaded", "dom-interactive", "loading"]),
  longest_script_invoker_type: new Set([
    "classic-script",
    "event-listener",
    "module-script",
    "reject-promise",
    "resolve-promise",
    "user-callback",
  ]),
  longest_script_subpart: new Set(["input-delay", "presentation-delay", "processing-duration"]),
  navigation_type: new Set([
    "back-forward",
    "back-forward-cache",
    "navigate",
    "prerender",
    "reload",
    "restore",
    "soft-navigation",
  ]),
  rating: new Set(["good", "needs-improvement", "poor"]),
};
const MAX_WEB_VITAL_VALUE = 86_400_000;

type TelemetryApi = Pick<Faro["api"], "pushEvent" | "setPage" | "setView">;
type FaroInitializer = (config: BrowserConfig) => Faro;

let initializationAttempted = false;
let telemetryApi: TelemetryApi | undefined;
let lastView: string | undefined;
const reportedErrors = new Set<string>();

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
  const ownOrigin = escapeRegExp(options.origin);
  const ignoreRemoteRequests = new RegExp(`^(?!${ownOrigin}(?:/|$))https?:`, "i");
  const hostedOrigin = new RegExp(`^${ownOrigin}(?:/|$)`, "i");
  const initialView = normalizeApplicationView(options.pathname ?? "/");

  return {
    url: COLLECTOR_URL,
    app: {
      name: telemetryAppName(options.platform),
      version: boundedBuildValue(options.version),
      environment: "production",
    },
    beforeSend: sanitizeTransportItem,
    instrumentations: [
      ...getWebInstrumentations({
        captureConsole: false,
        enableContentSecurityPolicyInstrumentation: false,
      }).filter((instrumentation) => ![
        "@grafana/faro-web-sdk:instrumentation-navigation",
        "@grafana/faro-web-sdk:instrumentation-session",
        "@grafana/faro-web-sdk:instrumentation-user-action",
      ].includes(instrumentation.name)),
      new TracingInstrumentation({
        omitTraceContextForUnsampledSessions: true,
        instrumentationOptions: {
          propagateTraceHeaderCorsUrls: options.platform === "browser" ? [hostedOrigin] : [],
        },
      }),
    ],
    ignoreUrls: options.platform === "browser" ? [ignoreRemoteRequests] : [/.*/],
    pageTracking: {
      page: { id: initialView, url: initialView },
      generatePageId: (location) => normalizeApplicationView(location.pathname),
    },
    sessionTracking: { enabled: false },
    trackGeolocation: false,
    trackResources: false,
    webVitalsInstrumentation: { trackAttributionSources: false },
    experimental: { trackNavigation: false },
    preventGlobalExposure: true,
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
  const dedupeKey = `${category}:${safeOutcome}`;
  if (reportedErrors.has(dedupeKey)) return;
  reportedErrors.add(dedupeKey);
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

export function sanitizeTransportItem(item: TransportItem): TransportItem | null {
  item.meta.user = undefined;
  item.meta.session = undefined;
  if (item.meta.page) {
    const view = normalizeApplicationView(item.meta.page.url ?? item.meta.page.id ?? "/unknown");
    item.meta.page = { id: view, url: view };
  }
  if (item.meta.view) item.meta.view = { name: normalizeApplicationView(item.meta.view.name) };

  if (item.type === "log") return null;
  if (item.type === "exception") {
    item.payload = {
      timestamp: "timestamp" in item.payload ? item.payload.timestamp : new Date(0).toISOString(),
      type: "unhandled_error",
      value: "Unhandled application error",
    };
  } else if (item.type === "event") {
    sanitizeEventPayload(item.payload);
  } else if (item.type === "measurement") {
    const payload = sanitizeMeasurementPayload(item.payload);
    if (!payload) return null;
    item.payload = payload;
  } else if (item.type === "trace") {
    sanitizeTraceValue(item.payload);
  }
  return item;
}

function sanitizeMeasurementPayload(payload: unknown) {
  if (!isRecord(payload) || payload.type !== "web-vitals" || !isRecord(payload.values)) return undefined;

  const values: Record<string, number> = {};
  for (const [name, value] of Object.entries(payload.values)) {
    if (WEB_VITAL_VALUE_NAMES.has(name)
      && typeof value === "number"
      && Number.isFinite(value)
      && value >= 0
      && value <= MAX_WEB_VITAL_VALUE) {
      values[name] = value;
    }
  }
  if (Object.keys(values).filter((name) => WEB_VITAL_NAMES.has(name)).length !== 1) return undefined;

  const context: Record<string, string> = {};
  if (isRecord(payload.context)) {
    for (const [name, value] of Object.entries(payload.context)) {
      if (typeof value === "string" && WEB_VITAL_CONTEXT_ENUMS[name]?.has(value)) context[name] = value;
    }
  }
  const parsedTimestamp = typeof payload.timestamp === "string" && payload.timestamp.length <= 32
    ? Date.parse(payload.timestamp)
    : Number.NaN;
  const timestamp = Number.isFinite(parsedTimestamp)
    ? new Date(parsedTimestamp).toISOString()
    : new Date(0).toISOString();

  return {
    type: "web-vitals",
    values,
    timestamp,
    ...(Object.keys(context).length > 0 ? { context } : {}),
  };
}

export function resetTelemetryForTests(): void {
  initializationAttempted = false;
  telemetryApi = undefined;
  lastView = undefined;
  reportedErrors.clear();
}

function sanitizeEventPayload(payload: unknown): void {
  if (!isRecord(payload)) return;
  const name = typeof payload.name === "string" ? payload.name : "unknown";
  if (name === "waltr.application_error") {
    const attributes = isRecord(payload.attributes) ? payload.attributes : {};
    payload.attributes = {
      category: ["startup", "dispatch", "recovery"].includes(String(attributes.category)) ? String(attributes.category) : "unknown",
      outcome: controlledOutcome(String(attributes.outcome ?? "unknown")),
    };
    return;
  }
  if (name === "view_changed") {
    const attributes = isRecord(payload.attributes) ? payload.attributes : {};
    payload.attributes = {
      fromView: normalizeApplicationView(String(attributes.fromView ?? "/unknown")),
      toView: normalizeApplicationView(String(attributes.toView ?? "/unknown")),
    };
    return;
  }
  if (name.startsWith("faro.performance.")) {
    const attributes = isRecord(payload.attributes) ? payload.attributes : {};
    payload.attributes = Object.fromEntries(Object.entries(attributes).filter(([, value]) => /^-?\d+(?:\.\d+)?$/.test(String(value))));
    return;
  }
  payload.attributes = {};
}

function sanitizeTraceValue(value: unknown, parentKey = ""): void {
  if (Array.isArray(value)) {
    if (parentKey === "attributes") {
      for (let index = value.length - 1; index >= 0; index -= 1) {
        const attribute = value[index];
        if (!isRecord(attribute) || typeof attribute.key !== "string" || !SAFE_TRACE_ATTRIBUTE_KEYS.has(attribute.key)) value.splice(index, 1);
      }
    }
    for (const child of value) sanitizeTraceValue(child, parentKey);
    return;
  }
  if (!isRecord(value)) return;
  for (const [key, child] of Object.entries(value)) {
    if (key === "name" && typeof child === "string" && ("kind" in value || "spanId" in value)) {
      value[key] = sanitizeSpanName(child);
    } else {
      sanitizeTraceValue(child, key);
    }
  }
}

function sanitizeSpanName(name: string): string {
  if (/documentLoad/i.test(name)) return "documentLoad";
  const method = name.match(/\b(GET|POST|PUT|PATCH|DELETE|HEAD|OPTIONS)\b/i)?.[1];
  return method ? `HTTP ${method.toUpperCase()}` : "web-operation";
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

function escapeRegExp(value: string): string {
  return value.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}
