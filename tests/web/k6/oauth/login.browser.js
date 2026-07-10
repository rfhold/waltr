import { check } from "k6";

import { options as browserOptions, withPage } from "../support/browser.js";
import { completeDexLogin, dexAuthorizeUrl } from "../support/oauth-login.js";

export const options = browserOptions;

export default async function () {
  const authUrl = __ENV.WALTR_OAUTH_AUTH_URL || dexAuthorizeUrl({
    issuer: __ENV.WALTR_OAUTH_ISSUER || "http://dex:5556/dex",
    clientId: __ENV.WALTR_OAUTH_CLIENT_ID || "waltr-test",
    redirectUri: __ENV.WALTR_OAUTH_REDIRECT_URI || "http://127.0.0.1/oauth/callback",
  });
  const username = __ENV.WALTR_OAUTH_USERNAME || "waltr@example.test";
  const password = __ENV.WALTR_OAUTH_PASSWORD || "password";

  await withPage(async (page) => {
    const redirectUrl = await completeDexLogin(page, { authUrl, username, password });
    console.log(`WALTR_OAUTH_REDIRECT_URL=${redirectUrl}`);

    check(redirectUrl, {
      "oauth login returns authorization code": (url) => url.includes("code=") && url.includes("state="),
    });
  });
}
