//! `tc-store` — persistence on SQLite.
//!
//! # Why SQLite
//!
//! A chat server's working set is dominated by "the last few hundred messages
//! in the channels I have open". That is a page-cache problem, not a
//! distributed-systems problem. Running the database in-process removes a
//! network hop, a connection pool's worth of context switches, and a
//! serialization round trip from every read — a local SQLite query costs
//! microseconds where a Postgres round trip costs hundreds. In WAL mode readers
//! never block the writer and the writer never blocks readers, which is exactly
//! the concurrency shape of a chat workload.
//!
//! # Blocking
//!
//! Every method here is synchronous and may block. Callers on an async runtime
//! must wrap them in `spawn_blocking` (tc-server does this in one place, see
//! `db::Db`). Queries are single-digit microseconds, so the handoff cost
//! dominates — but blocking a reactor thread on a page fault is still not
//! something to leave to chance.
//!
//! # Concurrency model
//!
//! WAL permits many concurrent readers and exactly one writer. The pool is
//! sized for readers; writers serialize on SQLite's write lock, with
//! `busy_timeout` absorbing contention rather than surfacing `SQLITE_BUSY`.

use std::path::Path;

use r2d2::{Pool, PooledConnection};
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::{Connection, TransactionBehavior};
use tc_core::Id;

mod channels;
mod messages;
mod search;
mod users;

pub use channels::NewChannel;
pub use messages::{HistoryPage, NewMessage};
pub use search::SearchQuery;

/// Schema version embedded in the database via `PRAGMA user_version`.
const SCHEMA_VERSION: i32 = 1;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("connection pool error: {0}")]
    Pool(#[from] r2d2::Error),
    #[error("not found")]
    NotFound,
    /// A uniqueness rule was violated (duplicate handle, channel name, ...).
    #[error("{0} already exists")]
    Conflict(&'static str),
    #[error("not permitted")]
    Forbidden,
    #[error("invalid input: {0}")]
    Invalid(&'static str),
    /// The on-disk schema is newer than this binary understands. Refusing to
    /// run beats silently corrupting data with stale queries.
    #[error("database schema is version {found}, this build supports {supported}")]
    SchemaTooNew { found: i32, supported: i32 },
}

impl Error {
    /// Translate SQLite's constraint errors into domain errors, so callers can
    /// react to "that handle is taken" without string-matching driver output.
    pub(crate) fn from_sqlite(e: rusqlite::Error, unique_means: &'static str) -> Error {
        use rusqlite::ErrorCode;
        if let rusqlite::Error::SqliteFailure(f, _) = &e
            && f.code == ErrorCode::ConstraintViolation
        {
            return Error::Conflict(unique_means);
        }
        Error::Sqlite(e)
    }
}

/// A handle to the database. Cheap to clone — it is a pool handle.
#[derive(Clone)]
pub struct Store {
    pool: Pool<SqliteConnectionManager>,
}

/// The connection-level pragmas that make SQLite behave well as a server
/// database. Applied to every pooled connection, since most are per-connection
/// state rather than persistent database settings.
fn tune(conn: &mut Connection) -> rusqlite::Result<()> {
    // `rarray(?)` lets us bind a whole id list to one placeholder, so batch
    // hydration reuses a single prepared statement regardless of page size.
    rusqlite::vtab::array::load_module(conn)?;

    conn.pragma_update(None, "journal_mode", "WAL")?;
    // NORMAL fsyncs the WAL at checkpoints rather than on every commit. In WAL
    // mode this is durable across process crashes (only a power loss can lose
    // recently committed transactions) and is worth roughly an order of
    // magnitude on write throughput.
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Absorb writer contention inside SQLite instead of returning SQLITE_BUSY
    // for the application to retry.
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    // Negative = kibibytes rather than pages: 64 MiB of page cache per
    // connection, enough to hold the hot recent-message set outright.
    conn.pragma_update(None, "cache_size", -65_536)?;
    // Keep transient sort/join b-trees off disk; they are small and bounded.
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    // Let the OS map the database rather than copying pages through read().
    conn.pragma_update(None, "mmap_size", 268_435_456i64)?;
    // Checkpoint the WAL a bit later than the 1000-page default; fewer, larger
    // checkpoints beat frequent small ones for a write-heavy workload.
    conn.pragma_update(None, "wal_autocheckpoint", 2_000)?;
    Ok(())
}

impl Store {
    /// Open (creating if needed) a database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        let manager = SqliteConnectionManager::file(path).with_init(tune);
        // Readers scale with cores; the single WAL writer serializes anyway, so
        // a larger pool would only add memory, not throughput.
        let max = (std::thread::available_parallelism().map_or(4, |n| n.get()) as u32).clamp(4, 16);
        let pool = Pool::builder().max_size(max).build(manager)?;
        let store = Store { pool };
        store.migrate()?;
        Ok(store)
    }

    /// An ephemeral database, for tests.
    ///
    /// Capped at one connection: `SqliteConnectionManager::memory()` gives each
    /// new connection its *own* blank database, so a larger pool would hand out
    /// connections that cannot see each other's writes.
    pub fn open_in_memory() -> Result<Store> {
        let manager = SqliteConnectionManager::memory().with_init(tune);
        let pool = Pool::builder().max_size(1).build(manager)?;
        let store = Store { pool };
        store.migrate()?;
        Ok(store)
    }

    pub(crate) fn conn(&self) -> Result<PooledConnection<SqliteConnectionManager>> {
        Ok(self.pool.get()?)
    }

    /// Apply the schema if this is a fresh database.
    ///
    /// Version 1 is the initial schema, so there is nothing to upgrade *from*
    /// yet; the version gate exists so that the first real migration has a
    /// correct starting point, and so a database written by a newer build fails
    /// loudly here instead of being queried with stale SQL.
    fn migrate(&self) -> Result<()> {
        let mut conn = self.conn()?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;

        if version > SCHEMA_VERSION {
            return Err(Error::SchemaTooNew {
                found: version,
                supported: SCHEMA_VERSION,
            });
        }
        if version == SCHEMA_VERSION {
            return Ok(());
        }

        // IMMEDIATE, not the default DEFERRED. A deferred transaction that
        // reads first and only later writes must be failed with SQLITE_BUSY the
        // moment it tries to upgrade, because its read snapshot may already be
        // stale — `busy_timeout` cannot rescue it. Taking the write lock up
        // front means contending writers queue on the timeout instead.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch(include_str!("schema.sql"))?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        Ok(())
    }

    /// Checkpoint the WAL and refresh planner statistics. Worth calling on a
    /// timer in a long-lived process.
    pub fn maintenance(&self) -> Result<()> {
        let conn = self.conn()?;
        conn.execute_batch("PRAGMA wal_checkpoint(PASSIVE); ANALYZE;")?;
        Ok(())
    }
}

/// SQLite stores signed 64-bit integers. Our IDs are unsigned, but every ID
/// this system can mint stays below 2^63 (the timestamp field would have to
/// reach the year 2163 to overflow), so the bit pattern round-trips exactly.
#[inline]
pub(crate) fn to_sql(id: Id) -> i64 {
    id.0 as i64
}

#[inline]
pub(crate) fn from_sql(v: i64) -> Id {
    Id(v as u64)
}

/// Pack user ids into a compact little-endian blob for `messages.mentions`.
pub(crate) fn pack_ids(ids: &[Id]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ids.len() * 8);
    for id in ids {
        out.extend_from_slice(&id.0.to_le_bytes());
    }
    out
}

