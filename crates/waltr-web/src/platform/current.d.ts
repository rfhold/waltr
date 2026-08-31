declare module "virtual:waltr-platform" {
  import type { ClientAdapterLoader } from "./client";

  export const loadClientAdapter: ClientAdapterLoader;
}
