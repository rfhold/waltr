import { createEffect, Show, type ParentProps } from "solid-js";

import { useAppState } from "../../app/context";
import { AccessRoute, NativeAccessDiagnostic } from "../access/AccessRoute";
import { OAuthDialog } from "../auth/OAuthDialog";
import { SplashRoute } from "../splash/SplashRoute";
import { AudioFailureAlert } from "../../components/AudioFailureAlert";

export function AppLayout(props: ParentProps<{ remote?: boolean }>) {
  const app = useAppState();
  const accessStatus = () => app.snapshot()?.access.status;
  const authenticated = () => accessStatus() === "authenticated";
  let automaticLoginClaimed = false;
  createEffect(() => {
    if (authenticated()) automaticLoginClaimed = false;
  });
  const claimAutomaticNativeLogin = () => {
    if (automaticLoginClaimed) return false;
    automaticLoginClaimed = true;
    return true;
  };
  const loginScreen = () => (!authenticated() && app.accessDiagnostic?.()?.restoring === true) || accessStatus() === "loginRequired" || accessStatus() === "loginInProgress" || accessStatus() === "loginFailed";
  const showSplash = () => (
    (!authenticated() && !loginScreen())
    || (authenticated() && (!app.systemReady() || !app.minimumSplashElapsed()))
  );

  return (
    <div class="app" data-app-phase={app.phase()}>
      <Show when={props.remote && loginScreen()}>
        <nav class="destination-rail destination-rail--recovery" aria-label="Server views">
          <a class="destination-rail__circle" href="/" rel="external" aria-label="Return to current server"
            title="Return to current server">{__WALTR_BUILD_PLATFORM__ === "tauri" ? "L" : "C"}</a>
        </nav>
      </Show>
      <AudioFailureAlert />
      <Show when={authenticated()}>
        <NativeAccessDiagnostic />
        {props.children}
        <OAuthDialog />
      </Show>
      {loginScreen() ? <AccessRoute claimAutomaticNativeLogin={claimAutomaticNativeLogin} /> : null}
      {showSplash() ? <SplashRoute remote={props.remote} /> : null}
    </div>
  );
}
