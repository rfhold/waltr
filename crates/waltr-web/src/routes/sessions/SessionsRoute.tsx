import { A, useNavigate, useParams } from "@solidjs/router";
import { Virtualizer, type CustomContainerComponentProps, type CustomItemComponentProps, type VirtualizerHandle } from "virtua/solid";
import {
  For,
  Match,
  Show,
  Switch,
  createContext,
  createEffect,
  createMemo,
  createSignal,
  on,
  onCleanup,
  untrack,
  useContext,
  type Accessor,
  type JSX,
  type ParentProps,
} from "solid-js";

import { useAppState } from "../../app/context";
import { CodeBlock, HighlightedCode } from "../../components/markdown/CodeBlock";
import { MarkdownMessage } from "../../components/markdown/MarkdownMessage";
import { ConfirmAction, Dialog, InlineAlert } from "../../components/ui";
import { languageFromFilePath } from "../../web-lib/language";
import type {
  ClientCommand,
  ConfigView,
  ExternalRequestView,
  McpServerView,
  ProviderView,
  RuntimeConfigMatchView,
  SessionTemplateOptionView,
  SessionTimelineView,
  SessionView,
  TimestampView,
} from "../../generated/waltr-client";

const TIMELINE_CHANNELS = [
  "user_text",
  "assistant_text",
  "thinking",
  "tool_call",
  "tool_progress_info",
  "tool_progress_error",
  "tool_result",
  "external_request",
  "metrics",
];
const TIMELINE_PAGE_SIZE = 64;
const TIMELINE_TOP_LOAD_THRESHOLD_PX = 320;
const SHELL_OUTPUT_RECENT_LINE_COUNT = 15;
const RESOURCE_SEARCH_DEBOUNCE_MS = 120;
const RESOURCE_SEARCH_LIMIT = 12;

const EXTERNAL_REQUEST_PERMISSION = 1;
const EXTERNAL_REQUEST_QUESTION = 2;

type SessionsContextValue = {
  sessions: Accessor<SessionView[]>;
  listError: Accessor<string | null | undefined>;
  loading: Accessor<boolean>;
  creating: Accessor<boolean>;
  createDisabled: Accessor<boolean>;
  createError: Accessor<string | undefined>;
  confirmingDelete: Accessor<string | undefined>;
  deletingSession: Accessor<string | undefined>;
  deleteErrors: Accessor<Record<string, string>>;
  createSession: () => Promise<void>;
  promptDelete: (id: string) => void;
  cancelDelete: () => void;
  deleteSession: (id: string) => Promise<void>;
  deleteDisabled: Accessor<boolean>;
};

const SessionsContext = createContext<SessionsContextValue>();

export function SessionsRoute(props: ParentProps) {
  const app = useAppState();
  const navigate = useNavigate();
  const params = useParams();
  const [creating, setCreating] = createSignal(false);
  const [createError, setCreateError] = createSignal<string>();
  const [confirmingDelete, setConfirmingDelete] = createSignal<string>();
  const [deletingSession, setDeletingSession] = createSignal<string>();
  const [deleteErrors, setDeleteErrors] = createSignal<Record<string, string>>({});
  const snapshot = createMemo(() => app.snapshot());
  const sessions = createMemo(() => sortSessionsByUpdated(snapshot()?.sessions.current ?? []));
  const listError = createMemo(
    () => snapshot()?.sessions.watch.error ?? snapshot()?.sessions.list.error,
  );
  const loading = createMemo(() => {
    const sessionsSnapshot = snapshot()?.sessions;

    if (!sessionsSnapshot || listError()) {
      return false;
    }

    return sessionsSnapshot.list.status !== "ready"
      && sessionsSnapshot.watch.status !== "opened"
      && sessionsSnapshot.watch.status !== "received";
  });
  let registeredSessionWatchInterest = false;
  let requestedConfig = false;
  const readyForSessionWatch = createMemo(() => app.clientLoaded() && app.systemReady());

  createEffect(on(readyForSessionWatch, (ready) => {
    if (!ready || registeredSessionWatchInterest) {
      return;
    }

    registeredSessionWatchInterest = true;
    untrack(() => dispatchSessionWatchCommand(app.dispatch, {
      type: "registerSessionWatchInterest",
    }));
  }));

  createEffect(() => {
    if (!readyForSessionWatch() || requestedConfig || snapshot()?.config.current) {
      return;
    }

    requestedConfig = true;
    untrack(() => dispatchSessionWatchCommand(app.dispatch, { type: "requestConfig" }));
  });

  onCleanup(() => {
    if (!registeredSessionWatchInterest) {
      return;
    }

    dispatchSessionWatchCommand(app.dispatch, {
      type: "releaseSessionWatchInterest",
    });
  });

  const createSession = async () => {
    setCreating(true);
    setCreateError(undefined);

    try {
      const terminalSnapshot = await app.dispatchCreateSession();
      const key = terminalSnapshot.sessions.create.key;
      const result = key ? terminalSnapshot.operations.current[key] : undefined;
      const session = result?.kind === "sessionCreate" ? result.session : null;

      if (!session) {
        throw new Error("session create completed without a session");
      }

      navigate(`/sessions/${encodeURIComponent(session.id)}`);
    } catch (error) {
      setCreateError(error instanceof Error ? error.message : "Unable to create session.");
    } finally {
      setCreating(false);
    }
  };

  const deleteSession = async (id: string) => {
    setDeletingSession(id);
    setDeleteErrors((errors) => omitKey(errors, id));

    try {
      await app.dispatchDeleteSession(id);
      setConfirmingDelete(undefined);

      if (params.sessionId === id) {
        navigate("/sessions");
      }
    } catch (error) {
      setDeleteErrors((errors) => ({
        ...errors,
        [id]: error instanceof Error ? error.message : "Unable to delete session.",
      }));
    } finally {
      setDeletingSession(undefined);
    }
  };

  return (
    <SessionsContext.Provider
      value={{
        sessions,
        listError,
        loading,
        creating,
        createDisabled: () => creating() || !app.clientLoaded() || !app.systemReady(),
        createError,
        confirmingDelete,
        deletingSession,
        deleteErrors,
        createSession,
        promptDelete: setConfirmingDelete,
        cancelDelete: () => setConfirmingDelete(undefined),
        deleteSession,
        deleteDisabled: () => Boolean(deletingSession()),
      }}
    >
      {props.children}
    </SessionsContext.Provider>
  );
}

export function SessionsListRoute() {
  const navigate = useNavigate();
  const sessions = useSessionsRoute();

  return (
    <section class="sessions sessions--list" aria-labelledby="sessions-title">
      <SessionsHeader
        title="Sessions"
        action={
          <button
            type="button"
            onClick={() => void sessions.createSession()}
            disabled={sessions.createDisabled()}
          >
            {sessions.creating() ? "Creating..." : "Create session"}
          </button>
        }
      />

      <SessionRouteAlerts />

      <section class="sessions__list-panel" aria-labelledby="sessions-list-title">
        <h2 id="sessions-list-title">Recent sessions</h2>
        <Show
          when={sessions.sessions().length > 0}
          fallback={<p class="sessions__empty">No sessions yet.</p>}
        >
          <ul class="sessions__list" aria-label="Session list">
            <For each={sessions.sessions()}>
              {(session) => (
                <li>
                  <div class="session-row">
                    <button
                      type="button"
                      class="session-row__open"
                      onClick={() => navigate(`/sessions/${encodeURIComponent(session.id)}`)}
                      aria-label={`Open session ${session.id}`}
                    >
                      <span>
                        <strong>{session.id}</strong>
                        <small>{formatTimestamp(session.updatedAt) ?? "No updates"}</small>
                      </span>
                      <StatusChit phase={session.status.phase} />
                    </button>
                    <SessionDeleteAction session={session} compact />
                  </div>
                  <Show when={sessions.deleteErrors()[session.id]}>
                    {(message) => <p class="sessions__row-error">{message()}</p>}
                  </Show>
                </li>
              )}
            </For>
          </ul>
        </Show>
      </section>
    </section>
  );
}

export function SessionsDetailRoute() {
  const params = useParams();
  const sessions = useSessionsRoute();
  const selectedSession = createMemo(() => {
    const id = params.sessionId;

    return id ? sessions.sessions().find((session) => session.id === id) : undefined;
  });

  return (
    <section class="sessions sessions--detail" aria-label="Session detail">
      <Show when={params.sessionId}>
        {(sessionId) => (
          <Switch>
            <Match when={selectedSession()}>
              {(session) => <SessionDetail session={session()} />}
            </Match>
            <Match when={true}>
              <nav class="sessions__breadcrumb" aria-label="Session breadcrumb">
                <A href="/sessions">Sessions</A>
                <span>/</span>
                <span>{sessionId()}</span>
              </nav>
              <h1 id="session-detail-title">Session</h1>
              <SessionRouteAlerts />
              <InlineAlert>Waiting for session...</InlineAlert>
            </Match>
          </Switch>
        )}
      </Show>
    </section>
  );
}

function SessionsHeader(props: { title: string; action?: JSX.Element }) {
  return (
    <header class="sessions__header">
      <div>
        <p class="eyebrow">Sessions</p>
        <h1 id="sessions-title">{props.title}</h1>
      </div>
      {props.action}
    </header>
  );
}

