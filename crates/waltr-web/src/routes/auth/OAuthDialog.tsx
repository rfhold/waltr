import { createEffect, createMemo, createSignal, For, Show } from "solid-js";

import { useAppState } from "../../app/context";
import { Dialog, Field, InlineAlert } from "../../components/ui";
import type { AuthAttemptSnapshot, ClientAuthTarget } from "../../generated/waltr-client";

function targetLabel(target: ClientAuthTarget) {
  return target.kind === "mcpServer" ? `MCP server ${target.key}` : `provider ${target.key}`;
}

function sameTarget(left: ClientAuthTarget | undefined, right: ClientAuthTarget | undefined) {
  return Boolean(left && right && left.kind === right.kind && left.key === right.key);
}

const TERMINAL_LIFECYCLES = new Set(["succeeded", "failed", "expired", "canceled"]);
const MAX_MESSAGE_LENGTH = 500;
const MAX_AUTH_URL_LENGTH = 4096;

function displayText(value: string) {
  const sanitized = value.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, " ").trim();
  return sanitized.length > MAX_MESSAGE_LENGTH
    ? `${sanitized.slice(0, MAX_MESSAGE_LENGTH - 1)}…`
    : sanitized;
}

function safeAuthorizationUrl(value: string) {
  if (value.length > MAX_AUTH_URL_LENGTH) return undefined;

  try {
    const url = new URL(value);
    return ["http:", "https:"].includes(url.protocol) && !url.username && !url.password
      ? url.href
      : undefined;
  } catch {
    return undefined;
  }
}

function completionModeLabel(mode: string) {
  return mode === "serverCallback" ? "Server callback" : mode === "manual" ? "Manual" : "Unknown";
}

function registrationMethodLabel(method: string) {
  switch (method) {
    case "dynamic": return "Dynamic";
    case "clientMetadata": return "Client metadata";
    case "preregistered": return "Preregistered";
    default: return "Unknown";
  }
}

function lifecycleLabel(lifecycle: string) {
  return lifecycle ? `${lifecycle[0]!.toUpperCase()}${lifecycle.slice(1)}` : "Unknown";
}

function lifecycleMessage(attempt: AuthAttemptSnapshot) {
  switch (attempt.lifecycle) {
    case "pending":
      return attempt.completionMode === "serverCallback"
        ? "Waiting for the server callback. This dialog updates automatically."
        : "Waiting for the final redirect URL or authorization code.";
    case "completing": return "Completing authorization.";
    case "succeeded": return "Authorization succeeded.";
    case "failed": return "Authorization failed.";
    case "expired": return "Authorization expired.";
    case "canceled": return "Authorization canceled.";
    default: return "Authorization status is unknown.";
  }
}

