import { check, sleep } from "k6";

import { baseUrl, options as browserOptions, withPage } from "../support/browser.js";
import { typeAndKeepFocus, waitForPathname, waitForText } from "../support/checks.js";
import { includesText, normalizeWhitespace } from "../support/text.js";

export const options = browserOptions;

const GATEWAY_BASE_URL = "https://agent-gateway.holdenitdown.net";
const GATEWAY_MODEL = "openai/gpt-5-nano";
const LIVE_PROMPT = "Reply exactly: waltr web live ok";
const LIVE_EXPECTED = "waltr web live ok";
const FOLLOWUP_PROMPT = "Reply exactly: waltr web live followup ok";
const FOLLOWUP_EXPECTED = "waltr web live followup ok";

export default async function () {
  if (__ENV.WALTR_AGENT_LIVE_GATEWAY !== "1") {
    return;
  }

  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await seedGatewayProvider(page);

    await page.locator('.shell__nav a[href="/sessions"]').click();
    await waitForText(page, "#sessions-title", "Sessions");
    await page.locator(".sessions__header button").click();
    await waitForText(page, '[aria-label="Session status summary"]', `Provider gateway-openai/${GATEWAY_MODEL}`);

    await typeAndKeepFocus(page, '[aria-label="Session message"]', LIVE_PROMPT);
    await submitEnabledForm(page, 'form[aria-label="Send session message"]');

    const userText = await waitForLiveText(page, '[aria-label="Session timeline"]', LIVE_PROMPT);
    const assistantText = await waitForLiveText(page, '[aria-label="Session timeline"]', LIVE_EXPECTED);

    check(null, {
      "live session rendered user message": () => includesText(userText, LIVE_PROMPT),
      "live session rendered assistant message": () => includesText(assistantText, LIVE_EXPECTED),
    });

    await page.reload({ waitUntil: "domcontentloaded" });
    await waitForLiveText(page, '[aria-label="Session timeline"]', LIVE_EXPECTED);
    await typeAndKeepFocus(page, '[aria-label="Session message"]', FOLLOWUP_PROMPT);
    await submitEnabledForm(page, 'form[aria-label="Send session message"]');

    const followupUserText = await waitForLiveText(page, '[aria-label="Session timeline"]', FOLLOWUP_PROMPT);
    const followupAssistantText = await waitForLiveText(page, '[aria-label="Session timeline"]', FOLLOWUP_EXPECTED);

    check(null, {
      "live session rendered followup user message": () => includesText(followupUserText, FOLLOWUP_PROMPT),
      "live session rendered followup assistant message": () => includesText(followupAssistantText, FOLLOWUP_EXPECTED),
    });
  });
}

async function seedGatewayProvider(page) {
  await page.locator('a[href="/config/provider"]').click();
  await waitForText(page, "#config-title", "Settings");
  await createProvider(page, "gateway-openai", GATEWAY_MODEL);

  await page.locator('a[href="/config/defaults"]').click();
  await waitForText(page, "#runtime-defaults-title", "New session defaults");
  await setSelectValue(page, "#runtime-default-provider", "gateway-openai");
  await setSelectValue(page, "#runtime-default-model", GATEWAY_MODEL);
  await submitForm(page, 'section[aria-labelledby="runtime-defaults-title"] form');
  await waitForText(page, 'section[aria-labelledby="runtime-defaults-title"]', "Clear defaults");
}

async function createProvider(page, name, model) {
  await page.locator(".config__actions button").click();
  await typeAndKeepFocus(page, 'form[aria-label="Create provider"] input[name="name"]', name);
  await page.locator('form[aria-label="Create provider"] select[name="kind"]').selectOption("openAi");
  await setInputValue(page, 'form[aria-label="Create provider"] input[name="url"]', GATEWAY_BASE_URL);

  const modelsSelector = `[aria-label="Models for ${name}"]`;
  await page.locator(`${modelsSelector} [aria-label="Model actions"] button`).click();
  await waitForPathname(page, "/config/provider/create/models/create");
  await setModelKey(page, model);
  await typeAndKeepFocus(page, 'form[aria-label="Create model"] input[name="name"]', model);
  await page.locator('form[aria-label="Create model"] select[name="kind"]').selectOption("llm");
  await submitForm(page, 'form[aria-label="Create model"]');
  await waitForText(page, modelsSelector, model);
  await submitForm(page, 'form[aria-label="Create provider"]');
  await waitForText(page, '[aria-label="Provider list"]', name);
}

async function setModelKey(page, value) {
  await page.evaluate((modelKey) => {
    const input = document.querySelector('form[aria-label="Create model"] input[name="key"]');
    if (!(input instanceof HTMLInputElement)) {
      throw new Error("Model key input not found");
    }

    input.removeAttribute("pattern");
    input.value = modelKey;
    input.dispatchEvent(new InputEvent("input", { bubbles: true, inputType: "insertText", data: modelKey }));
  }, value);
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

async function submitForm(page, selector) {
  await page.evaluate((formSelector) => {
    const form = document.querySelector(formSelector);
    if (!(form instanceof HTMLFormElement)) {
      throw new Error(`Form not found: ${formSelector}`);
    }

    form.requestSubmit();
  }, selector);
}

async function submitEnabledForm(page, selector) {
  const deadline = Date.now() + 10_000;
  let disabled = true;

  while (Date.now() < deadline) {
    disabled = await page.evaluate((formSelector) => {
      const form = document.querySelector(formSelector);
      if (!(form instanceof HTMLFormElement)) {
        throw new Error(`Form not found: ${formSelector}`);
      }

      const submit = form.querySelector('button[type="submit"]');
      if (!(submit instanceof HTMLButtonElement)) {
        throw new Error(`Submit button not found for ${formSelector}`);
      }

      return submit.disabled;
    }, selector);

    if (!disabled) {
      break;
    }

    sleep(0.25);
  }

  if (disabled) {
    throw new Error(`Submit button is disabled for ${selector}`);
  }

  await page.evaluate((formSelector) => {
    const form = document.querySelector(formSelector);
    if (!(form instanceof HTMLFormElement)) {
      throw new Error(`Form not found: ${formSelector}`);
    }

    const submit = form.querySelector('button[type="submit"]');
    if (!(submit instanceof HTMLButtonElement)) {
      throw new Error(`Submit button not found for ${formSelector}`);
    }

    form.requestSubmit(submit);
  }, selector);
}

async function waitForLiveText(page, selector, expectedText) {
  const deadline = Date.now() + 90_000;
  let text = "";

  await page.locator(selector).waitFor({ timeout: 10_000 });

  while (Date.now() < deadline) {
    text = normalizeWhitespace(await page.locator(selector).textContent());
    if (includesText(text, expectedText)) {
      return text;
    }

    sleep(0.5);
  }

  throw new Error(`Timed out waiting for ${selector} to contain ${expectedText}; saw ${text}`);
}
