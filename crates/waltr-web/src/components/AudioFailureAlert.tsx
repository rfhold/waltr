import { Show } from "solid-js";
import { useAppState } from "../app/context";
import type { AudioFailure } from "../platform/client";
import { InlineAlert } from "./ui/InlineAlert";

const messages: Record<AudioFailure["category"], string> = {
  remote_failed: "The audio Routine failed on the server.",
  permission: "Audio permission is unavailable or was denied.",
  login_required: "Sign in to use audio.",
  audio_failed: "The audio operation could not be completed.",
};

export function AudioFailureAlert() {
  const app = useAppState();
  return <Show when={app.audioFailure?.()}>{(failure) =>
    <InlineAlert tone="danger">
      <span>{messages[failure().category]} Diagnostic ID: {failure().diagnosticId}.</span>
      <button type="button" onClick={() => { void app.dismissAudioFailure?.(failure().diagnosticId); }}>Dismiss audio error</button>
    </InlineAlert>
  }</Show>;
}
