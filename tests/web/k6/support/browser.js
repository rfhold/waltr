import { browser } from "k6/browser";
import { check } from "k6";

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

export async function withPage(test) {
  const page = await browser.newPage();

  try {
    await test(page);
  } catch (error) {
    check(null, {
      "browser smoke completed": () => false,
    });
    throw error;
  } finally {
    await page.close();
  }
}
