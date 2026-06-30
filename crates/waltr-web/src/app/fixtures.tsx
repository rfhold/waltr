import { createMemo, type ParentProps } from "solid-js";

import { AppStateContextProvider, type AppPhase, type AppState } from "./context";
import type { ClientCommand, ClientSnapshot, OperationSnapshot, SessionTimelineView, SessionView, TimelineEventView } from "../generated/waltr-client";

export function fixtureSnapshotForPath(pathname: string) {
  if (!import.meta.env.DEV && import.meta.env.MODE !== "test") {
    return undefined;
  }

  if (pathname.startsWith("/__fixtures__/sessions/")) {
    return promptFixtureSnapshot();
  }

  return undefined;
}

export function FixtureAppStateProvider(props: ParentProps<{ snapshot: ClientSnapshot }>) {
  const snapshot = () => props.snapshot;
  const phase = createMemo<AppPhase>(() => "ready");
  const state: AppState = {
    phase,
    minimumSplashElapsed: () => true,
    snapshot,
    clientError: () => undefined,
    clientLoadError: () => undefined,
    clientLoaded: () => true,
    systemReady: () => true,
    dispatch: async (_command: ClientCommand) => props.snapshot,
    dispatchConfigPatch: async (_command: ClientCommand) => props.snapshot,
    searchCatalogModels: async () => ({ models: [], stale: false }),
    suggestCatalogMatches: async () => ({ matches: [], stale: false }),
    dispatchCreateSession: async () => props.snapshot,
    dispatchDeleteSession: async (_id: string) => props.snapshot,
    dispatchUpdateSessionRuntimeConfig: async (_command) => props.snapshot,
    dispatchSubmitUserMessage: async (_sessionId: string, _text: string) => props.snapshot,
    dispatchRespondExternalRequest: async (_sessionId: string, _requestId: string, _responseJson: string) => props.snapshot,
    dispatchInterruptSession: async (_sessionId: string) => props.snapshot,
  };

  return <AppStateContextProvider value={state}>{props.children}</AppStateContextProvider>;
}

function promptFixtureSnapshot(): ClientSnapshot {
  return {
    operations: { current: {} },
    config: {
      current: { etag: "fixture", providers: {}, mcpServers: {}, runtimeDefaults: null },
      load: operationSnapshot("idle"),
      patch: operationSnapshot("idle"),
      watch: operationSnapshot("received"),
    },
    status: { ready: true, watch: operationSnapshot("ready") },
    sessions: {
      current: [sessionView("permission"), sessionView("question")],
      create: operationSnapshot("idle"),
      list: operationSnapshot("ready"),
      delete: operationSnapshot("idle"),
      runtimeConfigUpdate: operationSnapshot("idle"),
      submitUserMessage: operationSnapshot("idle"),
      respondExternalRequest: operationSnapshot("idle"),
      interrupt: operationSnapshot("idle"),
      watch: operationSnapshot("received"),
      timelines: {
        permission: timeline(permissionRequestEvent()),
        question: timeline(questionRequestEvent()),
      },
    },
  };
}

function sessionView(id: string): SessionView {
  return {
    id,
    revision: 0n,
    status: { phase: "suspended", failedMessage: "" },
    currentPrompt: null,
    createdAt: timestamp(1_700_000_000),
    updatedAt: timestamp(1_700_000_000),
    lastActivityAt: timestamp(1_700_000_000),
    runtimeConfig: null,
    metrics: {
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
    },
  };
}

function permissionRequestEvent(): TimelineEventView {
  return timelineEvent("permission", "permission-fixture", 1, {
    tool_call: {
      name: "read",
      arguments: { filePath: "../outside/project-notes.md" },
    },
    patterns: [{ tool: "read", arguments: { filePath: "../outside/**" } }],
  });
}

function questionRequestEvent(): TimelineEventView {
  return timelineEvent("question", "question-fixture", 2, {
    questions: [
      {
        header: "Direction",
        question: "Which visual direction should the prompt take?",
        multiple: false,
        custom: true,
        options: [
          { label: "Compact", description: "Keep the prompt dense and fast." },
          { label: "Guided", description: "Use more explanatory copy." },
        ],
      },
      {
        header: "Details",
        question: "What should stay visible while answering?",
        multiple: true,
        custom: false,
        options: [
          { label: "Tool name", description: "Show the requested action." },
          { label: "Payload", description: "Show the full request payload." },
        ],
      },
    ],
  });
}

function timelineEvent(sessionId: string, requestId: string, kind: 1 | 2, payload: unknown): TimelineEventView {
  return {
    cursor: 1n,
    sessionId,
    turn: 1n,
    itemId: `external-request:${requestId}`,
    parentId: null,
    kind: "externalRequestOpened",
    channel: "external_request",
    content: {
      kind: "externalRequest",
      request: {
        id: requestId,
        kind,
        payloadJson: JSON.stringify(payload),
      },
    },
    status: "awaitingExternalInput",
    metadataJson: "{}",
  };
}

function timeline(event: TimelineEventView): SessionTimelineView {
  return { events: [event], load: operationSnapshot("ready"), watch: operationSnapshot("opened") };
}

function operationSnapshot(status: string): OperationSnapshot {
  return { status, error: null, key: null };
}

function timestamp(seconds: number) {
  return { seconds: BigInt(seconds), nanos: 0 };
}
