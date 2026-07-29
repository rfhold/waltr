import { useLocation, useNavigate, useParams } from "@solidjs/router";
import {
  For,
  Show,
  createContext,
  createEffect,
  createMemo,
  createSignal,
  useContext,
} from "solid-js";
import type { Accessor, ParentProps } from "solid-js";

import { useAppState } from "../../app/context";
import { ActionList, ActionRow, ConfirmAction, Dialog, Field, InlineAlert } from "../../components/ui";
import type {
  ClientCommand,
  ConfigView,
  McpServerTypeView,
  McpServerView,
} from "../../generated/waltr-client";
import { AuthStatusPanel } from "./AuthStatusPanel";

type FormMode = "create" | "edit";

type McpServerForm = {
  mode: FormMode;
  originalName?: string;
  name: string;
  kind: McpServerTypeView;
  command: string;
  url: string;
};

type ActiveForm =
  | { mode: "create" }
  | { mode: "edit"; serverName: string; server: McpServerView };

type McpServersSectionState = {
  watchedConfig: Accessor<ConfigView | undefined>;
  closeForm: () => void;
  operationError: Accessor<string | undefined>;
  operationPending: Accessor<boolean>;
  saveServer: (form: McpServerForm) => Promise<void>;
  setDeleteServer: (server?: string) => void;
  setSelectedServer: (server?: string) => void;
};

const McpServersSectionContext = createContext<McpServersSectionState>();