function SessionRouteAlerts() {
  const sessions = useSessionsRoute();

  return (
    <>
      <Show when={sessions.createError()}>
        {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
      </Show>

      <Switch>
        <Match when={sessions.listError()}>
          <InlineAlert tone="danger">Unable to load sessions.</InlineAlert>
        </Match>
        <Match when={sessions.loading()}>
          <InlineAlert>Loading sessions...</InlineAlert>
        </Match>
      </Switch>
    </>
  );
}

function SessionDetail(props: { session: SessionView }) {
  const app = useAppState();
  const sessions = useSessionsRoute();
  const [editingRuntime, setEditingRuntime] = createSignal<"provider" | "mcp">();
  const [draft, setDraft] = createSignal("");
  const [caret, setCaret] = createSignal<{ start: number; end: number }>({ start: 0, end: 0 });
  const [resourceInputVersion, setResourceInputVersion] = createSignal(0);
  const [resourceRequestId, setResourceRequestId] = createSignal<string>();
  const [selectedResourceIndex, setSelectedResourceIndex] = createSignal(0);
  const [templateSearchError, setTemplateSearchError] = createSignal<string>();
  const [commandError, setCommandError] = createSignal<string>();
  const [expandedShellOutputs, setExpandedShellOutputs] = createSignal<Record<string, boolean>>({});
  const [historyShift, setHistoryShift] = createSignal(false);
  let timelineScroll: HTMLDivElement | undefined;
  let timelineVirtualizer: VirtualizerHandle | undefined;
  let messageTextarea: HTMLTextAreaElement | undefined;
  let highlightLayer: HTMLDivElement | undefined;
  let registeredTimelineSession: string | undefined;
  let templateSearchDebounce: number | undefined;
  let templateSearchSequence = 0;
  let previousTimelineItemKeys: string[] = [];
  let initialScrollSession: string | undefined;
  let pendingHistoryCursor: SessionTimelineView["events"][number]["cursor"] | undefined;
  let exhaustedHistoryCursor: SessionTimelineView["events"][number]["cursor"] | undefined;
  let followTail = true;
  let lastTimelineScrollTop: number | undefined;
  let viewportUpdateFrame: number | undefined;
  let viewportUpdateShouldStickToTail = false;
  let clearHistoryShiftFrame: number | undefined;

  const timeline = createMemo(() => app.snapshot()?.sessions.timelines?.[props.session.id]);
  const active = createMemo(() => sessionActive(props.session));
  const items = createMemo(() => timelineItems(timeline()));
  const lastTurn = createMemo(() => items().reduce<bigint | undefined>((last, item) => last === undefined || item.turn > last ? item.turn : last, undefined));
  const turnFooterItemIds = createMemo(() => {
    const lastByTurn = new Map<string, string>();
    for (const item of items()) {
      lastByTurn.set(item.turn.toString(), item.id);
    }
    return new Set(lastByTurn.values());
  });
  const latestMetrics = createMemo(() => latestTurnMetrics(items()));
  const testKeepMountedIndexes = createMemo(() => import.meta.env.MODE === "test" ? items().map((_, index) => index) : undefined);
  const working = createMemo(() => active() && !hasOpenExternalPrompt(items()));
  const importantStatus = createMemo(() => sessionImportantStatus(props.session, working(), timeline()?.watch.error));
  const activeResourceToken = createMemo(() => {
    const text = draft();
    const rememberedCaret = caret();
    resourceInputVersion();

    if (messageTextarea && document.activeElement === messageTextarea) {
      return resourceTokenAtCaret(text, messageTextarea.selectionStart, messageTextarea.selectionEnd);
    }

    return resourceTokenAtCaret(text, rememberedCaret.start, rememberedCaret.end);
  });
  const activeTemplateSearch = createMemo(() => {
    const token = activeResourceToken();
    const requestId = resourceRequestId();
    const search = app.snapshot()?.sessions.templateSearches?.[props.session.id];

    if (!token || !requestId || !search) {
      return undefined;
    }

    if (search.requestId !== requestId || search.query !== token.query) {
      return undefined;
    }

    return search;
  });
  const visibleResourceOptions = createMemo(() => activeResourceToken() ? activeTemplateSearch()?.options ?? [] : []);
  const templateSearchPending = createMemo(() => Boolean(activeResourceToken() && resourceRequestId() && activeTemplateSearch()?.status !== "ready" && !templateSearchError()));
  const templateSearchMessage = createMemo(() => templateSearchError() ?? activeTemplateSearch()?.error);
  const timelineReady = createMemo(() => {
    const status = timeline()?.watch.status;

    return status === "opened" || status === "received";
  });

  createEffect(() => {
    const sessionId = props.session.id;

    if (!app.clientLoaded() || !app.systemReady() || registeredTimelineSession === sessionId) {
      return;
    }

    if (registeredTimelineSession) {
      dispatchSessionWatchCommand(app.dispatch, {
        type: "releaseSessionTimelineWatchInterest",
        sessionId: registeredTimelineSession,
      });
    }

    registeredTimelineSession = sessionId;
    initialScrollSession = undefined;
    pendingHistoryCursor = undefined;
    exhaustedHistoryCursor = undefined;
    followTail = true;
    lastTimelineScrollTop = undefined;
    cancelScheduledTimelineViewportUpdate();
    cancelScheduledHistoryShiftClear();
    setHistoryShift(false);
    previousTimelineItemKeys = [];
    void (async () => {
      try {
        await app.dispatch({
          type: "registerSessionTimelineWatchInterest",
          sessionId,
          afterCursor: null,
          channels: TIMELINE_CHANNELS,
          initialLimit: TIMELINE_PAGE_SIZE,
        });
      } catch (error) {
        setCommandError(error instanceof Error ? error.message : "Unable to watch session timeline.");
      }
    })();
  });

  createEffect(on(activeResourceToken, (token) => {
    cancelTemplateSearchDebounce();
    setTemplateSearchError(undefined);

    if (!token) {
      setResourceRequestId(undefined);
      setSelectedResourceIndex(0);
      return;
    }

    scheduleTemplateSearch(token, RESOURCE_SEARCH_DEBOUNCE_MS);
  }));

  onCleanup(() => {
    cancelTemplateSearchDebounce();
    cancelScheduledTimelineViewportUpdate();
    cancelScheduledHistoryShiftClear();
    if (!registeredTimelineSession) {
      return;
    }

    dispatchSessionWatchCommand(app.dispatch, {
      type: "releaseSessionTimelineWatchInterest",
      sessionId: registeredTimelineSession,
    });
  });

  createEffect(() => {
    const currentTimeline = timeline();
    const currentItems = items();
    const scroller = timelineScroll;
    const sessionId = props.session.id;
    reconcilePendingHistory(currentTimeline);
    const itemKeys = currentItems.map(timelineItemKey);
    const hasNewExternalPrompt = currentItems.some((item) => isExternalPromptItem(item) && !previousTimelineItemKeys.includes(timelineItemKey(item)));
    previousTimelineItemKeys = itemKeys;
    if (hasNewExternalPrompt) {
      followTail = true;
    }
    const shouldInitialScroll = currentItems.length > 0 && initialScrollSession !== sessionId;

    if (!scroller) {
      return;
    }

    if (!shouldInitialScroll && !followTail) {
      return;
    }

    scheduleTimelineScroll(() => {
      if (initialScrollSession !== sessionId && currentItems.length > 0) {
        initialScrollSession = sessionId;
        if (followTail) {
          scrollTimelineToBottom(scroller);
          rememberTimelineScrollTop(scroller);
        }
        maybeLoadOlderTimeline();
        return;
      }

      if (followTail) {
        scrollTimelineToBottom(scroller);
        rememberTimelineScrollTop(scroller);
      }
      maybeLoadOlderTimeline();
    });
  });

  const scheduleTimelineViewportUpdate = (scroller: HTMLElement, options: { stickToTail?: boolean; updateFollowTail?: boolean } = {}) => {
    viewportUpdateShouldStickToTail = viewportUpdateShouldStickToTail || options.stickToTail === true;
    const followTailUpdated = options.updateFollowTail !== false;
    if (followTailUpdated) {
      updateTimelineFollowTail(scroller);
    }

    if (viewportUpdateFrame !== undefined) {
      return;
    }

    viewportUpdateFrame = requestAnimationFrame(() => {
      viewportUpdateFrame = undefined;
      const shouldStickToTail = viewportUpdateShouldStickToTail;
      viewportUpdateShouldStickToTail = false;
      if (shouldStickToTail && followTail) {
        scrollTimelineToBottom(scroller);
        rememberTimelineScrollTop(scroller);
      }
      if (followTailUpdated) {
        maybeLoadOlderTimeline();
      } else {
        updateTimelineViewport(scroller);
      }
    });
  };

  const cancelScheduledTimelineViewportUpdate = () => {
    if (viewportUpdateFrame !== undefined) {
      cancelAnimationFrame(viewportUpdateFrame);
      viewportUpdateFrame = undefined;
    }
    viewportUpdateShouldStickToTail = false;
  };

  const cancelScheduledHistoryShiftClear = () => {
    if (clearHistoryShiftFrame !== undefined) {
      cancelAnimationFrame(clearHistoryShiftFrame);
      clearHistoryShiftFrame = undefined;
    }
  };

  const scheduleHistoryShiftClear = () => {
    cancelScheduledHistoryShiftClear();
    clearHistoryShiftFrame = requestAnimationFrame(() => {
      clearHistoryShiftFrame = undefined;
      setHistoryShift(false);
    });
  };

  const updateTimelineViewport = (scroller: HTMLElement, options: { topBoundary?: "near" | "strict" } = {}) => {
    updateTimelineFollowTail(scroller);
    maybeLoadOlderTimeline(options);
  };

  const updateTimelineFollowTail = (scroller: HTMLElement) => {
    if (lastTimelineScrollTop === undefined || scroller.scrollTop !== lastTimelineScrollTop) {
      followTail = isNearScrollBottom(scroller);
    }
    rememberTimelineScrollTop(scroller);
  };

  const rememberTimelineScrollTop = (scroller: HTMLElement) => {
    lastTimelineScrollTop = scroller.scrollTop;
  };

  const maybeLoadOlderTimeline = (options: { topBoundary?: "near" | "strict" } = {}) => {
    const scroller = timelineScroll;
    if (!scroller) {
      return;
    }

    if (!isScrollable(scroller)) {
      void loadOlderTimeline({ preserveAnchor: false });
      return;
    }

    const nearTop = options.topBoundary === "strict" ? isStrictlyNearScrollTop(scroller) : isNearScrollTop(scroller);
    if (!followTail && nearTop) {
      void loadOlderTimeline();
    }
  };

  const reconcilePendingHistory = (currentTimeline: SessionTimelineView | undefined) => {
    if (pendingHistoryCursor === undefined || !currentTimeline || currentTimeline.load.status === "loading") {
      return;
    }

    const requestedCursor = pendingHistoryCursor;
    pendingHistoryCursor = undefined;
    if (currentTimeline.load.status === "failed") {
      scheduleHistoryShiftClear();
      return;
    }

    if (currentTimeline.events[0]?.cursor === requestedCursor) {
      exhaustedHistoryCursor = requestedCursor;
    }
    scheduleHistoryShiftClear();
  };

  const loadOlderTimeline = async (options: { preserveAnchor: boolean } = { preserveAnchor: true }) => {
    const scroller = timelineScroll;
    const currentTimeline = timeline();
    const earliestCursor = currentTimeline?.events[0]?.cursor;

    if (!scroller || !currentTimeline || earliestCursor === undefined || pendingHistoryCursor !== undefined || exhaustedHistoryCursor === earliestCursor || currentTimeline.load.status === "loading") {
      return;
    }

    pendingHistoryCursor = earliestCursor;
    setHistoryShift(options.preserveAnchor);
    setCommandError(undefined);
    try {
      await app.dispatch({
        type: "requestSessionTimeline",
        sessionId: props.session.id,
        direction: "backward",
        cursor: earliestCursor,
        limit: TIMELINE_PAGE_SIZE,
        channels: TIMELINE_CHANNELS,
      });
    } catch (error) {
      pendingHistoryCursor = undefined;
      scheduleHistoryShiftClear();
      setCommandError(error instanceof Error ? error.message : "Unable to load earlier history.");
    }
  };

  const submit = async () => {
    const text = draft().trim();
    if (!text || !timelineReady()) {
      return;
    }

    setCommandError(undefined);
    try {
      await app.dispatchSubmitUserMessage(props.session.id, text);
      setDraft("");
      setCaret({ start: 0, end: 0 });
      clearResourceAutocomplete();
    } catch (error) {
      setCommandError(error instanceof Error ? error.message : "Unable to send message.");
    }
  };

  const sendTemplateSearch = async (token: ActiveResourceToken) => {
    const requestId = `${props.session.id}-resource-${++templateSearchSequence}`;
    setResourceRequestId(requestId);
    setTemplateSearchError(undefined);
    setSelectedResourceIndex(0);

    try {
      await app.dispatch({
        type: "searchSessionTemplates",
        requestId,
        sessionId: props.session.id,
        query: token.query,
        limit: RESOURCE_SEARCH_LIMIT,
        kinds: ["file", "tool"],
        target: "resources",
      });
    } catch (error) {
      if (resourceRequestId() === requestId) {
        setTemplateSearchError(error instanceof Error ? error.message : "Unable to search templates.");
      }
    }
  };

  const scheduleTemplateSearch = (token: ActiveResourceToken, delay: number) => {
    cancelTemplateSearchDebounce();
    templateSearchDebounce = window.setTimeout(() => {
      templateSearchDebounce = undefined;
      void sendTemplateSearch(token);
    }, delay);
  };

  const clearResourceAutocomplete = () => {
    setResourceRequestId(undefined);
    cancelTemplateSearchDebounce();
    setSelectedResourceIndex(0);
    setTemplateSearchError(undefined);
  };

  const cancelTemplateSearchDebounce = () => {
    if (templateSearchDebounce !== undefined) {
      window.clearTimeout(templateSearchDebounce);
      templateSearchDebounce = undefined;
    }
  };

  const rememberTextareaSelection = (textarea: HTMLTextAreaElement) => {
    setCaret({ start: textarea.selectionStart, end: textarea.selectionEnd });
    setResourceInputVersion((version) => version + 1);
  };

  const syncHighlightScroll = (textarea: HTMLTextAreaElement) => {
    if (!highlightLayer) {
      return;
    }

    highlightLayer.scrollTop = textarea.scrollTop;
    highlightLayer.scrollLeft = textarea.scrollLeft;
  };

  const selectNextResource = (offset: number) => {
    const options = visibleResourceOptions();
    if (options.length === 0) {
      return;
    }

    setSelectedResourceIndex((current) => (current + offset + options.length) % options.length);
  };

  const acceptResourceOption = (option: SessionTemplateOptionView | undefined) => {
    const token = activeResourceToken();
    const textarea = messageTextarea;

    if (!token || !option || !textarea) {
      return;
    }

    const value = draft();
    const nextValue = `${value.slice(0, token.start)}${option.template}${value.slice(token.end)}`;
    const nextCaret = token.start + option.template.length;
    setDraft(nextValue);
    clearResourceAutocomplete();

    queueMicrotask(() => {
      textarea.focus();
      textarea.setSelectionRange(nextCaret, nextCaret);
      setCaret({ start: nextCaret, end: nextCaret });
    });
  };

  const handleResourceAutocompleteKeyDown = (event: KeyboardEvent) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      void submit();
      return;
    }

    const options = visibleResourceOptions();
    if (options.length === 0) {
      return;
    }

    if (event.key === "ArrowDown") {
      event.preventDefault();
      selectNextResource(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      selectNextResource(-1);
    } else if (event.key === "Enter") {
      event.preventDefault();
      acceptResourceOption(options[selectedResourceIndex()]);
    } else if (event.key === "Escape") {
      event.preventDefault();
      clearResourceAutocomplete();
    }
  };

  const interrupt = async () => {
    setCommandError(undefined);
    try {
      await app.dispatchInterruptSession(props.session.id);
    } catch (error) {
      setCommandError(error instanceof Error ? error.message : "Unable to stop session.");
    }
  };

  const respond = async (requestId: string, response: unknown) => {
    setCommandError(undefined);
    try {
      await app.dispatchRespondExternalRequest(props.session.id, requestId, JSON.stringify(response));
    } catch (error) {
      setCommandError(error instanceof Error ? error.message : "Unable to answer prompt.");
    }
  };

  const rollbackTurn = async (turn: bigint) => {
    setCommandError(undefined);
    try {
      await app.dispatch({ type: "rollbackSessionToTurn", sessionId: props.session.id, turn });
    } catch (error) {
      setCommandError(error instanceof Error ? error.message : "Unable to roll back session.");
    }
  };

  const rollbackCursor = async (cursor: bigint) => {
    setCommandError(undefined);
    try {
      await app.dispatch({ type: "rollbackSessionToCursor", sessionId: props.session.id, cursor });
    } catch (error) {
      setCommandError(error instanceof Error ? error.message : "Unable to roll back response.");
    }
  };

  const continueTurn = async (turn: bigint) => {
    setCommandError(undefined);
    try {
      await app.dispatch({ type: "continueSessionFromTurn", sessionId: props.session.id, turn });
    } catch (error) {
      setCommandError(error instanceof Error ? error.message : "Unable to continue session.");
    }
  };

  const continueCursor = async (cursor: bigint) => {
    setCommandError(undefined);
    try {
      await app.dispatch({ type: "continueSessionFromCursor", sessionId: props.session.id, cursor });
    } catch (error) {
      setCommandError(error instanceof Error ? error.message : "Unable to rerun message.");
    }
  };

  const editUserMessage = async (item: TextTimelineItem) => {
    setCommandError(undefined);
    try {
      await app.dispatch({ type: "rollbackSessionBeforeCursor", sessionId: props.session.id, cursor: item.cursor });
      setDraft(retemplatizeUserMessage(item.text));
    } catch (error) {
      setCommandError(error instanceof Error ? error.message : "Unable to edit message.");
    }
  };

  const scrollTimelineToBottom = (scroller: HTMLElement) => {
    const scrollSize = import.meta.env.MODE === "test" ? scroller.scrollHeight : timelineVirtualizer?.scrollSize ?? scroller.scrollHeight;
    timelineVirtualizer?.scrollTo(scrollSize);
    scroller.scrollTop = scrollSize;
  };

  return (
    <div class="sessions__detail-page">
      <div
        class="sessions__detail-main"
        ref={timelineScroll}
        aria-label="Session timeline"
        onWheel={(event) => scheduleTimelineViewportUpdate(event.currentTarget)}
        onKeyDown={(event) => {
          if (isScrollKey(event.key)) {
            scheduleTimelineViewportUpdate(event.currentTarget);
          }
        }}
        onScroll={(event) => scheduleTimelineViewportUpdate(event.currentTarget)}
      >
        <Show when={sessions.deleteErrors()[props.session.id]}>
          {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
        </Show>
        <Show when={commandError()}>{(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}</Show>
        <Show
          when={items().length > 0}
          fallback={<p class="sessions__timeline-empty">No messages yet.</p>}
        >
          <Virtualizer
            as={TimelineVirtualizerList}
            item={TimelineVirtualizerItem}
            data={items()}
            shift={historyShift()}
            keepMounted={testKeepMountedIndexes()}
            scrollRef={timelineScroll}
            ref={(handle) => {
              timelineVirtualizer = handle;
            }}
            onScroll={(offset) => {
              if (timelineScroll) {
                timelineScroll.scrollTop = offset;
                scheduleTimelineViewportUpdate(timelineScroll);
              }
            }}
          >
            {(item) => (
              <div class={`sessions__timeline-item sessions__timeline-item--${item.kind}`} data-timeline-item-key={timelineItemKey(item)}>
                <TimelineItem
                  item={item}
                  expandedShellOutputs={expandedShellOutputs()}
                  onSetShellOutputExpanded={(id, expanded) => setExpandedShellOutputs((current) => ({ ...current, [id]: expanded }))}
                  onRespond={respond}
                  onInterrupt={interrupt}
                  showTurnActions={turnFooterItemIds().has(item.id)}
                  canRollbackTurn={lastTurn() !== undefined && item.turn < lastTurn()!}
                  onRollbackTurn={rollbackTurn}
                  onContinueTurn={continueTurn}
                  onContinueCursor={continueCursor}
                  onRollbackCursor={rollbackCursor}
                  onEditUserMessage={editUserMessage}
                />
              </div>
            )}
          </Virtualizer>
        </Show>
      </div>
      <div
        class={`sessions__detail-important-status sessions__detail-important-status--${importantStatus()?.tone ?? "empty"}`}
        aria-label="Session activity status"
        aria-live="polite"
      >
        <Show when={importantStatus()}>
          {(status) => (
            <>
              <span aria-hidden="true" /> {status().message}
            </>
          )}
        </Show>
      </div>
      <form
        class="sessions__composer"
        aria-label="Send session message"
        onSubmit={(event) => {
          event.preventDefault();
          void submit();
        }}
      >
        <div class="sessions__composer-input" onKeyDown={handleResourceAutocompleteKeyDown}>
          <Show when={visibleResourceOptions().length > 0}>
            <div class="sessions__resource-popover" role="listbox" aria-label="Resource suggestions">
              <For each={visibleResourceOptions()}>
                {(option, index) => (
                  <button
                    type="button"
                    role="option"
                    aria-selected={index() === selectedResourceIndex()}
                    class={`sessions__resource-option${index() === selectedResourceIndex() ? " is-selected" : ""}`}
                    onMouseDown={(event) => event.preventDefault()}
                    onClick={() => acceptResourceOption(option)}
                  >
                    <span class="sessions__resource-option-kind">{option.kind}</span>
                    <span class="sessions__resource-option-label">{option.label}</span>
                    <span class="sessions__resource-option-detail">{option.detail}</span>
                  </button>
                )}
              </For>
            </div>
          </Show>
          <div class="sessions__composer-textarea-shell">
            <div class="sessions__composer-highlight" aria-hidden="true" ref={highlightLayer}>
              {highlightResourceTokens(draft())}
            </div>
            <textarea
              ref={messageTextarea}
              aria-label="Session message"
              aria-autocomplete="list"
              aria-expanded={visibleResourceOptions().length > 0}
              placeholder="Send a message..."
              value={draft()}
              onInput={(event) => {
                setDraft(event.currentTarget.value);
                rememberTextareaSelection(event.currentTarget);
                syncHighlightScroll(event.currentTarget);
              }}
              onClick={(event) => rememberTextareaSelection(event.currentTarget)}
              onKeyUp={(event) => rememberTextareaSelection(event.currentTarget)}
              onSelect={(event) => rememberTextareaSelection(event.currentTarget)}
              onScroll={(event) => syncHighlightScroll(event.currentTarget)}
            />
          </div>
          <Show when={templateSearchPending() && visibleResourceOptions().length === 0}>
            <p class="sessions__resource-status">Searching resources...</p>
          </Show>
          <Show when={templateSearchMessage()}>
            {(message) => <p class="sessions__resource-status sessions__resource-status--error">{message()}</p>}
          </Show>
        </div>
        <div class="sessions__composer-actions">
          <button
            type="button"
            class={`sessions__composer-stop${active() ? "" : " sessions__composer-stop--hidden"}`}
            disabled={!active()}
            aria-hidden={!active()}
            tabIndex={active() ? 0 : -1}
            onClick={() => void interrupt()}
          >
            Stop
          </button>
          <button type="submit" disabled={!draft().trim() || !timelineReady()}>
            Send
          </button>
        </div>
      </form>
      <SessionDetailStatusBar
        session={props.session}
        contextTokens={latestContextTokens(latestMetrics())}
        onEditProvider={() => setEditingRuntime("provider")}
        onEditMcp={() => setEditingRuntime("mcp")}
      />
      <Show when={editingRuntime() === "provider"}>
        <SessionProviderDialog session={props.session} onClose={() => setEditingRuntime(undefined)} />
      </Show>
      <Show when={editingRuntime() === "mcp"}>
        <SessionMcpDialog session={props.session} onClose={() => setEditingRuntime(undefined)} />
      </Show>
    </div>
  );
}

type TimelineItemModel =
  | TextTimelineItem
  | ToolTimelineItem
  | TurnMetricsTimelineItem
  | ExternalPromptTimelineItem;

type TextTimelineItem = { id: string; turn: bigint; cursor: bigint; kind: "user" | "assistant" | "thinking"; text: string };
type TemplateTextPart = TemplatePlainTextPart | TemplateResultPart | TemplateErrorPart;
type TemplatePlainTextPart = { kind: "text"; text: string };
type TemplateResultPart = {
  kind: "result";
  raw: string;
  kindName: string;
  tool: string;
  source: string;
  arguments: string;
  content: string;
};
type TemplateErrorPart = { kind: "error"; raw: string; message: string };
type ActiveResourceToken = { start: number; end: number; query: string };
type ExternalPromptTimelineItem = {
  id: string;
  turn: bigint;
  cursor: bigint;
  openCursor: bigint;
  kind: "permission" | "question" | "prompt";
  request: ExternalRequestView;
  resolved: boolean;
};

type ToolInputEntry = { key: string; value: string };
type ToolState = "loading" | "complete" | "error";
type ToolTimelineItem = {
  id: string;
  turn: bigint;
  cursor: bigint;
  kind: "tool";
  name: string;
  state: ToolState;
  input: Record<string, unknown>;
  inputEntries: ToolInputEntry[];
  resultContent: string;
  progressInfo: string;
  progressError: string;
  taskProgress?: TaskProgress;
};

type TaskProgress = {
  description: string;
  taskId: string;
  mode: string;
  status: string;
  runId: bigint | null;
  disposition: string;
  queuedCount: bigint;
  metrics?: TaskProgressMetrics;
};

type TaskProgressMetrics = {
  contextTokens: bigint;
  totalInputTokens: bigint;
  outputTokens: bigint;
  totalTokens: bigint;
  costCurrency: string | null;
  totalCostMicros: bigint;
};

type TurnMetricsTimelineItem = {
  id: string;
  turn: bigint;
  cursor: bigint;
  kind: "metrics";
  inputTokens: bigint;
  outputTokens: bigint;
  cacheReadTokens: bigint;
  cacheWriteTokens: bigint;
  imageInputTokens: bigint;
  imageInputCount: bigint;
  totalTokens: bigint;
  totalInputTokens: bigint;
  contextTokens: bigint;
  costCurrency: string | null;
  totalCostMicros: bigint;
  addedInputTokens: bigint | null;
  addedInputCacheReadTokens: bigint | null;
  addedInputBaseTokens: bigint | null;
};

function resourceTokenAtCaret(text: string, selectionStart: number, selectionEnd: number): ActiveResourceToken | undefined {
  if (selectionStart !== selectionEnd) {
    return undefined;
  }

  let start = selectionStart;
  while (start > 0 && !isWhitespace(text[start - 1])) {
    start -= 1;
  }

  if (text[start] !== "@" || selectionStart <= start + 1) {
    return undefined;
  }

  const query = text.slice(start + 1, selectionStart);
  if (!query || hasWhitespace(query)) {
    return undefined;
  }

  let end = selectionStart;
  while (end < text.length && !isWhitespace(text[end])) {
    end += 1;
  }

  return { start, end, query };
}

function highlightResourceTokens(text: string): JSX.Element[] {
  const nodes: JSX.Element[] = [];
  let offset = 0;
  for (const token of resourceTemplateTokenRanges(text)) {
    const tokenStart = token.start;

    if (tokenStart > offset) {
      nodes.push(text.slice(offset, tokenStart));
    }

    nodes.push(<span class="sessions__composer-resource-token">{text.slice(token.start, token.end)}</span>);
    offset = token.end;
  }

  if (offset < text.length) {
    nodes.push(text.slice(offset));
  }

  return nodes.length > 0 ? nodes : [text];
}

function renderTemplateInline(text: string): JSX.Element[] {
  return parseTemplateText(text).flatMap((part) => {
    if (part.kind === "text") {
      return highlightResourceTokens(part.text);
    }

    if (part.kind === "error") {
      return [<span class="sessions__template-error-token">{part.message}</span>];
    }

    if (part.source) {
      return highlightResourceTokens(fileTemplateReference(part.source));
    }

    if (part.tool) {
      return highlightResourceTokens(`@tool(${part.tool}, ${part.arguments || "{}"})`);
    }

    return [part.raw];
  });
}

function resourceTemplateTokenRanges(text: string): Array<{ start: number; end: number }> {
  const ranges: Array<{ start: number; end: number }> = [];
  let index = 0;
  while (index < text.length) {
    const start = text.indexOf("@", index);
    if (start === -1) {
      break;
    }
    if (start > 0 && !isWhitespace(text[start - 1])) {
      index = start + 1;
      continue;
    }

    const end = resourceTemplateTokenEnd(text, start);
    if (end === undefined || end <= start + 1) {
      index = start + 1;
      continue;
    }
    ranges.push({ start, end });
    index = end;
  }
  return ranges;
}

function resourceTemplateTokenEnd(text: string, start: number) {
  if (text.startsWith('@"', start)) {
    return quotedResourceTokenEnd(text, start + 2);
  }
  if (text.startsWith("@tool(", start)) {
    return balancedToolTokenEnd(text, start + "@tool(".length);
  }

  let end = start + 1;
  while (end < text.length && !isWhitespace(text[end])) {
    end += 1;
  }
  return end;
}

function quotedResourceTokenEnd(text: string, start: number) {
  let escaped = false;
  for (let index = start; index < text.length; index += 1) {
    const character = text[index];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (character === "\\") {
      escaped = true;
      continue;
    }
    if (character === '"') {
      return index + 1;
    }
  }
  return undefined;
}

function balancedToolTokenEnd(text: string, start: number) {
  let depth = 1;
  let quote: string | undefined;
  let escaped = false;
  for (let index = start; index < text.length; index += 1) {
    const character = text[index];
    if (escaped) {
      escaped = false;
      continue;
    }
    if (character === "\\") {
      escaped = true;
      continue;
    }
    if (quote) {
      if (character === quote) {
        quote = undefined;
      }
      continue;
    }
    if (character === '"' || character === "'") {
      quote = character;
      continue;
    }
    if (character === "(") {
      depth += 1;
    } else if (character === ")") {
      depth -= 1;
      if (depth === 0) {
        return index + 1;
      }
    }
  }
  return undefined;
}

function hasWhitespace(value: string) {
  return /\s/.test(value);
}

function isWhitespace(value: string | undefined) {
  return value !== undefined && /\s/.test(value);
}

function TimelineVirtualizerList(props: CustomContainerComponentProps) {
  return (
    <ol ref={props.ref} class="sessions__timeline" aria-label="Timeline messages" style={props.style}>
      {props.children}
    </ol>
  );
}

function TimelineVirtualizerItem(props: CustomItemComponentProps) {
  const style = () => ({
    ...props.style,
    visibility: import.meta.env.MODE === "test" ? undefined : props.style.visibility,
  });

  return (
    <li ref={props.ref} style={style()}>
      <div class="sessions__timeline-virtual-item">
        {props.children}
      </div>
    </li>
  );
}

function TimelineItem(props: {
  item: TimelineItemModel;
  expandedShellOutputs: Record<string, boolean>;
  onSetShellOutputExpanded: (id: string, expanded: boolean) => void;
  onRespond: (requestId: string, response: unknown) => Promise<void>;
  onInterrupt: () => Promise<void>;
  showTurnActions: boolean;
  canRollbackTurn: boolean;
  onRollbackTurn: (turn: bigint) => Promise<void>;
  onContinueTurn: (turn: bigint) => Promise<void>;
  onContinueCursor: (cursor: bigint) => Promise<void>;
  onRollbackCursor: (cursor: bigint) => Promise<void>;
  onEditUserMessage: (item: TextTimelineItem) => Promise<void>;
}) {
  const item = () => props.item;

  return (
    <>
      <Switch>
        <Match when={textTimelineItem(item())}>
          {(textItem) => <TextTimelineItemView item={textItem()} />}
        </Match>
        <Match when={toolTimelineItem(item())}>
          {(toolItem) => (
            <ToolTimelineItemView
              item={toolItem()}
              expandedShellOutputs={props.expandedShellOutputs}
              onSetShellOutputExpanded={props.onSetShellOutputExpanded}
            />
          )}
        </Match>
        <Match when={metricsTimelineItem(item())}>
          {(metricsItem) => <TurnMetricsTimelineItemView item={metricsItem()} />}
        </Match>
        <Match when={permissionTimelineItem(item())}>
          {(promptItem) => <ExternalRequestTimelineItem item={promptItem()} onRespond={props.onRespond} onInterrupt={props.onInterrupt} />}
        </Match>
        <Match when={questionTimelineItem(item())}>
          {(promptItem) => <ExternalRequestTimelineItem item={promptItem()} onRespond={props.onRespond} onInterrupt={props.onInterrupt} />}
        </Match>
        <Match when={externalTimelineItem(item())}>
          {(promptItem) => <ExternalRequestTimelineItem item={promptItem()} onRespond={props.onRespond} onInterrupt={props.onInterrupt} />}
        </Match>
      </Switch>
      <TimelineItemActions
        item={item()}
        showTurnActions={props.showTurnActions}
        canRollbackTurn={props.canRollbackTurn}
        onRollbackTurn={props.onRollbackTurn}
        onContinueTurn={props.onContinueTurn}
        onContinueCursor={props.onContinueCursor}
        onRollbackCursor={props.onRollbackCursor}
        onEditUserMessage={props.onEditUserMessage}
      />
    </>
  );
}

function TimelineItemActions(props: {
  item: TimelineItemModel;
  showTurnActions: boolean;
  canRollbackTurn: boolean;
  onRollbackTurn: (turn: bigint) => Promise<void>;
  onContinueTurn: (turn: bigint) => Promise<void>;
  onContinueCursor: (cursor: bigint) => Promise<void>;
  onRollbackCursor: (cursor: bigint) => Promise<void>;
  onEditUserMessage: (item: TextTimelineItem) => Promise<void>;
}) {
  const canReAnswer = () => isExternalPromptItem(props.item) && props.item.resolved && (props.item.kind === "permission" || props.item.kind === "question");
  const userItem = () => (props.item.kind === "user" ? props.item : undefined);

  return (
    <Show when={props.showTurnActions || canReAnswer() || userItem()}>
      <footer class="sessions__timeline-actions">
        <Show when={userItem()}>
          {(item) => (
            <>
              <button type="button" onClick={() => void props.onContinueCursor(item().cursor)}>
                Rerun
              </button>
              <button type="button" onClick={() => void props.onEditUserMessage(item())}>
                Edit
              </button>
            </>
          )}
        </Show>
        <Show when={canReAnswer()}>
          <button type="button" onClick={() => void props.onRollbackCursor((props.item as ExternalPromptTimelineItem).openCursor)}>
            Re-answer
          </button>
        </Show>
        <Show when={props.showTurnActions && props.canRollbackTurn}>
          <button type="button" onClick={() => void props.onRollbackTurn(props.item.turn)}>
            Roll back
          </button>
        </Show>
        <Show when={props.showTurnActions}>
          <button type="button" onClick={() => void props.onContinueTurn(props.item.turn)}>
            Continue
          </button>
        </Show>
      </footer>
    </Show>
  );
}

function TurnMetricsTimelineItemView(props: { item: TurnMetricsTimelineItem }) {
  return (
    <article class="sessions__timeline-metrics" aria-label="Turn token metrics">
      <span title={`Input ${formatTokenCount(props.item.totalInputTokens)} / output ${formatTokenCount(props.item.outputTokens)}`}>{formatTokenCount(props.item.contextTokens)} tok</span>
      <span title="Added input on the next request">+{props.item.addedInputTokens === null ? "-" : formatTokenCount(props.item.addedInputTokens)} in</span>
      <span title="Cache-hit rate on the next request">{props.item.addedInputBaseTokens === null || props.item.addedInputCacheReadTokens === null ? "-" : formatCacheRate(props.item.addedInputCacheReadTokens, props.item.addedInputBaseTokens)} hit</span>
      <span>{formatCostMicros(props.item.totalCostMicros, props.item.costCurrency)}</span>
    </article>
  );
}

function TextTimelineItemView(props: { item: Extract<TimelineItemModel, { kind: "user" | "assistant" | "thinking" }> }) {
  return hasTemplateMarkers(props.item.text)
    ? <div class={`sessions__markdown sessions__markdown--${props.item.kind} sessions__template-inline`}>{renderTemplateInline(props.item.text)}</div>
    : <MarkdownMessage text={props.item.text} tone={props.item.kind} />;
}

function ToolTimelineItemView(props: {
  item: ToolTimelineItem;
  expandedShellOutputs: Record<string, boolean>;
  onSetShellOutputExpanded: (id: string, expanded: boolean) => void;
}) {
  return (
    <Switch fallback={<GenericToolTimelineItem item={props.item} />}>
      <Match when={props.item.name === "shell"}>
        <ShellToolTimelineItem
          item={props.item}
          expanded={props.expandedShellOutputs[props.item.id] ?? false}
          onSetExpanded={(expanded) => props.onSetShellOutputExpanded(props.item.id, expanded)}
        />
      </Match>
      <Match when={props.item.name === "task"}>
        <TaskToolTimelineItem item={props.item} />
      </Match>
      <Match when={props.item.name === "edit"}>
        <EditToolTimelineItem item={props.item} />
      </Match>
      <Match when={props.item.name === "apply_patch"}>
        <PatchToolTimelineItem item={props.item} />
      </Match>
    </Switch>
  );
}

function GenericToolTimelineItem(props: { item: ToolTimelineItem }) {
  return (
    <p class="sessions__timeline-tool">
      {toolLabel(props.item.name, props.item.inputEntries)}
      <ToolStateText state={props.item.state} />
    </p>
  );
}

function ShellToolTimelineItem(props: { item: ToolTimelineItem; expanded: boolean; onSetExpanded: (expanded: boolean) => void }) {
  const command = () => stringInput(props.item.input.command) || "(empty command)";
  const description = () => stringInput(props.item.input.description) || "Shell command";
  const output = () => props.item.progressInfo || props.item.progressError
    ? `${props.item.progressInfo}${props.item.progressError}`
    : props.item.resultContent;
  const outputLines = createMemo(() => {
    const lines = output().split("\n");
    return lines.at(-1) === "" ? lines.slice(0, -1) : lines;
  });
  const canCollapse = () => outputLines().length > SHELL_OUTPUT_RECENT_LINE_COUNT;
  const expanded = () => props.expanded;
  const visibleOutput = () => canCollapse() && !expanded()
    ? outputLines().slice(-SHELL_OUTPUT_RECENT_LINE_COUNT).join("\n")
    : output();
  const toggleOutput = () => {
    if (canCollapse()) {
      props.onSetExpanded(!expanded());
    }
  };
  const toggleOutputFromKey = (event: KeyboardEvent) => {
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      toggleOutput();
    }
  };

  return (
    <section class={`sessions__shell-tool sessions__shell-tool--${props.item.state}`} aria-label="Shell tool">
      <p class="sessions__shell-tool-header">
        <strong>{description()}</strong>
        <ToolStateText state={props.item.state} />
      </p>
      <CodeBlock code={`$ ${command()}`} language="bash" label="command" class="sessions__shell-command" />
      <Show when={output()}>
        <div
          class={`sessions__shell-output${canCollapse() && !expanded() ? " sessions__shell-output--collapsed" : ""}`}
          role={canCollapse() ? "button" : undefined}
          tabIndex={canCollapse() ? 0 : undefined}
          aria-label={canCollapse() ? `Shell output ${expanded() ? "expanded" : `truncated to last ${SHELL_OUTPUT_RECENT_LINE_COUNT} of ${outputLines().length} lines`}. Click to ${expanded() ? "collapse" : "expand"}.` : undefined}
          onClick={toggleOutput}
          onKeyDown={toggleOutputFromKey}
        >
          <Show when={canCollapse() && !expanded()}>
            <span class="sessions__shell-output-expand">... click to expand</span>
          </Show>
          <pre class="sessions__tool-terminal"><code>{visibleOutput()}</code></pre>
        </div>
      </Show>
    </section>
  );
}

function TaskToolTimelineItem(props: { item: ToolTimelineItem }) {
  const progress = () => props.item.taskProgress;
  const title = () => progress()?.description || stringInput(props.item.input.description) || "Task";
  const status = () => progress()?.status || taskResultStatus(props.item.resultContent) || (props.item.state === "loading" ? "running" : props.item.state);
  const mode = () => progress()?.mode || stringInput(props.item.input.mode) || "sync";
  const metrics = () => progress()?.metrics;

  return (
    <section class={`sessions__task-tool sessions__task-tool--${props.item.state}`} aria-label="Task tool">
      <div class="sessions__task-tool-header">
        <strong>{title()}</strong>
        <span>{mode()}</span>
      </div>
      <p class="sessions__task-tool-status">
        <span>{status()}</span><ToolStateText state={props.item.state} />
        <Show when={metrics()}>
          {(taskMetrics) => (
            <span class="sessions__task-tool-metrics" aria-label="Task token metrics">
              <span title={`Input ${formatTokenCount(taskMetrics().totalInputTokens)} / output ${formatTokenCount(taskMetrics().outputTokens)}`}>{formatTokenCount(taskMetrics().contextTokens)} tok</span>
              <span>{formatCostMicros(taskMetrics().totalCostMicros, taskMetrics().costCurrency)}</span>
            </span>
          )}
        </Show>
      </p>
    </section>
  );
}

function EditToolTimelineItem(props: { item: ToolTimelineItem }) {
  const filePath = () => stringInput(props.item.input.filePath) || "Unknown file";
  const oldString = () => stringInput(props.item.input.oldString);
  const newString = () => stringInput(props.item.input.newString);
  const language = () => languageFromFilePath(filePath());

  return (
    <section class={`sessions__file-tool sessions__file-tool--${props.item.state}`} aria-label="Edit tool">
      <div class="sessions__file-tool-header">
        <strong>edit</strong>
        <ToolStateText state={props.item.state} />
      </div>
      <ToolFiles files={[{ status: "M", path: filePath() }]} />
      <div class="sessions__tool-diff" role="region" aria-label={`Diff for ${filePath()}`}>
        <DiffLine kind="file" text={`--- ${filePath()}`} />
        <DiffLine kind="file" text={`+++ ${filePath()}`} />
        <For each={diffTextLines(oldString(), "remove")}>
          {(line) => <DiffLine kind={line.kind} text={line.text} language={language()} />}
        </For>
        <For each={diffTextLines(newString(), "add")}>
          {(line) => <DiffLine kind={line.kind} text={line.text} language={language()} />}
        </For>
      </div>
    </section>
  );
}

function PatchToolTimelineItem(props: { item: ToolTimelineItem }) {
  const patchText = () => stringInput(props.item.input.patchText);
  const files = () => patchFiles(patchText());

  return (
    <section class={`sessions__file-tool sessions__file-tool--${props.item.state}`} aria-label="Patch tool">
      <div class="sessions__file-tool-header">
        <strong>apply_patch</strong>
        <ToolStateText state={props.item.state} />
      </div>
      <ToolFiles files={files()} />
      <div class="sessions__tool-diff" role="region" aria-label="Patch diff">
        <For each={patchDiffLines(patchText())}>
          {(line) => <DiffLine kind={line.kind} text={line.text} language={line.language} />}
        </For>
      </div>
      <Show when={props.item.resultContent}>
        <p class="sessions__tool-result">{props.item.resultContent}</p>
      </Show>
    </section>
  );
}

function ToolStateText(props: { state: ToolState }) {
  return (
    <>
      <Show when={props.state === "loading"}>
        {" "}<span class="sessions__timeline-tool-loading">loading...<span aria-hidden="true" /></span>
      </Show>
      <Show when={props.state === "error"}> <span class="sessions__tool-state-error">error</span></Show>
    </>
  );
}

function ToolFiles(props: { files: Array<{ status: string; path: string }> }) {
  return (
    <ul class="sessions__tool-files" aria-label="Files modified">
      <For each={props.files} fallback={<li><span>?</span> Unknown files</li>}>
        {(file) => <li><span>{file.status}</span> {file.path}</li>}
      </For>
    </ul>
  );
}

function DiffLine(props: { kind: "add" | "remove" | "file" | "meta" | "context"; text: string; language?: string }) {
  const parts = () => diffLineParts(props.text, props.kind);
  const highlight = () => props.language && (props.kind === "add" || props.kind === "remove" || props.kind === "context");
  return (
    <div class={`sessions__tool-diff-line sessions__tool-diff-line--${props.kind}`}>
      <span class="sessions__tool-diff-prefix" aria-hidden="true">{parts().prefix}</span>
      <span class="sessions__tool-diff-code">
        <Show
          when={highlight()}
          fallback={parts().code}
        >
          <HighlightedCode code={parts().code} language={props.language} />
        </Show>
      </span>
    </div>
  );
}

function textTimelineItem(item: TimelineItemModel) {
  return item.kind === "user" || item.kind === "assistant" || item.kind === "thinking" ? item : undefined;
}

function toolTimelineItem(item: TimelineItemModel) {
  return item.kind === "tool" ? item : undefined;
}

function permissionTimelineItem(item: TimelineItemModel) {
  return item.kind === "permission" ? item : undefined;
}

function questionTimelineItem(item: TimelineItemModel) {
  return item.kind === "question" ? item : undefined;
}

function externalTimelineItem(item: TimelineItemModel) {
  return item.kind === "prompt" ? item : undefined;
}

function metricsTimelineItem(item: TimelineItemModel) {
  return item.kind === "metrics" ? item : undefined;
}

function latestTurnMetrics(items: TimelineItemModel[]) {
  for (let index = items.length - 1; index >= 0; index -= 1) {
    const item = metricsTimelineItem(items[index]!);
    if (item) {
      return item;
    }
  }

  return undefined;
}

function latestContextTokens(item: TurnMetricsTimelineItem | undefined) {
  return item ? item.contextTokens : 0n;
}

function ExternalRequestTimelineItem(props: {
  item: Extract<TimelineItemModel, { kind: "permission" | "question" | "prompt" }>;
  onRespond: (requestId: string, response: unknown) => Promise<void>;
  onInterrupt: () => Promise<void>;
}) {
  return (
    <Show when={!props.item.resolved} fallback={<ExternalRequestResult item={props.item} />}>
      <Switch>
        <Match when={props.item.kind === "permission"}>
          <PermissionPrompt request={props.item.request} onRespond={props.onRespond} onInterrupt={props.onInterrupt} />
        </Match>
        <Match when={props.item.kind === "question"}>
          <QuestionPrompt request={props.item.request} onRespond={props.onRespond} />
        </Match>
        <Match when={props.item.kind === "prompt"}>
          <ExternalPrompt request={props.item.request} />
        </Match>
      </Switch>
    </Show>
  );
}

function ExternalRequestResult(props: { item: Extract<TimelineItemModel, { kind: "permission" | "question" | "prompt" }> }) {
  return (
    <p class="sessions__timeline-tool sessions__timeline-external-result">
      {externalRequestLabel(props.item.kind)}: answered
    </p>
  );
}

function PermissionPrompt(props: {
  request: ExternalRequestView;
  onRespond: (requestId: string, response: unknown) => Promise<void>;
  onInterrupt: () => Promise<void>;
}) {
  const [message, setMessage] = createSignal("");
  const [confirmingAlways, setConfirmingAlways] = createSignal(false);
  const payload = createMemo(() => parseJsonRecord(props.request.payloadJson));
  const toolCall = createMemo(() => asRecord(payload().tool_call));
  const toolName = createMemo(() => String(toolCall()?.name ?? "Tool call"));
  const toolArguments = createMemo(() => JSON.stringify(toolCall()?.arguments ?? {}, null, 2));
  const patterns = createMemo(() => permissionPatterns(payload()));
  const patternLabel = createMemo(() => JSON.stringify(patterns(), null, 2));

  const deny = (mode: "return_tool_result" | "cancel_work") => ({
    decision: "deny",
    message: message().trim() || null,
    mode,
  });

  return (
    <section class="sessions__prompt sessions__prompt--permission" aria-label="Permission prompt">
      <header class="sessions__prompt-header">
        <div>
          <p class="sessions__prompt-kicker">Permission required</p>
          <h2>Permission request</h2>
          <p class="sessions__prompt-subtitle">The session wants to run a tool before it continues.</p>
        </div>
        <code class="sessions__prompt-id">{props.request.id}</code>
      </header>

      <div class="sessions__prompt-detail-card">
        <div class="sessions__prompt-detail-row">
          <span>Tool</span>
          <strong>{toolName()}</strong>
        </div>
        <details>
          <summary>Tool arguments</summary>
          <pre>{toolArguments()}</pre>
        </details>
        <Show when={patterns().length > 0}>
          <details open={confirmingAlways()}>
            <summary>Always approval pattern</summary>
            <pre>{patternLabel()}</pre>
          </details>
        </Show>
      </div>

      <div class="sessions__prompt-actions sessions__prompt-actions--primary">
        <button class="sessions__prompt-button--primary" type="button" onClick={() => void props.onRespond(props.request.id, { decision: "allow_once" })}>
          Allow once
        </button>
        <Show
          when={confirmingAlways()}
          fallback={
            <button class="sessions__prompt-button--secondary" type="button" disabled={patterns().length === 0} onClick={() => setConfirmingAlways(true)}>
              Allow always
            </button>
          }
        >
          <button
            class="sessions__prompt-button--primary"
            type="button"
            disabled={patterns().length === 0}
            onClick={() => void props.onRespond(props.request.id, { decision: "allow_always", patterns: patterns() })}
          >
            Confirm always
          </button>
        </Show>
      </div>

      <div class="sessions__prompt-deny">
        <label class="sessions__prompt-field sessions__prompt-field--stacked">
          <span>Denial message</span>
          <textarea value={message()} onInput={(event) => setMessage(event.currentTarget.value)} />
        </label>
        <div class="sessions__prompt-actions">
          <button class="sessions__prompt-button--danger" type="button" onClick={() => void props.onRespond(props.request.id, deny("return_tool_result"))}>
            Deny and resume
          </button>
          <button
            class="sessions__prompt-button--danger"
            type="button"
            onClick={() => {
              void props.onRespond(props.request.id, deny("cancel_work")).then(() => props.onInterrupt());
            }}
          >
            Deny with interrupt
          </button>
        </div>
      </div>
    </section>
  );
}

function QuestionPrompt(props: {
  request: ExternalRequestView;
  onRespond: (requestId: string, response: unknown) => Promise<void>;
}) {
  const payload = createMemo(() => parseJsonRecord(props.request.payloadJson));
  const questions = createMemo(() => asArray(payload().questions).map(normalizeQuestion));
  const [answers, setAnswers] = createSignal<Record<string, { selections: string[]; custom: string }>>({});

  const answerFor = (header: string) => answers()[header] ?? { selections: [], custom: "" };
  const setSelection = (header: string, label: string, multiple: boolean, selected: boolean) => {
    setAnswers((current) => {
      const answer = current[header] ?? { selections: [], custom: "" };
      const selections = multiple
        ? selected
          ? [...answer.selections, label]
          : answer.selections.filter((selection) => selection !== label)
        : selected
          ? [label]
          : [];

      return { ...current, [header]: { ...answer, selections } };
    });
  };
  const setCustom = (header: string, custom: string) => {
    setAnswers((current) => ({
      ...current,
      [header]: { ...(current[header] ?? { selections: [], custom: "" }), custom },
    }));
  };
  const submit = () => ({
    answers: questions().map((question) => {
      const answer = answerFor(question.header);
      return {
        header: question.header,
        selections: answer.selections,
        ...(answer.custom.trim() ? { custom: answer.custom.trim() } : {}),
      };
    }),
  });

  return (
    <section class="sessions__prompt sessions__prompt--question" aria-label="Question prompt">
      <header class="sessions__prompt-header">
        <div>
          <p class="sessions__prompt-kicker">Question</p>
          <h2>Answer required</h2>
          <p class="sessions__prompt-subtitle">The session is waiting for your input before it resumes.</p>
        </div>
        <code class="sessions__prompt-id">{props.request.id}</code>
      </header>
      <For each={questions()}>
        {(question) => (
          <fieldset class="sessions__question">
            <legend>{question.header}</legend>
            <p>{question.question}</p>
            <div class="sessions__question-options">
              <For each={question.options}>
                {(option) => (
                  <label class="sessions__question-option">
                    <input
                      type={question.multiple ? "checkbox" : "radio"}
                      name={`${props.request.id}-${question.header}`}
                      checked={answerFor(question.header).selections.includes(option.label)}
                      onChange={(event) => setSelection(question.header, option.label, question.multiple, event.currentTarget.checked)}
                    />
                    <span>{option.label}</span>
                    <small>{option.description}</small>
                  </label>
                )}
              </For>
            </div>
            <Show when={question.custom}>
              <label class="sessions__prompt-field sessions__prompt-field--stacked">
                <span>Custom answer</span>
                <textarea
                  value={answerFor(question.header).custom}
                  onInput={(event) => setCustom(question.header, event.currentTarget.value)}
                />
              </label>
            </Show>
          </fieldset>
        )}
      </For>
      <div class="sessions__prompt-actions sessions__prompt-actions--primary">
        <button class="sessions__prompt-button--primary" type="button" onClick={() => void props.onRespond(props.request.id, submit())}>
          Resume session
        </button>
      </div>
    </section>
  );
}

function ExternalPrompt(props: { request: ExternalRequestView }) {
  return (
    <section class="sessions__prompt" aria-label="External prompt">
      <p class="sessions__prompt-kicker">External request</p>
      <pre>{props.request.payloadJson}</pre>
    </section>
  );
}

function timelineItems(timeline: SessionTimelineView | undefined): TimelineItemModel[] {
  const items: TimelineItemModel[] = [];
  const textByItem = new Map<string, Extract<TimelineItemModel, { kind: "user" | "assistant" | "thinking" }>>();
  const toolById = new Map<string, ToolTimelineItem>();
  const toolIdByItemId = new Map<string, string>();
  const externalById = new Map<string, ExternalPromptTimelineItem>();
  const metricItems: TurnMetricsTimelineItem[] = [];

  for (const event of timeline?.events ?? []) {
    const channel = event.channel ?? "";
    switch (event.content.kind) {
      case "textDelta":
      case "textSnapshot": {
        const kind = textKind(channel);
        if (!kind) {
          appendToolProgress(toolById, toolIdByItemId, event.itemId, event.parentId, channel, event.content.kind === "textSnapshot" ? event.content.text : event.content.delta, event.content.kind);
          break;
        }

        const existing = textByItem.get(event.itemId);
        const text = event.content.kind === "textSnapshot"
          ? event.content.text
          : `${existing?.text ?? ""}${event.content.delta}`;
        if (existing) {
          existing.text = text;
          existing.cursor = event.cursor;
        } else {
          const item = { id: event.itemId, turn: event.turn, cursor: event.cursor, kind, text };
          textByItem.set(event.itemId, item);
          items.push(item);
        }
        break;
      }
      case "toolCall": {
        const id = event.content.id || event.itemId;
        toolIdByItemId.set(event.itemId, id);
        upsertToolItem(items, toolById, id, event.turn, event.cursor, event.content.name, "loading", parseJsonRecord(event.content.arguments_json), undefined);
        break;
      }
      case "toolResult":
        upsertToolItem(
          items,
          toolById,
          event.content.id || event.itemId,
          event.turn,
          event.cursor,
          event.content.name,
          event.content.error ? "error" : "complete",
          undefined,
          event.content.content,
        );
        break;
      case "externalRequest": {
        const request = event.content.request;
        if (!request) {
          break;
        }

        upsertExternalRequestItem(
          items,
          externalById,
          event.itemId,
          event.turn,
          event.cursor,
          request,
          event.kind === "externalRequestOpened",
          event.kind === "externalRequestResolved" || event.status === "resolved",
        );
        break;
      }
      case "json": {
        if (channel === "metrics") {
          const item = turnMetricsItem(event.itemId, event.turn, event.cursor, event.content.value_json);
          if (item) {
            metricItems.push(item);
            items.push(item);
          }
        } else if (channel === "tool_progress_info") {
          appendTaskProgress(toolById, toolIdByItemId, event.itemId, event.parentId, event.content.value_json);
        }
        break;
      }
      default:
        break;
    }
  }

  for (let index = 0; index < metricItems.length - 1; index += 1) {
    const current = metricItems[index]!;
    const next = metricItems[index + 1]!;
    current.addedInputTokens = next.totalInputTokens > current.totalInputTokens ? next.totalInputTokens - current.totalInputTokens : 0n;
    current.addedInputCacheReadTokens = next.cacheReadTokens;
    current.addedInputBaseTokens = next.totalInputTokens;
  }

  return items;
}

function turnMetricsItem(id: string, turn: bigint, cursor: bigint, valueJson: string): TurnMetricsTimelineItem | undefined {
  const payload = parseJsonRecord(valueJson);
  if (payload.kind !== "turn_metrics") {
    return undefined;
  }

  const usage = asRecord(payload.usage) ?? {};
  const cost = asRecord(payload.cost);
  const inputTokens = asBigInt(usage.inputTokens);
  const outputTokens = asBigInt(usage.outputTokens);
  const cacheReadTokens = asBigInt(usage.cacheReadTokens);
  const cacheWriteTokens = asBigInt(usage.cacheWriteTokens);
  const imageInputTokens = asBigInt(usage.imageInputTokens);
  const totalInputTokens = asBigInt(usage.totalInputTokens) || inputTokens + cacheReadTokens + cacheWriteTokens + imageInputTokens;
  const contextTokens = asBigInt(usage.contextTokens) || totalInputTokens + outputTokens;
  return {
    id,
    turn,
    cursor,
    kind: "metrics",
    inputTokens,
    outputTokens,
    cacheReadTokens,
    cacheWriteTokens,
    imageInputTokens,
    imageInputCount: asBigInt(usage.imageInputCount),
    totalTokens: asBigInt(usage.totalTokens),
    totalInputTokens,
    contextTokens,
    costCurrency: typeof cost?.currency === "string" ? cost.currency : null,
    totalCostMicros: asBigInt(cost?.totalMicros),
    addedInputTokens: null,
    addedInputCacheReadTokens: null,
    addedInputBaseTokens: null,
  };
}

function upsertToolItem(
  items: TimelineItemModel[],
  toolById: Map<string, ToolTimelineItem>,
  id: string,
  turn: bigint,
  cursor: bigint,
  name: string,
  state: ToolState,
  input: Record<string, unknown> | undefined,
  resultContent: string | undefined,
) {
  const existing = toolById.get(id);
  if (existing) {
    existing.turn = turn;
    existing.cursor = cursor;
    existing.name = name || existing.name;
    existing.state = state;
    existing.input = input ?? existing.input;
    existing.inputEntries = input ? topLevelInputEntries(input) : existing.inputEntries;
    existing.resultContent = resultContent ?? existing.resultContent;
    return;
  }

  const item = {
    id,
    turn,
    cursor,
    kind: "tool" as const,
    name,
    state,
    input: input ?? {},
    inputEntries: input ? topLevelInputEntries(input) : [],
    resultContent: resultContent ?? "",
    progressInfo: "",
    progressError: "",
  };
  toolById.set(id, item);
  items.push(item);
}

function appendTaskProgress(
  toolById: Map<string, ToolTimelineItem>,
  toolIdByItemId: Map<string, string>,
  itemId: string,
  parentId: string | null,
  valueJson: string,
) {
  const progress = taskProgress(valueJson);
  if (!progress) {
    return;
  }

  const toolItemId = parentId ?? itemId;
  const tool = toolById.get(toolIdByItemId.get(toolItemId) ?? toolItemId);
  if (!tool) {
    return;
  }
  tool.taskProgress = progress;
}

function taskProgress(valueJson: string): TaskProgress | undefined {
  const payload = parseJsonRecord(valueJson);
  if (payload.kind !== "task_progress") {
    return undefined;
  }
  const metrics = taskProgressMetrics(asRecord(payload.metrics));
  return {
    description: stringInput(payload.description),
    taskId: stringInput(payload.taskId),
    mode: stringInput(payload.mode),
    status: stringInput(payload.status),
    runId: payload.runId === null || payload.runId === undefined ? null : asBigInt(payload.runId),
    disposition: stringInput(payload.disposition),
    queuedCount: asBigInt(payload.queuedCount),
    metrics,
  };
}

function taskProgressMetrics(payload: Record<string, unknown> | undefined): TaskProgressMetrics | undefined {
  if (!payload) {
    return undefined;
  }
  const usage = asRecord(payload.usage) ?? {};
  const cost = asRecord(payload.cost);
  const inputTokens = asBigInt(usage.inputTokens);
  const outputTokens = asBigInt(usage.outputTokens);
  const cacheReadTokens = asBigInt(usage.cacheReadTokens);
  const cacheWriteTokens = asBigInt(usage.cacheWriteTokens);
  const imageInputTokens = asBigInt(usage.imageInputTokens);
  const totalInputTokens = asBigInt(usage.totalInputTokens) || inputTokens + cacheReadTokens + cacheWriteTokens + imageInputTokens;
  return {
    contextTokens: asBigInt(usage.contextTokens) || totalInputTokens + outputTokens,
    totalInputTokens,
    outputTokens,
    totalTokens: asBigInt(usage.totalTokens),
    costCurrency: typeof cost?.currency === "string" ? cost.currency : null,
    totalCostMicros: asBigInt(cost?.totalMicros),
  };
}

function taskResultStatus(resultContent: string): string {
  if (!resultContent) {
    return "";
  }
  return stringInput(parseJsonRecord(resultContent).status);
}

function appendToolProgress(
  toolById: Map<string, ToolTimelineItem>,
  toolIdByItemId: Map<string, string>,
  itemId: string,
  parentId: string | null,
  channel: string,
  text: string,
  contentKind: "textDelta" | "textSnapshot",
) {
  if (channel !== "tool_progress_info" && channel !== "tool_progress_error") {
    return;
  }

  const toolItemId = parentId ?? itemId;
  const tool = toolById.get(toolIdByItemId.get(toolItemId) ?? toolItemId);
  if (!tool) {
    return;
  }

  const key = channel === "tool_progress_info" ? "progressInfo" : "progressError";
  tool[key] = contentKind === "textSnapshot" ? text : `${tool[key]}${text}`;
}

function upsertExternalRequestItem(
  items: TimelineItemModel[],
  externalById: Map<string, ExternalPromptTimelineItem>,
  id: string,
  turn: bigint,
  cursor: bigint,
  request: ExternalRequestView,
  opened: boolean,
  resolved: boolean,
) {
  const existing = externalById.get(id);
  if (existing) {
    existing.turn = turn;
    existing.cursor = cursor;
    if (opened) {
      existing.openCursor = cursor;
    }
    existing.request = request;
    existing.resolved = resolved;
    return;
  }

  const item = { id, turn, cursor, openCursor: cursor, kind: requestKind(request), request, resolved };
  externalById.set(id, item);
  items.push(item);
}

function hasOpenExternalPrompt(items: TimelineItemModel[]) {
  return items.some((item) => isExternalPromptItem(item) && !item.resolved);
}

function isExternalPromptItem(item: TimelineItemModel): item is Extract<TimelineItemModel, { kind: "permission" | "question" | "prompt" }> {
  return item.kind === "permission" || item.kind === "question" || item.kind === "prompt";
}

function timelineItemKey(item: TimelineItemModel) {
  return isExternalPromptItem(item) ? `${item.id}:${item.resolved ? "resolved" : "open"}` : item.id;
}

function textKind(channel: string): "user" | "assistant" | "thinking" | undefined {
  if (channel === "user_text") {
    return "user";
  }
  if (channel === "assistant_text") {
    return "assistant";
  }
  if (channel === "thinking") {
    return "thinking";
  }

  return undefined;
}

function requestKind(request: ExternalRequestView): "permission" | "question" | "prompt" {
  if (request.kind === EXTERNAL_REQUEST_PERMISSION) {
    return "permission";
  }
  if (request.kind === EXTERNAL_REQUEST_QUESTION) {
    return "question";
  }

  return "prompt";
}

function externalRequestLabel(kind: "permission" | "question" | "prompt") {
  if (kind === "permission") {
    return "permission";
  }
  if (kind === "question") {
    return "question";
  }
  return "external request";
}

function parseTemplateText(text: string): TemplateTextPart[] {
  const parts: TemplateTextPart[] = [];
  let index = 0;
  while (index < text.length) {
    const resultStart = text.indexOf("<template-result", index);
    const errorStart = text.indexOf("<template-error>", index);
    const nextStart = nextTemplateMarkerStart(resultStart, errorStart);
    if (nextStart === -1) {
      pushTemplateTextPart(parts, text.slice(index));
      break;
    }

    pushTemplateTextPart(parts, text.slice(index, nextStart));
    if (nextStart === resultStart) {
      const parsed = parseTemplateResult(text, nextStart);
      if (!parsed) {
        pushTemplateTextPart(parts, text.slice(nextStart));
        break;
      }
      parts.push(parsed.part);
      index = parsed.end;
    } else {
      const parsed = parseTemplateError(text, nextStart);
      if (!parsed) {
        pushTemplateTextPart(parts, text.slice(nextStart));
        break;
      }
      parts.push(parsed.part);
      index = parsed.end;
    }
  }

  return parts.length > 0 ? parts : [{ kind: "text", text }];
}

function retemplatizeUserMessage(text: string) {
  return parseTemplateText(text).map((part) => {
    if (part.kind === "text") {
      return part.text;
    }
    if (part.kind === "error") {
      return part.raw;
    }
    if (part.source) {
      return fileTemplateReference(part.source);
    }
    if (part.tool) {
      return `@tool(${part.tool}, ${part.arguments || "{}"})`;
    }
    return part.raw;
  }).join("");
}

function hasTemplateMarkers(text: string) {
  return text.includes("<template-result") || text.includes("<template-error>");
}

function nextTemplateMarkerStart(resultStart: number, errorStart: number) {
  if (resultStart === -1) {
    return errorStart;
  }
  if (errorStart === -1) {
    return resultStart;
  }
  return Math.min(resultStart, errorStart);
}

function pushTemplateTextPart(parts: TemplateTextPart[], text: string) {
  if (text.length > 0) {
    parts.push({ kind: "text", text });
  }
}

function parseTemplateResult(text: string, start: number): { part: TemplateResultPart; end: number } | undefined {
  const openEnd = text.indexOf(">", start);
  if (openEnd === -1) {
    return undefined;
  }
  const closeTag = "</template-result>";
  const closeStart = text.indexOf(closeTag, openEnd + 1);
  if (closeStart === -1) {
    return undefined;
  }

  const openTag = text.slice(start, openEnd + 1);
  const body = text.slice(openEnd + 1, closeStart).replace(/^\n/, "").replace(/\n$/, "");
  const attributes = parseTemplateAttributes(openTag);
  const argumentsText = extractTemplateChild(body, "arguments");
  const contentText = extractTemplateChild(body, "content");
  return {
    part: {
      kind: "result",
      raw: text.slice(start, closeStart + closeTag.length),
      kindName: attributes.kind ?? "",
      tool: attributes.tool ?? "",
      source: attributes.source ?? "",
      arguments: argumentsText === undefined ? "" : decodeXmlText(argumentsText.trim()),
      content: contentText === undefined ? body : contentText.replace(/^\n/, "").replace(/\n$/, ""),
    },
    end: closeStart + closeTag.length,
  };
}

function parseTemplateError(text: string, start: number): { part: TemplateErrorPart; end: number } | undefined {
  const closeTag = "</template-error>";
  const contentStart = start + "<template-error>".length;
  const closeStart = text.indexOf(closeTag, contentStart);
  if (closeStart === -1) {
    return undefined;
  }
  const raw = text.slice(start, closeStart + closeTag.length);
  return {
    part: { kind: "error", raw, message: decodeXmlText(text.slice(contentStart, closeStart)) },
    end: closeStart + closeTag.length,
  };
}

function parseTemplateAttributes(openTag: string) {
  const attributes: Record<string, string> = {};
  for (const match of openTag.matchAll(/([A-Za-z_:][A-Za-z0-9_:.-]*)="([^"]*)"/g)) {
    attributes[match[1]!] = decodeXmlText(match[2]!);
  }
  return attributes;
}

