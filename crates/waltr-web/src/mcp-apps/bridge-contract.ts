export const MAX_HTML_BYTES = 1024 * 1024;
export const MAX_MESSAGE_BYTES = 2 * 1024 * 1024;
export const DENIED_PERMISSIONS = "camera 'none'; microphone 'none'; geolocation 'none'; clipboard-write 'none'; fullscreen 'none'; payment 'none'; usb 'none'";

export function boundedMessage(value: unknown): value is Record<string, unknown> {
  try {
    return !!value && typeof value === "object" && !Array.isArray(value)
      && new TextEncoder().encode(JSON.stringify(value)).length <= MAX_MESSAGE_BYTES;
  } catch { return false; }
}
