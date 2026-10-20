import { createResource, createSignal, For, Show, type JSX } from "solid-js";
import type { ClientAdapterLoader } from "../platform/client";
import type { ServerProfile, serverRegistry } from "../platform/tauri";
import { ServerSelectionContext } from "./serverSelection";

export type ServerPickerProps = {
  registry: typeof serverRegistry;
  loaderFor: (serverId: string) => ClientAdapterLoader;
  children: (loader: ClientAdapterLoader) => JSX.Element;
};

export function ServerPicker(props: ServerPickerProps) {
  const [selection, setSelection] = createSignal<{ id: string; loader: ClientAdapterLoader }>();
  const [url, setUrl] = createSignal("");
  const [name, setName] = createSignal("");
  const [error, setError] = createSignal<string>();
  const [busy, setBusy] = createSignal(false);
  const [listError, setListError] = createSignal(false);
  let initialized = false;
  const connect = (id: string) => {
    window.history.replaceState(null, "", "/");
    setSelection({ id, loader: props.loaderFor(id) });
  };
  const [registry, { refetch }] = createResource(async () => {
    setListError(false);
    let result;
    try {
      result = await props.registry.list();
    } catch {
      setListError(true);
      return undefined;
    }
    if (!initialized) {
      initialized = true;
      if (result.lastConnectedId && result.servers.some((server) => server.id === result.lastConnectedId)) {
        connect(result.lastConnectedId);
      }
    }
    return result;
  });
  const switchServer = () => {
    setSelection(undefined);
    setError(undefined);
    void refetch();
  };
  const retry = () => {
    const id = selection()?.id;
    setSelection(undefined);
    if (id) connect(id);
  };
  const save = async (event: SubmitEvent) => {
    event.preventDefault();
    setError(undefined);
    let parsed: URL;
    try {
      parsed = new URL(url().trim());
      if (!["http:", "https:"].includes(parsed.protocol) || parsed.username || parsed.password || parsed.search || parsed.hash) throw new Error();
    } catch {
      setError("Enter an HTTP or HTTPS server URL without credentials, query, or fragment.");
      return;
    }
    if (name().trim().length > 100) {
      setError("Server name must be 100 characters or fewer.");
      return;
    }
    setBusy(true);
    try {
      await props.registry.save(parsed.href, name().trim() || undefined);
      setUrl("");
      setName("");
      await refetch();
    } catch {
      setError("Unable to save the server. Check the URL and try again.");
    } finally {
      setBusy(false);
    }
  };
  const remove = async (server: ServerProfile) => {
    setBusy(true);
    setError(undefined);
    try {
      await props.registry.remove(server.id);
      await refetch();
    } catch {
      setError("Unable to remove the server. Try again.");
    } finally {
      setBusy(false);
    }
  };
  return (
    <ServerSelectionContext.Provider value={{ switchServer, retry }}>
      <Show when={selection()} keyed fallback={
        <main class="access">
          <section class="access__panel" aria-label="Server picker">
            <h1>Choose a server</h1>
            <p>Connect to this device or a saved Waltr server. Switching keeps you signed in.</p>
            <Show when={registry.loading}><p role="status">Loading servers...</p></Show>
            <Show when={listError()}>
              <p role="alert">Unable to load saved servers.</p>
              <button class="ui-button" type="button" onClick={() => void refetch()}>Retry loading servers</button>
            </Show>
            <For each={registry()?.servers}>{(server) => (
              <div>
                <button class="ui-button" type="button" disabled={busy()} onClick={() => connect(server.id)}>{server.name}</button>
                <Show when={server.url}><p>{server.url}</p></Show>
                <Show when={server.id !== "local"}>
                  <button class="ui-button" type="button" disabled={busy()} aria-label={`Remove ${server.name}`} onClick={() => void remove(server)}>Remove</button>
                </Show>
              </div>
            )}</For>
            <form class="config__form" onSubmit={save}>
              <h2>Add server</h2>
              <label>Server URL<input type="url" required placeholder="https://waltr.example.com" value={url()} onInput={(event) => setUrl(event.currentTarget.value)} /></label>
              <label>Name (optional)<input maxlength={100} value={name()} onInput={(event) => setName(event.currentTarget.value)} /></label>
              <button class="ui-button" type="submit" disabled={busy()}>Add server</button>
            </form>
            <p>Removing a server deletes its saved sign-in on this device, not its server data.</p>
            <Show when={error()}>{(message) => <p role="alert">{message()}</p>}</Show>
          </section>
        </main>
      }>{(selected) => props.children(selected.loader)}</Show>
    </ServerSelectionContext.Provider>
  );
}
