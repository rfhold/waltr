import { check } from "k6";

import { directConfigUrl, options as browserOptions, withPage } from "../support/browser.js";
import { waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(directConfigUrl, { waitUntil: "domcontentloaded" });
    const directConfigTitle = await waitForText(page, "#config-title", "Settings");
    const configSection = await waitForText(page, '[aria-label="Provider actions"]', "Create");

    check({ directConfigTitle, configSection }, {
      "direct config navigation renders once ready": ({ directConfigTitle, configSection }) =>
        includesText(directConfigTitle, "Settings") && includesText(configSection, "Create"),
    });
  });
}
