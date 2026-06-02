import { AppStateProvider } from "./state";
import { AppRouter } from "../routes";
import type { WasmClientLoader } from "../wasm/client";

export type AppProps = {
  loadWasmClient?: WasmClientLoader;
};

export function App(props: AppProps) {
  return (
    <AppStateProvider loadWasmClient={props.loadWasmClient}>
      <AppRouter />
    </AppStateProvider>
  );
}
