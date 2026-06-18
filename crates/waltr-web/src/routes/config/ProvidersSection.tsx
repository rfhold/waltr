import { useLocation, useNavigate, useParams } from "@solidjs/router";
import {
  For,
  Show,
  createContext,
  createEffect,
  createMemo,
  createSignal,
  on,
  useContext,
} from "solid-js";
import type { Accessor, ParentProps } from "solid-js";

import { useAppState } from "../../app/context";
import { ActionList, ActionRow, ConfirmAction, Dialog, Field, InlineAlert } from "../../components/ui";
import type {
  ClientCommand,
  ConfigView,
  ModelTypeView,
  ModelView,
  ProviderKindView,
  ProviderView,
} from "../../generated/waltr-client";

type FormMode = "create" | "edit";

type ProviderForm = {
  mode: FormMode;
  originalName?: string;
  name: string;
  kind: ProviderKindView;
  url: string;
  headers: ProviderView["headers"];
  models: ProviderView["models"];
};

type ActiveForm =
  | { mode: "create" }
  | { mode: "edit"; providerName: string; provider: ProviderView };

type ModelActiveForm =
  | { mode: "create" }
  | { mode: "edit"; modelKey: string; model: ModelView };

type ModelForm = {
  mode: FormMode;
  originalKey?: string;
  key: string;
  name: string;
  kind: ModelTypeView | "";
  temperature: string;
  topP: string;
  topK: string;
  minP: string;
  presencePenalty: string;
  repetitionPenalty: string;
  context: string;
  output: string;
  tools: boolean;
  image: boolean;
};

type ProvidersSectionState = {
  watchedConfig: Accessor<ConfigView | undefined>;
  closeForm: () => void;
  closeModelForm: () => void;
  currentProviderDraft: Accessor<ProviderForm | undefined>;
  operationError: Accessor<string | undefined>;
  operationPending: Accessor<boolean>;
  saveProvider: (form: ProviderForm) => Promise<void>;
  stageModelDelete: (modelKey: string) => void;
  stageModelSave: (form: ModelForm) => void;
  setDeleteModel: (model?: string) => void;
  setDeleteProvider: (provider?: string) => void;
  setSelectedProvider: (provider?: string) => void;
};

const ProvidersSectionContext = createContext<ProvidersSectionState>();

