import type { JSX } from "solid-js";
import { Show, createMemo } from "solid-js";

type FieldRenderProps = {
  id: string;
  describedBy: string | undefined;
  invalid: boolean;
};

type FieldProps = {
  id: string;
  label: string;
  hint?: JSX.Element;
  error?: JSX.Element;
  required?: boolean;
  children: (props: FieldRenderProps) => JSX.Element;
};

export function Field(props: FieldProps) {
  const hintId = () => `${props.id}-hint`;
  const errorId = () => `${props.id}-error`;
  const invalid = createMemo(() => props.error !== undefined && props.error !== null);
  const describedBy = createMemo(() => {
    const ids = [];

    if (props.hint !== undefined && props.hint !== null) {
      ids.push(hintId());
    }

    if (invalid()) {
      ids.push(errorId());
    }

    return ids.length > 0 ? ids.join(" ") : undefined;
  });

  return (
    <div class="ui-field">
      <div class="ui-field__label-row">
        <label class="ui-field__label" for={props.id}>
          {props.label}
        </label>
        <Show when={props.required}>
          <span class="ui-field__required" aria-hidden="true">
            required
          </span>
        </Show>
      </div>
      {props.children({ id: props.id, describedBy: describedBy(), invalid: invalid() })}
      <Show when={props.hint !== undefined && props.hint !== null}>
        <p id={hintId()} class="ui-field__hint">
          {props.hint}
        </p>
      </Show>
      <Show when={invalid()}>
        <p id={errorId()} class="ui-field__error" role="alert">
          {props.error}
        </p>
      </Show>
    </div>
  );
}
