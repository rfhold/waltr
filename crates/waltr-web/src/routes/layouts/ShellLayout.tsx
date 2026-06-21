import { A, useLocation } from "@solidjs/router";
import { createMemo } from "solid-js";
import type { ParentProps } from "solid-js";

import { useAppState } from "../../app/context";

export function ShellLayout(props: ParentProps) {
  const app = useAppState();
  const location = useLocation();
  const snapshot = createMemo(() => app.snapshot());

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
