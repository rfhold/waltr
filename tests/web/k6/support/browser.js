import { browser } from "k6/browser";
import { check } from "k6";

import { signInToWaltr } from "./access-auth.js";

export const options = {
  scenarios: {
    browser: {
      executor: "shared-iterations",
      vus: 1,
      iterations: 1,
      options: {
        browser: {
          type: "chromium",
        },
      },
    },
  },
  thresholds: {
    checks: ["rate==1.0"],
  },
};

export const baseUrl = __ENV.WALTR_WEB_BASE_URL ?? "http://127.0.0.1:5173";
export const directConfigUrl = `${baseUrl.replace(/\/$/, "")}/config`;
const screenshotsEnabled = __ENV.WALTR_WEB_SCREENSHOTS === "1";
const screenshotsDir = __ENV.WALTR_WEB_SCREENSHOTS_DIR ?? "/screenshots";

export async function withPage(test, { authenticate = true } = {}) {
  const page = await browser.newPage();
  let testError;

  try {
    if (authenticate && __ENV.WALTR_WEB_ACCESS_AUTH === "oidc") {
      await signInToWaltr(page, baseUrl);
    }
    await test(page);
  } catch (error) {
    testError = error;
    try {
      await screenshot(page, "browser-smoke-failed");
    } catch (screenshotError) {
      console.warn(`failed to capture browser failure screenshot: ${screenshotError}`);
    }
    check(null, {
      "browser smoke completed": () => false,
    });
  }

  try {
    await page.close();
  } catch (closeError) {
    if (!testError) {
      throw closeError;
    }
    console.warn(`failed to close browser page after test failure: ${closeError}`);
  }

  if (testError) {
    throw testError;
  }
}

export async function screenshot(page, name) {
  if (!screenshotsEnabled) {
    return;
  }

  await page.screenshot({
    fullPage: true,
    path: `${screenshotsDir}/${sanitizeScreenshotName(name)}.png`,
  });
}

function sanitizeScreenshotName(name) {
  return name.toLowerCase().replaceAll(/[^a-z0-9-]+/g, "-").replaceAll(/^-|-$/g, "");
}