export function McpServersSection(props: ParentProps & {
  watchedConfig: Accessor<ConfigView | undefined>;
}) {
  const app = useAppState();
  const location = useLocation();
  const navigate = useNavigate();
  const watchedConfig = props.watchedConfig;
  const [selectedServer, setSelectedServer] = createSignal<string>();
  const [deleteServer, setDeleteServer] = createSignal<string>();
  const [operationError, setOperationError] = createSignal<string>();
  const [operationPending, setOperationPending] = createSignal(false);
  const routeServerName = createMemo(() => serverNameFromEditPath(location.pathname));
  const visibleServerName = createMemo(() => routeServerName() ?? selectedServer());

  createEffect(() => {
    const config = watchedConfig();
    const servers = serverEntries(config);
    if (!config || servers.length === 0) {
      setSelectedServer(undefined);
      return;
    }

    const routeServer = routeServerName();
    if (routeServer && servers.some(([name]) => name === routeServer)) {
      setSelectedServer(routeServer);
      return;
    }

    setSelectedServer((selected) =>
      selected && servers.some(([name]) => name === selected)
        ? selected
        : servers[0]?.[0],
    );
  });

  const closeForm = () => navigate("/config/mcp");

  const saveServer = (form: McpServerForm) =>
    saveMcpServer(app.dispatchConfigPatch, form, closeForm, setOperationPending, setOperationError);

  const sectionState: McpServersSectionState = {
    watchedConfig,
    closeForm,
    operationError,
    operationPending,
    saveServer,
    setDeleteServer,
    setSelectedServer,
  };

  return (
    <McpServersSectionContext.Provider value={sectionState}>
      <div class="config__actions" aria-label="MCP server actions">
        <button
          type="button"
          onClick={() => {
            setDeleteServer(undefined);
            navigate("/config/mcp/create");
          }}
        >
          Create
        </button>
      </div>
      <Show when={operationError()}>
        {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
      </Show>
      <Show when={watchedConfig()} keyed>
        {(currentConfig) => (
          <ActionList class="config__list" label="MCP server list">
            <Show
              when={serverEntries(currentConfig).length > 0}
              fallback={<p class="panel">No MCP servers configured yet.</p>}
            >
              <For each={serverEntries(currentConfig)}>
                {([serverName, server]) => (
                  <ActionRow class="provider-row" selected={visibleServerName() === serverName}>
                    <button
                      type="button"
                      class="provider-row__summary"
                      aria-pressed={visibleServerName() === serverName}
                      onClick={() => setSelectedServer(serverName)}
                    >
                      <strong>{serverName}</strong>
                      <span>{server.kind}</span>
                      <code>{server.kind === "sse" ? server.url || "No URL configured" : server.command || "No command configured"}</code>
                    </button>
                    <Show
                      when={deleteServer() === serverName}
                      fallback={
                        <div class="provider-row__actions">
                          <button
                            type="button"
                            aria-label={`Edit ${serverName}`}
                            onClick={() => {
                              setSelectedServer(serverName);
                              setDeleteServer(undefined);
                              navigate(`/config/mcp/edit/${encodeURIComponent(serverName)}`);
                            }}
                          >
                            Edit
                          </button>
                          <button
                            type="button"
                            aria-label={`Delete ${serverName}`}
                            onClick={() => {
                              setSelectedServer(serverName);
                              setDeleteServer(serverName);
                            }}
                          >
                            Delete
                          </button>
                        </div>
                      }
                    >
                      <ConfirmAction
                        label={`Delete ${serverName} confirmation`}
                        prompt="Delete?"
                        confirmLabel="Confirm"
                        confirmAriaLabel={`Confirm delete ${serverName}`}
                        cancelAriaLabel={`Cancel delete ${serverName}`}
                        pending={operationPending()}
                        onConfirm={() => deleteMcpServerByName(app.dispatchConfigPatch, serverName, setDeleteServer, setOperationPending, setOperationError)}
                        onCancel={() => setDeleteServer(undefined)}
                      />
                    </Show>
                  </ActionRow>
                )}
              </For>
            </Show>
          </ActionList>
        )}
      </Show>
      <Show when={!routeServerName() && selectedServer()}>
        {(serverName) => <AuthStatusPanel target={{ kind: "mcpServer", key: serverName() }} />}
      </Show>
      {props.children}
    </McpServersSectionContext.Provider>
  );
}

export function ConfigMcpCreateRoute() {
  const section = useMcpServersSectionState();

  createEffect(() => section.setDeleteServer(undefined));

  return (
    <McpServerFormPanel
      form={{ mode: "create" }}
      onCancel={section.closeForm}
      onSubmit={section.saveServer}
      pending={section.operationPending()}
    />
  );
}

export function ConfigMcpEditRoute() {
  const section = useMcpServersSectionState();
  const params = useParams<{ server: string }>();
  const serverName = createMemo(() => params.server);
  const [activeForm, setActiveForm] = createSignal<ActiveForm>();

  createEffect(() => {
    const name = serverName();
    section.setDeleteServer(undefined);
    section.setSelectedServer(name);
    const form = activeForm();
    if (form?.mode === "edit" && form.serverName === name) return;
    const server = section.watchedConfig()?.mcpServers[name];
    setActiveForm(server ? { mode: "edit", serverName: name, server } : undefined);
  });

  return (
    <Show when={activeForm()} keyed>
      {(serverForm) => (
        <McpServerFormPanel
          form={serverForm}
          onCancel={section.closeForm}
          onSubmit={section.saveServer}
          pending={section.operationPending()}
        />
      )}
    </Show>
  );
}

function useMcpServersSectionState() {
  const state = useContext(McpServersSectionContext);
  if (!state) {
    throw new Error("MCP servers section child rendered outside McpServersSection");
  }
  return state;
}

function McpServerFormPanel(props: {
  form: ActiveForm;
  onCancel: () => void;
  onSubmit: (form: McpServerForm) => Promise<void>;
  pending: boolean;
}) {
  const [form, setForm] = createSignal(initialMcpServerForm(props.form));
  const update = (patch: Partial<McpServerForm>) => setForm({ ...form(), ...patch });
  const title = () => form().mode === "create" ? "Create MCP server" : "Edit MCP server";
  const requiredFieldMissing = createMemo(() => {
    const current = form();
    return current.kind === "sse"
      ? current.url.trim().length === 0
      : current.command.trim().length === 0;
  });

  return (
    <Dialog title={title()} onClose={props.onCancel} class="panel config__form">
      <form
        class="config__form"
        aria-label={title()}
        onSubmit={(event) => {
          event.preventDefault();
          if (requiredFieldMissing()) {
            return;
          }
          void props.onSubmit(form());
        }}
      >
        <Field id="mcp-server-name" label="Name" required hint="Use lowercase letters, numbers, dots, underscores, or dashes; no spaces.">
          {(field) => (
            <input name="name" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} value={form().name} required pattern="[a-z0-9._-]+" title="Use lowercase letters, numbers, dots, underscores, or dashes; no spaces." onInput={(event) => update({ name: event.currentTarget.value })} />
          )}
        </Field>
        <Field id="mcp-server-kind" label="Type">
          {(field) => (
            <select name="kind" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} value={form().kind} onChange={(event) => update({ kind: event.currentTarget.value as McpServerTypeView })}>
              <option value="stdio">stdio</option>
              <option value="sse">sse</option>
            </select>
          )}
        </Field>
        <Show
          when={form().kind === "sse"}
          fallback={
            <Field id="mcp-server-command" label="Command" required error={requiredFieldMissing() ? "A command is required for stdio servers." : undefined}>
              {(field) => (
                <input name="command" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} value={form().command} onInput={(event) => update({ command: event.currentTarget.value })} />
              )}
            </Field>
          }
        >
          <Field id="mcp-server-url" label="URL" required error={requiredFieldMissing() ? "A URL is required for sse servers." : undefined}>
            {(field) => (
              <input name="url" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="url" value={form().url} onInput={(event) => update({ url: event.currentTarget.value })} />
            )}
          </Field>
        </Show>
        <Show when={form().mode === "edit" && form().originalName}>
          {(serverName) => <AuthStatusPanel target={{ kind: "mcpServer", key: serverName() }} />}
        </Show>
        <div class="config__form-actions">
          <button type="submit" disabled={props.pending || requiredFieldMissing()}>Save</button>
          <button type="button" disabled={props.pending} onClick={props.onCancel}>Cancel</button>
        </div>
      </form>
    </Dialog>
  );
}

