import { Show, createMemo } from "solid-js";

import { useAppState } from "../../app/context";
import { logoText } from "./splash";

export function SplashRoute(props: { remote?: boolean }) {
  const app = useAppState();
  const status = createMemo(() => {
    if (app.clientLoadError()) {
      return "Unable to load client.";
    }

    if (app.clientError()) {
      const error = app.clientError();
      return error instanceof Error && error.message === "Native access discovery failed. Retry connection."
        ? "Unable to discover native sign-in configuration. Retry connection or switch server."
        : "Unable to connect to Waltr.";
    }

    const statusError = app.snapshot()?.status.watch.error;
    if (statusError) {
      return `Waiting for Waltr: ${statusError}`;
    }

    return app.clientLoaded() ? "Connecting to Waltr..." : "Preparing client...";
  });

  return (
    <main class="splash" aria-label="Waltr splash screen">
      <pre class="splash__logo" aria-label="Waltr logo">
        {logoText}
      </pre>
      <p class="splash__status" role="status" aria-live="polite">
        {status()}
      </p>
      <Show when={props.remote}>
        <a class="ui-button splash__recovery" href="/" rel="external">
          {__WALTR_BUILD_PLATFORM__ === "tauri" ? "Return to Local" : "Return to current server"}
        </a>
      </Show>
    </main>
  );
}
