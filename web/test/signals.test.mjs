// Tests for the dependency-free fine-grained reactive core.
//
// Run with: node --test web/test/signals.test.mjs web/test/virtual-list.test.mjs
// (a bare directory argument is broken on this Node build; always pass
// explicit file paths).
//
// This imports the TypeScript source directly (`../src/signals.ts`),
// relying on Node's native TypeScript support (type stripping for erasable
// syntax, available without flags on Node 23+/26). signals.ts is
// deliberately written using only erasable TS syntax so this works with no
// build step.

import { test } from "node:test";
import assert from "node:assert/strict";
import { signal, computed, effect, batch, untrack, onCleanup } from "../src/signals.ts";

// ---------------------------------------------------------------------------
// Basic signal behavior
// ---------------------------------------------------------------------------

test("signal: get/set/update roundtrip", () => {
  const s = signal(1);
  assert.equal(s(), 1);
  s.set(2);
  assert.equal(s(), 2);
  s.update((prev) => prev + 10);
  assert.equal(s(), 12);
});

test("signal: default equals (Object.is) suppresses notification for equal values", () => {
  const s = signal(1);
  let runs = 0;
  effect(() => {
    s();
    runs++;
  });
  assert.equal(runs, 1);
  s.set(1); // same value, Object.is(1,1) === true
  assert.equal(runs, 1, "effect must not re-run when the new value is equal");
  s.set(2);
  assert.equal(runs, 2);
});

test("signal: custom equals comparator (array contents) suppresses notification", () => {
  const arrayEquals = (a, b) => a.length === b.length && a.every((v, i) => v === b[i]);
  const s = signal([1, 2, 3], arrayEquals);
  let runs = 0;
  effect(() => {
    s();
    runs++;
  });
  assert.equal(runs, 1);
  s.set([1, 2, 3]); // different reference, same contents
  assert.equal(runs, 1, "custom equals must suppress notification for equal contents");
  s.set([1, 2, 4]);
  assert.equal(runs, 2);
});

test("signal: peek() reads without subscribing", () => {
  const s = signal(1);
  let runs = 0;
  effect(() => {
    s.peek();
    runs++;
  });
  assert.equal(runs, 1);
  s.set(2);
  assert.equal(runs, 1, "peek() must not create a subscription");
});

test("untrack(): reads inside it do not subscribe", () => {
  const s = signal(1);
  let runs = 0;
  effect(() => {
    untrack(() => s());
    runs++;
  });
  assert.equal(runs, 1);
  s.set(2);
  assert.equal(runs, 1, "untrack() must prevent subscription");
});

// ---------------------------------------------------------------------------
// effect() basics
// ---------------------------------------------------------------------------

test("effect: runs once immediately on creation", () => {
  let runs = 0;
  effect(() => {
    runs++;
  });
  assert.equal(runs, 1);
});

test("effect: re-runs when a read dependency changes", () => {
  const s = signal(0);
  let runs = 0;
  let seen = -1;
  effect(() => {
    seen = s();
    runs++;
  });
  assert.equal(runs, 1);
  s.set(5);
  assert.equal(runs, 2);
  assert.equal(seen, 5);
});

test("effect: stale dependency cleanup — a conditional branch that stops reading a signal stops being notified", () => {
  const cond = signal(true);
  const a = signal("a");
  const b = signal("b");
  let runs = 0;
  effect(() => {
    runs++;
    if (cond()) {
      a();
    } else {
      b();
    }
  });
  assert.equal(runs, 1);

  // Currently subscribed to `a` (via the true branch). Changing `a` re-runs.
  a.set("a2");
  assert.equal(runs, 2);

  // Flip the branch: now subscribed to `b`, unsubscribed from `a`.
  cond.set(false);
  assert.equal(runs, 3);

  a.set("a3");
  assert.equal(runs, 3, "must not re-run: `a` is no longer read after the branch flipped");

  b.set("b2");
  assert.equal(runs, 4, "must re-run: `b` is now the active dependency");
});

