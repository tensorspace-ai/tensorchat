// Tests for the dependency-free windowed scroller with DOM recycling.
//
// Run with: node --test web/test/signals.test.mjs web/test/virtual-list.test.mjs
// (a bare directory argument is broken on this Node build; always pass
// explicit file paths).
//
// There is no browser here, so this file implements a tiny fake DOM: plain
// objects exposing exactly the surface virtual-list.ts touches (scrollTop,
// scrollHeight, clientHeight, style, appendChild, removeChild, offsetHeight,
// addEventListener, removeEventListener, children). `requestAnimationFrame`
// is stubbed to run its callback synchronously so scroll handling can be
// tested without waiting a real frame; `ResizeObserver` is simply absent
// (as it is by default in Node), which exercises virtual-list.ts's
// fallback path for environments without it.

import { test } from "node:test";
import assert from "node:assert/strict";
import { VirtualList } from "../src/virtual-list.ts";

globalThis.requestAnimationFrame = (cb) => {
  cb();
  return 0;
};
globalThis.cancelAnimationFrame = () => {};
// Explicit, even though Node has no such global by default: virtual-list.ts
// must work with no ResizeObserver at all.
globalThis.ResizeObserver = undefined;

// ---------------------------------------------------------------------------
// Fake DOM
// ---------------------------------------------------------------------------

function makeElement(overrides = {}) {
  const el = {
    style: {},
    children: [],
    offsetHeight: 0,
    scrollTop: 0,
    scrollHeight: 0,
    clientHeight: 0,
    _listeners: new Map(),
    appendChild(child) {
      this.children.push(child);
    },
    removeChild(child) {
      const i = this.children.indexOf(child);
      if (i >= 0) this.children.splice(i, 1);
    },
    addEventListener(type, fn) {
      if (!this._listeners.has(type)) this._listeners.set(type, []);
      this._listeners.get(type).push(fn);
    },
    removeEventListener(type, fn) {
      const arr = this._listeners.get(type);
      if (!arr) return;
      const i = arr.indexOf(fn);
      if (i >= 0) arr.splice(i, 1);
    },
    _fire(type) {
      for (const fn of this._listeners.get(type) ?? []) fn();
    },
    ...overrides,
  };
  return el;
}

function makeViewport(clientHeight) {
  return makeElement({ clientHeight, scrollTop: 0 });
}

function makeContent() {
  return makeElement();
}

function makeItems(n, height, prefix = "m") {
  const items = [];
  for (let i = 0; i < n; i++) items.push({ id: `${prefix}${i}`, height });
  return items;
}

/** A harness bundling a fresh viewport/content pair with call counters. */
function harness({ clientHeight = 100, estimateHeight = 20, overscan = 4, withUpdateRow = true } = {}) {
  const viewport = makeViewport(clientHeight);
  const content = makeContent();
  let renderCalls = 0;
  let updateCalls = 0;
  const rowsByKey = new Map();

  const vl = new VirtualList({
    viewport,
    content,
    estimateHeight,
    overscan,
    key: (item) => item.id,
    renderRow: (item, index) => {
      renderCalls++;
      const el = makeElement({ offsetHeight: item.height, _id: item.id });
      rowsByKey.set(item.id, el);
      return el;
    },
    updateRow: withUpdateRow
      ? (el, item, index) => {
          updateCalls++;
          el.offsetHeight = item.height;
          el._id = item.id;
          rowsByKey.set(item.id, el);
        }
      : undefined,
  });

  return {
    vl,
    viewport,
    content,
    rowsByKey,
    counts: () => ({ renderCalls, updateCalls }),
    mountedIds: () => content.children.map((el) => el._id),
  };
}

// ---------------------------------------------------------------------------
// Windowing / binary search / total height
// ---------------------------------------------------------------------------

test("mounts only a bounded window of rows for a 10,000-item list", () => {
  const { vl, content, counts } = harness({ clientHeight: 600, estimateHeight: 20, overscan: 4 });
  const items = makeItems(10000, 20);
  vl.setItems(items);

  // Uniform 20px rows, 600px viewport: ~30 visible + 2*4 overscan = 38 rows.
  // firstVisible=0, lastVisible=30 (offsets[30]=600<=600), start=0, end=34.
  assert.equal(content.children.length, 35);
  assert.equal(counts().renderCalls, 35, "must not create a row per item for a 10k-item list");
});

