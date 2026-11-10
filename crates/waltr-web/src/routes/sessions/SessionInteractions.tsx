import { For, Match, Show, Switch, createEffect, createMemo, createSignal, createUniqueId, onCleanup, untrack } from "solid-js";
import { useAppState } from "../../app/context";
import { InlineAlert } from "../../components/ui";
import type { InteractionFormField, InteractionFormValue, InteractionRequestView, InteractionResponse, PendingInteractionView } from "../../generated/waltr-client";
import { formValues, initialFormDraft, questionAnswers, type FormDraft } from "./interaction-drafts";

function presentationKey(pending: PendingInteractionView) {
  return JSON.stringify([pending.identity, pending.request], (_key, value) => typeof value === "bigint" ? value.toString() : value);
}

export function SessionInteractions(props: { sessionId: string }) {
  const app = useAppState();
  const [watchError, setWatchError] = createSignal<string>();
  const observedSessionId = createMemo(() => props.sessionId);
  createEffect(() => {
    const sessionId = observedSessionId();
    let active = true;
    setWatchError(undefined);
    void untrack(() => app.dispatch({ type: "retainInteractions", sessionId })).catch(() => {
      if (active) setWatchError("Unable to observe session interactions.");
    });
    onCleanup(() => {
      active = false;
      void app.dispatch({ type: "releaseInteractions", sessionId }).catch(() => {});
    });
  });
  const view = () => app.snapshot()?.interactions[props.sessionId];
  const keys = createMemo(() => view()?.pending.map(presentationKey) ?? []);
  return <section class="sessions__interactions" aria-label="Session interactions">
    <Show when={watchError() || view()?.error}>{error => <InlineAlert tone="danger">{error()}</InlineAlert>}</Show>
    <Show when={view() && !view()?.connected}><p role="status">Reconnecting interactions. Responses are disabled.</p></Show>
    <For each={keys()}>{key => {
      const pending = () => view()?.pending.find(p => presentationKey(p) === key);
      return <Show when={pending()}>{current => <InteractionCard pending={current()} connected={view()?.connected ?? false} />}</Show>;
    }}</For>
  </section>;
}

function InteractionCard(props: { pending: PendingInteractionView; connected: boolean }) {
  const app = useAppState();
  const request = props.pending.request;
  const identity = props.pending.identity;
  const [error, setError] = createSignal<string>();
  const [dispatching, setDispatching] = createSignal(false);
  let active = true;
  onCleanup(() => { active = false; });
  const disabled = () => !props.connected || props.pending.responding || dispatching();
  const respond = async (response: InteractionResponse) => {
    if (disabled()) return;
    setError(undefined);
    setDispatching(true);
    try {
      await app.dispatch({ type: "respondInteraction", identity, response });
    } catch {
      if (active) setError("Unable to send response. Check the current request before trying again.");
    } finally {
      if (active) setDispatching(false);
    }
  };
  return <section class={`sessions__prompt sessions__prompt--${request.kind}`} aria-label={`${request.kind} interaction`} aria-busy={props.pending.responding || dispatching()}>
    <Switch>
      <Match when={request.kind === "permission"}>{_matched => <Permission request={request as Extract<InteractionRequestView, { kind: "permission" }>} disabled={disabled()} respond={respond} />}</Match>
      <Match when={request.kind === "question"}>{_matched => <Questions request={request as Extract<InteractionRequestView, { kind: "question" }>} disabled={disabled()} respond={respond} />}</Match>
      <Match when={request.kind === "form"}>{_matched => <InteractionForm request={request as Extract<InteractionRequestView, { kind: "form" }>} disabled={disabled()} respond={respond} />}</Match>
      <Match when={request.kind === "urlConsent"}>{_matched => {
        const urlRequest = request as Extract<InteractionRequestView, { kind: "urlConsent" }>;
        return <>
          <h2>URL consent</h2><p>{urlRequest.message}</p><pre class="sessions__interaction-url">{urlRequest.url}</pre>
          <p>Consent does not open this URL or confirm that any external step is complete.</p>
          <Actions disabled={disabled()} accept={() => void respond({ kind: "urlConsent", action: "accept" })} decline={() => void respond({ kind: "urlConsent", action: "decline" })} cancel={() => void respond({ kind: "urlConsent", action: "cancel" })} />
        </>;
      }}</Match>
      <Match when={request.kind === "unsupported"}>{_matched => {
        const unsupported = request as Extract<InteractionRequestView, { kind: "unsupported" }>;
        return <><h2>Unsupported interaction</h2><p>{unsupported.message}</p>
          <p>This request cannot be accepted by this client.</p>
          <Show when={unsupported.form}>
            <Actions disabled={disabled()} decline={() => void respond({ kind: "form", action: "decline", values: {} })} cancel={() => void respond({ kind: "form", action: "cancel", values: {} })} />
          </Show>
        </>;
      }}</Match>
    </Switch>
    <Show when={props.pending.responding || dispatching()}><p role="status">Sending response; waiting for server confirmation…</p></Show>
    <Show when={error() || props.pending.error}>{message => <InlineAlert tone="danger">{message()}</InlineAlert>}</Show>
  </section>;
}

