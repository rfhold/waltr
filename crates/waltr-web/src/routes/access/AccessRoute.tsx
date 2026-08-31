import { createSignal, Show } from "solid-js";

import { useAppState } from "../../app/context";
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

export function AccessRoute(props: { navigate?: (url: string) => void | Promise<void> }) {
  const app = useAppState();
  const [localError, setLocalError] = createSignal<string>();
  const access = () => app.snapshot()?.access;
  const accessProgressError = () => {
    const progress = app.accessProgress();
    return progress?.type === "unavailable" ? progress.message : undefined;
  };
  const loginUrl = () => waltrLoginUrl(window.location);
  const login = async (event: MouseEvent) => {
    event.preventDefault();
    setLocalError(undefined);

    try {
      const nativeAccess = app.nativeAccess();
      if (nativeAccess) {
        await nativeAccess.start();
        return;
      }

      const started = await app.dispatch({ type: "startWaltrLogin" });
      const attemptId = started.access.activeAttemptId;
      if (!attemptId) {
        throw new Error("Login did not return an attempt ID.");
      }

      try {
        await (props.navigate ?? ((url) => window.location.assign(url)))(loginUrl());
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
      const nativeAccess = app.nativeAccess();
      if (nativeAccess) await nativeAccess.cancel();
      else await app.dispatch({ type: "cancelWaltrLogin", attemptId });
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

        <Show when={access()?.error ?? localError()}>
          {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
        </Show>

        <Show when={accessProgressError()}>
          {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
        </Show>

        <Show when={access()?.status === "loginInProgress"} fallback={
          <a class="ui-button access__login" href={loginUrl()} onClick={login}>
            {access()?.status === "loginFailed" || localError() ? "Try again" : "Sign in"}
          </a>
        }>
          <div class="access__progress-actions">
            <p class="access__progress" role="status" aria-live="polite">Login in progress</p>
            <button class="ui-button" type="button" disabled={!access()?.activeAttemptId} onClick={cancel}>Cancel login</button>
          </div>
        </Show>
      </section>
    </main>
  );
}
