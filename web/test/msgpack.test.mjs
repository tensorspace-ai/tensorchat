// Tests for the dependency-free MessagePack encoder/decoder.
//
// Run with: node --test web/test/
//
// This imports the TypeScript source directly (`../src/msgpack.ts`) relying
// on Node's native TypeScript support (type stripping for erasable syntax,
// available without flags on Node 23+/26). msgpack.ts is deliberately
// written using only erasable TS syntax so this works with no build step.

import { test } from "node:test";
import assert from "node:assert/strict";
import { encode, decode } from "../src/msgpack.ts";

// ---------------------------------------------------------------------------
// Integer boundaries
// ---------------------------------------------------------------------------

test("integer boundaries roundtrip", () => {
  const values = [
    0,
    1,
    127,
    128,
    255,
    256,
    65535,
    65536,
    2 ** 32 - 1,
    2 ** 32,
    -1,
    -32,
    -33,
    -128,
    -129,
    -32768,
    -32769,
    -(2 ** 31),
  ];
  for (const v of values) {
    const encoded = encode(v);
    const decoded = decode(encoded);
    assert.equal(decoded, v, `roundtrip failed for ${v}`);
  }
});

test("positive fixint uses single byte", () => {
  assert.equal(encode(0).length, 1);
  assert.equal(encode(127).length, 1);
});

test("negative fixint uses single byte", () => {
  assert.equal(encode(-1).length, 1);
  assert.equal(encode(-32).length, 1);
});

// ---------------------------------------------------------------------------
// u64 / i64 handling — the critical requirement
// ---------------------------------------------------------------------------

test("u64 above 2^53 decodes to exact decimal string", () => {
  // 1234567890123456789 as uint64 big-endian bytes:
  // 0xcf followed by 8 bytes.
  const big = 1234567890123456789n;
  const bytes = new Uint8Array(9);
  bytes[0] = 0xcf;
  const view = new DataView(bytes.buffer);
  view.setBigUint64(1, big, false);
  const decoded = decode(bytes);
  assert.equal(typeof decoded, "string");
  assert.equal(decoded, "1234567890123456789");
});

test("u64 max value (2^64-1) decodes to exact decimal string", () => {
  const max = 18446744073709551615n;
  const bytes = new Uint8Array(9);
  bytes[0] = 0xcf;
  const view = new DataView(bytes.buffer);
  view.setBigUint64(1, max, false);
  const decoded = decode(bytes);
  assert.equal(typeof decoded, "string");
  assert.equal(decoded, "18446744073709551615");
});

test("u64 below 2^53 decodes to a number", () => {
  const small = 9007199254740991n; // Number.MAX_SAFE_INTEGER
  const bytes = new Uint8Array(9);
  bytes[0] = 0xcf;
  const view = new DataView(bytes.buffer);
  view.setBigUint64(1, small, false);
  const decoded = decode(bytes);
  assert.equal(typeof decoded, "number");
  assert.equal(decoded, Number(small));
});

test("i64 negative below -2^53 decodes to exact decimal string", () => {
  const big = -1234567890123456789n;
  const bytes = new Uint8Array(9);
  bytes[0] = 0xd3;
  const view = new DataView(bytes.buffer);
  view.setBigInt64(1, big, false);
  const decoded = decode(bytes);
  assert.equal(typeof decoded, "string");
  assert.equal(decoded, "-1234567890123456789");
});

test("i64 min value decodes to exact decimal string", () => {
  const min = -9223372036854775808n;
  const bytes = new Uint8Array(9);
  bytes[0] = 0xd3;
  const view = new DataView(bytes.buffer);
  view.setBigInt64(1, min, false);
  const decoded = decode(bytes);
  assert.equal(typeof decoded, "string");
  assert.equal(decoded, "-9223372036854775808");
});

test("bigint encodes to uint64/int64 and roundtrips via decode as string/number", () => {
  const encoded = encode(1234567890123456789n);
  const decoded = decode(encoded);
  assert.equal(decoded, "1234567890123456789");

  const small = encode(42n);
  assert.equal(decode(small), 42);
});

