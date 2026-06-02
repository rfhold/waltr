import type { ParentProps } from "solid-js";

import { useAppState } from "../../app/context";
import { SplashRoute } from "../splash/SplashRoute";

export function AppLayout(props: ParentProps) {
  const app = useAppState();
  const showSplash = () => !app.systemReady() || !app.minimumSplashElapsed();

  return (
    <div class="app" data-app-phase={app.phase()}>
      {props.children}
      {showSplash() ? <SplashRoute /> : null}
    </div>
  );
}
