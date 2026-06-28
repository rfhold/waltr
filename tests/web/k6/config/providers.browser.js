import { check, sleep } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { typeAndKeepFocus, waitForButton, waitForMissingText, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

const PROVIDER = "catalog-openai";
const MODEL = "gpt-5-nano";
const CATALOG_MODEL = "openai/gpt-5-nano";
const MODEL_NAME = "GPT-5 Nano";
const EDITED_MODEL_NAME = "GPT-5 Nano Embeddings";

export default async function () {
  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await page.locator('a[href="/config/provider"]').click();
    await waitForText(page, "#config-title", "Settings");
    await waitForText(page, '[aria-label="Provider actions"]', "Create");
    await screenshot(page, "config-provider-list-empty");

    await page.locator('[aria-label="Provider actions"] button').click();
    await waitForText(page, 'form[aria-label="Create provider"]', "Name");
    await screenshot(page, "config-provider-create-form-empty");
    await typeAndKeepFocus(
      page,
      'form[aria-label="Create provider"] input[name="name"]',
      PROVIDER,
    );
    await page
      .locator('form[aria-label="Create provider"] select[name="kind"]')
      .selectOption("openAiResponses");
    await typeAndKeepFocus(
      page,
      'form[aria-label="Create provider"] input[name="url"]',
      "https://api.openai.com/v1",
    );
    const modelsSelector = `[aria-label="Models for ${PROVIDER}"]`;
    await page.locator(`${modelsSelector} [aria-label="Model actions"] button`).click();
    const createModelRoute = await waitForPathname(page, "/config/provider/create/models/create");
    const createModelSeparate = await separateModelFormExists(page, modelsSelector, "Create model");
    const createModelActionsReachable = await modelFormActionsReachable(page, "Create model");
    await screenshot(page, "config-provider-model-create-dialog-empty");
    await typeAndKeepFocus(
      page,
      'form[aria-label="Create model"] input[name="key"]',
      MODEL,
    );
    await page.locator('form[aria-label="Create model"] select[name="kind"]').selectOption("llm");
    await typeAndKeepFocus(
      page,
      'form[aria-label="Create model"] input[name="catalogSearch"]',
      MODEL,
    );
    await clickButtonInForm(page, "Create model", "Search");
    const catalogSearchResult = await waitForCatalogResultCode(
      page,
      "Catalog search results",
      CATALOG_MODEL,
    );
    await screenshot(page, "config-provider-model-catalog-search-results");
    await clickCatalogResultByCode(page, "Catalog search results", CATALOG_MODEL);
    const catalogApplied = await waitForText(page, 'form[aria-label="Create model"]', CATALOG_MODEL);
    const catalogReasoningEfforts = await waitForInputValue(
      page,
      'form[aria-label="Create model"] input[name="reasoningEfforts"]',
      "high, low, medium",
    );
    await setInputValue(page, 'form[aria-label="Create model"] input[name="catalogSearch"]', "");
    await clickButtonInForm(page, "Create model", "Clear");
    await clickButtonInForm(page, "Create model", "Suggest");
    const catalogSuggestion = await waitForCatalogResultCode(
      page,
      "Suggested catalog matches",
      CATALOG_MODEL,
    );
    await screenshot(page, "config-provider-model-catalog-suggested");
    await clickCatalogResultByCode(page, "Suggested catalog matches", CATALOG_MODEL);
    await waitForText(page, 'form[aria-label="Create model"]', CATALOG_MODEL);
    await screenshot(page, "config-provider-model-catalog-applied");
    await submitForm(page, 'form[aria-label="Create model"]');
    const stagedCreateModel = await waitForText(page, modelsSelector, MODEL_NAME);
    const stagedCreateModelKind = await waitForText(page, modelsSelector, "llm");
    await page.locator('form[aria-label="Create provider"] button[type="submit"]').click();
    const createdProvider = await waitForText(page, '[aria-label="Provider list"]', PROVIDER);
    const createdUrl = await waitForText(page, '[aria-label="Provider list"]', "https://api.openai.com/v1");
    await waitForButton(page, `Edit ${PROVIDER}`);
    await screenshot(page, "config-provider-list-created");

    await page.locator(`button[aria-label="Edit ${PROVIDER}"]`).click();
    const persistedCreatedModel = await waitForText(page, modelsSelector, MODEL_NAME);
    await screenshot(page, "config-provider-edit-form-with-model");
    await page.locator(`button[aria-label="Edit model ${MODEL}"]`).click();
    const editModelRoute = await waitForPathname(page, `/config/provider/edit/${PROVIDER}/models/edit/${MODEL}`);
    const editModelSeparate = await separateModelFormExists(page, modelsSelector, "Edit model");
    const editModelActionsReachable = await modelFormActionsReachable(page, "Edit model");
    const persistedCatalogReasoningEfforts = await waitForInputValue(
      page,
      'form[aria-label="Edit model"] input[name="reasoningEfforts"]',
      "high, low, medium",
    );
    await screenshot(page, "config-provider-model-edit-dialog");
    await setInputValue(
      page,
      'form[aria-label="Edit model"] input[name="name"]',
      EDITED_MODEL_NAME,
    );
    await page.locator('form[aria-label="Edit model"] select[name="kind"]').selectOption("embedding");
    await submitForm(page, 'form[aria-label="Edit model"]');
    const editModelReturnedToProviderRoute = await waitForPathname(page, `/config/provider/edit/${PROVIDER}`);
    const stagedEditedModelName = await waitForText(page, modelsSelector, EDITED_MODEL_NAME);
    const stagedEditedModelKind = await waitForText(page, modelsSelector, "embedding");
    await page.locator('form[aria-label="Edit provider"] button[type="submit"]').click();
    await waitForPathname(page, "/config/provider");

    await page.locator(`button[aria-label="Edit ${PROVIDER}"]`).click();
    const persistedEditedModelName = await waitForText(page, modelsSelector, EDITED_MODEL_NAME);
    const persistedEditedModelKind = await waitForText(page, modelsSelector, "embedding");
    await page.locator(`button[aria-label="Delete model ${MODEL}"]`).click();
    const deleteModelConfirm = await waitForText(
      page,
      `[aria-label="Delete model ${MODEL} confirmation"]`,
      "Delete?",
    );
    await screenshot(page, "config-provider-model-delete-confirmation");
    await page.locator(`button[aria-label="Cancel delete model ${MODEL}"]`).click();
    const cancelledDeleteModel = await waitForText(page, modelsSelector, EDITED_MODEL_NAME);
    await page.locator(`button[aria-label="Delete model ${MODEL}"]`).click();
    await waitForText(page, `[aria-label="Delete model ${MODEL} confirmation"]`, "Delete?");
    await page.locator(`button[aria-label="Confirm delete model ${MODEL}"]`).click();
    const stagedDeletedModel = await waitForMissingText(page, modelsSelector, EDITED_MODEL_NAME);
    await page.locator('form[aria-label="Edit provider"] button[type="submit"]').click();
    await waitForPathname(page, "/config/provider");

    await page.locator(`button[aria-label="Edit ${PROVIDER}"]`).click();
    const persistedDeletedModel = await waitForMissingText(page, modelsSelector, EDITED_MODEL_NAME);
    await page.locator('form[aria-label="Edit provider"] select[name="kind"]').selectOption("anthropicMessages");
    await page.locator('form[aria-label="Edit provider"] button[type="submit"]').click();
    await waitForPathname(page, "/config/provider");
    const editedProvider = await waitForText(page, '[aria-label="Provider list"]', "anthropic");

    await waitForButton(page, `Delete ${PROVIDER}`);
    await page.locator(`button[aria-label="Delete ${PROVIDER}"]`).click();
    const deleteConfirm = await waitForText(
      page,
      `[aria-label="Delete ${PROVIDER} confirmation"]`,
      "Delete?",
    );
    await screenshot(page, "config-provider-delete-confirmation");
    await page.locator(`button[aria-label="Confirm delete ${PROVIDER}"]`).click();
    await waitForMissingText(page, '[aria-label="Provider list"]', PROVIDER);

    check(null, {
      "provider is created": () => includesText(createdProvider, PROVIDER),
      "provider URL is created": () => includesText(createdUrl, "https://api.openai.com/v1"),
      "create model route is reached": () => createModelRoute,
      "create model is separate": () => createModelSeparate,
      "create model actions are reachable": () => createModelActionsReachable,
      "catalog search returns model": () => includesText(catalogSearchResult, CATALOG_MODEL),
      "catalog facts apply to model": () => includesText(catalogApplied, CATALOG_MODEL),
      "catalog reasoning efforts apply": () => catalogReasoningEfforts === "high, low, medium",
      "catalog suggest returns model": () => includesText(catalogSuggestion, CATALOG_MODEL),
      "created model is staged": () => includesText(stagedCreateModel, MODEL_NAME),
      "created model kind is staged": () => includesText(stagedCreateModelKind, "llm"),
      "created model persists": () => includesText(persistedCreatedModel, MODEL_NAME),
      "edit model route is reached": () => editModelRoute,
      "edit model is separate": () => editModelSeparate,
      "edit model actions are reachable": () => editModelActionsReachable,
      "catalog reasoning efforts persist": () => persistedCatalogReasoningEfforts === "high, low, medium",
      "edit model returns to provider route": () => editModelReturnedToProviderRoute,
      "edited model name is staged": () => includesText(stagedEditedModelName, EDITED_MODEL_NAME),
      "edited model kind is staged": () => includesText(stagedEditedModelKind, "embedding"),
      "edited model name persists": () => includesText(persistedEditedModelName, EDITED_MODEL_NAME),
      "edited model kind persists": () => includesText(persistedEditedModelKind, "embedding"),
      "model delete confirmation appears": () => includesText(deleteModelConfirm, "Delete?"),
      "cancelled model delete keeps model": () => includesText(cancelledDeleteModel, EDITED_MODEL_NAME),
      "staged model delete removes model": () => !includesText(stagedDeletedModel, EDITED_MODEL_NAME),
      "deleted model stays removed": () => !includesText(persistedDeletedModel, EDITED_MODEL_NAME),
      "provider kind is edited": () => includesText(editedProvider, "anthropic"),
      "provider delete confirmation appears": () => includesText(deleteConfirm, "Delete?"),
    });
  });
}

