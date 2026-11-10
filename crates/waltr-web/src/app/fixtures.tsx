import { createMemo, createSignal, type ParentProps } from "solid-js";

import { AppStateContextProvider, type AppPhase, type AppState } from "./context";
import type { AuthSnapshot, ClientCommand, ClientSnapshot, OperationSnapshot, SessionTimelineView, SessionView } from "../generated/waltr-client";

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
  const [explicitAuthTarget, setExplicitAuthTarget] = createSignal<Parameters<AppState["dispatchBeginExplicitAuth"]>[0]>();
  const [presentedAuthAttemptId, setPresentedAuthAttemptId] = createSignal<string>();
  const [authorizationDraft, setAuthorizationDraft] = createSignal<{ attemptId: string; input: string }>();
  const state: AppState = {
    phase,
    minimumSplashElapsed: () => true,
    snapshot,
    clientError: () => undefined,
    clientLoadError: () => undefined,
    clientLoaded: () => true,
    systemReady: () => true,
    explicitAuthTarget,
    presentedAuthAttemptId,
    authorizationDraft,
    accessOperations: () => undefined,
    openAuthorizationAttempt: () => undefined,
    openExternalUrl: () => undefined,
    dispatch: async (_command: ClientCommand) => props.snapshot,
    dispatchBeginExplicitAuth: async (target) => {
      setExplicitAuthTarget(target);
      setPresentedAuthAttemptId(props.snapshot.auth.activeAttempt?.attemptId);
      return props.snapshot;
    },
    dispatchCompleteAuth: async (_attemptId, _input) => {
      setExplicitAuthTarget(undefined);
      setPresentedAuthAttemptId(undefined);
      return props.snapshot;
    },
    dispatchCancelAuth: async (_attemptId) => {
      setExplicitAuthTarget(undefined);
      setPresentedAuthAttemptId(undefined);
      return props.snapshot;
    },
    dispatchRefreshAuth: async (_target) => props.snapshot,
    dispatchLogoutAuth: async (_target) => props.snapshot,
    clearExplicitAuthTarget: () => {
      setExplicitAuthTarget(undefined);
      setPresentedAuthAttemptId(undefined);
    },
    setAuthorizationDraft: (attemptId, input) => setAuthorizationDraft({ attemptId, input }),
    clearAuthorizationDraft: () => setAuthorizationDraft(undefined),
    dispatchConfigPatch: async (_command: ClientCommand) => props.snapshot,
    dispatchResetConfig: async () => props.snapshot,
    dispatchCreateSession: async (_agentName) => props.snapshot,
    dispatchDeleteSession: async (_id: string) => props.snapshot,
    dispatchWipeSessions: async () => props.snapshot,
    dispatchUpdateSessionInferenceSettings: async (_command) => props.snapshot,
    dispatchUpdateSessionAgent: async (_id, _agentName) => props.snapshot,
    dispatchSubmitUserMessage: async (_sessionId: string, _text: string) => props.snapshot,
    dispatchInterruptSession: async (_sessionId: string) => props.snapshot,
    dispatchContinueSession: async (_command) => props.snapshot,
  };

  return <AppStateContextProvider value={state}>{props.children}</AppStateContextProvider>;
}

function promptFixtureSnapshot(): ClientSnapshot {
  return {
    interactions: {
      permission: { generation: "fixture", revision: 1n, connected: true, error: null, pending: [{ identity: { sessionId: "permission", generation: "fixture", id: "permission-fixture", operationId: "operation-1" }, request: { kind: "permission", message: "Read a file outside the workspace", identity: "read", claims: ['{"filePath":"../outside/project-notes.md"}'], suggestions: ['{"filePath":"../outside/**"}'] }, responding: false, error: null }] },
      question: { generation: "fixture", revision: 1n, connected: true, error: null, pending: [{ identity: { sessionId: "question", generation: "fixture", id: "question-fixture", operationId: "operation-2" }, request: questionRequest(), responding: false, error: null }] },
    },
    access: { status: "authenticated", error: null, activeAttemptId: null },
    operations: { current: {} },
    apps: { current: {}, requests: {} },
    auth: emptyAuthSnapshot(),
    catalog: {
      search: { requestId: "", query: "", status: "idle", error: null, stale: false, models: [] },
      suggestions: { requestId: "", status: "idle", error: null, stale: false, matches: [] },
    },
    config: {
      current: { etag: "fixture", providers: {}, mcpServers: {}, inferenceDefaults: null },
      resourceBaselines: {},
      load: operationSnapshot("idle"),
      patch: operationSnapshot("idle"),
      reset: operationSnapshot("idle"),
      watch: operationSnapshot("received"),
    },
    status: { ready: true, watch: operationSnapshot("ready") },
    routines: {
      requests: {},
      definitions: { watch: operationSnapshot("idle"), digest: "", definitions: [], supportedCapabilities: [], limits: null },
      history: { snapshotHighWater: null, snapshotComplete: false, operations: [], watch: operationSnapshot("idle"), recent: [], throughSequence: null, olderPageToken: null, pagination: operationSnapshot("idle") },
      sessionHistories: {},
      details: {},
      starts: {},
    },
    sessions: {
      agents: { entries: [], defaultAgentName: "", load: operationSnapshot("ready") },
      current: [sessionView("permission"), sessionView("question")],
      create: operationSnapshot("idle"),
      list: operationSnapshot("ready"),
      delete: operationSnapshot("idle"),
      wipe: operationSnapshot("idle"),
      inferenceSettingsUpdate: operationSnapshot("idle"),
      agentUpdate: operationSnapshot("idle"),
      submitUserMessage: operationSnapshot("idle"),
      interrupt: operationSnapshot("idle"),
      rollback: operationSnapshot("idle"),
      continueSession: operationSnapshot("idle"),
      watch: operationSnapshot("received"),
      timelines: {
        permission: timeline(),
        question: timeline(),
      },
      templateSearches: {},
    },
  };
}

function emptyAuthSnapshot(): AuthSnapshot {
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

function sessionView(id: string): SessionView {
  return {
    id,
    agentName: "default",
    revision: 0n,
    status: { phase: "runningTool", suspensionKind: null, failedMessage: "", authenticationFailure: null },
    coordination: { phase: "running", reason: null, queuedCount: 0, continuationArmed: false },
    progress: { phase: "running", runStage: "runningTool", reason: null, queuedCount: 0n, continuationArmed: false, running: true },
    capabilities: { promptDisposition: "queued", canInterrupt: true },
    currentPrompt: null,
    createdAt: timestamp(1_700_000_000),
    updatedAt: timestamp(1_700_000_000),
    lastActivityAt: timestamp(1_700_000_000),
    inferenceSettings: null,
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
    associations: { taskParentSessionId: null, taskDescription: null, routineInvocationId: null },
    metadata: {},
  };
}

function questionRequest(): Extract<import("../generated/waltr-client").InteractionRequestView, { kind: "question" }> {
  return {
    kind: "question",
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
  };
}

function timeline(): SessionTimelineView {
  return {
    events: [],
    projectionRevision: null,
    rawPageBoundary: null,
    hasMore: false,
    load: operationSnapshot("ready"),
    watch: operationSnapshot("opened"),
  };
}

function operationSnapshot(status: string): OperationSnapshot {
  return { status, error: null, key: null };
}

function timestamp(seconds: number) {
  return { seconds: BigInt(seconds), nanos: 0 };
}
