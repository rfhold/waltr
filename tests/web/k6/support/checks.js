import { check, sleep } from "k6";

import { includesText, normalizeWhitespace } from "./text.js";

export async function waitForButton(page, label) {
  const deadline = Date.now() + 10_000;
  let labels = [];

  while (Date.now() < deadline) {
    labels = await page.evaluate(() =>
      Array.from(document.querySelectorAll("button"), (button) =>
        button.getAttribute("aria-label") ?? button.textContent ?? "",
      ),
    );

    if (labels.includes(label)) {
      return;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for button ${label}; saw ${JSON.stringify(labels)}`);
}

export async function typeAndKeepFocus(page, selector, value) {
  await page.locator(selector).click();
  await page.keyboard.type(value);

  const result = await page.evaluate((inputSelector) => {
    const input = document.querySelector(inputSelector);

    return {
      focused: document.activeElement === input,
      value: input?.value ?? "",
    };
  }, selector);

  check(result, {
    [`${selector} keeps focus while typing`]: ({ focused, value: actual }) =>
      focused && actual === value,
  });

  if (!result.focused || result.value !== value) {
    throw new Error(`${selector} lost focus or value while typing; saw ${JSON.stringify(result)}`);
  }
}

export async function waitForText(page, selector, expectedText) {
  const deadline = Date.now() + 10_000;
  let text = "";

  await page.locator(selector).waitFor({ timeout: 10_000 });

  while (Date.now() < deadline) {
    text = normalizeWhitespace(await page.locator(selector).textContent());

    if (includesText(text, expectedText)) {
      return text;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for ${selector} to contain ${expectedText}; saw ${text}`);
}

export async function waitForMissingText(page, selector, unexpectedText) {
  const deadline = Date.now() + 10_000;
  let text = "";

  await page.locator(selector).waitFor({ timeout: 10_000 });

  while (Date.now() < deadline) {
    text = normalizeWhitespace(await page.locator(selector).textContent());

    if (!includesText(text, unexpectedText)) {
      return text;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for ${selector} to omit ${unexpectedText}; saw ${text}`);
}

export async function waitForPathname(page, expectedPathname) {
  const deadline = Date.now() + 10_000;
  let pathname = "";

  while (Date.now() < deadline) {
    pathname = await page.evaluate(() => window.location.pathname);

    if (pathname === expectedPathname) {
      return true;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for route ${expectedPathname}; saw ${pathname}`);
}

export async function waitForSelectOption(page, selector, value) {
  const deadline = Date.now() + 10_000;
  let options = [];

  await page.locator(selector).waitFor({ timeout: 10_000 });

  while (Date.now() < deadline) {
    options = await page.evaluate((selectSelector) => {
      const select = document.querySelector(selectSelector);

      if (!(select instanceof HTMLSelectElement)) {
        throw new Error(`Select not found: ${selectSelector}`);
      }

      return Array.from(select.options, (option) => option.value);
    }, selector);

    if (options.includes(value)) {
      return;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for ${selector} to include option ${value}; saw ${JSON.stringify(options)}`);
}
