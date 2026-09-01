import { A, useLocation } from "@solidjs/router";
import { createEffect, createMemo, createSignal, onCleanup, onMount, Show } from "solid-js";
import type { ParentProps } from "solid-js";

import { useAppState } from "../../app/context";
import { reportApplicationView } from "../../telemetry";

export function ShellLayout(props: ParentProps<{ reload?: () => void }>) {
  const app = useAppState();
  const location = useLocation();
  const snapshot = createMemo(() => app.snapshot());
  const [loggingOut, setLoggingOut] = createSignal(false);
  const [logoutError, setLogoutError] = createSignal<string>();
  const [drawerOpen, setDrawerOpen] = createSignal(false);
  let drawer: HTMLElement | undefined;
  let drawerTrigger: HTMLButtonElement | undefined;
  let previousPath = location.pathname;

  const closeDrawer = (restoreFocus = true) => {
    if (!drawerOpen()) return;
    setDrawerOpen(false);
    if (restoreFocus) queueMicrotask(() => drawerTrigger?.focus());
  };

  const openDrawer = () => {
    setDrawerOpen(true);
    queueMicrotask(() => drawer?.querySelector<HTMLElement>(".shell__drawer-close")?.focus());
  };

  const closeDrawerForDesktop = () => {
    if (!drawerOpen()) return;
    setDrawerOpen(false);
    queueMicrotask(() => drawer?.querySelector<HTMLElement>(".shell__link.active")?.focus());
  };

  createEffect(() => {
    const path = location.pathname;
    reportApplicationView(path);
    if (path !== previousPath) closeDrawer(false);
    previousPath = path;
  });

  const currentNavigation = createMemo(() => {
    if (location.pathname.startsWith("/sessions")) return "Sessions";
    if (location.pathname.startsWith("/routines")) return "Routines";
    if (location.pathname.startsWith("/config")) return "Config";
    return "Home";
  });

  onMount(() => {
    const handleResize = () => {
      if (window.innerWidth > 700) closeDrawerForDesktop();
    };
    const handleKeyDown = (event: KeyboardEvent) => {
      if (!drawerOpen()) return;
      if (event.key === "Escape") {
        event.preventDefault();
        closeDrawer();
        return;
      }
      if (event.key !== "Tab" || !drawer) return;

      const focusable = Array.from(drawer.querySelectorAll<HTMLElement>(
        'a[href], button:not(:disabled), [tabindex]:not([tabindex="-1"])',
      )).filter((element) => !element.hasAttribute("hidden"));
      if (focusable.length === 0) return;
      const first = focusable[0]!;
      const last = focusable[focusable.length - 1]!;
      if (event.shiftKey && document.activeElement === first) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && document.activeElement === last) {
        event.preventDefault();
        first.focus();
      } else if (!drawer.contains(document.activeElement)) {
        event.preventDefault();
        first.focus();
      }
    };

    document.addEventListener("keydown", handleKeyDown);
    window.addEventListener("resize", handleResize);
    onCleanup(() => {
      document.removeEventListener("keydown", handleKeyDown);
      window.removeEventListener("resize", handleResize);
    });
  });
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
    <div class="shell" classList={{ "shell--drawer-open": drawerOpen() }}>
      <header class="shell__app-bar" inert={drawerOpen()} aria-hidden={drawerOpen() ? "true" : undefined}>
        <div class="shell__app-identity">
          <span class="shell__app-brand">Waltr</span>
          <span class="shell__app-current">{currentNavigation()}</span>
        </div>
        <span class="shell__readiness">
          <span classList={{ "shell__readiness-dot--ready": snapshot()?.status.ready }} aria-hidden="true" />
          {snapshot()?.status.ready ? "Ready" : "Waiting"}
        </span>
        <button
          ref={drawerTrigger}
          class="shell__menu-trigger"
          type="button"
          aria-label="Open navigation menu"
          aria-expanded={drawerOpen()}
          aria-controls="primary-navigation-drawer"
          onClick={openDrawer}
        >
          <span aria-hidden="true" />
          <span aria-hidden="true" />
          <span aria-hidden="true" />
        </button>
      </header>
      <button
        class="shell__backdrop"
        classList={{ "shell__backdrop--open": drawerOpen() }}
        type="button"
        aria-label="Close navigation drawer"
        tabindex={drawerOpen() ? 0 : -1}
        onClick={() => closeDrawer()}
      />
      <aside
        ref={drawer}
        id="primary-navigation-drawer"
        class="shell__sidebar"
        classList={{ "shell__drawer--open": drawerOpen() }}
        aria-label="Primary navigation"
        aria-modal={drawerOpen() ? "true" : undefined}
        role={drawerOpen() ? "dialog" : undefined}
      >
        <div class="shell__drawer-header">
          <A class="shell__brand" href="/" onClick={() => closeDrawer()}>
            Waltr
          </A>
          <button class="shell__drawer-close" type="button" aria-label="Close navigation menu" onClick={() => closeDrawer()}>
            <span aria-hidden="true">x</span>
          </button>
        </div>
        <nav class="shell__nav" aria-label="Main">
          <A
            class="shell__link"
            classList={{ active: location.pathname === "/" }}
            href="/"
            aria-current={location.pathname === "/" ? "page" : undefined}
            onClick={() => closeDrawer()}
          >
            Home
          </A>
          <A
            class="shell__link"
            classList={{ active: location.pathname.startsWith("/sessions") }}
            href="/sessions"
            aria-current={location.pathname.startsWith("/sessions") ? "page" : undefined}
            onClick={() => closeDrawer()}
          >
            Sessions
          </A>
          <A
            class="shell__link"
            classList={{ active: location.pathname.startsWith("/routines") }}
            href="/routines"
            aria-current={location.pathname.startsWith("/routines") ? "page" : undefined}
            onClick={() => closeDrawer()}
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
            onClick={() => closeDrawer()}
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
            <div class="shell__status-details">
              <div>
                <span>System</span>
                <strong>{snapshot()?.status.ready ? "ready" : "waiting"}</strong>
              </div>
              <div>
                <span>Status watch</span>
                <strong>{snapshot()?.status.watch.status ?? "idle"}</strong>
              </div>
            </div>
          </section>
        </div>
      </aside>
      <main class="shell__content" id="main-content" inert={drawerOpen()} aria-hidden={drawerOpen() ? "true" : undefined}>
        {props.children}
      </main>
    </div>
  );
}
