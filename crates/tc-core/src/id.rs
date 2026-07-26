//! Time-sortable 64-bit IDs.
//!
//! Every entity in TensorChat is keyed by a `Id`: a Snowflake-style u64 laid
//! out as
//!
//! ```text
//!  63           22 21    12 11         0
//! +---------------+--------+-----------+
//! | ms since epoch| node   | sequence  |
//! |   42 bits     | 10 bits|  12 bits  |
//! +---------------+--------+-----------+
//! ```
//!
//! Why not UUIDs: IDs are the primary key and the pagination cursor for the
//! hottest table in the system (`messages`). A monotonic u64 keeps the B-tree
//! append-only (no page splits from random inserts), makes `ORDER BY id` a
//! free index scan, needs no secondary `created_at` index, and costs 8 bytes
//! instead of 16 — before counting the text encoding a UUID usually drags along.
//!
//! 42 bits of milliseconds runs to the year 2163. 12 sequence bits allow 4096
//! IDs per millisecond per node, i.e. ~4.1M IDs/sec/node.

use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 2024-01-01T00:00:00Z. Custom epochs buy back leading bits that a Unix epoch
/// would waste on decades that already happened.
pub const TC_EPOCH_MS: u64 = 1_704_067_200_000;

const SEQ_BITS: u32 = 12;
const NODE_BITS: u32 = 10;
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;
const NODE_MASK: u64 = (1 << NODE_BITS) - 1;
const TIME_SHIFT: u32 = SEQ_BITS + NODE_BITS;

/// A time-sortable identifier.
///
/// On the wire this is a bare `u64` for binary formats (MessagePack) and a
/// decimal *string* for human-readable ones (JSON). The string form is not
/// cosmetic: IDs routinely exceed 2^53, so a JSON number would be silently
/// mangled by any JavaScript client. See the `serde` impls below.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Id(pub u64);

impl Id {
    pub const ZERO: Id = Id(0);
    /// Sorts after every ID this system can generate; useful as a cursor
    /// sentinel meaning "from the newest message backwards".
    pub const MAX: Id = Id(u64::MAX);

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Milliseconds since the Unix epoch at which this ID was minted.
    #[inline]
    pub const fn timestamp_ms(self) -> u64 {
        (self.0 >> TIME_SHIFT) + TC_EPOCH_MS
    }

    #[inline]
    pub const fn node(self) -> u16 {
        ((self.0 >> SEQ_BITS) & NODE_MASK) as u16
    }

    /// The smallest ID that could have been minted at `unix_ms`.
    ///
    /// Lets a time range become a primary-key range: `id >= floor(t0) AND id <
    /// floor(t1)` is an index scan, no date functions, no extra column.
    #[inline]
    pub const fn floor_for_ms(unix_ms: u64) -> Id {
        Id(unix_ms.saturating_sub(TC_EPOCH_MS) << TIME_SHIFT)
    }

    #[inline]
    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Id({})", self.0)
    }
}

impl From<u64> for Id {
    fn from(v: u64) -> Self {
        Id(v)
    }
}

impl FromStr for Id {
    type Err = std::num::ParseIntError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        s.parse::<u64>().map(Id)
    }
}

impl serde::Serialize for Id {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            // JSON: emit a string. JS numbers lose precision past 2^53 and a
            // Snowflake blows past that immediately.
            s.collect_str(&self.0)
        } else {
            s.serialize_u64(self.0)
        }
    }
}

impl<'de> serde::Deserialize<'de> for Id {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Id;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a u64 id or its decimal string form")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Id, E> {
                Ok(Id(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Id, E> {
                Ok(Id(v as u64))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Id, E> {
                v.parse::<u64>().map(Id).map_err(serde::de::Error::custom)
            }
        }
        // Accept either shape regardless of format, so a JSON-ish client
        // talking MessagePack (or vice versa) still interoperates.
        d.deserialize_any(V)
    }
}

/// Lock-free monotonic ID generator.
///
/// Timestamp and sequence live in a single `AtomicU64` so the whole
/// allocate-an-ID operation is one CAS loop — no mutex, no contention cliff
/// when many connection tasks send at once.
#[derive(Debug)]
pub struct IdGen {
    node: u64,
    /// Packed `last_ms << SEQ_BITS | seq`.
    state: AtomicU64,
}

impl IdGen {
    pub fn new(node: u16) -> Self {
        IdGen {
            node: (node as u64) & NODE_MASK,
            state: AtomicU64::new(0),
        }
    }

