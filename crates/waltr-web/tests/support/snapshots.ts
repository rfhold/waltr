import type {
  ClientSnapshot,
  ConfigView,
  McpServerView,
  ModelView,
  OperationSnapshot,
  ProviderView,
  SessionSnapshot,
  SessionTimelineView,
  SessionView,
} from "../../src/generated/waltr-client";

export function snapshotWithConfig(
  config: ConfigView,
  options: { ready?: boolean } = {},
): ClientSnapshot {
  return {
    operations: { current: {} },
    config: {
      current: config,
      load: operationSnapshot("idle"),
      patch: operationSnapshot("idle"),
      watch: operationSnapshot("received"),
    },
    status: {
      ready: options.ready ?? true,
      watch: operationSnapshot("ready"),
    },
    sessions: emptySessionSnapshot(),
  };
}

export function emptySessionSnapshot(): SessionSnapshot {
  return {
    current: [],
    create: operationSnapshot("idle"),
    list: operationSnapshot("idle"),
    delete: operationSnapshot("idle"),
    runtimeConfigUpdate: operationSnapshot("idle"),
    submitUserMessage: operationSnapshot("idle"),
    respondExternalRequest: operationSnapshot("idle"),
    interrupt: operationSnapshot("idle"),
    watch: operationSnapshot("idle"),
    timelines: {},
  };
}

export function operationSnapshot(status: string): OperationSnapshot {
  return { status, error: null, key: null };
}

export function readySessionTimeline(): SessionTimelineView {
  return {
    events: [],
    load: operationSnapshot("ready"),
    watch: operationSnapshot("opened"),
  };
}

export function emptyConfig(etag = "v1"): ConfigView {
  return { etag, providers: {}, mcpServers: {}, runtimeDefaults: null };
}

export function snapshotWithSessions(sessions: SessionView[]): ClientSnapshot {
  const snapshot = snapshotWithConfig(emptyConfig("v1"));
  snapshot.sessions = {
    current: sessions,
    create: operationSnapshot("idle"),
    list: operationSnapshot("ready"),
    delete: operationSnapshot("idle"),
    runtimeConfigUpdate: operationSnapshot("idle"),
    submitUserMessage: operationSnapshot("idle"),
    respondExternalRequest: operationSnapshot("idle"),
    interrupt: operationSnapshot("idle"),
    watch: operationSnapshot("received"),
    timelines: {},
  };

  return snapshot;
}

export function sessionView(overrides: Partial<SessionView> = {}): SessionView {
  const id = overrides.id ?? "session-a";

  return {
    id,
    revision: 0n,
    status: { phase: "idle", failedMessage: "" },
    currentPrompt: null,
    createdAt: timestamp(1_700_000_000),
    updatedAt: timestamp(1_700_000_000),
    lastActivityAt: timestamp(1_700_000_000),
    runtimeConfig: null,
    metrics: sessionMetricsView(),
    ...overrides,
  };
}

export function sessionMetricsView() {
  return {
    inputTokens: 0n,
    outputTokens: 0n,
    cacheReadTokens: 0n,
    cacheWriteTokens: 0n,
    imageInputTokens: 0n,
    imageInputCount: 0n,
    totalTokens: 0n,
    totalInputTokens: 0n,
    contextTokens: 0n,
    costCurrency: null,
    totalCostMicros: 0n,
  };
}

export function timestamp(seconds: number) {
  return { seconds: BigInt(seconds), nanos: 0 };
}

export function modelView(overrides: Partial<ModelView> = {}): ModelView {
  return {
    name: "",
    kind: "llm",
    options: {
      temperature: null,
      topP: null,
      topK: null,
      minP: null,
      presencePenalty: null,
      repetitionPenalty: null,
      reasoningEfforts: [],
    },
    limits: { context: null, output: null },
    capabilities: { tools: false, image: false },
    catalogMatch: null,
    cost: null,
    ...overrides,
  };
}

export function mcpServerView(overrides: Partial<McpServerView> = {}): McpServerView {
  return {
    name: "",
    kind: "stdio",
    command: "",
    url: "",
    ...overrides,
  };
}

export function claudeConfig(): ConfigView {
  const claude: ProviderView = {
    kind: "anthropicMessages",
    name: "claude",
    url: "https://api.anthropic.com/v1/messages",
    headers: {},
    oauthCompatible: false,
    oauthSettings: null,
    models: {},
  };

  return {
    etag: "v1",
    providers: { claude },
    mcpServers: {},
    runtimeDefaults: null,
  };
}