export function ProvidersSection(props: ParentProps & {
  watchedConfig: Accessor<ConfigView | undefined>;
}) {
  const app = useAppState();
  const location = useLocation();
  const navigate = useNavigate();
  const watchedConfig = props.watchedConfig;
  const [selectedProvider, setSelectedProvider] = createSignal<string>();
  const [deleteProvider, setDeleteProvider] = createSignal<string>();
  const [operationError, setOperationError] = createSignal<string>();
  const [operationPending, setOperationPending] = createSignal(false);
  const [deleteModel, setDeleteModel] = createSignal<string>();
  const [providerFormDraft, setProviderFormDraft] = createSignal<ProviderForm>();
  const routeProviderName = createMemo(() => providerNameFromEditPath(location.pathname));
  const showingModelFormRoute = createMemo(() => isProviderModelFormPath(location.pathname));
  const activeProviderForm = createMemo<ActiveForm | undefined>(() => {
    if (isCreateProviderFormPath(location.pathname)) {
      return { mode: "create" };
    }

    const name = routeProviderName();
    const provider = name ? watchedConfig()?.providers[name] : undefined;
    return provider ? { mode: "edit", providerName: name!, provider } : undefined;
  });
  const activeProviderFormKey = createMemo(() => {
    const active = activeProviderForm();
    return active?.mode === "create" ? "create" : active ? `edit:${active.providerName}` : undefined;
  });
  const visibleProviderName = createMemo(() =>
    showingModelFormRoute()
      ? (routeProviderName() ?? selectedProvider())
      : selectedProvider(),
  );

  createEffect(() => {
    const config = watchedConfig();
    const providers = providerEntries(config);
    if (!config || providers.length === 0) {
      setSelectedProvider(undefined);
      return;
    }

    const routeProvider = routeProviderName();
    if (routeProvider && providers.some(([name]) => name === routeProvider)) {
      setSelectedProvider(routeProvider);
      return;
    }

    setSelectedProvider((selected) =>
      selected && providers.some(([name]) => name === selected)
        ? selected
        : providers[0]?.[0],
    );
  });

  createEffect(on(activeProviderFormKey, () => {
    const active = activeProviderForm();
    setDeleteModel(undefined);
    setProviderFormDraft(active ? initialProviderForm(active) : undefined);
  }));

  const closeModelForm = () => navigate(providerFormBasePath(providerFormDraft()));
  const stageModelSave = (modelForm: ModelForm) => {
    setProviderFormDraft((draft) => draft ? providerFormWithModel(draft, modelForm) : draft);
    setDeleteModel(undefined);
    closeModelForm();
  };
  const stageModelDelete = (modelKey: string) => {
    setProviderFormDraft((draft) => {
      if (!draft) {
        return draft;
      }

      const models = { ...(draft.models ?? {}) };
      delete models[modelKey];
      return { ...draft, models };
    });
    setDeleteModel(undefined);
  };

  const sectionState: ProvidersSectionState = {
    watchedConfig,
    closeForm: () => closeForm(navigate),
    closeModelForm,
    currentProviderDraft: providerFormDraft,
    operationError,
    operationPending,
    saveProvider: (form) => saveProvider(app.dispatchConfigPatch, form, navigate, setOperationPending, setOperationError),
    stageModelDelete,
    stageModelSave,
    setDeleteModel,
    setDeleteProvider,
    setSelectedProvider,
  };

  return (
    <ProvidersSectionContext.Provider value={sectionState}>
      <div class="config__actions" aria-label="Provider actions">
        <button
          type="button"
          onClick={() => {
            setDeleteProvider(undefined);
            setDeleteModel(undefined);
            navigate("/config/provider/create");
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
          <ActionList class="config__list" label="Provider list">
            <Show
              when={providerEntries(currentConfig).length > 0}
              fallback={<p class="panel">No providers configured yet.</p>}
            >
              <For each={providerEntries(currentConfig)}>
                {([providerName, provider]) => (
                  <ActionRow class="provider-row" selected={visibleProviderName() === providerName}>
                    <button
                      type="button"
                      class="provider-row__summary"
                      aria-pressed={visibleProviderName() === providerName}
                      onClick={() => setSelectedProvider(providerName)}
                    >
                      <strong>{providerName}</strong>
                      <span>{provider.kind}</span>
                      <code>{provider.url || "No URL configured"}</code>
                    </button>
                    <Show
                      when={deleteProvider() === providerName}
                      fallback={
                        <div class="provider-row__actions">
                          <button
                            type="button"
                            aria-label={`Edit ${providerName}`}
                            onClick={() => {
                              setSelectedProvider(providerName);
                              setDeleteProvider(undefined);
                              navigate(`/config/provider/edit/${encodeURIComponent(providerName)}`);
                            }}
                          >
                            Edit
                          </button>
                          <button
                            type="button"
                            aria-label={`Delete ${providerName}`}
                            onClick={() => {
                              setSelectedProvider(providerName);
                              setDeleteProvider(providerName);
                            }}
                          >
                            Delete
                          </button>
                        </div>
                      }
                    >
                      <ConfirmAction
                        label={`Delete ${providerName} confirmation`}
                        prompt="Delete?"
                        confirmLabel="Confirm"
                        confirmAriaLabel={`Confirm delete ${providerName}`}
                        cancelAriaLabel={`Cancel delete ${providerName}`}
                        pending={operationPending()}
                        onConfirm={() => deleteProviderByName(app.dispatchConfigPatch, providerName, setDeleteProvider, setOperationPending, setOperationError)}
                        onCancel={() => setDeleteProvider(undefined)}
                      />
                    </Show>
                  </ActionRow>
                )}
              </For>
            </Show>
          </ActionList>
        )}
      </Show>
      <Show when={providerFormDraft()}>
        {(draft) => (
          <ProviderFormPanel
            deleteModel={deleteModel()}
            form={draft()}
            onCancel={sectionState.closeForm}
            onDeleteModel={stageModelDelete}
            onModelDeletePrompt={setDeleteModel}
            onModelRoute={(path) => navigate(path)}
            onSubmit={sectionState.saveProvider}
            pending={operationPending()}
            setForm={setProviderFormDraft}
          />
        )}
      </Show>
      <Show when={activeProviderForm()}>
        {props.children}
      </Show>
      <Show when={!activeProviderForm()}>
        {props.children}
      </Show>
    </ProvidersSectionContext.Provider>
  );
}

