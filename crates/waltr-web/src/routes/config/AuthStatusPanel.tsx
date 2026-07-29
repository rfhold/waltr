import { Show, createMemo, createSignal } from "solid-js";

import { useAppState } from "../../app/context";
import { ConfirmAction, InlineAlert } from "../../components/ui";
import type { AuthStatusSnapshot, ClientAuthTarget } from "../../generated/waltr-client";

const MAX_MESSAGE_LENGTH = 500;

export function AuthStatusPanel(props: { target: ClientAuthTarget }) {
  const app = useAppState();
  const [pendingAction, setPendingAction] = createSignal<"authorize" | "refresh" | "reset">();
  const [confirmingReset, setConfirmingReset] = createSignal(false);
  const [error, setError] = createSignal<string>();
  const status = createMemo(() => app.snapshot()?.auth.statuses[targetMapKey(props.target)]);
  const listError = createMemo(() => app.snapshot()?.auth.list.error);
  const oauthSupported = createMemo(() => status()?.supportedMethods.some((method) => method.startsWith("oauth")) ?? false);
  const canAuthorize = createMemo(() => oauthSupported() && status()?.kind !== "apiKeyConfigured");
  const reauthorization = createMemo(() => ["oauthConfigured", "expired", "authorizationRequired"].includes(status()?.kind ?? ""));

  const requestStatuses = () => app.dispatch({ type: "requestAuthStatuses" }).catch(() => undefined);
  const runAction = async (
    action: "authorize" | "refresh" | "reset",
    operation: () => Promise<unknown>,
  ) => {
    setPendingAction(action);
    setError(undefined);
    try {
      await operation();
      if (action === "reset") setConfirmingReset(false);
      await requestStatuses();
    } catch (cause) {
      setError(safeError(cause));
    } finally {
      setPendingAction(undefined);
    }
  };

  return (
    <section class="auth-status" aria-label={`Authentication for ${props.target.key}`}>
      <div class="auth-status__heading">
        <div>
          <h3>Authentication</h3>
          <p>Credentials stay on the server and are never shown here.</p>
        </div>
        <span class={`auth-status__badge auth-status__badge--${statusTone(status()?.kind)}`}>
          {statusLabel(status())}
        </span>
      </div>

      <Show when={status()?.message}>
        {(message) => <InlineAlert tone={status()?.kind === "expired" || status()?.kind === "authorizationRequired" ? "warning" : undefined}>{safeText(message())}</InlineAlert>}
      </Show>

      <Show when={listError()}>
        {(message) => (
          <InlineAlert tone="danger">
            {safeText(message())} Authentication statuses may be stale.
          </InlineAlert>
        )}
      </Show>

      <Show when={(status()?.expiresAtMs ?? 0n) > 0n}>
        <dl class="auth-status__metadata">
          <div><dt>Expires</dt><dd>{formatExpiration(status()!.expiresAtMs)}</dd></div>
          <Show when={props.target.kind === "mcpServer" && status()?.registrationMethod}>
            <div><dt>Registration</dt><dd>{registrationMethodLabel(status()!.registrationMethod)}</dd></div>
          </Show>
        </dl>
      </Show>
      <Show when={props.target.kind === "mcpServer" && status()?.registrationMethod && (status()?.expiresAtMs ?? 0n) === 0n}>
        <dl class="auth-status__metadata"><div><dt>Registration</dt><dd>{registrationMethodLabel(status()!.registrationMethod)}</dd></div></dl>
      </Show>

      <Show when={error()}>{(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}</Show>

      <div class="auth-status__actions">
        <Show when={listError()}>
          <button
            type="button"
            disabled={app.snapshot()?.auth.list.status === "loading" || Boolean(pendingAction())}
            onClick={() => void requestStatuses()}
          >
            Retry status
          </button>
        </Show>
        <Show when={canAuthorize()}>
          <button
            type="button"
            disabled={Boolean(pendingAction())}
            onClick={() => void runAction("authorize", () => app.dispatchBeginExplicitAuth(props.target))}
          >
            {reauthorization() ? "Reauthorize" : "Authorize"}
          </button>
        </Show>
        <Show when={status()?.canRefresh}>
          <button
            type="button"
            disabled={Boolean(pendingAction())}
            onClick={() => void runAction("refresh", () => app.dispatchRefreshAuth(props.target))}
          >
            Force refresh
          </button>
        </Show>
        <Show when={["oauthConfigured", "expired", "authorizationRequired"].includes(status()?.kind ?? "")}>
          <Show
            when={confirmingReset()}
            fallback={<button type="button" disabled={Boolean(pendingAction())} onClick={() => setConfirmingReset(true)}>Reset</button>}
          >
            <ConfirmAction
              label={`Reset authentication for ${props.target.key} confirmation`}
              prompt="Remove credential?"
              confirmLabel="Reset"
              confirmAriaLabel={`Confirm reset authentication for ${props.target.key}`}
              cancelAriaLabel={`Cancel reset authentication for ${props.target.key}`}
              pending={pendingAction() === "reset"}
              onConfirm={() => void runAction("reset", () => app.dispatchLogoutAuth(props.target))}
              onCancel={() => setConfirmingReset(false)}
            />
          </Show>
        </Show>
      </div>
    </section>
  );
}

function targetMapKey(target: ClientAuthTarget) {
  return `${target.kind === "mcpServer" ? "mcp" : "provider"}:${target.key}`;
}

function statusLabel(status: AuthStatusSnapshot | undefined) {
  switch (status?.kind) {
    case "oauthConfigured": return "Connected";
    case "apiKeyConfigured": return "API key configured";
    case "expired": return "Expired";
    case "authorizationRequired": return "Authorization required";
    case "unsupported": return "Unsupported";
    case "notConfigured": return "Not connected";
    default: return "Status unavailable";
  }
}

function statusTone(kind: string | undefined) {
  if (kind === "oauthConfigured" || kind === "apiKeyConfigured") return "success";
  if (kind === "expired" || kind === "authorizationRequired") return "warning";
  return "neutral";
}

function formatExpiration(value: bigint) {
  const milliseconds = Number(value);
  if (!Number.isFinite(milliseconds)) return "Unknown";
  return new Intl.DateTimeFormat(undefined, { dateStyle: "medium", timeStyle: "short" }).format(new Date(milliseconds));
}

function registrationMethodLabel(method: string) {
  switch (method) {
    case "dynamic": return "Dynamic";
    case "clientMetadata": return "Client metadata";
    case "preregistered": return "Preregistered";
    default: return "Unknown";
  }
}

function safeText(value: string) {
  const sanitized = value.replace(/[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f-\u009f]/g, " ").trim();
  return sanitized.length > MAX_MESSAGE_LENGTH ? `${sanitized.slice(0, MAX_MESSAGE_LENGTH - 3)}...` : sanitized;
}

function safeError(error: unknown) {
  return safeText(error instanceof Error ? error.message : "Authentication operation failed.");
}
