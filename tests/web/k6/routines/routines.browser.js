import { check, sleep } from "k6";

import {
  baseUrl,
  options as browserOptions,
  responsiveViewports,
  screenshotMatrix,
  setResponsiveViewport,
  withPage,
} from "../support/browser.js";
import { typeAndKeepFocus, waitForPathname, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

const ROUTINE_NAME = "screenshot-metadata";
const ROUTINE_DISPLAY_NAME = "Prepare screenshot metadata";

export default async function () {
  await withPage(async (page) => {
    await setResponsiveViewport(page, responsiveViewports.desktop);
    await page.goto(`${baseUrl}/sessions`, { waitUntil: "domcontentloaded" });
    await waitForPathname(page, "/sessions");
    await waitForText(page, "#sessions-title", "Sessions");

    await createSession(page);
    await waitForText(page, '[aria-label="Session detail"]', "");

    await page.keyboard.press("Control+P");
    await waitForText(page, '[role="dialog"]', "Session commands");
    await typeAndKeepFocus(page, "#session-command-search", ROUTINE_DISPLAY_NAME);
    await waitForText(page, '[aria-label="Session command list"]', ROUTINE_DISPLAY_NAME);
    await page.locator('[aria-label="Session command list"] button').click();
    await waitForText(page, '[role="dialog"]', ROUTINE_DISPLAY_NAME);
    await page.locator('form[aria-label="Prepare screenshot metadata command"] button[type="submit"]').click();
    await waitForText(page, '[role="dialog"]', "Command admitted.");
    await page.locator(".sessions__commands-result .sessions__commands-actions button:first-child").click();

    const detailPath = await waitForRoutineCompletion(page);
    const detailText = await waitForText(page, '[aria-label="Routine invocation detail"]', "layout-verified");
    const detailHasList = await page.locator("#routine-definitions-title").count();
    const collapsed = await disclosureState(page);
    await normalizeScreenshotValues(page);
    await screenshotMatrix(page, "routines-detail-collapsed");

    await page.locator(".routine-task:first-child summary").click();
    const expanded = await disclosureState(page);
    await waitForText(page, '[aria-label="mark-screenshot-ready results"]', "true");
    await normalizeScreenshotValues(page);
    await screenshotMatrix(page, "routines-detail-expanded");

    await page.locator(".routine-task:first-child summary").click();
    await setResponsiveViewport(page, responsiveViewports.phone);
    const mobileCollapsed = await disclosureState(page);
    await setResponsiveViewport(page, responsiveViewports.desktop);

    await page.locator(".routine-invocation__back").click();
    await waitForPathname(page, "/routines");
    await waitForText(page, ".routine-definitions", ROUTINE_DISPLAY_NAME);
    const listText = await waitForText(page, '[aria-label="Routine invocation history"]', ROUTINE_NAME);
    const listHasDetail = await page.locator('[aria-label="Routine invocation detail"]').count();
    await normalizeScreenshotValues(page);
    await screenshotMatrix(page, "routines-list");

    check(null, {
      "Routine View invocation reaches detail-only route": () => detailPath.startsWith("/routines/") && detailHasList === 0,
      "Routine completes with deterministic task results": () => includesText(detailText, "Succeeded") && includesText(detailText, "layout-verified"),
      "Routine tasks are initially collapsed": () => collapsed === false,
      "Routine task summary expands one task": () => expanded === true,
      "Routine mobile detail returns to collapsed state": () => mobileCollapsed === false,
      "Routine list-only route contains succeeded invocation": () => includesText(listText, "Succeeded") && listHasDetail === 0,
    });
  });
}

async function createSession(page) {
  await page.locator(".sessions__header button").click();
  const dialog = page.locator('[role="dialog"][aria-label="Create session dialog"]');
  await dialog.waitFor();
  await dialog.locator('button[type="submit"]').click();
}

async function waitForRoutineCompletion(page) {
  const deadline = Date.now() + 20_000;
  let pathname = "";
  let text = "";

  while (Date.now() < deadline) {
    pathname = await page.evaluate(() => window.location.pathname);
    if (pathname.startsWith("/routines/")) {
      text = await page.locator('[aria-label="Routine invocation detail"]').textContent().catch(() => "");
      if (includesText(text, "Succeeded") && includesText(text, "mark-layout-verified")) return pathname;
    }
    sleep(0.25);
  }

  throw new Error(`Timed out waiting for completed Routine at ${pathname}; saw ${text}`);
}

async function disclosureState(page) {
  return page.evaluate(() => Boolean(document.querySelector(".routine-task:first-child details")?.open));
}

async function normalizeScreenshotValues(page) {
  await page.evaluate(() => {
    for (const row of document.querySelectorAll(".routine-detail__metadata > div")) {
      const label = row.querySelector("dt")?.textContent;
      const value = row.querySelector("dd");
      if (!value) continue;
      if (label === "Invocation") value.textContent = "rinv_screenshot";
      if (label === "Source session") {
        const link = value.querySelector("a");
        if (link) link.textContent = "session_screenshot";
      }
    }
    for (const timestamp of document.querySelectorAll(".routine-history time")) {
      timestamp.textContent = "8/21/2026, 12:00:00 AM";
    }
  });
}