export function ConfigCreateRoute() {
  const section = useProvidersSectionState();

  createEffect(() => section.setDeleteProvider(undefined));

  return null;
}

export function ConfigEditRoute() {
  const section = useProvidersSectionState();
  const params = useParams<{ provider: string }>();
  const providerName = createMemo(() => params.provider);

  createEffect(on(providerName, (name) => {
    section.setDeleteProvider(undefined);
    section.setSelectedProvider(name);
  }));

  return null;
}

export function ConfigModelCreateRoute() {
  const section = useProvidersSectionState();

  createEffect(() => {
    section.setDeleteModel(undefined);
  });

  return (
    <Show when={section.currentProviderDraft()} keyed>
      {(_draft) => (
        <ModelFormPanel
          form={{ mode: "create" }}
          onCancel={section.closeModelForm}
          onSubmit={async (form) => section.stageModelSave(form)}
          pending={section.operationPending()}
        />
      )}
    </Show>
  );
}

export function ConfigModelEditRoute() {
  const section = useProvidersSectionState();
  const params = useParams<{ model: string }>();
  const modelKey = createMemo(() => params.model);
  const activeForm = createMemo<ModelActiveForm | undefined>(() => {
    const model = section.currentProviderDraft()?.models?.[modelKey()];
    return model
      ? { mode: "edit", modelKey: modelKey(), model }
      : undefined;
  });

  createEffect(() => {
    section.setDeleteModel(undefined);
  });

  return (
    <Show when={activeForm()} keyed>
      {(modelForm) => (
        <ModelFormPanel
          form={modelForm}
          onCancel={section.closeModelForm}
          onSubmit={async (form) => section.stageModelSave(form)}
          pending={section.operationPending()}
        />
      )}
    </Show>
  );
}

function useProvidersSectionState() {
  const state = useContext(ProvidersSectionContext);
  if (!state) {
    throw new Error("Providers section child rendered outside ProvidersSection");
  }
  return state;
}

