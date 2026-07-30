/**
 * Dependency-free windowed scroller with DOM element recycling.
 *
 * Renders only the slice of a (potentially huge — 100k+ row) list that is
 * actually near the viewport, so a chat channel scrolls at 60fps regardless
 * of history length.
 *
 * ## Layout strategy (no spacer element)
 *
 * Rather than a separate spacer element to reserve scroll height, `content`
 * itself is given `style.height` equal to the sum of all row heights, and
 * every mounted row is absolutely positioned inside it via `style.top`.
 * This means the class never calls `document.createElement` itself — rows
 * come exclusively from the caller's `renderRow`/`updateRow` callbacks,
 * which keeps this module trivially testable with a plain-object fake DOM
 * (see the test file) instead of a real browser.
 *
 * ## Variable row heights: measured-height array + prefix sums
 *
 * `heights[i]` holds the best known height for row `i` (a real measurement
 * once mounted at least once, `estimateHeight` until then). `offsets[i]` is
 * the prefix sum (`offsets[0] = 0`, `offsets[i+1] = offsets[i] + heights[i]`),
 * rebuilt in one O(n) pass whenever any height actually changes — never on
 * every scroll frame. "Which rows are visible at scrollTop S" is then a
 * binary search over `offsets` (`findIndexAtOffset`), not a linear scan,
 * which is what keeps `setItems`/scroll handling cheap even at 100k rows.
 *
 * ## DOM recycling
 *
 * `mounted: Map<key, {el, index}>` tracks currently-attached rows by the
 * caller's stable `key`. On every render pass we compute the desired set
 * of keys for the new window; anything mounted that is no longer desired
 * is detached and pushed onto `pool` (only if `updateRow` was supplied —
 * without it there is no way to refresh a recycled element's content, so
 * recycling would just show stale rows). A desired key already mounted is
 * left alone (optionally refreshed via `updateRow`); a desired key that
 * isn't mounted pulls an element from `pool` (via `updateRow`) or, failing
 * that, calls `renderRow` to create one. An element for an already-mounted
 * key is never re-created.
 *
 * ## Chat semantics
 *
 * - `isPinnedToBottom()`: within ~40px of the bottom.
 * - `setItems()` captures, *before* mutating any state, (a) whether the
 *   viewport was pinned to the bottom and (b) an "anchor": the key of the
 *   row currently at the top of the viewport and its pixel offset within
 *   the viewport. After recomputing heights/offsets for the new item list,
 *   if the viewport was pinned we scroll to the new bottom (auto-scroll on
 *   append); otherwise, if the anchor key still exists, we restore
 *   `scrollTop` so that row sits at exactly the same pixel offset it had
 *   before — this is what makes prepending older history (the common
 *   "load more" case in chat) not jump the content the user is reading.
 * - When a mounted row's measured height changes (via `invalidate()` or a
 *   `ResizeObserver` callback) and that row sits above the current
 *   `scrollTop`, `scrollTop` is shifted by the same delta so visible
 *   content does not move under the user.
 *
 * ## Measurement
 *
 * Rows are measured with `offsetHeight` right after being mounted. A
 * `ResizeObserver` (when the global exists — it is optional and simply
 * unused otherwise) is attached to every mounted row to catch later
 * layout changes such as an image finishing loading.
 *
 * ## Scroll handling
 *
 * The `scroll` listener never does layout work itself; it just schedules a
 * single `requestAnimationFrame` callback (coalescing any number of scroll
 * events into one re-render per frame).
 */

export type VirtualListOptions<T> = {
  viewport: HTMLElement;
  content: HTMLElement;
  estimateHeight: number;
  overscan?: number;
  key: (item: T, index: number) => string;
  renderRow: (item: T, index: number) => HTMLElement;
  updateRow?: (el: HTMLElement, item: T, index: number) => void;
};

interface MountedRow {
  el: HTMLElement;
  index: number;
  key: string;
}

interface Anchor {
  key: string;
  offsetInViewport: number;
}

/** How close to the bottom (in px) counts as "pinned". */
const PIN_THRESHOLD = 40;

function raf(cb: () => void): number {
  if (typeof requestAnimationFrame === "function") {
    return requestAnimationFrame(cb) as unknown as number;
  }
  cb();
  return -1;
}

function cancelRaf(handle: number): void {
  if (handle >= 0 && typeof cancelAnimationFrame === "function") {
    cancelAnimationFrame(handle);
  }
}

export class VirtualList<T> {
  private opts: VirtualListOptions<T>;
  private overscan: number;

