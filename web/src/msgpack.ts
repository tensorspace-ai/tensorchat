/**
 * Dependency-free, high-performance MessagePack encoder/decoder.
 *
 * Compatible with the surface that Rust's `rmp-serde` emits and accepts:
 * nil, bool, all int families, float32/float64, str8/16/32 (+ fixstr),
 * bin8/16/32, array16/32 (+ fixarray), map16/32 (+ fixmap), and extension
 * types (decoded to `{ __ext: number, data: Uint8Array }`).
 *
 * ## u64 / i64 handling (read this before touching decode logic)
 *
 * TensorChat entity IDs are u64 Snowflakes that routinely exceed
 * `Number.MAX_SAFE_INTEGER` (2^53 - 1). JavaScript numbers cannot represent
 * such values exactly, and BigInt is awkward to use as an object key or in
 * `===` comparisons against JSON-derived strings. So:
 *
 *   - When decoding uint64 (0xcf) or int64 (0xd3), if the value fits safely
 *     in a JS number we return a plain `number`.
 *   - Otherwise we return the exact **decimal string** representation
 *     (e.g. `"1234567890123456789"`), computed from the two 32-bit halves
 *     via BigInt internally — never a BigInt value, never a lossily
 *     rounded number.
 *
 * This makes large IDs safe to use as object keys and in strict equality
 * checks on the client, while small integers stay ordinary numbers.
 *
 * On encode: a JS `string` is always encoded as a msgpack string, even if
 * it looks like a number (we never guess that a digit-string is meant to be
 * an integer). A JS `bigint` is always encoded as uint64/int64. A JS
 * `number` that is a safe integer is encoded as the smallest int family
 * that fits; anything else (non-integers, or integers outside the safe
 * range) is encoded as float64.
 *
 * ## Short-string fast path
 *
 * `TextDecoder`/`TextEncoder` have measurable fixed per-call overhead that
 * dominates for tiny strings. For decoding, once we know a string's byte
 * length is small (<= 16), we first scan those bytes: if every byte is
 * ASCII (< 0x80) we build the string with `String.fromCharCode` in a plain
 * loop, which is faster than round-tripping through `TextDecoder` for such
 * short inputs. Any non-ASCII byte in that scan falls back to the shared
 * `TextDecoder` instance, so correctness for unicode is never sacrificed.
 */

// ---------------------------------------------------------------------------
// Shared encoder/decoder instances (avoid re-allocating per call).
// ---------------------------------------------------------------------------

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder("utf-8");

/** Byte length at/under which we try the manual ASCII fast path for strings. */
const SHORT_STRING_MAX = 16;

// ---------------------------------------------------------------------------
// Decoded extension type shape.
// ---------------------------------------------------------------------------

export interface DecodedExt {
  __ext: number;
  data: Uint8Array;
}

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

/**
 * Growable byte buffer written via a DataView. Grows geometrically
 * (doubling) instead of concatenating chunks, so a single encode() call
 * does at most O(log n) reallocations rather than one allocation per value.
 */
class Writer {
  buf: Uint8Array;
  view: DataView;
  len = 0;

  constructor(initialCapacity = 1024) {
    this.buf = new Uint8Array(initialCapacity);
    this.view = new DataView(this.buf.buffer);
  }

  /**
   * Module-internal, not private: the string writer below needs to reserve
   * worst-case space before it knows the real encoded length, so it grows the
   * buffer directly rather than through a per-byte path.
   */
  ensure(extra: number): void {
    const needed = this.len + extra;
    if (needed <= this.buf.byteLength) return;
    let newCap = this.buf.byteLength * 2;
    while (newCap < needed) newCap *= 2;
    const newBuf = new Uint8Array(newCap);
    newBuf.set(this.buf.subarray(0, this.len));
    this.buf = newBuf;
    this.view = new DataView(newBuf.buffer);
  }

  u8(v: number): void {
    this.ensure(1);
    this.view.setUint8(this.len, v);
    this.len += 1;
  }

  i8(v: number): void {
    this.ensure(1);
    this.view.setInt8(this.len, v);
    this.len += 1;
  }

  u16(v: number): void {
    this.ensure(2);
    this.view.setUint16(this.len, v, false);
    this.len += 2;
  }

