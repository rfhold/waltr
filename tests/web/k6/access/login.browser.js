import { check, sleep } from "k6";

import { completeWaltrSignIn } from "../support/access-auth.js";
import { baseUrl, options as browserOptions, screenshot, withPage } from "../support/browser.js";
import { gotoWithCertVerifierRetry, waitForPathname, waitForText } from "../support/checks.js";

export const options = browserOptions;

const STATUS_RECONNECT_OBSERVATION_SECONDS = 5.5;

export default async function () {
  await withPage(async (page) => {
    await gotoWithCertVerifierRetry(page, `${baseUrl}/config/provider?access=e2e`, {
      waitUntil: "domcontentloaded",
    });
    const redirectedToProvider = page.url().startsWith("https://dex.test/");
    await screenshot(page, "access-login-required");

    await completeWaltrSignIn(page);
    await waitForPathname(page, "/config/provider");
    await waitForText(page, "#config-title", "Settings");
    const protectedWatch = await startProtectedWatch(page);
    const returnedToProtectedRoute = page.url().endsWith("/config/provider?access=e2e");
    await screenshot(page, "access-authenticated");

    const authenticatedCookies = await page.context().cookies();
    const sessionCookie = authenticatedCookies.find((cookie) => cookie.name === "__Host-waltr-session");
    const logoutButtonPresent = await page.locator("button.shell__logout").count() === 1;
    const logoutUrl = `${baseUrl.replace(/\/$/, "")}/oidc/logout`;
    const statusRequestBaseline = await statusRequestCount(page);
    await page.locator("button.shell__logout").click();
    const logoutResponse = await waitForLogoutResponse(page);
    const endedProtectedWatch = await waitForProtectedWatchEnd(page);
    const logoutProbe = await waitForDeniedLogoutProbe(page, statusRequestBaseline);
    sleep(STATUS_RECONNECT_OBSERVATION_SECONDS);
    const logoutStatusRequests = await statusRequestsFrom(page, statusRequestBaseline);
    const signedOutTitle = await waitForText(page, "#access-title", "Sign in to continue");
    const session = await page.evaluate(async () => {
      const response = await fetch("/oidc/session", { credentials: "same-origin" });
      return response.json();
    });
    const signedOutCookies = await page.context().cookies();
    const sessionCookiePresent = signedOutCookies.some((cookie) => cookie.name === "__Host-waltr-session");
    const navigationPresent = await page.locator("nav.shell__nav").count() !== 0;
    const logoutButtonStillPresent = await page.locator("button.shell__logout").count() !== 0;
    await screenshot(page, "access-logged-out");

    check({ redirectedToProvider, returnedToProtectedRoute, sessionCookie, protectedWatch, logoutButtonPresent, logoutResponse, endedProtectedWatch, logoutProbe, logoutStatusRequests, signedOutTitle, session, sessionCookiePresent, navigationPresent, logoutButtonStillPresent }, {
      "protected document redirects to the OIDC provider": ({ redirectedToProvider }) => redirectedToProvider,
      "OIDC callback restores the protected route": ({ returnedToProtectedRoute }) => returnedToProtectedRoute,
      "session cookie uses the __Host name and root path": ({ sessionCookie }) =>
        sessionCookie?.name === "__Host-waltr-session" && sessionCookie?.path === "/",
      "session cookie is Secure": ({ sessionCookie }) => sessionCookie?.secure === true,
      "session cookie is HttpOnly": ({ sessionCookie }) => sessionCookie?.httpOnly === true,
      "session cookie is SameSite Lax": ({ sessionCookie }) =>
        sessionCookie?.sameSite?.toLowerCase() === "lax",
      "protected gRPC-web watch is active before logout": ({ protectedWatch }) =>
        protectedWatch.initialMessage === true,
      "authenticated shell exposes logout": ({ logoutButtonPresent }) => logoutButtonPresent,
      "logout click posts successfully": ({ logoutResponse }) =>
        logoutResponse.url === logoutUrl && logoutResponse.status === 204,
      "logout ends the exact protected watch as unauthenticated": ({ endedProtectedWatch }) =>
        endedProtectedWatch.done === true
        && endedProtectedWatch.grpcStatus === 16
        && endedProtectedWatch.error === null,
      "successful logout is followed immediately by one denied anonymous status probe": ({ logoutResponse, logoutProbe }) =>
        logoutProbe.postLogoutRequestCount === 1
        && logoutProbe.request.sequence > logoutResponse.sequence
        && logoutProbe.request.responseSequence > logoutResponse.sequence
        && logoutProbe.request.logoutResponseSequence === logoutResponse.sequence
        && logoutProbe.request.protected === false
        && logoutProbe.request.responseUrl === `${baseUrl.replace(/\/$/, "")}/waltr.status.v1.StatusService/WatchStatus`
        && logoutProbe.request.httpStatus === 200
        && logoutProbe.request.grpcStatus === 16
        && logoutProbe.request.grpcMessage.includes("Waltr access authentication required"),
      "the complete logout status suffix contains only the denied anonymous probe": ({ logoutStatusRequests }) =>
        logoutStatusRequests.length === 1
        && logoutStatusRequests[0].protected === false
        && logoutStatusRequests[0].grpcStatus === 16,
      "logout renders the signed-out route": ({ signedOutTitle }) => signedOutTitle.includes("Sign in to continue"),
      "logout removes authenticated navigation": ({ navigationPresent }) => !navigationPresent,
      "logout removes the Sign out action": ({ logoutButtonStillPresent }) => !logoutButtonStillPresent,
      "logout clears the Waltr session cookie": ({ sessionCookiePresent }) => !sessionCookiePresent,
      "logout revokes the Waltr access session": ({ session }) => session.authenticated === false,
    });
  }, { authenticate: false });
}

