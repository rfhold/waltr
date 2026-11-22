import { createMemo, createResource, For, Show } from "solid-js";
import { useLocation } from "@solidjs/router";

import { useAppState } from "../../app/context";
import { remoteScope } from "../../app/destination";

export function DestinationRail() {
  const app = useAppState();
  const location = useLocation();
  const scope = createMemo(() => remoteScope(location.pathname));
  const authRevision = createMemo(() => {
    const snapshot = app.snapshot();
    if (!snapshot || snapshot.access.status !== "authenticated") return undefined;
    if (scope()) return "remote";
    return JSON.stringify(Object.entries(snapshot.auth.statuses)
      .filter(([key]) => key.startsWith("remote:"))
      .map(([key, value]) => [key, value?.kind, value?.metadata.remoteRouteId]));
  });
  const [entries, { refetch }] = createResource(
    () => {
      const read = app.entryDestinations?.();
      const revision = authRevision();
      return read && revision !== undefined ? { read, revision } : undefined;
    },
    async ({ read }) => read().then((entries) => ({ entries, failed: false }), () => ({ entries: [], failed: true })),
  );
  const entryLabel = __WALTR_BUILD_PLATFORM__ === "tauri" ? "Local" : "Current server";
  const validEntries = () => entries()?.entries.filter((entry) =>
    /^[a-zA-Z0-9_.-]{1,128}$/.test(entry.name) && ![".", "..", "local"].includes(entry.name)) ?? [];
  const unavailableScope = () => scope() && !validEntries().some((entry) =>
    scope()?.selector === `${entry.name}@${entry.routeId}`);

  return (
    <nav class="destination-rail" aria-label="Server views">
      <a class="destination-rail__circle" href="/" rel="external" title={entryLabel}
        aria-label={entryLabel} aria-current={!scope() ? "true" : undefined}>{entryLabel[0]}</a>
      <For each={validEntries()}>{(entry) => {
        const ready = () => Boolean(entry.routeId && /^[0-9a-f]{32}$/.test(entry.routeId));
        const label = () => ready() ? entry.name : `Manage ${entry.name}`;
        return <a class="destination-rail__circle" href={ready() ? `/remote/${entry.name}~${entry.routeId}/` : "/config/remotes"}
          rel="external" title={label()} aria-label={label()}
          aria-current={scope()?.selector === `${entry.name}@${entry.routeId}` ? "true" : undefined}>
          {entry.name[0]?.toUpperCase()}
        </a>;
      }}</For>
      <Show when={unavailableScope() ? scope() : undefined}>
        {(remote) => <span class="destination-rail__circle" aria-current="true" aria-label={remote().name}
          title={remote().name}>{remote().name[0]?.toUpperCase()}</span>}
      </Show>
      <a class="destination-rail__circle" rel="external" href="/config/remotes" title="Manage Remotes" aria-label="Manage Remotes">+</a>
      <Show when={entries()?.failed}>
        <button class="destination-rail__circle" type="button" onClick={() => void refetch()}
          aria-label="Retry server list" title="Retry server list">↻</button>
      </Show>
    </nav>
  );
}