function extractTemplateChild(body: string, tag: string) {
  const openTag = `<${tag}>`;
  const closeTag = `</${tag}>`;
  const openStart = body.indexOf(openTag);
  if (openStart === -1) {
    return undefined;
  }
  const contentStart = openStart + openTag.length;
  const closeStart = body.indexOf(closeTag, contentStart);
  if (closeStart === -1) {
    return undefined;
  }
  return body.slice(contentStart, closeStart);
}

function decodeXmlText(value: string) {
  return value
    .replace(/&quot;/g, '"')
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&amp;/g, "&");
}

function fileTemplateReference(path: string) {
  if (/^[A-Za-z0-9./_-]+$/.test(path)) {
    return `@${path}`;
  }
  return `@"${path.replace(/\\/g, "\\\\").replace(/"/g, '\\"')}"`;
}

function toolLabel(name: string, inputEntries: ToolInputEntry[]) {
  if (inputEntries.length === 0) {
    return name;
  }
  return `${name}(${inputEntries.map(({ key, value }) => `${key}: ${value}`).join(", ")})`;
}

function topLevelInputEntries(record: Record<string, unknown>): ToolInputEntry[] {
  return Object.keys(record).sort().map((key) => ({ key, value: formatToolInputValue(record[key]) }));
}

function formatToolInputValue(value: unknown) {
  return JSON.stringify(value) ?? String(value);
}

