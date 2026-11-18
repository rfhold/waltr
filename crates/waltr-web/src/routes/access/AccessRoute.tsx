import { createEffect, createSignal, onCleanup, Show } from "solid-js";

import { useAppState } from "../../app/context";
import { ServerControls } from "../../app/ServerControls";
import { InlineAlert } from "../../components/ui";
import type { NativeLoginMethod } from "../../platform/client";

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

export function waltrSignInUrl(location: BrowserLocation) {
  const safe = safeLocalReturnPath(location);
  let path = safe.split(/[?#]/, 1)[0];
  while (path.includes("%")) path = decodeURIComponent(path);
  path = new URL(path.replaceAll("?", "%3F").replaceAll("#", "%23"), "https://waltr.invalid").pathname.toLowerCase();
  const query = new URLSearchParams({
    return_to: path === "/oidc" || path.startsWith("/oidc/") || new TextEncoder().encode(safe).length > 2048 ? "/" : safe,
  });
  return `/oidc/sign-in?${query}`;
}

export function NativeAccessDiagnostic() {
  const app = useAppState();
  return (
    <>
      <Show when={app.accessDiagnostic?.()?.persistenceWarning}>
        <InlineAlert tone="info">Secure sign-in storage is unavailable. Access may work for this session, but you may need to sign in again after restarting Waltr.</InlineAlert>
      </Show>
      <Show when={app.accessDiagnostic?.()?.failed && !app.accessDiagnostic?.()?.restoring && !app.accessDiagnostic?.()?.deviceUnsupported}>
        <InlineAlert tone="danger">Native sign-in or saved-session restoration failed. Try signing in again.</InlineAlert>
      </Show>
    </>
  );
}

export function AccessRoute(props: {
  navigate?: (url: string) => void | Promise<void>;
  claimAutomaticNativeLogin?: () => boolean;
}) {
  const app = useAppState();
  const [localError, setLocalError] = createSignal<string>();
  const access = () => app.snapshot()?.access;
  const restoring = () => app.accessDiagnostic?.()?.restoring === true;
  const nativeLogin = () => app.accessOperations()?.startNativeLogin;
  const loginUrl = () => waltrSignInUrl(window.location);
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
          {restoring() ? "Restoring sign-in…" : access()?.status === "loginFailed" ? "Login did not complete" : "Sign in to continue"}
        </h1>
        <p class="access__copy">
          {restoring() ? "Checking your saved session." : !nativeLogin() && access()?.status === "loginInProgress"
            ? "Redirecting to your organization's sign-in page..."
            : "Your Waltr session is missing or has expired. Sign in to restore access."}
        </p>

        <NativeAccessDiagnostic />
        <Show when={!restoring() && !(nativeLogin() && app.accessDiagnostic?.()?.deviceUnsupported) && (access()?.error ?? localError())}>
          {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
        </Show>

        <Show when={!restoring()} fallback={<p role="status" aria-live="polite">Restoring sign-in…</p>}>
          <Show when={nativeLogin()} fallback={
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
          }>
            {(start) => <NativeSignIn start={start()} claimAutomaticNativeLogin={props.claimAutomaticNativeLogin} />}
          </Show>
        </Show>
        <ServerControls />
      </section>
    </main>
  );
}

function NativeSignIn(props: {
  start: (method: NativeLoginMethod) => Promise<void>;
  claimAutomaticNativeLogin?: () => boolean;
}) {
  const app = useAppState();
  const access = () => app.snapshot()?.access;
  const diagnostic = () => app.accessDiagnostic?.();
  const activeAttempt = () => access()?.status === "loginInProgress" ? access()?.activeAttemptId : undefined;
  const [busy, setBusy] = createSignal(false);
  const [method, setMethod] = createSignal<NativeLoginMethod>();
  const [error, setError] = createSignal<string>();
  const [expired, setExpired] = createSignal(false);
  const disabled = () => !diagnostic() || busy();
  const device = () => {
    const details = diagnostic()?.device;
    return details && details.attemptId === activeAttempt() ? details : undefined;
  };
  const code = () => {
    const details = device();
    return !busy() && !expired() && details && details.expiresAtMillis > Date.now()
      ? details : undefined;
  };
  let live = true;
  let autoStarted = false;
  onCleanup(() => { live = false; });

  createEffect(() => {
    const details = device();
    setExpired(false);
    if (!details) return;
    const timer = window.setTimeout(() => setExpired(true), Math.max(0, details.expiresAtMillis - Date.now()));
    onCleanup(() => window.clearTimeout(timer));
  });
  const changeLogin = async (next?: NativeLoginMethod) => {
    if (!live || disabled()) return;
    setBusy(true);
    setError(undefined);
    try {
      const attemptId = activeAttempt();
      if (attemptId) {
        const retired = await app.dispatch({ type: "cancelWaltrLogin", attemptId });
        if (!live) return;
        if (retired.access.activeAttemptId || activeAttempt()) {
          if (activeAttempt() === attemptId) setError("Unable to cancel sign-in. Try again.");
          return;
        }
      }
      if (next && live) {
        const status = access()?.status;
        if (status !== "loginRequired" && status !== "loginFailed") return;
        setMethod(next);
        await props.start(next);
      }
    } catch {
      if (live) {
        setError(next ? "Unable to start sign-in. Try again." : "Unable to cancel sign-in. Try again.");
      }
    } finally {
      if (live) setBusy(false);
    }
  };
  createEffect(() => {
    const status = access()?.status;
    const progress = diagnostic();
    if (!autoStarted && progress && !progress.restoring && (status === "loginRequired" || status === "loginFailed")) {
      autoStarted = true;
      if (props.claimAutomaticNativeLogin?.() !== false) void changeLogin("device");
    }
  });

  return (
    <>
      <Show when={error()}>{(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}</Show>
      <Show when={diagnostic()?.deviceUnsupported && !busy()}>
        <InlineAlert tone="info">QR sign-in is unavailable for this server. Sign in using this device’s browser.</InlineAlert>
      </Show>
      <Show when={code()}>
        {(details) => (
          <div class="access__device">
            <Show when={details().qr}>
              {(qr) => (
                <svg class="access__qr" role="img" aria-label="QR code for Waltr sign-in" viewBox={`0 0 ${qr().size} ${qr().size}`} shape-rendering="crispEdges">
                  <rect width={qr().size} height={qr().size} fill="white" />
                  <path d={qr().path} fill="black" />
                </svg>
              )}
            </Show>
            <p>On your phone, {details().qr ? "scan the QR code or visit" : "visit"} <span class="access__verification-url">{details().verificationUri}</span> and enter this code if asked: <strong>{details().userCode}</strong></p>
          </div>
        )}
      </Show>
      <Show when={busy() || activeAttempt()}>
        <p class="access__progress" role="status">
          {busy() ? "Preparing sign-in…" : expired() ? "This code has expired." : code()
            ? "Waiting for you to finish signing in on your phone."
            : method() === "browser" ? "Finish signing in using this device’s browser." : "Preparing a sign-in code…"}
        </p>
      </Show>
      <div class="access__progress-actions">
        <button class="ui-button" type="button" disabled={disabled() || (method() === "browser" && !!activeAttempt())} onClick={() => void changeLogin("browser")}>
          Sign in on this device
        </button>
        <Show when={!diagnostic()?.deviceUnsupported}>
          <button class="ui-button" type="button" disabled={disabled()} onClick={() => void changeLogin("device")}>
            {activeAttempt() && method() !== "browser" || method() === "device" && access()?.status === "loginFailed" ? "Get a new code" : "Sign in with a phone"}
          </button>
        </Show>
        <Show when={activeAttempt()}>
          <button class="ui-button" type="button" disabled={disabled()} onClick={() => void changeLogin()}>Cancel login</button>
        </Show>
      </div>
    </>
  );
}