function ProviderFormPanel(props: {
  deleteModel?: string;
  form: ProviderForm;
  onCancel: () => void;
  onDeleteModel: (modelKey: string) => void;
  onModelDeletePrompt: (model?: string) => void;
  onModelRoute: (path: string) => void;
  onSubmit: (form: ProviderForm) => Promise<void>;
  pending: boolean;
  setForm: (next: ProviderForm | ((current: ProviderForm | undefined) => ProviderForm | undefined)) => void;
}) {
  const form = () => props.form;
  const update = (patch: Partial<ProviderForm>) => props.setForm((current) => current ? { ...current, ...patch } : current);
  const title = () => props.form.mode === "create" ? "Create provider" : "Edit provider";
  const modelRouteBase = () => providerFormBasePath(props.form);

  return (
    <Dialog title={title()} onClose={props.onCancel} class="panel config__provider-form-panel">
        <form
          class="config__form"
          aria-label={title()}
          onSubmit={(event) => {
            event.preventDefault();
            void props.onSubmit(form());
          }}
        >
          <Field id="provider-name" label="Name" required hint="Use lowercase letters, numbers, dots, underscores, or dashes; no spaces.">
            {(field) => (
              <input name="name" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} value={form().name} required pattern="[a-z0-9._-]+" title="Use lowercase letters, numbers, dots, underscores, or dashes; no spaces." onInput={(event) => update({ name: event.currentTarget.value })} />
            )}
          </Field>
          <Field id="provider-kind" label="Type">
            {(field) => (
              <select name="kind" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} value={form().kind} onChange={(event) => update({ kind: event.currentTarget.value as ProviderKindView })}>
                <option value="anthropic">Anthropic</option>
                <option value="openAi">OpenAI</option>
              </select>
            )}
          </Field>
          <Field id="provider-url" label="URL">
            {(field) => (
              <input name="url" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="url" value={form().url} onInput={(event) => update({ url: event.currentTarget.value })} />
            )}
          </Field>
          <div class="config__form-actions">
            <button type="submit" disabled={props.pending}>Save</button>
            <button type="button" disabled={props.pending} onClick={props.onCancel}>Cancel</button>
          </div>
        </form>
        <div class="config__models" aria-label={`Models for ${form().name || "new provider"}`}>
          <div class="config__models-header">
            <h3>Models</h3>
            <div class="config__actions" aria-label="Model actions">
              <button
                type="button"
                onClick={() => {
                  props.onModelDeletePrompt(undefined);
                  props.onModelRoute(`${modelRouteBase()}/models/create`);
                }}
              >
                Add model
              </button>
            </div>
          </div>
          <Show
            when={modelEntriesForForm(form()).length > 0}
            fallback={<p class="panel">No models configured yet.</p>}
          >
            <ActionList class="config__list" label={`Model list for ${form().name || "new provider"}`}>
              <For each={modelEntriesForForm(form())}>
                {([modelKey, model]) => (
                  <ActionRow class="provider-row provider-row--model">
                    <div class="provider-row__summary">
                      <strong>{model.name || modelKey}</strong>
                      <span>{model.kind}</span>
                    </div>
                    <Show
                      when={props.deleteModel === modelKey}
                      fallback={
                        <div class="provider-row__actions">
                          <button
                            type="button"
                            aria-label={`Edit model ${modelKey}`}
                            onClick={() => {
                              props.onModelDeletePrompt(undefined);
                              props.onModelRoute(`${modelRouteBase()}/models/edit/${encodeURIComponent(modelKey)}`);
                            }}
                          >
                            Edit
                          </button>
                          <button
                            type="button"
                            aria-label={`Delete model ${modelKey}`}
                            onClick={() => props.onModelDeletePrompt(modelKey)}
                          >
                            Delete
                          </button>
                        </div>
                      }
                    >
                      <ConfirmAction
                        label={`Delete model ${modelKey} confirmation`}
                        prompt="Delete?"
                        confirmLabel="Confirm"
                        confirmAriaLabel={`Confirm delete model ${modelKey}`}
                        cancelAriaLabel={`Cancel delete model ${modelKey}`}
                        pending={props.pending}
                        onConfirm={() => props.onDeleteModel(modelKey)}
                        onCancel={() => props.onModelDeletePrompt(undefined)}
                      />
                    </Show>
                  </ActionRow>
                )}
              </For>
            </ActionList>
          </Show>
        </div>
    </Dialog>
  );
}

