import { check, sleep } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { waitForMissingText, waitForPathname, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

const PRIMARY_PROVIDER = "k6-openai";
const PRIMARY_MODEL = "gpt-5-nano";
const SECONDARY_PROVIDER = "k6-anthropic";
const SECONDARY_MODEL = "claude";

export default async function () {
  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await page.goto(`${baseUrl}/sessions`, { waitUntil: "domcontentloaded" });
    await waitForPathname(page, "/sessions");
    await waitForText(page, "#sessions-title", "Sessions");
    await waitForText(page, "#sessions-list-title", "Recent sessions");
    await screenshot(page, "sessions-list-empty");

    await page.locator(".sessions__header button").click();
    await waitForText(page, '[aria-label="Session detail"]', "");
    await waitForText(page, '[aria-label="Session status summary"]', `Provider ${PRIMARY_PROVIDER}/${PRIMARY_MODEL}`);
    await waitForText(page, '[aria-label="Session status summary"]', "MCP git");
    const thinkingSelectEnabled = await waitForSelectEnabled(page, '[aria-label="Session thinking level"]');

    const createdPathname = await page.evaluate(() => window.location.pathname);
    const sessionId = decodeURIComponent(createdPathname.replace(/^\/sessions\//, ""));
    const detailTimeline = await waitForText(page, '[aria-label="Session timeline"]', "No messages yet.");
    await screenshot(page, "sessions-detail-thinking-default");

    await setSelectValue(page, '[aria-label="Session thinking level"]', "high");
    const thinkingRuntime = await waitForText(page, '[aria-label="Session status summary"]', `${PRIMARY_PROVIDER}/${PRIMARY_MODEL} (high)`);
    const thinkingSelectValue = await waitForSelectValue(page, '[aria-label="Session thinking level"]', "high");
    await screenshot(page, "sessions-detail-thinking-high");

    await clickButtonByLabel(page, "Edit provider/model");
    await waitForText(page, '[role="dialog"]', "Edit provider/model");
    const providerDialogThinking = await waitForLabeledSelectValue(page, "Thinking level", "high");
    await screenshot(page, "sessions-provider-runtime-thinking-dialog");
    await clickRuntimeProviderOption(page, SECONDARY_PROVIDER);
    await clickButtonByText(page, "Save runtime");
    const providerRuntime = await waitForText(page, '[aria-label="Session status summary"]', `Provider ${SECONDARY_PROVIDER}/${SECONDARY_MODEL}`);

    await clickButtonByLabel(page, "Edit MCP");
    await waitForText(page, '[role="dialog"]', "Edit MCP servers");
    await toggleMcpRuntime(page, "files");
    await clickButtonByText(page, "Save MCP");
    const mcpRuntime = await waitForText(page, '[aria-label="Session status summary"]', "files");
    await screenshot(page, "sessions-detail-created");

    await page.locator('.shell__nav a[href="/sessions"]').click();
    await waitForPathname(page, "/sessions");
    const listedSession = await waitForText(page, '[aria-label="Session list"]', sessionId);
    await screenshot(page, "sessions-list-with-session");

    await page.locator(`button[aria-label="Open session ${sessionId}"]`).click();
    await waitForPathname(page, `/sessions/${encodeURIComponent(sessionId)}`);
    const reopenedDetail = await waitForText(page, '[aria-label="Session status summary"]', `${SECONDARY_PROVIDER}/${SECONDARY_MODEL}`);

    await clickButtonByLabel(page, `Delete session ${sessionId}`);
    await screenshot(page, "sessions-delete-confirmation");
    await clickButtonByLabel(page, `Confirm delete session ${sessionId}`);
    await waitForPathname(page, "/sessions");
    const listAfterDelete = await waitForMissingText(page, ".sessions__list-panel", sessionId);

    check(null, {
      "session detail route reached": () => createdPathname.startsWith("/sessions/") && sessionId.length > 0,
      "session detail shows empty timeline": () => includesText(detailTimeline, "No messages yet."),
      "session thinking select is enabled": () => thinkingSelectEnabled,
      "session thinking runtime updates": () => includesText(thinkingRuntime, `${PRIMARY_PROVIDER}/${PRIMARY_MODEL} (high)`),
      "session thinking select persists update": () => thinkingSelectValue === "high",
      "provider dialog shows selected thinking": () => providerDialogThinking === "high",
      "session detail edits provider runtime": () => includesText(providerRuntime, `${SECONDARY_PROVIDER}/${SECONDARY_MODEL}`),
      "session detail edits mcp runtime": () => includesText(mcpRuntime, "files") && includesText(mcpRuntime, "git"),
      "session list contains created session": () => includesText(listedSession, sessionId),
      "session selection reopens detail": () => includesText(reopenedDetail, `${SECONDARY_PROVIDER}/${SECONDARY_MODEL}`),
      "session list omits deleted session": () => !includesText(listAfterDelete, sessionId),
    });
  });
}

async function waitForSelectEnabled(page, selector) {
  const deadline = Date.now() + 10_000;
  let disabled = true;

  while (Date.now() < deadline) {
    disabled = await page.evaluate((selectSelector) => {
      const select = document.querySelector(selectSelector);
      if (!(select instanceof HTMLSelectElement)) {
        throw new Error(`Select not found: ${selectSelector}`);
      }

      return select.disabled;
    }, selector);

    if (!disabled) {
      return true;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for enabled select ${selector}`);
}

async function waitForSelectValue(page, selector, expectedValue) {
  const deadline = Date.now() + 10_000;
  let value = "";

  while (Date.now() < deadline) {
    value = await page.evaluate((selectSelector) => {
      const select = document.querySelector(selectSelector);
      if (!(select instanceof HTMLSelectElement)) {
        throw new Error(`Select not found: ${selectSelector}`);
      }

      return select.value;
    }, selector);

    if (value === expectedValue) {
      return value;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for ${selector} to equal ${expectedValue}; saw ${value}`);
}

async function waitForLabeledSelectValue(page, labelText, expectedValue) {
  const deadline = Date.now() + 10_000;
  let value = "";

  while (Date.now() < deadline) {
    value = await page.evaluate((labelText) => {
      const label = Array.from(document.querySelectorAll("label")).find((candidate) =>
        candidate.textContent?.includes(labelText),
      );
      const select = label?.querySelector("select");
      if (!(select instanceof HTMLSelectElement)) {
        throw new Error(`Labeled select not found: ${labelText}`);
      }

      return select.value;
    }, labelText);

    if (value === expectedValue) {
      return value;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for ${labelText} to equal ${expectedValue}; saw ${value}`);
}

async function toggleMcpRuntime(page, server) {
  await page.evaluate((serverName) => {
    const labels = Array.from(document.querySelectorAll(".sessions__runtime-option"));
    const label = labels.find((candidate) => candidate.textContent?.includes(serverName));
    const input = label?.querySelector('input[type="checkbox"]');

    if (!(input instanceof HTMLInputElement)) {
      throw new Error(`Runtime MCP checkbox not found: ${serverName}`);
    }

    input.click();
  }, server);
}

async function setSelectValue(page, selector, value) {
  await page.evaluate(({ selector, value }) => {
    const select = document.querySelector(selector);

    if (!(select instanceof HTMLSelectElement)) {
      throw new Error(`Select not found: ${selector}`);
    }

    select.value = value;
    select.dispatchEvent(new Event("change", { bubbles: true }));
  }, { selector, value });
}

async function clickButtonByLabel(page, label) {
  await page.evaluate((buttonLabel) => {
    const button = document.querySelector(`button[aria-label="${CSS.escape(buttonLabel)}"]`);

    if (!(button instanceof HTMLButtonElement)) {
      throw new Error(`Button not found: ${buttonLabel}`);
    }

    button.scrollIntoView({ block: "center", inline: "center" });
    button.click();
  }, label);
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

async function clickRuntimeProviderOption(page, providerKey) {
  await page.evaluate((key) => {
    const options = document.querySelector('[aria-label="Provider options"]');
    if (!(options instanceof HTMLElement)) {
      throw new Error("Provider options not found");
    }

    const button = Array.from(options.querySelectorAll("button")).find((candidate) =>
      candidate.querySelector("span")?.textContent?.trim() === key,
    );
    if (!(button instanceof HTMLButtonElement)) {
      throw new Error(`Provider option not found: ${key}`);
    }

    button.scrollIntoView({ block: "center", inline: "center" });
    button.click();
  }, providerKey);
}
