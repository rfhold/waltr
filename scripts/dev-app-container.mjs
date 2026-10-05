import { isIP } from "node:net";

function port(value, name) {
  if (!/^[1-9][0-9]*$/.test(value) || Number(value) > 65535) {
    throw new Error(`${name} must be an explicit port from 1 to 65535`);
  }
  return Number(value);
}

function exactOrigin(value, name) {
  const url = new URL(value);
  if (!["http:", "https:"].includes(url.protocol) || url.origin !== value) {
    throw new Error(`${name} must be an exact HTTP(S) origin without credentials or a path`);
  }
  return url;
}

function containerUrl(value, hostOrigins, apiOrigin) {
  const url = new URL(value);
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password
    || url.search || url.hash || hostOrigins.includes(url.origin) || url.origin === apiOrigin
    || (url.protocol === "http:" && !["localhost", "127.0.0.1", "[::1]"].includes(url.hostname))) {
    throw new Error("VITE_WALTR_MCP_APP_CONTAINER_URL requires a separate HTTPS or loopback HTTP asset origin");
  }
  return url.href;
}

export function devAppContainerConfig(env = process.env, apiOrigin = "http://127.0.0.1:14333") {
  const webPort = port(env.WALTR_WEB_DEV_PORT ?? "5173", "WALTR_WEB_DEV_PORT");
  const hostOrigins = [`http://127.0.0.1:${webPort}`, `http://localhost:${webPort}`]
    .map((value) => new URL(value).origin);
  const viteArgs = ["--host", "127.0.0.1", "--port", String(webPort), "--strictPort"];

  if (env.VITE_WALTR_MCP_APP_CONTAINER_URL !== undefined) {
    return {
      viteArgs,
      webEnv: { VITE_WALTR_MCP_APP_CONTAINER_URL: containerUrl(env.VITE_WALTR_MCP_APP_CONTAINER_URL, hostOrigins, apiOrigin) },
      containerEnv: null,
    };
  }

  const addr = env.WALTR_APP_CONTAINER_ADDR ?? "127.0.0.1:14334";
  const match = /^(127\.0\.0\.1|\[::1\]):([0-9]+)$/.exec(addr);
  if (!match || !isIP(match[1].replace(/^\[|\]$/g, ""))) {
    throw new Error("WALTR_APP_CONTAINER_ADDR must be 127.0.0.1:PORT or [::1]:PORT for managed development");
  }
  const bindPort = port(match[2], "WALTR_APP_CONTAINER_ADDR port");
  const origin = env.WALTR_APP_CONTAINER_ORIGIN ?? new URL(`http://${addr}`).origin;
  const publicUrl = exactOrigin(origin, "WALTR_APP_CONTAINER_ORIGIN");
  if (["localhost", "127.0.0.1", "[::1]"].includes(publicUrl.hostname)
    && (Number(publicUrl.port || (publicUrl.protocol === "https:" ? 443 : 80)) !== bindPort
      || (publicUrl.hostname !== "localhost" && publicUrl.hostname !== match[1]))) {
    throw new Error("WALTR_APP_CONTAINER_ORIGIN must match the loopback listener address and port");
  }
  const url = containerUrl(`${origin}/container.html`, hostOrigins, apiOrigin);
  const parents = [...new Set([...hostOrigins, ...(env.WALTR_APP_CONTAINER_HOST_ORIGINS?.split(",") ?? [])])];
  for (const parent of parents) {
    exactOrigin(parent, "WALTR_APP_CONTAINER_HOST_ORIGINS");
    if (parent === origin) throw new Error("Apps container must have a separate origin from every parent");
  }
  if (parents.length > 20) throw new Error("Apps supports at most 20 exact host origins");
  return {
    viteArgs,
    webEnv: { VITE_WALTR_MCP_APP_CONTAINER_URL: url },
    containerEnv: {
      WALTR_APP_CONTAINER_ADDR: addr,
      WALTR_APP_CONTAINER_ORIGIN: origin,
      WALTR_APP_CONTAINER_HOST_ORIGINS: parents.join(","),
    },
  };
}