function stringInput(value: unknown) {
  return typeof value === "string" ? value : "";
}

function diffTextLines(text: string, kind: "add" | "remove") {
  const prefix = kind === "add" ? "+" : "-";
  return (text.length > 0 ? text.split("\n") : [""]).map((line) => ({ kind, text: `${prefix} ${line}` }));
}

function patchFiles(patchText: string) {
  const files: Array<{ status: string; path: string }> = [];
  for (const line of patchText.split("\n")) {
    const match = /^\*\*\* (Add|Update|Delete) File: (.+)$/.exec(line);
    if (!match) {
      continue;
    }

    files.push({ status: patchFileStatus(match[1]), path: match[2] });
  }

  return files;
}

function patchDiffLines(patchText: string) {
  let currentLanguage: string | undefined;
  return patchText.split("\n").map((text) => {
    const fileMatch = /^\*\*\* (?:Add|Update|Delete) File: (.+)$/.exec(text);
    if (fileMatch) {
      currentLanguage = languageFromFilePath(fileMatch[1]);
    }

    const kind = patchLineKind(text);
    return {
      kind,
      text,
      language: kind === "add" || kind === "remove" || kind === "context" ? currentLanguage : undefined,
    };
  });
}

function patchFileStatus(action: string) {
  if (action === "Add") {
    return "A";
  }
  if (action === "Delete") {
    return "D";
  }
  return "M";
}

