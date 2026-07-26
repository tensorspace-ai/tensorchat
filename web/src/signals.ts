/**
 * Dependency-free, fine-grained reactive core (signals / computed / effect).
 *
 * This is the backbone of the whole UI — there is no framework sitting on
 * top of it — so it is written to be small, allocation-light on the hot
 * read/write paths, and glitch-free (an effect or computed never observes a
 * partially-updated dependency graph).
 *
 * ## Model
 *
 * Every signal, computed, and effect is backed by the same internal `Node`
 * shape (see below). A node can play a "source" role (something can depend
 * on it: signals and computeds), a "consumer" role (it depends on things:
 * computeds and effects), or both (computed is both at once).
 *
 * - A **signal** is a source-only node. Writes that produce an equal value
 *   (per `Object.is` or a custom comparator) are no-ops and never notify
 *   anyone.
 * - A **computed** is lazy and cached: its body does not run until read,
 *   and re-running is skipped unless a dependency has changed since the
 *   last read. It is implemented as a node that is both a source (others
 *   subscribe to its cached value) and a consumer (it subscribes to the
 *   signals/computeds it reads).
 * - An **effect** is a consumer-only node. It runs eagerly once at
 *   creation, then again whenever a signal it read last time changes.
 *   Dependencies are re-collected on every run, so a conditional branch
 *   that stops reading a signal also stops being notified by it
 *   (stale-dependency cleanup).
 *
 * ## Glitch-free propagation: push-dirty, pull-recompute
 *
 * Writing a signal walks its subscriber graph *synchronously*: every
 * downstream computed is marked `dirty` immediately (but not recomputed —
 * that stays lazy), and every downstream *effect* reached in the walk is
 * collected into a de-duplicated queue to run once. Because marking is
 * eager while recomputation stays lazy, an effect that reads a computed
 * during its run always triggers a fresh, consistent recompute right then
 * — there is no way to observe a computed whose cached value lags behind
 * a signal it depends on. This also means a diamond dependency (two
 * computeds derived from the same signal, both read by one effect) only
 * ever schedules that effect once per write, because the walk de-dupes by
 * node identity.
 *
 * Subscriptions are established *eagerly at read time* (not after a run
 * completes): the moment a node reads a source inside a tracked run, it is
 * subscribed immediately. This matters for effects that write a signal
 * they just read (a bounded self-referential update): the write must see
 * the subscription that was just established a few lines earlier in the
 * same run, or the resulting re-run would never be scheduled. Stale
 * dependencies (read last time, not this time) are unsubscribed once the
 * run finishes, by diffing against the previous run's dependency list.
 *
 * ## Batching and re-entrancy
 *
 * `batch()` defers effect execution until the outermost batch exits, so
 * multiple writes coalesce into one flush. Flushing itself is a single
 * `pending` queue drained to a fixpoint: an effect that writes a signal
 * during its own run adds more work to the *same* queue rather than
 * recursing, so cascades (and bounded self-triggering effects) resolve
 * within one flush. An unconditional self-retriggering effect is capped at
 * `MAX_FLUSH_ITERATIONS` runs per flush and then throws — this codebase's
 * chosen answer to "must not infinite-loop on unrelated signals" is: loops
 * that terminate on their own work fine, loops that don't are capped and
 * reported rather than hanging the tab.
 */

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

export type Signal<T> = {
  (): T;
  set(v: T): void;
  update(f: (prev: T) => T): void;
  peek(): T;
};

type EqualsFn<T = unknown> = (a: T, b: T) => boolean;

function defaultEquals(a: unknown, b: unknown): boolean {
  return Object.is(a, b);
}

/** Cap on effect re-runs within a single flush; see module doc above. */
const MAX_FLUSH_ITERATIONS = 1000;

// ---------------------------------------------------------------------------
// Internal node shape (shared by signal / computed / effect)
// ---------------------------------------------------------------------------

interface Node {
  // --- source role (signals and computeds) ---
  value: unknown;
  equals: EqualsFn;
  subscribers: Set<Node>;
  dirty: boolean; // computed only: cached value may be stale
  hasValue: boolean; // computed only: has computed at least once

