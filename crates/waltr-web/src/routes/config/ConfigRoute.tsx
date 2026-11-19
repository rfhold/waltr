import { Navigate, useLocation, useNavigate } from "@solidjs/router";
import { For, Match, Show, Switch, createEffect, createMemo, createSignal, on, onCleanup, untrack } from "solid-js";
import type { ParentProps } from "solid-js";

import { useAppState } from "../../app/context";
import { ConfirmAction, InlineAlert, TabPanel, Tabs } from "../../components/ui";
import type { TabItem } from "../../components/ui";
import type { ClientCommand } from "../../generated/waltr-client";
import { McpServersSection } from "./McpServersSection";
import { ProvidersSection } from "./ProvidersSection";
import { InferenceDefaultsSection } from "./InferenceDefaultsSection";
import { AuthStatusPanel } from "./AuthStatusPanel";

export {
  ConfigCreateRoute,
  ConfigEditRoute,
  ConfigModelCreateRoute,
  ConfigModelEditRoute,
} from "./ProvidersSection";
export { ConfigMcpCreateRoute, ConfigMcpEditRoute } from "./McpServersSection";

type ConfigTabId = "providers" | "mcp" | "defaults" | "workspaceBindings";

const CONFIG_TABS: readonly TabItem<ConfigTabId>[] = [
  { id: "providers", label: "Providers", href: "/config/provider" },
  { id: "mcp", label: "MCP Servers", href: "/config/mcp" },
  { id: "defaults", label: "Defaults", href: "/config/defaults" },
  { id: "workspaceBindings", label: "Workspace Bindings", href: "/config/workspace-bindings" },
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
  const resourceBaselines = createMemo(() => snapshot()?.config.resourceBaselines ?? {});
  const workspaceBindings = createMemo(() => snapshot()?.config.workspaceBindings ?? {});
  const error = createMemo(
    () => snapshot()?.config.watch.error ?? snapshot()?.config.load?.error,
  );
  const loading = createMemo(() => !watchedConfig() && !error());
  const [activeTab, setActiveTab] = createSignal<ConfigTabId>(CONFIG_TABS[0]!.id);
  const [resetConfirmationOpen, setResetConfirmationOpen] = createSignal(false);
  const [resetPending, setResetPending] = createSignal(false);
  const [resetError, setResetError] = createSignal<string>();
  const [resetSucceeded, setResetSucceeded] = createSignal(false);
  const routeTab = createMemo<ConfigTabId | undefined>(() => {
    if (location.pathname.startsWith("/config/mcp")) {
      return "mcp";
    }
    if (location.pathname.startsWith("/config/defaults")) {
      return "defaults";
    }
    if (location.pathname.startsWith("/config/provider")) {
      return "providers";
    }
    if (location.pathname === "/config/workspace-bindings") {
      return "workspaceBindings";
    }
    return undefined;
  });
  const visibleTab = createMemo(() => activeTab());
  const visibleTabs = createMemo(() =>
    Object.keys(workspaceBindings()).length > 0 || visibleTab() === "workspaceBindings"
      ? CONFIG_TABS
      : CONFIG_TABS.filter((tab) => tab.id !== "workspaceBindings"),
  );
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
    untrack(() => dispatchConfigWatchCommand(app.dispatch, {
      type: "requestAuthStatuses",
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

  const resetConfig = async () => {
    if (resetPending()) {
      return;
    }

    setResetPending(true);
    setResetError(undefined);
    setResetSucceeded(false);
    try {
      await app.dispatchResetConfig();
      setResetConfirmationOpen(false);
      setResetSucceeded(true);
    } catch (error) {
      setResetError(error instanceof Error ? error.message : String(error));
    } finally {
      setResetPending(false);
    }
  };

  return (
    <section class="config" aria-labelledby="config-title">
      <ShowRedirect when={location.pathname === "/config" || location.pathname === "/config/"} />
      <header class="config__header">
        <div class="config__heading">
          <p class="eyebrow">Configuration</p>
          <h1 id="config-title">Settings</h1>
        </div>
        <Show when={watchedConfig()}>
          <div class="config__reset-control">
            <Show
              when={resetConfirmationOpen()}
              fallback={
                <button
                  class="ui-button ui-button--danger"
                  type="button"
                  onClick={() => {
                    setResetError(undefined);
                    setResetSucceeded(false);
                    setResetConfirmationOpen(true);
                  }}
                >
                  Reset configuration
                </button>
              }
            >
              <ConfirmAction
                label="Reset configuration confirmation"
                prompt="Reset providers, MCP servers, and inference defaults to server defaults? Credentials remain untouched."
                confirmLabel={resetPending() ? "Resetting..." : "Reset configuration"}
                confirmAriaLabel="Confirm reset configuration"
                cancelAriaLabel="Cancel reset configuration"
                pending={resetPending()}
                onConfirm={() => void resetConfig()}
                onCancel={() => {
                  if (!resetPending()) {
                    setResetConfirmationOpen(false);
                    setResetError(undefined);
                  }
                }}
              />
            </Show>
            <Show when={resetError()}>
              {(message) => <InlineAlert tone="danger">{message()}</InlineAlert>}
            </Show>
          </div>
        </Show>
      </header>
      <Show when={resetSucceeded()}>
        <InlineAlert tone="success">Configuration reset.</InlineAlert>
      </Show>
      <Tabs
        id="config-sections"
        label="Configuration sections"
        tabs={visibleTabs()}
        selected={visibleTab()}
        onSelect={(tab) => {
          setActiveTab(tab);
          const href = CONFIG_TABS.find((item) => item.id === tab)?.href;
          if (href) navigate(href);
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
            <ProvidersSection watchedConfig={watchedConfig} resourceBaselines={resourceBaselines}>
              <Show when={routeTab() === "providers"}>{props.children}</Show>
            </ProvidersSection>
          </Match>
          <Match when={visibleTab() === "mcp"}>
            <McpServersSection watchedConfig={watchedConfig} resourceBaselines={resourceBaselines}>
              <Show when={routeTab() === "mcp"}>{props.children}</Show>
            </McpServersSection>
          </Match>
          <Match when={visibleTab() === "defaults"}>
            <InferenceDefaultsSection watchedConfig={watchedConfig} />
          </Match>
          <Match when={visibleTab() === "workspaceBindings"}>
            <section aria-labelledby="workspace-bindings-title">
              <h2 id="workspace-bindings-title">Workspace bindings</h2>
              <For each={Object.keys(workspaceBindings())} fallback={<p>No workspace bindings configured.</p>}>
                {(key) => (
                  <article>
                    <h3>{key}</h3>
                    <p>{workspaceBindings()[key]?.machineId} · {workspaceBindings()[key]?.endpoint}</p>
                    <AuthStatusPanel target={{ kind: "workspaceBinding", key }} />
                  </article>
                )}
              </For>
            </section>
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