function patchLineKind(line: string): "add" | "remove" | "file" | "meta" | "context" {
  if (line.startsWith("***")) {
    return "file";
  }
  if (line.startsWith("@@")) {
    return "meta";
  }
  if (line.startsWith("+")) {
    return "add";
  }
  if (line.startsWith("-")) {
    return "remove";
  }
  return "context";
}

function diffLineParts(text: string, kind: "add" | "remove" | "file" | "meta" | "context") {
  if ((kind === "add" || kind === "remove") && (text.startsWith("+ ") || text.startsWith("- "))) {
    return { prefix: text.slice(0, 2), code: text.slice(2) };
  }
  if ((kind === "add" || kind === "remove") && (text.startsWith("+") || text.startsWith("-"))) {
    return { prefix: text.slice(0, 1), code: text.slice(1) };
  }
  if (kind === "context" && text.startsWith(" ")) {
    return { prefix: " ", code: text.slice(1) };
  }

  return { prefix: "", code: text };
}

function sessionActive(session: SessionView) {
  return !["idle", "suspended", "finished", "failed", "unknown", "unspecified"].includes(session.status.phase);
}

function sessionImportantStatus(session: SessionView, working: boolean, watchError?: string | null) {
  if (watchError) {
    return {
      tone: "error",
      message: watchError,
    };
  }

  if (session.status.phase === "failed") {
    return {
      tone: "error",
      message: session.status.failedMessage || "Session failed.",
    };
  }

  return working ? { tone: "working", message: "Working..." } : undefined;
}

