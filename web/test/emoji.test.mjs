// Tests for the emoji table, search ranking, and shortcode expansion.
//
// Run with: node --test web/test/emoji.test.mjs
//
// Imports the TypeScript source directly, like the other suites here.

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  allEmoji,
  categories,
  emojiByName,
  expandShortcodes,
  searchEmoji,
} from "../src/emoji.ts";

// --- The table itself ------------------------------------------------------

test("every entry parses into a character, a name, and a category", () => {
  const all = allEmoji();
  assert.ok(all.length > 200, `expected a useful set, got ${all.length}`);
  for (const e of all) {
    assert.ok(e.char.length > 0, `empty char for ${e.name}`);
    assert.match(e.name, /^[a-z0-9_+-]+$/, `bad shortcode: ${e.name}`);
    assert.ok(e.terms.includes(e.name), `${e.name} must match its own name`);
    assert.ok(categories().includes(e.category));
  }
});

test("shortcodes are unique", () => {
  // A duplicate would make `emojiByName` silently prefer whichever came first.
  const seen = new Set();
  for (const e of allEmoji()) {
    assert.ok(!seen.has(e.name), `duplicate shortcode: ${e.name}`);
    seen.add(e.name);
  }
});

test("every category in the display order actually has entries", () => {
  for (const c of categories()) {
    assert.ok(
      allEmoji().some((e) => e.category === c),
      `category ${c} is empty, so it would render as a bare heading`,
    );
  }
});

// --- Search ----------------------------------------------------------------

test("an exact shortcode ranks first", () => {
  assert.equal(searchEmoji("ok")[0].name, "ok");
});

test("a name prefix outranks a keyword match", () => {
  // `:th` is far more likely to be someone spelling out "thumbsup" than
  // reaching for "earth_africa".
  const names = searchEmoji("thumbs").map((e) => e.name);
  assert.equal(names[0], "thumbsup");
});

test("keywords find emoji whose name you do not know", () => {
  assert.ok(searchEmoji("lol").some((e) => e.name === "joy"));
  assert.ok(searchEmoji("idk").some((e) => e.name === "shrug"));
  assert.ok(searchEmoji("deploy").some((e) => e.name === "rocket"));
  assert.ok(searchEmoji("+1").some((e) => e.name === "thumbsup"));
});

test("a leading colon is ignored, so typing `:fire` works", () => {
  assert.deepEqual(
    searchEmoji(":fire").map((e) => e.name),
    searchEmoji("fire").map((e) => e.name),
  );
});

test("an empty query returns the head of the set, not nothing", () => {
  assert.ok(searchEmoji("").length > 0);
});

test("no match returns empty rather than throwing", () => {
  assert.deepEqual(searchEmoji("zzzzznotanemoji"), []);
});

test("the limit is honoured", () => {
  assert.equal(searchEmoji("a", 5).length, 5);
});

// --- Lookup and expansion --------------------------------------------------

test("emojiByName accepts a bare or colon-wrapped name", () => {
  const bare = emojiByName("fire");
  assert.ok(bare);
  assert.equal(emojiByName(":fire:").char, bare.char);
  assert.equal(emojiByName("FIRE").char, bare.char);
  assert.equal(emojiByName("not_an_emoji"), undefined);
});

test("expandShortcodes replaces known codes and leaves unknown ones alone", () => {
  const fire = emojiByName("fire").char;
  const tada = emojiByName("tada").char;
  assert.equal(expandShortcodes("ship it :fire:"), `ship it ${fire}`);
  assert.equal(expandShortcodes(":fire::tada:"), `${fire}${tada}`);
  assert.equal(
    expandShortcodes("not :a_real_code: here"),
    "not :a_real_code: here",
    "an unknown code must survive as literal text",
  );
});

test("expandShortcodes leaves ordinary punctuation untouched", () => {
  // The pattern must not eat prose with colons in it.
  for (const text of [
    "see below: it works",
    "10:30 tomorrow",
    "http://example.com",
    "ratio 3:1",
    "",
  ]) {
    assert.equal(expandShortcodes(text), text, `mangled: ${text}`);
  }
});