  private items: T[] = [];
  private heights: number[] = [];
  private measured: boolean[] = [];
  private offsets: number[] = [0];

  private mounted: Map<string, MountedRow> = new Map();
  private pool: HTMLElement[] = [];
  private elementKeys: WeakMap<HTMLElement, string> = new WeakMap();

  private resizeObserver: ResizeObserver | undefined;
  private onScroll: () => void;
  private rafScheduled = false;
  private rafHandle = -1;
  private destroyed = false;

  constructor(opts: VirtualListOptions<T>) {
    this.opts = opts;
    this.overscan = opts.overscan ?? 4;
    this.opts.content.style.position = "relative";

    this.onScroll = (): void => this.scheduleRender();
    this.opts.viewport.addEventListener("scroll", this.onScroll);

    if (typeof ResizeObserver !== "undefined") {
      this.resizeObserver = new ResizeObserver((entries) => this.handleResize(entries));
    }
  }

  // -------------------------------------------------------------------
  // Public API
  // -------------------------------------------------------------------

  setItems(items: T[]): void {
    if (this.destroyed) return;

    // Only an update to an already-populated list can be "pinned" (there is
    // no prior scroll position to maintain the very first time items are
    // set — isPinnedToBottom() would trivially say yes for an empty list,
    // which would otherwise auto-scroll a freshly-loaded list to its
    // bottom on the very first render). Callers that want that chat-style
    // "open scrolled to the latest message" behavior can call
    // scrollToBottom() themselves right after the first setItems().
    const wasPinned = this.items.length > 0 && this.isPinnedToBottom();
    const anchor = this.captureAnchor();

    const oldHeightByKey = new Map<string, number>();
    for (let i = 0; i < this.items.length; i++) {
      oldHeightByKey.set(this.opts.key(this.items[i] as T, i), this.heights[i]!);
    }

    const n = items.length;
    const heights = new Array<number>(n);
    const measured = new Array<boolean>(n);
    const newIndexByKey = new Map<string, number>();
    for (let i = 0; i < n; i++) {
      const key = this.opts.key(items[i] as T, i);
      newIndexByKey.set(key, i);
      const carried = oldHeightByKey.get(key);
      if (carried !== undefined) {
        heights[i] = carried;
        measured[i] = true;
      } else {
        heights[i] = this.opts.estimateHeight;
        measured[i] = false;
      }
    }

    this.items = items;
    this.heights = heights;
    this.measured = measured;
    this.rebuildOffsets();

    const total = this.offsets[n]!;
    const maxScroll = Math.max(0, total - this.opts.viewport.clientHeight);

    if (wasPinned) {
      this.opts.viewport.scrollTop = maxScroll;
    } else if (anchor !== null && newIndexByKey.has(anchor.key)) {
      const newIndex = newIndexByKey.get(anchor.key)!;
      const target = this.offsets[newIndex]! - anchor.offsetInViewport;
      this.opts.viewport.scrollTop = Math.max(0, Math.min(target, maxScroll));
    } else {
      this.opts.viewport.scrollTop = Math.max(0, Math.min(this.opts.viewport.scrollTop, maxScroll));
    }

    this.renderVisible();
  }

  /**
   * Re-render every currently-mounted row from the latest items, then
   * re-measure.
   *
   * `renderVisible` deliberately leaves an already-mounted, still-desired row
   * untouched — otherwise a stable row would be rebuilt on every scroll frame,
   * which is the whole point of keeping it mounted. That is right for
   * *scrolling* and wrong for a *content* change: a reaction landing on a
   * message that is already on screen, or a pin, or an inline editor opening,
   * changes what a row should show without changing the set of rows. Without
   * this, such a change only appeared once the row happened to be recycled by
   * scrolling past it.
   *
   * Callers invoke this after `setItems` when item *contents* may have changed,
   * not just the item set. It touches only mounted rows — a dozen or so — so it
   * is cheap enough to call on every data change; it is scroll frames, not data
   * changes, that this class is careful about.
   *
   * A no-op without `updateRow`: there would be no way to refresh an element in
   * place, and re-creating it would defeat the recycling this exists to serve.
   */
  refresh(): void {
    if (this.destroyed || this.opts.updateRow === undefined) return;
    const indices: number[] = [];
    for (const row of this.mounted.values()) {
      const item = this.items[row.index];
      if (item === undefined) continue;
      this.opts.updateRow(row.el, item, row.index);
      indices.push(row.index);
    }
    if (indices.length > 0) this.measureRows(indices);
  }

