import { For, Show, createMemo, createSignal } from "solid-js";
import type { Accessor } from "solid-js";

import { useAppState } from "../../app/context";
import { Field, InlineAlert } from "../../components/ui";
import type { ClientCommand, ConfigView, InferenceDefaultsView } from "../../generated/waltr-client";

type InferenceDefaultsForm = {
  providerKey: string;
  modelId: string;
  reasoningEffort: string;
};

export function InferenceDefaultsSection(props: {
  watchedConfig: Accessor<ConfigView | undefined>;
}) {
  const app = useAppState();
  const [operationPending, setOperationPending] = createSignal(false);
  const [operationError, setOperationError] = createSignal<string>();
  const [draft, setDraft] = createSignal<InferenceDefaultsForm>();
  const config = props.watchedConfig;
  const form = createMemo(() => draft() ?? defaultsToForm(config()?.inferenceDefaults));
  const providers = createMemo(() => providerKeys(config()));
  const models = createMemo(() => modelKeys(config(), form().providerKey));
  const reasoningEfforts = createMemo(() => modelReasoningEfforts(config(), form().providerKey, form().modelId));
  const providerMissing = createMemo(() => form().providerKey !== "" && !providers().includes(form().providerKey));
  const modelMissing = createMemo(() => form().modelId !== "" && !models().includes(form().modelId));
  const reasoningMissing = createMemo(() => form().reasoningEffort !== "" && !reasoningEfforts().includes(form().reasoningEffort));
  const saveDisabled = createMemo(() => operationPending() || providerMissing() || modelMissing() || reasoningMissing());

  const update = (patch: Partial<InferenceDefaultsForm>) => {
    setDraft((current) => ({ ...form(), ...current, ...patch }));
  };

  return (
    <section class="config__section" aria-labelledby="inference-defaults-title">
      <div class="config__section-header">
        <div>
          <p class="eyebrow">Inference Defaults</p>
          <h2 id="inference-defaults-title">New session inference</h2>
        </div>
      </div>

      <Show when={operationError()}>
        {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
      </Show>

      <form
        class="config__form"
        onSubmit={(event) => {
          event.preventDefault();
          void saveDefaults(app.dispatchConfigPatch, formToDefaults(form()), setOperationPending, setOperationError, setDraft);
        }}
      >
        <Field id="inference-default-provider" label="Provider" error={providerMissing() ? "Selected provider is no longer configured." : undefined}>
          {(field) => (
            <select
              id={field.id}
              aria-describedby={field.describedBy}
              aria-invalid={field.invalid}
              value={form().providerKey}
              onChange={(event) => update({ providerKey: event.currentTarget.value, modelId: "", reasoningEffort: "" })}
            >
              <option value="">No provider default</option>
              <For each={providers()}>{(provider) => <option value={provider}>{provider}</option>}</For>
            </select>
          )}
        </Field>

        <Field id="inference-default-model" label="Model" error={modelMissing() ? "Selected model is no longer configured." : undefined}>
          {(field) => (
            <select
              id={field.id}
              aria-describedby={field.describedBy}
              aria-invalid={field.invalid}
              disabled={form().providerKey === ""}
              value={form().modelId}
              onChange={(event) => update({ modelId: event.currentTarget.value, reasoningEffort: "" })}
            >
              <option value="">No model selected</option>
              <For each={models()}>{(model) => <option value={model}>{model}</option>}</For>
            </select>
          )}
        </Field>

        <Field id="inference-default-reasoning" label="Reasoning effort" error={reasoningMissing() ? "Selected reasoning effort is no longer configured." : undefined}>
          {(field) => (
            <select
              id={field.id}
              aria-describedby={field.describedBy}
              aria-invalid={field.invalid}
              disabled={form().modelId === ""}
              value={form().reasoningEffort}
              onChange={(event) => update({ reasoningEffort: event.currentTarget.value })}
            >
              <option value="">No reasoning default</option>
              <For each={reasoningEfforts()}>{(effort) => <option value={effort}>{effort}</option>}</For>
            </select>
          )}
        </Field>

        <div class="config__form-actions">
          <button type="submit" disabled={saveDisabled()}>{operationPending() ? "Saving..." : "Save defaults"}</button>
          <button
            type="button"
            disabled={operationPending()}
            onClick={() => void saveDefaults(app.dispatchConfigPatch, null, setOperationPending, setOperationError, setDraft)}
          >
            Clear defaults
          </button>
        </div>
      </form>
    </section>
  );
}

function defaultsToForm(defaults: InferenceDefaultsView | null | undefined): InferenceDefaultsForm {
  return defaults
    ? {
        providerKey: defaults.providerKey,
        modelId: defaults.modelId,
        reasoningEffort: defaults.reasoningEffort ?? "",
      }
    : emptyDefaults();
}

function emptyDefaults(): InferenceDefaultsForm {
  return { providerKey: "", modelId: "", reasoningEffort: "" };
}

async function saveDefaults(
  dispatch: (command: ClientCommand) => Promise<unknown>,
  defaults: InferenceDefaultsView | null,
  setOperationPending: (pending: boolean) => void,
  setOperationError: (message?: string) => void,
  setDraft: (form?: InferenceDefaultsForm) => void,
) {
  setOperationPending(true);
  setOperationError(undefined);
  try {
    await dispatch({ type: "setInferenceDefaults", defaults });
    setDraft(undefined);
  } catch (error) {
    setOperationError(errorMessage(error));
  } finally {
    setOperationPending(false);
  }
}

function formToDefaults(form: InferenceDefaultsForm): InferenceDefaultsView {
  return {
    providerKey: form.providerKey,
    modelId: form.modelId,
    reasoningEffort: form.reasoningEffort || null,
  };
}

function providerKeys(config: ConfigView | undefined) {
  return Object.keys(config?.providers ?? {}).sort((left, right) => left.localeCompare(right));
}

function modelKeys(config: ConfigView | undefined, providerKey: string) {
  return Object.keys(config?.providers?.[providerKey]?.models ?? {}).sort((left, right) => left.localeCompare(right));
}

function modelReasoningEfforts(config: ConfigView | undefined, providerKey: string, modelId: string) {
  return [...(config?.providers?.[providerKey]?.models?.[modelId]?.options.reasoningEfforts ?? [])].sort((left, right) => left.localeCompare(right));
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : String(error);
}
