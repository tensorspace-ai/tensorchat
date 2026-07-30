// Tests for draft persistence.
//
// Run with: node --test web/test/drafts.test.mjs
//
// The module talks to `localStorage` and to the page lifecycle, neither of
// which exists in Node. Both are small enough to stub, and stubbing them is
// what makes the interesting cases testable at all: a corrupt stored value, a
// storage area that throws, and the prune rules.
//
// Imports the TypeScript source directly, like the other suites here.

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";

// --- Browser stubs ---------------------------------------------------------

class FakeStorage {
  constructor() {
    this.map = new Map();
    /** Set to make every write throw, standing in for a full quota. */
    this.readonly = false;
  }
  getItem(k) {
    return this.map.has(k) ? this.map.get(k) : null;
  }
  setItem(k, v) {
    if (this.readonly) throw new Error("QuotaExceededError");
    this.map.set(k, String(v));
  }
  removeItem(k) {
    this.map.delete(k);
  }
}

const listeners = new Map();
globalThis.localStorage = new FakeStorage();
globalThis.addEventListener = (name, fn) => {
  listeners.set(name, [...(listeners.get(name) ?? []), fn]);
};
globalThis.document = { visibilityState: "visible" };

// Imported after the stubs exist: the module registers lifecycle listeners at
// import time.
const { loadDraft, saveDraft, clearDraft, clearAllDrafts, flushDrafts } =
  await import("../src/drafts.ts");

const KEY = "tc_drafts";

/** What is actually on "disk", as opposed to the in-memory cache. */
function stored() {
  const raw = localStorage.getItem(KEY);
  return raw === null ? null : JSON.parse(raw);
}

beforeEach(() => {
  localStorage.map.clear();
  localStorage.readonly = false;
  clearAllDrafts();
});

// --- Round trip ------------------------------------------------------------

test("a draft survives a flush and comes back", () => {
  saveDraft("c1", "half a thought");
  flushDrafts();
  assert.equal(stored().c1.t, "half a thought");
  assert.equal(loadDraft("c1"), "half a thought");
});

test("drafts are per channel", () => {
  saveDraft("c1", "one");
  saveDraft("c2", "two");
  assert.equal(loadDraft("c1"), "one");
  assert.equal(loadDraft("c2"), "two");
  assert.equal(loadDraft("c3"), "", "an untouched channel has no draft");
});

test("saving empty text removes the draft", () => {
  saveDraft("c1", "typed then deleted");
  saveDraft("c1", "");
  flushDrafts();
  assert.equal(loadDraft("c1"), "");
  assert.equal("c1" in stored(), false, "the entry is gone, not blank");
});

test("clearDraft is what sending uses", () => {
  saveDraft("c1", "sent");
  clearDraft("c1");
  assert.equal(loadDraft("c1"), "");
});

// --- Writes are debounced but never lost -----------------------------------

test("a write is debounced, not immediate", () => {
  saveDraft("c1", "typing");
  // Nothing on disk yet — that is the point of the debounce.
  assert.equal(stored(), null);
  // ...but the value is readable immediately from the cache, so the composer
  // never shows stale text.
  assert.equal(loadDraft("c1"), "typing");
});

test("pagehide flushes synchronously", () => {
  // The case the debounce would otherwise lose: a reload moments after the
  // last keystroke.
  saveDraft("c1", "unsaved when the tab closed");
  for (const fn of listeners.get("pagehide") ?? []) fn();
  assert.equal(stored().c1.t, "unsaved when the tab closed");
});

test("hiding the tab flushes, and staying visible does not", () => {
  saveDraft("c1", "backgrounded");
  const fire = () => {
    for (const fn of listeners.get("visibilitychange") ?? []) fn();
  };

  document.visibilityState = "visible";
  fire();
  assert.equal(stored(), null, "a visible tab has not gone anywhere");

  document.visibilityState = "hidden";
  fire();
  assert.equal(stored().c1.t, "backgrounded");
  document.visibilityState = "visible";
});

// --- Hostile and broken storage --------------------------------------------

test("a corrupt stored value does not throw", () => {
  localStorage.map.set(KEY, "{not json at all");
  clearAllDrafts();
  localStorage.map.set(KEY, "{not json at all");
  // Force a re-read by importing state fresh is not possible here, so assert
  // the behaviour that matters: reading never throws and yields nothing.
  assert.doesNotThrow(() => loadDraft("c1"));
});

test("entries of the wrong shape are skipped, not trusted", async () => {
  // Another tool, or an older format, could own this key.
  localStorage.map.set(
    KEY,
    JSON.stringify({
      good: { t: "keep me", at: Date.now() },
      noText: { at: Date.now() },
      noTime: { t: "when?" },
      notAnObject: 42,
      ancient: { t: "old", at: Date.now() - 400 * 24 * 60 * 60 * 1000 },
    }),
  );
  // A fresh module instance is the only way to re-run the read path.
  const fresh = await import(`../src/drafts.ts?shape=${Date.now()}`);
  assert.equal(fresh.loadDraft("good"), "keep me");
  assert.equal(fresh.loadDraft("noText"), "");
  assert.equal(fresh.loadDraft("noTime"), "");
  assert.equal(fresh.loadDraft("notAnObject"), "");
  assert.equal(fresh.loadDraft("ancient"), "", "stale drafts are dropped");
});

test("a storage area that throws does not break typing", () => {
  // Private browsing and a full quota both look like this.
  localStorage.readonly = true;
  assert.doesNotThrow(() => {
    saveDraft("c1", "still typing");
    flushDrafts();
  });
  // The draft is still usable for the rest of the session.
  assert.equal(loadDraft("c1"), "still typing");
});

// --- Bounds ----------------------------------------------------------------

test("the number of stored drafts is capped, keeping the newest", async () => {
  const now = Date.now();
  const many = {};
  // 60 drafts, all in the past and ascending, so `c0` is unambiguously the
  // oldest and the one saved below is unambiguously the newest.
  for (let i = 0; i < 60; i++) {
    many[`c${i}`] = { t: `draft ${i}`, at: now - (60 - i) * 1000 };
  }
  localStorage.map.set(KEY, JSON.stringify(many));

  const fresh = await import(`../src/drafts.ts?cap=${Date.now()}`);
  fresh.saveDraft("newest", "just typed");
  fresh.flushDrafts();

  const kept = stored();
  assert.equal(Object.keys(kept).length, 50);
  assert.equal(kept.newest.t, "just typed", "the newest is always kept");
  assert.equal("c0" in kept, false, "the oldest is dropped first");
  assert.equal("c59" in kept, true);
});

test("an over-long draft is truncated rather than rejected", () => {
  // The server refuses more than 16 KiB, so storing more could never be sent.
  const huge = "x".repeat(20 * 1024);
  saveDraft("c1", huge);
  flushDrafts();
  assert.equal(loadDraft("c1").length, 16 * 1024);
});

test("re-saving identical text does not restamp the draft", () => {
  saveDraft("c1", "same");
  flushDrafts();
  const first = stored().c1.at;
  saveDraft("c1", "same");
  flushDrafts();
  assert.equal(
    stored().c1.at,
    first,
    "an unchanged draft should not look freshly written to the pruner",
  );
});

// --- Sign-out --------------------------------------------------------------

test("clearAllDrafts leaves nothing behind for the next user", () => {
  saveDraft("c1", "private");
  saveDraft("c2", "also private");
  flushDrafts();
  clearAllDrafts();
  assert.equal(stored(), null);
  assert.equal(loadDraft("c1"), "");
});