test("binary search picks the correct window for a given scrollTop, with mixed measured/estimated heights", () => {
  const { vl, content, viewport, mountedIds } = harness({ clientHeight: 50, estimateHeight: 20, overscan: 0 });
  // Distinct real heights for items 0..2 (measured once mounted); the rest
  // match estimateHeight exactly, so whether or not they have been mounted
  // yet is irrelevant to the expected offsets — only items 0..2 make the
  // "measured vs. estimated" distinction observable.
  const items = makeItems(20, 20);
  items[0].height = 15;
  items[1].height = 20;
  items[2].height = 25;

  vl.setItems(items);
  // offsets: 0,15,35,60 ... initial window at scrollTop=0, viewport 50:
  // firstVisible=0, lastVisible = findIndexAtOffset(50) -> offsets[2]=35<=50, offsets[3]=60>50 -> 2.
  assert.deepEqual(mountedIds(), ["m0", "m1", "m2"]);

  // Now scroll to 85. Offsets beyond index 2 use estimateHeight (20):
  // offsets: 0(m0) 15(m1) 35(m2) 60(m3) 80(m4) 100(m5) 120(m6) 140(m7)...
  // findIndexAtOffset(85) -> largest i with offsets[i]<=85 -> offsets[4]=80<=85 -> i=4.
  // findIndexAtOffset(85+50=135) -> offsets[6]=120<=135, offsets[7]=140>135 -> i=6.
  viewport.scrollTop = 85;
  viewport._fire("scroll");
  assert.deepEqual(mountedIds(), ["m4", "m5", "m6"]);

  const m4 = content.children.find((el) => el._id === "m4");
  assert.equal(m4.style.top, "80px");
});

test("total content height equals the sum of all row heights", () => {
  const { vl, content } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  const items = makeItems(30, 20);
  items[5].height = 50; // will be measured as 50 once mounted (it's within the initial window)
  vl.setItems(items);

  const expectedTotal = items.reduce((sum, it, i) => sum + (i === 5 ? 50 : 20), 0);
  assert.equal(content.style.height, `${expectedTotal}px`);
});

// ---------------------------------------------------------------------------
// DOM recycling
// ---------------------------------------------------------------------------

test("scrolling by one row reuses mounted elements: renderRow is not re-invoked for keys still visible", () => {
  const { vl, viewport, counts } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 0 });
  const items = makeItems(50, 20);
  vl.setItems(items);
  const before = counts().renderCalls;

  viewport.scrollTop = 20; // scroll down exactly one row
  viewport._fire("scroll");
  const after = counts();

  // The single newly-revealed row is filled via the pooled element the
  // row that scrolled out leaves behind (updateRow), so renderRow must not
  // be called again at all for this scroll step.
  assert.equal(after.renderCalls, before, "no key still visible (or newly revealed via recycling) should be freshly rendered");
});

test("updateRow is called instead of renderRow when reusing a pooled element for a new key", () => {
  const { vl, viewport, counts } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 0 });
  const items = makeItems(50, 20);
  vl.setItems(items);
  const before = counts();

  viewport.scrollTop = 20;
  viewport._fire("scroll");
  const after = counts();

  assert.equal(after.renderCalls, before.renderCalls, "the row leaving the window is recycled, not discarded");
  assert.equal(after.updateCalls, before.updateCalls + 1, "the newly-revealed row reuses a pooled element via updateRow");
});

test("scrolling far across a 10,000-item list keeps the total created-element count bounded", () => {
  const { vl, viewport, counts } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  const items = makeItems(10000, 20);
  vl.setItems(items);
  const initial = counts().renderCalls;

  for (const idx of [1000, 3000, 5000, 7000, 9000, 500, 8000]) {
    vl.scrollToIndex(idx, "start");
  }

  const final = counts().renderCalls;
  assert.ok(final < initial * 3, `created-element count must stay bounded, got ${initial} -> ${final}`);
  assert.ok(final < 200, "must be nowhere near proportional to the 10,000-item list or the scroll distance");
});

test("never re-creates an element for a key that is already mounted", () => {
  const { vl, counts } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  const items = makeItems(20, 20);
  vl.setItems(items);
  const before = counts().renderCalls;
  vl.setItems(items.slice()); // same keys, new array reference
  assert.equal(counts().renderCalls, before, "re-supplying the same items must not re-render already-mounted rows");
});

// ---------------------------------------------------------------------------
// Chat semantics: anchoring and auto-scroll
// ---------------------------------------------------------------------------

test("prepending items anchors the previously-topmost row at the same visual offset", () => {
  const { vl, viewport, content } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  const items = makeItems(20, 20);
  vl.setItems(items);

  viewport.scrollTop = 100; // topmost visible row is m5 (offset 100)
  viewport._fire("scroll");
  assert.equal(content.children.some((el) => el._id === "m5"), true);

  const prepended = makeItems(10, 20, "p");
  vl.setItems([...prepended, ...items]);

  // m5's new index is 10 + 5 = 15, uniform height 20 -> new offset 300.
  // anchorOffsetInViewport was 100 (row offset) - 100 (scrollTop) = 0,
  // so the restored scrollTop must be exactly 300.
  assert.equal(viewport.scrollTop, 300, "scrollTop must be restored so the anchor row does not visually jump");

  const anchorRow = content.children.find((el) => el._id === "m5");
  assert.ok(anchorRow, "the anchor row must still be mounted after the prepend");
  assert.equal(anchorRow.style.top, "300px");
});

