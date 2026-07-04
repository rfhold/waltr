import init, { WaltrClient } from "../../generated/wasm/waltr_client";

export type {
  ClientCommand,
  ClientSnapshot,
  OperationSnapshot,
} from "../generated/waltr-client";

import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";

export type CatalogModelRef = {
  providerId: string;
  modelId: string;
};

export type CatalogModelSummary = {
  model: CatalogModelRef | null;
  providerName: string;
  modelName: string;
  family: string | null;
  status: string | null;
  limits: { context: number | null; output: number | null } | null;
  cost: {
    inputPerMillionTokens: number | null;
    outputPerMillionTokens: number | null;
    cacheReadPerMillionTokens: number | null;
    cacheWritePerMillionTokens: number | null;
    imageInputPerMillionTokens: number | null;
    imageInputPerImage: number | null;
    currency: string;
  } | null;
  capabilities: { tools: boolean; image: boolean; audio: boolean } | null;
  reasoning: { supported: boolean; effort: string | null; efforts: string[] } | null;
};

export type CatalogMatch = {
  model: CatalogModelRef | null;
  providerName: string;
  modelName: string;
  confidence: "unspecified" | "exact" | "high" | "suggested" | "unknown";
  reasons: string[];
  upstreamModel: CatalogModelRef | null;
};

export type CatalogSearchRequest = {
  query: string;
  providerId?: string;
  limit?: number;
};

export type CatalogSearchResponse = {
  models: CatalogModelSummary[];
  stale: boolean;
};

export type CatalogSuggestRequest = {
  providerKey: string;
  modelKey: string;
  limit?: number;
};

export type CatalogSuggestResponse = {
  matches: CatalogMatch[];
  stale: boolean;
};

export type SessionTemplateKind = "file" | "tool" | "unknown";
export type SessionTemplateSearchTarget = "resources" | "prompts";

export type SessionTemplateOption = {
  kind: SessionTemplateKind;
  label: string;
  detail: string;
  template: string;
  metadata: unknown;
};

export type SessionTemplateSearchRequest = {
  requestId: string;
  sessionId: string;
  query: string;
  limit?: number;
  kinds?: Array<"file" | "tool">;
  target?: SessionTemplateSearchTarget;
};

export type SessionTemplateSearchResponse = {
  requestId: string;
  options: SessionTemplateOption[];
  error: string | null;
};

export type ClientSnapshotListener = (snapshot: ClientSnapshot) => void;

export type WasmClient = {
  endpoint: string;
  snapshot: () => ClientSnapshot | undefined;
  start: () => Promise<void>;
  subscribe: (listener: ClientSnapshotListener) => () => void;
  dispatch: (command: ClientCommand) => Promise<ClientSnapshot>;
  searchCatalogModels: (request: CatalogSearchRequest) => Promise<CatalogSearchResponse>;
  suggestCatalogMatches: (request: CatalogSuggestRequest) => Promise<CatalogSuggestResponse>;
  searchSessionTemplates: (request: SessionTemplateSearchRequest) => Promise<SessionTemplateSearchResponse>;
};

export type WasmClientLoader = () => Promise<WasmClient>;

export async function loadWasmClient(): Promise<WasmClient> {
  await init();

  const client = WaltrClient.forCurrentOrigin();
  let currentSnapshot: ClientSnapshot | undefined;
  let currentSnapshotJson: string | undefined;
  let started = false;
  const listeners = new Set<ClientSnapshotListener>();

  const publish = (snapshot: ClientSnapshot) => {
    currentSnapshot = snapshot;

    for (const listener of listeners) {
      listener(snapshot);
    }

    return snapshot;
  };
  const publishJson = (snapshotJson: string) => {
    if (snapshotJson === currentSnapshotJson && currentSnapshot) {
      return currentSnapshot;
    }

    currentSnapshotJson = snapshotJson;

    return publish(JSON.parse(snapshotJson) as ClientSnapshot);
  };

  const readSnapshot = async () =>
    publishJson(await client.snapshotJson());

  const dispatch = async (command: ClientCommand) => {
    try {
      return publishJson(await client.dispatchJson(JSON.stringify(command)));
    } catch (error) {
      await readSnapshot();
      throw error;
    }
  };

  const searchCatalogModels = async (request: CatalogSearchRequest) =>
    JSON.parse(await (client as WaltrClient & {
      searchCatalogModelsJson(requestJson: string): Promise<string>;
    }).searchCatalogModelsJson(JSON.stringify(request))) as CatalogSearchResponse;

  const suggestCatalogMatches = async (request: CatalogSuggestRequest) =>
    JSON.parse(await (client as WaltrClient & {
      suggestCatalogMatchesJson(requestJson: string): Promise<string>;
    }).suggestCatalogMatchesJson(JSON.stringify(request))) as CatalogSuggestResponse;

  const searchSessionTemplates = async (request: SessionTemplateSearchRequest) =>
    JSON.parse(await (client as WaltrClient & {
      searchSessionTemplatesJson(requestJson: string): Promise<string>;
    }).searchSessionTemplatesJson(JSON.stringify(request))) as SessionTemplateSearchResponse;

  const start = async () => {
    if (started) {
      return;
    }

    started = true;
    await readSnapshot();

    while (started) {
      publishJson(await client.nextStateJson());
    }
  };

  return {
    endpoint: client.endpoint(),
    snapshot: () => currentSnapshot,
    start,
    subscribe: (listener) => {
      listeners.add(listener);

      if (currentSnapshot) {
        listener(currentSnapshot);
      }

      return () => {
        listeners.delete(listener);
      };
    },
    dispatch,
    searchCatalogModels,
    suggestCatalogMatches,
    searchSessionTemplates,
  };
}
