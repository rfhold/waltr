import type { ClientCommand, ClientSnapshot } from "../generated/waltr-client";

export type ClientSnapshotListener = (snapshot: ClientSnapshot) => void;

export type AccessProgress =
  | { type: "native" }
  | { type: "loggingOut" }
  | { type: "unavailable"; message: string };

export type NativeAccessOperations = {
  progress: () => AccessProgress | undefined;
  subscribe: (listener: (progress: AccessProgress) => void) => () => void;
  start: () => Promise<void>;
  cancel: () => Promise<void>;
  logout: () => Promise<void>;
};

export type ClientAdapter = {
  snapshot: () => ClientSnapshot | undefined;
  start: () => Promise<void>;
  subscribe: (listener: ClientSnapshotListener) => () => void;
  dispatch: (command: ClientCommand) => Promise<ClientSnapshot>;
  dispose: () => void | Promise<void>;
  access?: NativeAccessOperations;
  openExternalUrl?: (url: string) => Promise<void>;
};

export type ClientAdapterLoader = () => Promise<ClientAdapter>;

export type BuildPlatform = "browser" | "tauri";

export function telemetryAppName(platform: BuildPlatform): "waltr-spa" | "waltr-tauri" {
  return platform === "tauri" ? "waltr-tauri" : "waltr-spa";
}