function initialMcpServerForm(form: ActiveForm): McpServerForm {
  if (form.mode === "create") {
    return { mode: "create", name: "", kind: "stdio", command: "", url: "" };
  }

  return serverToForm(form.serverName, form.server);
}

function serverToForm(serverName: string, server: McpServerView): McpServerForm {
  return {
    mode: "edit",
    originalName: serverName,
    name: serverName,
    kind: server.kind,
    command: server.command,
    url: server.url,
  };
}

async function saveMcpServer(
  dispatch: (command: ClientCommand) => Promise<unknown>,
  form: McpServerForm,
  closeForm: () => void,
  setOperationPending: (pending: boolean) => void,
  setOperationError: (message?: string) => void,
) {
  const server = formToServer(form);
  const command: ClientCommand = form.mode === "edit" && form.originalName
    ? { type: "updateMcpServer", originalName: form.originalName, server }
    : { type: "addMcpServer", server };

  setOperationPending(true);
  setOperationError(undefined);
  try {
    await dispatch(command);
    closeForm();
  } catch (error) {
    setOperationError(errorMessage(error));
  } finally {
    setOperationPending(false);
  }
}

async function deleteMcpServerByName(
  dispatch: (command: ClientCommand) => Promise<unknown>,
  serverName: string,
  setDeleteServer: (server?: string) => void,
  setOperationPending: (pending: boolean) => void,
  setOperationError: (message?: string) => void,
) {
  setOperationPending(true);
  setOperationError(undefined);
  try {
    await dispatch({ type: "deleteMcpServer", name: serverName });
    setDeleteServer(undefined);
  } catch (error) {
    setOperationError(errorMessage(error));
  } finally {
    setOperationPending(false);
  }
}

function serverEntries(config: ConfigView | undefined): Array<[string, McpServerView]> {
  return Object.entries(config?.mcpServers ?? {})
    .filter((entry): entry is [string, McpServerView] => entry[1] !== undefined)
    .sort(([left], [right]) => left.localeCompare(right));
}

function serverNameFromEditPath(pathname: string) {
  const match = /^\/config\/mcp\/edit\/([^/]+)/.exec(pathname);
  return match ? decodeURIComponent(match[1]!) : undefined;
}

function formToServer(form: McpServerForm): McpServerView {
  return {
    kind: form.kind,
    name: form.name,
    command: form.kind === "sse" ? "" : form.command,
    url: form.kind === "sse" ? form.url : "",
  };
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : "MCP server operation failed.";
}
