import { check, sleep } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { typeAndKeepFocus, waitForButton, waitForMissingText, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await page.locator('a[href="/config/provider"]').click();
    await waitForText(page, "#config-title", "Settings");
    await waitForText(page, '[aria-label="Provider actions"]', "Create");
    await screenshot(page, "config-provider-list-empty");

    await page.locator(".config__actions button").click();
    await waitForText(page, 'form[aria-label="Create provider"]', "Name");
    await screenshot(page, "config-provider-create-form-empty");
    await typeAndKeepFocus(
      page,
      'form[aria-label="Create provider"] input[name="name"]',
      "openai",
    );
    await page
      .locator('form[aria-label="Create provider"] select[name="kind"]')
      .selectOption("openAi");
    await typeAndKeepFocus(
      page,
      'form[aria-label="Create provider"] input[name="url"]',
      "https://api.openai.com",
    );
    const modelsSelector = '[aria-label="Models for openai"]';
    await page.locator(`${modelsSelector} [aria-label="Model actions"] button`).click();
    const createModelRoute = await waitForPathname(page, "/config/provider/create/models/create");
    const createModelSeparate = await separateModelFormExists(page, modelsSelector, "Create model");
    const createModelActionsReachable = await modelFormActionsReachable(page, "Create model");
    await screenshot(page, "config-provider-model-create-dialog-empty");
    await typeAndKeepFocus(
      page,
      'form[aria-label="Create model"] input[name="key"]',
      "gpt4o",
    );
    await typeAndKeepFocus(
      page,
      'form[aria-label="Create model"] input[name="name"]',
      "GPT-4o",
    );
    await page.locator('form[aria-label="Create model"] select[name="kind"]').selectOption("llm");
    await submitForm(page, 'form[aria-label="Create model"]');
    const stagedCreateModel = await waitForText(page, modelsSelector, "GPT-4o");
    const stagedCreateModelKind = await waitForText(page, modelsSelector, "llm");
    await page.locator('form[aria-label="Create provider"] button[type="submit"]').click();
    const createdProvider = await waitForText(page, '[aria-label="Provider list"]', "openai");
    const createdUrl = await waitForText(page, '[aria-label="Provider list"]', "https://api.openai.com");
    await waitForButton(page, "Edit openai");
    await screenshot(page, "config-provider-list-created");

    await page.locator('button[aria-label="Edit openai"]').click();
    const persistedCreatedModel = await waitForText(page, modelsSelector, "GPT-4o");
    await screenshot(page, "config-provider-edit-form-with-model");
    await page.locator('button[aria-label="Edit model gpt4o"]').click();
    const editModelRoute = await waitForPathname(page, "/config/provider/edit/openai/models/edit/gpt4o");
    const editModelSeparate = await separateModelFormExists(page, modelsSelector, "Edit model");
    const editModelActionsReachable = await modelFormActionsReachable(page, "Edit model");
    await screenshot(page, "config-provider-model-edit-dialog");
    await setInputValue(
      page,
      'form[aria-label="Edit model"] input[name="name"]',
      "GPT-4o Embeddings",
    );
    await page.locator('form[aria-label="Edit model"] select[name="kind"]').selectOption("embedding");
    await submitForm(page, 'form[aria-label="Edit model"]');
    const editModelReturnedToProviderRoute = await waitForPathname(page, "/config/provider/edit/openai");
    const stagedEditedModelName = await waitForText(page, modelsSelector, "GPT-4o Embeddings");
    const stagedEditedModelKind = await waitForText(page, modelsSelector, "embedding");
    await page.locator('form[aria-label="Edit provider"] button[type="submit"]').click();
    await waitForPathname(page, "/config/provider");

    await page.locator('button[aria-label="Edit openai"]').click();
    const persistedEditedModelName = await waitForText(page, modelsSelector, "GPT-4o Embeddings");
    const persistedEditedModelKind = await waitForText(page, modelsSelector, "embedding");
    await page.locator('button[aria-label="Delete model gpt4o"]').click();
    const deleteModelConfirm = await waitForText(
      page,
      '[aria-label="Delete model gpt4o confirmation"]',
      "Delete?",
    );
    await screenshot(page, "config-provider-model-delete-confirmation");
    await page.locator('button[aria-label="Cancel delete model gpt4o"]').click();
    const cancelledDeleteModel = await waitForText(page, modelsSelector, "GPT-4o Embeddings");
    await page.locator('button[aria-label="Delete model gpt4o"]').click();
    await waitForText(page, '[aria-label="Delete model gpt4o confirmation"]', "Delete?");
    await page.locator('button[aria-label="Confirm delete model gpt4o"]').click();
    const stagedDeletedModel = await waitForMissingText(page, modelsSelector, "GPT-4o Embeddings");
    await page.locator('form[aria-label="Edit provider"] button[type="submit"]').click();
    await waitForPathname(page, "/config/provider");

    await page.locator('button[aria-label="Edit openai"]').click();
    const persistedDeletedModel = await waitForMissingText(page, modelsSelector, "GPT-4o Embeddings");
    await page.locator('form[aria-label="Edit provider"] select[name="kind"]').selectOption("anthropic");
    await page.locator('form[aria-label="Edit provider"] button[type="submit"]').click();
    const editedProvider = await waitForText(page, '[aria-label="Provider list"]', "anthropic");

    await page.locator('button[aria-label="Delete openai"]').click();
    const deleteConfirm = await waitForText(
      page,
      '[aria-label="Delete openai confirmation"]',
      "Delete?",
    );
    await screenshot(page, "config-provider-delete-confirmation");
    await page.locator('button[aria-label="Confirm delete openai"]').click();
    await waitForMissingText(page, '[aria-label="Provider list"]', "openai");

    check(null, {
      "provider is created": () => includesText(createdProvider, "openai"),
      "provider URL is created": () => includesText(createdUrl, "https://api.openai.com"),
      "create model route is reached": () => createModelRoute,
      "create model is separate": () => createModelSeparate,
      "create model actions are reachable": () => createModelActionsReachable,
      "created model is staged": () => includesText(stagedCreateModel, "GPT-4o"),
      "created model kind is staged": () => includesText(stagedCreateModelKind, "llm"),
      "created model persists": () => includesText(persistedCreatedModel, "GPT-4o"),
      "edit model route is reached": () => editModelRoute,
      "edit model is separate": () => editModelSeparate,
      "edit model actions are reachable": () => editModelActionsReachable,
      "edit model returns to provider route": () => editModelReturnedToProviderRoute,
      "edited model name is staged": () => includesText(stagedEditedModelName, "GPT-4o Embeddings"),
      "edited model kind is staged": () => includesText(stagedEditedModelKind, "embedding"),
      "edited model name persists": () => includesText(persistedEditedModelName, "GPT-4o Embeddings"),
      "edited model kind persists": () => includesText(persistedEditedModelKind, "embedding"),
      "model delete confirmation appears": () => includesText(deleteModelConfirm, "Delete?"),
      "cancelled model delete keeps model": () => includesText(cancelledDeleteModel, "GPT-4o Embeddings"),
      "staged model delete removes model": () => !includesText(stagedDeletedModel, "GPT-4o Embeddings"),
      "deleted model stays removed": () => !includesText(persistedDeletedModel, "GPT-4o Embeddings"),
      "provider kind is edited": () => includesText(editedProvider, "anthropic"),
      "provider delete confirmation appears": () => includesText(deleteConfirm, "Delete?"),
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

async function waitForPathname(page, expectedPathname) {
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

async function separateModelFormExists(page, modelsSelector, formLabel) {
  return page.evaluate(({ modelsSelector, formLabel }) => {
    const models = document.querySelector(modelsSelector);
    const form = document.querySelector(`.config__model-dialog form[aria-label="${formLabel}"]`);
    return Boolean(form) && !Boolean(models?.querySelector(`form[aria-label="${formLabel}"]`));
  }, { modelsSelector, formLabel });
}

async function modelFormActionsReachable(page, formLabel) {
  return page.evaluate((label) => {
    const form = document.querySelector(`.config__model-dialog form[aria-label="${label}"]`);

    if (!(form instanceof HTMLElement)) {
      return false;
    }

    form.style.maxHeight = "180px";
    form.style.overflowY = "auto";

    const save = form.querySelector('button[type="submit"]');
    const cancel = Array.from(form.querySelectorAll('button[type="button"]'))
      .find((button) => button.textContent?.trim() === "Cancel");

    if (!(save instanceof HTMLElement) || !(cancel instanceof HTMLElement)) {
      return false;
    }

    form.scrollTop = form.scrollHeight;
    const formRect = form.getBoundingClientRect();
    const saveRect = save.getBoundingClientRect();
    const cancelRect = cancel.getBoundingClientRect();
    const visibleWithinForm = (rect) => rect.top >= formRect.top && rect.bottom <= formRect.bottom;

    return form.scrollHeight > form.clientHeight && visibleWithinForm(saveRect) && visibleWithinForm(cancelRect);
  }, formLabel);
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
