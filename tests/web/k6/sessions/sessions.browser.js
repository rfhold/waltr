import { check } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { typeAndKeepFocus, waitForMissingText, waitForPathname, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await seedRuntimeConfig(page);

    await page.locator('.shell__nav a[href="/sessions"]').click();
    await waitForText(page, "#sessions-title", "Sessions");
    await waitForText(page, "#sessions-list-title", "Recent sessions");
    await screenshot(page, "sessions-list-empty");

    await page.locator(".sessions__header button").click();
    await waitForText(page, '[aria-label="Session detail"]', "");
    await waitForText(page, '[aria-label="Session status summary"]', "Provider openai/gpt4o");
    await waitForText(page, '[aria-label="Session status summary"]', "MCP git");

    const createdPathname = await page.evaluate(() => window.location.pathname);
    const sessionId = decodeURIComponent(createdPathname.replace(/^\/sessions\//, ""));
    const detailStatus = await waitForText(page, '[aria-label="Session status summary"]', "idle");

    await clickButtonByText(page, "Edit provider/model");
    await waitForText(page, '[role="dialog"]', "Edit provider/model");
    await clickButtonByText(page, "anthropic");
    await clickButtonByText(page, "Save runtime");
    const providerRuntime = await waitForText(page, '[aria-label="Session status summary"]', "Provider anthropic/claude");

    await clickButtonByText(page, "Edit MCP");
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
    const reopenedDetail = await waitForText(page, '[aria-label="Session status summary"]', "anthropic/claude");

    await clickButtonByLabel(page, `Delete session ${sessionId}`);
    await screenshot(page, "sessions-delete-confirmation");
    await clickButtonByLabel(page, `Confirm delete session ${sessionId}`);
    await waitForPathname(page, "/sessions");
    const listAfterDelete = await waitForMissingText(page, ".sessions__list-panel", sessionId);

    check(null, {
      "session detail route reached": () => createdPathname.startsWith("/sessions/") && sessionId.length > 0,
      "session detail shows status": () => includesText(detailStatus, "idle"),
      "session detail edits provider runtime": () => includesText(providerRuntime, "anthropic/claude"),
      "session detail edits mcp runtime": () => includesText(mcpRuntime, "files") && includesText(mcpRuntime, "git"),
      "session list contains created session": () => includesText(listedSession, sessionId),
      "session selection reopens detail": () => includesText(reopenedDetail, "anthropic/claude"),
      "session list omits deleted session": () => !includesText(listAfterDelete, sessionId),
    });
  });
}

async function seedRuntimeConfig(page) {
  await page.locator('a[href="/config/provider"]').click();
  await waitForText(page, "#config-title", "Settings");
  await createProvider(page, "openai", "gpt4o");
  await createProvider(page, "anthropic", "claude");

  await page.locator('a[href="/config/mcp"]').click();
  await waitForPathname(page, "/config/mcp");
  await createMcpServer(page, "files");
  await createMcpServer(page, "git");

  await page.locator('a[href="/config/defaults"]').click();
  await waitForText(page, "#runtime-defaults-title", "New session defaults");
  await setSelectValue(page, "#runtime-default-provider", "openai");
  await setSelectValue(page, "#runtime-default-model", "gpt4o");
  await setExplicitMcpDefault(page, "git", true);
  await submitForm(page, 'section[aria-labelledby="runtime-defaults-title"] form');
  await waitForText(page, 'section[aria-labelledby="runtime-defaults-title"]', "Clear defaults");
}

async function createProvider(page, name, model) {
  await page.locator(".config__actions button").click();
  await typeAndKeepFocus(page, 'form[aria-label="Create provider"] input[name="name"]', name);
  await page.locator('form[aria-label="Create provider"] select[name="kind"]').selectOption("openAi");
  await setInputValue(page, 'form[aria-label="Create provider"] input[name="url"]', "https://example.invalid/v1");

  const modelsSelector = `[aria-label="Models for ${name}"]`;
  await page.locator(`${modelsSelector} [aria-label="Model actions"] button`).click();
  await waitForPathname(page, "/config/provider/create/models/create");
  await typeAndKeepFocus(page, 'form[aria-label="Create model"] input[name="key"]', model);
  await typeAndKeepFocus(page, 'form[aria-label="Create model"] input[name="name"]', model);
  await page.locator('form[aria-label="Create model"] select[name="kind"]').selectOption("llm");
  await submitForm(page, 'form[aria-label="Create model"]');
  await waitForText(page, modelsSelector, model);
  await submitForm(page, 'form[aria-label="Create provider"]');
  await waitForText(page, '[aria-label="Provider list"]', name);
}

async function createMcpServer(page, name) {
  await page.locator('[aria-label="MCP server actions"] button').click();
  await typeAndKeepFocus(page, 'form[aria-label="Create MCP server"] input[name="name"]', name);
  await setInputValue(page, 'form[aria-label="Create MCP server"] input[name="command"]', `${name}-mcp`);
  await submitForm(page, 'form[aria-label="Create MCP server"]');
  await waitForText(page, ".config__list", name);
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

async function setInputValue(page, selector, value) {
  await page.evaluate(({ selector, value }) => {
    const input = document.querySelector(selector);

    if (!(input instanceof HTMLInputElement)) {
      throw new Error(`Input not found: ${selector}`);
    }

    input.value = value;
    input.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: value }));
  }, { selector, value });
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

async function setExplicitMcpDefault(page, server, checked) {
  await page.evaluate(({ server, checked }) => {
    const labels = Array.from(document.querySelectorAll('[aria-label="Explicit MCP defaults"] label'));
    const label = labels.find((candidate) => candidate.textContent?.includes(server));
    const input = label?.querySelector('input[type="checkbox"]');

    if (!(input instanceof HTMLInputElement)) {
      throw new Error(`MCP default checkbox not found: ${server}`);
    }

    input.checked = checked;
    input.dispatchEvent(new Event("change", { bubbles: true }));
  }, { server, checked });
}

async function submitForm(page, selector) {
  await page.evaluate((formSelector) => {
    const form = document.querySelector(formSelector);

    if (!(form instanceof HTMLFormElement)) {
      throw new Error(`Form not found: ${formSelector}`);
    }

    form.requestSubmit();
  }, selector);
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
