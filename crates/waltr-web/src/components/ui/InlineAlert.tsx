import type { JSX } from "solid-js";

type InlineAlertTone = "info" | "success" | "warning" | "danger";

type InlineAlertProps = {
  tone?: InlineAlertTone;
  live?: "polite" | "assertive" | "off";
  children: JSX.Element;
};

export function InlineAlert(props: InlineAlertProps) {
  const tone = () => props.tone ?? "info";
  const live = () => props.live ?? (tone() === "danger" ? "assertive" : "polite");
  const role = () => (tone() === "danger" || live() === "assertive" ? "alert" : "status");

  return (
    <p class={`ui-alert ui-alert--${tone()}`} role={role()} aria-live={live()}>
      {props.children}
    </p>
  );
}
