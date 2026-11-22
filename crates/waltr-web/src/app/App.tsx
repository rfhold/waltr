import { AppStateProvider } from "./state";
import { AppRouter } from "../routes";
import { loadClientAdapter } from "virtual:waltr-platform";
import type { ClientAdapterLoader } from "../platform/client";
import { remoteScope } from "./destination";

export type AppProps = {
  loadClientAdapter?: ClientAdapterLoader;
};

export function App(props: AppProps) {
  const scope = remoteScope(window.location.pathname);
  return (
    <AppStateProvider loadClientAdapter={props.loadClientAdapter ?? (() => scope && !scope.selector
        ? Promise.reject(new Error("Open this destination from Config → Remotes."))
        : loadClientAdapter(scope?.selector))}>
      <AppRouter base={scope?.base} />
    </AppStateProvider>
  );
}
