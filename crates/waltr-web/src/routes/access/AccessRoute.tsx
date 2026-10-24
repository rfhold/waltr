import { createSignal, Show } from "solid-js";

import { useAppState } from "../../app/context";
import { ServerControls } from "../../app/ServerControls";
import { InlineAlert } from "../../components/ui";

export type BrowserLocation = Pick<Location, "pathname" | "search" | "hash">;

const MAX_RETURN_PATH_DECODE_DEPTH = 4;
const ENCODED_PATH_SEPARATOR = /%(?:2f|5c)/i;
const CONTROL_CHARACTER = /\p{Cc}/u;
const URI_SCHEME = /^[a-z][a-z0-9+.-]*:/i;

export function safeLocalReturnPath(location: BrowserLocation) {
  const returnPath = `${location.pathname}${location.search}${location.hash}`;
  let candidate = returnPath;

  for (let depth = 0; depth < MAX_RETURN_PATH_DECODE_DEPTH; depth += 1) {
    if (!validReturnPathLayer(candidate) || ENCODED_PATH_SEPARATOR.test(candidate)) {
      return "/";
    }
    if (!candidate.includes("%")) {
      return returnPath;
    }

    try {
      candidate = decodeURIComponent(candidate);
    } catch {
      return "/";
    }
  }

  return "/";
}

function validReturnPathLayer(candidate: string) {
  const withoutLeadingSlash = candidate.slice(1);

  return candidate.startsWith("/")
    && !candidate.startsWith("//")
    && !candidate.includes("\\")
    && !Array.from(candidate).some((character) => CONTROL_CHARACTER.test(character))
    && !URI_SCHEME.test(candidate)
    && !URI_SCHEME.test(withoutLeadingSlash)
    && !withoutLeadingSlash.startsWith("//");
}

export function waltrLoginUrl(location: BrowserLocation) {
  const query = new URLSearchParams({ return_to: safeLocalReturnPath(location) });
  return `/oidc/login?${query}`;
}

export function NativeAccessDiagnostic() {
  const app = useAppState();
  return (
    <>
      <Show when={app.accessDiagnostic?.()?.persistenceWarning}>
        <InlineAlert tone="info">Secure sign-in storage is unavailable. Access may work for this session, but you may need to sign in again after restarting Waltr.</InlineAlert>
      </Show>
      <Show when={app.accessDiagnostic?.()?.failed}>
        <InlineAlert tone="danger">Native sign-in or saved-session restoration failed. Try signing in again.</InlineAlert>
      </Show>
    </>
  );
}

export function AccessRoute(props: { navigate?: (url: string) => void | Promise<void> }) {
  const app = useAppState();
  const [localError, setLocalError] = createSignal<string>();
  const access = () => app.snapshot()?.access;
  const loginUrl = () => waltrLoginUrl(window.location);
  const login = async (event: MouseEvent) => {
    event.preventDefault();
    setLocalError(undefined);

    try {
      const accessOperations = app.accessOperations();
      if (!accessOperations) throw new Error("Sign-in is unavailable in this client.");

      const started = await app.dispatch({ type: "startWaltrLogin" });
      const attemptId = started.access.activeAttemptId;
      if (!attemptId) {
        throw new Error("Login did not return an attempt ID.");
      }

      try {
        await (props.navigate ?? accessOperations.openLogin)(loginUrl());
      } catch {
        if (app.snapshot()?.access.activeAttemptId !== attemptId) {
          return;
        }
        await app.dispatch({ type: "failWaltrLogin", attemptId, error: "Unable to open the sign-in page." });
      }
    } catch (error) {
      setLocalError(error instanceof Error ? error.message : "Unable to start login.");
    }
  };
  const cancel = async () => {
    const attemptId = access()?.activeAttemptId;
    if (!attemptId) {
      return;
    }

    setLocalError(undefined);
    try {
      await app.dispatch({ type: "cancelWaltrLogin", attemptId });
    } catch (error) {
      setLocalError(error instanceof Error ? error.message : "Unable to cancel login.");
    }
  };

  return (
    <main class="access" aria-labelledby="access-title">
      <section class="access__panel">
        <p class="eyebrow">Waltr access</p>
        <h1 id="access-title">
          {access()?.status === "loginFailed" ? "Login did not complete" : "Sign in to continue"}
        </h1>
        <p class="access__copy">
          {access()?.status === "loginInProgress"
            ? "Redirecting to your organization's sign-in page..."
            : "Your Waltr session is missing or has expired. Sign in to restore access."}
        </p>

        <NativeAccessDiagnostic />
        <Show when={access()?.error ?? localError()}>
          {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
        </Show>

        <Show when={access()?.status === "loginInProgress"} fallback={
          <Show when={app.accessOperations()} fallback={
            <InlineAlert tone="info">Waltr access operations are unavailable in this client.</InlineAlert>
          }>
            <a class="ui-button access__login" href={loginUrl()} onClick={login}>
              {access()?.status === "loginFailed" || localError() ? "Try again" : "Sign in"}
            </a>
          </Show>
        }>
          <div class="access__progress-actions">
            <p class="access__progress" role="status" aria-live="polite">Login in progress</p>
            <button class="ui-button" type="button" disabled={!access()?.activeAttemptId} onClick={cancel}>Cancel login</button>
          </div>
        </Show>
        <ServerControls />
      </section>
    </main>
  );
}
