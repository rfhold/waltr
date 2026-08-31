import { AppStateProvider } from "./state";
import { AppRouter } from "../routes";
import { loadClientAdapter } from "virtual:waltr-platform";
import type { ClientAdapterLoader } from "../platform/client";

export type AppProps = {
  loadClientAdapter?: ClientAdapterLoader;
};

export function App(props: AppProps) {
  return (
    <AppStateProvider loadClientAdapter={props.loadClientAdapter ?? loadClientAdapter}>
      <AppRouter />
    </AppStateProvider>
  );
}
