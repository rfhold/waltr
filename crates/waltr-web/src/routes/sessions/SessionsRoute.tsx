import { A, useNavigate, useParams } from "@solidjs/router";
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
import { ConfirmAction, Dialog, InlineAlert } from "../../components/ui";
import type {
  ClientCommand,
  ConfigView,
  McpServerView,
  ProviderView,
  RuntimeConfigMatchView,
  SessionView,
  TimestampView,
} from "../../generated/waltr-client";

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
  const sessions = useSessionsRoute();
  const [editingRuntime, setEditingRuntime] = createSignal<"provider" | "mcp">();

  return (
    <div class="sessions__detail-page">
      <div class="sessions__detail-main">
        <Show when={sessions.deleteErrors()[props.session.id]}>
          {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
        </Show>
      </div>
      <SessionDetailStatusBar
        session={props.session}
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

function SessionDetailStatusBar(props: {
  session: SessionView;
  onEditProvider: () => void;
  onEditMcp: () => void;
}) {
  return (
    <footer class="sessions__detail-status" aria-label="Session status summary">
      <span><strong>Status</strong> {props.session.status.phase}</span>
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
      <SessionDeleteAction session={props.session} compact />
    </footer>
  );
}

function SessionProviderDialog(props: { session: SessionView; onClose: () => void }) {
  const app = useAppState();
  const [query, setQuery] = createSignal("");
  const [providerKey, setProviderKey] = createSignal(props.session.runtimeConfig?.provider?.key ?? "");
  const [modelId, setModelId] = createSignal(props.session.runtimeConfig?.provider?.modelId ?? "");
  const [error, setError] = createSignal<string>();
  const [saving, setSaving] = createSignal(false);
  const config = createMemo(() => app.snapshot()?.config.current ?? undefined);
  const providers = createMemo(() => providerEntries(config(), query()));
  const selectedProvider = createMemo(() => config()?.providers[providerKey()]);
  const models = createMemo(() => Object.entries(selectedProvider()?.models ?? {}));

  createEffect(() => {
    const provider = selectedProvider();
    if (provider && !provider.models[modelId()]) {
      setModelId(Object.keys(provider.models)[0] ?? "");
    }
  });

  const save = async () => {
    const provider = providerKey();
    const model = modelId();
    if (!provider || !model) {
      setError("Choose a provider and model.");
      return;
    }
    await updateRuntimeConfig(app.dispatch, props.session, {
      providerKey: provider,
      modelId: model,
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
    await updateRuntimeConfig(app.dispatch, props.session, {
      providerKey: props.session.runtimeConfig?.provider?.key ?? "",
      modelId: props.session.runtimeConfig?.provider?.modelId ?? "",
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

  return `${provider.key}/${provider.modelId}`;
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
  dispatch: (command: ClientCommand) => Promise<unknown>,
  session: SessionView,
  options: {
    providerKey: string;
    modelId: string;
    enabledMcpServerKeys: string[];
    setSaving: (saving: boolean) => void;
    setError: (message: string | undefined) => void;
    onClose: () => void;
  },
) {
  options.setSaving(true);
  options.setError(undefined);

  try {
    await dispatch({
      type: "updateSessionRuntimeConfig",
      id: session.id,
      providerKey: options.providerKey,
      modelId: options.modelId,
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