  i16(v: number): void {
    this.ensure(2);
    this.view.setInt16(this.len, v, false);
    this.len += 2;
  }

  u32(v: number): void {
    this.ensure(4);
    this.view.setUint32(this.len, v, false);
    this.len += 4;
  }

  i32(v: number): void {
    this.ensure(4);
    this.view.setInt32(this.len, v, false);
    this.len += 4;
  }

  u64(v: bigint): void {
    this.ensure(8);
    this.view.setBigUint64(this.len, v, false);
    this.len += 8;
  }

  i64(v: bigint): void {
    this.ensure(8);
    this.view.setBigInt64(this.len, v, false);
    this.len += 8;
  }

  f32(v: number): void {
    this.ensure(4);
    this.view.setFloat32(this.len, v, false);
    this.len += 4;
  }

  f64(v: number): void {
    this.ensure(8);
    this.view.setFloat64(this.len, v, false);
    this.len += 8;
  }

  bytes(src: Uint8Array): void {
    this.ensure(src.byteLength);
    this.buf.set(src, this.len);
    this.len += src.byteLength;
  }

  finish(): Uint8Array {
    return this.buf.subarray(0, this.len);
  }
}

function encodeString(w: Writer, str: string): void {
  // Worst case UTF-8 is 4 bytes per UTF-16 code unit (surrogate pairs
  // included, since each code unit expands separately in the estimate).
  const maxBytes = str.length * 4;

  // We need to know the byte length before writing the header (str8/16/32
  // pick different header sizes). Reserve worst-case space for header
  // (5 bytes) + payload up front, encode the payload starting 5 bytes past
  // the header position, then shift it left once the real header size
  // (which depends on the actual encoded byte length) is known.
  w.ensure(5 + maxBytes);
  const headerPos = w.len;
  const payloadStart = headerPos + 5;
  let written: number;
  if (typeof textEncoder.encodeInto === "function") {
    const result = textEncoder.encodeInto(str, w.buf.subarray(payloadStart, payloadStart + maxBytes));
    written = result.written ?? 0;
  } else {
    const encoded = textEncoder.encode(str);
    w.buf.set(encoded, payloadStart);
    written = encoded.byteLength;
  }

  let headerSize: number;
  if (written <= 0x1f) {
    headerSize = 1;
  } else if (written <= 0xff) {
    headerSize = 2;
  } else if (written <= 0xffff) {
    headerSize = 3;
  } else {
    headerSize = 5;
  }

  // Shift payload left to sit directly after the real header.
  const realPayloadStart = headerPos + headerSize;
  if (realPayloadStart !== payloadStart) {
    w.buf.copyWithin(realPayloadStart, payloadStart, payloadStart + written);
  }

  // Write header at headerPos.
  if (headerSize === 1) {
    w.view.setUint8(headerPos, 0xa0 | written);
  } else if (headerSize === 2) {
    w.view.setUint8(headerPos, 0xd9);
    w.view.setUint8(headerPos + 1, written);
  } else if (headerSize === 3) {
    w.view.setUint8(headerPos, 0xda);
    w.view.setUint16(headerPos + 1, written, false);
  } else {
    w.view.setUint8(headerPos, 0xdb);
    w.view.setUint32(headerPos + 1, written, false);
  }

  w.len = realPayloadStart + written;
}

function encodeBin(w: Writer, data: Uint8Array): void {
  const n = data.byteLength;
  if (n <= 0xff) {
    w.u8(0xc4);
    w.u8(n);
  } else if (n <= 0xffff) {
    w.u8(0xc5);
    w.u16(n);
  } else {
    w.u8(0xc6);
    w.u32(n);
  }
  w.bytes(data);
}

const EXT_FIXED_SIZE_TAGS: Record<number, number> = { 1: 0xd4, 2: 0xd5, 4: 0xd6, 8: 0xd7, 16: 0xd8 };

function encodeExt(w: Writer, ext: DecodedExt): void {
  const n = ext.data.byteLength;
  const fixedTag = EXT_FIXED_SIZE_TAGS[n];
  if (fixedTag !== undefined) {
    w.u8(fixedTag);
  } else if (n <= 0xff) {
    w.u8(0xc7);
    w.u8(n);
  } else if (n <= 0xffff) {
    w.u8(0xc8);
    w.u16(n);
  } else {
    w.u8(0xc9);
    w.u32(n);
  }
  w.i8(ext.__ext);
  w.bytes(ext.data);
}

