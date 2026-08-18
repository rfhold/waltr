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
    await createSession(page);

    const protectedPrompt = "trigger protected mcp oauth";
    await typeAndKeepFocus(page, '[aria-label="Session message"]', protectedPrompt);
    const sessionUrl = page.url();
    const sessionPath = await page.evaluate(() => window.location.pathname);
    await page.locator('[aria-label="Send session message"] button[type="submit"]').click();
    const authenticationFailure = await waitForText(
      page,
      ".sessions__auth-failure",
      "Authentication failed for MCP server protected",
    );
    await waitForText(page, ".sessions__auth-failure", "Open MCP server Config");
    const retainedDraft = await textareaValue(page, '[aria-label="Session message"]');
    const noRuntimeDialog = await page.locator('.oauth-dialog').count() === 0;
    visibleSnapshots.push(await visibleText(page, ".sessions__auth-failure"));
    await screenshot(page, "sessions-mcp-oauth-required");

    await page.locator('.sessions__auth-failure a').click();
    await waitForPathname(page, "/config/mcp/edit/protected");
    await waitForText(page, '[aria-label="Authentication for protected"]', "Authentication");
    await clickAuthorizationButton(page, '[aria-label="Authentication for protected"]');
    await waitForText(page, '.oauth-dialog', "Open authorization URL");
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
      '.oauth-dialog',
      "Waiting for the server callback",
    );
    await waitForText(page, '.oauth-dialog', "Client metadata");
    visibleSnapshots.push(await visibleTextWithout(
      page,
      '.oauth-dialog',
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

    await waitForMissingElement(page, '.oauth-dialog', 20_000);
    await page.goto(`${baseUrl}/sessions`, { waitUntil: "domcontentloaded" });
    await waitForPathname(page, "/sessions");
    const sessionId = sessionPath.split("/").at(-1);
    await page.locator(`[aria-label="Open session ${sessionId}"]`).click();
    await waitForPathname(page, sessionPath);
    await typeAndKeepFocus(page, '[aria-label="Session message"]', protectedPrompt);
    await page.locator('[aria-label="Send session message"] button[type="submit"]').click();
    await waitForTextareaValue(page, '[aria-label="Session message"]', "");
    const noRetryDialog = await page.locator('.oauth-dialog').count() === 0;

    await page.goto(`${baseUrl}/sessions`, { waitUntil: "domcontentloaded" });
    await waitForPathname(page, "/sessions");
    await createSession(page);
    await clearTextarea(page, '[aria-label="Session message"]');
    await typeAndKeepFocus(page, '[aria-label="Session message"]', "@resources");
    const suggestions = await waitForText(
      page,
      '[aria-label="Resource suggestions"]',
      "mcp_protected_exec",
    );
    await clickResourceSuggestion(page, "mcp_protected_exec");
    const selectedOperation = await waitForTextareaValueChange(
      page,
      '[aria-label="Session message"]',
      "@resources",
    );
    visibleSnapshots.push(await visibleText(page, "body"));
    await screenshot(page, "sessions-mcp-oauth-authorized-resource-search");

    const noReauthorization = await page.locator('.oauth-dialog').count() === 0;
    const combinedSnapshots = visibleSnapshots.join("\n");
    const clientSecret = __ENV.WALTR_OAUTH_CLIENT_SECRET || "waltr-access-test-secret";

    check(null, {
      "MCP failure is inline and target scoped": () =>
        includesText(authenticationFailure, "Authentication failed for MCP server protected"),
      "MCP failure retains the draft without opening OAuth": () =>
        retainedDraft === protectedPrompt && noRuntimeDialog,
      "server callback lifecycle reaches shared-client terminal state": () =>
        includesText(pendingDialog, "Waiting for the server callback") && noReauthorization,
      "persisted credential is reused without reauthorization": () => noReauthorization,
      "explicit retry is admitted without another MCP auth blocker": () => noRetryDialog,
      "dynamic bearer reaches the protected Waltr MCP resource": () =>
        authorizationContract.callbackUri === `${baseUrl}/outbound-oauth/callback` &&
        includesText(suggestions, "mcp_protected_exec"),
      "protected Waltr dispatcher is selectable": () =>
        includesText(suggestions, "mcp_protected_exec") && selectedOperation !== "@resources",
      "authorization URL omits the PKCE verifier": () => authorizationContract.codeVerifier === "",
      "authorization request carries callback state": () => authorizationContract.state.length >= 8,
      "authorization request carries an S256 challenge": () => authorizationContract.codeChallenge.length >= 8,
      "inline failure snapshot omits callback state and PKCE material": () =>
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

async function createSession(page) {
  await page.locator('.sessions__header button').click();
  const dialog = page.locator('[role="dialog"][aria-label="Create session dialog"]');
  await dialog.waitFor();
  await dialog.locator('button[type="submit"]').click();
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

async function textareaValue(page, selector) {
  return page.evaluate((inputSelector) => {
    const input = document.querySelector(inputSelector);
    if (!(input instanceof HTMLTextAreaElement)) {
      throw new Error(`Textarea not found: ${inputSelector}`);
    }
    return input.value;
  }, selector);
}

async function waitForTextareaValue(page, selector, expectedValue) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    if (await textareaValue(page, selector) === expectedValue) return;
    sleep(0.25);
  }
  throw new Error(`Timed out waiting for ${selector} value ${JSON.stringify(expectedValue)}`);
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

async function clickAuthorizationButton(page, selector) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    const clicked = await page.evaluate((sectionSelector) => {
      const section = document.querySelector(sectionSelector);
      const button = Array.from(section?.querySelectorAll("button") ?? []).find((candidate) => {
        const text = candidate.textContent?.trim();
        return text === "Authorize" || text === "Reauthorize";
      });
      if (!(button instanceof HTMLButtonElement)) return false;
      button.click();
      return true;
    }, selector);
    if (clicked) return;
    sleep(0.25);
  }
  throw new Error(`Authorization action not found in ${selector}`);
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
