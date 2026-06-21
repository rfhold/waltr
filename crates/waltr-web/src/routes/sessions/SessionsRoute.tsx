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
import { ConfirmAction, InlineAlert } from "../../components/ui";
import type { ClientCommand, SessionView, TimestampView } from "../../generated/waltr-client";

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
    <section class="sessions sessions--detail" aria-labelledby="session-detail-title">
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

  return (
    <>
      <nav class="sessions__breadcrumb" aria-label="Session breadcrumb">
        <A href="/sessions">Sessions</A>
        <span>/</span>
        <span>{props.session.id}</span>
      </nav>
      <div class="sessions__detail-card">
        <div class="sessions__detail-heading">
          <h1 id="session-detail-title">Session</h1>
          <StatusChit phase={props.session.status.phase} />
        </div>
        <div class="sessions__detail-actions">
          <SessionDeleteAction session={props.session} />
        </div>
        <Show when={sessions.deleteErrors()[props.session.id]}>
          {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
        </Show>
      </div>
    </>
  );
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
