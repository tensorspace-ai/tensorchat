// Tests for the service worker's push and caching behaviour.
//
// Run with: node --test web/test/sw.test.mjs
//
// The worker is the one piece of this client that runs with no page, no DOM and
// no store — which is exactly why its decisions are worth pinning down. It is
// also the piece hardest to reach in a browser: it wakes on an event nobody can
// schedule and reports to nobody.
//
// So the globals it touches are stubbed here — `self`, `caches`, `fetch`,
// `clients`, `registration` — and the real `src/sw.ts` is imported against
// them. The build injects `__CACHE_NAME__`/`__PRECACHE__` via esbuild `define`;
// in Node they resolve as ordinary globals, so they are set the same way.

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";

// --- Worker environment stubs ----------------------------------------------

globalThis.__CACHE_NAME__ = "tc-testcache";
globalThis.__PRECACHE__ = ["/", "/index.html", "/assets/app.abc.js"];

/** A Cache with just the surface sw.ts uses. */
class FakeCache {
  constructor() {
    this.entries = new Map();
  }
  async addAll(requests) {
    for (const r of requests) this.entries.set(typeof r === "string" ? r : r.url, "precached");
  }
  async put(req, res) {
    this.entries.set(typeof req === "string" ? req : req.url, res);
  }
  async match(req) {
    return this.entries.get(typeof req === "string" ? req : req.url);
  }
}

const cacheStore = new Map();
globalThis.caches = {
  async open(name) {
    if (!cacheStore.has(name)) cacheStore.set(name, new FakeCache());
    return cacheStore.get(name);
  },
  async keys() {
    return [...cacheStore.keys()];
  },
  async delete(name) {
    return cacheStore.delete(name);
  },
  async match(req) {
    for (const c of cacheStore.values()) {
      const hit = await c.match(req);
      if (hit) return hit;
    }
    return undefined;
  },
};

globalThis.Request = class Request {
  constructor(url, init = {}) {
    this.url = String(url);
    this.method = init.method ?? "GET";
    this.mode = init.mode ?? "no-cors";
    this.cache = init.cache;
  }
};

/** Registered listeners, so a test can fire an event the way the browser would. */
const listeners = new Map();

/** Notifications the worker asked for. */
let shown = [];
/** Windows the worker can see. */
let windows = [];
/** Scripted responses for `fetch`, by pathname. */
let responses = new Map();
let fetchCalls = [];

globalThis.self = {
  location: { origin: "https://chat.example" },
  addEventListener(type, fn) {
    listeners.set(type, [...(listeners.get(type) ?? []), fn]);
  },
  skipWaiting: async () => {},
  clients: {
    matchAll: async () => windows,
    claim: async () => {},
    openWindow: async (url) => {
      windows.push({ url, opened: true });
      return windows.at(-1);
    },
  },
  registration: {
    showNotification: async (title, options) => {
      shown.push({ title, ...options });
    },
  },
};

globalThis.fetch = async (req) => {
  const url = typeof req === "string" ? req : req.url;
  fetchCalls.push(url);
  const path = new URL(url, "https://chat.example").pathname;
  const scripted = responses.get(path);
  if (scripted === "throw") throw new Error("offline");
  return (
    scripted ?? {
      ok: true,
      type: "basic",
      status: 200,
      clone() {
        return this;
      },
      json: async () => [],
    }
  );
};

await import("../src/sw.ts");

/** Fire a worker event and wait for whatever it passed to `waitUntil`. */
async function fire(type, event = {}) {
  const pending = [];
  const ev = {
    waitUntil: (p) => pending.push(p),
    respondWith: (p) => pending.push(p.then((r) => (ev.response = r))),
    ...event,
  };
  for (const fn of listeners.get(type) ?? []) fn(ev);
  await Promise.allSettled(pending);
  return ev;
}

beforeEach(() => {
  shown = [];
  windows = [];
  responses = new Map();
  fetchCalls = [];
});

// --- Install / activate -----------------------------------------------------

test("install precaches the app shell", async () => {
  await fire("install");
  const cache = cacheStore.get("tc-testcache");
  assert.ok(cache.entries.has("/index.html"), "the shell must be available offline");
  assert.ok(cache.entries.has("/assets/app.abc.js"), "and so must the bundle that renders it");
});

test("activate deletes caches from previous builds", async () => {
  // The cache name carries the build identity, so this is what stops a deploy
  // from serving last deployment's bundle forever.
  cacheStore.set("tc-anoldbuild", new FakeCache());
  await fire("activate");
  assert.deepEqual([...cacheStore.keys()], ["tc-testcache"]);
});

// --- Push -------------------------------------------------------------------

/** A notification payload as `/api/me/notifications` returns it. */
function items(...list) {
  responses.set("/api/me/notifications", {
    ok: true,
    status: 200,
    json: async () => list,
  });
}

test("a payload-less push fetches its content from our own origin", async () => {
  // This is the whole design: nothing sensitive rides in the push itself.
  items({ ch: "c1", id: "m1", title: "alice", body: "are you around?" });
  await fire("push");

  assert.ok(
    fetchCalls.some((u) => u.includes("/api/me/notifications")),
    "the worker must ask us, not read a payload",
  );
  assert.equal(shown.length, 1);
  assert.equal(shown[0].title, "alice");
  assert.equal(shown[0].body, "are you around?");
  assert.equal(shown[0].data.url, "/#/c/c1/m1", "clicking it opens the message");
});

