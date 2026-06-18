import type {
  ClientSnapshot,
  ConfigView,
  McpServerView,
  ModelView,
  OperationSnapshot,
  ProviderView,
  SessionSnapshot,
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
    watch: operationSnapshot("idle"),
  };
}

export function operationSnapshot(status: string): OperationSnapshot {
  return { status, error: null, key: null };
}

export function emptyConfig(etag = "v1"): ConfigView {
  return { etag, providers: {}, mcpServers: {} };
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
    },
    limits: { context: null, output: null },
    capabilities: { tools: false, image: false },
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
    kind: "anthropic",
    name: "claude",
    url: "https://api.anthropic.com",
    headers: {},
    models: {},
  };

  return {
    etag: "v1",
    providers: { claude },
    mcpServers: {},
  };
}
