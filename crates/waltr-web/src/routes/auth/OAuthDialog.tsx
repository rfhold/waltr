import { createMemo, createSignal, Show } from "solid-js";

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

export function OAuthDialog() {
  const app = useAppState();
  const [input, setInput] = createSignal("");
  const [localError, setLocalError] = createSignal<string | undefined>();
  const requirement = createMemo(() => app.snapshot()?.auth?.pending[0]);
  const attempt = createMemo(() => attemptFor(requirement(), app.snapshot()?.auth?.activeAttempt));
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

    if (!current) {
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

    if (currentAttempt) {
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
            <InlineAlert tone="warning">{current().message}</InlineAlert>

            <Show when={attempt()} fallback={<button class="ui-button" disabled={busy()} onClick={begin}>Begin OAuth</button>}>
              {(currentAttempt) => (
                <div class="oauth-dialog__attempt">
                  <Show when={currentAttempt().messages.length > 0}>
                    <ul class="oauth-dialog__messages">
                      {currentAttempt().messages.map((message) => <li>{message}</li>)}
                    </ul>
                  </Show>
                  <a class="ui-button oauth-dialog__open" href={currentAttempt().authUrl} target="_blank" rel="noreferrer">
                    Open authorization URL
                  </a>
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
                </div>
              )}
            </Show>

            <Show when={localError() ?? operationError()}>
              {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
            </Show>

            <div class="oauth-dialog__actions">
              <button class="ui-button" disabled={busy()} onClick={close}>Cancel</button>
            </div>
          </div>
        </Dialog>
      )}
    </Show>
  );
}
