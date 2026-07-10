import { check, sleep } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { typeAndKeepFocus, waitForPathname, waitForText } from "../support/checks.js";
import { completeDexLoginViaHttp } from "../support/oauth-login.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(`${baseUrl}/sessions`, { waitUntil: "domcontentloaded" });
    await waitForPathname(page, "/sessions");
    await page.locator(".sessions__header button").click();
    await waitForText(page, '[aria-label="Session status summary"]', "MCP protected");

    await typeAndKeepFocus(page, '[aria-label="Session message"]', "trigger protected mcp oauth");
    await page.locator('[aria-label="Send session message"] button[type="submit"]').click();
    const oauthDialog = await waitForText(page, '[role="dialog"]', "OAuth required");
    await waitForText(page, '[role="dialog"]', "MCP server protected");
    await screenshot(page, "sessions-mcp-oauth-required");

    await clickButtonByText(page, "Begin OAuth");
    await waitForText(page, '[role="dialog"]', "Open authorization URL");
    const authUrl = await page.evaluate(() => {
      const link = document.querySelector(".oauth-dialog__open");
      if (!(link instanceof HTMLAnchorElement)) {
        throw new Error("OAuth authorization link not found");
      }

      return link.href;
    });

    const redirectUrl = completeDexLoginViaHttp({
      authUrl,
      username: __ENV.WALTR_OAUTH_USERNAME || "waltr@example.test",
      password: __ENV.WALTR_OAUTH_PASSWORD || "password",
    });

    await waitForText(page, '[role="dialog"]', "Final redirect URL or code");
    await page.locator(".oauth-dialog__field textarea").type(redirectUrl);
    await clickButtonByText(page, "Complete OAuth");
    await waitForMissingElement(page, '[role="dialog"]');

    await clearTextarea(page, '[aria-label="Session message"]');
    await typeAndKeepFocus(page, '[aria-label="Session message"]', "@status");
    const suggestions = await waitForText(page, '[aria-label="Resource suggestions"]', "status_resource");
    await screenshot(page, "sessions-mcp-oauth-authorized-resource-search");

    check(null, {
      "mcp oauth requirement is shown": () => includesText(oauthDialog, "MCP server protected"),
      "dex login returned authorization code": () => redirectUrl.includes("code=") && redirectUrl.includes("state="),
      "protected mcp resources are available after oauth": () => includesText(suggestions, "status_resource"),
    });
  });
}

async function waitForMissingElement(page, selector) {
  const deadline = Date.now() + 10_000;
  let count = 1;

  while (Date.now() < deadline) {
    count = await page.evaluate((elementSelector) => document.querySelectorAll(elementSelector).length, selector);
    if (count === 0) {
      return;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for ${selector} to disappear; saw ${count}`);
}

async function clearTextarea(page, selector) {
  await page.evaluate((inputSelector) => {
    const input = document.querySelector(inputSelector);
    if (!(input instanceof HTMLTextAreaElement)) {
      throw new Error(`Textarea not found: ${inputSelector}`);
    }

    input.value = "";
    input.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "deleteContentBackward" }));
  }, selector);
}

async function clickButtonByText(page, text) {
  await page.evaluate((buttonText) => {
    const button = Array.from(document.querySelectorAll("button")).find((candidate) =>
      candidate.textContent?.includes(buttonText),
    );

    if (!(button instanceof HTMLButtonElement)) {
      throw new Error(`Button not found: ${buttonText}`);
    }

    button.scrollIntoView({ block: "center", inline: "center" });
    button.click();
  }, text);
}