async function startProtectedWatch(page) {
  try {
    await page.evaluate(() => {
      window.__waltrProtectedWatch = {
        chunks: [],
        done: false,
        error: null,
        logoutResponse: null,
        sequence: 0,
        statusRequests: [],
      };
      window.__waltrOriginalFetch = window.fetch.bind(window);
    });
  } catch (error) {
    throw new Error(`protected watch setup failed: ${error}`);
  }
  try {
    await page.evaluate(() => {
      const watchPath = "/waltr.status.v1.StatusService/WatchStatus";
      const state = window.__waltrProtectedWatch;
      const originalFetch = window.__waltrOriginalFetch;
      window.fetch = (input, init) => {
        const rawUrl = typeof input === "string" ? input : input.url;
        const url = new URL(rawUrl, window.location.href);
        const inputMethod = typeof input === "string" ? "GET" : input.method;
        const method = String((init && init.method) || inputMethod).toUpperCase();
        const headers = new Headers((init && init.headers) || (typeof input === "string" ? undefined : input.headers));
        const isStatusWatch = method === "POST"
          && url.href === new URL(watchPath, window.location.origin).href;
        let statusRequest;
        if (isStatusWatch) {
          statusRequest = {
            grpcMessage: "",
            grpcStatus: null,
            httpStatus: null,
            logoutResponseSequence: state.logoutResponse?.status === 204
              ? state.logoutResponse.sequence
              : null,
            protected: headers.get("x-waltr-test-watch") === "protected",
            responseUrl: null,
            responseSequence: null,
            sequence: ++state.sequence,
          };
          state.statusRequests.push(statusRequest);
        }
        const response = originalFetch(input, init);
        if (method === "POST" && url.href === new URL("/oidc/logout", window.location.origin).href) {
          response.then((result) => {
            state.logoutResponse = {
              sequence: ++state.sequence,
              status: result.status,
              url: result.url,
            };
          });
        }
        if (!statusRequest) return response;
        return response.then((result) => {
          statusRequest.responseSequence = ++state.sequence;
          statusRequest.httpStatus = result.status;
          statusRequest.responseUrl = result.url;
          const grpcStatus = result.headers.get("grpc-status");
          if (grpcStatus !== null) statusRequest.grpcStatus = Number(grpcStatus);
          const grpcMessage = result.headers.get("grpc-message");
          if (grpcMessage !== null) {
            try {
              statusRequest.grpcMessage = decodeURIComponent(grpcMessage);
            } catch {
              statusRequest.grpcMessage = grpcMessage;
            }
          }
          return result;
        });
      };
    });
  } catch (error) {
    throw new Error(`protected watch fetch instrumentation failed: ${error}`);
  }
  try {
    await page.evaluate(() => {
      const state = window.__waltrProtectedWatch;
      window.fetch("/waltr.status.v1.StatusService/WatchStatus", {
        method: "POST",
        credentials: "same-origin",
        headers: {
          "accept": "application/grpc-web+proto",
          "content-type": "application/grpc-web+proto",
          "x-waltr-test-watch": "protected",
          "x-grpc-web": "1",
        },
        body: new Uint8Array([0, 0, 0, 0, 0]),
      }).then((response) => {
        if (!response.ok || !response.body) {
          throw new Error(`protected watch failed to open: HTTP ${response.status}`);
        }
        const reader = response.body.getReader();
        const read = () => reader.read().then((result) => {
          if (result.done) {
            state.done = true;
            return;
          }
          state.chunks.push(Array.from(result.value));
          return read();
        });
        return read();
      }).catch((error) => {
        state.error = String(error);
        state.done = true;
      });
    });
  } catch (error) {
    throw new Error(`protected watch launch failed: ${error}`);
  }
  const deadline = Date.now() + 10_000;
  let state;
  while (Date.now() < deadline) {
    state = await page.evaluate(() => {
      return {
        chunks: window.__waltrProtectedWatch.chunks,
        protectedRequest: window.__waltrProtectedWatch.statusRequests.find((request) => request.protected),
      };
    });
    if (decodeGrpcWebFrames(state.chunks).initialMessage) {
      return { initialMessage: true, request: state.protectedRequest };
    }
    sleep(0.1);
  }
  throw new Error(`protected watch did not start: ${JSON.stringify(state)}`);
}

