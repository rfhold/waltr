import { sleep } from "k6";
import http from "k6/http";

import { screenshot } from "./browser.js";

export async function completeDexLogin(page, { authUrl, username, password, failureScreenshot = "oauth-login-failed" }) {
  await page.goto(authUrl, { waitUntil: "domcontentloaded" });
  return completeDexLoginOnPage(page, { username, password, failureScreenshot });
}

export async function completeDexLoginOnPage(
  page,
  { username, password, failureScreenshot = "oauth-login-failed", isComplete },
) {
  await page.locator('input[name="login"]').waitFor({ timeout: 10_000 });
  await page.locator('input[name="login"]').type(username);
  await page.locator('input[name="password"]').type(password);
  await page.locator('button[type="submit"], input[type="submit"]').click();
  return await waitForCodeRedirect(page, failureScreenshot, isComplete);
}

export function completeDexLoginViaHttp({ authUrl, username, password }) {
  const loginUrl = followRedirects(authUrl);
  const loginResponse = http.post(loginUrl, { login: username, password }, { redirects: 0 });
  const redirectUrl = followRedirects(locationHeader(loginResponse), loginResponse);

  if (redirectUrl.includes("code=") && redirectUrl.includes("state=")) {
    return redirectUrl;
  }

  throw new Error(`Dex HTTP login did not reach authorization code redirect at ${redirectUrl}: ${loginResponse.body}`);
}

async function waitForCodeRedirect(page, failureScreenshot, isComplete) {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    const url = page.url();
    if ((url.includes("code=") && url.includes("state=")) || isComplete?.(url)) {
      return url;
    }

    sleep(0.25);
  }

  await screenshot(page, failureScreenshot);
  const bodyText = await page.locator("body").textContent().catch(() => "");
  throw new Error(`Dex login did not reach authorization code redirect at ${page.url()}: ${bodyText}`);
}

export function dexAuthorizeUrl({ issuer, clientId, redirectUri, scope = "openid email profile" }) {
  const url = new URL(`${issuer.replace(/\/$/, "")}/auth`);
  url.searchParams.set("client_id", clientId);
  url.searchParams.set("redirect_uri", redirectUri);
  url.searchParams.set("response_type", "code");
  url.searchParams.set("scope", scope);
  url.searchParams.set("state", "waltr-oauth-test");

  return url.toString();
}

function followRedirects(initialUrl, initialResponse = null) {
  let url = initialUrl;
  let response = initialResponse;

  for (let attempt = 0; attempt < 10; attempt += 1) {
    if (url.includes("code=") && url.includes("state=")) {
      return url;
    }

    response ??= http.get(url, { redirects: 0 });
    const location = locationHeader(response);
    if (!location) {
      return url;
    }

    url = resolveUrl(url, location);
    response = null;
  }

  return url;
}

function locationHeader(response) {
  return response.headers.Location ?? response.headers.location ?? "";
}

function resolveUrl(base, location) {
  if (location.startsWith("http://") || location.startsWith("https://")) {
    return location;
  }

  const origin = base.match(/^https?:\/\/[^/]+/)?.[0];
  if (!origin) {
    return location;
  }

  if (location.startsWith("/")) {
    return `${origin}${location}`;
  }

  return `${base.replace(/\/[^/]*$/, "/")}${location}`;
}
