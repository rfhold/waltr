import { AppStateProvider } from "./state";
import { AppRouter } from "../routes";
import { loadTauriClient, serverRegistry } from "../platform/tauri";
import { ServerPicker } from "./ServerPicker";

export default function NativeApp() {
  return (
    <ServerPicker registry={serverRegistry} loaderFor={(id) => () => loadTauriClient(undefined, id)}>
      {(loader) => <AppStateProvider loadClientAdapter={loader}><AppRouter /></AppStateProvider>}
    </ServerPicker>
  );
}
