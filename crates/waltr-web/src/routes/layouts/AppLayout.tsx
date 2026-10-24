import { Show, type ParentProps } from "solid-js";

import { useAppState } from "../../app/context";
import { AccessRoute, NativeAccessDiagnostic } from "../access/AccessRoute";
import { OAuthDialog } from "../auth/OAuthDialog";
import { SplashRoute } from "../splash/SplashRoute";

export function AppLayout(props: ParentProps) {
  const app = useAppState();
  const accessStatus = () => app.snapshot()?.access.status;
  const authenticated = () => accessStatus() === "authenticated";
  const loginScreen = () => accessStatus() === "loginRequired" || accessStatus() === "loginInProgress" || accessStatus() === "loginFailed";
  const showSplash = () => (
    (!authenticated() && !loginScreen())
    || (authenticated() && (!app.systemReady() || !app.minimumSplashElapsed()))
  );

  return (
    <div class="app" data-app-phase={app.phase()}>
      <Show when={authenticated()}>
        <NativeAccessDiagnostic />
        {props.children}
        <OAuthDialog />
      </Show>
      {loginScreen() ? <AccessRoute /> : null}
      {showSplash() ? <SplashRoute /> : null}
    </div>
  );
}
