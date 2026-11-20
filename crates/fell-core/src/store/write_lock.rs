//! Who gets the mailbox write lock next.
//!
//! SQLite has one write lock per file and no queue. A connection that finds
//! it taken sleeps in its busy handler and tries again. SQLite's own handler
//! sleeps up to 100 ms between tries, while sync, when the server streams
//! faster than the store writes, commits one batch and begins the next within
//! microseconds. A user write then almost never saw the lock free and gave up
//! after its 5 s timeout with "database is locked".
//!
//! Every store connection here tries every few milliseconds and waits up to
//! [`LOCK_WAIT`]. A connection waiting for any write but sync's notes it on
//! the file's [`Turns`], and sync stands aside before its next batch until
//! that write has the lock. The turn works between connections in one
//! process; another process (`fell-mcp`) waits the same way without it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::Result;

/// How long a write waits for the lock before it fails. No transaction Fell
/// runs comes close, so reaching it means another process is stuck holding it.
const LOCK_WAIT: Duration = Duration::from_secs(30);
/// A write still counts as waiting this long after its last try. Longer
/// than the longest gap between tries.
const STILL_WAITING: Duration = Duration::from_millis(20);
/// The longest sync stands aside per batch, so a write that is itself
/// waiting on another process cannot stall sync.
const MOST_YIELD: Duration = Duration::from_secs(1);

