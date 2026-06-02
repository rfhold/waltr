import { Navigate, useLocation, useNavigate } from "@solidjs/router";
import { Match, Switch, createEffect, createMemo, createSignal, on, onCleanup, untrack } from "solid-js";
import type { ParentProps } from "solid-js";

import { useAppState } from "../../app/context";
import { InlineAlert, TabPanel, Tabs } from "../../components/ui";
import type { TabItem } from "../../components/ui";
import type { ClientCommand } from "../../generated/waltr-client";
import { McpServersSection } from "./McpServersSection";
import { ProvidersSection } from "./ProvidersSection";

export {
  ConfigCreateRoute,
  ConfigEditRoute,
  ConfigModelCreateRoute,
  ConfigModelEditRoute,
} from "./ProvidersSection";
export { ConfigMcpCreateRoute, ConfigMcpEditRoute } from "./McpServersSection";

type ConfigTabId = "providers" | "mcp";

const CONFIG_TABS: readonly TabItem<ConfigTabId>[] = [
  { id: "providers", label: "Providers", href: "/config/provider" },
  { id: "mcp", label: "MCP Servers", href: "/config/mcp" },
];

export function ConfigRoute(props: ParentProps) {
  const app = useAppState();
  const location = useLocation();
  const navigate = useNavigate();
  const snapshot = createMemo(() => app.snapshot());
  const watchedConfig = createMemo(() =>
    snapshot()?.config.watch.status === "received"
      ? (snapshot()?.config.current ?? undefined)
      : undefined,
  );
  const error = createMemo(
    () => snapshot()?.config.watch.error ?? snapshot()?.config.load?.error,
  );
  const loading = createMemo(() => !watchedConfig() && !error());
  const [activeTab, setActiveTab] = createSignal<ConfigTabId>(CONFIG_TABS[0]!.id);
  const routeTab = createMemo<ConfigTabId | undefined>(() => {
    if (location.pathname.startsWith("/config/mcp")) {
      return "mcp";
    }
    if (location.pathname.startsWith("/config/provider")) {
      return "providers";
    }
    return undefined;
  });
  const visibleTab = createMemo(() => activeTab());
  createEffect(on(routeTab, (tab) => {
    if (tab) {
      setActiveTab(tab);
    }
  }));
  let registeredConfigWatchInterest = false;

  const readyForConfigWatch = createMemo(() => app.clientLoaded() && app.systemReady());

  createEffect(on(readyForConfigWatch, (ready) => {
    if (!ready || registeredConfigWatchInterest) {
      return;
    }

    registeredConfigWatchInterest = true;
    untrack(() => dispatchConfigWatchCommand(app.dispatch, {
      type: "registerConfigWatchInterest",
    }));
  }));

  onCleanup(() => {
    if (!registeredConfigWatchInterest) {
      return;
    }

    dispatchConfigWatchCommand(app.dispatch, {
      type: "releaseConfigWatchInterest",
    });
  });

  return (
    <section class="config" aria-labelledby="config-title">
      <ShowRedirect when={location.pathname === "/config" || location.pathname === "/config/"} />
      <header class="config__header">
        <p class="eyebrow">Configuration</p>
        <h1 id="config-title">Settings</h1>
      </header>
      <Tabs
        id="config-sections"
        label="Configuration sections"
        tabs={CONFIG_TABS}
        selected={visibleTab()}
        onSelect={(tab) => {
          setActiveTab(tab);
          navigate(tab === "mcp" ? "/config/mcp" : "/config/provider");
        }}
      />
      <TabPanel
        id={`config-sections-${visibleTab()}-panel`}
        tabId={`config-sections-${visibleTab()}-tab`}
      >
        <Switch>
          <Match when={error()}>
            <InlineAlert tone="danger">Unable to load configuration.</InlineAlert>
          </Match>
          <Match when={loading()}>
            <InlineAlert>
              Loading configuration... ({snapshot()?.config.watch.status ?? "idle"})
            </InlineAlert>
          </Match>
          <Match when={visibleTab() === "providers"}>
            <ProvidersSection watchedConfig={watchedConfig}>
              {props.children}
            </ProvidersSection>
          </Match>
          <Match when={visibleTab() === "mcp"}>
            <McpServersSection watchedConfig={watchedConfig}>
              {props.children}
            </McpServersSection>
          </Match>
        </Switch>
      </TabPanel>
    </section>
  );
}

function ShowRedirect(props: { when: boolean }) {
  return props.when ? <Navigate href="/config/provider" /> : null;
}

function dispatchConfigWatchCommand(
  dispatch: (command: ClientCommand) => Promise<unknown>,
  command: ClientCommand,
) {
  void dispatch(command).catch(() => undefined);
}
