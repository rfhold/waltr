import { check } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { typeAndKeepFocus, waitForPathname, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await page.locator('a[href="/config/provider"]').click();
    await waitForText(page, "#config-title", "Settings");
    await page.locator(".config__actions button").click();
    await typeAndKeepFocus(page, 'form[aria-label="Create provider"] input[name="name"]', "openai");
    await page.locator('form[aria-label="Create provider"] select[name="kind"]').selectOption("openAi");
    await setInputValue(page, 'form[aria-label="Create provider"] input[name="url"]', "https://api.openai.com");

    const modelsSelector = '[aria-label="Models for openai"]';
    await page.locator(`${modelsSelector} [aria-label="Model actions"] button`).click();
    await waitForPathname(page, "/config/provider/create/models/create");
    await typeAndKeepFocus(page, 'form[aria-label="Create model"] input[name="key"]', "gpt4o");
    await typeAndKeepFocus(page, 'form[aria-label="Create model"] input[name="name"]', "GPT-4o");
    await page.locator('form[aria-label="Create model"] select[name="kind"]').selectOption("llm");
    await submitForm(page, 'form[aria-label="Create model"]');
    await waitForText(page, modelsSelector, "GPT-4o");
    await submitForm(page, 'form[aria-label="Create provider"]');
    await waitForText(page, '[aria-label="Provider list"]', "openai");

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
    await setSelectValue(page, "#runtime-default-provider", "openai");
    await setSelectValue(page, "#runtime-default-model", "gpt4o");
    await setExplicitMcpDefault(page, "localstdio", true);
    const explicitDefaults = await waitForText(page, '[aria-label="Explicit MCP defaults"]', "localstdio");
    await screenshot(page, "config-defaults-explicit-mcp");
    await submitForm(page, 'section[aria-labelledby="runtime-defaults-title"] form');
    await waitForText(page, 'section[aria-labelledby="runtime-defaults-title"]', "Clear defaults");
    await screenshot(page, "config-defaults-saved");

    check({ defaultsRoute, explicitDefaults }, {
      "runtime defaults screen saves explicit MCP default": ({ defaultsRoute, explicitDefaults }) =>
        defaultsRoute && includesText(explicitDefaults, "localstdio"),
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
