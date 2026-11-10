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
    await typeAndKeepFocus(page, '[role="dialog"] input', ROUTINE_NAME);
    const command = `form[aria-label="${ROUTINE_NAME} command"]`;
    await waitForText(page, command, `Start ${ROUTINE_NAME}`);
    await page.locator(`${command} button[type="submit"]`).click();
    await waitForText(page, command, "View invocation");
    await page.locator(`${command} a`).click();

    const detailPath = await waitForRoutineCompletion(page);
    const detailText = await waitForText(page, '[aria-label="Routine invocation detail"]', '"screenshotLayout":true');
    const detailHasList = await page.locator("#routines-title").count();
    const collapsed = await disclosureState(page);
    await normalizeScreenshotValues(page);
    await screenshotMatrix(page, "routines-detail-collapsed");

    await page.locator('[aria-label="Routine operations"] li:first-child summary').click();
    const expanded = await disclosureState(page);
    await waitForText(page, '[aria-label="Routine operations"] li:first-child details', '"operation":"context.snapshot"');
    await waitForText(page, '[aria-label="Routine operations"] li:first-child details', '"status":"completed"');
    await normalizeScreenshotValues(page);
    await screenshotMatrix(page, "routines-detail-expanded");

    await page.locator('[aria-label="Routine operations"] li:first-child summary').click();
    await setResponsiveViewport(page, responsiveViewports.phone);
    const mobileCollapsed = await disclosureState(page);
    await setResponsiveViewport(page, responsiveViewports.desktop);

    await page.locator(".routine-invocation__back").click();
    await waitForPathname(page, "/routines");
    await waitForText(page, ".routines", ROUTINE_NAME);
    const listText = await waitForText(page, '[aria-label="Routine invocation history"]', ROUTINE_NAME);
    const listHasDetail = await page.locator('[aria-label="Routine invocation detail"]').count();
    await normalizeScreenshotValues(page);
    await screenshotMatrix(page, "routines-list");

    check(null, {
      "Routine View invocation reaches detail-only route": () => detailPath.startsWith("/routines/") && detailHasList === 0,
      "Routine completes with deterministic metadata results": () => includesText(detailText, "Succeeded") && includesText(detailText, '"screenshotReady":true') && includesText(detailText, '"screenshotLayout":true'),
      "Routine operations are initially collapsed": () => collapsed === false,
      "Routine operation summary expands one operation": () => expanded === true,
      "Routine mobile detail returns to collapsed state": () => mobileCollapsed === false,
      "Routine list-only route contains succeeded invocation": () => includesText(listText, "Succeeded") && listHasDetail === 0,
    });
  });
}

async function createSession(page) {
  await page.locator(".sessions__header button").click();
  const dialog = page.locator('[role="dialog"][aria-label="Create session dialog"]');
  await dialog.waitFor();
  await dialog.locator('input[name="session-agent"][value="routine-metadata"]').check();
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
      if (includesText(text, "Succeeded") && includesText(text, "context.transact")) return pathname;
    }
    sleep(0.25);
  }

  throw new Error(`Timed out waiting for completed Routine at ${pathname}; saw ${text}`);
}

async function disclosureState(page) {
  return page.evaluate(() => Boolean(document.querySelector('[aria-label="Routine operations"] li:first-child details')?.open));
}

async function normalizeScreenshotValues(page) {
  await page.evaluate(() => {
    for (const term of document.querySelectorAll(".routine-detail > dl > dt")) {
      const label = term.textContent;
      const value = term.nextElementSibling;
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