test("effect: writing an unrelated signal from inside an effect does not loop", () => {
  const a = signal(0);
  const c = signal(0);
  let runs = 0;
  effect(() => {
    a();
    c.set(c.peek() + 1); // writes a signal this effect never reads
    runs++;
  });
  assert.equal(runs, 1);
  assert.equal(c(), 1);
  a.set(1);
  assert.equal(runs, 2);
  assert.equal(c(), 2);
});

test("effect: bounded self-referential update runs to completion without throwing", () => {
  const s = signal(0);
  let runs = 0;
  effect(() => {
    runs++;
    const v = s();
    if (v < 5) s.set(v + 1);
  });
  assert.equal(s(), 5);
  assert.equal(runs, 6, "initial run + 5 bounded self-triggered re-runs");
});

test("effect: unconditional self-retriggering effect is capped (throws) instead of hanging", () => {
  const s = signal(0);
  assert.throws(() => {
    effect(() => {
      s.set(s() + 1);
    });
  }, /flush|infinite|cyclic/i);
});

// ---------------------------------------------------------------------------
// onCleanup / dispose
// ---------------------------------------------------------------------------

test("onCleanup: runs before the owning effect's next run", () => {
  const s = signal(0);
  const events = [];
  effect(() => {
    const v = s();
    events.push(`run:${v}`);
    onCleanup(() => events.push(`cleanup:${v}`));
  });
  assert.deepEqual(events, ["run:0"]);
  s.set(1);
  assert.deepEqual(events, ["run:0", "cleanup:0", "run:1"]);
});

test("onCleanup: runs on dispose", () => {
  const events = [];
  const dispose = effect(() => {
    onCleanup(() => events.push("disposed"));
  });
  assert.deepEqual(events, []);
  dispose();
  assert.deepEqual(events, ["disposed"]);
});

test("effect returning a function is equivalent to calling onCleanup with it", () => {
  const s = signal(0);
  const events = [];
  effect(() => {
    const v = s();
    return () => events.push(`teardown:${v}`);
  });
  s.set(1);
  assert.deepEqual(events, ["teardown:0"]);
});

test("dispose: stops the effect and unsubscribes it from everything", () => {
  const s = signal(0);
  let runs = 0;
  const dispose = effect(() => {
    s();
    runs++;
  });
  assert.equal(runs, 1);
  s.set(1);
  assert.equal(runs, 2);
  dispose();
  s.set(2);
  assert.equal(runs, 2, "must not run after dispose");
});

// ---------------------------------------------------------------------------
// computed(): laziness, caching, correctness
// ---------------------------------------------------------------------------

test("computed: is lazy — body does not run until read", () => {
  let bodyRuns = 0;
  const s = signal(1);
  const c = computed(() => {
    bodyRuns++;
    return s() * 2;
  });
  assert.equal(bodyRuns, 0, "must not run just from being defined");
  s.set(2); // no one has read it yet — still no reason to recompute
  assert.equal(bodyRuns, 0);
  assert.equal(c(), 4);
  assert.equal(bodyRuns, 1);
});

test("computed: is cached — does not re-run on read unless a dependency changed", () => {
  let bodyRuns = 0;
  const s = signal(1);
  const c = computed(() => {
    bodyRuns++;
    return s() * 2;
  });
  c();
  c();
  c();
  assert.equal(bodyRuns, 1, "repeated reads without a dependency change must not re-run the body");
  s.set(5);
  c();
  c();
  assert.equal(bodyRuns, 2, "one dependency change triggers exactly one recompute, cached afterward");
});

test("computed: recomputes correctly and caches the new value", () => {
  const s = signal(3);
  const c = computed(() => s() * s());
  assert.equal(c(), 9);
  s.set(4);
  assert.equal(c(), 16);
});

test("computed: default equals (Object.is) — custom equals is honored for its own cache", () => {
  const s = signal(1);
  let bodyRuns = 0;
  const c = computed(
    () => {
      bodyRuns++;
      return [s()]; // new array identity every recompute
    },
    (a, b) => a.length === b.length && a[0] === b[0],
  );
  const first = c();
  s.set(2);
  const second = c();
  assert.equal(bodyRuns, 2);
  assert.notEqual(first, second);
  s.set(2); // signal write is a no-op (Object.is(2,2)), no recompute at all
  assert.equal(bodyRuns, 2);
});

