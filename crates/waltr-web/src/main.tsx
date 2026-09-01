import { render } from "solid-js/web";

import { App } from "./app/App";
import { initializeTelemetry } from "./telemetry";
import "./styles/index.css";

initializeTelemetry();

const root = document.getElementById("root");

if (!(root instanceof HTMLElement)) {
  throw new Error("missing root element");
}

render(() => <App />, root);
