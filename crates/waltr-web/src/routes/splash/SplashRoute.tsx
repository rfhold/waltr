import { createMemo } from "solid-js";

import { useAppState } from "../../app/context";
import { logoText } from "./splash";

export function SplashRoute() {
  const app = useAppState();
  const status = createMemo(() => {
    if (app.clientLoadError()) {
      return "Unable to load client.";
    }

    if (app.clientError()) {
      return "Unable to connect to Waltr.";
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
    </main>
  );
}
