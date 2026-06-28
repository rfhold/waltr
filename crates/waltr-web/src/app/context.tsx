import {
  createContext,
  useContext,
  type Accessor,
  type ParentProps,
} from "solid-js";

import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";
import type {
  CatalogSearchRequest,
  CatalogSearchResponse,
  CatalogSuggestRequest,
  CatalogSuggestResponse,
} from "../wasm/client";

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
  dispatchConfigPatch: (command: ClientCommand) => Promise<ClientSnapshot>;
  searchCatalogModels: (request: CatalogSearchRequest) => Promise<CatalogSearchResponse>;
  suggestCatalogMatches: (request: CatalogSuggestRequest) => Promise<CatalogSuggestResponse>;
  dispatchCreateSession: () => Promise<ClientSnapshot>;
  dispatchDeleteSession: (id: string) => Promise<ClientSnapshot>;
  dispatchUpdateSessionRuntimeConfig: (command: Extract<ClientCommand, { type: "updateSessionRuntimeConfig" }>) => Promise<ClientSnapshot>;
  dispatchSubmitUserMessage: (sessionId: string, text: string) => Promise<ClientSnapshot>;
  dispatchRespondExternalRequest: (sessionId: string, requestId: string, responseJson: string) => Promise<ClientSnapshot>;
  dispatchInterruptSession: (sessionId: string) => Promise<ClientSnapshot>;
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