function isNearScrollBottom(element: HTMLElement) {
  return element.scrollHeight - element.scrollTop - element.clientHeight < 48;
}

function isScrollable(element: HTMLElement) {
  return element.scrollHeight - element.clientHeight > 48;
}

function isNearScrollTop(element: HTMLElement) {
  return element.scrollTop <= Math.max(TIMELINE_TOP_LOAD_THRESHOLD_PX, element.clientHeight * 0.5);
}

function isStrictlyNearScrollTop(element: HTMLElement) {
  return element.scrollTop <= Math.max(TIMELINE_TOP_LOAD_THRESHOLD_PX * 0.5, element.clientHeight * 0.25);
}

function scheduleTimelineScroll(action: () => void) {
  requestAnimationFrame(() => requestAnimationFrame(action));
}

function isScrollKey(key: string) {
  return key === "ArrowUp"
    || key === "ArrowDown"
    || key === "PageUp"
    || key === "PageDown"
    || key === "Home"
    || key === "End"
    || key === " ";
}

function parseJsonRecord(value: string): Record<string, unknown> {
  try {
    return asRecord(JSON.parse(value)) ?? {};
  } catch {
    return {};
  }
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : undefined;
}

function asArray(value: unknown): unknown[] {
  return Array.isArray(value) ? value : [];
}

