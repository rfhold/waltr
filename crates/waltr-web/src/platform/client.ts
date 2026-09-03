import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";

export type ClientSnapshotListener = (snapshot: ClientSnapshot) => void;

export type AccessOperations = {
  openLogin: (url: string) => void | Promise<void>;
  logout: () => Promise<boolean>;
};

export type ClientAdapter = {
  snapshot: () => ClientSnapshot | undefined;
  start: () => Promise<void>;
  subscribe: (listener: ClientSnapshotListener) => () => void;
  dispatch: (command: ClientCommand) => Promise<ClientSnapshot>;
  dispose: () => void | Promise<void>;
  access?: AccessOperations;
  openAuthorizationAttempt?: (attemptId: string) => Promise<void>;
  openExternalUrl?: (url: string) => Promise<void>;
};

export type ClientAdapterLoader = () => Promise<ClientAdapter>;

export type BuildPlatform = "browser" | "tauri";

export function telemetryAppName(platform: BuildPlatform): "waltr-spa" | "waltr-tauri" {
  return platform === "tauri" ? "waltr-tauri" : "waltr-spa";
}
