import { For, Show, createMemo, createSignal } from "solid-js";
import type { Accessor } from "solid-js";

import { useAppState } from "../../app/context";
import { ActionList, ActionRow, Field, InlineAlert } from "../../components/ui";
import type { ClientCommand, ConfigView, RuntimeDefaultsView } from "../../generated/waltr-client";

type RuntimeDefaultsForm = {
  providerKey: string;
  modelId: string;
  mcpDefaultEnabled: boolean;
  enabledMcpServerKeys: string[];
};

export function RuntimeDefaultsSection(props: {
  watchedConfig: Accessor<ConfigView | undefined>;
}) {
  const app = useAppState();
  const [operationPending, setOperationPending] = createSignal(false);
  const [operationError, setOperationError] = createSignal<string>();
  const [draft, setDraft] = createSignal<RuntimeDefaultsForm>();
  const config = props.watchedConfig;
  const form = createMemo(() => draft() ?? defaultsToForm(config()?.runtimeDefaults));
  const providers = createMemo(() => providerKeys(config()));
  const models = createMemo(() => modelKeys(config(), form().providerKey));
  const mcpServers = createMemo(() => mcpKeys(config()));
  const providerMissing = createMemo(() => form().providerKey !== "" && !providers().includes(form().providerKey));
  const modelMissing = createMemo(() => form().modelId !== "" && !models().includes(form().modelId));
  const saveDisabled = createMemo(() => operationPending() || providerMissing() || modelMissing());

  const update = (patch: Partial<RuntimeDefaultsForm>) => {
    setDraft((current) => ({ ...form(), ...current, ...patch }));
  };

  return (
    <section class="config__section" aria-labelledby="runtime-defaults-title">
      <div class="config__section-header">
        <div>
          <p class="eyebrow">Runtime Defaults</p>
          <h2 id="runtime-defaults-title">New session defaults</h2>
        </div>
      </div>

      <Show when={operationError()}>
        {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
      </Show>

      <form
        class="config__form"
        onSubmit={(event) => {
          event.preventDefault();
          void saveDefaults(app.dispatchConfigPatch, form(), setOperationPending, setOperationError, setDraft);
        }}
      >
        <Field id="runtime-default-provider" label="Provider">
          {(field) => (
            <select
              id={field.id}
              aria-describedby={field.describedBy}
              aria-invalid={field.invalid}
              value={form().providerKey}
              onChange={(event) => update({ providerKey: event.currentTarget.value, modelId: "" })}
            >
              <option value="">No provider default</option>
              <For each={providers()}>{(provider) => <option value={provider}>{provider}</option>}</For>
            </select>
          )}
        </Field>

        <Field id="runtime-default-model" label="Model" error={modelMissing() ? "Selected model is no longer configured." : undefined}>
          {(field) => (
            <select
              id={field.id}
              aria-describedby={field.describedBy}
              aria-invalid={field.invalid}
              disabled={form().providerKey === ""}
              value={form().modelId}
              onChange={(event) => update({ modelId: event.currentTarget.value })}
            >
              <option value="">No model selected</option>
              <For each={models()}>{(model) => <option value={model}>{model}</option>}</For>
            </select>
          )}
        </Field>

        <label class="config__form-checkbox runtime-defaults__all-mcp">
          <input
            type="checkbox"
            checked={form().mcpDefaultEnabled}
            onChange={(event) => update({ mcpDefaultEnabled: event.currentTarget.checked })}
          />
          <span>Enable all configured MCP servers by default</span>
        </label>

        <div class="config__models" aria-labelledby="runtime-defaults-mcp-title">
          <div class="config__models-header">
            <div>
              <p class="eyebrow">MCP Servers</p>
              <h3 id="runtime-defaults-mcp-title">Explicit MCP defaults</h3>
            </div>
          </div>
          <ActionList class="config__list" label="Explicit MCP defaults">
            <Show when={mcpServers().length > 0} fallback={<p class="panel">No MCP servers configured yet.</p>}>
              <For each={mcpServers()}>
                {(server) => {
                  const enabled = createMemo(() => form().enabledMcpServerKeys.includes(server));
                  const active = createMemo(() => form().mcpDefaultEnabled || enabled());

                  return (
                    <ActionRow class="provider-row provider-row--model" selected={active()}>
                      <label class="provider-row__summary runtime-defaults__mcp-row">
                        <input
                          type="checkbox"
                          disabled={form().mcpDefaultEnabled}
                          checked={enabled()}
                          onChange={(event) => update({
                            enabledMcpServerKeys: toggleKey(form().enabledMcpServerKeys, server, event.currentTarget.checked),
                          })}
                        />
                        <strong>{server}</strong>
                        <span>{active() ? "enabled" : "disabled"}</span>
                        <code>{form().mcpDefaultEnabled ? "All MCP servers" : "Explicit default"}</code>
                      </label>
                    </ActionRow>
                  );
                }}
              </For>
            </Show>
          </ActionList>
        </div>

        <div class="config__form-actions">
          <button type="submit" disabled={saveDisabled()}>Save defaults</button>
          <button
            type="button"
            disabled={operationPending()}
            onClick={() => void saveDefaults(app.dispatchConfigPatch, emptyDefaults(), setOperationPending, setOperationError, setDraft)}
          >
            Clear defaults
          </button>
        </div>
      </form>
    </section>
  );
}

function defaultsToForm(defaults: RuntimeDefaultsView | null | undefined): RuntimeDefaultsForm {
  return defaults
    ? {
        providerKey: defaults.providerKey,
        modelId: defaults.modelId,
        mcpDefaultEnabled: defaults.mcpDefaultEnabled,
        enabledMcpServerKeys: defaults.enabledMcpServerKeys ?? [],
      }
    : emptyDefaults();
}

function emptyDefaults(): RuntimeDefaultsForm {
  return {
    providerKey: "",
    modelId: "",
    mcpDefaultEnabled: false,
    enabledMcpServerKeys: [],
  };
}

async function saveDefaults(
  dispatch: (command: ClientCommand) => Promise<unknown>,
  form: RuntimeDefaultsForm,
  setOperationPending: (pending: boolean) => void,
  setOperationError: (message?: string) => void,
  setDraft: (form?: RuntimeDefaultsForm) => void,
) {
  setOperationPending(true);
  setOperationError(undefined);
  try {
    await dispatch({ type: "setRuntimeDefaults", defaults: formToDefaults(form) });
    setDraft(undefined);
  } catch (error) {
    setOperationError(errorMessage(error));
  } finally {
    setOperationPending(false);
  }
}

function formToDefaults(form: RuntimeDefaultsForm): RuntimeDefaultsView {
  return {
    providerKey: form.providerKey,
    modelId: form.modelId,
    mcpDefaultEnabled: form.mcpDefaultEnabled,
    enabledMcpServerKeys: form.mcpDefaultEnabled ? [] : form.enabledMcpServerKeys,
  };
}

function providerKeys(config: ConfigView | undefined) {
  return Object.keys(config?.providers ?? {}).sort((left, right) => left.localeCompare(right));
}

function modelKeys(config: ConfigView | undefined, providerKey: string) {
  return Object.keys(config?.providers?.[providerKey]?.models ?? {}).sort((left, right) => left.localeCompare(right));
}

function mcpKeys(config: ConfigView | undefined) {
  return Object.keys(config?.mcpServers ?? {}).sort((left, right) => left.localeCompare(right));
}

function toggleKey(keys: readonly string[], key: string, enabled: boolean) {
  const set = new Set(keys);
  if (enabled) {
    set.add(key);
  } else {
    set.delete(key);
  }
  return [...set].sort((left, right) => left.localeCompare(right));
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
