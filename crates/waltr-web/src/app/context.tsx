import {
  createContext,
  useContext,
  type Accessor,
  type ParentProps,
} from "solid-js";

import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";

export type AppPhase = "splash" | "ready";

export type AppState = {
  phase: Accessor<AppPhase>;
  minimumSplashElapsed: Accessor<boolean | undefined>;
  snapshot: Accessor<ClientSnapshot | undefined>;
  clientError: Accessor<unknown>;
  clientLoadError: Accessor<unknown>;
  clientLoaded: Accessor<boolean>;
  systemReady: Accessor<boolean>;
  dispatch: (command: ClientCommand) => Promise<ClientSnapshot>;
};

export const AppStateContext = createContext<AppState>();

export function AppStateContextProvider(props: ParentProps<{ value: AppState }>) {
  return (
    <AppStateContext.Provider value={props.value}>
      {props.children}
    </AppStateContext.Provider>
  );
}

export function useAppState() {
  const state = useContext(AppStateContext);

  if (!state) {
    throw new Error("missing app state provider");
  }

  return state;
}