  /** Re-measure one row (if mounted), or every currently-mounted row. */
  invalidate(index?: number): void {
    if (this.destroyed) return;
    if (index === undefined) {
      const indices: number[] = [];
      for (const row of this.mounted.values()) indices.push(row.index);
      for (let i = 0; i < this.measured.length; i++) this.measured[i] = false;
      this.measureRows(indices);
      return;
    }
    if (index < 0 || index >= this.items.length) return;
    const key = this.opts.key(this.items[index] as T, index);
    const row = this.mounted.get(key);
    if (row !== undefined) {
      row.index = index;
      this.measureRows([index]);
    } else {
      this.measured[index] = false;
    }
  }

  scrollToIndex(index: number, align: "start" | "end" = "start"): void {
    if (this.destroyed) return;
    const n = this.items.length;
    if (n === 0) return;
    const clamped = Math.max(0, Math.min(index, n - 1));
    const total = this.offsets[n]!;
    const maxScroll = Math.max(0, total - this.opts.viewport.clientHeight);
    const target =
      align === "end"
        ? this.offsets[clamped]! + this.heights[clamped]! - this.opts.viewport.clientHeight
        : this.offsets[clamped]!;
    this.opts.viewport.scrollTop = Math.max(0, Math.min(target, maxScroll));
    this.renderVisible();
  }

  scrollToBottom(): void {
    if (this.destroyed) return;
    if (this.items.length === 0) return;
    this.scrollToIndex(this.items.length - 1, "end");
  }

  isPinnedToBottom(): boolean {
    const total = this.offsets[this.items.length] ?? 0;
    if (total === 0) return true;
    const viewport = this.opts.viewport;
    const distanceFromBottom = total - (viewport.scrollTop + viewport.clientHeight);
    return distanceFromBottom <= PIN_THRESHOLD;
  }

  destroy(): void {
    if (this.destroyed) return;
    this.destroyed = true;
    this.opts.viewport.removeEventListener("scroll", this.onScroll);
    if (this.rafScheduled) cancelRaf(this.rafHandle);
    this.rafScheduled = false;
    if (this.resizeObserver !== undefined) this.resizeObserver.disconnect();
    this.unmountAll();
  }

  // -------------------------------------------------------------------
  // Rendering
  // -------------------------------------------------------------------

  private scheduleRender(): void {
    if (this.rafScheduled) return;
    this.rafScheduled = true;
    this.rafHandle = raf(() => {
      this.rafScheduled = false;
      if (!this.destroyed) this.renderVisible();
    });
  }

  private renderVisible(): void {
    const n = this.items.length;
    if (n === 0) {
      this.unmountAll();
      this.opts.content.style.height = "0px";
      return;
    }

    const viewportHeight = this.opts.viewport.clientHeight;
    const scrollTop = this.opts.viewport.scrollTop;
    const firstVisible = this.findIndexAtOffset(scrollTop);
    const lastVisible = this.findIndexAtOffset(scrollTop + viewportHeight);
    const start = Math.max(0, firstVisible - this.overscan);
    const end = Math.min(n - 1, lastVisible + this.overscan);

    const desiredKeys = new Set<string>();
    for (let i = start; i <= end; i++) {
      desiredKeys.add(this.opts.key(this.items[i] as T, i));
    }

    // Sweep rows that left the window: detach, recycle if possible.
    for (const [key, row] of this.mounted) {
      if (desiredKeys.has(key)) continue;
      this.opts.content.removeChild(row.el);
      if (this.resizeObserver !== undefined) this.resizeObserver.unobserve(row.el);
      if (this.opts.updateRow !== undefined) this.pool.push(row.el);
      this.mounted.delete(key);
    }

    const toMeasure: number[] = [];
    for (let i = start; i <= end; i++) {
      const item = this.items[i] as T;
      const key = this.opts.key(item, i);
      let row = this.mounted.get(key);
      if (row !== undefined) {
        // Already mounted and still desired: leave it exactly as-is. Per
        // updateRow's contract ("reuse an existing element") it only
        // fires when an element is (re)claimed from the pool below, not
        // on every pass for a row that never left the window — otherwise
        // a stable, continuously-visible row would be re-rendered on
        // every single scroll frame, which defeats the point of keeping
        // it mounted in the first place.
        row.index = i;
      } else {
        let el: HTMLElement;
        if (this.opts.updateRow !== undefined && this.pool.length > 0) {
          el = this.pool.pop()!;
          this.opts.updateRow(el, item, i);
        } else {
          el = this.opts.renderRow(item, i);
        }
        this.opts.content.appendChild(el);
        if (this.resizeObserver !== undefined) this.resizeObserver.observe(el);
        this.elementKeys.set(el, key);
        row = { el, index: i, key };
        this.mounted.set(key, row);
      }
      this.positionRow(row);
      if (!this.measured[i]) toMeasure.push(i);
    }

    if (toMeasure.length > 0) this.measureRows(toMeasure);

    this.opts.content.style.height = `${this.offsets[n]}px`;
  }