function encodeArrayHeader(w: Writer, n: number): void {
  if (n <= 0x0f) {
    w.u8(0x90 | n);
  } else if (n <= 0xffff) {
    w.u8(0xdc);
    w.u16(n);
  } else {
    w.u8(0xdd);
    w.u32(n);
  }
}

function encodeMapHeader(w: Writer, n: number): void {
  if (n <= 0x0f) {
    w.u8(0x80 | n);
  } else if (n <= 0xffff) {
    w.u8(0xde);
    w.u16(n);
  } else {
    w.u8(0xdf);
    w.u32(n);
  }
}

function isDecodedExt(value: unknown): value is DecodedExt {
  return (
    typeof value === "object" &&
    value !== null &&
    !Array.isArray(value) &&
    typeof (value as { __ext?: unknown }).__ext === "number" &&
    (value as { data?: unknown }).data instanceof Uint8Array
  );
}

function encodeNumber(w: Writer, n: number): void {
  if (Number.isInteger(n) && Number.isSafeInteger(n)) {
    if (n >= 0) {
      if (n <= 0x7f) {
        w.u8(n);
      } else if (n <= 0xff) {
        w.u8(0xcc);
        w.u8(n);
      } else if (n <= 0xffff) {
        w.u8(0xcd);
        w.u16(n);
      } else if (n <= 0xffffffff) {
        w.u8(0xce);
        w.u32(n);
      } else {
        w.u8(0xcf);
        w.u64(BigInt(n));
      }
    } else {
      if (n >= -32) {
        w.i8(n);
      } else if (n >= -128) {
        w.u8(0xd0);
        w.i8(n);
      } else if (n >= -32768) {
        w.u8(0xd1);
        w.i16(n);
      } else if (n >= -2147483648) {
        w.u8(0xd2);
        w.i32(n);
      } else {
        w.u8(0xd3);
        w.i64(BigInt(n));
      }
    }
  } else {
    // Non-integer, or an integer outside the safe range (e.g. produced by
    // arithmetic) — encode as float64 to avoid silent precision loss.
    w.u8(0xcb);
    w.f64(n);
  }
}

function encodeBigInt(w: Writer, n: bigint): void {
  if (n >= 0n) {
    if (n <= 0xffffffffffffffffn) {
      w.u8(0xcf);
      w.u64(n);
    } else {
      throw new RangeError(`msgpack: bigint ${n} exceeds uint64 range`);
    }
  } else {
    if (n >= -9223372036854775808n) {
      w.u8(0xd3);
      w.i64(n);
    } else {
      throw new RangeError(`msgpack: bigint ${n} exceeds int64 range`);
    }
  }
}

function encodeValue(w: Writer, value: unknown): void {
  if (value === undefined || value === null) {
    w.u8(0xc0);
    return;
  }
  if (typeof value === "boolean") {
    w.u8(value ? 0xc3 : 0xc2);
    return;
  }
  if (typeof value === "number") {
    encodeNumber(w, value);
    return;
  }
  if (typeof value === "bigint") {
    encodeBigInt(w, value);
    return;
  }
  if (typeof value === "string") {
    encodeString(w, value);
    return;
  }
  if (value instanceof Uint8Array) {
    encodeBin(w, value);
    return;
  }
  if (ArrayBuffer.isView(value)) {
    encodeBin(w, new Uint8Array(value.buffer, value.byteOffset, value.byteLength));
    return;
  }
  if (Array.isArray(value)) {
    encodeArrayHeader(w, value.length);
    for (const item of value) encodeValue(w, item);
    return;
  }
  if (isDecodedExt(value)) {
    encodeExt(w, value);
    return;
  }
  if (value instanceof Map) {
    encodeMapHeader(w, value.size);
    for (const [k, v] of value) {
      encodeValue(w, k);
      encodeValue(w, v);
    }
    return;
  }
  if (typeof value === "object") {
    const entries = Object.entries(value as Record<string, unknown>);
    encodeMapHeader(w, entries.length);
    for (const [k, v] of entries) {
      encodeString(w, k);
      encodeValue(w, v);
    }
    return;
  }
  throw new TypeError(`msgpack: cannot encode value of type ${typeof value}`);
}