test("appending while pinned to the bottom auto-scrolls to the new bottom", () => {
  const { vl, viewport } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  const items = makeItems(20, 20); // total height 400
  vl.setItems(items);
  vl.scrollToBottom();
  assert.equal(viewport.scrollTop, 300); // 400 - 100
  assert.equal(vl.isPinnedToBottom(), true);

  vl.setItems([...items, ...makeItems(5, 20, "n")]); // total height now 500
  assert.equal(viewport.scrollTop, 400, "must auto-scroll to the new bottom (500 - 100)");
});

test("appending while scrolled away from the bottom does not auto-scroll", () => {
  const { vl, viewport } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  const items = makeItems(20, 20);
  vl.setItems(items);
  assert.equal(viewport.scrollTop, 0);
  assert.equal(vl.isPinnedToBottom(), false);

  vl.setItems([...items, ...makeItems(5, 20, "n")]);
  assert.equal(viewport.scrollTop, 0, "must stay anchored at the top, not jump to the bottom");
});

test("isPinnedToBottom is true within ~40px of the bottom and false further away", () => {
  const { vl, viewport } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  const items = makeItems(20, 20); // total height 400
  vl.setItems(items);

  viewport.scrollTop = 400 - 100; // exactly at the bottom
  assert.equal(vl.isPinnedToBottom(), true);

  viewport.scrollTop = 400 - 100 - 40; // right at the threshold
  assert.equal(vl.isPinnedToBottom(), true);

  viewport.scrollTop = 400 - 100 - 41;
  assert.equal(vl.isPinnedToBottom(), false);
});

// ---------------------------------------------------------------------------
// scrollToIndex
// ---------------------------------------------------------------------------

test("scrollToIndex('start') scrolls the row to the top of the viewport", () => {
  const { vl, viewport } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  vl.setItems(makeItems(20, 20));
  vl.scrollToIndex(10, "start");
  assert.equal(viewport.scrollTop, 200);
});

test("scrollToIndex('end') scrolls the row to the bottom of the viewport", () => {
  const { vl, viewport } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  vl.setItems(makeItems(20, 20));
  vl.scrollToIndex(10, "end");
  // offset(200) + height(20) - clientHeight(100) = 120
  assert.equal(viewport.scrollTop, 120);
});

// ---------------------------------------------------------------------------
// invalidate()
// ---------------------------------------------------------------------------

test("invalidate re-measures a mounted row and shifts scrollTop when it is above the viewport", () => {
  const { vl, viewport, rowsByKey } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 6 });
  const items = makeItems(20, 20);
  vl.setItems(items);

  viewport.scrollTop = 60; // row 0 stays mounted: start = max(0, 3-6) = 0
  viewport._fire("scroll");

  const row0 = rowsByKey.get("m0");
  row0.offsetHeight = 50; // simulate content growth (e.g. an image finished loading)
  vl.invalidate(0);

  assert.equal(viewport.scrollTop, 90, "scrollTop must shift by the +30px delta so visible content does not move");
});

test("invalidate on a currently off-screen row does not crash and just marks it stale", () => {
  const { vl, viewport } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 0 });
  const items = makeItems(50, 20);
  vl.setItems(items);
  viewport.scrollTop = 800; // row 0 long gone from the window
  viewport._fire("scroll");
  assert.doesNotThrow(() => vl.invalidate(0));
});

// ---------------------------------------------------------------------------
// destroy() / edge cases
// ---------------------------------------------------------------------------

test("destroy removes the scroll listener so further scroll events do not re-render", () => {
  const { vl, viewport, counts } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  vl.setItems(makeItems(50, 20));
  const before = counts().renderCalls;

  vl.destroy();
  viewport.scrollTop = 500;
  viewport._fire("scroll");

  assert.equal(counts().renderCalls, before, "no re-render must happen after destroy");
  assert.equal(viewport._listeners.get("scroll")?.length ?? 0, 0, "the scroll listener must be detached");
});

test("empty list does not crash and clears mounted rows", () => {
  const { vl, content } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  assert.doesNotThrow(() => vl.setItems(makeItems(10, 20)));
  assert.doesNotThrow(() => vl.setItems([]));
  assert.equal(content.children.length, 0);
  assert.equal(content.style.height, "0px");
});

test("single-item list mounts exactly one row without crashing", () => {
  const { vl, content } = harness({ clientHeight: 100, estimateHeight: 20, overscan: 2 });
  assert.doesNotThrow(() => vl.setItems(makeItems(1, 20)));
  assert.equal(content.children.length, 1);
});