async function clickButtonInForm(page, formLabel, buttonText) {
  await page.evaluate(({ formLabel, buttonText }) => {
    const form = document.querySelector(`form[aria-label="${formLabel}"]`);
    if (!(form instanceof HTMLFormElement)) {
      throw new Error(`Form not found: ${formLabel}`);
    }

    const button = Array.from(form.querySelectorAll("button")).find((candidate) =>
      candidate.textContent?.trim() === buttonText,
    );
    if (!(button instanceof HTMLButtonElement)) {
      throw new Error(`Button not found in ${formLabel}: ${buttonText}`);
    }

    button.scrollIntoView({ block: "center", inline: "center" });
    button.click();
  }, { formLabel, buttonText });
}

async function waitForCatalogResultCode(page, resultsLabel, code) {
  const deadline = Date.now() + 10_000;
  let text = "";

  while (Date.now() < deadline) {
    text = await page.evaluate(({ resultsLabel, code }) => {
      const results = document.querySelector(`[aria-label="${resultsLabel}"]`);
      if (!(results instanceof HTMLElement)) {
        return "";
      }

      const button = Array.from(results.querySelectorAll("button")).find((candidate) =>
        Array.from(candidate.querySelectorAll("code")).some((codeNode) => codeNode.textContent?.trim() === code),
      );

      return button?.textContent ?? "";
    }, { resultsLabel, code });

    if (text) {
      return text;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for ${resultsLabel} to include ${code}`);
}

async function clickCatalogResultByCode(page, resultsLabel, code) {
  await page.evaluate(({ resultsLabel, code }) => {
    const results = document.querySelector(`[aria-label="${resultsLabel}"]`);
    if (!(results instanceof HTMLElement)) {
      throw new Error(`Catalog results not found: ${resultsLabel}`);
    }

    const button = Array.from(results.querySelectorAll("button")).find((candidate) =>
      Array.from(candidate.querySelectorAll("code")).some((codeNode) => codeNode.textContent?.trim() === code),
    );
    if (!(button instanceof HTMLButtonElement)) {
      throw new Error(`Catalog result not found: ${code}`);
    }

    button.scrollIntoView({ block: "center", inline: "center" });
    button.click();
  }, { resultsLabel, code });
}

async function waitForInputValue(page, selector, expectedValue) {
  const deadline = Date.now() + 10_000;
  let value = "";

  while (Date.now() < deadline) {
    value = await page.evaluate((inputSelector) => {
      const input = document.querySelector(inputSelector);
      if (!(input instanceof HTMLInputElement)) {
        throw new Error(`Input not found: ${inputSelector}`);
      }

      return input.value;
    }, selector);

    if (value === expectedValue) {
      return value;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for ${selector} to equal ${expectedValue}; saw ${value}`);
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

    const save = form.querySelector('button[type="submit"]');
    const cancel = Array.from(form.querySelectorAll('button[type="button"]'))
      .find((button) => button.textContent?.trim() === "Cancel");

    if (!(save instanceof HTMLElement) || !(cancel instanceof HTMLElement)) {
      return false;
    }

    return [save, cancel].every((button) => {
      const style = window.getComputedStyle(button);
      return style.display !== "none" && style.visibility !== "hidden";
    });
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
