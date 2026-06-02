import { render } from "solid-js/web";

import { App } from "./app/App";
import "./styles/index.css";

const root = document.getElementById("root");

if (!(root instanceof HTMLElement)) {
  throw new Error("missing root element");
}

render(() => <App />, root);
