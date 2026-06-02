import { A } from "@solidjs/router";
import type { JSX } from "solid-js";
import { For } from "solid-js";

export type TabItem<T extends string> = {
  id: T;
  label: string;
  href: string;
};

type TabsProps<T extends string> = {
  id: string;
  label: string;
  tabs: readonly TabItem<T>[];
  selected: T;
  onSelect: (id: T) => void;
};

type TabPanelProps = {
  id: string;
  tabId: string;
  children: JSX.Element;
};

export function Tabs<T extends string>(props: TabsProps<T>) {
  const selectByOffset = (current: T, offset: number) => {
    const currentIndex = props.tabs.findIndex((tab) => tab.id === current);
    const nextIndex = (currentIndex + offset + props.tabs.length) % props.tabs.length;
    props.onSelect(props.tabs[nextIndex].id);
  };

  const handleKeyDown = (tab: T): JSX.EventHandler<HTMLAnchorElement, KeyboardEvent> => (event) => {
    if (event.key === "ArrowRight") {
      event.preventDefault();
      selectByOffset(tab, 1);
    } else if (event.key === "ArrowLeft") {
      event.preventDefault();
      selectByOffset(tab, -1);
    } else if (event.key === "Home") {
      event.preventDefault();
      props.onSelect(props.tabs[0].id);
    } else if (event.key === "End") {
      event.preventDefault();
      props.onSelect(props.tabs[props.tabs.length - 1].id);
    }
  };

  return (
    <div class="ui-tabs" role="tablist" aria-label={props.label}>
      <For each={props.tabs}>
        {(tab) => {
          const selected = () => props.selected === tab.id;
          const tabId = () => `${props.id}-${tab.id}-tab`;
          const panelId = () => `${props.id}-${tab.id}-panel`;

          return (
            <A
              id={tabId()}
              href={tab.href}
              class="ui-tabs__tab"
              classList={{ "ui-tabs__tab--active": selected() }}
              role="tab"
              aria-selected={selected()}
              aria-controls={panelId()}
              tabIndex={selected() ? 0 : -1}
              onClick={() => props.onSelect(tab.id)}
              onKeyDown={handleKeyDown(tab.id)}
            >
              {tab.label}
            </A>
          );
        }}
      </For>
    </div>
  );
}

export function TabPanel(props: TabPanelProps) {
  return (
    <section id={props.id} class="ui-tabs__panel" role="tabpanel" aria-labelledby={props.tabId} tabIndex={0}>
      {props.children}
    </section>
  );
}
