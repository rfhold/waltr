import type {
  AuthSnapshot,
  CatalogSnapshot,
  ClientSnapshot,
  ConfigView,
  McpServerView,
  ModelView,
  OperationSnapshot,
  ProviderView,
  ResourceEditBaselineView,
  SessionSnapshot,
  SessionTimelineView,
  SessionView,
} from "../../src/generated/waltr-client";

export function snapshotWithConfig(
  config: ConfigView,
  options: { ready?: boolean } = {},
): ClientSnapshot {
  return {
    access: { status: "authenticated", error: null, activeAttemptId: null },
    operations: { current: {} },
    auth: emptyAuthSnapshot(),
    catalog: emptyCatalogSnapshot(),
    config: {
      current: config,
      resourceBaselines: resourceBaselines(config),
      load: operationSnapshot("idle"),
      patch: operationSnapshot("idle"),
      reset: operationSnapshot("idle"),
      watch: operationSnapshot("received"),
    },
    status: {
      ready: options.ready ?? true,
      watch: operationSnapshot("ready"),
    },
    sessions: emptySessionSnapshot(),
  };
}

function resourceBaselines(config: ConfigView): Record<string, ResourceEditBaselineView> {
  const baselines: Record<string, ResourceEditBaselineView> = {};

  for (const [name, provider] of Object.entries(config.providers)) {
    if (!provider) continue;
    const path = `providers/${name}.yaml`;
    baselines[path] = {
      path,
      persistedEtag: config.etag,
      absentEtag: "absent-etag",
      activeSource: `apiVersion: waltr.dev/v1alpha1\nkind: Provider\nmetadata:\n  name: ${name}\nspec:\n  transport: ${provider.kind}\n  url: ${provider.url}\n`,
    };
  }

  for (const [name, server] of Object.entries(config.mcpServers)) {
    if (!server) continue;
    const path = `mcp-servers/${name}.yaml`;
    baselines[path] = {
      path,
      persistedEtag: config.etag,
      absentEtag: "absent-etag",
      activeSource: `apiVersion: waltr.dev/v1alpha1\nkind: McpServer\nmetadata:\n  name: ${name}\nspec:\n  transport: ${server.kind}\n`,
    };
  }

  return baselines;
}

export function emptyCatalogSnapshot(): CatalogSnapshot {
  return {
    search: {
      requestId: "",
      query: "",
      status: "idle",
      error: null,
      stale: false,
      models: [],
    },
    suggestions: {
      requestId: "",
      status: "idle",
      error: null,
      stale: false,
      matches: [],
    },
  };
}

export function emptyAuthSnapshot(): AuthSnapshot {
  return {
    statuses: {},
    activeAttempt: null,
    list: operationSnapshot("idle"),
    begin: operationSnapshot("idle"),
    complete: operationSnapshot("idle"),
    cancel: operationSnapshot("idle"),
    refresh: operationSnapshot("idle"),
    logout: operationSnapshot("idle"),
  };
}

export function emptySessionSnapshot(): SessionSnapshot {
  return {
    agents: { entries: [], defaultAgentName: "", load: operationSnapshot("idle") },
    current: [],
    create: operationSnapshot("idle"),
    list: operationSnapshot("idle"),
    delete: operationSnapshot("idle"),
    wipe: operationSnapshot("idle"),
    inferenceSettingsUpdate: operationSnapshot("idle"),
    agentUpdate: operationSnapshot("idle"),
    submitUserMessage: operationSnapshot("idle"),
    respondExternalRequest: operationSnapshot("idle"),
    interrupt: operationSnapshot("idle"),
    rollback: operationSnapshot("idle"),
    continueSession: operationSnapshot("idle"),
    watch: operationSnapshot("idle"),
    timelines: {},
    templateSearches: {},
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
  return { etag, providers: {}, mcpServers: {}, inferenceDefaults: null };
}

export function snapshotWithSessions(sessions: SessionView[]): ClientSnapshot {
  const snapshot = snapshotWithConfig(emptyConfig("v1"));
  snapshot.sessions = {
    agents: {
      entries: [
        { name: "default", description: "General-purpose agent" },
        { name: "reviewer", description: "Reviews changes and risks" },
      ],
      defaultAgentName: "default",
      load: operationSnapshot("ready"),
    },
    current: sessions,
    create: operationSnapshot("idle"),
    list: operationSnapshot("ready"),
    delete: operationSnapshot("idle"),
    wipe: operationSnapshot("idle"),
    inferenceSettingsUpdate: operationSnapshot("idle"),
    agentUpdate: operationSnapshot("idle"),
    submitUserMessage: operationSnapshot("idle"),
    respondExternalRequest: operationSnapshot("idle"),
    interrupt: operationSnapshot("idle"),
    rollback: operationSnapshot("idle"),
    continueSession: operationSnapshot("idle"),
    watch: operationSnapshot("received"),
    timelines: {},
    templateSearches: {},
  };

  return snapshot;
}

export function sessionView(overrides: Partial<SessionView> = {}): SessionView {
  const id = overrides.id ?? "session-a";

  return {
    id,
    agentName: "default",
    revision: 0n,
    status: { phase: "idle", failedMessage: "", authenticationFailure: null },
    currentPrompt: null,
    createdAt: timestamp(1_700_000_000),
    updatedAt: timestamp(1_700_000_000),
    lastActivityAt: timestamp(1_700_000_000),
    inferenceSettings: null,
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
    inferenceDefaults: null,
  };
}