test("a push is ignored while a window is focused", async () => {
  // The in-page notifier already covers that case; two buzzes for one message
  // is how people end up turning notifications off.
  windows = [{ visibilityState: "visible", focused: true, url: "https://chat.example/" }];
  items({ ch: "c1", id: "m1", title: "alice", body: "hi" });
  await fire("push");
  assert.equal(shown.length, 0);
  assert.equal(fetchCalls.length, 0, "and it should not even ask");
});

test("a push notifies when a window is open but not focused", async () => {
  windows = [{ visibilityState: "hidden", focused: false, url: "https://chat.example/" }];
  items({ ch: "c1", id: "m1", title: "alice", body: "hi" });
  await fire("push");
  assert.equal(shown.length, 1);
});

test("a burst collapses to one notification per conversation", async () => {
  // Coming back after an hour away should not produce a wall of them.
  items(
    { ch: "c1", id: "m3", title: "alice", body: "third" },
    { ch: "c1", id: "m2", title: "alice", body: "second" },
    { ch: "c2", id: "m1", title: "bob", body: "elsewhere" },
  );
  await fire("push");
  assert.equal(shown.length, 2, "two conversations, two notifications");
  assert.deepEqual(
    shown.map((n) => n.tag),
    ["tc-c1", "tc-c2"],
    "tagged per conversation, so a later message replaces rather than stacks",
  );
  assert.equal(shown[0].body, "third", "the newest message is the one shown");
});

test("a push still notifies when the fetch fails", async () => {
  // Offline, or an expired session. The push already established that
  // something happened; saying nothing would be the wrong way to fail.
  responses.set("/api/me/notifications", "throw");
  await fire("push");
  assert.equal(shown.length, 1);
  assert.equal(shown[0].title, "TensorChat");
  assert.match(shown[0].body, /new message/i);
});

test("a push with nothing pending falls back rather than showing an empty card", async () => {
  items();
  await fire("push");
  assert.equal(shown.length, 1);
  assert.equal(shown[0].title, "TensorChat");
});

// --- Notification click -----------------------------------------------------

test("clicking a notification reuses an open tab and navigates it", async () => {
  let navigated = null;
  windows = [
    {
      url: "https://chat.example/",
      focus: async () => {},
      navigate: async (u) => {
        navigated = u;
      },
    },
  ];
  let closed = false;
  await fire("notificationclick", {
    notification: { close: () => (closed = true), data: { url: "/#/c/c1/m1" } },
  });
  assert.ok(closed, "the notification is dismissed");
  assert.equal(navigated, "/#/c/c1/m1", "an existing tab is reused, not duplicated");
});

test("clicking with nothing open opens a window at the permalink", async () => {
  await fire("notificationclick", {
    notification: { close: () => {}, data: { url: "/#/c/c1/m1" } },
  });
  assert.equal(windows.at(-1)?.url, "/#/c/c1/m1");
});

// --- Fetch ------------------------------------------------------------------

test("API and websocket requests are never cached", async () => {
  // Chat state is live by definition; a stale reply is worse than an error.
  for (const path of ["/api/channels", "/ws", "/healthz"]) {
    const ev = await fire("fetch", {
      request: new Request(`https://chat.example${path}`),
    });
    assert.equal(ev.response, undefined, `${path} must pass straight through`);
  }
});

test("non-GET requests are never cached", async () => {
  const ev = await fire("fetch", {
    request: new Request("https://chat.example/index.html", { method: "POST" }),
  });
  assert.equal(ev.response, undefined);
});

test("another origin is left alone", async () => {
  const ev = await fire("fetch", {
    request: new Request("https://elsewhere.example/thing.js"),
  });
  assert.equal(ev.response, undefined);
});

test("a hashed asset is served from cache without a network call", async () => {
  const cache = await caches.open("tc-testcache");
  await cache.put("https://chat.example/assets/app.abc.js", { ok: true, cached: true });
  const ev = await fire("fetch", {
    request: new Request("https://chat.example/assets/app.abc.js"),
  });
  assert.equal(ev.response.cached, true);
  assert.equal(fetchCalls.length, 0, "immutable assets never need revalidating");
});

test("navigation goes to the network first, so a deploy is picked up", async () => {
  const cache = await caches.open("tc-testcache");
  await cache.put("https://chat.example/index.html", { ok: true, stale: true });
  responses.set("/index.html", {
    ok: true,
    type: "basic",
    status: 200,
    fresh: true,
    clone() {
      return this;
    },
  });

  const ev = await fire("fetch", {
    request: new Request("https://chat.example/index.html", { mode: "navigate" }),
  });
  assert.equal(ev.response.fresh, true, "a cached shell must not pin the old bundle");
});

test("navigation falls back to the cached shell when offline", async () => {
  const cache = await caches.open("tc-testcache");
  await cache.put("/index.html", { ok: true, shell: true });
  responses.set("/some/route", "throw");

  const ev = await fire("fetch", {
    request: new Request("https://chat.example/some/route", { mode: "navigate" }),
  });
  assert.equal(ev.response.shell, true, "the app routes on the fragment, so the shell suffices");
});
