import {
  createContext,
  useContext,
  type Accessor,
  type ParentProps,
} from "solid-js";

import type { ClientAuthTarget, ClientCommand, ClientSnapshot } from "../generated/waltr-client";
import type { AccessOperations } from "../platform/client";

export type AppPhase = "splash" | "ready";

export type AppState = {
  phase: Accessor<AppPhase>;
  minimumSplashElapsed: Accessor<boolean | undefined>;
  snapshot: Accessor<ClientSnapshot | undefined>;
  clientError: Accessor<unknown>;
  clientLoadError: Accessor<unknown>;
  clientLoaded: Accessor<boolean>;
  systemReady: Accessor<boolean>;
  explicitAuthTarget: Accessor<ClientAuthTarget | undefined>;
  presentedAuthAttemptId: Accessor<string | undefined>;
  authorizationDraft: Accessor<{ attemptId: string; input: string } | undefined>;
  accessOperations: Accessor<AccessOperations | undefined>;
  openAuthorizationAttempt: Accessor<((attemptId: string) => Promise<void>) | undefined>;
  openExternalUrl: Accessor<((url: string) => Promise<void>) | undefined>;
  dispatch: (command: ClientCommand) => Promise<ClientSnapshot>;
  dispatchBeginExplicitAuth: (target: ClientAuthTarget) => Promise<ClientSnapshot>;
  dispatchCompleteAuth: (attemptId: string, input: string) => Promise<ClientSnapshot>;
  dispatchCancelAuth: (attemptId: string) => Promise<ClientSnapshot>;
  dispatchRefreshAuth: (target: ClientAuthTarget) => Promise<ClientSnapshot>;
  dispatchLogoutAuth: (target: ClientAuthTarget) => Promise<ClientSnapshot>;
  clearExplicitAuthTarget: () => void;
  setAuthorizationDraft: (attemptId: string, input: string) => void;
  clearAuthorizationDraft: () => void;
  dispatchConfigPatch: (command: ClientCommand) => Promise<ClientSnapshot>;
  dispatchResetConfig: () => Promise<ClientSnapshot>;
  dispatchCreateSession: (agentName: string) => Promise<ClientSnapshot>;
  dispatchDeleteSession: (id: string) => Promise<ClientSnapshot>;
  dispatchWipeSessions: () => Promise<ClientSnapshot>;
  dispatchUpdateSessionInferenceSettings: (command: Extract<ClientCommand, { type: "updateSessionInferenceSettings" }>) => Promise<ClientSnapshot>;
  dispatchUpdateSessionAgent: (id: string, agentName: string) => Promise<ClientSnapshot>;
  dispatchSubmitUserMessage: (sessionId: string, text: string) => Promise<ClientSnapshot>;
  dispatchRespondExternalRequest: (sessionId: string, requestId: string, responseJson: string) => Promise<ClientSnapshot>;
  dispatchInterruptSession: (sessionId: string) => Promise<ClientSnapshot>;
  dispatchContinueSession: (command: Extract<ClientCommand, { type: "continueSessionFromTurn" | "continueSessionFromCursor" }>) => Promise<ClientSnapshot>;
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
