//! Concurrent-writer tests.
//!
//! These use a real file-backed database rather than the in-memory store,
//! because the failure they guard against only exists when several connections
//! contend for SQLite's single write lock — and the in-memory store is capped
//! at one connection.
//!
//! # The bug this exists to prevent
//!
//! SQLite's `busy_timeout` does *not* apply to a `DEFERRED` transaction that
//! reads first and then tries to upgrade to a write. Such a transaction already
//! holds a read snapshot that a competing writer may have invalidated, so
//! SQLite has no choice but to fail it immediately with `SQLITE_BUSY` — waiting
//! could deadlock. Every write path in `tc-store` therefore opens its
//! transaction as `IMMEDIATE`, which takes the write lock up front so
//! contending writers queue on the timeout instead of erroring.
//!
//! Without that, concurrent message sends fail under perfectly ordinary load.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tensorchat_core::{ChannelKind, Id, IdGen};
use tensorchat_store::{NewChannel, NewMessage, Store};

struct Fixture {
    store: Store,
    ids: Arc<IdGen>,
    channel: Id,
    users: Vec<Id>,
    _dir: tempdir::TempDir,
}

/// A minimal scoped temporary directory, so this test needs no dev-dependency.
mod tempdir {
    use std::path::{Path, PathBuf};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new(tag: &str) -> std::io::Result<TempDir> {
            let mut path = std::env::temp_dir();
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            path.push(format!(
                "tc-test-{tag}-{unique}-{:?}",
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&path)?;
            Ok(TempDir(path))
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

fn fixture(users: usize) -> Fixture {
    let dir = tempdir::TempDir::new("concurrency").expect("temp dir");
    let store = Store::open(dir.path().join("test.db")).expect("open store");
    let ids = Arc::new(IdGen::new(1));

    let members: Vec<Id> = (0..users)
        .map(|i| {
            store
                .create_user(
                    ids.next(),
                    &format!("user{i}"),
                    &format!("User {i}"),
                    "hash",
                )
                .expect("create user")
                .id
        })
        .collect();

    let channel = store
        .create_channel(NewChannel {
            id: ids.next(),
            kind: ChannelKind::Public,
            name: "general",
            topic: "",
            created_by: members[0],
            created_at: 1,
            members: members.clone(),
        })
        .expect("create channel")
        .id;

    Fixture {
        store,
        ids,
        channel,
        users: members,
        _dir: dir,
    }
}

#[test]
fn concurrent_message_writes_all_succeed() {
    let fx = fixture(8);
    let failures = Arc::new(AtomicUsize::new(0));
    let per_thread = 40;

    let mut handles = Vec::new();
    for (t, author) in fx.users.iter().copied().enumerate() {
        let store = fx.store.clone();
        let ids = fx.ids.clone();
        let channel = fx.channel;
        let failures = failures.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..per_thread {
                let body = format!("thread {t} message {i}");
                let result = store.insert_message(NewMessage {
                    id: ids.next(),
                    channel_id: channel,
                    author_id: author,
                    body: &body,
                    thread_root: None,
                    attachments: &[],
                    mentions: &[],
                });
                if let Err(e) = result {
                    // Print the first few so a regression names itself rather
                    // than showing up as a bare count.
                    if failures.fetch_add(1, Ordering::Relaxed) < 3 {
                        eprintln!("insert failed: {e}");
                    }
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }

    assert_eq!(
        failures.load(Ordering::Relaxed),
        0,
        "concurrent writers must queue on the write lock, not fail"
    );

    let page = fx
        .store
        .history(fx.channel, fx.users[0], None, 200)
        .expect("read history");
    let expected = fx.users.len() * per_thread;
    // Every write landed, and the channel pointer tracks the newest of them.
    assert_eq!(
        fx.store.channel(fx.channel).unwrap().last_message,
        page.messages[0].id
    );
    let mut total = page.messages.len();
    let mut cursor = page.next_cursor;
    while let Some(c) = cursor {
        let next = fx
            .store
            .history(fx.channel, fx.users[0], Some(c), 200)
            .unwrap();
        total += next.messages.len();
        cursor = next.next_cursor;
    }
    assert_eq!(total, expected, "no writes were lost");
}

#[test]
fn readers_are_not_blocked_by_a_writer() {
    // WAL's central property: a reader never waits for the writer. If this
    // regresses (for example by switching the journal mode), reads would start
    // failing or stalling under write load.
    let fx = fixture(4);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let writer = {
        let store = fx.store.clone();
        let ids = fx.ids.clone();
        let (channel, author) = (fx.channel, fx.users[0]);
        let stop = stop.clone();
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(Ordering::Relaxed) {
                let body = format!("write {n}");
                store
                    .insert_message(NewMessage {
                        id: ids.next(),
                        channel_id: channel,
                        author_id: author,
                        body: &body,
                        thread_root: None,
                        attachments: &[],
                        mentions: &[],
                    })
                    .expect("write during concurrent reads");
                n += 1;
            }
            n
        })
    };

    let mut reads = 0;
    for _ in 0..200 {
        fx.store
            .history(fx.channel, fx.users[1], None, 20)
            .expect("read during concurrent writes");
        reads += 1;
    }
    stop.store(true, Ordering::Relaxed);
    let writes = writer.join().expect("writer panicked");

    assert_eq!(reads, 200);
    assert!(writes > 0, "the writer should have made progress");
}

#[test]
fn concurrent_reaction_toggles_converge() {
    let fx = fixture(6);
    let message = fx
        .store
        .insert_message(NewMessage {
            id: fx.ids.next(),
            channel_id: fx.channel,
            author_id: fx.users[0],
            body: "react to me",
            thread_root: None,
            attachments: &[],
            mentions: &[],
        })
        .expect("seed message")
        .id;

    let mut handles = Vec::new();
    for user in fx.users.iter().copied() {
        let store = fx.store.clone();
        handles.push(std::thread::spawn(move || {
            for _ in 0..25 {
                store.set_reaction(message, user, "👍", true).expect("add");
                store
                    .set_reaction(message, user, "👍", false)
                    .expect("remove");
            }
            // Everyone ends up having reacted.
            store
                .set_reaction(message, user, "👍", true)
                .expect("final add");
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }

    let page = fx.store.history(fx.channel, fx.users[0], None, 10).unwrap();
    let reaction = page.messages[0]
        .reactions
        .iter()
        .find(|r| r.emoji == "👍")
        .expect("reaction survived the churn");
    assert_eq!(
        reaction.count as usize,
        fx.users.len(),
        "each user should be counted exactly once"
    );
}

#[test]
fn concurrent_dm_creation_yields_one_conversation() {
    // Two people opening a DM with each other at the same moment must converge
    // on a single channel; the unique index is what makes the loser re-read
    // rather than create a duplicate.
    let fx = fixture(2);
    let (a, b) = (fx.users[0], fx.users[1]);

    let mut handles = Vec::new();
    for (from, to) in [(a, b), (b, a)] {
        let store = fx.store.clone();
        let ids = fx.ids.clone();
        handles.push(std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..10 {
                seen.push(
                    store
                        .open_dm(ids.next(), from, vec![to], 1)
                        .expect("open dm")
                        .id,
                );
            }
            seen
        }));
    }

    let mut all: Vec<Id> = Vec::new();
    for h in handles {
        all.extend(h.join().expect("worker panicked"));
    }
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 1, "a DM must never be duplicated, got {all:?}");
}

/// A single-use invite must admit exactly one account no matter how many people
/// redeem it at once.
///
/// This is the reason `create_user_via_invite` claims the seat with a
/// conditional `UPDATE` inside the same transaction as the account insert. The
/// obvious implementation — read `uses`, compare against `max_uses`, then insert
/// — passes every single-threaded test and over-subscribes the link the moment
/// two people click it together.
#[test]
fn a_single_use_invite_admits_exactly_one_racing_account() {
    let fx = fixture(1);
    let seats = 1;
    let contenders = 8;

    fx.store
        .create_invite(tensorchat_store::NewInvite {
            id: fx.ids.next(),
            token_hash: b"race",
            label: "",
            created_by: Some(fx.users[0]),
            created_at: 1,
            expires_at: None,
            max_uses: seats,
        })
        .expect("create invite");

    let admitted = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();
    for i in 0..contenders {
        let store = fx.store.clone();
        let ids = fx.ids.clone();
        let admitted = admitted.clone();
        handles.push(std::thread::spawn(move || {
            let handle = format!("newcomer{i}");
            match store.create_user_via_invite(ids.next(), &handle, &handle, "hash", b"race", 2) {
                Ok(_) => {
                    admitted.fetch_add(1, Ordering::Relaxed);
                }
                // Forbidden is the expected loss: the seat was already taken.
                Err(tensorchat_store::Error::Forbidden) => {}
                Err(e) => panic!("unexpected error redeeming an invite: {e}"),
            }
        }));
    }
    for h in handles {
        h.join().expect("worker panicked");
    }

    assert_eq!(
        admitted.load(Ordering::Relaxed),
        seats as usize,
        "a {seats}-seat invite admitted the wrong number of accounts"
    );
    // And the counter agrees with reality, rather than having been incremented
    // by losers whose inserts rolled back.
    let invite = &fx.store.invites().expect("list invites")[0];
    assert_eq!(invite.uses, seats);
    assert!(!invite.is_live(2), "the link is spent");
}