// ---------------------------------------------------------------------------
// Glitch-free propagation (the critical correctness property)
// ---------------------------------------------------------------------------

test("glitch-free: effect reading a signal and a computed derived from it sees consistent values and runs exactly once per write", () => {
  const a = signal(1);
  const b = computed(() => a() * 2);
  let runs = 0;
  let inconsistent = 0;
  effect(() => {
    runs++;
    const av = a();
    const bv = b();
    if (bv !== av * 2) inconsistent++;
  });
  assert.equal(runs, 1);
  assert.equal(inconsistent, 0);

  a.set(2);
  assert.equal(runs, 2, "must run exactly once for one signal write");
  assert.equal(inconsistent, 0, "must never observe b !== a*2");

  a.set(3);
  assert.equal(runs, 3);
  assert.equal(inconsistent, 0);
});

test("diamond dependency: a -> b, a -> c, (b, c) -> effect runs exactly once per change to a", () => {
  const a = signal(1);
  const b = computed(() => a() * 2);
  const c = computed(() => a() + 100);
  let runs = 0;
  effect(() => {
    b();
    c();
    runs++;
  });
  assert.equal(runs, 1);
  a.set(2);
  assert.equal(runs, 2, "diamond must collapse to a single effect run, not two");
  assert.equal(b(), 4);
  assert.equal(c(), 102);
});

test("glitch-free: chained computeds (a -> b -> c) stay consistent for a downstream effect", () => {
  const a = signal(2);
  const b = computed(() => a() * 2);
  const c = computed(() => b() + 1);
  let runs = 0;
  let observed = [];
  effect(() => {
    runs++;
    observed.push([a(), b(), c()]);
  });
  a.set(5);
  assert.equal(runs, 2);
  assert.deepEqual(observed[1], [5, 10, 11]);
});

// ---------------------------------------------------------------------------
// batch()
// ---------------------------------------------------------------------------

test("batch: multiple set calls run effects once at the end", () => {
  const a = signal(1);
  const b = signal(2);
  let runs = 0;
  effect(() => {
    a();
    b();
    runs++;
  });
  assert.equal(runs, 1);
  batch(() => {
    a.set(10);
    b.set(20);
  });
  assert.equal(runs, 2, "batched writes must flush effects exactly once");
  assert.equal(a(), 10);
  assert.equal(b(), 20);
});

test("batch: nested batches flush only at the outermost exit", () => {
  const a = signal(1);
  let runs = 0;
  effect(() => {
    a();
    runs++;
  });
  batch(() => {
    batch(() => {
      a.set(2);
    });
    assert.equal(runs, 1, "inner batch exit must not flush yet");
    a.set(3);
  });
  assert.equal(runs, 2, "only the outermost batch exit flushes");
  assert.equal(a(), 3);
});

test("batch: return value of the batched function is passed through", () => {
  const result = batch(() => 42);
  assert.equal(result, 42);
});

// ---------------------------------------------------------------------------
// Misc coverage
// ---------------------------------------------------------------------------

test("multiple independent effects on the same signal all run", () => {
  const s = signal(0);
  let r1 = 0;
  let r2 = 0;
  effect(() => {
    s();
    r1++;
  });
  effect(() => {
    s();
    r2++;
  });
  s.set(1);
  assert.equal(r1, 2);
  assert.equal(r2, 2);
});

test("computed depending on nothing computes once and never re-runs", () => {
  let bodyRuns = 0;
  const c = computed(() => {
    bodyRuns++;
    return 42;
  });
  assert.equal(c(), 42);
  assert.equal(c(), 42);
  assert.equal(bodyRuns, 1);
});

test("effect subscribing to a computed re-runs when the computed's upstream signal changes", () => {
  const s = signal(1);
  const doubled = computed(() => s() * 2);
  let runs = 0;
  let last = -1;
  effect(() => {
    last = doubled();
    runs++;
  });
  assert.equal(runs, 1);
  assert.equal(last, 2);
  s.set(10);
  assert.equal(runs, 2);
  assert.equal(last, 20);
});
