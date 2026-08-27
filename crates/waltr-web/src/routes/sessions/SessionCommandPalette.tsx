import { useNavigate } from "@solidjs/router";
import { For, Show, createEffect, createMemo, createSignal, onCleanup, onMount } from "solid-js";

import { useAppState } from "../../app/context";
import { clientRequestId } from "../../app/requestId";
import { Dialog, Field, InlineAlert } from "../../components/ui";
import type { ClientManualRoutineDefinition, ClientRoutineInputDefinition, ClientRoutineValue } from "../../generated/waltr-client";

type Drafts = Record<string, string | boolean>;
type FieldErrors = Record<string, string>;

export function SessionCommandPalette(props: { sessionId: string }) {
  const app = useAppState();
  const navigate = useNavigate();
  const [open, setOpen] = createSignal(false);
  const [search, setSearch] = createSignal("");
  const [selected, setSelected] = createSignal<ClientManualRoutineDefinition>();
  const [drafts, setDrafts] = createSignal<Drafts>({});
  const [localErrors, setLocalErrors] = createSignal<FieldErrors>({});
  const [clearedServerErrors, setClearedServerErrors] = createSignal<Record<string, boolean>>({});
  const [operationKey, setOperationKey] = createSignal<string>();
  const [submitting, setSubmitting] = createSignal(false);
  const [dispatchFailed, setDispatchFailed] = createSignal(false);
  let trigger: HTMLButtonElement | undefined;
  let watchesRegistered = false;

  const routines = createMemo(() => app.snapshot()?.routines);
  const definitions = createMemo(() => routines()?.definitions.definitions ?? []);
  const filteredDefinitions = createMemo(() => {
    const query = search().trim().toLocaleLowerCase();
    if (!query) return definitions();
    return definitions().filter((definition) => definition.displayName.toLocaleLowerCase().includes(query)
      || definition.description.toLocaleLowerCase().includes(query));
  });
  const operation = createMemo(() => {
    const key = operationKey();
    const current = key ? routines()?.starts[key] : undefined;
    const definition = selected();
    return current
      && current.operationKey === key
      && current.sourceSessionId === props.sessionId
      && current.routineName === definition?.routineName
      && current.triggerName === definition.triggerName
      ? current
      : undefined;
  });
  const invocationId = createMemo(() => operation()?.invocationId ?? undefined);
  const pending = createMemo(() => submitting() || operation()?.status === "loading");
  const serverFieldErrors = createMemo(() => {
    const errors: FieldErrors = {};
    const cleared = clearedServerErrors();
    for (const violation of operation()?.validationFailure?.violations ?? []) {
      const name = violation.fieldPath.startsWith("inputs.") ? violation.fieldPath.slice("inputs.".length) : violation.fieldPath;
      if (selected()?.inputs.some((input) => input.name === name) && !cleared[name]) errors[name] ??= violation.message;
    }
    return errors;
  });
  const generalViolations = createMemo(() => (operation()?.validationFailure?.violations ?? []).filter((violation) => {
    const name = violation.fieldPath.startsWith("inputs.") ? violation.fieldPath.slice("inputs.".length) : violation.fieldPath;
    return !selected()?.inputs.some((input) => input.name === name);
  }));

  createEffect(() => {
    if (!open() || watchesRegistered || !app.clientLoaded() || !app.systemReady()) return;
    watchesRegistered = true;
    void app.dispatch({ type: "registerRoutineDefinitionWatchInterest" }).catch(() => undefined);
  });

  createEffect(() => {
    const status = operation()?.status;
    if (status === "ready" || status === "failed") setSubmitting(false);
  });

  const releaseWatches = () => {
    if (!watchesRegistered) return;
    watchesRegistered = false;
    void app.dispatch({ type: "releaseRoutineDefinitionWatchInterest" }).catch(() => undefined);
  };

  const openPalette = () => {
    setOpen(true);
    setSearch("");
    setSelected(undefined);
    setOperationKey(undefined);
    setDispatchFailed(false);
  };

  const closePalette = () => {
    if (pending()) return;
    setOpen(false);
    setSelected(undefined);
    setOperationKey(undefined);
    releaseWatches();
    queueMicrotask(() => trigger?.focus());
  };

  const handleDialogClose = () => {
    if (pending()) return;
    if (selected() && !invocationId()) {
      setSelected(undefined);
      setOperationKey(undefined);
      setDispatchFailed(false);
      queueMicrotask(() => document.querySelector<HTMLInputElement>("#session-command-search")?.focus());
      return;
    }
    closePalette();
  };

  const selectDefinition = (definition: ClientManualRoutineDefinition) => {
    setSelected(definition);
    setDrafts(Object.fromEntries(definition.inputs.map((input) => [input.name, input.defaultValue?.value ?? (input.type === "boolean" ? false : "")])));
    setLocalErrors({});
    setClearedServerErrors({});
    setOperationKey(undefined);
    setDispatchFailed(false);
    queueMicrotask(() => document.getElementById(fieldId(definition.inputs[0]?.name))?.focus());
  };

  const editDraft = (name: string, value: string | boolean) => {
    setDrafts((current) => ({ ...current, [name]: value }));
    setLocalErrors((current) => omit(current, name));
    setClearedServerErrors((current) => ({ ...current, [name]: true }));
  };

  const submit = () => {
    const definition = selected();
    if (!definition || pending()) return;
    const errors = validateInputs(definition.inputs, drafts());
    setLocalErrors(errors);
    if (Object.keys(errors).length > 0) return;

    const nextOperationKey = clientRequestId(`${props.sessionId}-routine-operation`);
    const inputs = Object.fromEntries(definition.inputs.map((input) => [input.name, routineValue(input, drafts()[input.name])]));
    setOperationKey(nextOperationKey);
    setClearedServerErrors({});
    setDispatchFailed(false);
    setSubmitting(true);
    void app.dispatch({
      type: "startManualRoutine",
      operationKey: nextOperationKey,
      requestId: clientRequestId(`${props.sessionId}-routine-request`),
      routineName: definition.routineName,
      triggerName: definition.triggerName,
      sourceSessionId: props.sessionId,
      inputs,
    }).then((snapshot) => {
      if (snapshot.routines.starts[nextOperationKey]?.status !== "loading") setSubmitting(false);
    }).catch(() => {
      setSubmitting(false);
      if (operationKey() === nextOperationKey) setDispatchFailed(true);
    });
  };

  const handleShortcut = (event: KeyboardEvent) => {
    if (event.key.toLocaleLowerCase() !== "p" || !event.ctrlKey || event.metaKey || event.altKey || event.shiftKey || open()) return;
    if (!app.clientLoaded() || !app.systemReady()) return;
    if (isEditable(event.target) || document.querySelector('[role="dialog"][aria-modal="true"]')) return;
    event.preventDefault();
    openPalette();
  };

  onMount(() => document.addEventListener("keydown", handleShortcut));
  onCleanup(() => {
    document.removeEventListener("keydown", handleShortcut);
    releaseWatches();
  });

  return (
    <>
      <button ref={trigger} type="button" class="sessions__detail-status-button" onClick={openPalette}>Commands</button>
      <Show when={open()}>
        <Dialog title="Session commands" class="sessions__commands-dialog" onClose={handleDialogClose}>
          <Show when={selected()} fallback={
            <div class="sessions__commands-list-view">
              <Field id="session-command-search" label="Search commands">
                {(field) => <input {...fieldAttributes(field)} value={search()} onInput={(event) => setSearch(event.currentTarget.value)} />}
              </Field>
              <Show when={["idle", "loading", "opened", "waiting"].includes(routines()?.definitions.watch.status ?? "idle")}><p>Loading session commands...</p></Show>
              <Show when={routines()?.definitions.watch.status === "failed" && definitions().length === 0}><InlineAlert tone="danger">Unable to load session commands.</InlineAlert></Show>
              <Show when={routines()?.definitions.watch.status === "failed" && definitions().length > 0}><p role="status">Reconnecting to session commands...</p></Show>
              <Show when={routines()?.definitions.watch.status === "received" && definitions().length === 0}><p>No session commands available.</p></Show>
              <Show when={definitions().length > 0 && filteredDefinitions().length === 0}><p>No matching session commands.</p></Show>
              <ul class="sessions__commands-list" aria-label="Session command list">
                <For each={filteredDefinitions()}>{(definition) => <li><button type="button" onClick={() => selectDefinition(definition)}><strong>{definition.displayName}</strong><span>{definition.description}</span><small>{requiredSummary(definition)}</small></button></li>}</For>
              </ul>
              <div class="sessions__commands-actions"><button type="button" onClick={closePalette}>Close</button></div>
            </div>
          }>
            {(definition) => <div class="sessions__commands-form-view">
              <h3>{definition().displayName}</h3><p>{definition().description}</p>
              <Show when={invocationId()} fallback={
                <form aria-label={`${definition().displayName} command`} onSubmit={(event) => { event.preventDefault(); submit(); }}>
                  <For each={definition().inputs}>{(input) => <Field id={fieldId(input.name)} label={input.name} required={input.required} error={localErrors()[input.name] ?? serverFieldErrors()[input.name]}>
                    {(field) => input.type === "string"
                      ? <textarea {...fieldAttributes(field)} value={String(drafts()[input.name] ?? "")} onInput={(event) => editDraft(input.name, event.currentTarget.value)} />
                      : <input {...fieldAttributes(field)} type="checkbox" checked={Boolean(drafts()[input.name])} onChange={(event) => editDraft(input.name, event.currentTarget.checked)} />}
                  </Field>}</For>
                  <Show when={operation()?.validationFailure?.message}><InlineAlert tone="danger">{operation()!.validationFailure!.message}</InlineAlert></Show>
                  <For each={generalViolations()}>{(violation) => <InlineAlert tone="danger">{violation.message}</InlineAlert>}</For>
                  <Show when={(operation()?.status === "failed" && !operation()?.validationFailure) || dispatchFailed()}><InlineAlert tone="danger">{operation()?.error || "Unable to start session command."}</InlineAlert></Show>
                  <div class="sessions__commands-actions"><button type="button" disabled={pending()} onClick={handleDialogClose}>Back</button><button type="submit" disabled={pending()}>{pending() ? "Starting command..." : definition().displayName}</button></div>
                </form>
              }>
                <div class="sessions__commands-result">
                  <p role="status">Command admitted.</p>
                  <div class="sessions__commands-actions">
                    <button type="button" onClick={() => navigate(`/routines/${encodeURIComponent(invocationId()!)}`)}>View invocation</button>
                    <button type="button" onClick={closePalette}>Close</button>
                  </div>
                </div>
              </Show>
            </div>}
          </Show>
        </Dialog>
      </Show>
    </>
  );
}

