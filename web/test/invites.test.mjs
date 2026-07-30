// Tests for the client's half of invite links.
//
// Run with: node --test web/test/invites.test.mjs
//
// Two pure functions carry the logic worth testing: parsing a token out of the
// URL, and turning an invite's expiry and use count into a phrase an
// administrator can act on. Everything else in the feature is DOM assembly or a
// `fetch`, neither of which this suite has a browser for.
//
// Imports the TypeScript source directly, like the other suites here.

import { test } from "node:test";
import assert from "node:assert/strict";
import { inviteFromLocation } from "../src/ui/login.ts";
import { describeInvite } from "../src/ui/modals.ts";

// --- Parsing the link ------------------------------------------------------

test("reads a token out of an invite fragment", () => {
  assert.equal(inviteFromLocation("#/join/abc123"), "abc123");
  // Base64url is what the server mints, so the alphabet has to survive.
  assert.equal(
    inviteFromLocation("#/join/Zm9vYmFy-_QwXyZ"),
    "Zm9vYmFy-_QwXyZ",
  );
});

test("percent-escapes are decoded", () => {
  assert.equal(inviteFromLocation("#/join/a%2Bb"), "a+b");
});

test("ignores fragments that are not invites", () => {
  // The app's own routes must not be mistaken for invites, or opening a
  // permalink would show the sign-up form.
  for (const hash of [
    "",
    "#",
    "#/c/123",
    "#/c/123/456",
    "#/join",
    "#/join/",
    "#/join/a/b",
    "#/joinx/abc",
    "#join/abc",
  ]) {
    assert.equal(inviteFromLocation(hash), null, `${hash} should not parse`);
  }
});

// --- Describing an invite --------------------------------------------------

const HOUR = 3_600_000;
const NOW = 1_700_000_000_000;

/** A live, unlimited, never-expiring invite, to be narrowed per test. */
function invite(over = {}) {
  return {
    id: "1",
    label: "",
    created_at: NOW,
    expires_at: null,
    max_uses: 0,
    uses: 0,
    live: true,
    ...over,
  };
}

test("an unlimited invite reports how many have joined", () => {
  assert.equal(
    describeInvite(invite({ uses: 3 }), NOW),
    "3 joined · never expires",
  );
});

test("a capped invite reports the fraction used", () => {
  assert.equal(
    describeInvite(invite({ uses: 2, max_uses: 5, expires_at: NOW + 3 * HOUR }), NOW),
    "2/5 used · 3h left",
  );
});

test("remaining time switches to days past two", () => {
  // Hours are the useful unit for a link expiring today and useless for one
  // expiring next month.
  assert.equal(
    describeInvite(invite({ expires_at: NOW + 47 * HOUR }), NOW).endsWith("47h left"),
    true,
  );
  assert.equal(
    describeInvite(invite({ expires_at: NOW + 72 * HOUR }), NOW).endsWith("3d left"),
    true,
  );
});

test("a dead invite says which way it died", () => {
  // Expired and exhausted call for different fixes — a longer link versus a
  // bigger one — so the two must not collapse into one word.
  assert.equal(
    describeInvite(
      invite({ live: false, expires_at: NOW - HOUR, max_uses: 5, uses: 1 }),
      NOW,
    ),
    "expired",
  );
  assert.equal(
    describeInvite(invite({ live: false, max_uses: 1, uses: 1 }), NOW),
    "used",
  );
  assert.equal(
    describeInvite(invite({ live: false, max_uses: 5, uses: 5 }), NOW),
    "all 5 uses taken",
  );
});

test("an invite that is both expired and spent reports the expiry", () => {
  // Arbitrary but deliberate: expiry is the condition that will not change if
  // the cap is raised, so it is the one worth naming.
  assert.equal(
    describeInvite(
      invite({ live: false, expires_at: NOW - HOUR, max_uses: 1, uses: 1 }),
      NOW,
    ),
    "expired",
  );
});

test("time left never goes negative on a live invite", () => {
  // Clock skew between the browser and the server can put a live invite's
  // expiry marginally in the past; "-1h left" would look like a bug.
  assert.equal(
    describeInvite(invite({ expires_at: NOW - 60_000, max_uses: 2 }), NOW),
    "0/2 used · 0h left",
  );
});