test("numeric-looking string roundtrips as a string, not a number", () => {
  const id = "1234567890123456789";
  const encoded = encode(id);
  const decoded = decode(encoded);
  assert.equal(typeof decoded, "string");
  assert.equal(decoded, id);
});

test("small numeric-looking string also stays a string", () => {
  const s = "12345";
  const decoded = decode(encode(s));
  assert.equal(typeof decoded, "string");
  assert.equal(decoded, s);
});

// ---------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------

test("empty string roundtrips", () => {
  assert.equal(decode(encode("")), "");
});

test("short ascii string roundtrips (fast path)", () => {
  const s = "hello";
  assert.equal(decode(encode(s)), s);
});

test("40-char ascii string roundtrips", () => {
  const s = "abcdefghijklmnopqrstuvwxyz0123456789ABCD";
  assert.equal(s.length, 40);
  assert.equal(decode(encode(s)), s);
});

test("unicode strings roundtrip: emoji, CJK, combining chars", () => {
  const cases = [
    "hello 🎉 world",
    "こんにちは世界",
    "é", // e + combining acute accent
    "🧑‍🚀🧑‍🚀🧑‍🚀", // ZWJ sequences (astronaut emoji)
    "混合 mixed テスト 🚀",
  ];
  for (const s of cases) {
    assert.equal(decode(encode(s)), s, `roundtrip failed for ${JSON.stringify(s)}`);
  }
});

test("string longer than 65535 chars roundtrips (str32)", () => {
  const s = "x".repeat(70000);
  const encoded = encode(s);
  assert.equal(encoded[0], 0xdb, "expected str32 tag");
  const decoded = decode(encoded);
  assert.equal(decoded, s);
  assert.equal(decoded.length, 70000);
});

// ---------------------------------------------------------------------------
// Containers
// ---------------------------------------------------------------------------

test("empty array roundtrips", () => {
  assert.deepEqual(decode(encode([])), []);
});

test("empty map roundtrips", () => {
  assert.deepEqual(decode(encode({})), {});
});

test("nested maps and arrays roundtrip", () => {
  const value = {
    a: [1, 2, { b: "c", d: [true, false, null] }],
    e: { f: { g: [1, 2, 3] } },
    list: [[], [{}], [1, [2, [3, [4]]]]],
  };
  assert.deepEqual(decode(encode(value)), value);
});

test("large array (array16/32 boundary) roundtrips", () => {
  const arr16 = new Array(16).fill(0).map((_, i) => i);
  assert.deepEqual(decode(encode(arr16)), arr16);

  const arr = new Array(70000).fill(0).map((_, i) => i % 256);
  const encoded = encode(arr);
  assert.equal(encoded[0], 0xdd, "expected array32 tag");
  assert.deepEqual(decode(encoded), arr);
});

test("large map (map16/32 boundary) roundtrips", () => {
  const obj = {};
  for (let i = 0; i < 20; i++) obj[`k${i}`] = i;
  assert.deepEqual(decode(encode(obj)), obj);
});

// ---------------------------------------------------------------------------
// Floats
// ---------------------------------------------------------------------------

test("floats roundtrip: negative, fractional, Infinity, NaN", () => {
  assert.equal(decode(encode(3.14)), 3.14);
  assert.equal(decode(encode(-3.14)), -3.14);
  assert.equal(decode(encode(0.1)), 0.1);
  assert.equal(decode(encode(Infinity)), Infinity);
  assert.equal(decode(encode(-Infinity)), -Infinity);
  assert.ok(Number.isNaN(decode(encode(NaN))));
});

test("negative zero collapses to 0 (msgpack integers have no signed zero)", () => {
  // -0 is Number.isInteger(-0) === true, so it's encoded via the smallest
  // integer family (positive fixint 0), same as JSON.stringify(-0) === "0".
  // This is expected/documented behavior, not a bug.
  assert.equal(decode(encode(-0.0)), 0);
});

