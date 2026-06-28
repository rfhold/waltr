import { check } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(`${baseUrl}/__fixtures__/sessions/permission`, { waitUntil: "domcontentloaded" });
    const permissionPrompt = await waitForText(page, '[aria-label="Permission prompt"]', "Permission required");
    await waitForText(page, '[aria-label="Permission prompt"]', "Allow once");
    await waitForText(page, '[aria-label="Permission prompt"]', "Deny with interrupt");
    await screenshot(page, "sessions-permission-prompt");

    await page.goto(`${baseUrl}/__fixtures__/sessions/question`, { waitUntil: "domcontentloaded" });
    const questionPrompt = await waitForText(page, '[aria-label="Question prompt"]', "Question");
    await waitForText(page, '[aria-label="Question prompt"]', "Direction");
    await waitForText(page, '[aria-label="Question prompt"]', "Resume session");
    await screenshot(page, "sessions-question-prompt");

    check(null, {
      "permission prompt fixture rendered": () => includesText(permissionPrompt, "Permission required") && includesText(permissionPrompt, "Allow once"),
      "question prompt fixture rendered": () => includesText(questionPrompt, "Direction") && includesText(questionPrompt, "Resume session"),
    });
  });
}
