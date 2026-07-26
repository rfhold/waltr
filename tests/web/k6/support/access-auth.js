import { check } from "k6";

import { gotoWithCertVerifierRetry, waitForText } from "./checks.js";
import { completeDexLoginOnPage } from "./oauth-login.js";

export async function signInToWaltr(page, baseUrl, returnPath = "/") {
  await gotoWithCertVerifierRetry(page, `${baseUrl.replace(/\/$/, "")}${returnPath}`, {
    waitUntil: "domcontentloaded",
  });
  await completeWaltrSignIn(page);
}

export async function completeWaltrSignIn(page) {
  if (await page.locator("#access-title").count() > 0) {
    await waitForText(page, "#access-title", "Sign in to continue");
    await page.locator("a.access__login").click();
  }
  await completeDexLoginOnPage(page, {
    username: __ENV.WALTR_OAUTH_USERNAME || "waltr@example.test",
    password: __ENV.WALTR_OAUTH_PASSWORD || "password",
    failureScreenshot: "waltr-access-login-failed",
    isComplete: (url) => url.startsWith(__ENV.WALTR_WEB_BASE_URL),
  });
  await waitForText(page, '[aria-label="Primary navigation"]', "Waltr");

  const authenticated = await accessSessionAuthenticated(page);
  check(authenticated, {
    "Waltr access session is authenticated": (value) => value,
  });
  if (!authenticated) {
    throw new Error("Waltr access login completed without an authenticated session");
  }
}

export async function accessSessionAuthenticated(page) {
  return page.evaluate(async () => {
    const response = await fetch("/oidc/session", { credentials: "same-origin" });
    if (!response.ok) {
      throw new Error(`OIDC session endpoint returned ${response.status}`);
    }
    const session = await response.json();
    return session.authenticated === true;
  });
}