type ResponseProps = { disabled: boolean; respond: (response: InteractionResponse) => Promise<void> };

function Permission(props: ResponseProps & { request: Extract<InteractionRequestView, { kind: "permission" }> }) {
  const [selected, setSelected] = createSignal<number[]>([]);
  const [message, setMessage] = createSignal("");
  const respond = (decision: Extract<InteractionResponse, { kind: "permission" }>["decision"]) => void props.respond({
    kind: "permission", decision, suggestionIndices: decision === "allowAlways" ? selected() : [], message: message().trim() || null,
  });
  return <><h2>Permission required</h2><p>{props.request.message}</p>
    <p>Identity: <code>{props.request.identity}</code></p>
    <h3>Requested claims</h3><For each={props.request.claims}>{claim => <pre>{claim}</pre>}</For>
    <fieldset disabled={props.disabled} class="sessions__question"><legend>Always-allow scopes (select explicitly)</legend>
      <For each={props.request.suggestions}>{(suggestion, index) => <label class="sessions__question-option">
        <input type="checkbox" checked={selected().includes(index())} onChange={event => setSelected(current => event.currentTarget.checked ? [...current, index()] : current.filter(i => i !== index()))} />
        <pre>{suggestion}</pre>
      </label>}</For>
      <Show when={props.request.suggestions.length === 0}><p>No persistent approval scopes are offered.</p></Show>
    </fieldset>
    <label class="sessions__prompt-field sessions__prompt-field--stacked">Response message<textarea disabled={props.disabled} value={message()} onInput={event => setMessage(event.currentTarget.value)} /></label>
    <div class="sessions__prompt-actions">
      <button type="button" disabled={props.disabled} onClick={() => respond("allowOnce")}>Allow once</button>
      <button type="button" disabled={props.disabled || selected().length === 0} onClick={() => respond("allowAlways")}>Allow selected scopes always</button>
      <button type="button" disabled={props.disabled} onClick={() => respond("deny")}>Deny</button>
      <button type="button" disabled={props.disabled} onClick={() => respond("cancelWork")}>Cancel work</button>
    </div>
  </>;
}

function Questions(props: ResponseProps & { request: Extract<InteractionRequestView, { kind: "question" }> }) {
  const group = createUniqueId();
  const [selections, setSelections] = createSignal<string[][]>(props.request.questions.map(() => []));
  const [custom, setCustom] = createSignal<string[]>(props.request.questions.map(() => ""));
  const [error, setError] = createSignal<string>();
  const submit = () => {
    const result = questionAnswers(props.request.questions, selections(), custom());
    if (typeof result === "string") { setError(result); return; }
    setError(undefined);
    void props.respond({ kind: "question", answers: result });
  };
  return <><h2>Answer required</h2><For each={props.request.questions}>{(question, index) => <fieldset class="sessions__question" disabled={props.disabled}>
    <legend>{question.header}</legend><p>{question.question}</p>
    <For each={question.options}>{option => <label class="sessions__question-option">
      <input type={question.multiple ? "checkbox" : "radio"} name={`${group}-${index()}`} checked={selections()[index()]?.includes(option.label)} onChange={event => {
        const checked = event.currentTarget.checked;
        setSelections(current => current.map((values, i) => i !== index() ? values : question.multiple ? checked ? [...values, option.label] : values.filter(v => v !== option.label) : [option.label]));
        if (!question.multiple) setCustom(current => current.map((value, i) => i === index() ? "" : value));
      }} />
      <span>{option.label}</span><small>{option.description}</small>
    </label>}</For>
    <Show when={question.custom}><label class="sessions__prompt-field sessions__prompt-field--stacked">Custom answer
      <textarea value={custom()[index()]} onInput={event => {
        const value = event.currentTarget.value;
        setCustom(current => current.map((answer, i) => i === index() ? value : answer));
        if (!question.multiple && value.trim()) setSelections(current => current.map((answer, i) => i === index() ? [] : answer));
      }} />
    </label></Show>
  </fieldset>}</For>
    <Show when={error()}>{message => <InlineAlert tone="danger">{message()}</InlineAlert>}</Show>
    <button type="button" disabled={props.disabled} onClick={submit}>Submit answers</button>
  </>;
}

