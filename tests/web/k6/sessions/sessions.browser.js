import { check, sleep } from "k6";

import {
  baseUrl,
  options as browserOptions,
  responsiveViewports,
  screenshotMatrix,
  setResponsiveViewport,
  withPage,
} from "../support/browser.js";
import { typeAndKeepFocus, waitForMissingText, waitForPathname, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

const PRIMARY_PROVIDER = "k6-openai";
const PRIMARY_MODEL = "gpt-5-nano";
const SECONDARY_PROVIDER = "k6-anthropic";
const SECONDARY_MODEL = "claude";

export default async function () {
  await withPage(async (page) => {
    await setResponsiveViewport(page, responsiveViewports.desktop);
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await page.goto(`${baseUrl}/sessions`, { waitUntil: "domcontentloaded" });
    await waitForPathname(page, "/sessions");
    await waitForText(page, "#sessions-title", "Sessions");
    await waitForText(page, "#sessions-list-title", "Recent sessions");
    await screenshotMatrix(page, "sessions-list-empty");

    await createSession(page);
    await waitForText(page, '[aria-label="Session detail"]', "");
    await waitForText(page, '[aria-label="Session status summary"]', `Provider ${PRIMARY_PROVIDER}/${PRIMARY_MODEL}`);
    const thinkingSelectEnabled = await waitForSelectEnabled(page, '[aria-label="Session thinking level"]');

    const createdPathname = await page.evaluate(() => window.location.pathname);
    const sessionId = decodeURIComponent(createdPathname.replace(/^\/sessions\//, ""));
    const detailTimeline = await waitForText(page, '[aria-label="Session timeline"]', "No messages yet.");
    const sessionContext = await waitForText(page, '[aria-label="Session context"]', "No Routine invocations.");
    const detailFillsShell = await page.evaluate(() => {
      const shellContent = document.querySelector(".shell__content");
      const detail = document.querySelector(".sessions--detail");
      if (!shellContent || !detail) return false;
      return Math.abs(shellContent.getBoundingClientRect().height - detail.getBoundingClientRect().height) < 1;
    });
    await screenshotMatrix(page, "sessions-detail-empty");
    await setResponsiveViewport(page, responsiveViewports.phone);
    const mobileGeometry = await page.evaluate(() => {
      const shell = document.querySelector(".shell");
      const context = document.querySelector(".sessions__context-disclosure");
      const composer = document.querySelector(".sessions__composer");
      if (!(shell instanceof HTMLElement) || !(context instanceof HTMLDetailsElement) || !(composer instanceof HTMLElement)) return null;
      return {
        contextOpen: context.open,
        shellWithinViewport: shell.getBoundingClientRect().width <= window.innerWidth + 1,
        composerReachable: composer.getBoundingClientRect().bottom <= window.innerHeight + 1,
        horizontalOverflow: document.documentElement.scrollWidth - document.documentElement.clientWidth,
      };
    });
    await setResponsiveViewport(page, responsiveViewports.desktop);

    await typeAndKeepFocus(page, '[aria-label="Session message"]', "@read");
    const resourceSuggestions = await waitForText(page, '[aria-label="Resource suggestions"]', "read");
    await screenshotMatrix(page, "sessions-resource-search-popover");
    await page.keyboard.press("Enter");
    const resourceFilled = await waitForTextareaValueChange(page, '[aria-label="Session message"]', "@read");
    await clearTextarea(page, '[aria-label="Session message"]');

    await setSelectValue(page, '[aria-label="Session thinking level"]', "high");
    const thinkingRuntime = await waitForText(page, '[aria-label="Session status summary"]', `${PRIMARY_PROVIDER}/${PRIMARY_MODEL} (high)`);
    const thinkingSelectValue = await waitForSelectValue(page, '[aria-label="Session thinking level"]', "high");
    await screenshotMatrix(page, "sessions-detail-thinking-high", {
      prepareViewport: async (page, { viewportName }) => {
        if (viewportName === "phone" || viewportName === "compact") {
          await openSessionDetailSettings(page);
        }
      },
    });

    await clickButtonByLabel(page, "Edit provider/model");
    await waitForText(page, '[role="dialog"]', "Edit inference settings");
    const providerDialogThinking = await waitForLabeledSelectValue(page, "Thinking level", "high");
    await screenshotMatrix(page, "sessions-provider-runtime-thinking-dialog");
    await clickRuntimeProviderOption(page, SECONDARY_PROVIDER);
    await clickButtonByText(page, "Save inference settings");
    const providerRuntime = await waitForText(page, '[aria-label="Session status summary"]', `Provider ${SECONDARY_PROVIDER}/${SECONDARY_MODEL}`);
    await screenshotMatrix(page, "sessions-detail-created");

    await page.locator('.shell__nav a[href="/sessions"]').click();
    await waitForPathname(page, "/sessions");
    const listedSession = await waitForText(page, '[aria-label="Session list"]', sessionId);
    await screenshotMatrix(page, "sessions-list-with-session");

    await page.locator(`button[aria-label="Open session ${sessionId}"]`).click();
    await waitForPathname(page, `/sessions/${encodeURIComponent(sessionId)}`);
    const reopenedDetail = await waitForText(page, '[aria-label="Session status summary"]', `${SECONDARY_PROVIDER}/${SECONDARY_MODEL}`);

    await clickButtonByLabel(page, `Delete session ${sessionId}`);
    await screenshotMatrix(page, "sessions-delete-confirmation", {
      prepareViewport: async (page, { viewportName }) => {
        if (viewportName === "phone") {
          await openSessionDetailSettings(page);
          await ensureResponsiveDeleteConfirmation(page, sessionId, true);
        }
        if (viewportName === "compact") {
          await openSessionDetailSettings(page);
          await ensureResponsiveDeleteConfirmation(page, sessionId, false);
        }
      },
    });
    await page.locator(`[aria-label="Delete session ${sessionId} confirmation"]`).waitFor();
    await clickButtonByLabel(page, `Confirm delete session ${sessionId}`);
    await waitForPathname(page, "/sessions");
    const listAfterDelete = await waitForMissingText(page, ".sessions__list-panel", sessionId);

    await createSession(page);
    await waitForText(page, '[aria-label="Session detail"]', "");
    const wipeSessionPathname = await page.evaluate(() => window.location.pathname);
    const wipeSessionId = decodeURIComponent(wipeSessionPathname.replace(/^\/sessions\//, ""));
    await page.locator('.shell__nav a[href="/sessions"]').click();
    await waitForPathname(page, "/sessions");
    await page.locator('a[href="/sessions/data"]').click();
    await waitForPathname(page, "/sessions/data");
    const wipeCopy = await waitForText(page, ".sessions__data-panel", "does not affect configuration, authentication, OAuth credentials, or MCP/Waltr SQL state");
    await clickButtonByText(page, "Delete all sessions");
    await screenshotMatrix(page, "sessions-wipe-confirmation");
    await clickButtonByLabel(page, "Confirm delete all sessions");
    const wipeSuccess = await waitForText(page, ".sessions__data-panel", "All agent sessions deleted.");
    await waitForPathname(page, "/sessions/data");
    await page.locator('.shell__nav a[href="/sessions"]').click();
    await waitForPathname(page, "/sessions");
    const listAfterWipe = await waitForMissingText(page, ".sessions__list-panel", wipeSessionId);

    check(null, {
      "session detail route reached": () => createdPathname.startsWith("/sessions/") && sessionId.length > 0,
      "session detail shows empty timeline": () => includesText(detailTimeline, "No messages yet."),
      "session detail shows empty context": () => includesText(sessionContext, "Usage Ctx 0 Cache 0% / 0% Cost - Subtasks No subtasks. Routines No Routine invocations."),
      "session detail fills the scroll viewport": () => detailFillsShell,
      "mobile context starts collapsed": () => mobileGeometry?.contextOpen === false,
      "mobile detail avoids viewport overflow": () => mobileGeometry?.shellWithinViewport === true && mobileGeometry.horizontalOverflow <= 1,
      "mobile composer remains reachable": () => mobileGeometry?.composerReachable === true,
      "session resource search shows suggestions": () => includesText(resourceSuggestions, "read"),
      "session resource search fills composer": () => resourceFilled.startsWith("@tool(") && resourceFilled !== "@read",
      "session thinking select is enabled": () => thinkingSelectEnabled,
      "session thinking runtime updates": () => includesText(thinkingRuntime, `${PRIMARY_PROVIDER}/${PRIMARY_MODEL} (high)`),
      "session thinking select persists update": () => thinkingSelectValue === "high",
      "provider dialog shows selected thinking": () => providerDialogThinking === "high",
      "session detail edits inference settings": () => includesText(providerRuntime, `${SECONDARY_PROVIDER}/${SECONDARY_MODEL}`),
      "session list contains created session": () => includesText(listedSession, sessionId),
      "session selection reopens detail": () => includesText(reopenedDetail, `${SECONDARY_PROVIDER}/${SECONDARY_MODEL}`),
      "session list omits deleted session": () => !includesText(listAfterDelete, sessionId),
      "session wipe target created": () => wipeSessionPathname.startsWith("/sessions/") && wipeSessionId.length > 0,
      "session wipe copy preserves unrelated state": () => includesText(wipeCopy, "configuration") && includesText(wipeCopy, "OAuth credentials"),
      "session wipe succeeds in place": () => includesText(wipeSuccess, "All agent sessions deleted."),
      "session list omits wiped session": () => !includesText(listAfterWipe, wipeSessionId),
    });
  });
}

async function createSession(page) {
  await page.locator('.sessions__header button').click();
  const dialog = page.locator('[role="dialog"][aria-label="Create session dialog"]');
  await dialog.waitFor();
  await dialog.locator('button[type="submit"]').click();
}

async function openSessionDetailSettings(page) {
  const settings = page.locator(".sessions__detail-settings");
  await settings.waitFor();
  const open = await page.evaluate(() => {
    const details = document.querySelector(".sessions__detail-settings");
    if (!(details instanceof HTMLDetailsElement)) {
      throw new Error("Session settings disclosure not found");
    }
    return details.open;
  });

  if (!open) {
    await settings.locator("summary").click();
  }
  await page.locator(".sessions__detail-settings[open]").waitFor();
}

async function ensureResponsiveDeleteConfirmation(page, sessionId, activate) {
  const confirmationSelector = `[aria-label="Delete session ${sessionId} confirmation"]`;
  const confirmation = page.locator(confirmationSelector);

  if (await confirmation.count() === 0) {
    if (!activate) {
      throw new Error(`Responsive delete confirmation was not preserved for ${sessionId}`);
    }
    await page
      .locator(`.sessions__detail-settings button[aria-label="Delete session ${sessionId}"]`)
      .click();
  }

  await confirmation.waitFor();
  await page.locator(`button[aria-label="Confirm delete session ${sessionId}"]`).waitFor();
}

async function waitForTextareaValueChange(page, selector, previousValue) {
  const deadline = Date.now() + 10_000;
  let value = "";

  while (Date.now() < deadline) {
    value = await page.evaluate((inputSelector) => {
      const input = document.querySelector(inputSelector);
      if (!(input instanceof HTMLTextAreaElement)) {
        throw new Error(`Textarea not found: ${inputSelector}`);
      }

      return input.value;
    }, selector);

    if (value && value !== previousValue) {
      return value;
    }

    sleep(0.25);
  }

  throw new Error(`Timed out waiting for ${selector} value to change from ${previousValue}; saw ${value}`);
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
