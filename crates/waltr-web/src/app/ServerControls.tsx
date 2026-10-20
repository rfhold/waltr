import { Show } from "solid-js";
import { useServerSelection } from "./serverSelection";

export function ServerControls(props: { failed?: boolean }) {
  const selection = useServerSelection();
  return (
    <Show when={selection}>
      <div class="access__progress-actions">
        <Show when={props.failed}>
          <button class="ui-button" type="button" onClick={() => selection?.retry()}>Retry connection</button>
        </Show>
        <button class="ui-button" type="button" onClick={() => selection?.switchServer()}>Switch server</button>
      </div>
    </Show>
  );
}