function fieldId(name: string | undefined) { return name === undefined ? "" : `session-command-input-${encodeURIComponent(name).replaceAll("%", "-")}`; }
function fieldAttributes(field: { id: string; describedBy: string | undefined; invalid: boolean }) { return { id: field.id, "aria-describedby": field.describedBy, "aria-invalid": field.invalid || undefined }; }
function requiredSummary(definition: ClientManualRoutineDefinition) { const required = definition.inputs.filter((input) => input.required).map((input) => input.name); return required.length > 0 ? `Required: ${required.join(", ")}` : "No required inputs"; }

function validateInputs(inputs: ClientRoutineInputDefinition[], drafts: Drafts) {
  const errors: FieldErrors = {};
  for (const input of inputs) {
    if (input.type !== "string") continue;
    const value = String(drafts[input.name] ?? "");
    if (input.required && value.trim() === "") errors[input.name] = `${input.name} is required.`;
    else if ((!input.required && value === "") || input.minimumStringLength === null) continue;
    else if (BigInt(Array.from(value).length) < input.minimumStringLength) errors[input.name] = `${input.name} must be at least ${input.minimumStringLength.toString()} characters.`;
  }
  return errors;
}

function routineValue(input: ClientRoutineInputDefinition, draft: string | boolean | undefined): ClientRoutineValue {
  return input.type === "boolean" ? { type: "boolean", value: Boolean(draft) } : { type: "string", value: String(draft ?? "") };
}
function omit(errors: FieldErrors, name: string) { const next = { ...errors }; delete next[name]; return next; }
function isEditable(target: EventTarget | null) { return target instanceof HTMLElement && (target.isContentEditable || ["INPUT", "TEXTAREA", "SELECT"].includes(target.tagName)); }