/// Inverse of [`pack_ids`]. Ignores a trailing partial element rather than
/// panicking, so a corrupt row degrades to a missing mention.
pub(crate) fn unpack_ids(blob: &[u8]) -> Vec<Id> {
    blob.chunks_exact(8)
        .map(|c| {
            Id(u64::from_le_bytes(
                c.try_into().expect("chunks_exact yields 8 bytes"),
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_is_migrated_and_idempotent() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.conn().unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        drop(conn);
        // Re-running migrate must be a no-op, not "table already exists".
        store.migrate().unwrap();
    }

    #[test]
    fn fts5_is_available_in_this_build() {
        // The entire search feature depends on the bundled SQLite shipping
        // FTS5; fail here with a clear message rather than deep inside a query.
        let store = Store::open_in_memory().unwrap();
        let conn = store.conn().unwrap();
        conn.execute_batch("CREATE VIRTUAL TABLE probe USING fts5(x);")
            .expect("bundled SQLite must include FTS5");
    }

    #[test]
    fn strict_tables_reject_wrong_types() {
        let store = Store::open_in_memory().unwrap();
        let conn = store.conn().unwrap();
        let bad = conn.execute(
            "INSERT INTO users (id, handle, display_name, password_hash) \
             VALUES ('nope', 'a', 'A', 'x')",
            [],
        );
        assert!(bad.is_err(), "STRICT should reject a text id");
    }

    #[test]
    fn id_packing_roundtrips() {
        let ids = vec![Id(1), Id(u64::MAX >> 1), Id(0)];
        assert_eq!(unpack_ids(&pack_ids(&ids)), ids);
        assert!(unpack_ids(&[]).is_empty());
        // Truncated blob: drop the partial tail rather than panic.
        assert_eq!(unpack_ids(&[1, 2, 3]), Vec::<Id>::new());
    }
}
