import type { JSX } from "solid-js";

type ActionListProps = {
  label: string;
  class?: string;
  children: JSX.Element;
};

type ActionRowProps = {
  class?: string;
  selected?: boolean;
  children: JSX.Element;
};

export function ActionList(props: ActionListProps) {
  return (
    <ul class={`ui-action-list ${props.class ?? ""}`.trim()} aria-label={props.label}>
      {props.children}
    </ul>
  );
}

export function ActionRow(props: ActionRowProps) {
  return (
    <li
      class={`ui-action-row ${props.class ?? ""}`.trim()}
      classList={{ "ui-action-row--selected": props.selected }}
      aria-current={props.selected ? "true" : undefined}
    >
      {props.children}
    </li>
  );
}
