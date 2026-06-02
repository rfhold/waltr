import { check } from "k6";

import { baseUrl, directConfigUrl, options as browserOptions, withPage } from "../support/browser.js";
import { waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(directConfigUrl, { waitUntil: "domcontentloaded" });
    const directConfigLanding = await waitForText(page, "#landing-title", "Waltr is ready.");

    check(directConfigLanding, {
      "direct config navigation waits for readiness": (text) => includesText(text, "Waltr is ready."),
    });

    await page.locator('a[href="/config"]').click();
    await waitForText(page, "#config-title", "Settings");
    const configSection = await waitForText(page, '[aria-label="Provider actions"]', "Create");

    check(configSection, {
      "config section renders once loaded": (text) => includesText(text, "Create"),
    });
  });
}
