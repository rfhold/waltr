const status = document.querySelector("#status");
const device = document.querySelector("#device");
const verification = document.querySelector("#verification");
const code = document.querySelector("#code");
const qr = document.querySelector("#qr");
const restart = document.querySelector("#restart");
const cancel = document.querySelector("#cancel");
const fallback = document.querySelector("#same-device");
const returnTo = new URL(fallback.href).searchParams.get("return_to");

let generation = 0;
let attempt;
let timer;
let starting;
let polling;
let retiring;
let completed = false;
let leavingForFallback = false;
let page = new AbortController();

function stop(message, action = "Get a new code") {
  generation += 1;
  clearTimeout(timer);
  device.hidden = true;
  qr.removeAttribute("src");
  verification.removeAttribute("href");
  verification.textContent = "";
  code.textContent = "";
  restart.hidden = false;
  restart.textContent = action;
  status.textContent = message;
}

function post(action, body) {
  return fetch(`/oidc/device/${action}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
    mode: "same-origin",
    redirect: "error",
    cache: "no-store",
    signal: page.signal,
  });
}

function schedule(delay, run) {
  if (!retiring && !page.signal.aborted) {
    timer = setTimeout(() => {
      const flight = poll(run);
      polling = flight;
      void flight.finally(() => {
        if (polling === flight) polling = undefined;
      });
    }, Math.max(100, delay));
  }
}

async function poll(run) {
  try {
    const response = await post("poll", { attempt });
    if (!response.ok) throw new Error("Poll failed");
    const result = await response.json();
    if (run !== generation) return;
    switch (result.status) {
      case "pending":
        schedule(result.retry_after_ms, run);
        break;
      case "complete":
        completed = true;
        attempt = undefined;
        stop("Signed in. Opening Waltr…");
        restart.hidden = true;
        window.location.replace(result.return_to);
        break;
      case "denied":
        if (!retiring) {
          attempt = undefined;
          stop("Sign-in was denied.");
        }
        break;
      case "expired":
        if (!retiring) {
          attempt = undefined;
          stop("The code expired or is no longer available.");
        }
        break;
      default:
        throw new Error("Unexpected poll response");
    }
  } catch {
    if (run === generation && !retiring && !page.signal.aborted) {
      stop("Could not finish QR sign-in. Try again or sign in on this device.");
    }
  }
}

async function start() {
  const run = ++generation;
  restart.hidden = true;
  status.textContent = "Getting a sign-in code…";
  const request = (async () => {
    const response = await post("start", { return_to: returnTo });
    if (response.status === 409) return { status: "busy" };
    if (!response.ok) throw new Error("Start failed");
    return response.json();
  })();
  starting = request;
  try {
    const result = await request;
    if (run !== generation) return;
    if (result.status === "busy") {
      if (!retiring) stop("A QR sign-in may already be open in another tab. Finish or cancel it there, try again, or sign in on this device.", "Try again");
      return;
    }
    if (result.status === "expired") {
      if (!retiring) stop("The code expired. Get a new code to try again.");
      return;
    }
    attempt = result.attempt;
    code.textContent = result.user_code;
    verification.href = result.verification_uri;
    verification.textContent = result.verification_uri;
    qr.src = result.qr_image;
    device.hidden = false;
    status.textContent = "Waiting for you to approve sign-in.";
    schedule(result.retry_after_ms, run);
  } catch {
    if (run === generation && !retiring && !page.signal.aborted) {
      stop("QR sign-in is unavailable. Try again or sign in on this device.", "Try again");
    }
  } finally {
    if (starting === request) starting = undefined;
  }
}

function abandon() {
  if (!retiring) {
    clearTimeout(timer);
    const pending = starting;
    const flight = polling;
    const known = attempt;
    const run = generation;
    cancel.disabled = true;
    status.textContent = "Cancelling the sign-in code…";
    retiring = (async () => {
      let outcome = "unconfirmed";
      try {
        const id = known ?? (await pending?.then(result => result.attempt, () => undefined));
        if (run !== generation || page.signal.aborted) return;
        if (id) {
          attempt = id;
          const response = await post("cancel", { attempt: id });
          if (response.status === 204) outcome = "cancelled";
          if (response.status === 409) outcome = "gone";
        }
      } catch {
        // Completion may already have won, even if the cancel request failed.
      }
      // The poll can still set the cookie when cancellation loses the server claim.
      await flight;
      if (!completed && !page.signal.aborted && run === generation) {
        if (outcome !== "unconfirmed") attempt = undefined;
        stop({
          cancelled: "The sign-in code was cancelled.",
          gone: "The code is no longer available.",
          unconfirmed: "Could not confirm that the code was cancelled. You can still sign in on this device.",
        }[outcome]);
        return outcome;
      }
    })().finally(() => {
      retiring = undefined;
      cancel.disabled = false;
    });
  }
  return retiring;
}

cancel.addEventListener("click", () => void abandon());
restart.addEventListener("click", async () => {
  if (starting || retiring || completed || leavingForFallback || page.signal.aborted) return;
  if (attempt) {
    restart.hidden = true;
    const activePage = page;
    const outcome = await abandon();
    if (activePage.signal.aborted || completed || leavingForFallback || !outcome || outcome === "unconfirmed") return;
  }
  void start();
});
fallback.addEventListener("click", async event => {
  if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  if (!attempt && !starting && !retiring && !completed) return;
  event.preventDefault();
  leavingForFallback = true;
  const activePage = page;
  if (!completed) await abandon();
  if (!completed && !activePage.signal.aborted) window.location.assign(fallback.href);
});
window.addEventListener("pagehide", () => {
  // A tab that leaves must not retire a code another tab is using.
  generation += 1;
  clearTimeout(timer);
  page.abort();
});
window.addEventListener("pageshow", event => {
  if (!event.persisted) return;
  page = new AbortController();
  completed = false;
  leavingForFallback = false;
  starting = undefined;
  stop("Get a sign-in code to continue.");
});
void start();
