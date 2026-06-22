import { check } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });

    await waitForText(page, "#landing-title", "Waltr is ready.");
    const systemStatus = await waitForText(
      page,
      '[aria-label="Global system status"]',
      "System ready",
    );
    const statusWatch = await waitForText(
      page,
      '[aria-label="Global system status"]',
      "Status watch received",
    );
    await screenshot(page, "status-landing-ready");

    check({ systemStatus, statusWatch }, {
      "landing receives ready status": ({ systemStatus, statusWatch }) =>
        includesText(systemStatus, "System ready") &&
        includesText(statusWatch, "Status watch received"),
    });
  });
}