  // --- consumer role (computeds and effects) ---
  isEffect: boolean;
  fn: (() => unknown) | null;
  deps: Node[];
  cleanups: (() => void)[] | null;
  running: boolean;
  disposed: boolean;
}

// The node currently executing (whose reads should be tracked), or null
// when running plain top-level code / inside untrack().
let currentConsumer: Node | null = null;

// Batching state. While batchDepth > 0, scheduled effects sit in `pending`
// instead of running immediately. `draining` guards against a nested flush
// attempt starting a second concurrent pass over the same queue.
let batchDepth = 0;
let draining = false;
const pending = new Set<Node>();

// ---------------------------------------------------------------------------
// Dependency tracking
// ---------------------------------------------------------------------------

/**
 * Record a read of `node` against the currently-running consumer, if any,
 * subscribing immediately (see module doc: eager subscribe-at-read).
 */
function trackRead(node: Node): void {
  const consumer = currentConsumer;
  if (consumer === null) return;
  if (!consumer.deps.includes(node)) {
    consumer.deps.push(node);
    node.subscribers.add(consumer);
  }
}

function runNodeCleanups(node: Node): void {
  const cleanups = node.cleanups;
  if (cleanups === null) return;
  node.cleanups = null;
  for (let i = cleanups.length - 1; i >= 0; i--) {
    cleanups[i]!();
  }
}

function registerCleanup(node: Node, fn: () => void): void {
  if (node.cleanups === null) node.cleanups = [];
  node.cleanups.push(fn);
}

/**
 * Run `body` with `node` as the active consumer, tracking every source it
 * reads. Cleanups from the previous run fire first. Once `body` returns,
 * any dependency read last time but not this time is unsubscribed.
 */
function trackedRun(node: Node, body: () => void): void {
  if (node.running) {
    throw new Error("signals: cyclic update (node re-entered while already running)");
  }
  runNodeCleanups(node);

  const prevDeps = node.deps;
  node.deps = [];
  const prevConsumer = currentConsumer;
  currentConsumer = node;
  node.running = true;
  try {
    body();
  } finally {
    currentConsumer = prevConsumer;
    node.running = false;
  }

  for (let i = 0; i < prevDeps.length; i++) {
    const dep = prevDeps[i]!;
    if (!node.deps.includes(dep)) dep.subscribers.delete(node);
  }
}

function unsubscribeAllDeps(node: Node): void {
  for (let i = 0; i < node.deps.length; i++) node.deps[i]!.subscribers.delete(node);
  node.deps.length = 0;
}

// ---------------------------------------------------------------------------
// Write propagation
// ---------------------------------------------------------------------------

/**
 * A source's value just changed. Walk its subscriber graph: mark every
 * downstream computed dirty (without recomputing — recomputation stays
 * lazy, driven by the next read) and collect every downstream effect,
 * de-duplicated by identity, into the flush queue.
 */
function propagateChange(source: Node): void {
  const effectsToRun: Node[] = [];
  const visited = new Set<Node>();
  const stack: Node[] = [source];

  while (stack.length > 0) {
    const n = stack.pop()!;
    for (const sub of n.subscribers) {
      if (visited.has(sub)) continue;
      visited.add(sub);
      if (sub.isEffect) {
        effectsToRun.push(sub);
      } else {
        sub.dirty = true;
        stack.push(sub);
      }
    }
  }

  if (effectsToRun.length > 0) scheduleEffects(effectsToRun);
}

function scheduleEffects(effects: Node[]): void {
  for (let i = 0; i < effects.length; i++) pending.add(effects[i]!);
  maybeDrain();
}

/**
 * Drain `pending` to a fixpoint, unless a batch is still open or a drain is
 * already in progress further up the call stack (in which case this call
 * just returns — the in-progress loop below will see the new entries,
 * since it re-scans `pending` on every iteration).
 */
function maybeDrain(): void {
  if (batchDepth > 0 || draining) return;
  draining = true;
  let iterations = 0;
  try {
    for (;;) {
      let node: Node | undefined;
      for (const candidate of pending) {
        if (!candidate.running) {
          node = candidate;
          break;
        }
      }
      if (node === undefined) break; // empty, or everything left is mid-run

      pending.delete(node);
      if (node.disposed) continue;

      iterations++;
      if (iterations > MAX_FLUSH_ITERATIONS) {
        pending.clear();
        throw new Error(
          `signals: exceeded ${MAX_FLUSH_ITERATIONS} effect runs in a single flush (likely an unconditional self-retriggering effect)`,
        );
      }
      runEffectNode(node);
    }
  } finally {
    draining = false;
  }
}

