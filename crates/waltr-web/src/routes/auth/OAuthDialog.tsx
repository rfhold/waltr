import { createEffect, createMemo, createSignal, For, Show } from "solid-js";

import { useAppState } from "../../app/context";
import { Dialog, InlineAlert } from "../../components/ui";
import type { AuthAttemptSnapshot, AuthRequirementSnapshot, ClientAuthTarget } from "../../generated/waltr-client";

function targetLabel(target: ClientAuthTarget) {
  return target.kind === "mcpServer" ? `MCP server ${target.key}` : `provider ${target.key}`;
}

function sameTarget(left: ClientAuthTarget | undefined, right: ClientAuthTarget | undefined) {
  return Boolean(left && right && left.kind === right.kind && left.key === right.key);
}

function attemptFor(requirement: AuthRequirementSnapshot | undefined, attempt: AuthAttemptSnapshot | null | undefined) {
  if (!requirement || !attempt || !sameTarget(requirement.target, attempt.target)) {
    return undefined;
  }

  return attempt;
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
    case "static": return "Static";
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
  const requirement = createMemo(() => app.snapshot()?.auth?.pending[0]);
  const attempt = createMemo(() => attemptFor(requirement(), app.snapshot()?.auth?.activeAttempt));
  const attemptId = createMemo(() => attempt()?.attemptId);
  const terminal = createMemo(() => TERMINAL_LIFECYCLES.has(attempt()?.lifecycle ?? ""));
  const cancellable = createMemo(() => ["pending", "completing"].includes(attempt()?.lifecycle ?? ""));
  const manualInput = createMemo(() => attempt()?.completionMode === "manual" && attempt()?.lifecycle === "pending");
  const authorizationUrl = createMemo(() => safeAuthorizationUrl(attempt()?.authUrl ?? ""));
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
  createEffect(() => {
    attemptId();
    setInput("");
    setLocalError(undefined);
  });

  const begin = async () => {
    const current = requirement();

    if (!current) {
      return;
    }

    setLocalError(undefined);
    await app.dispatch({ type: "beginAuth", target: current.target });
  };

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
    await app.dispatch({ type: "completeAuth", attemptId: current.attemptId, input: value });
    setInput("");
  };

  const close = async () => {
    const currentRequirement = requirement();
    const currentAttempt = attempt();

    setLocalError(undefined);

    if (currentAttempt && ["pending", "completing"].includes(currentAttempt.lifecycle)) {
      await app.dispatch({ type: "cancelAuth", attemptId: currentAttempt.attemptId });
      return;
    }

    if (currentRequirement) {
      await app.dispatch({ type: "dismissAuthRequirement", target: currentRequirement.target });
    }
  };

  return (
    <Show when={requirement()}>
      {(current) => (
        <Dialog title="OAuth required" label="OAuth required" class="oauth-dialog" onClose={close}>
          <div class="oauth-dialog__body">
            <p class="oauth-dialog__intro">
              Authenticate {targetLabel(current().target)} to continue agent work.
            </p>
            <InlineAlert tone="warning">{displayText(current().message)}</InlineAlert>

            <Show when={attempt()} fallback={<button class="ui-button" disabled={busy()} onClick={begin}>Begin OAuth</button>}>
              {(currentAttempt) => (
                <div class="oauth-dialog__attempt">
                  <dl class="oauth-dialog__metadata">
                    <div><dt>Status</dt><dd>{lifecycleLabel(currentAttempt().lifecycle)}</dd></div>
                    <div><dt>Completion</dt><dd>{completionModeLabel(currentAttempt().completionMode)}</dd></div>
                    <div><dt>Registration</dt><dd>{registrationMethodLabel(currentAttempt().registrationMethod)}</dd></div>
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
                    fallback={<InlineAlert tone="danger">Authorization URL unavailable.</InlineAlert>}
                  >
                    {(url) => (
                      <>
                        <code class="oauth-dialog__url">{url()}</code>
                        <a class="ui-button oauth-dialog__open" href={url()} target="_blank" rel="noopener noreferrer">
                          Open authorization URL
                        </a>
                      </>
                    )}
                  </Show>
                  <Show when={manualInput()}>
                    <label class="oauth-dialog__field">
                      <span>Final redirect URL or code</span>
                      <textarea
                        rows={4}
                        value={input()}
                        onInput={(event) => setInput(event.currentTarget.value)}
                        placeholder="Paste the final redirect URL or authorization code"
                      />
                    </label>
                    <button class="ui-button" disabled={busy()} onClick={complete}>Complete OAuth</button>
                  </Show>
                  <Show when={currentAttempt().failureReason}>
                    {(reason) => <InlineAlert tone="danger">{displayText(reason())}</InlineAlert>}
                  </Show>
                </div>
              )}
            </Show>

            <Show when={localError() ?? operationError()}>
              {(message) => <InlineAlert tone="danger">{displayText(message())}</InlineAlert>}
            </Show>

            <div class="oauth-dialog__actions">
              <button class="ui-button" disabled={busy()} onClick={close}>
                {terminal() || !cancellable() ? "Dismiss" : "Cancel"}
              </button>
            </div>
          </div>
        </Dialog>
      )}
    </Show>
  );
}
