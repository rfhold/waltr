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
export const responsiveViewports = Object.freeze({
  desktop: Object.freeze({ width: 1440, height: 900 }),
  tablet: Object.freeze({ width: 768, height: 1024 }),
  phone: Object.freeze({ width: 390, height: 844 }),
  compact: Object.freeze({ width: 320, height: 700 }),
});
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

export async function screenshotMatrix(page, name, { prepareViewport } = {}) {
  if (!screenshotsEnabled) {
    return;
  }

  await page.evaluate(() => {
    const id = "waltr-screenshot-stability";
    if (document.getElementById(id)) return;
    const style = document.createElement("style");
    style.id = id;
    style.textContent = "* { caret-color: transparent !important; }";
    document.head.append(style);
  });

  try {
    for (const [viewportName, viewport] of Object.entries(responsiveViewports)) {
      await setResponsiveViewport(page, viewport);
      await prepareViewport?.(page, { viewportName, viewport });
      await screenshot(page, `${name}--${viewportName}`);
    }
  } finally {
    await setResponsiveViewport(page, responsiveViewports.desktop);
  }
}

export async function setResponsiveViewport(page, viewport) {
  await page.setViewportSize(viewport);
  await page.evaluate(() => new Promise((resolve) => {
    requestAnimationFrame(() => requestAnimationFrame(resolve));
  }));
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
