import { check } from "k6";

import { baseUrl, options as browserOptions, withPage } from "../support/browser.js";
import { waitForMissingText, waitForPathname, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    await page.goto(baseUrl, { waitUntil: "domcontentloaded" });
    await waitForText(page, "#landing-title", "Waltr is ready.");

    await page.locator('.shell__nav a[href="/sessions"]').click();
    await waitForText(page, "#sessions-title", "Sessions");
    await waitForText(page, "#sessions-list-title", "Recent sessions");

    await page.locator(".sessions__header button").click();
    await waitForText(page, '[aria-label="Session breadcrumb"]', "Sessions");
    await waitForText(page, ".sessions__detail-card .session-status", "idle");

    const createdPathname = await page.evaluate(() => window.location.pathname);
    const sessionId = decodeURIComponent(createdPathname.replace(/^\/sessions\//, ""));
    const detailBreadcrumb = await waitForText(page, '[aria-label="Session breadcrumb"]', sessionId);
    const detailStatus = await waitForText(page, ".sessions__detail-card", "idle");

    await page.locator('.shell__nav a[href="/sessions"]').click();
    await waitForPathname(page, "/sessions");
    const listedSession = await waitForText(page, '[aria-label="Session list"]', sessionId);

    await page.locator(`button[aria-label="Open session ${sessionId}"]`).click();
    await waitForPathname(page, `/sessions/${encodeURIComponent(sessionId)}`);
    const reopenedBreadcrumb = await waitForText(page, '[aria-label="Session breadcrumb"]', sessionId);

    await page.locator(`.sessions__detail-card button[aria-label="Delete session ${sessionId}"]`).click();
    await page.locator(`.sessions__detail-card button[aria-label="Confirm delete session ${sessionId}"]`).click();
    await waitForPathname(page, "/sessions");
    const listAfterDelete = await waitForMissingText(page, ".sessions__list-panel", sessionId);

    check(null, {
      "session detail route reached": () => createdPathname.startsWith("/sessions/") && sessionId.length > 0,
      "session breadcrumb shows id": () => includesText(detailBreadcrumb, sessionId),
      "session detail shows status": () => includesText(detailStatus, "idle"),
      "session list contains created session": () => includesText(listedSession, sessionId),
      "session selection reopens detail": () => includesText(reopenedBreadcrumb, sessionId),
      "session list omits deleted session": () => !includesText(listAfterDelete, sessionId),
    });
  });
}
