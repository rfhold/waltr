import { A, useLocation } from "@solidjs/router";
import { createMemo, createSignal, Show } from "solid-js";
import type { ParentProps } from "solid-js";

import { useAppState } from "../../app/context";

export function ShellLayout(props: ParentProps<{ reload?: () => void }>) {
  const app = useAppState();
  const location = useLocation();
  const snapshot = createMemo(() => app.snapshot());
  const [loggingOut, setLoggingOut] = createSignal(false);
  const [logoutError, setLogoutError] = createSignal<string>();
  const logout = async () => {
    setLoggingOut(true);
    setLogoutError(undefined);

    const nativeAccess = app.nativeAccess();
    if (nativeAccess) {
      try {
        await nativeAccess.logout();
      } catch {
        setLogoutError("Waltr could not sign you out. Try again.");
      } finally {
        setLoggingOut(false);
      }
      return;
    }

    let response: Response;
    try {
      response = await fetch("/oidc/logout", { method: "POST", credentials: "same-origin" });
    } catch {
      setLogoutError("Unable to reach Waltr to sign out. Check your connection and try again.");
      setLoggingOut(false);
      return;
    }

    if (!response.ok) {
      setLogoutError("Waltr could not sign you out. Try again.");
      setLoggingOut(false);
      return;
    }

    try {
      await app.dispatch({ type: "logoutWaltr" });
    } catch {
      setLogoutError("The server session ended, but the app could not clear local state. Reloading...");
      setLoggingOut(false);
      (props.reload ?? (() => window.location.reload()))();
      return;
    }
    setLoggingOut(false);
  };

  return (
    <div class="shell">
      <aside class="shell__sidebar" aria-label="Primary navigation">
        <A class="shell__brand" href="/">
          Waltr
        </A>
        <nav class="shell__nav" aria-label="Main">
          <A
            class="shell__link"
            classList={{ active: location.pathname === "/" }}
            href="/"
            aria-current={location.pathname === "/" ? "page" : undefined}
          >
            Home
          </A>
          <A
            class="shell__link"
            classList={{ active: location.pathname.startsWith("/sessions") }}
            href="/sessions"
            aria-current={location.pathname.startsWith("/sessions") ? "page" : undefined}
          >
            Sessions
          </A>
          <A
            class="shell__link"
            classList={{ active: location.pathname.startsWith("/routines") }}
            href="/routines"
            aria-current={location.pathname.startsWith("/routines") ? "page" : undefined}
          >
            Routines
          </A>
        </nav>
        <div class="shell__sidebar-bottom">
          <A
            class="shell__link"
            classList={{ active: location.pathname.startsWith("/config") }}
            href="/config/provider"
            aria-current={location.pathname.startsWith("/config") ? "page" : undefined}
          >
            Config
          </A>
          <button class="shell__logout" type="button" disabled={loggingOut()} onClick={logout}>
            {loggingOut() ? "Signing out..." : "Sign out"}
          </button>
          <Show when={logoutError()}>
            {(message) => <p class="shell__logout-error" role="alert">{message()}</p>}
          </Show>
          <section class="shell__status" aria-label="Global system status" role="status" aria-live="polite">
            <div>
              <span>System</span>
              <strong>{snapshot()?.status.ready ? "ready" : "waiting"}</strong>
            </div>
            <div>
              <span>Status watch</span>
              <strong>{snapshot()?.status.watch.status ?? "idle"}</strong>
            </div>
          </section>
        </div>
      </aside>
      <main class="shell__content" id="main-content">
        {props.children}
      </main>
    </div>
  );
}
