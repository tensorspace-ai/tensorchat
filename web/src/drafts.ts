/**
 * Unsent message text, kept across reloads.
 *
 * A draft used to live in a `Map` inside the composer, which meant it survived
 * switching channels and nothing else. Closing the tab on a half-written
 * paragraph lost the paragraph — and the paragraph is the one thing in the
 * client the user cannot get back from the server.
 *
 * The whole set lives under a single `localStorage` key as one JSON object.
 * A key per draft would be tidier to write and worse to read: pruning would
 * mean scanning the entire storage area for a prefix on every load.
 *
 * Writes are debounced. `localStorage.setItem` is synchronous and hits disk, so
 * doing it per keystroke would put a blocking write on the typing path. See
 * [`scheduleFlush`] for why that is still crash-safe.
 */

import type { Id } from './protocol.ts';

const KEY = 'tc_drafts';

/** Wait after the last keystroke before writing. Long enough that a fast typist
 *  causes one write per pause rather than per character. */
const DEBOUNCE_MS = 400;

/** Matches the server's `MAX_BODY_BYTES`. Anything longer could never be sent,
 *  so storing it would only waste the storage budget. */
const MAX_DRAFT_BYTES = 16 * 1024;

/** How many channels' drafts to keep. Well past the number of conversations
 *  anyone has in flight; the cap exists so a long-lived tab cannot grow the
 *  entry unbounded. */
const MAX_DRAFTS = 50;

/** Drafts older than this are assumed abandoned. */
const MAX_AGE_MS = 30 * 24 * 60 * 60 * 1000;

type Draft = { t: string; at: number };
type DraftMap = Record<string, Draft>;

/** In-memory mirror, so reads never parse JSON and a failed write never
 *  desynchronizes what the composer is showing from what it thinks it saved. */
let cache: DraftMap | null = null;
let timer: ReturnType<typeof setTimeout> | null = null;

function read(): DraftMap {
  if (cache) return cache;
  cache = {};
  try {
    const raw = localStorage.getItem(KEY);
    if (raw) {
      const parsed: unknown = JSON.parse(raw);
      // Anything could be under this key — another tool, a corrupted write, an
      // older format. Validate per entry rather than trusting the shape.
      if (parsed && typeof parsed === 'object') {
        const now = Date.now();
        for (const [id, v] of Object.entries(parsed as Record<string, unknown>)) {
          const d = v as Partial<Draft>;
          if (typeof d?.t !== 'string' || typeof d?.at !== 'number') continue;
          if (now - d.at > MAX_AGE_MS) continue;
          cache[id] = { t: d.t, at: d.at };
        }
      }
    }
  } catch {
    // Private browsing, a full quota, or malformed JSON. Drafts are a
    // convenience; losing them must never keep the app from starting.
    cache = {};
  }
  return cache;
}

function write(): void {
  if (!cache) return;
  // Prune before writing rather than on read: this is the only moment the set
  // can have grown, and it keeps the stored object bounded rather than merely
  // the object we hand back.
  const ids = Object.keys(cache);
  if (ids.length > MAX_DRAFTS) {
    const byAge = ids.sort((a, b) => cache![b]!.at - cache![a]!.at);
    for (const id of byAge.slice(MAX_DRAFTS)) delete cache[id];
  }
  try {
    localStorage.setItem(KEY, JSON.stringify(cache));
  } catch {
    // Out of quota, or storage is unavailable. The in-memory cache still
    // carries the draft for this session.
  }
}

/**
 * Write soon, and definitely before the page goes away.
 *
 * The debounce means a reload within [`DEBOUNCE_MS`] of the last keystroke
 * would otherwise lose those keystrokes, so `pagehide` flushes synchronously.
 * `pagehide` rather than `beforeunload`: it fires for the bfcache and for a
 * mobile tab being backgrounded, both of which `beforeunload` misses.
 */
function scheduleFlush(): void {
  if (timer !== null) clearTimeout(timer);
  timer = setTimeout(() => {
    timer = null;
    write();
  }, DEBOUNCE_MS);
}

export function flushDrafts(): void {
  if (timer !== null) {
    clearTimeout(timer);
    timer = null;
  }
  write();
}

if (typeof addEventListener === 'function') {
  addEventListener('pagehide', flushDrafts);
  // Backgrounding a tab on mobile can kill it without ever firing `pagehide`.
  addEventListener('visibilitychange', () => {
    if (document.visibilityState === 'hidden') flushDrafts();
  });
}

export function loadDraft(key: Id): string {
  return read()[key]?.t ?? '';
}

/** Record a draft, or drop it when the text is empty. */
export function saveDraft(key: Id, text: string): void {
  const drafts = read();
  if (!text) {
    if (!(key in drafts)) return;
    delete drafts[key];
  } else {
    // Truncate by bytes, not characters: the server's limit is bytes, and a
    // draft of emoji is four times longer than its length suggests.
    let t = text;
    if (new TextEncoder().encode(t).length > MAX_DRAFT_BYTES) {
      t = t.slice(0, MAX_DRAFT_BYTES);
    }
    const existing = drafts[key];
    if (existing?.t === t) return;
    drafts[key] = { t, at: Date.now() };
  }
  scheduleFlush();
}

export function clearDraft(key: Id): void {
  saveDraft(key, '');
}

/**
 * Drop every draft. Called on sign-out, so unsent text does not sit in storage
 * for whoever uses the browser next.
 */
export function clearAllDrafts(): void {
  cache = {};
  if (timer !== null) {
    clearTimeout(timer);
    timer = null;
  }
  try {
    localStorage.removeItem(KEY);
  } catch {
    /* nothing to do */
  }
}
