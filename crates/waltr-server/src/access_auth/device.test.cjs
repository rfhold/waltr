const assert = require("node:assert/strict");
const { readFileSync } = require("node:fs");
const test = require("node:test");
const { runInNewContext } = require("node:vm");

const script = readFileSync(`${__dirname}/device.js`, "utf8");
const tick = () => new Promise(resolve => setImmediate(resolve));
const details = attempt => ({ attempt, retry_after_ms: 100 });

function browser() {
  const elements = {
    "#same-device": { href: "https://waltr.example/oidc/login?return_to=%2Fsessions" },
  };
  const calls = [], navigation = [];
  const timers = new Map();
  const events = {};
  runInNewContext(script, {
    URL, AbortController,
    document: { querySelector(id) {
      return Object.assign(elements[id] ??= {}, {
        addEventListener(name, callback) { this[name] = callback; },
        removeAttribute(name) { delete this[name]; },
      });
    } },
    window: {
      addEventListener(name, callback) { events[name] = callback; },
      location: {
        replace: path => navigation.push(["complete", path]),
        assign: path => navigation.push(["fallback", path]),
      },
    },
    fetch: (path, { body, signal }) => new Promise((resolve, reject) => {
      calls.push({
        action: path.split("/").at(-1),
        body: JSON.parse(body),
        fail: reject,
        reply: (status, value) => resolve({ status, ok: status >= 200 && status < 300, json: async () => value }),
        signal,
      });
      signal.addEventListener("abort", reject, { once: true });
    }),
    setTimeout: callback => {
      const timer = Symbol();
      timers.set(timer, callback);
      return timer;
    },
    clearTimeout: timer => timers.delete(timer),
  });
  return {
    calls, elements, navigation, timers, events,
    async poll() {
      const [timer, callback] = timers.entries().next().value;
      timers.delete(timer);
      callback();
      await tick();
    },
  };
}

test("renewal after a lost poll retires the exact known attempt before requesting a new code", async () => {
  const b = browser();
  b.calls[0].reply(200, details("old"));
  await tick();
  await b.poll();
  b.calls[1].fail(new Error("connection closed"));
  await tick();
  assert.equal(b.elements["#device"].hidden, true);

  const renewal = b.elements["#restart"].click();
  assert.equal(b.calls[2].action, "cancel");
  assert.deepEqual(b.calls[2].body, { attempt: "old" });
  assert.equal(b.calls.length, 3);
  b.calls[2].reply(204);
  await renewal;
  assert.equal(b.calls[3].action, "start");
  b.calls[3].reply(200, details("replacement"));
  await tick();
  await b.poll();
  assert.deepEqual(b.calls[4].body, { attempt: "replacement" });
});

test("unknown start outcomes can resume, and a busy start does not cancel another tab", async () => {
  const b = browser();
  b.calls[0].fail(new Error("connection closed"));
  await tick();
  await b.elements["#restart"].click();
  b.calls[1].reply(409);
  await tick();
  assert.match(b.elements["#status"].textContent, /another tab/);
  await b.elements["#restart"].click();
  assert.deepEqual(b.calls.map(call => call.action), ["start", "start", "start"]);
});

test("unconfirmed renewal retries the original cancellation without starting a code", async () => {
  const b = browser();
  b.calls[0].reply(200, details("old"));
  await tick();
  await b.poll();
  b.calls[1].fail(new Error("poll interrupted"));
  await tick();
  const renewal = b.elements["#restart"].click();
  b.calls[2].fail(new Error("cancel interrupted"));
  await renewal;
  assert.equal(b.calls.length, 3);
  const retry = b.elements["#restart"].click();
  assert.equal(b.calls[3].action, "cancel");
  assert.deepEqual(b.calls[3].body, { attempt: "old" });
  b.calls[3].reply(409);
  await retry;
  assert.equal(b.calls[4].action, "start");
});

test("a completion that claims the attempt before cancel beats fallback", async () => {
  const b = browser();
  b.calls[0].reply(200, details("old"));
  await tick();
  await b.poll();
  const fallback = b.elements["#same-device"].click({ preventDefault() {} });
  b.calls[2].reply(409);
  await tick();
  assert.deepEqual(b.navigation, []);
  b.calls[1].reply(200, { status: "complete", return_to: "/sessions" });
  await fallback;
  assert.deepEqual(b.navigation, [["complete", "/sessions"]]);
  assert.match(b.elements["#status"].textContent, /Signed in/);
});

test("leaving aborts polling without cancelling the attempt", async () => {
  const b = browser();
  b.calls[0].reply(200, details("old"));
  await tick();
  await b.poll();
  b.events.pagehide();
  await tick();
  assert.equal(b.calls[1].signal.aborted, true);
  assert.equal(b.timers.size, 0);
  assert.deepEqual(b.calls.map(call => call.action), ["start", "poll"]);
});