    #[inline]
    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(TC_EPOCH_MS)
            .saturating_sub(TC_EPOCH_MS)
    }

    /// Mint the next ID. Strictly increasing per generator, even if the wall
    /// clock steps backwards (NTP) or the per-ms sequence space is exhausted:
    /// in both cases we borrow from the future rather than emit a duplicate.
    pub fn next(&self) -> Id {
        let mut cur = self.state.load(Ordering::Relaxed);
        loop {
            let now = Self::now_ms();
            let last_ms = cur >> SEQ_BITS;
            let seq = cur & SEQ_MASK;

            let next = if now > last_ms {
                now << SEQ_BITS
            } else if seq < SEQ_MASK {
                cur + 1
            } else {
                // Sequence exhausted for this millisecond: roll into the next
                // one. Callers are never blocked; IDs stay unique and ordered.
                (last_ms + 1) << SEQ_BITS
            };

            match self
                .state
                .compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => {
                    let ms = next >> SEQ_BITS;
                    let seq = next & SEQ_MASK;
                    return Id((ms << TIME_SHIFT) | (self.node << SEQ_BITS) | seq);
                }
                Err(actual) => cur = actual,
            }
        }
    }
}

impl Default for IdGen {
    fn default() -> Self {
        IdGen::new(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique_and_monotonic_under_contention() {
        let idgen = std::sync::Arc::new(IdGen::new(7));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let idgen = idgen.clone();
            handles.push(std::thread::spawn(move || {
                (0..20_000).map(|_| idgen.next()).collect::<Vec<_>>()
            }));
        }
        let mut all = Vec::new();
        for h in handles {
            let batch = h.join().unwrap();
            // Each thread observes its own IDs in strictly increasing order.
            assert!(batch.windows(2).all(|w| w[0] < w[1]), "not monotonic");
            all.extend(batch);
        }
        let unique: HashSet<u64> = all.iter().map(|i| i.0).collect();
        assert_eq!(unique.len(), all.len(), "duplicate ids minted");
        assert!(all.iter().all(|i| i.node() == 7));
    }

    #[test]
    fn timestamp_roundtrips_and_floor_brackets_the_range() {
        let id = IdGen::new(1).next();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(id.timestamp_ms().abs_diff(now) < 5_000);

        let lo = Id::floor_for_ms(id.timestamp_ms());
        let hi = Id::floor_for_ms(id.timestamp_ms() + 1);
        assert!(lo <= id && id < hi, "floor must bracket the id");
    }

    #[test]
    fn id_is_a_number_in_msgpack_but_a_string_in_json() {
        // This is load-bearing: a JSON number would be corrupted by JS clients,
        // and a MessagePack string would waste bytes on the hottest field.
        let id = Id(1_234_567_890_123_456_789);
        assert_eq!(
            serde_json::to_string(&id).unwrap(),
            "\"1234567890123456789\""
        );

        let mp = rmp_serde::to_vec_named(&id).unwrap();
        assert_eq!(mp.len(), 9, "expected a 9-byte msgpack uint64");
        assert_eq!(mp[0], 0xcf, "expected the uint64 marker");

        assert_eq!(rmp_serde::from_slice::<Id>(&mp).unwrap(), id);
        assert_eq!(
            serde_json::from_str::<Id>("\"1234567890123456789\"").unwrap(),
            id
        );
        // Tolerant in both directions.
        assert_eq!(serde_json::from_str::<Id>("42").unwrap(), Id(42));
    }
}
