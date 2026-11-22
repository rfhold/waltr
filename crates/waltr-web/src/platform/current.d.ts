declare module "virtual:waltr-platform" {
  import type { ClientAdapter } from "./client";

  export const loadClientAdapter: (destination?: string) => Promise<ClientAdapter>;
}

declare const __WALTR_BUILD_PLATFORM__: "browser" | "tauri";
declare const __WALTR_BUILD_VERSION__: string;
declare const __WALTR_TELEMETRY_ENABLED__: boolean;
