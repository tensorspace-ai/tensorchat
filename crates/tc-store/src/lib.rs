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
mod pins;
mod saved;
mod search;
mod tokens;
mod users;

pub use channels::NewChannel;
pub use messages::{HistoryPage, NewMessage};
pub use pins::MAX_PINS_PER_CHANNEL;
pub use saved::MAX_SAVED_PAGE;
pub use search::SearchQuery;
pub use tokens::ApiToken;

/// Schema version embedded in the database via `PRAGMA user_version`.
const SCHEMA_VERSION: i32 = 6;

/// Incremental upgrades, each paired with the version it produces.
///
/// A database created by an older build replays every entry newer than its own
/// version. A *fresh* database skips all of them and gets `schema.sql`, which is
/// always the current schema — replaying years of history to build an empty
/// database would be pointless, and it would make `schema.sql` stop being a
/// readable description of what the tables actually are.
///
/// The risk in having two paths is that they drift, so
/// `migrations_match_a_fresh_schema` runs both and compares the result. Adding a
/// migration without updating `schema.sql`, or vice versa, fails that test.
///
/// Entry `001` is the version 1 baseline and is never applied by
/// [`Store::migrate`] — nothing predates it. It exists so the equivalence test
/// has somewhere to start.
const MIGRATIONS: &[(i32, &str)] = &[
    (1, include_str!("migrations/001_initial.sql")),
    (2, include_str!("migrations/002_pins.sql")),
    (3, include_str!("migrations/003_saved.sql")),
    (4, include_str!("migrations/004_mute.sql")),
    (5, include_str!("migrations/005_admin.sql")),
    (6, include_str!("migrations/006_api_tokens.sql")),
];

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

    /// Bring the database up to [`SCHEMA_VERSION`], creating it if it is empty.
    ///
    /// Runs on every open, so a deployment upgrades by restarting the binary —
    /// there is no separate migration step to forget. A database written by a
    /// *newer* build fails loudly here rather than being queried with stale SQL.
    ///
    /// The whole upgrade is one transaction: a half-applied schema is the one
    /// state from which there is no automatic recovery, so an interrupted or
    /// failing migration must leave the database exactly as it was.
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
        // front means contending writers queue on the timeout instead. It also
        // serializes two processes starting at once, so the second one waits
        // and then finds the schema already current rather than racing it.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version == 0 {
            // Fresh: the current schema outright, no history to replay.
            tx.execute_batch(include_str!("schema.sql"))?;
        } else {
            for (_, sql) in MIGRATIONS.iter().filter(|(v, _)| *v > version) {
                tx.execute_batch(sql)?;
            }
        }
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

    /// A database's schema, described structurally rather than as source text.
    ///
    /// `sqlite_master.sql` is the *original* CREATE statement, comments and all,
    /// and `ALTER TABLE ADD COLUMN` splices into that text rather than
    /// regenerating it. Comparing it would make this test fail on a difference
    /// in prose, and pass or fail unpredictably around any migration that uses
    /// ALTER. So compare what SQLite actually enforces: the objects that exist,
    /// each table's columns, and each table's indexes.
    fn schema_of(conn: &Connection) -> Vec<String> {
        let mut out = Vec::new();

        let mut objects = conn
            .prepare(
                "SELECT type, name, coalesce(sql, '') FROM sqlite_master \
                 WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name",
            )
            .unwrap();
        let rows: Vec<(String, String, String)> = objects
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        drop(objects);

        for (kind, name, sql) in rows {
            match kind.as_str() {
                "table" => {
                    out.push(format!("table {name}"));
                    let mut cols = conn.prepare(&format!("PRAGMA table_info({name})")).unwrap();
                    let mut described: Vec<String> = cols
                        .query_map([], |r| {
                            Ok(format!(
                                "  col {} {} notnull={} default={:?} pk={}",
                                r.get::<_, String>(1)?,
                                r.get::<_, String>(2)?,
                                r.get::<_, i64>(3)?,
                                r.get::<_, Option<String>>(4)?,
                                r.get::<_, i64>(5)?,
                            ))
                        })
                        .unwrap()
                        .collect::<rusqlite::Result<_>>()
                        .unwrap();
                    // Column *order* is part of the schema for `SELECT *`, but
                    // nothing here selects star, and ALTER can only append. Sort
                    // so that "same columns, added at a different point in the
                    // list" is not reported as a difference.
                    described.sort();
                    out.extend(described);
                }
                // Indexes, triggers and views are never rewritten in place, so
                // their text is a fair comparison — and for a trigger the text
                // *is* the behavior.
                _ => out.push(format!("{kind} {name} {}", normalize_sql(&sql))),
            }
        }
        out
    }

    /// Collapse whitespace so formatting differences between `schema.sql` and a
    /// migration file are not mistaken for schema differences.
    fn normalize_sql(sql: &str) -> String {
        sql.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn migrations_match_a_fresh_schema() {
        // The one thing that can go wrong with keeping `schema.sql` current
        // *and* shipping incremental migrations is that they drift: a new build
        // creates a table on a fresh install that an upgraded database never
        // gets, and the bug only appears on someone else's server. So build a
        // database both ways and compare what SQLite ended up with.
        let fresh = Connection::open_in_memory().unwrap();
        fresh
            .execute_batch(include_str!("schema.sql"))
            .expect("fresh schema must apply");

        let upgraded = Connection::open_in_memory().unwrap();
        for (version, sql) in MIGRATIONS {
            upgraded
                .execute_batch(sql)
                .unwrap_or_else(|e| panic!("migration {version} failed: {e}"));
        }

        assert_eq!(
            schema_of(&upgraded),
            schema_of(&fresh),
            "schema.sql and the migration chain have diverged — a change landed \
             in one but not the other"
        );
    }

    #[test]
    fn the_migration_list_is_ordered_and_reaches_the_current_version() {
        let versions: Vec<i32> = MIGRATIONS.iter().map(|(v, _)| *v).collect();
        let mut sorted = versions.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(versions, sorted, "migrations must be ascending and unique");
        assert_eq!(
            versions.last().copied(),
            Some(SCHEMA_VERSION),
            "the last migration must produce SCHEMA_VERSION"
        );
    }

    #[test]
    fn an_older_database_is_upgraded_in_place_without_losing_rows() {
        // Stand up a database at the oldest supported version, put a row in it,
        // then let `Store::open` find it and bring it forward.
        let dir = std::env::temp_dir().join(format!("tc-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);

        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0].1).unwrap();
            conn.pragma_update(None, "user_version", MIGRATIONS[0].0)
                .unwrap();
            conn.execute(
                "INSERT INTO users (id, handle, display_name, password_hash) \
                 VALUES (1, 'alice', 'Alice', 'phc')",
                [],
            )
            .unwrap();
        }

        let store = Store::open(&path).expect("an older database must open");
        let conn = store.conn().unwrap();
        let v: i32 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        let handle: String = conn
            .query_row("SELECT handle FROM users WHERE id = 1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(handle, "alice", "existing data must survive the upgrade");
        assert_eq!(schema_of(&conn), {
            let fresh = Connection::open_in_memory().unwrap();
            fresh.execute_batch(include_str!("schema.sql")).unwrap();
            schema_of(&fresh)
        });

        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        // Better to fail at startup than to query tomorrow's tables with
        // today's SQL and corrupt them.
        let dir = std::env::temp_dir().join(format!("tc-newer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("future.db");
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        assert!(matches!(
            Store::open(&path),
            Err(Error::SchemaTooNew { .. })
        ));
        let _ = std::fs::remove_dir_all(&dir);
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
