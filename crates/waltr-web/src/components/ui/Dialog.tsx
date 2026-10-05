import type { JSX } from "solid-js";
import { createUniqueId, onCleanup, onMount } from "solid-js";

const focusableSelector = [
  "a[href]",
  "button:not([disabled])",
  "textarea:not([disabled])",
  "input:not([disabled])",
  "select:not([disabled])",
  "iframe",
  "[tabindex]:not([tabindex='-1'])",
].join(",");

type DialogProps = {
  title: string;
  label?: string;
  onClose: () => void;
  class?: string;
  style?: JSX.CSSProperties;
  children: JSX.Element;
};

export function Dialog(props: DialogProps) {
  const titleId = createUniqueId();
  let dialogRef: HTMLDivElement | undefined;
  const priorFocus = document.activeElement instanceof HTMLElement ? document.activeElement : undefined;

  const focusableElements = () => {
    if (!dialogRef) {
      return [];
    }

    return Array.from(dialogRef.querySelectorAll<HTMLElement>(focusableSelector)).filter(
      (element) => element.offsetParent !== null || element === document.activeElement,
    );
  };

  const handleKeyDown: JSX.EventHandler<HTMLDivElement, KeyboardEvent> = (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      props.onClose();
      return;
    }

    if (event.key !== "Tab") {
      return;
    }

    const elements = focusableElements();

    if (elements.length === 0) {
      event.preventDefault();
      dialogRef?.focus();
      return;
    }

    const first = elements[0];
    const last = elements[elements.length - 1];

    if (event.shiftKey && document.activeElement === first) {
      event.preventDefault();
      last.focus();
    } else if (!event.shiftKey && document.activeElement === last) {
      event.preventDefault();
      first.focus();
    }
  };

  onMount(() => {
    const first = focusableElements()[0];
    (first ?? dialogRef)?.focus();
  });

  onCleanup(() => {
    priorFocus?.focus();
  });

  return (
    <div class="ui-dialog-backdrop">
      <div
        ref={dialogRef}
        class={`ui-dialog ${props.class ?? ""}`.trim()}
        style={props.style}
        role="dialog"
        aria-modal="true"
        aria-label={props.label ?? `${props.title} dialog`}
        tabIndex={-1}
        onKeyDown={handleKeyDown}
      >
        <h2 id={titleId} class="ui-dialog__title">
          {props.title}
        </h2>
        {props.children}
      </div>
    </div>
  );
}
