// Tests for the "is this worth interrupting someone for?" rules.
//
// Run with: node --test web/test/notify.test.mjs
//
// The interesting part of notifications is not the Notification API call, it is
// the set of conditions that decide whether to make one. Those conditions are
// pure logic over the store, so they are testable with stubs and no browser.
//
// Imports the TypeScript source directly, like the other suites here.

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";
import { createNotifier } from "../src/notify.ts";

// --- Browser stubs ---------------------------------------------------------

let fired = [];

class FakeNotification {
  static permission = "granted";
  static requestPermission = async () => FakeNotification.permission;
  constructor(title, options) {
    Object.assign(this, { title, ...options });
    this.closed = false;
    // Push the instance, so a test can invoke the `onclick` the code assigns.
    fired.push(this);
  }
  close() {
    this.closed = true;
  }
}

const storage = new Map();

function installGlobals() {
  fired = [];
  storage.clear();
  storage.set("tc_notifications", "on");
  FakeNotification.permission = "granted";

  globalThis.Notification = FakeNotification;
  globalThis.document = { hidden: true };
  globalThis.window = { focus() {} };
  globalThis.localStorage = {
    getItem: (k) => storage.get(k) ?? null,
    setItem: (k, v) => storage.set(k, v),
    removeItem: (k) => storage.delete(k),
  };
}

// --- Store stub ------------------------------------------------------------

const ME = "1";
const OTHER = "2";

function makeStore(overrides = {}) {
  const channels = new Map([
    ["100", { id: "100", k: "public", n: "general" }],
    ["200", { id: "200", k: "dm", m: [ME, OTHER] }],
  ]);
  return {
    me: () => ({ id: ME, h: "me", n: "Me" }),
    channels: () => channels,
    currentChannel: () => null,
    isMuted: () => false,
    userName: (id) => (id === ME ? "Me" : "Someone"),
    channelTitle: (c) => c.n ?? "Someone",
    ...overrides,
  };
}

/** A `msg` frame in the named channel. */
function msg({ channel = "100", author = OTHER, body = "hello", mentions = [] } = {}) {
  return { msg: { m: { id: "9", ch: channel, au: author, b: body, mn: mentions } } };
}

beforeEach(installGlobals);

// --- What should notify ----------------------------------------------------

test("a mention in a channel notifies", () => {
  const n = createNotifier(makeStore(), () => {});
  n.consider(msg({ mentions: [ME] }));
  assert.equal(fired.length, 1);
  assert.equal(fired[0].title, "#general");
  assert.match(fired[0].body, /Someone: hello/);
});

test("a direct message notifies without needing a mention", () => {
  const n = createNotifier(makeStore(), () => {});
  n.consider(msg({ channel: "200" }));
  assert.equal(fired.length, 1);
});

test("a mention pierces a muted channel, ambient traffic does not", () => {
  const store = makeStore({ isMuted: () => true });

  const n = createNotifier(store, () => {});
  n.consider(msg({ channel: "200" }));
  assert.equal(fired.length, 0, "muted DM traffic stays quiet");

  n.consider(msg({ mentions: [ME] }));
  assert.equal(fired.length, 1, "a mention still gets through");
});

test("messages in one channel collapse onto a single tag", () => {
  const n = createNotifier(makeStore(), () => {});
  n.consider(msg({ mentions: [ME] }));
  n.consider(msg({ mentions: [ME], body: "again" }));
  assert.equal(fired[0].tag, fired[1].tag);
});

// --- What should stay quiet ------------------------------------------------

test("plain channel chatter that does not mention you is ignored", () => {
  const n = createNotifier(makeStore(), () => {});
  n.consider(msg());
  assert.equal(fired.length, 0);
});

test("your own messages never notify you", () => {
  const n = createNotifier(makeStore(), () => {});
  n.consider(msg({ author: ME, mentions: [ME] }));
  assert.equal(fired.length, 0);
});

test("the channel you are looking at in a focused window stays quiet", () => {
  globalThis.document.hidden = false;
  const n = createNotifier(makeStore({ currentChannel: () => "100" }), () => {});
  n.consider(msg({ mentions: [ME] }));
  assert.equal(fired.length, 0, "it is already on screen");
});

test("a background tab notifies even for the channel that is open", () => {
  globalThis.document.hidden = true;
  const n = createNotifier(makeStore({ currentChannel: () => "100" }), () => {});
  n.consider(msg({ mentions: [ME] }));
  assert.equal(fired.length, 1);
});

test("another channel notifies even when the window is focused", () => {
  globalThis.document.hidden = false;
  const n = createNotifier(makeStore({ currentChannel: () => "999" }), () => {});
  n.consider(msg({ mentions: [ME] }));
  assert.equal(fired.length, 1);
});

test("non-message frames are ignored", () => {
  const n = createNotifier(makeStore(), () => {});
  n.consider({ typing: { ch: "100", u: OTHER } });
  n.consider({ presence: { u: OTHER, p: "online" } });
  assert.equal(fired.length, 0);
});

// --- Enablement ------------------------------------------------------------

test("nothing fires while the preference is off", () => {
  storage.set("tc_notifications", "off");
  const n = createNotifier(makeStore(), () => {});
  assert.equal(n.enabled(), false);
  n.consider(msg({ mentions: [ME] }));
  assert.equal(fired.length, 0);
});

test("nothing fires when the browser has denied permission", () => {
  FakeNotification.permission = "denied";
  const n = createNotifier(makeStore(), () => {});
  assert.equal(n.enabled(), false);
  n.consider(msg({ mentions: [ME] }));
  assert.equal(fired.length, 0);
});

test("enabling asks for permission and remembers the answer", async () => {
  storage.set("tc_notifications", "off");
  FakeNotification.permission = "default";
  const n = createNotifier(makeStore(), () => {});

  FakeNotification.permission = "granted";
  FakeNotification.requestPermission = async () => "granted";
  assert.equal(await n.setEnabled(true), true);
  assert.equal(storage.get("tc_notifications"), "on");
  assert.equal(n.enabled(), true);

  assert.equal(await n.setEnabled(false), false);
  assert.equal(storage.get("tc_notifications"), "off");
});

test("a refused permission prompt leaves them off", async () => {
  storage.set("tc_notifications", "off");
  FakeNotification.permission = "default";
  FakeNotification.requestPermission = async () => {
    FakeNotification.permission = "denied";
    return "denied";
  };
  const n = createNotifier(makeStore(), () => {});
  assert.equal(await n.setEnabled(true), false);
  assert.equal(storage.get("tc_notifications"), "off");
});

test("clicking a notification focuses the window and opens its channel", () => {
  let opened = null;
  let focused = false;
  globalThis.window.focus = () => (focused = true);

  const n = createNotifier(makeStore(), (ch) => (opened = ch));
  n.consider(msg({ channel: "200" }));
  assert.equal(fired.length, 1);
  assert.equal(opened, null, "nothing happens until it is clicked");

  fired[0].onclick();
  assert.equal(opened, "200");
  assert.equal(focused, true);
  assert.equal(fired[0].closed, true, "the bubble dismisses itself");
});

test("a Notification constructor that throws does not propagate", () => {
  // Some browsers throw rather than reject when notifications are blocked by
  // policy. That must never take the socket's frame handler with it.
  globalThis.Notification = class {
    static permission = "granted";
    constructor() {
      throw new Error("blocked by policy");
    }
  };
  const n = createNotifier(makeStore(), () => {});
  assert.doesNotThrow(() => n.consider(msg({ mentions: [ME] })));
});
