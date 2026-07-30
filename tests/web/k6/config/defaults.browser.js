import { check, sleep } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { typeAndKeepFocus, waitForMissingText, waitForPathname, waitForSelectOption, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

const PROVIDER = "defaults-openai";
const MODEL = "gpt-5-nano";

export default async function () {
  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await page.locator('a[href="/config/provider"]').click();
    await waitForText(page, "#config-title", "Settings");
    await page.locator('[aria-label="Provider actions"] button').click();
    await typeAndKeepFocus(page, 'form[aria-label="Create provider"] input[name="name"]', PROVIDER);
    await page.locator('form[aria-label="Create provider"] select[name="kind"]').selectOption("openAiResponses");
    await setInputValue(page, 'form[aria-label="Create provider"] input[name="url"]', "https://api.openai.com/v1/chat/completions");

    const modelsSelector = `[aria-label="Models for ${PROVIDER}"]`;
    await page.locator(`${modelsSelector} [aria-label="Model actions"] button`).click();
    await waitForPathname(page, "/config/provider/create/models/create");
    await typeAndKeepFocus(page, 'form[aria-label="Create model"] input[name="key"]', MODEL);
    await typeAndKeepFocus(page, 'form[aria-label="Create model"] input[name="name"]', "GPT-5 Nano");
    await page.locator('form[aria-label="Create model"] select[name="kind"]').selectOption("llm");
    await submitForm(page, 'form[aria-label="Create model"]');
    await waitForText(page, modelsSelector, "GPT-5 Nano");
    await submitForm(page, 'form[aria-label="Create provider"]');
    await waitForText(page, '[aria-label="Provider list"]', PROVIDER);

    await page.locator('a[href="/config/mcp"]').click();
    await waitForPathname(page, "/config/mcp");
    await page.locator('[aria-label="MCP server actions"] button').click();
    await typeAndKeepFocus(page, 'form[aria-label="Create MCP server"] input[name="name"]', "localstdio");
    await setInputValue(page, 'form[aria-label="Create MCP server"] input[name="command"]', "npx local-mcp");
    await submitForm(page, 'form[aria-label="Create MCP server"]');
    await waitForText(page, ".config__list", "localstdio");

    await page.locator('a[href="/config/defaults"]').click();
    const defaultsRoute = await waitForPathname(page, "/config/defaults");
    await waitForText(page, "#runtime-defaults-title", "New session defaults");
    await waitForSelectOption(page, "#runtime-default-provider", PROVIDER);
    await setSelectValue(page, "#runtime-default-provider", PROVIDER);
    await waitForSelectOption(page, "#runtime-default-model", MODEL);
    await setSelectValue(page, "#runtime-default-model", MODEL);
    await waitForText(page, '[aria-label="Explicit MCP defaults"]', "localstdio");
    await setExplicitMcpDefault(page, "localstdio", true);
    const explicitDefaults = await waitForText(page, '[aria-label="Explicit MCP defaults"]', "localstdio");
    await screenshot(page, "config-defaults-explicit-mcp");
    await submitForm(page, 'section[aria-labelledby="runtime-defaults-title"] form');
    await waitForText(page, 'section[aria-labelledby="runtime-defaults-title"]', "Clear defaults");
    await screenshot(page, "config-defaults-saved");

    await page.locator(".config__header button").click();
    const resetConfirmation = await waitForText(
      page,
      '[aria-label="Reset configuration confirmation"]',
      "Credentials remain untouched",
    );
    await page.locator('button[aria-label="Confirm reset configuration"]').click();
    const resetSuccess = await waitForText(page, ".config", "Configuration reset.");

    await page.locator('a[href="/config/provider"]').click();
    const resetProviders = await waitForText(page, '[aria-label="Provider list"]', "claude-code");
    await waitForText(page, '[aria-label="Provider list"]', "anthropic");
    await waitForText(page, '[aria-label="Provider list"]', "openai");
    await waitForText(page, '[aria-label="Provider list"]', "codex");
    const customProviderRemoved = await waitForMissingText(page, '[aria-label="Provider list"]', PROVIDER);

    await page.locator('a[href="/config/mcp"]').click();
    const resetMcpServers = await waitForText(page, ".config__list", "waltr");
    const customMcpRemoved = await waitForMissingText(page, ".config__list", "localstdio");

    await page.locator('a[href="/config/defaults"]').click();
    const resetProviderDefault = await waitForSelectValue(page, "#runtime-default-provider", "");
    const resetModelDefault = await waitForSelectValue(page, "#runtime-default-model", "");

    check({
      defaultsRoute,
      explicitDefaults,
      resetConfirmation,
      resetSuccess,
      resetProviders,
      customProviderRemoved,
      resetMcpServers,
      customMcpRemoved,
      resetProviderDefault,
      resetModelDefault,
    }, {
      "runtime defaults screen saves explicit MCP default": ({ defaultsRoute, explicitDefaults }) =>
        defaultsRoute && includesText(explicitDefaults, "localstdio"),
      "configuration reset restores stable server defaults": ({
        resetConfirmation,
        resetSuccess,
        resetProviders,
        customProviderRemoved,
        resetMcpServers,
        customMcpRemoved,
        resetProviderDefault,
        resetModelDefault,
      }) =>
        includesText(resetConfirmation, "Credentials remain untouched") &&
        includesText(resetSuccess, "Configuration reset.") &&
        ["anthropic", "claude-code", "openai", "codex"].every((provider) => includesText(resetProviders, provider)) &&
        !includesText(customProviderRemoved, PROVIDER) &&
        includesText(resetMcpServers, "waltr") &&
        !includesText(customMcpRemoved, "localstdio") &&
        resetProviderDefault === "" &&
        resetModelDefault === "",
    });
  });
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

async function waitForSelectValue(page, selector, expectedValue) {
  const deadline = Date.now() + 10_000;
  let value = null;

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