function ModelFormPanel(props: {
  form: ModelActiveForm;
  onCancel: () => void;
  onSubmit: (form: ModelForm) => Promise<void>;
  pending: boolean;
}) {
  const [form, setForm] = createSignal(initialModelForm(props.form));
  const update = (patch: Partial<ModelForm>) => setForm({ ...form(), ...patch });
  const title = () => form().mode === "create" ? "Create model" : "Edit model";
  const typeMissing = createMemo(() => {
    const kind = form().kind;
    return kind === "" || kind === "unspecified";
  });

  return (
    <Dialog title={title()} onClose={props.onCancel} class="panel config__form config__model-dialog">
      <form
        class="config__form"
        aria-label={title()}
        onSubmit={(event) => {
          event.preventDefault();
          if (typeMissing()) {
            return;
          }
          void props.onSubmit(form());
        }}
      >
        <Field id="model-key" label="Key" required hint="Use lowercase letters, numbers, dots, underscores, or dashes; no spaces.">
          {(field) => (
            <input name="key" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} value={form().key} required pattern="[a-z0-9._-]+" title="Use lowercase letters, numbers, dots, underscores, or dashes; no spaces." onInput={(event) => update({ key: event.currentTarget.value })} />
          )}
        </Field>
        <Field id="model-name" label="Display name">
          {(field) => (
            <input name="name" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} value={form().name} onInput={(event) => update({ name: event.currentTarget.value })} />
          )}
        </Field>
        <Field id="model-kind" label="Type" required error={typeMissing() ? "A model type is required." : undefined}>
          {(field) => (
            <select name="kind" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} value={form().kind} onChange={(event) => update({ kind: event.currentTarget.value as ModelTypeView | "" })}>
              <option value="">Select a type…</option>
              <option value="llm">LLM</option>
              <option value="embedding">Embedding</option>
              <option value="stt">Speech-to-text</option>
              <option value="tts">Text-to-speech</option>
            </select>
          )}
        </Field>
        <Field id="model-temperature" label="Temperature">
          {(field) => <input name="temperature" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="number" step="any" value={form().temperature} onInput={(event) => update({ temperature: event.currentTarget.value })} />}
        </Field>
        <Field id="model-top-p" label="Top P">
          {(field) => <input name="topP" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="number" step="any" value={form().topP} onInput={(event) => update({ topP: event.currentTarget.value })} />}
        </Field>
        <Field id="model-top-k" label="Top K">
          {(field) => <input name="topK" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="number" step="any" value={form().topK} onInput={(event) => update({ topK: event.currentTarget.value })} />}
        </Field>
        <Field id="model-min-p" label="Min P">
          {(field) => <input name="minP" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="number" step="any" value={form().minP} onInput={(event) => update({ minP: event.currentTarget.value })} />}
        </Field>
        <Field id="model-presence-penalty" label="Presence penalty">
          {(field) => <input name="presencePenalty" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="number" step="any" value={form().presencePenalty} onInput={(event) => update({ presencePenalty: event.currentTarget.value })} />}
        </Field>
        <Field id="model-repetition-penalty" label="Repetition penalty">
          {(field) => <input name="repetitionPenalty" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="number" step="any" value={form().repetitionPenalty} onInput={(event) => update({ repetitionPenalty: event.currentTarget.value })} />}
        </Field>
        <Field id="model-context" label="Context limit">
          {(field) => <input name="context" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="number" step="any" value={form().context} onInput={(event) => update({ context: event.currentTarget.value })} />}
        </Field>
        <Field id="model-output" label="Output limit">
          {(field) => <input name="output" id={field.id} aria-describedby={field.describedBy} aria-invalid={field.invalid} type="number" step="any" value={form().output} onInput={(event) => update({ output: event.currentTarget.value })} />}
        </Field>
        <label class="config__form-checkbox">
          <input name="tools" type="checkbox" checked={form().tools} onChange={(event) => update({ tools: event.currentTarget.checked })} />
          <span>Supports tools</span>
        </label>
        <label class="config__form-checkbox">
          <input name="image" type="checkbox" checked={form().image} onChange={(event) => update({ image: event.currentTarget.checked })} />
          <span>Supports image input</span>
        </label>
        <div class="config__form-actions">
          <button type="submit" disabled={props.pending || typeMissing()}>Save</button>
          <button type="button" disabled={props.pending} onClick={props.onCancel}>Cancel</button>
        </div>
      </form>
    </Dialog>
  );
}

function initialProviderForm(form: ActiveForm): ProviderForm {
  if (form.mode === "create") {
    return { mode: "create", name: "", kind: "anthropic", url: "", headers: {}, models: {} };
  }

  return providerToForm(form.providerName, form.provider);
}

function providerToForm(providerName: string, provider: ProviderView): ProviderForm {
  return {
    mode: "edit",
    originalName: providerName,
    name: providerName,
    kind: provider.kind,
    url: provider.url,
    headers: provider.headers,
    models: provider.models ?? {},
  };
}

async function saveProvider(
  dispatch: (command: ClientCommand) => Promise<unknown>,
  form: ProviderForm,
  navigate: (path: string) => void,
  setOperationPending: (pending: boolean) => void,
  setOperationError: (message?: string) => void,
) {
  const provider = formToProvider(form);
  const command: ClientCommand = form.mode === "edit" && form.originalName
    ? { type: "updateProvider", originalName: form.originalName, provider }
    : { type: "addProvider", provider };

  setOperationPending(true);
  setOperationError(undefined);
  try {
    await dispatch(command);
    closeForm(navigate);
  } catch (error) {
    setOperationError(errorMessage(error));
  } finally {
    setOperationPending(false);
  }
}

function closeForm(navigate: (path: string) => void) {
  navigate("/config/provider");
}

function isProviderModelFormPath(pathname: string) {
  return /^\/config\/provider\/(?:create|edit\/[^/]+)\/models\/(?:create|edit\/[^/]+)\/?$/.test(pathname);
}

function isCreateProviderFormPath(pathname: string) {
  return /^\/config\/provider\/create(?:\/models\/(?:create|edit\/[^/]+))?\/?$/.test(pathname);
}

function providerNameFromEditPath(pathname: string) {
  const match = /^\/config\/provider\/edit\/([^/]+)/.exec(pathname);
  return match ? decodeURIComponent(match[1]!) : undefined;
}

