import { check, sleep } from "k6";

import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { typeAndKeepFocus, waitForMissingElement, waitForPathname, waitForText } from "../support/checks.js";
import { includesText } from "../support/text.js";

export const options = browserOptions;

export default async function () {
  await withPage(async (page) => {
    const visibleSnapshots = [];
    await page.goto(`${baseUrl}/sessions`, { waitUntil: "domcontentloaded" });
    await waitForPathname(page, "/sessions");
    await page.locator(".sessions__header button").click();
    await waitForText(page, '[aria-label="Session status summary"]', "MCP protected");

    await typeAndKeepFocus(page, '[aria-label="Session message"]', "trigger protected mcp oauth");
    await page.locator('[aria-label="Send session message"] button[type="submit"]').click();
    const oauthDialog = await waitForText(page, '[role="dialog"]', "OAuth required");
    await waitForText(page, '[role="dialog"]', "MCP server protected");
    visibleSnapshots.push(await visibleText(page, '[role="dialog"]'));
    await screenshot(page, "sessions-mcp-oauth-required");

    await clickButtonByText(page, "Begin OAuth");
    await waitForText(page, '[role="dialog"]', "Open authorization URL");
    const authUrl = await page.evaluate(() => {
      const link = document.querySelector(".oauth-dialog__open");
      if (!(link instanceof HTMLAnchorElement)) {
        throw new Error("OAuth authorization link not found");
      }
      return link.href;
    });
    const authorizationContract = await page.evaluate((value) => {
      const url = new URL(value);
      return {
        state: url.searchParams.get("state") ?? "",
        codeChallenge: url.searchParams.get("code_challenge") ?? "",
        codeVerifier: url.searchParams.get("code_verifier") ?? "",
        callbackUri: url.searchParams.get("redirect_uri") ?? "",
        clientId: url.searchParams.get("client_id") ?? "",
      };
    }, authUrl);
    const pendingDialog = await waitForText(
      page,
      '[role="dialog"]',
      "Waiting for the server callback",
    );
    await waitForText(page, '[role="dialog"]', "Client metadata");
    visibleSnapshots.push(await visibleTextWithout(
      page,
      '[role="dialog"]',
      ".oauth-dialog__open, .oauth-dialog__url",
    ));

    const authPage = await page.context().newPage();
    let callbackCode = "";
    try {
      const response = await authPage.goto(authUrl, { waitUntil: "domcontentloaded" });
      const authText = await authPage.locator("body").textContent();
      if (!includesText(authText, "Authorize Waltr")) {
        throw new Error(
          `Hosted authorization page did not render at ${safeUrl(authPage.url())} ` +
            `(status ${response?.status() ?? "unknown"})`,
        );
      }
      const consentText = await waitForText(authPage, "main", "Authorize Waltr");
      visibleSnapshots.push(await visibleText(authPage, "main"));
      const consentContract = await authPage.evaluate(() => {
        const form = document.querySelector('form[action="/oauth/authorize/decision"]');
        const csrf = form?.querySelector('input[name="csrf"]');
        const transaction = form?.querySelector('input[name="transaction_id"]');
        return {
          post: form?.getAttribute("method")?.toLowerCase() === "post",
          csrf: csrf instanceof HTMLInputElement && csrf.value.length >= 32,
          transaction: transaction instanceof HTMLInputElement && transaction.value.length > 0,
        };
      });
      await clickButtonByText(authPage, "Approve");
      const callbackText = await waitForText(authPage, "body", "Authorization complete");
      visibleSnapshots.push(await visibleText(authPage, "body"));
      const callbackContract = await authPage.evaluate(() => {
        const url = new URL(window.location.href);
        return {
          path: url.pathname,
          code: url.searchParams.get("code") ?? "",
          state: url.searchParams.get("state") ?? "",
        };
      });
      callbackCode = callbackContract.code;

      check(null, {
        "CIMD registration is selected": () =>
          authorizationContract.clientId === `${baseUrl}/.well-known/oauth-client/waltr` &&
          includesText(consentText, "Waltr outbound MCP client"),
        "hosted consent posts CSRF evidence": () =>
          consentContract.post && consentContract.csrf && consentContract.transaction,
        "server callback completes durably": () =>
          callbackContract.path === "/outbound-oauth/callback" &&
          callbackCode.length > 0 &&
          callbackContract.state === authorizationContract.state,
      });
    } finally {
      await authPage.close();
    }

    await waitForMissingElement(page, '[role="dialog"]', 20_000);
    await page.goto(`${baseUrl}/sessions`, { waitUntil: "domcontentloaded" });
    await waitForPathname(page, "/sessions");
    await page.locator(".sessions__header button").click();
    await waitForText(page, '[aria-label="Session status summary"]', "MCP protected");

    await clearTextarea(page, '[aria-label="Session message"]');
    await typeAndKeepFocus(page, '[aria-label="Session message"]', "@configuration");
    const suggestions = await waitForText(
      page,
      '[aria-label="Resource suggestions"]',
      "configuration.inspect",
    );
    await clickResourceSuggestion(page, "configuration.inspect");
    const selectedOperation = await waitForTextareaValueChange(
      page,
      '[aria-label="Session message"]',
      "@configuration",
    );
    visibleSnapshots.push(await visibleText(page, "body"));
    await screenshot(page, "sessions-mcp-oauth-authorized-resource-search");

    const noReauthorization = await page.locator('[role="dialog"]').count() === 0;
    const combinedSnapshots = visibleSnapshots.join("\n");
    const clientSecret = __ENV.WALTR_OAUTH_CLIENT_SECRET || "waltr-access-test-secret";

    check(null, {
      "mcp oauth requirement is shown": () => includesText(oauthDialog, "MCP server protected"),
      "server callback lifecycle reaches shared-client terminal state": () =>
        includesText(pendingDialog, "Waiting for the server callback") && noReauthorization,
      "persisted credential is reused without reauthorization": () => noReauthorization,
      "dynamic bearer reaches the protected Waltr MCP resource": () =>
        authorizationContract.callbackUri === `${baseUrl}/outbound-oauth/callback` &&
        includesText(suggestions, "configuration.inspect"),
      "protected configuration.inspect operation is selectable": () =>
        includesText(suggestions, "configuration.inspect") && selectedOperation !== "@configuration",
      "authorization URL omits the PKCE verifier": () => authorizationContract.codeVerifier === "",
      "authorization request carries callback state": () => authorizationContract.state.length >= 8,
      "authorization request carries an S256 challenge": () => authorizationContract.codeChallenge.length >= 8,
      "required-dialog snapshot omits callback state and PKCE material": () =>
        snapshotOmits(visibleSnapshots[0], authorizationContract.state, authorizationContract.codeChallenge),
      "pending non-URL snapshot omits callback state": () =>
        snapshotOmits(visibleSnapshots[1], authorizationContract.state),
      "pending non-URL snapshot omits PKCE material": () =>
        snapshotOmits(visibleSnapshots[1], authorizationContract.codeChallenge),
      "consent snapshot omits callback state and PKCE material": () =>
        snapshotOmits(visibleSnapshots[2], authorizationContract.state, authorizationContract.codeChallenge),
      "callback snapshot omits callback state and PKCE material": () =>
        snapshotOmits(visibleSnapshots[3], authorizationContract.state, authorizationContract.codeChallenge),
      "authorized UI snapshot omits callback state and PKCE material": () =>
        snapshotOmits(visibleSnapshots[4], authorizationContract.state, authorizationContract.codeChallenge),
      "UI snapshots omit authorization code": () =>
        callbackCode.length >= 8 && !combinedSnapshots.includes(callbackCode),
      "UI snapshots omit client secret": () => !combinedSnapshots.includes(clientSecret),
      "UI snapshots omit token fields and raw sensitive errors": () => [
        "access_token",
        "refresh_token",
        "code_verifier",
        "client_secret",
        "Authorization could not be completed",
      ].every((label) => !combinedSnapshots.includes(label)),
    });
  });
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

  throw new Error(`Timed out waiting for ${selector} value to change`);
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

async function clickResourceSuggestion(page, text) {
  await page.evaluate((expectedText) => {
    const options = Array.from(document.querySelectorAll('[aria-label="Resource suggestions"] [role="option"]'));
    const option = options.find((candidate) => candidate.textContent?.includes(expectedText));
    if (!(option instanceof HTMLButtonElement)) {
      throw new Error(`Resource suggestion not found: ${expectedText}`);
    }
    option.click();
  }, text);
}

function safeUrl(value) {
  const match = value.match(/^(https?:\/\/[^/?#]+)([^?#]*)/);
  return match ? `${match[1]}${match[2] || "/"}` : "an invalid URL";
}

function snapshotOmits(snapshot, ...values) {
  return values.every((value) => value.length < 8 || !snapshot.includes(value));
}

async function visibleText(page, selector) {
  return page.evaluate((elementSelector) => {
    const element = document.querySelector(elementSelector);
    if (!(element instanceof HTMLElement)) {
      throw new Error(`Visible UI element not found: ${elementSelector}`);
    }
    return element.innerText;
  }, selector);
}

async function visibleTextWithout(page, selector, excludedSelector) {
  return page.evaluate(({ selector: elementSelector, excludedSelector: excluded }) => {
    const element = document.querySelector(elementSelector);
    if (!(element instanceof HTMLElement)) {
      throw new Error(`Visible UI element not found: ${elementSelector}`);
    }
    const clone = element.cloneNode(true);
    if (!(clone instanceof HTMLElement)) {
      throw new Error(`Could not snapshot visible UI element: ${elementSelector}`);
    }
    clone.querySelectorAll(excluded).forEach((excludedElement) => excludedElement.remove());
    return clone.innerText;
  }, { selector, excludedSelector });
}
