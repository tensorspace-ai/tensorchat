// Tests for theme preference resolution.
//
// Run with: node --test web/test/theme.test.mjs
//
// The rule worth pinning down is that there are *three* preferences and only
// two themes: "system" is not a synonym for either one, and resolving it has to
// consult the OS every time rather than being frozen at first read.
//
// Imports the TypeScript source directly, like the other suites here.

import { test, beforeEach } from "node:test";
import assert from "node:assert/strict";

// --- Browser stubs ---------------------------------------------------------

let systemPrefersLight = false;
const schemeListeners = [];

globalThis.localStorage = {
  map: new Map(),
  getItem(k) {
    return this.map.has(k) ? this.map.get(k) : null;
  },
  setItem(k, v) {
    if (this.readonly) throw new Error("QuotaExceededError");
    this.map.set(k, String(v));
  },
  removeItem(k) {
    this.map.delete(k);
  },
};

globalThis.matchMedia = (query) => ({
  media: query,
  get matches() {
    return query.includes("light") ? systemPrefersLight : !systemPrefersLight;
  },
  addEventListener: (_type, fn) => schemeListeners.push(fn),
  removeEventListener: () => {},
});

const html = { dataset: {}, style: {} };
globalThis.document = { documentElement: html };

const { readPreference, resolveTheme, applyTheme, setPreference, watchSystemTheme } =
  await import("../src/theme.ts");

beforeEach(() => {
  localStorage.map.clear();
  localStorage.readonly = false;
  systemPrefersLight = false;
  html.dataset = {};
  html.style = {};
});

// --- Reading the preference ------------------------------------------------

test("the default preference is system, not dark", () => {
  // They are not the same thing: "system" keeps tracking the OS, and the
  // difference is the entire reason there are three options.
  assert.equal(readPreference(), "system");
});

test("only the three known values are honoured", () => {
  for (const good of ["system", "light", "dark"]) {
    localStorage.setItem("tc_theme", good);
    assert.equal(readPreference(), good);
  }
  // Anything else — a typo, another tool's key, an older format — falls back.
  for (const bad of ["", "Dark", "solarized", "true", "null"]) {
    localStorage.setItem("tc_theme", bad);
    assert.equal(readPreference(), "system", `${bad} should not be honoured`);
  }
});

test("unreadable storage falls back to system rather than throwing", () => {
  const original = localStorage.getItem;
  localStorage.getItem = () => {
    throw new Error("SecurityError");
  };
  assert.equal(readPreference(), "system");
  localStorage.getItem = original;
});

// --- Resolving ------------------------------------------------------------

test("an explicit preference ignores the OS entirely", () => {
  systemPrefersLight = true;
  assert.equal(resolveTheme("dark"), "dark");
  systemPrefersLight = false;
  assert.equal(resolveTheme("light"), "light");
});

test("system resolves against the OS, both ways", () => {
  systemPrefersLight = true;
  assert.equal(resolveTheme("system"), "light");
  systemPrefersLight = false;
  assert.equal(resolveTheme("system"), "dark");
});

test("dark is the fallback when the OS has no opinion available", async () => {
  // matchMedia is absent in some embedded webviews; the stylesheet's own
  // default is dark, so resolving to dark keeps CSS and JS agreeing.
  const saved = globalThis.matchMedia;
  globalThis.matchMedia = undefined;
  const fresh = await import(`../src/theme.ts?nomm=${Date.now()}`);
  assert.equal(fresh.resolveTheme("system"), "dark");
  globalThis.matchMedia = saved;
});

// --- Applying -------------------------------------------------------------

test("applying stamps both the attribute and color-scheme", () => {
  // `color-scheme` is what makes native scrollbars and form controls match;
  // without it a light page keeps dark browser furniture.
  systemPrefersLight = true;
  applyTheme("system");
  assert.equal(html.dataset.theme, "light");
  assert.equal(html.style.colorScheme, "light");

  applyTheme("dark");
  assert.equal(html.dataset.theme, "dark");
  assert.equal(html.style.colorScheme, "dark");
});

test("setting a preference persists it and applies it at once", () => {
  setPreference("light");
  assert.equal(localStorage.getItem("tc_theme"), "light");
  assert.equal(html.dataset.theme, "light");
  assert.equal(readPreference(), "light");
});

test("an unwritable store still themes the current session", () => {
  localStorage.readonly = true;
  assert.doesNotThrow(() => setPreference("light"));
  assert.equal(html.dataset.theme, "light", "the theme applies even unsaved");
});

// --- Following the OS -----------------------------------------------------

test("system follows the OS flipping, and an explicit choice does not", () => {
  schemeListeners.length = 0;
  watchSystemTheme();
  assert.equal(schemeListeners.length, 1, "one listener, registered once");

  setPreference("system");
  systemPrefersLight = true;
  schemeListeners[0]();
  assert.equal(html.dataset.theme, "light", "system tracks the OS");

  // An explicit choice must survive the OS changing under it.
  setPreference("dark");
  systemPrefersLight = false;
  schemeListeners[0]();
  assert.equal(html.dataset.theme, "dark");
  systemPrefersLight = true;
  schemeListeners[0]();
  assert.equal(html.dataset.theme, "dark", "an explicit choice ignores the OS");
});