async function deleteProviderByName(
  dispatch: (command: ClientCommand) => Promise<unknown>,
  providerName: string,
  setDeleteProvider: (provider?: string) => void,
  setOperationPending: (pending: boolean) => void,
  setOperationError: (message?: string) => void,
) {
  setOperationPending(true);
  setOperationError(undefined);
  try {
    await dispatch({ type: "deleteProvider", name: providerName });
    setDeleteProvider(undefined);
  } catch (error) {
    setOperationError(errorMessage(error));
  } finally {
    setOperationPending(false);
  }
}

function providerEntries(config: ConfigView | undefined): Array<[string, ProviderView]> {
  return Object.entries(config?.providers ?? {})
    .filter((entry): entry is [string, ProviderView] => entry[1] !== undefined)
    .sort(([left], [right]) => left.localeCompare(right));
}

function formToProvider(form: ProviderForm): ProviderView {
  return {
    kind: form.kind,
    name: form.name,
    url: form.url,
    headers: form.headers,
    models: form.models ?? {},
  };
}

function modelEntriesForForm(form: ProviderForm): Array<[string, ModelView]> {
  return Object.entries(form.models ?? {})
    .filter((entry): entry is [string, ModelView] => entry[1] !== undefined)
    .sort(([left], [right]) => left.localeCompare(right));
}

function providerFormBasePath(form: ProviderForm | undefined) {
  if (!form) {
    return "/config/provider";
  }

  if (form.mode === "create") {
    return "/config/provider/create";
  }

  return `/config/provider/edit/${encodeURIComponent(form.originalName ?? form.name)}`;
}

function providerFormWithModel(providerForm: ProviderForm, modelForm: ModelForm): ProviderForm {
  const models = { ...(providerForm.models ?? {}) };
  if (modelForm.mode === "edit" && modelForm.originalKey && modelForm.originalKey !== modelForm.key) {
    delete models[modelForm.originalKey];
  }
  models[modelForm.key] = formToModel(modelForm);
  return { ...providerForm, models };
}

function initialModelForm(form: ModelActiveForm): ModelForm {
  if (form.mode === "create") {
    return {
      mode: "create",
      key: "",
      name: "",
      kind: "",
      temperature: "",
      topP: "",
      topK: "",
      minP: "",
      presencePenalty: "",
      repetitionPenalty: "",
      context: "",
      output: "",
      tools: false,
      image: false,
    };
  }

  const model = form.model;
  return {
    mode: "edit",
    originalKey: form.modelKey,
    key: form.modelKey,
    name: model.name,
    kind: model.kind,
    temperature: numberToInput(model.options.temperature),
    topP: numberToInput(model.options.topP),
    topK: numberToInput(model.options.topK),
    minP: numberToInput(model.options.minP),
    presencePenalty: numberToInput(model.options.presencePenalty),
    repetitionPenalty: numberToInput(model.options.repetitionPenalty),
    context: numberToInput(model.limits.context),
    output: numberToInput(model.limits.output),
    tools: model.capabilities.tools,
    image: model.capabilities.image,
  };
}

function numberToInput(value: number | null): string {
  return value === null || value === undefined ? "" : String(value);
}

function inputToNumber(value: string): number | null {
  const trimmed = value.trim();
  if (trimmed.length === 0) {
    return null;
  }
  const parsed = Number(trimmed);
  return Number.isFinite(parsed) ? parsed : null;
}

function formToModel(form: ModelForm): ModelView {
  return {
    name: form.name,
    kind: (form.kind === "" ? "unspecified" : form.kind) as ModelTypeView,
    options: {
      temperature: inputToNumber(form.temperature),
      topP: inputToNumber(form.topP),
      topK: inputToNumber(form.topK),
      minP: inputToNumber(form.minP),
      presencePenalty: inputToNumber(form.presencePenalty),
      repetitionPenalty: inputToNumber(form.repetitionPenalty),
    },
    limits: {
      context: inputToNumber(form.context),
      output: inputToNumber(form.output),
    },
    capabilities: {
      tools: form.tools,
      image: form.image,
    },
  };
}

function errorMessage(error: unknown) {
  return error instanceof Error ? error.message : "Provider operation failed.";
}
