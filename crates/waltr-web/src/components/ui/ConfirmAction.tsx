import { onMount, type JSX } from "solid-js";

type ConfirmActionProps = {
  label: string;
  prompt: JSX.Element;
  confirmLabel: string;
  confirmAriaLabel?: string;
  cancelLabel?: string;
  cancelAriaLabel?: string;
  focusConfirmOnMount?: boolean;
  pending?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
};

export function ConfirmAction(props: ConfirmActionProps) {
  let confirmButton!: HTMLButtonElement;

  onMount(() => {
    if (props.focusConfirmOnMount) confirmButton.focus();
  });

  return (
    <div class="ui-confirm" role="group" aria-label={props.label}>
      <span class="ui-confirm__prompt">{props.prompt}</span>
      <button
        class="ui-button ui-button--danger"
        type="button"
        ref={confirmButton}
        aria-label={props.confirmAriaLabel}
        disabled={props.pending}
        onClick={props.onConfirm}
      >
        {props.confirmLabel}
      </button>
      <button
        class="ui-button"
        type="button"
        aria-label={props.cancelAriaLabel}
        disabled={props.pending}
        onClick={props.onCancel}
      >
        {props.cancelLabel ?? "Cancel"}
      </button>
    </div>
  );
}