function asBigInt(value: unknown): bigint {
  if (typeof value === "bigint") {
    return value;
  }
  if (typeof value === "number" && Number.isFinite(value) && value >= 0) {
    return BigInt(Math.trunc(value));
  }
  if (typeof value === "string" && /^\d+$/.test(value)) {
    return BigInt(value);
  }
  return 0n;
}

function permissionPatterns(payload: Record<string, unknown>) {
  const patterns = asArray(payload.patterns);
  if (patterns.length > 0) {
    return patterns;
  }

  const pattern = payload.pattern;
  return pattern === undefined ? [] : [pattern];
}

type QuestionModel = {
  question: string;
  header: string;
  options: Array<{ label: string; description: string }>;
  multiple: boolean;
  custom: boolean;
};

function normalizeQuestion(value: unknown): QuestionModel {
  const record = asRecord(value) ?? {};
  return {
    question: String(record.question ?? ""),
    header: String(record.header ?? "Question"),
    multiple: Boolean(record.multiple),
    custom: Boolean(record.custom),
    options: asArray(record.options).map((option) => {
      const optionRecord = asRecord(option) ?? {};
      return {
        label: String(optionRecord.label ?? ""),
        description: String(optionRecord.description ?? ""),
      };
    }),
  };
}

function SessionDetailStatusBar(props: {
  session: SessionView;
  contextTokens: bigint;
  onEditProvider: () => void;
  onEditMcp: () => void;
}) {
  const app = useAppState();
  const [reasoningEffort, setReasoningEffort] = createSignal(props.session.runtimeConfig?.provider?.reasoningEffort ?? "");
  const [savingReasoningEffort, setSavingReasoningEffort] = createSignal(false);
  const [reasoningEffortError, setReasoningEffortError] = createSignal<string>();
  const config = createMemo(() => app.snapshot()?.config.current ?? undefined);
  const reasoningEfforts = createMemo(() => currentReasoningEfforts(config(), props.session));

  createEffect(() => {
    const current = props.session.runtimeConfig?.provider?.reasoningEffort ?? "";
    props.session.id;
    props.session.runtimeConfig?.provider?.key;
    props.session.runtimeConfig?.provider?.modelId;
    if (!untrack(savingReasoningEffort)) {
      setReasoningEffort(current);
    }
  });

  const updateReasoningEffort = async (effort: string) => {
    const provider = props.session.runtimeConfig?.provider;
    if (!provider) {
      setReasoningEffortError("Choose a provider before setting thinking.");
      return;
    }

    setReasoningEffort(effort);
    await updateRuntimeConfig(app.dispatchUpdateSessionRuntimeConfig, props.session, {
      providerKey: provider.key,
      modelId: provider.modelId,
      reasoningEffort: effort || null,
      enabledMcpServerKeys: props.session.runtimeConfig?.mcpServers.map((server) => server.key) ?? [],
      setSaving: setSavingReasoningEffort,
      setError: setReasoningEffortError,
      onClose: () => undefined,
    });
  };

  return (
    <footer class="sessions__detail-status" aria-label="Session status summary">
      <span class="sessions__detail-status-item">
        <strong>Provider</strong> {formatRuntimeProvider(props.session)}
        <button
          type="button"
          class="sessions__detail-status-button"
          aria-label="Edit provider/model"
          onClick={props.onEditProvider}
        >
          Edit
        </button>
      </span>
      <label class="sessions__detail-status-item sessions__detail-status-select">
        <strong>Thinking</strong>
        <select
          aria-label="Session thinking level"
          value={reasoningEffort()}
          disabled={reasoningEfforts().length === 0 || savingReasoningEffort()}
          onChange={(event) => void updateReasoningEffort(event.currentTarget.value)}
        >
          <option value="">None</option>
          <For each={reasoningEfforts()}>
            {(effort) => <option value={effort}>{effort}</option>}
          </For>
        </select>
      </label>
      <Show when={reasoningEffortError()}>
        {(message) => <span class="sessions__detail-status-error">{message()}</span>}
      </Show>
      <span class="sessions__detail-status-item">
        <strong>MCP</strong> {formatRuntimeMcpServers(props.session)}
        <button
          type="button"
          class="sessions__detail-status-button"
          aria-label="Edit MCP"
          onClick={props.onEditMcp}
        >
          Edit
        </button>
      </span>
      <span class="sessions__detail-metrics" aria-label="Session token and cost totals">
        <span class="sessions__detail-status-item" title="Latest reported input + output tokens">
          <strong>Ctx</strong> {formatTokenCount(props.contextTokens)}
        </span>
        <span class="sessions__detail-status-item">
          <strong>Cache</strong> {formatCacheRate(props.session.metrics.cacheReadTokens, props.session.metrics.totalInputTokens)} / {formatCacheRate(props.session.metrics.cacheWriteTokens, props.session.metrics.totalInputTokens)}
        </span>
        <span class="sessions__detail-status-item">
          <strong>Cost</strong> {formatCostMicros(props.session.metrics.totalCostMicros, props.session.metrics.costCurrency)}
        </span>
      </span>
      <SessionDeleteAction session={props.session} compact />
    </footer>
  );
}

function formatTokenCount(value: bigint) {
  return new Intl.NumberFormat().format(Number(value));
}

function formatCacheRate(part: bigint, total: bigint) {
  if (total === 0n) {
    return "0%";
  }
  return `${((Number(part) / Number(total)) * 100).toFixed(1)}%`;
}