function InteractionForm(props: ResponseProps & { request: Extract<InteractionRequestView, { kind: "form" }> }) {
  const [draft, setDraft] = createSignal<FormDraft>(initialFormDraft(props.request.fields));
  const [error, setError] = createSignal<string>();
  const submit = () => {
    const result = formValues(props.request.fields, draft());
    if (typeof result === "string") { setError(result); return; }
    setError(undefined);
    void props.respond({ kind: "form", action: "accept", values: result });
  };
  return <><h2>Information requested</h2><p>{props.request.message}</p>
    <fieldset disabled={props.disabled} class="sessions__interaction-fields"><legend>Requested fields</legend>
      <For each={props.request.fields}>{field => <FormField field={field} value={draft()[field.name]} change={value => setDraft(current => ({ ...current, [field.name]: value }))} />}</For>
    </fieldset>
    <Show when={error()}>{message => <InlineAlert tone="danger">{message()}</InlineAlert>}</Show>
    <Actions disabled={props.disabled} accept={submit} decline={() => void props.respond({ kind: "form", action: "decline", values: {} })} cancel={() => void props.respond({ kind: "form", action: "cancel", values: {} })} />
  </>;
}

function FormField(props: { field: InteractionFormField; value: InteractionFormValue | undefined; change: (value: InteractionFormValue | undefined) => void }) {
  const id = createUniqueId();
  const schema = props.field.schema;
  const text = () => props.value === undefined ? "" : String(props.value);
  const title = () => props.field.title ?? props.field.name;
  const choiceTitle = (choice: string) => Object.hasOwn(props.field.choiceTitles, choice) ? props.field.choiceTitles[choice]! : choice;
  return <div class="sessions__prompt-field sessions__prompt-field--stacked">
    <label for={id}>{title()}{props.field.required ? " (required)" : " (optional)"}</label>
    <Show when={props.field.description}><p id={`${id}-description`}>{props.field.description}</p></Show>
    <Switch>
      <Match when={schema.kind === "boolean"}>
        <select id={id} aria-describedby={`${id}-description`} value={text()} onChange={event => props.change(event.currentTarget.value === "" ? undefined : event.currentTarget.value === "true")}>
          <option value="">Not provided</option><option value="true">Yes</option><option value="false">No</option>
        </select>
      </Match>
      <Match when={schema.kind === "stringArray"}>{_matched => {
        const array = schema as Extract<typeof schema, { kind: "stringArray" }>;
        return <fieldset id={id}><legend>{title()} choices</legend>
          <p>Select {String(array.minItems ?? 0)} to {String(array.maxItems ?? "any number of")} choices.</p>
          <For each={array.choices}>{choice => <label class="sessions__question-option">
            <input type="checkbox" checked={Array.isArray(props.value) && props.value.includes(choice)} onChange={event => {
              const values = Array.isArray(props.value) ? props.value : [];
              props.change(event.currentTarget.checked ? [...values, choice] : values.filter(v => v !== choice));
            }} />{choiceTitle(choice)}
          </label>}</For>
          <Show when={!props.field.required}><button type="button" onClick={() => props.change(undefined)}>Omit {title()}</button></Show>
        </fieldset>;
      }}</Match>
      <Match when={schema.kind === "number"}>{_matched => {
        const number = schema as Extract<typeof schema, { kind: "number" }>;
        return <input id={id} aria-describedby={`${id}-description`} type="number" step={number.integer ? 1 : "any"} min={number.minimum ?? undefined} max={number.maximum ?? undefined} value={text()} onInput={event => props.change(event.currentTarget.value === "" ? undefined : event.currentTarget.value)} />;
      }}</Match>
      <Match when={schema.kind === "string"}>{_matched => {
        const string = schema as Extract<typeof schema, { kind: "string" }>;
        return <Show when={string.choices} fallback={<>
          <input id={id} aria-describedby={`${id}-description`} type={string.format === "email" ? "email" : string.format === "uri" ? "url" : string.format === "date" ? "date" : "text"} placeholder={string.format === "date-time" ? "RFC 3339 date and time" : undefined} minLength={string.minLength == null ? undefined : Number(string.minLength)} maxLength={string.maxLength == null ? undefined : Number(string.maxLength)} value={text()} onInput={event => props.change(event.currentTarget.value)} />
          <Show when={!props.field.required}><button type="button" onClick={() => props.change(undefined)}>Omit {title()}</button></Show>
        </>}>
          {choices => <select id={id} value={props.value === undefined ? "" : `value:${text()}`} onChange={event => props.change(event.currentTarget.value === "" ? undefined : event.currentTarget.value.slice(6))}>
            <option value="">Not provided</option><For each={choices()}>{choice => <option value={`value:${choice}`}>{choiceTitle(choice)}</option>}</For>
          </select>}
        </Show>;
      }}</Match>
    </Switch>
  </div>;
}

function Actions(props: { disabled: boolean; accept?: () => void; decline: () => void; cancel: () => void }) {
  return <div class="sessions__prompt-actions">
    <Show when={props.accept}><button type="button" disabled={props.disabled} onClick={() => props.accept?.()}>Accept</button></Show>
    <button type="button" disabled={props.disabled} onClick={props.decline}>Decline</button>
    <button type="button" disabled={props.disabled} onClick={props.cancel}>Cancel request</button>
  </div>;
}
