export function telemetryEnabledForBuild(command: string, mode: string): boolean {
  return command === "build" && (mode === "production" || mode === "tauri");
}