  private positionRow(row: MountedRow): void {
    row.el.style.position = "absolute";
    row.el.style.top = `${this.offsets[row.index]}px`;
    row.el.style.left = "0";
    row.el.style.right = "0";
  }

  private unmountAll(): void {
    for (const row of this.mounted.values()) {
      this.opts.content.removeChild(row.el);
      if (this.resizeObserver !== undefined) this.resizeObserver.unobserve(row.el);
    }
    this.mounted.clear();
  }

  // -------------------------------------------------------------------
  // Measurement
  // -------------------------------------------------------------------

  private measureRows(indices: number[]): void {
    const changes = new Map<number, number>();
    for (const i of indices) {
      if (i < 0 || i >= this.items.length) continue;
      const key = this.opts.key(this.items[i] as T, i);
      const row = this.mounted.get(key);
      if (row === undefined) continue;
      const h = row.el.offsetHeight;
      if (typeof h === "number" && h >= 0 && h !== this.heights[i]) {
        changes.set(i, h);
      }
      this.measured[i] = true;
    }
    if (changes.size > 0) this.applyHeightChanges(changes);
  }

  private handleResize(entries: ResizeObserverEntry[]): void {
    if (this.destroyed) return;
    const changes = new Map<number, number>();
    for (const entry of entries) {
      const el = entry.target as HTMLElement;
      const key = this.elementKeys.get(el);
      if (key === undefined) continue;
      const row = this.mounted.get(key);
      if (row === undefined) continue;
      const h = el.offsetHeight;
      if (typeof h === "number" && h >= 0 && h !== this.heights[row.index]) {
        changes.set(row.index, h);
      }
      this.measured[row.index] = true;
    }
    if (changes.size > 0) this.applyHeightChanges(changes);
  }

  /**
   * Apply newly-measured heights, rebuild the prefix-sum offsets in one
   * O(n) pass, and — if any changed row sits above the current scrollTop —
   * shift scrollTop by the same total delta so visible content does not
   * move under the user. Finishes with a full re-render so mounted rows
   * end up repositioned (and the window re-evaluated) against the new
   * offsets.
   */
  private applyHeightChanges(changes: Map<number, number>): void {
    if (changes.size === 0) return;
    const scrollTopBefore = this.opts.viewport.scrollTop;
    let compensation = 0;
    for (const [index, newHeight] of changes) {
      const oldHeight = this.heights[index]!;
      const oldOffset = this.offsets[index]!;
      this.heights[index] = newHeight;
      if (oldOffset < scrollTopBefore) compensation += newHeight - oldHeight;
    }
    this.rebuildOffsets();
    if (compensation !== 0) {
      this.opts.viewport.scrollTop = scrollTopBefore + compensation;
    }
    this.renderVisible();
  }

  private rebuildOffsets(): void {
    const n = this.heights.length;
    const offsets = new Array<number>(n + 1);
    offsets[0] = 0;
    for (let i = 0; i < n; i++) offsets[i + 1] = offsets[i]! + this.heights[i]!;
    this.offsets = offsets;
  }

  /**
   * Binary search: the largest row index whose start offset is <= target
   * (clamped into range). O(log n) — this is what keeps window recompute
   * cheap even for a 100k-row list, instead of a linear scan.
   */
  private findIndexAtOffset(target: number): number {
    const n = this.items.length;
    if (n === 0) return 0;
    const offsets = this.offsets;
    let lo = 0;
    let hi = n - 1;
    while (lo < hi) {
      const mid = (lo + hi + 1) >> 1;
      if (offsets[mid]! <= target) lo = mid;
      else hi = mid - 1;
    }
    return lo;
  }

  /**
   * Record, before any state mutation, which row currently sits at the top
   * of the viewport and how far below the viewport's own top edge it sits.
   * Used by setItems() to keep that row visually stationary across a
   * prepend/append.
   */
  private captureAnchor(): Anchor | null {
    const n = this.items.length;
    if (n === 0) return null;
    const scrollTop = this.opts.viewport.scrollTop;
    const idx = this.findIndexAtOffset(scrollTop);
    const key = this.opts.key(this.items[idx] as T, idx);
    const offsetInViewport = this.offsets[idx]! - scrollTop;
    return { key, offsetInViewport };
  }
}
