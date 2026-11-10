import http from "k6/http";
import { check } from "k6";

import { baseUrl, options as browserOptions, withPage } from "../support/browser.js";

export const options = browserOptions;

export default async function () {
  const endpoint = `${baseUrl.replace(/\/$/, "")}/health`;
  const httpResponse = http.get(endpoint, { redirects: 0 });
  check(httpResponse, {
    "k6 HTTP trusts the generated CA": (response) => response.status === 200,
    "TLS gateway negotiates HTTP/2": (response) => response.proto === "HTTP/2.0",
  });
  const session = http.get(`${baseUrl.replace(/\/$/, "")}/oidc/session`, { redirects: 0 });
  check(session, {
    "anonymous access matches the configured authentication mode": (response) =>
      response.status === 200 && response.json("authenticated") === (__ENV.WALTR_WEB_ACCESS_AUTH === "disabled"),
  });

  await withPage(async (page) => {
    const browserResponse = await page.goto(endpoint, {
      waitUntil: "domcontentloaded",
    });
    check(browserResponse, {
      "Chromium NSS trusts the generated CA": (response) => response?.status() === 200,
    });
    const protocol = await page.evaluate(() => performance.getEntriesByType("navigation")[0]?.nextHopProtocol);
    check(protocol, {
      "Chromium negotiates HTTP/2": (value) => value === "h2",
    });
    console.log(`TLS gateway protocols: k6=${httpResponse.proto}, Chromium=${protocol}`);
  }, { authenticate: false });
}