/** Encode a JS value to a MessagePack-formatted Uint8Array. */
export function encode(value: unknown): Uint8Array {
  const w = new Writer();
  encodeValue(w, value);
  return w.finish();
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/**
 * Single-pass reader over a Uint8Array/DataView pair. Offset is tracked as
 * an instance field (closure-scoped per decode() call), never by slicing
 * the source buffer — slicing would allocate a new buffer per nested
 * value, which is exactly what we must avoid for performance.
 */
class Reader {
  buf: Uint8Array;
  view: DataView;
  pos = 0;

  constructor(buf: Uint8Array) {
    this.buf = buf;
    this.view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  }

  private need(n: number): void {
    if (this.pos + n > this.buf.byteLength) {
      throw new RangeError(
        `msgpack: unexpected end of input (need ${n} byte(s) at offset ${this.pos}, have ${this.buf.byteLength - this.pos})`,
      );
    }
  }

  u8(): number {
    this.need(1);
    const v = this.view.getUint8(this.pos);
    this.pos += 1;
    return v;
  }

  i8(): number {
    this.need(1);
    const v = this.view.getInt8(this.pos);
    this.pos += 1;
    return v;
  }

  u16(): number {
    this.need(2);
    const v = this.view.getUint16(this.pos, false);
    this.pos += 2;
    return v;
  }

  i16(): number {
    this.need(2);
    const v = this.view.getInt16(this.pos, false);
    this.pos += 2;
    return v;
  }

  u32(): number {
    this.need(4);
    const v = this.view.getUint32(this.pos, false);
    this.pos += 4;
    return v;
  }

  i32(): number {
    this.need(4);
    const v = this.view.getInt32(this.pos, false);
    this.pos += 4;
    return v;
  }

  u64(): bigint {
    this.need(8);
    const v = this.view.getBigUint64(this.pos, false);
    this.pos += 8;
    return v;
  }

  i64(): bigint {
    this.need(8);
    const v = this.view.getBigInt64(this.pos, false);
    this.pos += 8;
    return v;
  }

  f32(): number {
    this.need(4);
    const v = this.view.getFloat32(this.pos, false);
    this.pos += 4;
    return v;
  }

  f64(): number {
    this.need(8);
    const v = this.view.getFloat64(this.pos, false);
    this.pos += 8;
    return v;
  }

  bytes(n: number): Uint8Array {
    this.need(n);
    const v = this.buf.subarray(this.pos, this.pos + n);
    this.pos += n;
    return v;
  }

  /**
   * Decode a UTF-8 string of exactly `n` bytes.
   *
   * Fast path: for short strings (n <= SHORT_STRING_MAX) we scan the bytes
   * first. If every byte is pure ASCII (< 0x80), we build the string with
   * `String.fromCharCode` in a manual loop — this avoids per-call overhead
   * of `TextDecoder.decode()` that dominates for tiny strings. Any non-ASCII
   * byte found during the scan falls back to the shared TextDecoder, so
   * unicode correctness is never compromised.
   */
  str(n: number): string {
    this.need(n);
    const start = this.pos;
    this.pos += n;

    if (n <= SHORT_STRING_MAX) {
      let allAscii = true;
      for (let i = 0; i < n; i++) {
        if (this.buf[start + i]! >= 0x80) {
          allAscii = false;
          break;
        }
      }
      if (allAscii) {
        let out = "";
        for (let i = 0; i < n; i++) {
          out += String.fromCharCode(this.buf[start + i]!);
        }
        return out;
      }
    }

    return textDecoder.decode(this.buf.subarray(start, start + n));
  }
}

const MIN_SAFE_BIGINT = BigInt(Number.MIN_SAFE_INTEGER);
const MAX_SAFE_BIGINT = BigInt(Number.MAX_SAFE_INTEGER);

/**
 * Convert a 64-bit integer (already assembled as a bigint by DataView's
 * getBigUint64/getBigInt64) into either a safe JS number or its exact
 * decimal string, per the u64/i64 handling policy documented at the top of
 * this file. Using the DataView-native bigint readers means the value is
 * computed correctly and directly from the underlying bytes — no manual
 * 32-bit-half arithmetic that could silently be off by a sign or a carry.
 */
function bigIntToNumberOrString(v: bigint): number | string {
  if (v >= MIN_SAFE_BIGINT && v <= MAX_SAFE_BIGINT) {
    return Number(v);
  }
  return v.toString();
}

function decodeValue(r: Reader): unknown {
  const tag = r.u8();

  // positive fixint 0x00 - 0x7f
  if (tag <= 0x7f) return tag;
  // fixmap 0x80 - 0x8f
  if (tag >= 0x80 && tag <= 0x8f) return decodeMap(r, tag & 0x0f);
  // fixarray 0x90 - 0x9f
  if (tag >= 0x90 && tag <= 0x9f) return decodeArray(r, tag & 0x0f);
  // fixstr 0xa0 - 0xbf
  if (tag >= 0xa0 && tag <= 0xbf) return r.str(tag & 0x1f);
  // negative fixint 0xe0 - 0xff
  if (tag >= 0xe0) return tag - 0x100;

  switch (tag) {
    case 0xc0:
      return null;
    case 0xc2:
      return false;
    case 0xc3:
      return true;

    case 0xc4: // bin8
      return r.bytes(r.u8()).slice();
    case 0xc5: // bin16
      return r.bytes(r.u16()).slice();
    case 0xc6: // bin32
      return r.bytes(r.u32()).slice();

    case 0xc7: { // ext8
      const n = r.u8();
      const type = r.i8();
      return { __ext: type, data: r.bytes(n).slice() };
    }
    case 0xc8: { // ext16
      const n = r.u16();
      const type = r.i8();
      return { __ext: type, data: r.bytes(n).slice() };
    }
    case 0xc9: { // ext32
      const n = r.u32();
      const type = r.i8();
      return { __ext: type, data: r.bytes(n).slice() };
    }

    case 0xca: // float32
      return r.f32();
    case 0xcb: // float64
      return r.f64();

    case 0xcc: // uint8
      return r.u8();
    case 0xcd: // uint16
      return r.u16();
    case 0xce: // uint32
      return r.u32();
    case 0xcf: // uint64
      return bigIntToNumberOrString(r.u64());

    case 0xd0: // int8
      return r.i8();
    case 0xd1: // int16
      return r.i16();
    case 0xd2: // int32
      return r.i32();
    case 0xd3: // int64
      return bigIntToNumberOrString(r.i64());

    case 0xd4: { // fixext1
      const type = r.i8();
      return { __ext: type, data: r.bytes(1).slice() };
    }
    case 0xd5: { // fixext2
      const type = r.i8();
      return { __ext: type, data: r.bytes(2).slice() };
    }
    case 0xd6: { // fixext4
      const type = r.i8();
      return { __ext: type, data: r.bytes(4).slice() };
    }
    case 0xd7: { // fixext8
      const type = r.i8();
      return { __ext: type, data: r.bytes(8).slice() };
    }
    case 0xd8: { // fixext16
      const type = r.i8();
      return { __ext: type, data: r.bytes(16).slice() };
    }

    case 0xd9: // str8
      return r.str(r.u8());
    case 0xda: // str16
      return r.str(r.u16());
    case 0xdb: // str32
      return r.str(r.u32());

    case 0xdc: // array16
      return decodeArray(r, r.u16());
    case 0xdd: // array32
      return decodeArray(r, r.u32());

    case 0xde: // map16
      return decodeMap(r, r.u16());
    case 0xdf: // map32
      return decodeMap(r, r.u32());

    default:
      throw new RangeError(`msgpack: unknown type tag 0x${tag.toString(16)} at offset ${r.pos - 1}`);
  }
}

function decodeArray(r: Reader, n: number): unknown[] {
  const out = new Array(n);
  for (let i = 0; i < n; i++) {
    out[i] = decodeValue(r);
  }
  return out;
}

function decodeMap(r: Reader, n: number): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (let i = 0; i < n; i++) {
    const key = decodeValue(r);
    const value = decodeValue(r);
    out[key as string] = value;
  }
  return out;
}

/** Decode a MessagePack-formatted Uint8Array to a JS value. */
export function decode(buf: Uint8Array): unknown {
  const r = new Reader(buf);
  const value = decodeValue(r);
  return value;
}
