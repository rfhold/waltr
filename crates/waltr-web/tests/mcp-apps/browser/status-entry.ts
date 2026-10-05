import { startStatusApp } from "./status";
void startStatusApp().catch(() => {
  document.getElementById("error")!.textContent = "Unable to connect to the app host.";
});