function runEffectNode(node: Node): void {
  trackedRun(node, () => {
    const cleanup = (node.fn as () => void | (() => void))();
    if (typeof cleanup === "function") registerCleanup(node, cleanup);
  });
}

// ---------------------------------------------------------------------------
// signal()
// ---------------------------------------------------------------------------

export function signal<T>(initial: T, equals?: EqualsFn<T>): Signal<T> {
  const node: Node = {
    value: initial,
    equals: (equals as EqualsFn | undefined) ?? defaultEquals,
    subscribers: new Set(),
    dirty: false,
    hasValue: true,
    isEffect: false,
    fn: null,
    deps: [],
    cleanups: null,
    running: false,
    disposed: false,
  };

  function write(v: T): void {
    if (node.equals(node.value, v)) return;
    node.value = v;
    propagateChange(node);
  }

  const read = (() => {
    trackRead(node);
    return node.value as T;
  }) as Signal<T>;

  read.set = write;
  read.update = (f: (prev: T) => T): void => write(f(node.value as T));
  read.peek = (): T => node.value as T;

  return read;
}

// ---------------------------------------------------------------------------
// computed()
// ---------------------------------------------------------------------------

export function computed<T>(fn: () => T, equals?: EqualsFn<T>): () => T {
  const node: Node = {
    value: undefined,
    equals: (equals as EqualsFn | undefined) ?? defaultEquals,
    subscribers: new Set(),
    dirty: true,
    hasValue: false,
    isEffect: false,
    fn: fn as () => unknown,
    deps: [],
    cleanups: null,
    running: false,
    disposed: false,
  };

  function recompute(): void {
    trackedRun(node, () => {
      const next = node.fn!();
      // Note: this equals check only controls the node's own cached value
      // (e.g. lets an array-producing computed keep a stable reference).
      // It deliberately does not try to retroactively "un-schedule"
      // downstream effects that propagateChange already queued when the
      // upstream signal changed — those effects still run once and will
      // simply observe this computed's unchanged value, which is correct,
      // just not maximally minimal. See module doc.
      if (!node.hasValue || !node.equals(node.value, next)) {
        node.value = next;
      }
      node.hasValue = true;
    });
    node.dirty = false;
  }

  return (): T => {
    if (node.dirty || !node.hasValue) recompute();
    trackRead(node);
    return node.value as T;
  };
}

// ---------------------------------------------------------------------------
// effect()
// ---------------------------------------------------------------------------

export function effect(fn: () => void | (() => void)): () => void {
  const node: Node = {
    value: undefined,
    equals: defaultEquals,
    subscribers: new Set(), // unused: effects are consumer-only
    dirty: false,
    hasValue: false,
    isEffect: true,
    fn: fn as () => unknown,
    deps: [],
    cleanups: null,
    running: false,
    disposed: false,
  };

  runEffectNode(node);
  // Pick up any self-scheduled continuation from the initial run (e.g. a
  // bounded self-referential effect) that maybeDrain() had to defer because
  // this node was still `running` when it was first queued.
  maybeDrain();

  return (): void => {
    if (node.disposed) return;
    node.disposed = true;
    runNodeCleanups(node);
    unsubscribeAllDeps(node);
  };
}

// ---------------------------------------------------------------------------
// batch() / untrack() / onCleanup()
// ---------------------------------------------------------------------------

export function batch<T>(fn: () => T): T {
  batchDepth++;
  try {
    return fn();
  } finally {
    batchDepth--;
    if (batchDepth === 0) maybeDrain();
  }
}

export function untrack<T>(fn: () => T): T {
  const prevConsumer = currentConsumer;
  currentConsumer = null;
  try {
    return fn();
  } finally {
    currentConsumer = prevConsumer;
  }
}

export function onCleanup(fn: () => void): void {
  if (currentConsumer === null) {
    throw new Error("onCleanup() called outside of an effect/computed run");
  }
  registerCleanup(currentConsumer, fn);
}
