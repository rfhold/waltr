import { afterEach, describe, expect, test, vi } from "vitest";
import { acceptNativeAppsBootstrap, appsAvailable, configuredContainerAddress } from "../../src/mcp-apps/policy";

afterEach(() => { acceptNativeAppsBootstrap(undefined); vi.unstubAllGlobals(); vi.unstubAllEnvs(); });

function packagedWindow(frame = false) {
  const window = { location: { protocol: "tauri:", host: "localhost", origin: "tauri://localhost" }, top: undefined as unknown };
  window.top = frame ? {} : window;
  vi.stubGlobal("window", window);
}

describe.skipIf(__WALTR_BUILD_PLATFORM__ !== "tauri")("native Apps startup capability", () => {
  test("default native ignores ambient globals and web container configuration", () => {
    packagedWindow();
    vi.stubGlobal("__WALTR_NATIVE_APPS_TEST__", true);
    vi.stubEnv("VITE_WALTR_MCP_APP_CONTAINER_URL", "http://127.0.0.1:8123/container.html");
    acceptNativeAppsBootstrap(undefined);
    expect(appsAvailable()).toBe(false);
    expect(() => configuredContainerAddress()).toThrow();
  });

  test("only packaged native startup can enable the exact loopback asset address", () => {
    packagedWindow();
    acceptNativeAppsBootstrap({ containerUrl: "http://127.0.0.1:8123/container.html" });
    expect(appsAvailable()).toBe(true);
    expect(configuredContainerAddress().href).toBe("http://127.0.0.1:8123/container.html");
    acceptNativeAppsBootstrap(undefined);
    expect(appsAvailable()).toBe(false);
  });

  test("frames and non-asset startup values cannot enable Apps", () => {
    packagedWindow(true);
    acceptNativeAppsBootstrap({ containerUrl: "http://127.0.0.1:8123/container.html" });
    expect(appsAvailable()).toBe(false);
    packagedWindow();
    for (const containerUrl of ["https://remote.example/container.html", "http://localhost:8123/container.html", "http://127.0.0.1:8123/mcp", "not a URL", "http://user:secret@127.0.0.1:8123/container.html", "http://127.0.0.1:8123/container.html?endpoint=private"]) {
      acceptNativeAppsBootstrap({ containerUrl });
      expect(appsAvailable()).toBe(false);
    }
  });

  test("startup capability cannot be replaced by another address while active", () => {
    packagedWindow();
    acceptNativeAppsBootstrap({ containerUrl: "http://127.0.0.1:8123/container.html" });
    acceptNativeAppsBootstrap({ containerUrl: "http://127.0.0.1:9999/container.html" });
    expect(configuredContainerAddress().href).toBe("http://127.0.0.1:8123/container.html");
  });
});
