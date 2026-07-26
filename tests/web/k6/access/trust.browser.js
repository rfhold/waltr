import http from "k6/http";
import { check } from "k6";

import { baseUrl, options as browserOptions, withPage } from "../support/browser.js";

export const options = browserOptions;

export default async function () {
  const endpoint = `${baseUrl.replace(/\/$/, "")}/health`;
  const httpResponse = http.get(endpoint, { redirects: 0 });
  check(httpResponse, {
    "k6 HTTP trusts the generated CA": (response) => response.status === 200,
  });

  await withPage(async (page) => {
    const browserResponse = await page.goto(endpoint, {
      waitUntil: "domcontentloaded",
    });
    check(browserResponse, {
      "Chromium NSS trusts the generated CA": (response) => response?.status() === 200,
    });
  }, { authenticate: false });
}