/// What every connection this process has on one mailbox file shares.
#[derive(Default)]
pub(crate) struct Turns {
    /// When a write other than sync's last found the lock taken, as
    /// nanoseconds since [`epoch`] plus one. 0 is never.
    waiting_at: AtomicU64,
}

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn now_mark() -> u64 {
    u64::try_from(epoch().elapsed().as_nanos())
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

/// Names one mailbox file: its device and inode, so every path to it
/// (a symlink, a second hard link) shares one [`Turns`].
#[cfg(unix)]
pub(crate) type FileKey = (u64, u64);
/// Names one mailbox file: the path every store in this process opens it by.
#[cfg(not(unix))]
pub(crate) type FileKey = std::path::PathBuf;

impl Turns {
    /// The one `Turns` for the mailbox `key` names, as long as a store has
    /// it open.
    pub(crate) fn for_file(key: FileKey) -> Arc<Self> {
        static FILES: OnceLock<Mutex<HashMap<FileKey, Weak<Turns>>>> = OnceLock::new();
        let mut files = FILES
            .get_or_init(Mutex::default)
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        files.retain(|_, turns| turns.strong_count() > 0);
        if let Some(turns) = files.get(&key).and_then(Weak::upgrade) {
            return turns;
        }
        let turns = Arc::new(Self::default());
        files.insert(key, Arc::downgrade(&turns));
        turns
    }

    fn note_waiting(&self) {
        self.waiting_at.store(now_mark(), Ordering::Relaxed);
    }

    fn someone_waiting(&self) -> bool {
        let at = self.waiting_at.load(Ordering::Relaxed);
        let window = u64::try_from(STILL_WAITING.as_nanos()).unwrap_or(u64::MAX);
        at != 0 && now_mark().saturating_sub(at) < window
    }

    /// Before a sync batch: let a waiting write take the lock first.
    fn stand_aside(&self) {
        let started = Instant::now();
        while self.someone_waiting() && started.elapsed() < MOST_YIELD {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// Lowers the flag it holds when dropped.
struct Lowered<'a>(&'a AtomicBool);

impl Drop for Lowered<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// How one store connection waits for the write lock.
pub(crate) struct LockWait {
    turns: Arc<Turns>,
    /// Set while sync begins a batch: its waits do not hold sync back.
    background: AtomicBool,
    /// Set while a call must fail at once instead of waiting.
    no_wait: AtomicBool,
}

impl LockWait {
    /// Replace `conn`'s busy handler with this one.
    pub(crate) fn install(conn: &mut Connection, turns: Arc<Turns>) -> Result<Arc<Self>> {
        let wait = Arc::new(Self {
            turns,
            background: AtomicBool::new(false),
            no_wait: AtomicBool::new(false),
        });
        let state = Arc::clone(&wait);
        let mut since = Instant::now();
        conn.busy_handler(Some(move |tries: i32| {
            if state.no_wait.load(Ordering::Relaxed) {
                return false;
            }
            if tries == 0 {
                since = Instant::now();
            }
            if !state.background.load(Ordering::Relaxed) {
                state.turns.note_waiting();
            }
            if since.elapsed() >= LOCK_WAIT {
                return false;
            }
            // 1 ms, then up to 5 ms: the lock is often free for only a moment.
            let pause = u64::try_from(tries.clamp(0, 4)).unwrap_or(4) + 1;
            std::thread::sleep(Duration::from_millis(pause));
            true
        }))?;
        Ok(wait)
    }

    /// Run `f` with lock waits failing at once.
    pub(crate) fn without_waiting<T>(&self, f: impl FnOnce() -> T) -> T {
        self.no_wait.store(true, Ordering::Relaxed);
        let _reset = Lowered(&self.no_wait);
        f()
    }

    /// Begin a sync write with `begin`, after any write already waiting.
    pub(crate) fn behind_waiting_writes<T>(&self, begin: impl FnOnce() -> T) -> T {
        self.turns.stand_aside();
        self.background.store(true, Ordering::Relaxed);
        let _reset = Lowered(&self.background);
        begin()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use crate::account::{Account, AccountId, BackendKind};
    use crate::backend::MailboxDelta;
    use crate::compose::{DraftFields, DraftSession, ReplyParts};
    use crate::message::{Address, Message, MessageHeaders, MessageId, RemoteId};
    use crate::store::Store;
    use crate::threading::ThreadId;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fell-write-lock-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn account() -> Account {
        Account {
            id: AccountId("a1".into()),
            display_name: "Test".into(),
            email: "me@example.test".into(),
            backend_kind: BackendKind::Imap,
        }
    }

    /// One INBOX copy as IMAP sync stores it.
    fn inbox_message(uid: u32) -> Message {
        let id = MessageId::new(format!("m{uid}@example.test"));
        let body = format!("Message {uid}. ").repeat(40);
        Message {
            account_id: AccountId("a1".into()),
            headers: MessageHeaders {
                message_id: id.clone(),
                proton_conversation_id: None,
                in_reply_to: None,
                references: Vec::new(),
                from: Address {
                    name: Some(format!("Sender {}", uid % 97)),
                    email: format!("sender{}@example.test", uid % 97),
                },
                subject: format!("Subject {uid}"),
                date_unix: 1_700_000_000 + i64::from(uid),
            },
            remote_id: RemoteId(format!("imap:1:{uid}:INBOX")),
            thread_id: ThreadId::from_message_id(&id),
            snippet: body.chars().take(80).collect(),
            body_text: Some(body),
            unread: true,
            folders: vec!["INBOX".into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
            reply_to: Some(Vec::new()),
            attachment_names: Vec::new(),
            parts: None,
        }
    }

    fn delta(first_uid: u32, count: u32) -> MailboxDelta {
        MailboxDelta {
            messages: (first_uid..first_uid + count).map(inbox_message).collect(),
            ..MailboxDelta::default()
        }
    }

    /// A sync pass whose server streams faster than the store writes: one
    /// 32-message batch after another, with no pause between commits.
    struct Sync {
        stop: Arc<AtomicBool>,
        batches: Arc<AtomicUsize>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Sync {
        fn start(path: &Path, first_uid: u32) -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let batches = Arc::new(AtomicUsize::new(0));
            let (ready_tx, ready) = std::sync::mpsc::channel();
            let thread = std::thread::spawn({
                let path = path.to_path_buf();
                let stop = Arc::clone(&stop);
                let batches = Arc::clone(&batches);
                move || {
                    let store = Store::open(&path).unwrap();
                    let account_id = AccountId("a1".into());
                    let mut uid = first_uid;
                    while !stop.load(Ordering::Relaxed) {
                        store.apply_delta(&account_id, &delta(uid, 32)).unwrap();
                        uid += 32;
                        if batches.fetch_add(1, Ordering::Relaxed) == 2 {
                            let _ = ready_tx.send(());
                        }
                    }
                }
            });
            ready
                .recv_timeout(Duration::from_secs(60))
                .expect("sync never wrote");
            Self {
                stop,
                batches,
                thread: Some(thread),
            }
        }

        fn batches(&self) -> usize {
            self.batches.load(Ordering::Relaxed)
        }

        fn finish(mut self) {
            self.stop.store(true, Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap();
        }
    }

    impl Drop for Sync {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn timed<T>(what: &str, run: impl FnOnce() -> T) -> (T, Duration) {
        let started = Instant::now();
        let out = run();
        let took = started.elapsed();
        eprintln!("{what}: {took:?}");
        (out, took)
    }

    fn plan(store: &Store, sql: &str, params: impl rusqlite::Params) -> String {
        let plan: Vec<String> = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(params, |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        plan.join("\n")
    }

    /// Sync writes one flag per server copy at the end of a mailbox, then
    /// sweeps the folder. A lookup there that walks the account or the
    /// mailbox makes that one transaction quadratic: before these indexes a
    /// 50,000-message mailbox held the write lock for about 20 minutes.
    #[test]
    fn end_of_mailbox_writes_seek_by_row() {
        let store = Store::open_in_memory().unwrap();
        let targets = plan(
            &store,
            crate::store::FLAG_TARGETS,
            rusqlite::params!["a1", "imap:1:1:INBOX", "INBOX", 1, 1],
        );
        // Every step seeks a row by more than the account.
        for (step, index) in [
            (
                "messages",
                "idx_messages_remote (account_id=? AND remote_id=?)",
            ),
            (
                "message_aliases",
                "idx_message_aliases_remote (account_id=? AND remote_id=?)",
            ),
            ("messages", "(account_id=? AND message_id=?)"),
        ] {
            assert!(
                targets
                    .lines()
                    .any(|line| line.contains(step) && line.contains(index)),
                "{targets}"
            );
        }
        assert!(!targets.contains("(account_id=?)"), "{targets}");
        assert!(!targets.contains("SCAN"), "{targets}");
        // The sweep walks the folder's rows once, and finds each row's copy
        // by row, not by reading the mailbox's copies.
        let sweep = plan(
            &store,
            super::super::copies::SWEEP_STALE,
            rusqlite::params!["a1", "INBOX", "INBOX", 1],
        );
        assert!(
            sweep.contains("idx_message_copies_row (account_id=? AND message_id=?)"),
            "{sweep}"
        );
        assert!(!sweep.contains("SCAN"), "{sweep}");
    }

    /// Enough flags to split the delta in two: it ends where one
    /// transaction would have.
    #[test]
    fn a_delta_split_in_two_lands_whole() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let account_id = AccountId("a1".into());
        let total = u32::try_from(2 * super::super::SPLIT_DELTA_AT + 100).unwrap();
        store.apply_delta(&account_id, &delta(1, total)).unwrap();
        // The copies were recorded when the rows were stored. Forget them so
        // the listing's found copies are what puts them back.
        store
            .conn
            .execute("DELETE FROM message_copies", [])
            .unwrap();

        let mut reply = inbox_message(total + 1);
        reply.headers.in_reply_to = Some(MessageId::new("m5@example.test"));
        reply.headers.references = vec![MessageId::new("m5@example.test")];
        let present: Vec<u32> = (1..=total + 1).collect();
        let end = MailboxDelta {
            messages: vec![reply.clone()],
            flags: (1..=total)
                .map(|uid| crate::backend::FlagUpdate {
                    remote_id: RemoteId(format!("imap:1:{uid}:INBOX")),
                    unread: Some(false),
                    flagged: None,
                    keywords: None,
                    modseq: None,
                })
                .collect(),
            removals: vec![crate::backend::FolderRemoval {
                mailbox: "INBOX".into(),
                folder: "INBOX".into(),
                kind: crate::backend::RemovalKind::Listing {
                    uidvalidity: 1,
                    present: present.clone(),
                    through: total + 1,
                    found: present
                        .iter()
                        .map(|uid| (*uid, MessageId::new(format!("m{uid}@example.test"))))
                        .collect(),
                    sweep: true,
                },
            }],
            cursor: Some(crate::backend::SyncCursor {
                account_key: "a1".into(),
                token: "after-the-end".into(),
            }),
            settle: crate::backend::Settle::All,
            roles: None,
            progress: None,
            too_large: Vec::new(),
        };
        store.apply_delta(&account_id, &end).unwrap();

        let unread: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE unread = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(unread, 1, "only the new reply is unread");
        let copies: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM message_copies", [], |row| row.get(0))
            .unwrap();
        assert_eq!(copies, i64::from(total + 1));
        let thread = inbox_message(5).thread_id;
        let joined = store.thread_messages(&account_id, &thread).unwrap();
        assert_eq!(joined.len(), 2, "the reply joined its parent's thread");
        let threads = store.list_threads(&account_id).unwrap();
        assert_eq!(threads.len(), usize::try_from(total).unwrap());
        assert_eq!(threads.iter().filter(|thread| thread.unread).count(), 1);
        assert_eq!(
            store.sync_cursor(&account_id).unwrap().unwrap().token,
            "after-the-end"
        );
        // Every row kept its folder: the listing named every copy.
        let foldered: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE folders = 'INBOX'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(foldered, i64::from(total + 1));
    }

    /// The end of a pass over INBOX: each copy in `uids` with the unread
    /// state the server reported when the pass fetched flags, and a listing
    /// up to `through` that found every one of them.
    fn end_of_inbox(
        uids: std::ops::RangeInclusive<u32>,
        through: u32,
        unread: bool,
        token: &str,
    ) -> MailboxDelta {
        let present: Vec<u32> = uids.collect();
        MailboxDelta {
            flags: present
                .iter()
                .map(|uid| crate::backend::FlagUpdate {
                    remote_id: RemoteId(format!("imap:1:{uid}:INBOX")),
                    unread: Some(unread),
                    flagged: None,
                    keywords: None,
                    modseq: None,
                })
                .collect(),
            removals: vec![crate::backend::FolderRemoval {
                mailbox: "INBOX".into(),
                folder: "INBOX".into(),
                kind: crate::backend::RemovalKind::Listing {
                    uidvalidity: 1,
                    present: present.clone(),
                    through,
                    found: present
                        .iter()
                        .map(|uid| (*uid, MessageId::new(format!("m{uid}@example.test"))))
                        .collect(),
                    sweep: true,
                },
            }],
            cursor: Some(crate::backend::SyncCursor {
                account_key: "a1".into(),
                token: token.into(),
            }),
            settle: crate::backend::Settle::All,
            roles: None,
            ..MailboxDelta::default()
        }
    }

    /// A later pass that asked for flags changed since the stored modseq
    /// and got none.
    fn nothing_changed(token: &str) -> MailboxDelta {
        MailboxDelta {
            cursor: Some(crate::backend::SyncCursor {
                account_key: "a1".into(),
                token: token.into(),
            }),
            ..MailboxDelta::default()
        }
    }

    fn unread_of(store: &Store, uid: u32) -> bool {
        store
            .conn
            .query_row(
                "SELECT unread FROM messages WHERE message_id = ?1",
                [format!("m{uid}@example.test")],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    fn token(store: &Store) -> Option<String> {
        store
            .sync_cursor(&AccountId("a1".into()))
            .unwrap()
            .map(|cursor| cursor.token)
    }

    /// An agent marks a thread read while sync is part way through the
    /// flags at the end of a mailbox. Those flags are what the server said
    /// before the pass, and the pass stores the modseq, so the next pass
    /// never sends that flag again: if a later part of the delta wrote the
    /// old unread state over the agent's change, it would stay wrong.
    #[test]
    fn a_thread_marked_read_while_sync_commits_part_of_a_delta_stays_read() {
        let dir = scratch("flag-gap");
        let path = dir.join("mail.db");
        let ui = Store::open(&path).unwrap();
        ui.upsert_account(&account()).unwrap();
        let account_id = AccountId("a1".into());
        ui.apply_delta(&account_id, &delta(1, 2_100)).unwrap();

        let sync = Store::open(&path).unwrap();
        let marked = Arc::new(AtomicUsize::new(0));
        let mut agent = Some(Store::open(&path).unwrap());
        *sync.on_sync_commit.borrow_mut() = Some(Box::new({
            let marked = Arc::clone(&marked);
            move || {
                // Once, at the first point sync lets another write in.
                if let Some(agent) = agent.take() {
                    let thread = inbox_message(2_050).thread_id;
                    agent.mark_thread_read(&AccountId("a1".into()), &thread, "agent")?;
                    marked.fetch_add(1, Ordering::Relaxed);
                }
                Ok(())
            }
        }));
        sync.apply_delta(
            &account_id,
            &end_of_inbox(1..=2_100, 2_100, true, "modseq-1"),
        )
        .unwrap();
        assert_eq!(
            marked.load(Ordering::Relaxed),
            1,
            "sync let the agent in mid-delta"
        );
        assert!(!unread_of(&ui, 2_050), "the agent's read was overwritten");
        assert!(unread_of(&ui, 2_049));
        assert_eq!(token(&ui).as_deref(), Some("modseq-1"));

        // The next pass asks for flags changed since modseq-1: none.
        sync.apply_delta(&account_id, &nothing_changed("modseq-2"))
            .unwrap();
        assert!(!unread_of(&ui, 2_050));
        assert_eq!(token(&ui).as_deref(), Some("modseq-2"));
        drop((sync, ui));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whenever another write can get in during a delta, the copies table is
    /// either as it was before the delta or as the whole listing left it: an
    /// archive queued then sees every UID it must move and none the listing
    /// is about to drop.
    #[test]
    fn a_write_during_a_delta_never_sees_half_a_listing() {
        let dir = scratch("copies-gap");
        let path = dir.join("mail.db");
        let sync = Store::open(&path).unwrap();
        sync.upsert_account(&account()).unwrap();
        let account_id = AccountId("a1".into());
        sync.apply_delta(&account_id, &delta(1, 2_100)).unwrap();
        // UID 5000 is gone from the server: the listing below drops it.
        sync.apply_delta(&account_id, &delta(5_000, 1)).unwrap();
        // Copies recorded before Fell kept them: the listing records them.
        sync.conn
            .execute("DELETE FROM message_copies WHERE uid <= 2100", [])
            .unwrap();
        let copies = "SELECT COUNT(*) FROM message_copies";
        let gone = "SELECT COUNT(*) FROM message_copies WHERE uid = 5000";

        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let reader = Store::open(&path).unwrap();
        *sync.on_sync_commit.borrow_mut() = Some(Box::new({
            let seen = Arc::clone(&seen);
            move || {
                seen.lock()
                    .unwrap()
                    .push((count(&reader, copies), count(&reader, gone)));
                Ok(())
            }
        }));
        sync.apply_delta(&account_id, &end_of_inbox(1..=2_100, 5_000, false, "done"))
            .unwrap();
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.is_empty(), "sync let other writes in mid-delta");
        for state in &seen {
            assert_eq!(*state, (1, 1), "a write saw part of the listing: {seen:?}");
        }
        assert_eq!(count(&sync, copies), 2_100);
        assert_eq!(count(&sync, gone), 0);
        drop(sync);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Sync stops part way through a delta, and the next pass sends the
    /// same delta again from the stored cursor. Nothing of the first try
    /// that a later write could land between is left half done, and the
    /// retry ends where one clean try would have.
    #[test]
    fn a_delta_cut_short_is_redone_whole() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let account_id = AccountId("a1".into());
        store.apply_delta(&account_id, &delta(1, 2_100)).unwrap();
        store
            .conn
            .execute("DELETE FROM message_copies", [])
            .unwrap();
        let unread = "SELECT COUNT(*) FROM messages WHERE unread = 1";
        let copies = "SELECT COUNT(*) FROM message_copies";
        let end = end_of_inbox(1..=2_100, 2_100, false, "modseq-1");

        // Stopped among the flags: none of them landed.
        store.fail_after_flags.set(Some(1_500));
        assert!(store.apply_delta(&account_id, &end).is_err());
        store.fail_after_flags.set(None);
        assert_eq!(count(&store, unread), 2_100, "some flags landed");
        assert_eq!(count(&store, copies), 0);
        assert_eq!(token(&store), None);

        // Stopped where sync lets other writes in: every flag or none, and
        // no copies or cursor yet.
        let mut stop = true;
        *store.on_sync_commit.borrow_mut() = Some(Box::new(move || {
            if std::mem::take(&mut stop) {
                return Err(crate::Error::Backend("sync stopped".into()));
            }
            Ok(())
        }));
        assert!(store.apply_delta(&account_id, &end).is_err());
        let left = count(&store, unread);
        assert!(left == 0 || left == 2_100, "{left} flags still unread");
        assert_eq!(count(&store, copies), 0);
        assert_eq!(token(&store), None);

        // The retry.
        store.apply_delta(&account_id, &end).unwrap();
        assert_eq!(count(&store, unread), 0);
        assert_eq!(count(&store, copies), 2_100);
        assert_eq!(token(&store).as_deref(), Some("modseq-1"));

        // A later change sticks through the next pass.
        let thread = inbox_message(2_050).thread_id;
        store
            .mark_thread_unread(&account_id, &thread, "agent")
            .unwrap();
        store
            .apply_delta(&account_id, &nothing_changed("modseq-2"))
            .unwrap();
        assert!(unread_of(&store, 2_050));
    }

    #[test]
    fn a_message_committed_before_a_split_delta_error_is_already_stored_on_retry() {
        let store = Store::open_in_memory().unwrap();
        let account = account();
        store.upsert_account(&account).unwrap();
        let total = u32::try_from(super::super::SPLIT_DELTA_AT).unwrap();
        let mut end = end_of_inbox(1..=total, total, false, "modseq-1");
        let message = inbox_message(1);
        let id = message.headers.message_id.clone();
        end.messages.push(message);

        *store.on_sync_commit.borrow_mut() = Some(Box::new(|| {
            Err(crate::Error::Backend(
                "stopped after messages and flags".into(),
            ))
        }));
        assert!(store.apply_sync_delta(&account.id, &end).is_err());
        assert!(store.message(&account.id, &id).unwrap().is_some());
        assert_eq!(token(&store), None);

        store.on_sync_commit.borrow_mut().take();
        let retry = store.apply_sync_delta(&account.id, &end).unwrap();
        assert!(retry.changed);
        assert_eq!(retry.new_messages, 0);
        assert_eq!(token(&store).as_deref(), Some("modseq-1"));
    }

    /// A store opened through a symlink or a second hard link shares the
    /// turn of one opened by the file's own name.
    #[cfg(unix)]
    #[test]
    fn every_path_to_a_mailbox_shares_its_turn() {
        let dir = scratch("paths");
        let path = dir.join("mail.db");
        let by_name = Store::open(&path).unwrap();
        let symlink = dir.join("symlink.db");
        std::os::unix::fs::symlink(&path, &symlink).unwrap();
        let hard_link = dir.join("hard-link.db");
        std::fs::hard_link(&path, &hard_link).unwrap();
        let by_symlink = Store::open(&symlink).unwrap();
        let by_hard_link = Store::open(&hard_link).unwrap();
        let other_dir = scratch("paths-other");
        let other = Store::open(other_dir.join("mail.db")).unwrap();

        let turns = |store: &Store| Arc::clone(&store.lock_wait.turns);
        assert!(Arc::ptr_eq(&turns(&by_name), &turns(&by_symlink)));
        assert!(Arc::ptr_eq(&turns(&by_name), &turns(&by_hard_link)));
        assert!(!Arc::ptr_eq(&turns(&by_name), &turns(&other)));
        drop((by_name, by_symlink, by_hard_link, other));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    #[test]
    fn archive_undo_and_a_new_draft_land_while_sync_writes_back_to_back() {
        let dir = scratch("ui");
        let path = dir.join("mail.db");
        let ui = Store::open(&path).unwrap();
        ui.upsert_account(&account()).unwrap();
        let account_id = AccountId("a1".into());
        let target = inbox_message(1);
        ui.apply_delta(&account_id, &delta(1, 1)).unwrap();

        let sync = Sync::start(&path, 1_000);
        // Generous for a loaded machine, and well under the 5 s lock wait
        // after which a write used to give up.
        let quick = Duration::from_secs(2);

        let (archived, took) = timed("archive", || {
            ui.archive_thread(&account_id, &target.thread_id, "INBOX")
        });
        archived.expect("archive failed while sync was writing");
        assert!(took < quick, "archive waited {took:?} for sync");

        let (undone, took) = timed("undo", || ui.undo(&account_id));
        undone.expect("undo failed while sync was writing");
        assert!(took < quick, "undo waited {took:?} for sync");

        let root = dir.join("drafts");
        let (started, took) = timed("new draft", || {
            DraftSession::start_reply_in(
                &root,
                "c1",
                account_id.clone(),
                DraftFields {
                    from: "me@example.test".into(),
                    ..DraftFields::default()
                },
                "",
                &ReplyParts::default(),
                &path,
            )
        });
        started.expect("starting a draft failed while sync was writing");
        assert!(took < quick, "starting a draft waited {took:?} for sync");

        // Sync kept going once the reader had its turn.
        let before = sync.batches();
        let deadline = Instant::now() + Duration::from_secs(30);
        while sync.batches() < before + 3 {
            assert!(Instant::now() < deadline, "sync stalled after the actions");
            std::thread::sleep(Duration::from_millis(5));
        }
        sync.finish();
        drop(ui);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn percentile(sorted: &[Duration], p: f64) -> Duration {
        if sorted.is_empty() {
            return Duration::ZERO;
        }
        sorted[((sorted.len() - 1) as f64 * p).round() as usize]
    }

    /// Archive and undo latency while sync streams batches into a large
    /// mailbox, then while sync applies one mailbox's end-of-pass delta
    /// (flags for every copy, the UID listing, and the folder sweep).
    /// `cargo test -p fell-core --release measure_action_latency -- --ignored --nocapture`
    /// with `FELL_BENCH_MESSAGES` (default 50000).
    #[test]
    #[ignore = "measurement, minutes in a debug build"]
    fn measure_action_latency_during_a_large_sync() {
        let messages: u32 = std::env::var("FELL_BENCH_MESSAGES")
            .ok()
            .and_then(|raw| raw.parse().ok())
            .unwrap_or(50_000);
        let dir = scratch("bench");
        let path = dir.join("mail.db");
        let ui = Store::open(&path).unwrap();
        ui.upsert_account(&account()).unwrap();
        let account_id = AccountId("a1".into());
        let seeded = Instant::now();
        let mut uid = 1;
        while uid <= messages {
            let count = 500.min(messages + 1 - uid);
            ui.apply_delta(&account_id, &delta(uid, count)).unwrap();
            uid += count;
        }
        eprintln!("seeded {messages} messages in {:?}", seeded.elapsed());

        let report = |what: &str, mut took: Vec<Duration>, failed: usize| {
            took.sort();
            eprintln!(
                "{what}: n={} failed={failed} p50={:?} p95={:?} max={:?}",
                took.len(),
                percentile(&took, 0.5),
                percentile(&took, 0.95),
                took.last().copied().unwrap_or_default()
            );
        };

        // End of a mailbox: every copy's flags and the UID listing, while
        // the store holds exactly `messages` rows.
        let total = ui
            .list_threads(&account_id)
            .map(|threads| threads.len())
            .unwrap_or(0);
        let present: Vec<u32> = (1..=messages).collect();
        let end = MailboxDelta {
            flags: present
                .iter()
                .map(|uid| crate::backend::FlagUpdate {
                    remote_id: RemoteId(format!("imap:1:{uid}:INBOX")),
                    unread: Some(uid % 3 == 0),
                    flagged: None,
                    keywords: None,
                    modseq: None,
                })
                .collect(),
            removals: vec![crate::backend::FolderRemoval {
                mailbox: "INBOX".into(),
                folder: "INBOX".into(),
                kind: crate::backend::RemovalKind::Listing {
                    uidvalidity: 1,
                    present: present.clone(),
                    through: messages,
                    found: present
                        .iter()
                        .map(|uid| (*uid, MessageId::new(format!("m{uid}@example.test"))))
                        .collect(),
                    sweep: true,
                },
            }],
            settle: crate::backend::Settle::All,
            ..MailboxDelta::default()
        };
        let done = Arc::new(AtomicBool::new(false));
        let writer = std::thread::spawn({
            let path = path.clone();
            let done = Arc::clone(&done);
            move || {
                let store = Store::open(&path).unwrap();
                let split = Arc::new(std::sync::Mutex::new(None));
                *store.on_sync_commit.borrow_mut() = Some(Box::new({
                    let split = Arc::clone(&split);
                    move || {
                        *split.lock().unwrap() = Some(Instant::now());
                        Ok(())
                    }
                }));
                let started = Instant::now();
                store.apply_delta(&AccountId("a1".into()), &end).unwrap();
                let finished = Instant::now();
                done.store(true, Ordering::Relaxed);
                if let Some(at) = *split.lock().unwrap() {
                    eprintln!(
                        "  flags transaction {:?}, copies transaction {:?}",
                        at - started,
                        finished - at
                    );
                }
                finished - started
            }
        });
        std::thread::sleep(Duration::from_millis(20));
        let mut took = Vec::new();
        let mut failed = 0;
        let mut round = 0u32;
        while !done.load(Ordering::Relaxed) {
            let thread = inbox_message(3 + round * 11).thread_id;
            round += 1;
            let started = Instant::now();
            match ui.archive_thread(&account_id, &thread, "INBOX") {
                Ok(_) => {
                    took.push(started.elapsed());
                    let started = Instant::now();
                    match ui.undo(&account_id) {
                        Ok(_) => took.push(started.elapsed()),
                        Err(_) => failed += 1,
                    }
                }
                Err(_) => failed += 1,
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let end_took = writer.join().unwrap();
        eprintln!("end-of-mailbox delta ({total} threads) took {end_took:?}");
        report("archive+undo during the end-of-mailbox delta", took, failed);

        // Streaming batches.
        let sync = Sync::start(&path, messages + 1);
        let mut took = Vec::new();
        let mut failed = 0;
        for round in 0..20u32 {
            let thread = inbox_message(1 + round * 7).thread_id;
            let started = Instant::now();
            if ui.archive_thread(&account_id, &thread, "INBOX").is_err() {
                failed += 1;
                continue;
            }
            took.push(started.elapsed());
            std::thread::sleep(Duration::from_millis(50));
            let started = Instant::now();
            match ui.undo(&account_id) {
                Ok(_) => took.push(started.elapsed()),
                Err(_) => failed += 1,
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let batches = sync.batches();
        sync.finish();
        report("archive+undo during batches", took, failed);
        eprintln!("sync batches meanwhile: {batches}");
        drop(ui);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