export function OAuthDialog() {
  const app = useAppState();
  const [input, setInput] = createSignal("");
  const [localError, setLocalError] = createSignal<string | undefined>();
  const activeAttempt = createMemo(() => app.snapshot()?.auth?.activeAttempt);
  const attempt = createMemo(() => (
    sameTarget(app.explicitAuthTarget(), activeAttempt()?.target)
      ? activeAttempt() ?? undefined
      : undefined
  ));
  const target = createMemo(() => attempt()?.target);
  const attemptId = createMemo(() => attempt()?.attemptId);
  const terminal = createMemo(() => TERMINAL_LIFECYCLES.has(attempt()?.lifecycle ?? ""));
  const cancellable = createMemo(() => ["pending", "completing"].includes(attempt()?.lifecycle ?? ""));
  const manualInput = createMemo(() => attempt()?.completionMode === "manual" && attempt()?.lifecycle === "pending");
  const authorizationUrl = createMemo(() => safeAuthorizationUrl(attempt()?.authUrl ?? ""));
  const opaqueAuthorization = createMemo(() => (
    Boolean(attemptId() && app.openAuthorizationAttempt()) && attempt()?.lifecycle === "pending" && !authorizationUrl()
  ));
  const busy = createMemo(() => {
    const auth = app.snapshot()?.auth;

    return (
      auth?.begin.status === "loading" ||
      auth?.complete.status === "loading" ||
      auth?.cancel.status === "loading"
    );
  });
  const operationError = createMemo(
    () => app.snapshot()?.auth?.begin.error ?? app.snapshot()?.auth?.complete.error ?? app.snapshot()?.auth?.cancel.error,
  );
  const manualFieldError = createMemo(() => localError() === "Paste the final redirect URL or authorization code." ? localError() : undefined);
  createEffect(() => {
    attemptId();
    setInput("");
    setLocalError(undefined);
  });

  const complete = async () => {
    const current = attempt();
    const value = input().trim();

    if (!current || current.completionMode !== "manual" || current.lifecycle !== "pending") {
      return;
    }

    if (!value) {
      setLocalError("Paste the final redirect URL or authorization code.");
      return;
    }

    setLocalError(undefined);
    try {
      await app.dispatchCompleteAuth(current.attemptId, value);
      setInput("");
      await app.dispatch({ type: "requestAuthStatuses" }).catch(() => undefined);
    } catch (error) {
      setLocalError(displayText(error instanceof Error ? error.message : "Authorization could not be completed."));
    }
  };

  const close = async () => {
    const currentAttempt = attempt();

    setLocalError(undefined);

    if (currentAttempt && ["pending", "completing"].includes(currentAttempt.lifecycle)) {
      try {
        await app.dispatchCancelAuth(currentAttempt.attemptId);
        await app.dispatch({ type: "requestAuthStatuses" }).catch(() => undefined);
      } catch (error) {
        setLocalError(displayText(error instanceof Error ? error.message : "Authorization could not be canceled."));
      }
      return;
    }

    if (currentAttempt) {
      app.clearExplicitAuthTarget();
    }
  };
  const openAuthorization = (url: string, event: MouseEvent) => {
    const openExternalUrl = app.openExternalUrl();
    if (!openExternalUrl) return;
    event.preventDefault();
    setLocalError(undefined);
    void openExternalUrl(url).catch((error) => {
      setLocalError(displayText(error instanceof Error ? error.message : "Authorization URL could not be opened."));
    });
  };
  const openOpaqueAuthorization = async () => {
    const currentAttemptId = attemptId();
    const openAttempt = app.openAuthorizationAttempt();
    setLocalError(undefined);
    if (!currentAttemptId || !openAttempt) {
      setLocalError("Authorization opener unavailable.");
      return;
    }
    try {
      await openAttempt(currentAttemptId);
    } catch {
      setLocalError("Authorization could not be opened. Try again.");
    }
  };

  return (
    <Show when={target()}>
      {(currentTarget) => (
        <Dialog title="OAuth authorization" label="OAuth authorization" class="panel oauth-dialog" onClose={close}>
          <div class="config__form oauth-dialog__body">
            <p class="oauth-dialog__intro">
              Authenticate {targetLabel(currentTarget())}.
            </p>

            <Show when={attempt()}>
              {(currentAttempt) => (
                <div class="oauth-dialog__attempt">
                  <dl class="oauth-dialog__metadata">
                    <div><dt>Status</dt><dd>{lifecycleLabel(currentAttempt().lifecycle)}</dd></div>
                    <div><dt>Completion</dt><dd>{completionModeLabel(currentAttempt().completionMode)}</dd></div>
                    <Show when={currentAttempt().target.kind === "mcpServer"}>
                      <div><dt>Registration</dt><dd>{registrationMethodLabel(currentAttempt().registrationMethod)}</dd></div>
                    </Show>
                  </dl>
                  <InlineAlert tone={currentAttempt().lifecycle === "failed" ? "danger" : "info"}>
                    {lifecycleMessage(currentAttempt())}
                  </InlineAlert>
                  <Show when={currentAttempt().messages.length > 0}>
                    <ul class="oauth-dialog__messages">
                      <For each={currentAttempt().messages.slice(0, 8)}>{(message) => <li>{displayText(message)}</li>}</For>
                    </ul>
                  </Show>
                  <Show
                    when={authorizationUrl()}
                    fallback={opaqueAuthorization()
                      ? null
                      : <InlineAlert tone="danger">Authorization URL unavailable.</InlineAlert>}
                  >
                    {(url) => (
                      <code class="oauth-dialog__url">{url()}</code>
                    )}
                  </Show>
                  <Show when={manualInput()}>
                    <Field id="oauth-manual-input" label="Final redirect URL or code" error={manualFieldError()}>
                      {(field) => (
                        <textarea
                          id={field.id}
                          aria-describedby={field.describedBy}
                          aria-invalid={field.invalid}
                          rows={4}
                          value={input()}
                          onInput={(event) => setInput(event.currentTarget.value)}
                          placeholder="Paste the final redirect URL or authorization code"
                        />
                      )}
                    </Field>
                  </Show>
                  <Show when={currentAttempt().failureReason}>
                    {(reason) => <InlineAlert tone="danger">{displayText(reason())}</InlineAlert>}
                  </Show>
                </div>
              )}
            </Show>

            <Show when={(manualFieldError() ? undefined : localError()) ?? operationError()}>
              {(message) => <InlineAlert tone="danger">{displayText(message())}</InlineAlert>}
            </Show>

            <div class="config__form-actions oauth-dialog__actions">
              <Show when={!terminal() ? authorizationUrl() : undefined} fallback={
                <Show when={opaqueAuthorization()}>
                  <button type="button" class="ui-button oauth-dialog__open" disabled={busy()} onClick={openOpaqueAuthorization}>Open authorization</button>
                </Show>
              }>
                {(url) => <a class="ui-button oauth-dialog__open" href={url()} target="_blank" rel="noopener noreferrer" onClick={(event) => openAuthorization(url(), event)}>Open authorization URL</a>}
              </Show>
              <Show when={manualInput()}>
                <button type="button" class="ui-button oauth-dialog__primary" disabled={busy()} onClick={complete}>Complete OAuth</button>
              </Show>
              <button type="button" class="ui-button" disabled={busy()} onClick={close}>
                {terminal() || !cancellable() ? "Dismiss" : "Cancel"}
              </button>
            </div>
          </div>
        </Dialog>
      )}
    </Show>
  );
}
