import { check } from "k6";

import { baseUrl, options as browserOptions, screenshotMatrix, withPage } from "../support/browser.js";
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
    await page.locator(".splash").waitFor({ state: "detached" });
    await screenshotMatrix(page, "status-landing-ready");
    await screenshotMatrix(page, "status-navigation", {
      prepareViewport: async (page, { viewportName }) => {
        if (viewportName === "phone") {
          await page.locator('button[aria-label="Open navigation menu"]').click();
          await page.evaluate(() => new Promise((resolve) => setTimeout(resolve, 250)));
        }
        if (viewportName === "compact") {
          await page.locator('button[aria-label="Close navigation menu"]').waitFor();
        }
      },
    });

    check({ systemStatus, statusWatch }, {
      "landing receives ready status": ({ systemStatus, statusWatch }) =>
        includesText(systemStatus, "System ready") &&
        includesText(statusWatch, "Status watch received"),
    });
  });
}
