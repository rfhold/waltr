import { lazy } from "solid-js";
import { fixtureSnapshotForPath } from "./fixtures";
import { AppStateProvider } from "./state";
import { AppRouter } from "../routes";
import { loadClientAdapter } from "virtual:waltr-platform";
import type { ClientAdapterLoader } from "../platform/client";

export type AppProps = {
  loadClientAdapter?: ClientAdapterLoader;
};

export function App(props: AppProps) {
  if (__WALTR_BUILD_PLATFORM__ === "tauri" && !props.loadClientAdapter && !fixtureSnapshotForPath(window.location.pathname)) {
    const NativeApp = lazy(() => import("./NativeApp"));
    return <NativeApp />;
  }
  return (
    <AppStateProvider loadClientAdapter={props.loadClientAdapter ?? loadClientAdapter}>
      <AppRouter />
    </AppStateProvider>
  );
}