function formatCostMicros(micros: bigint, currency: string | null) {
  if (micros === 0n) {
    return "-";
  }
  const amount = Number(micros) / 1_000_000;
  return `${currency ?? "USD"} ${amount.toFixed(6)}`;
}

function currentReasoningEfforts(config: ConfigView | undefined, session: SessionView) {
  const provider = session.runtimeConfig?.provider;
  if (!provider) {
    return [];
  }

  return config?.providers[provider.key]?.models[provider.modelId]?.options.reasoningEfforts ?? [];
}

function SessionProviderDialog(props: { session: SessionView; onClose: () => void }) {
  const app = useAppState();
  const [query, setQuery] = createSignal("");
  const [providerKey, setProviderKey] = createSignal(props.session.runtimeConfig?.provider?.key ?? "");
  const [modelId, setModelId] = createSignal(props.session.runtimeConfig?.provider?.modelId ?? "");
  const [reasoningEffort, setReasoningEffort] = createSignal(props.session.runtimeConfig?.provider?.reasoningEffort ?? "");
  const [error, setError] = createSignal<string>();
  const [saving, setSaving] = createSignal(false);
  const config = createMemo(() => app.snapshot()?.config.current ?? undefined);
  const providers = createMemo(() => providerEntries(config(), query()));
  const selectedProvider = createMemo(() => config()?.providers[providerKey()]);
  const selectedModel = createMemo(() => selectedProvider()?.models[modelId()]);
  const models = createMemo(() => Object.entries(selectedProvider()?.models ?? {}));
  const reasoningEfforts = createMemo(() => selectedModel()?.options.reasoningEfforts ?? []);

  createEffect(() => {
    const provider = selectedProvider();
    if (provider && !provider.models[modelId()]) {
      setModelId(Object.keys(provider.models)[0] ?? "");
    }
  });

  createEffect(() => {
    const effort = reasoningEffort();
    if (effort !== "" && !reasoningEfforts().includes(effort)) {
      setReasoningEffort("");
    }
  });

  const save = async () => {
    const provider = providerKey();
    const model = modelId();
    if (!provider || !model) {
      setError("Choose a provider and model.");
      return;
    }
    await updateRuntimeConfig(app.dispatchUpdateSessionRuntimeConfig, props.session, {
      providerKey: provider,
      modelId: model,
      reasoningEffort: reasoningEffort() || null,
      enabledMcpServerKeys: props.session.runtimeConfig?.mcpServers.map((server) => server.key) ?? [],
      setSaving,
      setError,
      onClose: props.onClose,
    });
  };

  return (
    <Dialog title="Edit provider/model" onClose={props.onClose} class="sessions__runtime-dialog">
      <div class="sessions__runtime-editor">
        <p class="sessions__runtime-note">
          Updates this session metadata immediately. The next prompt will use the selected runtime.
        </p>
        <Show when={error()}>{(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}</Show>
        <label class="sessions__runtime-search">
          Search providers
          <input value={query()} onInput={(event) => setQuery(event.currentTarget.value)} />
        </label>
        <div class="sessions__runtime-list" role="listbox" aria-label="Provider options">
          <For each={providers()}>
            {([key, provider]) => (
              <button
                type="button"
                class={`sessions__runtime-option ${providerKey() === key ? "is-selected" : ""}`.trim()}
                onClick={() => {
                  setProviderKey(key);
                  setModelId(Object.keys(provider.models)[0] ?? "");
                }}
              >
                <span>{key}</span>
                <RuntimeStateBadge state={providerRuntimeState(props.session, key)} />
              </button>
            )}
          </For>
        </div>
        <label class="sessions__runtime-search">
          Model
          <select value={modelId()} onChange={(event) => setModelId(event.currentTarget.value)}>
            <For each={models()}>
              {([key, model]) => <option value={key}>{model?.name ? `${key} - ${model.name}` : key}</option>}
            </For>
          </select>
        </label>
        <label class="sessions__runtime-search">
          Thinking level
          <select value={reasoningEffort()} onChange={(event) => setReasoningEffort(event.currentTarget.value)} disabled={reasoningEfforts().length === 0}>
            <option value="">None</option>
            <For each={reasoningEfforts()}>
              {(effort) => <option value={effort}>{effort}</option>}
            </For>
          </select>
        </label>
        <div class="sessions__runtime-actions">
          <button type="button" onClick={props.onClose}>Cancel</button>
          <button type="button" onClick={() => void save()} disabled={saving()}>
            {saving() ? "Saving..." : "Save runtime"}
          </button>
        </div>
      </div>
    </Dialog>
  );
}

function SessionMcpDialog(props: { session: SessionView; onClose: () => void }) {
  const app = useAppState();
  const [query, setQuery] = createSignal("");
  const [selected, setSelected] = createSignal(new Set(props.session.runtimeConfig?.mcpServers.map((server) => server.key) ?? []));
  const [error, setError] = createSignal<string>();
  const [saving, setSaving] = createSignal(false);
  const config = createMemo(() => app.snapshot()?.config.current ?? undefined);
  const servers = createMemo(() => mcpEntries(config(), props.session, query()));

  const toggle = (key: string) => {
    setSelected((current) => {
      const next = new Set(current);
      if (next.has(key)) {
        next.delete(key);
      } else {
        next.add(key);
      }
      return next;
    });
  };

  const save = async () => {
    await updateRuntimeConfig(app.dispatchUpdateSessionRuntimeConfig, props.session, {
      providerKey: props.session.runtimeConfig?.provider?.key ?? "",
      modelId: props.session.runtimeConfig?.provider?.modelId ?? "",
      reasoningEffort: props.session.runtimeConfig?.provider?.reasoningEffort ?? null,
      enabledMcpServerKeys: Array.from(selected()),
      setSaving,
      setError,
      onClose: props.onClose,
    });
  };

  return (
    <Dialog title="Edit MCP servers" onClose={props.onClose} class="sessions__runtime-dialog">
      <div class="sessions__runtime-editor">
        <p class="sessions__runtime-note">
          Missing servers stay visible so drift is explicit before you save.
        </p>
        <Show when={error()}>{(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}</Show>
        <label class="sessions__runtime-search">
          Search MCP servers
          <input value={query()} onInput={(event) => setQuery(event.currentTarget.value)} />
        </label>
        <div class="sessions__runtime-list" aria-label="MCP server options">
          <For each={servers()}>
            {(server) => (
              <label class={`sessions__runtime-option ${selected().has(server.key) ? "is-selected" : ""}`.trim()}>
                <input
                  type="checkbox"
                  checked={selected().has(server.key)}
                  onChange={() => toggle(server.key)}
                  disabled={!server.configured}
                />
                <span>{server.key}</span>
                <RuntimeStateBadge state={server.state} />
              </label>
            )}
          </For>
        </div>
        <div class="sessions__runtime-actions">
          <button type="button" onClick={props.onClose}>Cancel</button>
          <button type="button" onClick={() => void save()} disabled={saving()}>
            {saving() ? "Saving..." : "Save MCP"}
          </button>
        </div>
      </div>
    </Dialog>
  );
}

function formatRuntimeProvider(session: SessionView) {
  const provider = session.runtimeConfig?.provider;

  if (!provider) {
    return "none";
  }

  return provider.reasoningEffort ? `${provider.key}/${provider.modelId} (${provider.reasoningEffort})` : `${provider.key}/${provider.modelId}`;
}

function formatRuntimeMcpServers(session: SessionView) {
  const servers = session.runtimeConfig?.mcpServers ?? [];

  if (servers.length === 0) {
    return "none";
  }

  return servers.map((server) => server.key).join(", ");
}

function RuntimeStateBadge(props: { state: RuntimeConfigMatchView | "available" }) {
  return <span class={`sessions__runtime-state sessions__runtime-state--${props.state}`}>{runtimeStateLabel(props.state)}</span>;
}

function providerEntries(config: ConfigView | undefined, query: string): Array<[string, ProviderView]> {
  return Object.entries(config?.providers ?? {})
    .filter((entry): entry is [string, ProviderView] => Boolean(entry[1]))
    .filter(([key, provider]) => searchable(`${key} ${provider.name}`, query));
}

function mcpEntries(config: ConfigView | undefined, session: SessionView, query: string) {
  const configured = Object.entries(config?.mcpServers ?? {})
    .filter((entry): entry is [string, McpServerView] => Boolean(entry[1]))
    .map(([key]) => ({
      key,
      configured: true,
      state: mcpRuntimeState(session, key) ?? "available" as const,
    }));
  const missing = (session.runtimeConfig?.mcpServers ?? [])
    .filter((server) => !(config?.mcpServers ?? {})[server.key])
    .map((server) => ({
      key: server.key,
      configured: false,
      state: server.matchStatus,
    }));

  return [...configured, ...missing].filter((server) => searchable(server.key, query));
}

function providerRuntimeState(session: SessionView, key: string) {
  return session.runtimeConfig?.provider?.key === key
    ? session.runtimeConfig.provider.matchStatus
    : "available";
}

function mcpRuntimeState(session: SessionView, key: string) {
  return session.runtimeConfig?.mcpServers.find((server) => server.key === key)?.matchStatus;
}

function runtimeStateLabel(state: RuntimeConfigMatchView | "available") {
  switch (state) {
    case "configured":
      return "Configured";
    case "changed":
      return "Changed";
    case "missing":
      return "Missing";
    case "available":
      return "Available";
    default:
      return "Unknown";
  }
}

function searchable(value: string, query: string) {
  return value.toLowerCase().includes(query.trim().toLowerCase());
}

async function updateRuntimeConfig(
  dispatchUpdateSessionRuntimeConfig: (command: Extract<ClientCommand, { type: "updateSessionRuntimeConfig" }>) => Promise<unknown>,
  session: SessionView,
  options: {
    providerKey: string;
    modelId: string;
    reasoningEffort: string | null;
    enabledMcpServerKeys: string[];
    setSaving: (saving: boolean) => void;
    setError: (message: string | undefined) => void;
    onClose: () => void;
  },
) {
  options.setSaving(true);
  options.setError(undefined);

  try {
    await dispatchUpdateSessionRuntimeConfig({
      type: "updateSessionRuntimeConfig",
      id: session.id,
      providerKey: options.providerKey,
      modelId: options.modelId,
      reasoningEffort: options.reasoningEffort,
      enabledMcpServerKeys: options.enabledMcpServerKeys,
    });
    options.onClose();
  } catch (error) {
    options.setError(error instanceof Error ? error.message : "Unable to update session runtime.");
  } finally {
    options.setSaving(false);
  }
}

function SessionDeleteAction(props: { session: SessionView; compact?: boolean }) {
  const sessions = useSessionsRoute();
  const label = props.compact ? "Delete" : "Delete session";

  return (
    <div class={props.compact ? "session-row__actions" : "sessions__detail-actions-control"}>
      <Show
        when={sessions.confirmingDelete() === props.session.id}
        fallback={
          <button
            type="button"
            class="session-delete-button"
            aria-label={`Delete session ${props.session.id}`}
            disabled={sessions.deleteDisabled()}
            onClick={() => sessions.promptDelete(props.session.id)}
          >
            {label}
          </button>
        }
      >
        <ConfirmAction
          label={`Delete session ${props.session.id} confirmation`}
          prompt="Delete?"
          confirmLabel="Confirm"
          confirmAriaLabel={`Confirm delete session ${props.session.id}`}
          cancelAriaLabel={`Cancel delete session ${props.session.id}`}
          pending={sessions.deletingSession() === props.session.id}
          onConfirm={() => void sessions.deleteSession(props.session.id)}
          onCancel={sessions.cancelDelete}
        />
      </Show>
    </div>
  );
}

function StatusChit(props: { phase: string }) {
  return <span class="session-status">{props.phase}</span>;
}

function useSessionsRoute() {
  const state = useContext(SessionsContext);

  if (!state) {
    throw new Error("missing sessions route context");
  }

  return state;
}

function sortSessionsByUpdated(sessions: readonly SessionView[]) {
  return [...sessions].sort((left, right) => timestampMillis(right.updatedAt) - timestampMillis(left.updatedAt));
}

function timestampMillis(timestamp: TimestampView | null) {
  if (!timestamp) {
    return Number.NEGATIVE_INFINITY;
  }

  return Number(timestamp.seconds) * 1_000 + Math.floor(timestamp.nanos / 1_000_000);
}

function formatTimestamp(timestamp: TimestampView | null) {
  const millis = timestampMillis(timestamp);

  if (!Number.isFinite(millis)) {
    return undefined;
  }

  return new Date(millis).toLocaleString();
}

function omitKey(record: Record<string, string>, key: string) {
  const next = { ...record };
  delete next[key];

  return next;
}

function dispatchSessionWatchCommand(
  dispatch: (command: ClientCommand) => Promise<unknown>,
  command: ClientCommand,
) {
  void dispatch(command).catch(() => undefined);
}