test("float encodes as float64 tag", () => {
  const encoded = encode(3.14);
  assert.equal(encoded[0], 0xcb);
});

// ---------------------------------------------------------------------------
// Binary
// ---------------------------------------------------------------------------

test("binary data roundtrips", () => {
  const data = new Uint8Array([0, 1, 2, 255, 254, 128, 127]);
  const decoded = decode(encode(data));
  assert.ok(decoded instanceof Uint8Array);
  assert.deepEqual(Array.from(decoded), Array.from(data));
});

test("empty binary data roundtrips", () => {
  const data = new Uint8Array(0);
  const decoded = decode(encode(data));
  assert.ok(decoded instanceof Uint8Array);
  assert.equal(decoded.length, 0);
});

test("large binary data (bin16/32 boundary) roundtrips", () => {
  const data = new Uint8Array(70000);
  for (let i = 0; i < data.length; i++) data[i] = i % 256;
  const encoded = encode(data);
  assert.equal(encoded[0], 0xc6, "expected bin32 tag");
  const decoded = decode(encoded);
  assert.deepEqual(Array.from(decoded), Array.from(data));
});

// ---------------------------------------------------------------------------
// Extension types
// ---------------------------------------------------------------------------

test("extension type decodes to {__ext, data} shape", () => {
  // fixext1: 0xd4, type=5, 1 data byte
  const bytes = new Uint8Array([0xd4, 5, 0xaa]);
  const decoded = decode(bytes);
  assert.equal(decoded.__ext, 5);
  assert.ok(decoded.data instanceof Uint8Array);
  assert.deepEqual(Array.from(decoded.data), [0xaa]);
});

test("extension shape roundtrips through encode", () => {
  const ext = { __ext: 7, data: new Uint8Array([1, 2, 3, 4]) };
  const decoded = decode(encode(ext));
  assert.equal(decoded.__ext, 7);
  assert.deepEqual(Array.from(decoded.data), [1, 2, 3, 4]);
});

// ---------------------------------------------------------------------------
// Realistic TensorChat frame
// ---------------------------------------------------------------------------

test("realistic TensorChat server frame roundtrips exactly", () => {
  const frame = {
    msg: {
      m: {
        id: "1234567890123456789",
        ch: 987654321012345678,
        au: 5,
        b: "hello 🎉",
        rx: [],
        at: [],
      },
    },
  };
  const encoded = encode(frame);
  const decoded = decode(encoded);
  assert.deepEqual(decoded, frame);
  // id must stay a string (it's a u64 snowflake beyond safe integer range)
  assert.equal(typeof decoded.msg.m.id, "string");
  // ch, as a JS number literal in source, was already rounded to the
  // nearest double at parse time; it must roundtrip through msgpack
  // (as float64, since it exceeds Number.MAX_SAFE_INTEGER) with no
  // further precision loss.
  assert.equal(typeof decoded.msg.m.ch, "number");
  assert.equal(decoded.msg.m.ch, frame.msg.m.ch);
});

// ---------------------------------------------------------------------------
// Malformed input
// ---------------------------------------------------------------------------

test("truncated input throws", () => {
  // str8 header claiming 10 bytes but providing none
  const bytes = new Uint8Array([0xd9, 10]);
  assert.throws(() => decode(bytes));
});

test("truncated fixarray throws", () => {
  // fixarray of length 3 but only 1 element present
  const bytes = new Uint8Array([0x93, 0x01]);
  assert.throws(() => decode(bytes));
});

test("truncated uint64 throws", () => {
  const bytes = new Uint8Array([0xcf, 1, 2, 3]);
  assert.throws(() => decode(bytes));
});

test("empty buffer throws", () => {
  assert.throws(() => decode(new Uint8Array(0)));
});

test("unknown type tag throws", () => {
  // 0xc1 is reserved/unused in the MessagePack spec
  const bytes = new Uint8Array([0xc1]);
  assert.throws(() => decode(bytes));
});