async function waitForLogoutResponse(page) {
  const deadline = Date.now() + 10_000;
  let response;
  while (Date.now() < deadline) {
    response = await page.evaluate(() => window.__waltrProtectedWatch.logoutResponse);
    if (response) return response;
    sleep(0.1);
  }
  throw new Error(`logout response was not observed: ${JSON.stringify(response)}`);
}

async function waitForDeniedLogoutProbe(page, statusRequestBaseline) {
  const deadline = Date.now() + 10_000;
  let requests = [];
  while (Date.now() < deadline) {
    requests = await statusRequestsFrom(page, statusRequestBaseline);
    if (requests[0]?.grpcStatus !== null && requests[0]?.grpcStatus !== undefined) {
      return { request: requests[0], postLogoutRequestCount: requests.length };
    }
    sleep(0.1);
  }
  throw new Error(`denied logout status probe was not observed: ${JSON.stringify(requests)}`);
}

async function statusRequestCount(page) {
  return page.evaluate(() => window.__waltrProtectedWatch.statusRequests.length);
}

async function statusRequestsFrom(page, baseline) {
  return page.evaluate((requestBaseline) => {
    return window.__waltrProtectedWatch.statusRequests.slice(requestBaseline);
  }, baseline);
}

async function waitForProtectedWatchEnd(page) {
  const deadline = Date.now() + 10_000;
  let state;
  while (Date.now() < deadline) {
    state = await page.evaluate(() => {
      return {
        done: window.__waltrProtectedWatch.done,
        error: window.__waltrProtectedWatch.error,
        chunks: window.__waltrProtectedWatch.chunks,
      };
    });
    if (state.done) {
      return { ...state, grpcStatus: decodeGrpcWebFrames(state.chunks).grpcStatus };
    }
    sleep(0.1);
  }
  throw new Error(`protected watch did not end after logout: ${JSON.stringify(state)}`);
}

function decodeGrpcWebFrames(chunks) {
  const bytes = chunks.flat();
  let offset = 0;
  let initialMessage = false;
  let grpcStatus = null;
  while (offset + 5 <= bytes.length) {
    const flags = bytes[offset];
    const length = ((bytes[offset + 1] << 24) >>> 0)
      + (bytes[offset + 2] << 16)
      + (bytes[offset + 3] << 8)
      + bytes[offset + 4];
    if (offset + 5 + length > bytes.length) break;
    const payload = bytes.slice(offset + 5, offset + 5 + length);
    if ((flags & 128) === 0) {
      initialMessage = true;
    } else {
      const trailers = String.fromCharCode(...payload);
      const status = trailers.match(/(?:^|\n)grpc-status:\s*(\d+)/i);
      if (status) grpcStatus = Number(status[1]);
    }
    offset += 5 + length;
  }
  return { initialMessage, grpcStatus };
}
