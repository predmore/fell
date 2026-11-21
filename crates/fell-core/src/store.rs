use std::borrow::Cow;
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::account::{Account, AccountId, BackendKind};
use crate::backend::MailboxDelta;
use crate::embed::{
    Embedding, dequeue_message, enqueue_message, invalidate_message_embedding,
    remove_message_embedding,
};
use crate::folders::{FolderRole, FolderRoles};
use crate::message::{
    Address, Message, MessageHeaders, MessageId, RemoteId, body_fingerprint, decode_addresses,
    encode_addresses, is_short_reply_body,
};
use crate::search::{
    IndexedText, SearchHit, SearchOptions, SearchResults, fts5_phrase, sync_terms,
    terms_for_indexed,
};
use crate::threading::{ThreadId, resolve_thread_id};

pub(crate) mod bodies;
pub(crate) mod copies;
pub(crate) mod intents;
pub(crate) mod keywords;
mod part_cache;
mod roles;
pub(crate) mod sent_bcc;
mod sent_echo_repair;
mod summaries;
mod threads;
pub(crate) mod views;
mod write_lock;

pub use bodies::{BodyRequest, PartKinds, PartStamp};
pub use intents::PendingFlags;
pub(crate) use summaries::folder_names as message_folder_names;
pub use summaries::{FolderPage, FolderSpan, ListFilter, ListKey, ListSource, ThreadList};

pub(crate) fn listed_folder_roles(
    conn: &Connection,
    account: &AccountId,
) -> Result<Option<FolderRoles>> {
    let Some(roles) = roles::listed(conn, account)? else {
        return Ok(None);
    };
    Ok(Some(
        roles
            .with_bridge_names(views::is_bridge(conn, account)?)
            .with_views(views::move_exclusions(conn, account)?),
    ))
}

const SCHEMA: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS accounts (
    id TEXT PRIMARY KEY,
    display_name TEXT NOT NULL,
    email TEXT NOT NULL,
    backend_kind TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    rowid INTEGER PRIMARY KEY,
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    remote_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    proton_conversation_id TEXT,
    in_reply_to TEXT,
    references_header TEXT NOT NULL,
    from_name TEXT,
    from_email TEXT NOT NULL,
    subject TEXT NOT NULL,
    date_unix INTEGER NOT NULL,
    snippet TEXT NOT NULL,
    body_text TEXT,
    unread INTEGER NOT NULL DEFAULT 1,
    folders TEXT NOT NULL DEFAULT '',
    labels TEXT NOT NULL DEFAULT '',
    arrived_via TEXT NOT NULL DEFAULT '',
    to_line TEXT NOT NULL DEFAULT '',
    cc_line TEXT NOT NULL DEFAULT '',
    bcc_line TEXT NOT NULL DEFAULT '',
    starred INTEGER NOT NULL DEFAULT 0,
    has_attachment INTEGER NOT NULL DEFAULT 0,
    archived INTEGER NOT NULL DEFAULT 0,
    snooze_until INTEGER,
    body_fingerprint TEXT NOT NULL DEFAULT '',
    list_id TEXT,
    reply_to_line TEXT NOT NULL DEFAULT '',
    attachment_names TEXT NOT NULL DEFAULT '',
    part_cache_stamp BLOB,
    FOREIGN KEY(account_id) REFERENCES accounts(id),
    UNIQUE (account_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(account_id, thread_id);
CREATE INDEX IF NOT EXISTS idx_messages_account_date ON messages(account_id, date_unix DESC);

CREATE TABLE IF NOT EXISTS message_aliases (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    canonical_message_id TEXT NOT NULL,
    remote_id TEXT NOT NULL DEFAULT '',
    folder TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (account_id, message_id)
);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    subject,
    sender,
    recipients,
    body,
    account_id UNINDEXED,
    message_id UNINDEXED
);

CREATE TABLE IF NOT EXISTS agent_grants (
    agent_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    enabled INTEGER NOT NULL,
    can_read INTEGER NOT NULL,
    can_triage INTEGER NOT NULL,
    can_draft INTEGER NOT NULL,
    PRIMARY KEY (agent_id, account_id),
    FOREIGN KEY(account_id) REFERENCES accounts(id)
);

CREATE TABLE IF NOT EXISTS agent_actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id TEXT NOT NULL,
    actor TEXT NOT NULL,
    kind TEXT NOT NULL,
    summary TEXT NOT NULL,
    undone INTEGER NOT NULL DEFAULT 0,
    created_unix INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_agent_actions_account ON agent_actions(account_id, id);

CREATE TABLE IF NOT EXISTS agent_action_messages (
    action_id INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    labels TEXT NOT NULL,
    folders TEXT NOT NULL,
    unread INTEGER NOT NULL,
    archived INTEGER NOT NULL,
    snooze_until INTEGER,
    -- NULL on rows logged before star was undoable: undo leaves the star.
    starred INTEGER,
    label_was_keyword INTEGER,
    FOREIGN KEY(action_id) REFERENCES agent_actions(id)
);

CREATE TABLE IF NOT EXISTS agent_action_drafts (
    action_id INTEGER NOT NULL,
    draft_id INTEGER NOT NULL,
    FOREIGN KEY(action_id) REFERENCES agent_actions(id)
);

CREATE TABLE IF NOT EXISTS review_drafts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id TEXT NOT NULL,
    actor TEXT NOT NULL,
    to_addrs TEXT NOT NULL,
    cc_addrs TEXT NOT NULL,
    bcc_addrs TEXT NOT NULL,
    subject TEXT NOT NULL,
    body TEXT NOT NULL,
    in_reply_to TEXT,
    thread_id TEXT,
    created_unix INTEGER NOT NULL,
    FOREIGN KEY(account_id) REFERENCES accounts(id)
);

CREATE TABLE IF NOT EXISTS compose_drafts (
    id TEXT PRIMARY KEY,
    account_id TEXT NOT NULL,
    from_addr TEXT NOT NULL,
    to_addrs TEXT NOT NULL,
    cc_addrs TEXT NOT NULL,
    bcc_addrs TEXT NOT NULL,
    subject TEXT NOT NULL,
    body TEXT NOT NULL,
    in_reply_to TEXT,
    identities TEXT NOT NULL DEFAULT '',
    owner TEXT NOT NULL DEFAULT '',
    updated_unix INTEGER NOT NULL,
    references_header TEXT NOT NULL DEFAULT '',
    quote_text TEXT NOT NULL DEFAULT '',
    quote_html TEXT NOT NULL DEFAULT '',
    reply_to_unverified INTEGER NOT NULL DEFAULT 0,
    discarded_action_id INTEGER,
    FOREIGN KEY(account_id) REFERENCES accounts(id)
);

CREATE TABLE IF NOT EXISTS compose_attachments (
    draft_id TEXT NOT NULL,
    path TEXT NOT NULL,
    name TEXT NOT NULL,
    bytes BLOB NOT NULL,
    PRIMARY KEY (draft_id, path),
    FOREIGN KEY(draft_id) REFERENCES compose_drafts(id) ON DELETE CASCADE
);

-- How the last send of a draft ended: 'maybe-sent' from the claim on,
-- then 'sent', or 'clear' when nothing went out. See compose::persist.
CREATE TABLE IF NOT EXISTS compose_send (
    account_id TEXT NOT NULL,
    draft_id TEXT NOT NULL,
    state TEXT NOT NULL,
    message_id TEXT,
    updated_unix INTEGER NOT NULL,
    PRIMARY KEY (account_id, draft_id)
);
CREATE INDEX IF NOT EXISTS idx_compose_send_message
ON compose_send(account_id, state, message_id);
"#;

const FTS_TABLE: &str = r#"
CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject,
    sender,
    recipients,
    body,
    account_id UNINDEXED,
    message_id UNINDEXED
);
"#;

const FTS_COPY: &str = r#"
INSERT INTO messages_fts(subject, sender, recipients, body, account_id, message_id)
SELECT
    subject,
    trim(coalesce(from_name, '') || ' ' || from_email),
    trim(
        replace(replace(to_line, char(31), ' '), char(30), ' ') || ' ' ||
        replace(replace(cc_line, char(31), ' '), char(30), ' ') || ' ' ||
        replace(replace(bcc_line, char(31), ' '), char(30), ' ')
    ),
    COALESCE(body_text, snippet),
    account_id,
    message_id
FROM messages;
"#;

pub(crate) const DROP_FTS_TRIGGERS: &str = r#"
DROP TRIGGER IF EXISTS messages_ai;
DROP TRIGGER IF EXISTS messages_ad;
DROP TRIGGER IF EXISTS messages_au;
"#;

const FTS_TRIGGERS: &str = r#"
CREATE TRIGGER messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(subject, sender, recipients, body, account_id, message_id)
    VALUES (
        new.subject,
        trim(coalesce(new.from_name, '') || ' ' || new.from_email),
        trim(
            replace(replace(new.to_line, char(31), ' '), char(30), ' ') || ' ' ||
            replace(replace(new.cc_line, char(31), ' '), char(30), ' ') || ' ' ||
            replace(replace(new.bcc_line, char(31), ' '), char(30), ' ')
        ),
        COALESCE(new.body_text, new.snippet),
        new.account_id,
        new.message_id
    );
END;

CREATE TRIGGER messages_ad AFTER DELETE ON messages BEGIN
    DELETE FROM messages_fts
    WHERE account_id = old.account_id AND message_id = old.message_id;
END;

CREATE TRIGGER messages_au AFTER UPDATE OF
    subject, from_name, from_email, body_text, snippet, to_line, cc_line, bcc_line
ON messages BEGIN
    DELETE FROM messages_fts
    WHERE account_id = old.account_id AND message_id = old.message_id;
    INSERT INTO messages_fts(subject, sender, recipients, body, account_id, message_id)
    VALUES (
        new.subject,
        trim(coalesce(new.from_name, '') || ' ' || new.from_email),
        trim(
            replace(replace(new.to_line, char(31), ' '), char(30), ' ') || ' ' ||
            replace(replace(new.cc_line, char(31), ' '), char(30), ' ') || ' ' ||
            replace(replace(new.bcc_line, char(31), ' '), char(30), ' ')
        ),
        COALESCE(new.body_text, new.snippet),
        new.account_id,
        new.message_id
    );
END;
"#;

pub(crate) const SELECT_MESSAGE_COLS: &str =
    "account_id, message_id, remote_id, thread_id, proton_conversation_id,
        in_reply_to, references_header, from_name, from_email, subject,
        date_unix, snippet, body_text, unread, folders, labels, arrived_via,
        to_line, cc_line, bcc_line, starred, has_attachment, archived, snooze_until, list_id,
        reply_to_line, attachment_names";

fn message_lookup_sql() -> String {
    // Separate the two lookups so SQLite can seek by account and message ID.
    format!(
        "SELECT {SELECT_MESSAGE_COLS} FROM messages m
         WHERE m.account_id = ?1 AND m.message_id = ?2
         UNION ALL
         SELECT {SELECT_MESSAGE_COLS} FROM messages m
         WHERE m.account_id = ?1 AND m.message_id = (
             SELECT a.canonical_message_id FROM message_aliases a
             WHERE a.account_id = ?1 AND a.message_id = ?2
         )
         LIMIT 1"
    )
}

/// Flags in a delta, at or past which [`Store::apply_delta`] commits them
/// before the delta's copy work and lets a waiting write in between.
const SPLIT_DELTA_AT: usize = 1_000;

/// The rows one server copy's flag lands on: the row it is, the row a
/// merged copy joined, and the row its recorded copy backs. Each part seeks
/// its own index. As one `OR`, SQLite walked every message in the account
/// for each flag, and the flags at the end of a large mailbox held the
/// write lock for many minutes.
const FLAG_TARGETS: &str = "SELECT message_id FROM messages
     WHERE account_id = ?1 AND message_id IN (
         SELECT message_id FROM messages
         WHERE account_id = ?1 AND remote_id = ?2
         UNION ALL
         SELECT canonical_message_id FROM message_aliases
         WHERE account_id = ?1 AND remote_id = ?2
         UNION ALL
         SELECT message_id FROM message_copies
         WHERE account_id = ?1 AND mailbox = ?3
           AND uidvalidity = ?4 AND uid = ?5
     )";

pub(crate) enum ReplyToVerification<'a> {
    Present(&'a crate::backend::FetchedReplyTo),
    Absent(&'a [RemoteId]),
}

/// On-disk mailbox. The UI reads this; sync writes it.
pub struct Store {
    /// File-backed stores fill body vectors here. In-memory stores leave this
    /// empty so tests can index on the calling thread.
    indexer: Option<crate::embed::Indexer>,
    /// Query model and vector matrix for hybrid search, loaded on first use.
    semantic: std::cell::RefCell<crate::search::semantic::Semantic>,
    pub(crate) conn: Connection,
    path: Option<std::path::PathBuf>,
    /// This connection's busy handler, shared with the file's other stores
    /// in this process so sync lets their writes go first.
    lock_wait: std::sync::Arc<write_lock::LockWait>,
    /// Shared lock on the mailbox inode, held until this store is dropped.
    /// Dropped after the SQLite connection so a new opener cannot migrate
    /// the WAL while this connection still has the file.
    #[cfg(unix)]
    _mailbox_lock: Option<std::fs::File>,
    /// Tests compare incremental threading against the whole-account pass.
    #[cfg(test)]
    full_rethread: std::cell::Cell<bool>,
    /// Tests: runs each time [`Store::apply_delta`] commits part of a delta
    /// and before it begins the rest. An error stops the delta there, as a
    /// crash would.
    #[cfg(test)]
    on_sync_commit: std::cell::RefCell<Option<SyncCommitHook>>,
    /// Tests: [`Store::apply_delta`] fails after this many flags.
    #[cfg(test)]
    fail_after_flags: std::cell::Cell<Option<usize>>,
}

#[cfg(test)]
type SyncCommitHook = Box<dyn FnMut() -> Result<()> + Send>;

/// One page of FTS hits. `next_cursor` is passed back to [`Store::search_page`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchPage {
    pub hits: Vec<SearchHit>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSummary {
    pub thread_id: ThreadId,
    pub subject: String,
    pub last_date_unix: i64,
    pub message_count: usize,
    pub unread: bool,
}

pub use bodies::cid_upgrade::{CidRepairBatch, CidRepairProgress};

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_mailbox(path.as_ref(), true)
    }

    pub(crate) fn open_for_part_fetch(path: &Path) -> Result<Self> {
        Self::open_mailbox(path, false)
    }

    fn open_mailbox(requested: &Path, index: bool) -> Result<Self> {
        // The mailbox holds message text. Create it owner-only before SQLite writes.
        create_private_file(requested)?;
        // One path string per inode. A symlink or a second hard link must not
        // open a different `-wal` while another connection still has the file.
        let claimed = claim_mailbox(requested)?;
        #[cfg(unix)]
        let file = {
            use std::os::unix::fs::MetadataExt;
            let meta = claimed.lock.metadata()?;
            (meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let file = claimed.path.clone();
        let turns = write_lock::Turns::for_file(file);
        let mut store = Self::from_connection(claimed.conn, turns)?;
        store.path = Some(claimed.path.clone());
        #[cfg(unix)]
        {
            store._mailbox_lock = Some(claimed.lock);
        }
        store.seal_private_files()?;
        // Embeddings are filled off this thread, and only in one process.
        // A lock problem skips the indexer. Open still returns the mailbox.
        if index {
            store.indexer = crate::embed::Indexer::try_spawn(&claimed.path);
        }
        Ok(store)
    }

    /// Copy the write-ahead log into the database file. Neither form waits:
    /// a passive checkpoint copies what no reader still needs, and `truncate`
    /// also resets the log to zero bytes, but only if no other connection is
    /// reading or writing at that moment. Otherwise it gives up at once
    /// instead of holding writers back for the busy timeout. Returns the
    /// frames left in the log, or -1 when the reset was skipped.
    pub fn checkpoint(&self, truncate: bool) -> Result<i64> {
        let sql = if truncate {
            "PRAGMA wal_checkpoint(TRUNCATE)"
        } else {
            "PRAGMA wal_checkpoint(PASSIVE)"
        };
        let run = || {
            self.conn.query_row(sql, [], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })
        };
        let result = if truncate {
            self.lock_wait.without_waiting(run)
        } else {
            run()
        };
        match result {
            Ok((busy, _, _)) if busy != 0 => Ok(-1),
            Ok((_, log, done)) => Ok((log - done).max(0)),
            // Another connection held the lock: try again after the next pass.
            Err(rusqlite::Error::SqliteFailure(err, _))
                if err.code == rusqlite::ErrorCode::DatabaseBusy =>
            {
                Ok(-1)
            }
            Err(err) => Err(err.into()),
        }
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, std::sync::Arc::default())
    }

    fn from_connection(
        mut conn: Connection,
        turns: std::sync::Arc<write_lock::Turns>,
    ) -> Result<Self> {
        let lock_wait = write_lock::LockWait::install(&mut conn, turns)?;
        // Sync writes and a folder read, whose window bounds come in several
        // shapes, together use more than rusqlite's default of 16 cached
        // statements. Past that, each read prepares its SQL again.
        conn.set_prepared_statement_cache_capacity(64);
        // A first sync can push gigabytes through the log. SQLite reuses the
        // file after a checkpoint but never shrinks it; this trims it back.
        conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
        conn.execute_batch(SCHEMA)?;
        ensure_message_columns(&conn)?;
        ensure_compose_columns(&conn)?;
        ensure_alias_columns(&conn)?;
        ensure_agent_action_columns(&conn)?;
        copies::ensure_copies(&conn)?;
        conn.execute_batch(views::SCHEMA)?;
        conn.execute_batch(crate::actions::FOLDER_TOMBSTONE_SCHEMA)?;
        keywords::upgrade_undo_schema(&conn)?;
        bodies::ensure_bodies(&conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_messages_fingerprint
                ON messages(account_id, body_fingerprint);
             -- Per-thread lookups ordered by date (folder lists pick each
             -- thread's first subject and latest sender). Without it SQLite
             -- walks every message in the account once per thread.
             CREATE INDEX IF NOT EXISTS idx_messages_thread_date
                ON messages(account_id, thread_id, date_unix);
             -- Sync sets unread by server copy. Without these, each flag
             -- walked every message in the account, and the flags for a
             -- large mailbox held the write lock for many minutes.
             CREATE INDEX IF NOT EXISTS idx_messages_remote
                ON messages(account_id, remote_id);
             CREATE INDEX IF NOT EXISTS idx_message_aliases_remote
                ON message_aliases(account_id, remote_id);",
        )?;
        ensure_fts(&conn)?;
        crate::search::ensure_vocabulary(&conn)?;
        crate::search::rebuild_vocabulary_if_empty(&conn)?;
        // The fingerprint merge invalidates body vectors, so the table has to
        // exist before that backfill runs.
        crate::embed::ensure_embedding_schema(&conn)?;
        // Summaries first: every write transaction refreshes them on commit,
        // including the one that installs the threading triggers. Then the
        // threading tables, which the version 3 upgrade below already reads.
        summaries::ensure_thread_summaries(&conn)?;
        conn.execute_batch(sent_bcc::SENT_BCC_TABLE)?;
        conn.execute_batch(roles::ROLES_TABLE)?;
        conn.execute_batch(intents::INTENT_SCHEMA)?;
        let outbox_tx =
            rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
        crate::outbox::ensure_schema(&outbox_tx)?;
        outbox_tx.commit()?;
        threads::ensure_thread_schema(&conn)?;
        // Version 3 is stamped only after the fingerprint backfill finishes.
        // Batches commit on their own so a large mailbox is not one transaction.
        let version = schema_version(&conn)?
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        if version < 3 {
            backfill_fingerprints(&conn)?;
            merge_backfilled_duplicates(&conn)?;
            // Replies to a copy deleted by the merge stay on its old thread
            // until this runs. Do it before version 3 is stamped so a crash
            // retries the join and the rethread together.
            rethread_accounts(&conn)?;
            conn.execute(
                "INSERT INTO meta (key, value) VALUES ('schema_version', '3')
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                [],
            )?;
        }
        // Reply-to and subject indexes for incremental threading. The first
        // open fills them and re-resolves every account once.
        threads::ensure_thread_index(&conn)?;
        sent_echo_repair::on_open(&conn)?;
        // 4 queues mail stored before message_bodies for sync to fetch
        // again. It runs after the version 3 merge, so the queue names the
        // rows that survived it, and stamps 4 in the same transaction.
        if version < 4 {
            bodies::queue_stored_mail(&conn)?;
        }
        // 5 queues mail whose attachments were never listed (message_parts):
        // all mail stored before it, fetched once more by the same backfill.
        if version < 5 {
            bodies::queue_unlisted_mail(&conn)?;
        }
        crate::actions::migrate_purge_snapshots(&conn)?;
        keywords::migrate_reserved_labels(&conn)?;
        Ok(Self {
            indexer: None,
            semantic: std::cell::RefCell::new(crate::search::semantic::Semantic::new(
                crate::embed::EmbedderSource::from_env(),
            )),
            conn,
            path: None,
            lock_wait,
            #[cfg(test)]
            full_rethread: std::cell::Cell::new(false),
            #[cfg(test)]
            on_sync_commit: std::cell::RefCell::new(None),
            #[cfg(test)]
            fail_after_flags: std::cell::Cell::new(None),
            #[cfg(unix)]
            _mailbox_lock: None,
        })
    }

    /// Tests: rows, matrix bytes, and key bytes of the search vector matrix.
    #[cfg(test)]
    pub(crate) fn testing_vector_sizes(&self) -> Option<(usize, usize, usize)> {
        self.semantic.borrow_mut().sizes(&self.conn)
    }

    #[cfg(test)]
    pub(crate) fn mailbox_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[cfg(test)]
    pub(crate) fn testing_pause_indexer(&self, paused: bool) {
        if let Some(indexer) = &self.indexer {
            indexer.pause(paused);
        }
    }

    #[cfg(test)]
    pub(crate) fn embedding_indexer_running(&self) -> bool {
        self.indexer
            .as_ref()
            .is_some_and(crate::embed::Indexer::is_running)
    }

    pub(crate) fn seal_private_files(&self) -> Result<()> {
        if let Some(path) = &self.path {
            restrict_store_files(path)?;
        }
        Ok(())
    }

    pub fn upsert_account(&self, account: &Account) -> Result<()> {
        self.conn.execute(
            "INSERT INTO accounts (id, display_name, email, backend_kind)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(id) DO UPDATE SET
                display_name = excluded.display_name,
                email = excluded.email,
                backend_kind = excluded.backend_kind",
            params![
                account.id.as_str(),
                account.display_name,
                account.email,
                account.backend_kind.as_str()
            ],
        )?;
        self.seal_private_files()?;
        Ok(())
    }

    pub fn get_account(&self, id: &AccountId) -> Result<Option<Account>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, display_name, email, backend_kind FROM accounts WHERE id = ?1")?;
        let row: Option<(String, String, String, String)> = stmt
            .query_row(params![id.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .optional()?;
        row.map(|(id, display_name, email, kind)| {
            let backend_kind = BackendKind::parse(&kind)
                .ok_or_else(|| crate::Error::Backend(format!("unknown backend_kind {kind}")))?;
            Ok(Account {
                id: AccountId(id),
                display_name,
                email,
                backend_kind,
            })
        })
        .transpose()
    }

    pub fn sync_cursor(&self, account_id: &AccountId) -> Result<Option<crate::SyncCursor>> {
        sync_cursor_on(&self.conn, account_id)
    }

    /// Read and star changes made in Fell that sync has not seen the server
    /// agree with yet. Sync keeps these values over the server's.
    pub fn pending_flag_changes(&self, account_id: &AccountId) -> Result<Vec<PendingFlags>> {
        intents::pending(&self.conn, account_id)
    }

    /// Write one sync delta.
    ///
    /// The end of a mailbox can carry a flag for every copy and, on its first
    /// listing, every copy's Message-ID. A delta with at least
    /// [`SPLIT_DELTA_AT`] flags and work on copies is two transactions, and
    /// the second begins behind any write that is waiting:
    ///
    /// 1. its new messages and every flag;
    /// 2. its folder roles, any one-time Sent repair, every copy its listings
    ///    found, the copies they no longer list, settling, and the cursor.
    ///
    /// Each part is whole, never cut inside. A write that gets in between
    /// comes after every flag, so no flag the server sent before the pass
    /// lands on top of it. A Sent repair reads the current rows in the second
    /// transaction. Copies change together at the second commit, so an
    /// action reading them in between (an archive queueing the UIDs to move)
    /// never sees half a listing. The cursor is written only in the second
    /// part, so a pass cut short after the first is done again by the next.
    /// Any other delta is one transaction, including deltas that confirm
    /// a send so the Sent copy and confirmed claim become visible together.
    pub fn apply_delta(&self, account_id: &AccountId, delta: &MailboxDelta) -> Result<()> {
        self.apply_sync_delta(account_id, delta).map(|_| ())
    }

    pub(crate) fn apply_sync_delta(
        &self,
        account_id: &AccountId,
        delta: &MailboxDelta,
    ) -> Result<DeltaWrites> {
        let mut tx = self.begin_sync_write()?;
        let mut merge_roles = MergeRoles::new(delta.roles.as_ref());
        let bridge = (!delta.flags.is_empty() || !delta.links.is_empty())
            && views::is_bridge(&tx, account_id)?;
        let views_changed = if let Some(listed) = &delta.views {
            views::replace(&tx, account_id, listed)?
        } else {
            false
        };
        let mut new_messages = 0;
        let keyword_copies: HashSet<_> = if delta.messages.is_empty() {
            HashSet::new()
        } else {
            delta
                .flags
                .iter()
                .filter(|flag| flag.keywords.is_some())
                .map(|flag| &flag.remote_id.0)
                .collect()
        };
        for message in &delta.messages {
            if &message.account_id != account_id {
                return Err(crate::Error::Backend(format!(
                    "delta for account {} includes message for {}",
                    account_id.as_str(),
                    message.account_id.as_str()
                )));
            }
            // Skip a folder archive or delete already took off this message.
            let Some(mut message) = crate::actions::message_for_delta(&tx, message)? else {
                continue;
            };
            views::project_arrival(&tx, &mut message)?;
            // These labels have copy provenance and are applied with FLAGS below.
            if keyword_copies.contains(&message.remote_id.0) {
                message.labels.clear();
            }
            // A combined delta can carry the listing for these messages.
            new_messages += usize::from(insert_message(&tx, &message, &mut merge_roles)?);
        }
        let has_arrivals = !delta.messages.is_empty();
        for message_id in &delta.too_large {
            if let Some(canonical) = canonical_id_on(&tx, account_id, message_id)? {
                bodies::mark_withheld(&tx, account_id, &canonical)?;
            }
        }
        let mut linked = false;
        for link in &delta.links {
            if let Some(copy) = crate::imap::parse_remote_id(&link.flags.remote_id.0)
                && copies::link_arrival(&tx, account_id, &copy, &link.message_id)?
            {
                linked = true;
                apply_flag(&tx, account_id, &link.flags, bridge)?;
            }
        }
        // Apply every flag in one transaction. Local flag intents keep the
        // user's read state separate from updates reported by the server.
        for flag in &delta.flags {
            #[cfg(test)]
            self.stop_among_flags()?;
            apply_flag(&tx, account_id, flag, bridge)?;
        }
        let copy_work = delta.roles.is_some()
            || !delta.removals.is_empty()
            || delta.settle != crate::backend::Settle::Later;
        let confirmed = crate::compose::confirm_synced_sends(&tx, account_id, delta)?;
        let split = copy_work && delta.flags.len() >= SPLIT_DELTA_AT && confirmed.is_empty();
        if split {
            if has_arrivals {
                self.rethread_written(&tx, account_id)?;
            }
            tx.commit()?;
            #[cfg(test)]
            if let Some(hook) = self.on_sync_commit.borrow_mut().as_mut() {
                hook()?;
            }
            tx = self.begin_sync_write()?;
        }
        let repaired = if let Some(found) = &delta.roles {
            roles::replace(&tx, account_id, found)?;
            sent_echo_repair::on_listing(&tx, account_id, found)?
        } else {
            false
        };
        let took_over = copies::apply_removals(&tx, account_id, &delta.removals)?;
        let settled = copies::settle(&tx, account_id, delta.settle)?;
        if (has_arrivals && !split) || repaired || took_over || settled {
            self.rethread_written(&tx, account_id)?;
        }
        if let Some(cursor) = &delta.cursor {
            set_sync_cursor_on(&tx, account_id, cursor)?;
        }
        tx.commit()?;
        self.seal_private_files()?;
        Ok(DeltaWrites {
            new_messages,
            changed: has_arrivals
                || !delta.flags.is_empty()
                || !delta.too_large.is_empty()
                || views_changed
                || linked
                || repaired
                || took_over
                || settled,
            confirmed_drafts: confirmed,
        })
    }

    /// Whether each of `messages` (stored ids, as [`Store::thread_messages`]
    /// returns them) has its body yet, in the same order.
    pub fn body_states(
        &self,
        account_id: &AccountId,
        messages: &[MessageId],
    ) -> Result<Vec<crate::BodyState>> {
        messages
            .iter()
            .map(|id| bodies::state(&self.conn, account_id, id.as_str()))
            .collect()
    }

    /// UIDs from `low` to `high` with a stored copy in one mailbox (named as
    /// remote ids spell it) under `uidvalidity`, ascending. Sync asks so it
    /// never downloads a body the store has.
    pub fn stored_uids(
        &self,
        account_id: &AccountId,
        mailbox: &str,
        uidvalidity: u32,
        range: (u32, u32),
    ) -> Result<Vec<u32>> {
        copies::stored_uids(&self.conn, account_id, mailbox, uidvalidity, range)
    }

    /// Sync begins its batches back to back. A write already waiting for
    /// the lock goes first.
    fn begin_sync_write(&self) -> Result<WriteTx<'_>> {
        self.lock_wait
            .behind_waiting_writes(|| immediate_transaction(&self.conn))
    }

    /// Tests: count down `fail_after_flags` and fail when it runs out.
    #[cfg(test)]
    fn stop_among_flags(&self) -> Result<()> {
        match self.fail_after_flags.get() {
            Some(0) => Err(crate::Error::Backend("sync stopped mid-delta".into())),
            Some(left) => {
                self.fail_after_flags.set(Some(left - 1));
                Ok(())
            }
            None => Ok(()),
        }
    }

    /// Re-resolve the threads this transaction's writes touched.
    fn rethread_written(&self, conn: &Connection, account_id: &AccountId) -> Result<()> {
        #[cfg(test)]
        if self.full_rethread.get() {
            threads::index_queued_subjects(conn)?;
            threads::rethread_all(conn, account_id)?;
            conn.execute_batch("DELETE FROM thread_dirty_rows; DELETE FROM thread_dirty_ids;")?;
            return Ok(());
        }
        threads::rethread_dirty(conn, account_id)
    }

    pub fn upsert_message(&self, message: &Message) -> Result<ThreadId> {
        let tx = immediate_transaction(&self.conn)?;
        insert_message(&tx, message, &mut MergeRoles::new(None))?;
        self.rethread_written(&tx, &message.account_id)?;
        let thread_id =
            thread_id_for_message_id(&tx, &message.account_id, &message.headers.message_id)?
                .ok_or_else(|| {
                    crate::Error::Backend("upserted message missing after rethread".into())
                })?;
        tx.commit()?;
        self.seal_private_files()?;
        Ok(thread_id)
    }

    /// Remove one message, its search postings, and its body embedding.
    ///
    /// Document frequency drops the same way an insert raises it, inside the
    /// transaction that deletes the row. `false` means the message was not stored.
    pub fn delete_message(&self, account_id: &AccountId, message_id: &MessageId) -> Result<bool> {
        let tx = immediate_transaction(&self.conn)?;
        let Some(removed) = delete_stored(&tx, account_id, message_id)? else {
            tx.rollback()?;
            return Ok(false);
        };
        tx.commit()?;
        Ok(removed)
    }

    /// Vector stored for this message body, if the indexer has written one.
    ///
    /// The vector comes from `body_text`, or from the snippet when the body is
    /// null. This read does not compute it and does not call a network. `None`
    /// means this account has no embedding row yet. Keyword search does not
    /// read the table.
    pub fn message_embedding(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
    ) -> Result<Option<Embedding>> {
        crate::embed::load_embedding(&self.conn, account_id.as_str(), message_id.as_str())
    }

    pub fn list_threads(&self, account_id: &AccountId) -> Result<Vec<ThreadSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                thread_id,
                MAX(date_unix) AS last_date,
                COUNT(*) AS n,
                MAX(unread) AS any_unread,
                (SELECT subject FROM messages m2
                 WHERE m2.thread_id = m.thread_id AND m2.account_id = m.account_id
                 ORDER BY date_unix DESC LIMIT 1) AS subject
             FROM messages m
             WHERE account_id = ?1
             GROUP BY thread_id
             ORDER BY last_date DESC",
        )?;
        let rows = stmt.query_map(params![account_id.as_str()], |row| {
            Ok(ThreadSummary {
                thread_id: ThreadId(row.get(0)?),
                last_date_unix: row.get(1)?,
                message_count: row.get::<_, i64>(2)? as usize,
                unread: row.get::<_, i64>(3)? != 0,
                subject: row.get(4)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn thread_messages(
        &self,
        account_id: &AccountId,
        thread_id: &ThreadId,
    ) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SELECT_MESSAGE_COLS}
             FROM messages
             WHERE account_id = ?1 AND thread_id = ?2
             ORDER BY date_unix ASC"
        ))?;
        let rows = stmt.query_map(
            params![account_id.as_str(), thread_id.as_str()],
            row_to_message,
        )?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        drop(stmt);
        sent_bcc::add_recorded_bcc(&self.conn, &mut out)?;
        Ok(out)
    }

    /// True when `token` is already a message id, thread id, reply id, or address.
    /// One stored message by its id, or by an id that collapsed into it.
    pub fn message(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
    ) -> Result<Option<Message>> {
        self.conn
            .prepare_cached(&message_lookup_sql())?
            .query_row(
                params![account_id.as_str(), message_id.as_str()],
                row_to_message,
            )
            .optional()
            .map_err(Into::into)
    }

    /// The HTML and plain parts and bulk headers of one message, by its id
    /// or an id that collapsed into it.
    ///
    /// One row is read and inflated, so call this off the UI thread when a
    /// conversation opens, never for a folder list ([`Store::thread_messages`]
    /// and the folder reads leave [`Message::parts`] empty). `None` means
    /// sync has not stored parts for this message yet (mail synced before they
    /// were kept, or a local write). Parts with neither `html` nor `plain`
    /// mean no server copy had one to give. In both cases show `body_text`.
    /// If there is no row, ask sync with [`crate::sync::fetch_message_body`].
    pub fn message_body(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
    ) -> Result<Option<crate::MessageParts>> {
        let Some(canonical) = self.canonical_id(account_id, message_id)? else {
            return Ok(None);
        };
        bodies::read(&self.conn, account_id, &canonical)
    }

    /// Read a canonical message's HTML, plain text and bulk headers only if
    /// its stored body still has the revision returned by
    /// [`Store::stored_part_kinds`]. Attachment metadata is left out.
    pub fn message_body_at_stamp(
        &self,
        account_id: &AccountId,
        message_id: &str,
        stamp: PartStamp,
    ) -> Result<Option<crate::MessageParts>> {
        bodies::read_at_stamp(&self.conn, account_id, message_id, stamp)
    }

    /// The attachments, `cid:` images and signatures of one message, by its
    /// id or an id that collapsed into it, in document order. Metadata only.
    /// `None` when sync has not listed them yet (mail synced before the list
    /// was kept; [`crate::sync::fetch_message_body`] fetches it).
    pub fn message_attachments(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
    ) -> Result<Option<Vec<crate::MimePart>>> {
        let Some(canonical) = self.canonical_id(account_id, message_id)? else {
            return Ok(None);
        };
        bodies::list(&self.conn, account_id, &canonical)
    }

    /// What [`crate::parts::fetch_part`] needs to fetch one part: the part
    /// and the message's server copies. `None` when the message, the part,
    /// or any server copy is missing.
    pub fn part_request(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
        section: &str,
    ) -> Result<Option<crate::parts::PartRequest>> {
        Ok(self
            .part_requests(account_id, message_id)?
            .into_iter()
            .find(|request| request.part.section == section))
    }

    /// A [`crate::parts::PartRequest`] for every listed part of one message,
    /// in document order. Empty when none are listed or no copy is left.
    pub fn part_requests(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
    ) -> Result<Vec<crate::parts::PartRequest>> {
        let Some(canonical) = self.canonical_id(account_id, message_id)? else {
            return Ok(Vec::new());
        };
        let Some(list) = bodies::list(&self.conn, account_id, &canonical)? else {
            return Ok(Vec::new());
        };
        let copies = bodies::copies_of(&self.conn, account_id, &canonical)?;
        if copies.is_empty() {
            return Ok(Vec::new());
        }
        Ok(list
            .into_iter()
            .map(|part| crate::parts::PartRequest {
                account_id: account_id.clone(),
                message_id: MessageId(canonical.clone()),
                part,
                copies: copies.clone(),
            })
            .collect())
    }

    /// Which parts are stored for each of `message_ids` (canonical ids, as
    /// [`Store::thread_messages`] returns them). An id with no row is left
    /// out: sync has not stored its parts yet. No part is read or inflated,
    /// so a conversation can tell which bodies need preparing before it
    /// reads any of them.
    pub fn stored_part_kinds(
        &self,
        account_id: &AccountId,
        message_ids: &[&str],
    ) -> Result<std::collections::HashMap<String, PartKinds>> {
        bodies::kinds(&self.conn, account_id, message_ids)
    }

    /// Up to `limit` messages stored before parts were kept, newest first,
    /// with the server copies to fetch them from.
    pub fn missing_bodies(&self, account_id: &AccountId, limit: usize) -> Result<Vec<BodyRequest>> {
        bodies::missing(&self.conn, account_id, limit)
    }

    /// The server copies to fetch for one message with no stored body part
    /// or no part list, including one an earlier backfill could not fetch.
    /// `None` when it has both already, or no copy to fetch.
    pub fn body_request(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
    ) -> Result<Option<BodyRequest>> {
        let Some(canonical) = self.canonical_id(account_id, message_id)? else {
            return Ok(None);
        };
        if bodies::read(&self.conn, account_id, &canonical)?
            .is_some_and(|parts| parts.html.is_some() || parts.plain.is_some())
            && bodies::parts_listed(&self.conn, account_id, &canonical)?
        {
            return Ok(None);
        }
        let copies = bodies::copies_of(&self.conn, account_id, &canonical)?;
        if copies.is_empty() {
            return Ok(None);
        }
        Ok(Some(BodyRequest {
            message_id: MessageId(canonical),
            copies,
        }))
    }

    pub(crate) fn missing_reply_to(
        &self,
        account: &AccountId,
        before: Option<i64>,
        limit: u32,
    ) -> Result<Vec<(i64, MessageId)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT m.rowid, m.message_id FROM messages m
             WHERE m.rowid < ?2 AND m.account_id = ?1 AND m.reply_to_line = ''
             AND EXISTS (
                 SELECT 1 FROM message_copies c
                 WHERE c.account_id = m.account_id AND c.message_id = m.message_id
             )
             ORDER BY m.rowid DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![account.as_str(), before.unwrap_or(i64::MAX), limit],
            |row| Ok((row.get(0)?, MessageId(row.get(1)?))),
        )?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub(crate) fn reply_to_copies(
        &self,
        account: &AccountId,
        message: &MessageId,
    ) -> Result<Vec<RemoteId>> {
        bodies::copies_of(&self.conn, account, message.as_str())
    }

    /// Commit only while the header still belongs to this message. An
    /// absence is evidence about the entire copy set, so recheck that set
    /// under the write lock before recording it.
    pub(crate) fn store_verified_reply_to<'a>(
        &self,
        account: &AccountId,
        verified: &[(&MessageId, ReplyToVerification<'a>)],
    ) -> Result<Vec<MessageId>> {
        if verified.is_empty() {
            return Ok(Vec::new());
        }
        let tx = self
            .lock_wait
            .behind_waiting_writes(|| immediate_transaction(&self.conn))?;
        let mut changed = Vec::new();
        {
            let mut present = tx.prepare_cached(
                "UPDATE messages SET reply_to_line = ?3
                 WHERE account_id = ?1 AND message_id = ?2 AND reply_to_line = ''
                 AND EXISTS (
                     SELECT 1 FROM message_copies
                     WHERE account_id = ?1 AND message_id = ?2
                       AND mailbox = ?4 AND uidvalidity = ?5 AND uid = ?6
                 )",
            )?;
            let mut absent = tx.prepare_cached(
                "UPDATE messages SET reply_to_line = ?3
                 WHERE account_id = ?1 AND message_id = ?2 AND reply_to_line = ''",
            )?;
            for (message, verification) in verified {
                let id = message.as_str();
                let writes = match verification {
                    ReplyToVerification::Present(answer) => {
                        let Some(copy) = crate::imap::parse_remote_id(&answer.remote_id.0) else {
                            continue;
                        };
                        present.execute(params![
                            account.as_str(),
                            id,
                            crate::message::encode_reply_to(Some(&answer.addresses)),
                            copy.mailbox,
                            copy.uidvalidity,
                            copy.uid,
                        ])?
                    }
                    ReplyToVerification::Absent(copies) => {
                        if bodies::copies_of(&tx, account, id)? != *copies {
                            continue;
                        }
                        absent.execute(params![
                            account.as_str(),
                            id,
                            crate::message::encode_reply_to(Some(&[])),
                        ])?
                    }
                };
                if writes > 0 {
                    changed.push((*message).clone());
                }
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Store parts fetched again for messages that had none. `None` parts
    /// record that no copy could be fetched, so the backfill stops asking;
    /// a later sync of the message still fills them. Parts already stored
    /// are kept, with unresolved CID roles refreshed for the upgrade queue.
    /// Returns how many messages gained a body.
    pub fn store_fetched_parts(
        &self,
        account_id: &AccountId,
        fetched: &[(MessageId, Option<crate::MessageParts>)],
    ) -> Result<usize> {
        // Deflate before the write lock: archive and mark-read wait on it.
        let compressed: Vec<(&MessageId, bodies::StoredParts)> = fetched
            .iter()
            .map(|(message_id, parts)| {
                let stored = parts
                    .as_ref()
                    .map(bodies::StoredParts::new)
                    .unwrap_or_default();
                (message_id, stored)
            })
            .collect();
        // A CID-only answer can require inspecting the kept HTML again.
        // Snapshot that input before contending for the mailbox write lock.
        let read = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Deferred,
        )?;
        let repairs = compressed
            .iter()
            .map(|(id, parts)| {
                let Some(canonical) = canonical_id_on(&read, account_id, id)? else {
                    return Ok(None);
                };
                bodies::cid_upgrade::prepare_fetched(
                    &read,
                    account_id,
                    canonical,
                    parts.mime_parts(),
                )
            })
            .collect::<Result<Vec<_>>>()?;
        read.commit()?;
        let tx = self
            .lock_wait
            .behind_waiting_writes(|| immediate_transaction(&self.conn))?;
        let mut filled = 0;
        for ((message_id, parts), repair) in compressed.iter().zip(repairs) {
            let Some(canonical) = canonical_id_on(&tx, account_id, message_id)? else {
                continue;
            };
            if bodies::cid_upgrade::apply_fetched(&tx, account_id, &canonical, repair)? {
                continue;
            }
            if parts.has_body() {
                filled += 1;
            }
            let listed = bodies::write(&tx, account_id, &canonical, parts, bodies::Write::FillGap)?;
            if listed && let Some(list) = parts.mime_parts() {
                // The first list replaces flags an older classifier set.
                set_attachments_from_list(&tx, account_id, &canonical, list)?;
            }
        }
        tx.commit()?;
        Ok(filled)
    }

    fn canonical_id(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
    ) -> Result<Option<String>> {
        canonical_id_on(&self.conn, account_id, message_id)
    }

    pub fn id_is_stored(&self, account_id: &AccountId, token: &str) -> Result<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT 1 FROM messages
                 WHERE account_id = ?1
                   AND (message_id = ?2 OR thread_id = ?2
                        OR IFNULL(in_reply_to, '') = ?2 OR from_email = ?2)
                 UNION ALL
                 SELECT 1 FROM message_aliases
                 WHERE account_id = ?1 AND message_id = ?2
                 UNION ALL
                 SELECT 1 FROM review_drafts
                 WHERE account_id = ?1
                   AND (IFNULL(thread_id, '') = ?2 OR IFNULL(in_reply_to, '') = ?2)
                 LIMIT 1",
                params![account_id.as_str(), token],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Search one account in the local store: [`Store::search_with`] with
    /// the default options, so hybrid when a local model is installed.
    pub fn search(&self, account_id: &AccountId, query: &str) -> Result<SearchResults> {
        self.search_with(account_id, query, SearchOptions::default())
    }

    /// Text excerpts for a window of ranked keyword hits. Use the corrected
    /// query when search supplied one. Operator-only hits have no FTS excerpt.
    pub fn search_previews(
        &self,
        account: &AccountId,
        query: &str,
        messages: &[MessageId],
    ) -> Result<std::collections::HashMap<MessageId, crate::SearchPreview>> {
        crate::search::previews(&self.conn, account, query, messages)
    }

    /// Messages and excerpts matching the effective keyword query inside one result.
    pub fn search_thread_matches(
        &self,
        account: &AccountId,
        query: &str,
        thread: &ThreadId,
    ) -> Result<Vec<crate::SearchMatch>> {
        crate::search::thread_matches(&self.conn, account, query, thread)
    }

    /// Folders and flags for ranked rows. An explicit folder filter must
    /// still match a folder on the hit. With no folder filter, prefer the
    /// originating folder, then Inbox or the hit's first folder.
    pub fn search_locations(
        &self,
        account: &AccountId,
        query: &str,
        folder: &str,
        hits: &[SearchHit],
    ) -> Result<std::collections::HashMap<MessageId, crate::SearchLocation>> {
        crate::search::locations(
            &self.conn,
            account,
            query,
            folder,
            &self.folder_roles(account)?,
            hits,
        )
    }

    /// Values for a search operator, drawn from this account's local mail.
    pub fn search_suggestions(
        &self,
        account: &AccountId,
        field: crate::SearchField,
        prefix: &str,
        limit: usize,
    ) -> Result<Vec<String>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        use crate::SearchField::*;
        match field {
            Folder => crate::search::folder_suggestions(&self.conn, account, prefix, limit),
            Label => crate::search::label_suggestions(&self.conn, account, prefix, limit),
            Sender | Recipient => {
                crate::search::address_suggestions(&self.conn, account, field, prefix, limit)
            }
        }
    }

    /// Search one account in the local store.
    ///
    /// This reads SQLite only. It never calls a backend or a network, so the
    /// hits change only after a local write, and it never waits for one. One
    /// hit per thread. Keyword hits are ranked by their best BM25 message.
    /// When the typed query misses and a trigram correction hits,
    /// `did_you_mean` is set and the keyword hits are the corrected results.
    ///
    /// With `options.semantic` and an installed sentence model, the query's
    /// words are also embedded and matched against the stored body vectors,
    /// and the two thread rankings are fused (see [`crate::search`]). The
    /// first such search loads the model and the vectors unless
    /// [`Store::warm_search`] already did. Without a model this is keyword
    /// search and embeds nothing.
    pub fn search_with(
        &self,
        account_id: &AccountId,
        query: &str,
        options: SearchOptions,
    ) -> Result<SearchResults> {
        // One deferred read transaction from the matrix refresh to the last
        // row read: a sync or indexer commit in between cannot pair one
        // state's vectors or operator matches with another's rows. It never
        // asks for the write lock, so in WAL it waits for no writer.
        let snapshot = crate::search::semantic::Snapshot::begin(&self.conn)?;
        let results = if options.semantic {
            let mut semantic = self.semantic.borrow_mut();
            let ready = semantic.ready(&self.conn);
            crate::search::reached(crate::search::Step::Refreshed);
            crate::search::execute(&self.conn, account_id, query, ready)?
        } else {
            crate::search::execute(&self.conn, account_id, query, None)?
        };
        snapshot.commit()?;
        Ok(results)
    }

    /// Start loading the query model and the stored vectors on a background
    /// thread, so the first hybrid search does not wait for them. Does
    /// nothing when no model is installed, for an in-memory store, or when
    /// they are already loading or loaded.
    pub fn warm_search(&self) {
        self.semantic.borrow_mut().warm(self.path.as_deref());
    }

    /// Tests in other crates: `true` once [`Store::warm_search`] was called
    /// on this store, even when it had nothing to load. Only with the
    /// `test-utils` feature.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn testing_search_warm_asked(&self) -> bool {
        self.semantic.borrow().warm_asked
    }

    /// Tests in other crates: fill every missing or outdated body vector
    /// with `embedder` on this thread. Returns the rows written. Only with
    /// the `test-utils` feature, which no release build enables.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn testing_index_embeddings(&self, embedder: &dyn crate::BodyEmbedder) -> Result<usize> {
        let mut total = 0;
        loop {
            let wrote = crate::embed::index_embeddings_with(&self.conn, 256, embedder)?;
            if wrote == 0 {
                return Ok(total);
            }
            total += wrote;
        }
    }

    /// Tests in other crates: rank searches with `embedder`, as if it were
    /// the installed model, over the vectors it stored. An embedder with no
    /// dimensions is an error. Only with the `test-utils` feature.
    #[cfg(any(test, feature = "test-utils"))]
    #[doc(hidden)]
    pub fn testing_search_embedder(
        &self,
        embedder: Box<dyn crate::BodyEmbedder + Send>,
    ) -> Result<()> {
        self.semantic
            .borrow_mut()
            .use_embedder(embedder, &self.conn)
    }

    /// `true` when [`Store::search`] ranks with a local sentence model as
    /// well as the words. Waits for the model to load when it has not yet.
    pub fn semantic_search_active(&self) -> bool {
        self.semantic.borrow_mut().ready(&self.conn).is_some()
    }

    /// Connection for [`crate::actions`]. The UI does not call this.
    pub(crate) fn connection(&self) -> &Connection {
        &self.conn
    }

    /// Up to 20 hits. `cursor` is `None` for the first page, or `next_cursor`.
    pub fn search_page(
        &self,
        account_id: &AccountId,
        query: &str,
        cursor: Option<&str>,
    ) -> Result<SearchPage> {
        if query.trim().is_empty() {
            return Ok(SearchPage {
                hits: Vec::new(),
                next_cursor: None,
            });
        }
        let offset = page_offset(cursor)?;
        let phrase = fts5_phrase(query);
        let mut stmt = self.conn.prepare(
            "SELECT m.message_id, m.thread_id, m.subject, m.snippet, m.from_email, m.date_unix
             FROM messages_fts
             JOIN messages m
               ON m.message_id = messages_fts.message_id
              AND m.account_id = messages_fts.account_id
             WHERE m.account_id = ?1 AND messages_fts MATCH ?2
             ORDER BY rank, m.message_id
             LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(
            params![account_id.as_str(), phrase, SEARCH_PAGE_SIZE + 1, offset],
            search_hit_from_row,
        )?;
        let mut hits = Vec::new();
        for row in rows {
            hits.push(row?);
        }
        let next_cursor = if hits.len() as i64 > SEARCH_PAGE_SIZE {
            hits.truncate(SEARCH_PAGE_SIZE as usize);
            Some((offset + SEARCH_PAGE_SIZE).to_string())
        } else {
            None
        };
        Ok(SearchPage { hits, next_cursor })
    }

    pub fn list_accounts(&self) -> Result<Vec<Account>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, display_name, email, backend_kind FROM accounts ORDER BY id")?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (id, display_name, email, kind) = row?;
            let backend_kind = BackendKind::parse(&kind)
                .ok_or_else(|| crate::Error::Backend(format!("unknown backend_kind {kind}")))?;
            out.push(Account {
                id: AccountId(id),
                display_name,
                email,
                backend_kind,
            });
        }
        Ok(out)
    }

    /// Distinct folder names stored for this account, in first-seen order.
    ///
    /// Reads the per-thread folder index, not every message.
    pub fn folder_names(&self, account_id: &AccountId) -> Result<Vec<String>> {
        summaries::folder_names(&self.conn, account_id)
    }

    /// Which folder is Sent, Trash, Archive and the rest for this account.
    ///
    /// What the last server listing found. An account no server has listed
    /// (the demo mailbox, or mail stored before roles were recorded) gets
    /// [`FolderRoles::from_names`] over its stored folders.
    pub fn folder_roles(&self, account_id: &AccountId) -> Result<FolderRoles> {
        if let Some(found) = listed_folder_roles(&self.conn, account_id)? {
            return Ok(found);
        }
        Ok(FolderRoles::from_names(&self.folder_names(account_id)?)
            .with_bridge_names(views::is_bridge(&self.conn, account_id)?)
            .with_views(views::move_exclusions(&self.conn, account_id)?))
    }

    /// Threads with at least one message in `folder`, newest activity first.
    ///
    /// A message with no folder is Inbox only when it is not archived. Matching
    /// is case-insensitive. `message_count` is the whole thread, including
    /// copies in other folders. Reads the per-thread summaries, so the cost
    /// follows the folder, not the mailbox.
    pub fn folder_threads(
        &self,
        account_id: &AccountId,
        folder: &str,
    ) -> Result<Vec<FolderThread>> {
        summaries::folder_threads(&self.conn, account_id, folder)
    }

    /// The name each address last sent mail under, by lowercase address,
    /// for addresses that have sent this account mail with a name. Reads
    /// the account's latest 20,000 messages at most, newest first.
    pub fn sender_names(
        &self,
        account_id: &AccountId,
        emails: &[String],
    ) -> Result<std::collections::HashMap<String, String>> {
        let wanted: std::collections::HashSet<String> = emails
            .iter()
            .map(|email| email.trim().to_lowercase())
            .filter(|email| !email.is_empty())
            .collect();
        let mut out = std::collections::HashMap::new();
        if wanted.is_empty() {
            return Ok(out);
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT from_email, from_name FROM messages
             WHERE account_id = ?1 AND trim(coalesce(from_name, '')) <> ''
             ORDER BY date_unix DESC LIMIT 20000",
        )?;
        let mut rows = stmt.query(params![account_id.as_str()])?;
        while let Some(row) = rows.next()? {
            let email = row.get::<_, String>(0)?.trim().to_lowercase();
            if wanted.contains(&email) && !out.contains_key(&email) {
                let name: String = row.get(1)?;
                out.insert(email, name.trim().to_string());
                if out.len() == wanted.len() {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Who each thread's latest message in `folder` is addressed to: its To,
    /// then its Cc. The Drafts list names a synced draft by who it is to, as
    /// it does a compose draft. A thread with no message in the folder is
    /// left out.
    pub fn folder_recipients(
        &self,
        account_id: &AccountId,
        folder: &str,
        threads: &[ThreadId],
    ) -> Result<std::collections::HashMap<ThreadId, Vec<Address>>> {
        let names: Vec<String> = folder_query_names(folder)
            .into_iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let mut stmt = self.conn.prepare_cached(
            "SELECT folders, to_line, cc_line FROM messages
             WHERE account_id = ?1 AND thread_id = ?2
             ORDER BY date_unix DESC, rowid DESC",
        )?;
        let mut out = std::collections::HashMap::new();
        for thread in threads {
            let mut rows = stmt.query(params![account_id.as_str(), thread.0])?;
            while let Some(row) = rows.next()? {
                let folders: String = row.get(0)?;
                let here = split_field(&folders)
                    .iter()
                    .any(|name| names.contains(&name.to_ascii_lowercase()));
                if !here {
                    continue;
                }
                let mut people = decode_addresses(&row.get::<_, String>(1)?);
                people.extend(decode_addresses(&row.get::<_, String>(2)?));
                out.insert(thread.clone(), people);
                break;
            }
        }
        Ok(out)
    }

    /// One window of the list [`Store::folder_threads`] returns, whether the
    /// folder has rows beyond it on either side, and the folder's unread
    /// count. A window is found by list position (see [`ListKey`]), so its
    /// cost follows the rows it returns, not the size of the folder.
    pub fn folder_page(
        &self,
        account_id: &AccountId,
        folder: &str,
        span: &FolderSpan,
    ) -> Result<FolderPage> {
        summaries::folder_page(&self.conn, account_id, folder, span)
    }

    /// A window of an exact label's list and the position of its selected thread.
    pub fn label_page(
        &self,
        account: &AccountId,
        label: &str,
        span: &FolderSpan,
        follow: Option<&ThreadId>,
    ) -> Result<(FolderPage, Option<ListKey>)> {
        summaries::label_page(&self.conn, account, label, span, follow)
    }

    /// Up to `limit` threads of a folder or label after `after` (from the
    /// top when `None`) that pass `filter`, newest activity first. Paging by
    /// key, as [`Store::folder_page`] does, so new mail never shifts a page.
    pub fn thread_list(
        &self,
        account_id: &AccountId,
        source: ListSource<'_>,
        filter: &ListFilter,
        after: Option<&ListKey>,
        limit: usize,
    ) -> Result<ThreadList> {
        summaries::thread_list(&self.conn, account_id, source, filter, after, limit)
    }

    /// Where `thread` sits in `folder`'s list, or `None` when it is not in
    /// the folder. The list uses it to follow a selected thread that new
    /// mail moved outside the loaded rows.
    pub fn folder_key_of(
        &self,
        account_id: &AccountId,
        folder: &str,
        thread: &ThreadId,
    ) -> Result<Option<ListKey>> {
        summaries::folder_key_of(&self.conn, account_id, folder, thread)
    }
}

#[cfg(test)]
impl Store {
    /// The scan `folder_names` replaced. Tests compare against it.
    pub(crate) fn folder_names_scan(&self, account_id: &AccountId) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT folders FROM messages WHERE account_id = ?1 ORDER BY rowid")?;
        let rows = stmt.query_map(params![account_id.as_str()], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        for raw in rows {
            for name in split_field(&raw?) {
                let key = name.to_ascii_lowercase();
                if seen.insert(key) {
                    out.push(name);
                }
            }
        }
        Ok(out)
    }

    /// The scan `folder_threads` replaced. Tests compare against it.
    ///
    /// A message with no folder is Inbox only when it is not archived. Matching
    /// is case-insensitive. `message_count` is the whole thread, including
    /// copies in other folders. SQLite returns one row per matching thread.
    pub(crate) fn folder_threads_scan(
        &self,
        account_id: &AccountId,
        folder: &str,
    ) -> Result<Vec<FolderThread>> {
        let names = folder_query_names(folder);
        let inbox = names.iter().any(|name| name.eq_ignore_ascii_case("inbox"));
        let in_folder = folder_predicate("folders", "archived", 2, names.len(), inbox);
        let in_message = folder_predicate("m.folders", "m.archived", 2, names.len(), inbox);
        let sql = format!(
            "SELECT
                m.thread_id,
                (SELECT subject FROM messages s
                  WHERE s.account_id = m.account_id AND s.thread_id = m.thread_id
                  ORDER BY s.date_unix ASC, s.rowid ASC LIMIT 1),
                (SELECT snippet FROM messages s
                  WHERE s.account_id = m.account_id AND s.thread_id = m.thread_id
                  ORDER BY s.date_unix DESC, s.rowid DESC LIMIT 1),
                MAX(m.date_unix),
                COUNT(*),
                MAX(CASE WHEN {in_message} THEN m.unread ELSE 0 END),
                MAX(m.starred),
                MAX(m.has_attachment),
                (SELECT from_email FROM messages s
                  WHERE s.account_id = m.account_id AND s.thread_id = m.thread_id
                  ORDER BY s.date_unix DESC, s.rowid DESC LIMIT 1),
                (SELECT from_name FROM messages s
                  WHERE s.account_id = m.account_id AND s.thread_id = m.thread_id
                  ORDER BY s.date_unix DESC, s.rowid DESC LIMIT 1),
                (SELECT group_concat(part, char(30)) FROM (
                    SELECT ifnull(from_name, '') || char(31) || from_email AS part
                    FROM messages p
                    WHERE p.account_id = m.account_id AND p.thread_id = m.thread_id
                    ORDER BY p.date_unix ASC, p.rowid ASC
                    LIMIT -1
                )),
                (SELECT group_concat(labels, char(30)) FROM (
                    SELECT labels FROM messages p
                    WHERE p.account_id = m.account_id AND p.thread_id = m.thread_id
                      AND labels != ''
                    ORDER BY p.date_unix ASC, p.rowid ASC
                    LIMIT -1
                ))
             FROM messages m
             WHERE m.account_id = ?1
               AND m.thread_id IN (
                   SELECT thread_id FROM messages
                   WHERE account_id = ?1 AND ({in_folder})
               )
             GROUP BY m.thread_id
             ORDER BY MAX(m.date_unix) DESC, m.thread_id ASC"
        );
        let mut bind = Vec::with_capacity(1 + names.len());
        bind.push(account_id.as_str().to_string());
        bind.extend(names);
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(bind.iter()), |row| {
            let participants: String = row.get::<_, Option<String>>(10)?.unwrap_or_default();
            let labels: String = row.get::<_, Option<String>>(11)?.unwrap_or_default();
            Ok(FolderThread {
                thread_id: ThreadId(row.get(0)?),
                subject: row.get(1)?,
                snippet: row.get(2)?,
                last_date_unix: row.get(3)?,
                message_count: row.get::<_, i64>(4)? as usize,
                unread: row.get::<_, i64>(5)? != 0,
                participants: decode_participants(&participants),
                latest_email: row.get(8)?,
                latest_name: row.get(9)?,
                labels: unique_tokens(&labels),
                starred: row.get::<_, i64>(6)? != 0,
                has_attachment: row.get::<_, i64>(7)? != 0,
            })
        })?;
        let mut threads = Vec::new();
        for row in rows {
            threads.push(row?);
        }
        Ok(threads)
    }
}

const SEARCH_PAGE_SIZE: i64 = 20;

fn search_hit_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SearchHit> {
    Ok(SearchHit {
        message_id: MessageId(row.get(0)?),
        thread_id: ThreadId(row.get(1)?),
        subject: row.get(2)?,
        snippet: row.get(3)?,
        sender_email: row.get(4)?,
        date_unix: row.get(5)?,
        semantic_only: false,
    })
}

fn page_offset(cursor: Option<&str>) -> Result<i64> {
    match cursor.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(0),
        Some(raw) => raw
            .parse::<i64>()
            .ok()
            .filter(|offset| *offset >= 0)
            .ok_or_else(|| crate::Error::Invalid("cursor is not a page token".into())),
    }
}

struct ClaimedMailbox {
    path: std::path::PathBuf,
    /// Opened before the exclusive lock is released.
    conn: Connection,
    #[cfg(unix)]
    lock: std::fs::File,
}

/// Pick the one SQLite path for this inode and hold a shared lock on it.
///
/// SQLite names `-wal` and `-shm` from the path string. Once a path is
/// published, every open uses that path and does not replace it. A published
/// path is created only while this process alone holds the exclusive lock.
fn claim_mailbox(requested: &Path) -> Result<ClaimedMailbox> {
    #[cfg(unix)]
    {
        claim_mailbox_unix(requested)
    }
    #[cfg(not(unix))]
    {
        let path = requested.canonicalize()?;
        let conn = open_mailbox_db(&path)?;
        Ok(ClaimedMailbox { path, conn })
    }
}

#[cfg(unix)]
const OPEN_PATH_ATTR: &str = "user.fell.sqlite_path";

#[cfg(unix)]
fn claim_mailbox_unix(requested: &Path) -> Result<ClaimedMailbox> {
    use std::os::unix::fs::MetadataExt;

    use fs4::fs_std::FileExt;

    let canonical = absolute_opened(&requested.canonicalize()?)?;
    loop {
        let db = std::fs::File::open(requested)?;
        let nlink = db.metadata()?.nlink();
        let exclusive = match db.try_lock_exclusive() {
            Ok(held) => held,
            // The filesystem cannot record a flock. A single name cannot split
            // its WAL, so the canonical file still opens.
            Err(_) if nlink <= 1 => {
                let path = read_published_path(&db).unwrap_or_else(|| canonical.clone());
                let conn = open_mailbox_db(&path)?;
                return Ok(ClaimedMailbox {
                    path,
                    conn,
                    lock: db,
                });
            }
            Err(err) => {
                return Err(crate::Error::Invalid(format!("cannot lock mailbox: {err}")));
            }
        };
        if !exclusive {
            // Block until the exclusive holder shares or exits. Replaying a
            // large WAL can take longer than a short poll.
            db.lock_shared()?;
            if let Some(path) = read_published_path(&db) {
                let conn = open_mailbox_db(&path)?;
                if read_published_path(&db).as_ref() == Some(&path) {
                    return Ok(ClaimedMailbox {
                        path,
                        conn,
                        lock: db,
                    });
                }
                drop(conn);
                drop(db);
                continue;
            }
            if nlink <= 1 {
                let conn = open_mailbox_db(&canonical)?;
                return Ok(ClaimedMailbox {
                    path: canonical,
                    conn,
                    lock: db,
                });
            }
            return Err(crate::Error::Invalid(
                "mailbox is already open and its path was not published".into(),
            ));
        }
        delay_publish_for_test();
        let (path, conn) = if let Some(published) = read_published_path(&db) {
            // Someone already chose the path. Do not checkpoint or open
            // another name, and do not replace it.
            let conn = open_mailbox_db(&published)?;
            (published, conn)
        } else {
            let requested_abs = absolute_opened(requested)?;
            if requested_abs == canonical {
                (canonical.clone(), open_mailbox_db(&canonical)?)
            } else {
                // Alone, so this checkpoint is the only connection. Keep the
                // requested path when a live reader makes the checkpoint busy.
                open_requested_or_canonical(&requested_abs, &canonical)?
            }
        };
        // A non-canonical path must be recorded. Otherwise a later open sees
        // no attribute, and a symlink does not raise nlink, so it would guess
        // the canonical file and start a second WAL.
        if publish_mailbox_path(&db, &path).is_err() && path != canonical {
            drop(conn);
            return Err(crate::Error::Invalid(
                "cannot publish the shared mailbox path".into(),
            ));
        }
        // Every live store holds this shared lock, so a later open cannot
        // take the exclusive lock and start a second WAL.
        db.lock_shared()?;
        divert_published_path_for_test(&db);
        if let Some(now) = read_published_path(&db)
            && now != path
        {
            drop(conn);
            drop(db);
            continue;
        }
        return Ok(ClaimedMailbox {
            path,
            conn,
            lock: db,
        });
    }
}

#[cfg(unix)]
fn open_requested_or_canonical(
    requested: &Path,
    canonical: &Path,
) -> Result<(std::path::PathBuf, Connection)> {
    // A non-empty canonical WAL already holds frames. Opening the symlink
    // would checkpoint a different `-wal` and then recover the other one on top.
    if canonical_wal_has_frames(canonical) {
        return Ok((canonical.to_path_buf(), open_mailbox_db(canonical)?));
    }
    // The canonical WAL is absent. Replay the alias WAL before any connection
    // opens the canonical path.
    let conn = Connection::open(requested)?;
    conn.busy_timeout(Duration::ZERO)?;
    let (busy, log, checkpointed): (i64, i64, i64) =
        conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?;
    if busy == 0 && log == checkpointed {
        drop(conn);
        Ok((canonical.to_path_buf(), open_mailbox_db(canonical)?))
    } else {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Ok((requested.to_path_buf(), conn))
    }
}

#[cfg(unix)]
fn canonical_wal_has_frames(canonical: &Path) -> bool {
    let mut wal = canonical.as_os_str().to_os_string();
    wal.push("-wal");
    std::fs::metadata(std::path::Path::new(&wal))
        .map(|meta| meta.len() > 0)
        .unwrap_or(false)
}

fn open_mailbox_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // Wait for other writers from the first statement on, including the
    // journal mode below, not only after `from_connection` sets it.
    conn.busy_timeout(Duration::from_secs(5))?;
    // The UI and `fell-mcp` share this file. WAL lets a reader run beside a writer.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    Ok(conn)
}

#[cfg(unix)]
fn read_published_path(file: &std::fs::File) -> Option<std::path::PathBuf> {
    use std::os::unix::ffi::OsStringExt;
    use xattr::FileExt;

    if reject_mailbox_xattr() {
        return None;
    }
    let bytes = file.get_xattr(OPEN_PATH_ATTR).ok()??;
    let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(bytes));
    same_inode(file, &path).then_some(path)
}

#[cfg(unix)]
fn publish_mailbox_path(file: &std::fs::File, path: &Path) -> Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use xattr::FileExt;

    if reject_mailbox_xattr() {
        return Err(crate::Error::Invalid(
            "cannot publish the shared mailbox path".into(),
        ));
    }
    file.set_xattr(OPEN_PATH_ATTR, path.as_os_str().as_bytes())
        .map_err(|err| {
            crate::Error::Invalid(format!("cannot publish the shared mailbox path: {err}"))
        })?;
    Ok(())
}

#[cfg(unix)]
fn reject_mailbox_xattr() -> bool {
    #[cfg(test)]
    {
        REJECT_MAILBOX_XATTR.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(all(unix, test))]
thread_local! {
    static REJECT_MAILBOX_XATTR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Pretend this filesystem rejects the shared-path attribute.
#[cfg(all(unix, test))]
pub(crate) fn testing_reject_mailbox_xattr(reject: bool) {
    REJECT_MAILBOX_XATTR.with(|cell| cell.set(reject));
}

#[cfg(all(unix, test))]
thread_local! {
    static DIVERT_PUBLISHED_ONCE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// After the next open downgrades its lock, publish this path once.
#[cfg(all(unix, test))]
pub(crate) fn testing_divert_published_path_once(path: std::path::PathBuf) {
    DIVERT_PUBLISHED_ONCE.with(|cell| *cell.borrow_mut() = Some(path));
}

#[cfg(all(unix, test))]
static DELAY_PUBLISH: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The next exclusive open waits before it publishes the shared path.
#[cfg(all(unix, test))]
pub(crate) fn testing_delay_publish_once(delay: bool) {
    DELAY_PUBLISH.store(delay, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(unix)]
fn delay_publish_for_test() {
    #[cfg(test)]
    {
        if DELAY_PUBLISH.swap(false, std::sync::atomic::Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
}

#[cfg(unix)]
fn divert_published_path_for_test(file: &std::fs::File) {
    #[cfg(test)]
    {
        let next = DIVERT_PUBLISHED_ONCE.with(|cell| cell.borrow_mut().take());
        if let Some(path) = next {
            let _ = publish_mailbox_path(file, &path);
        }
    }
    #[cfg(not(test))]
    {
        let _ = file;
    }
}

#[cfg(unix)]
fn same_inode(file: &std::fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;

    let Ok(ours) = file.metadata() else {
        return false;
    };
    let Ok(theirs) = std::fs::metadata(path) else {
        return false;
    };
    ours.dev() == theirs.dev() && ours.ino() == theirs.ino()
}

#[cfg(unix)]
fn absolute_opened(path: &Path) -> Result<std::path::PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn create_private_file(path: &Path) -> Result<()> {
    if path.exists() {
        return restrict_mode(path);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::File::create(path)?;
    }
    Ok(())
}

fn restrict_store_files(path: &Path) -> Result<()> {
    restrict_mode(path)?;
    let mut wal = path.as_os_str().to_os_string();
    wal.push("-wal");
    restrict_mode(Path::new(&wal))?;
    let mut shm = path.as_os_str().to_os_string();
    shm.push("-shm");
    restrict_mode(Path::new(&shm))?;
    Ok(())
}

fn restrict_mode(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    Ok(())
}

/// One person on a thread row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Participant {
    pub name: Option<String>,
    pub email: String,
}

/// A thread as it appears in one folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderThread {
    pub thread_id: ThreadId,
    pub subject: String,
    pub snippet: String,
    pub last_date_unix: i64,
    pub message_count: usize,
    pub unread: bool,
    /// First-seen senders, oldest first.
    pub participants: Vec<Participant>,
    pub latest_email: String,
    pub latest_name: Option<String>,
    pub labels: Vec<String>,
    pub starred: bool,
    pub has_attachment: bool,
}

/// Names that count as the same folder. Unknown names match only themselves.
pub fn folder_aliases(name: &str) -> &'static [&'static str] {
    match name.to_ascii_lowercase().as_str() {
        "inbox" => &["inbox"],
        "sent" => &["sent"],
        "drafts" => &["drafts"],
        "archive" | "archived" => &["archive", "archived"],
        "spam" | "junk" => &["spam", "junk"],
        "trash" | "deleted" => &["trash", "deleted"],
        _ => &[],
    }
}

fn folder_query_names(folder: &str) -> Vec<String> {
    let aliases = folder_aliases(folder);
    if aliases.is_empty() {
        vec![folder.to_string()]
    } else {
        aliases.iter().map(|name| (*name).to_string()).collect()
    }
}

#[cfg(test)]
fn folder_predicate(
    folders: &str,
    archived: &str,
    param_start: usize,
    alias_count: usize,
    inbox: bool,
) -> String {
    let mut parts = Vec::with_capacity(alias_count);
    for index in 0..alias_count {
        let param = param_start + index;
        parts.push(format!(
            "instr(char(30) || lower({folders}) || char(30), char(30) || lower(?{param}) || char(30)) > 0"
        ));
    }
    let joined = parts.join(" OR ");
    if inbox {
        format!("({joined} OR ({archived} = 0 AND {folders} = ''))")
    } else {
        format!("({joined})")
    }
}

fn decode_participants(raw: &str) -> Vec<Participant> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for part in raw.split('\u{1e}') {
        if part.is_empty() {
            continue;
        }
        let (name, email) = part.split_once('\u{1f}').unwrap_or(("", part));
        let key = email.to_ascii_lowercase();
        if email.is_empty() || !seen.insert(key) {
            continue;
        }
        out.push(Participant {
            name: if name.is_empty() {
                None
            } else {
                Some(name.to_string())
            },
            email: email.to_string(),
        });
    }
    out
}

fn unique_tokens(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for token in split_field(raw) {
        let key = token.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(token);
        }
    }
    out
}

fn sync_cursor_on(conn: &Connection, account_id: &AccountId) -> Result<Option<crate::SyncCursor>> {
    let key = format!("cursor:{}", account_id.as_str());
    let token: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    Ok(token.map(|token| crate::SyncCursor {
        account_key: account_id.0.clone(),
        token,
    }))
}

fn set_sync_cursor_on(
    conn: &Connection,
    account_id: &AccountId,
    cursor: &crate::SyncCursor,
) -> Result<()> {
    let key = format!("cursor:{}", account_id.as_str());
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, cursor.token],
    )?;
    Ok(())
}

fn thread_id_for_message_id(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &MessageId,
) -> Result<Option<ThreadId>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT m.thread_id FROM messages m
             WHERE m.account_id = ?1 AND m.message_id = ?2
             UNION ALL
             SELECT m.thread_id FROM message_aliases a
             JOIN messages m
               ON m.account_id = a.account_id AND m.message_id = a.canonical_message_id
             WHERE a.account_id = ?1 AND a.message_id = ?2
             LIMIT 1",
            params![account_id.as_str(), message_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(id.map(ThreadId))
}

/// Two indexed lookups. `message_id = ? OR message_id IN (…)` scans.
const CANONICAL_ID_SQL: &str = "SELECT message_id FROM messages
     WHERE account_id = ?1 AND message_id = ?2
     UNION ALL
     SELECT m.message_id FROM message_aliases a
     JOIN messages m
       ON m.account_id = a.account_id AND m.message_id = a.canonical_message_id
     WHERE a.account_id = ?1 AND a.message_id = ?2
     LIMIT 1";

/// The stored row's Message-ID for `message_id` or an id collapsed into it.
fn canonical_id_on(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &MessageId,
) -> Result<Option<String>> {
    conn.prepare_cached(CANONICAL_ID_SQL)?
        .query_row(params![account_id.as_str(), message_id.as_str()], |row| {
            row.get(0)
        })
        .optional()
        .map_err(Into::into)
}

/// Loaded only when a fingerprint match depends on which folder is Sent.
/// A sync delta shares one lookup of the recorded roles across its arrivals.
struct MergeRoles<'a> {
    listed: Option<Cow<'a, FolderRoles>>,
}

impl<'a> MergeRoles<'a> {
    fn new(listing: Option<&'a FolderRoles>) -> Self {
        Self {
            listed: listing.map(Cow::Borrowed),
        }
    }

    fn resolve<'b>(
        &'b mut self,
        conn: &Connection,
        message: &Message,
        fallback: &'b mut Option<FolderRoles>,
    ) -> Result<&'b FolderRoles> {
        if self.listed.is_none() {
            self.listed = Some(Cow::Owned(
                roles::listed(conn, &message.account_id)?.unwrap_or_default(),
            ));
        }
        if let Some(found) = self.listed.as_deref().filter(|roles| !roles.is_empty()) {
            return Ok(found);
        }
        match fallback {
            Some(found) => Ok(found),
            None => {
                let mut names = summaries::folder_names(conn, &message.account_id)?;
                // The incoming copy may be the first message seen in this folder.
                names.extend_from_slice(&message.folders);
                Ok(fallback.insert(FolderRoles::from_names(&names)))
            }
        }
    }
}

pub(crate) struct DeltaWrites {
    pub new_messages: usize,
    /// The caller should refresh its view of the stored mail.
    pub changed: bool,
    pub confirmed_drafts: Vec<String>,
}

/// Returns whether the arrival made a new row. Every accepted arrival still
/// records its remote copy, including arrivals folded into a stored row.
fn insert_message(
    conn: &Connection,
    message: &Message,
    roles: &mut MergeRoles<'_>,
) -> Result<bool> {
    let inserted = insert_arrival(conn, message, roles)?;
    copies::record(conn, message)?;
    Ok(inserted)
}

fn insert_arrival(
    conn: &Connection,
    message: &Message,
    roles: &mut MergeRoles<'_>,
) -> Result<bool> {
    let fingerprint = message_fingerprint(message);
    if let Some((stored, via_alias)) =
        find_stored(conn, &message.account_id, &message.headers.message_id)?
    {
        if !via_alias
            && !alias_points_at(conn, &message.account_id, &stored.canonical_id)?
            && let Some(other) = find_list_collapse(
                conn,
                message,
                &fingerprint,
                Some(&stored.canonical_id),
                false,
                roles,
            )?
        {
            absorb_duplicate(conn, message, &stored, &other)?;
        } else {
            write_kept(conn, message, &stored, via_alias, &[], None)?;
        }
        return Ok(false);
    }
    if let Some(stored) = find_list_collapse(conn, message, &fingerprint, None, false, roles)? {
        remember_alias(conn, message, &stored.canonical_id)?;
        write_kept(conn, message, &stored, true, &[], None)?;
        return Ok(false);
    }
    insert_new(conn, message, &fingerprint)?;
    Ok(true)
}

fn insert_new(conn: &Connection, message: &Message, fingerprint: &str) -> Result<ThreadId> {
    let account_id = &message.account_id;
    let thread_id = resolve_thread_id(&message.headers, |parent| {
        thread_id_for_message_id(conn, account_id, parent)
    })?;
    let references = references_header(&message.headers);
    let mut folders = union_tokens("", &message.folders, false);
    if message.archived {
        folders = strip_inbox_tokens(&folders);
    }
    let labels = union_tokens("", &message.labels, false);
    let to_line = encode_addresses(&message.to);
    let cc_line = encode_addresses(&message.cc);
    let bcc_line = encode_addresses(&message.bcc);
    let list_id = clean_list_id(&message.list_id);
    // A new row stores this copy's part list, so the marker and names
    // come from it.
    let (has_attachment, attachment_names) = match &message.parts {
        Some(parts) => (
            crate::imap::has_attachment(&parts.mime_parts),
            union_tokens("", &crate::imap::attachment_names(&parts.mime_parts), false),
        ),
        None => (
            message.has_attachment,
            union_tokens("", &message.attachment_names, false),
        ),
    };
    conn.execute(
        "INSERT INTO messages (
            account_id, message_id, remote_id, thread_id, proton_conversation_id,
            in_reply_to, references_header, from_name, from_email, subject,
            date_unix, snippet, body_text, unread, folders, labels, arrived_via,
            to_line, cc_line, bcc_line, starred, has_attachment, archived, snooze_until,
            body_fingerprint, list_id, reply_to_line, attachment_names, part_cache_stamp
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
            ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, randomblob(16)
         )",
        params![
            message.account_id.as_str(),
            message.headers.message_id.as_str(),
            message.remote_id.0,
            thread_id.as_str(),
            message.headers.proton_conversation_id,
            message.headers.in_reply_to.as_ref().map(MessageId::as_str),
            references,
            message.headers.from.name,
            message.headers.from.email,
            message.headers.subject,
            message.headers.date_unix,
            message.snippet,
            message.body_text,
            i64::from(message.unread),
            folders,
            labels,
            union_tokens("", &message.arrived_via, true),
            to_line,
            cc_line,
            bcc_line,
            i64::from(message.starred),
            i64::from(has_attachment),
            i64::from(message.archived),
            message.snooze_until,
            fingerprint,
            list_id,
            crate::message::encode_reply_to(message.reply_to.as_deref()),
            attachment_names,
        ],
    )?;
    if let Some(parts) = &message.parts {
        bodies::write(
            conn,
            account_id,
            message.headers.message_id.as_str(),
            &bodies::StoredParts::new(parts),
            bodies::Write::Replace,
        )?;
    }
    let body = message.body_text.as_deref().unwrap_or(&message.snippet);
    let terms = terms_for_indexed(IndexedText {
        subject: &message.headers.subject,
        from_name: message.headers.from.name.as_deref(),
        from_email: &message.headers.from.email,
        body,
        to_line: &to_line,
        cc_line: &cc_line,
        bcc_line: &bcc_line,
        folders: &folders,
        labels: &labels,
    });
    sync_terms(conn, message.account_id.as_str(), &HashSet::new(), &terms)?;
    enqueue_message(
        conn,
        message.account_id.as_str(),
        message.headers.message_id.as_str(),
    )?;
    Ok(thread_id)
}

/// Write `message` onto `stored`.
///
/// `alias` is a list-echo collapse, or a later sync of an id that already
/// collapsed. That path fills gaps only: a stored body, parent, subject,
/// proton id, folder, label, address, star, archive bit, or snooze is never
/// replaced. A same-id resync
/// (`alias == false`, and not a list resync of an already merged row) may
/// replace a non-empty subject, display name, and Proton conversation id, and
/// a non-empty body. It still keeps a stored body, parent, or Proton id when
/// the incoming copy has none.
fn write_kept(
    conn: &Connection,
    message: &Message,
    stored: &Arrival,
    alias: bool,
    extra_folders: &[String],
    merged_labels: Option<String>,
) -> Result<ThreadId> {
    let stored_list = list_is_set(&stored.list_id);
    let incoming_list = clean_list_id(&message.list_id).is_some();
    // The list copy can be the canonical id. A later sync of that id must not
    // undo the Sent copy's parent or body. A non-list same-id update still
    // replaces a non-empty parent.
    let list_resync_of_merged = !alias
        && incoming_list
        && alias_points_at(conn, &message.account_id, &stored.canonical_id)?;
    let like_alias = alias || list_resync_of_merged;
    let (body_text, snippet) = if like_alias {
        filled_body_keep_stored(stored, message)
    } else {
        filled_body_canonical(stored, message)
    };
    let fingerprint = fingerprint_of(body_text.as_deref());
    let (in_reply_to, references) = parents_for_write(stored, message, like_alias);
    let subject = merged_subject(
        &stored.subject,
        stored_list,
        &message.headers.subject,
        incoming_list,
        !like_alias,
    );
    let proton = merged_proton(
        &stored.proton_conversation_id,
        &message.headers.proton_conversation_id,
        like_alias,
    );
    let mut headers = message.headers.clone();
    // Fall back to the row we are keeping. An incoming list id with no parents
    // must not rename that row's thread to the rewritten Message-ID.
    headers.message_id = MessageId(stored.canonical_id.clone());
    headers.subject.clone_from(&subject);
    headers.proton_conversation_id.clone_from(&proton);
    headers.in_reply_to = in_reply_to
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(MessageId::new);
    headers.references = parse_references(&references);
    let thread_id = resolve_thread_id(&headers, |parent| {
        thread_id_for_message_id(conn, &message.account_id, parent)
    })?;
    // Duplicate copies union arrival folders and labels. Archive and snooze are
    // local triage, so a later copy must not clear them, and INBOX stays off
    // while the message is archived.
    let archived = stored.archived || message.archived;
    let mut folder_in = message.folders.clone();
    folder_in.extend(extra_folders.iter().cloned());
    // The deleted copy's folders and labels enter the new terms here. The
    // earlier terms belong to the keeper alone.
    let mut folders = union_tokens(&stored.folders, &folder_in, false);
    if archived {
        folders = strip_inbox_tokens(&folders);
    }
    let labels =
        merged_labels.unwrap_or_else(|| union_tokens(&stored.labels, &message.labels, false));
    let arrived_via = union_tokens(&stored.arrived_via, &message.arrived_via, true);
    let to_line = union_addresses(&stored.to_line, &message.to);
    let cc_line = union_addresses(&stored.cc_line, &message.cc);
    let bcc_line = union_addresses(&stored.bcc_line, &message.bcc);
    // The attachment marker and names describe the row's part list. A
    // write that sets the list sets them from it, which also replaces flags
    // an older classifier gave a row never listed (a signature counted as a
    // file). A copy whose list was not taken changes neither, so no name
    // appears that `message_parts` does not hold. Only a row with no list
    // at all unions them, as before lists were kept.
    let listed_before = bodies::parts_listed(conn, &message.account_id, &stored.canonical_id)?;
    let new_list = match &message.parts {
        Some(parts) => {
            // Parts follow the body: a collapsed copy only fills a gap.
            let mode = if like_alias {
                bodies::Write::FillGap
            } else {
                bodies::Write::Replace
            };
            let parts = bodies::StoredParts::new(parts);
            let wrote = bodies::write(
                conn,
                &message.account_id,
                &stored.canonical_id,
                &parts,
                mode,
            )?;
            wrote.then(|| parts.mime_parts().map(<[crate::MimePart]>::to_vec))
        }
        None => None,
    }
    .flatten();
    let (has_attachment, attachment_names) = match &new_list {
        Some(list) => (
            crate::imap::has_attachment(list),
            union_tokens("", &crate::imap::attachment_names(list), false),
        ),
        None if listed_before => (stored.has_attachment, stored.attachment_names.clone()),
        None => (
            stored.has_attachment || message.has_attachment,
            union_tokens(&stored.attachment_names, &message.attachment_names, false),
        ),
    };
    // An empty arrival only answers for this copy. A legacy row can
    // still have another copy with a Reply-To, so let the backfill check it.
    let addresses = message.reply_to.as_deref().filter(|list| !list.is_empty());
    let reply_to_line = crate::message::merge_reply_to(
        &stored.reply_to_line,
        crate::message::encode_reply_to(addresses),
    );
    let list_id = merged_list_id(&stored.list_id, &message.list_id);
    // Read and star changes made in Fell stay until the server agrees.
    let unread = intents::server_unread(
        conn,
        &message.account_id,
        &stored.canonical_id,
        &message.remote_id.0,
        message.unread,
    )?;
    // A pending local star wins while arrivals are merged.
    let starred = intents::local_star(conn, &message.account_id, &stored.canonical_id)?
        .unwrap_or(stored.starred || message.starred);
    let from_email = if alias {
        prefer_nonempty(&stored.from_email, &message.headers.from.email)
    } else {
        prefer_nonempty(&message.headers.from.email, &stored.from_email)
    };
    let from_name = merged_display_name(
        &stored.from_name,
        stored_list,
        &message.headers.from.name,
        incoming_list,
        !like_alias,
    );
    let date_unix = if alias || (incoming_list && stored.date_unix != 0) {
        if stored.date_unix != 0 {
            stored.date_unix
        } else {
            message.headers.date_unix
        }
    } else {
        message.headers.date_unix
    };
    // The message row keeps the first copy's server id. The other copy's id
    // and folder live on the alias, so neither is discarded. A kept row with
    // no server id still records that alias, so a later sync or reply finds
    // the thread.
    if alias {
        remember_alias(conn, message, &stored.canonical_id)?;
    }
    let remote_id = if alias && !stored.remote_id.is_empty() {
        stored.remote_id.clone()
    } else {
        message.remote_id.0.clone()
    };
    let old_terms = terms_for_indexed(IndexedText {
        subject: &stored.subject,
        from_name: stored.from_name.as_deref(),
        from_email: &stored.from_email,
        body: &stored.body,
        to_line: &stored.to_line,
        cc_line: &stored.cc_line,
        bcc_line: &stored.bcc_line,
        folders: &stored.folders,
        labels: &stored.labels,
    });
    let indexed = body_text.as_deref().unwrap_or(&snippet);
    let new_terms = terms_for_indexed(IndexedText {
        subject: &subject,
        from_name: from_name.as_deref(),
        from_email: &from_email,
        body: indexed,
        to_line: &to_line,
        cc_line: &cc_line,
        bcc_line: &bcc_line,
        folders: &folders,
        labels: &labels,
    });
    conn.execute(
        "UPDATE messages SET
            remote_id = ?1,
            thread_id = ?2,
            proton_conversation_id = ?3,
            in_reply_to = ?4,
            references_header = ?5,
            from_name = ?6,
            from_email = ?7,
            subject = ?8,
            date_unix = ?9,
            snippet = ?10,
            body_text = ?11,
            unread = ?12,
            folders = ?13,
            labels = ?14,
            arrived_via = ?15,
            to_line = ?16,
            cc_line = ?17,
            bcc_line = ?18,
            starred = ?19,
            has_attachment = ?20,
            archived = ?21,
            snooze_until = ?22,
            body_fingerprint = ?23,
            list_id = ?24,
            reply_to_line = ?25,
            attachment_names = ?26
         WHERE account_id = ?27 AND message_id = ?28",
        params![
            remote_id,
            thread_id.as_str(),
            proton,
            in_reply_to,
            references,
            from_name,
            from_email,
            subject,
            date_unix,
            snippet,
            body_text,
            i64::from(unread),
            folders,
            labels,
            arrived_via,
            to_line,
            cc_line,
            bcc_line,
            i64::from(starred),
            i64::from(has_attachment),
            i64::from(stored.archived || message.archived),
            kept_snooze(stored.snooze_until, message.snooze_until),
            fingerprint,
            list_id,
            reply_to_line,
            attachment_names,
            message.account_id.as_str(),
            stored.canonical_id,
        ],
    )?;
    sync_terms(conn, message.account_id.as_str(), &old_terms, &new_terms)?;
    // The vector belongs to the row that stays, not to the id this copy
    // arrived under (which may be an alias).
    invalidate_message_embedding(
        conn,
        message.account_id.as_str(),
        &stored.canonical_id,
        indexed,
    )?;
    Ok(thread_id)
}

#[derive(Clone)]
struct Arrival {
    canonical_id: String,
    remote_id: String,
    proton_conversation_id: Option<String>,
    in_reply_to: Option<String>,
    references_header: String,
    from_name: Option<String>,
    from_email: String,
    subject: String,
    date_unix: i64,
    snippet: String,
    body_text: Option<String>,
    body: String,
    folders: String,
    labels: String,
    arrived_via: String,
    to_line: String,
    cc_line: String,
    bcc_line: String,
    starred: bool,
    has_attachment: bool,
    archived: bool,
    snooze_until: Option<i64>,
    list_id: Option<String>,
    reply_to_line: String,
    attachment_names: String,
}

const ARRIVAL_COLS: &str = "m.message_id, m.remote_id, m.proton_conversation_id, m.in_reply_to,
    m.references_header, m.from_name, m.from_email, m.subject, m.date_unix, m.snippet,
    m.body_text, m.folders, m.labels, m.arrived_via, m.to_line, m.cc_line, m.bcc_line,
    m.has_attachment, m.archived, m.starred, m.list_id, m.snooze_until, m.reply_to_line,
    m.attachment_names";

fn map_arrival(row: &rusqlite::Row<'_>) -> rusqlite::Result<Arrival> {
    let snippet: String = row.get(9)?;
    let body_text: Option<String> = row.get(10)?;
    let body = body_text
        .clone()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| snippet.clone());
    Ok(Arrival {
        canonical_id: row.get(0)?,
        remote_id: row.get(1)?,
        proton_conversation_id: row.get(2)?,
        in_reply_to: row.get(3)?,
        references_header: row.get(4)?,
        from_name: row.get(5)?,
        from_email: row.get(6)?,
        subject: row.get(7)?,
        date_unix: row.get(8)?,
        snippet,
        body_text,
        body,
        folders: row.get(11)?,
        labels: row.get(12)?,
        arrived_via: row.get(13)?,
        to_line: row.get(14)?,
        cc_line: row.get(15)?,
        bcc_line: row.get(16)?,
        has_attachment: row.get::<_, i64>(17)? != 0,
        archived: row.get::<_, i64>(18)? != 0,
        starred: row.get::<_, i64>(19)? != 0,
        list_id: row.get(20)?,
        snooze_until: row.get(21)?,
        reply_to_line: row.get(22)?,
        attachment_names: row.get(23)?,
    })
}

/// Delete a stored message with its search postings, body vector, queue
/// entry, aliases, and server copies. `message_id` may be an alias of the row.
/// `None` means nothing was stored under it.
fn delete_stored(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &MessageId,
) -> Result<Option<bool>> {
    let Some((stored, _)) = find_stored(conn, account_id, message_id)? else {
        return Ok(None);
    };
    let old_terms = terms_for_indexed(IndexedText {
        subject: &stored.subject,
        from_name: stored.from_name.as_deref(),
        from_email: &stored.from_email,
        body: &stored.body,
        to_line: &stored.to_line,
        cc_line: &stored.cc_line,
        bcc_line: &stored.bcc_line,
        folders: &stored.folders,
        labels: &stored.labels,
    });
    sync_terms(conn, account_id.as_str(), &old_terms, &HashSet::new())?;
    for id in [stored.canonical_id.as_str(), message_id.as_str()] {
        remove_message_embedding(conn, account_id.as_str(), id)?;
        dequeue_message(conn, account_id.as_str(), id)?;
    }
    let removed = conn.execute(
        "DELETE FROM messages WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), stored.canonical_id],
    )?;
    conn.execute(
        "DELETE FROM message_aliases
         WHERE account_id = ?1
           AND (message_id = ?2 OR canonical_message_id = ?2 OR message_id = ?3)",
        params![
            account_id.as_str(),
            stored.canonical_id,
            message_id.as_str()
        ],
    )?;
    copies::forget(conn, account_id, &stored.canonical_id)?;
    sent_bcc::forget_if_gone(conn, account_id, &stored.canonical_id)?;
    sent_bcc::forget_if_gone(conn, account_id, message_id.as_str())?;
    Ok(Some(removed > 0))
}

fn find_stored(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &MessageId,
) -> Result<Option<(Arrival, bool)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ARRIVAL_COLS}, 0
         FROM messages m
         WHERE m.account_id = ?1 AND m.message_id = ?2
         UNION ALL
         SELECT {ARRIVAL_COLS}, 1
         FROM message_aliases a
         JOIN messages m
           ON m.account_id = a.account_id AND m.message_id = a.canonical_message_id
         WHERE a.account_id = ?1 AND a.message_id = ?2
         LIMIT 1"
    ))?;
    let hit = stmt
        .query_row(params![account_id.as_str(), message_id.as_str()], |row| {
            let stored = map_arrival(row)?;
            let via_alias: i64 = row.get(24)?;
            Ok((stored, via_alias != 0))
        })
        .optional()?;
    Ok(hit)
}

/// Six hours. A list echo of one send, not next week's copy of the same text.
const LIST_ECHO_WINDOW_SECS: u64 = 6 * 60 * 60;

/// Same sender, exact body fingerprint, and a delivery this close.
///
/// One side must be a list rewrite, and Proton conversation ids must be absent
/// or equal. A short reply joins only when it is the same post. A longer list
/// copy joins a Sent copy only when the subject key matches; a different
/// parent is still that post. Any other pair must share the subject key and
/// parent. More than one candidate stays as separate rows.
fn find_list_collapse(
    conn: &Connection,
    message: &Message,
    fingerprint: &str,
    exclude: Option<&str>,
    unlabeled_ok: bool,
    role_lookup: &mut MergeRoles<'_>,
) -> Result<Option<Arrival>> {
    if fingerprint.is_empty() {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {ARRIVAL_COLS}
         FROM messages m
         WHERE m.account_id = ?1 AND m.body_fingerprint = ?2 AND m.body_fingerprint != ''"
    ))?;
    let rows = stmt.query_map(
        params![message.account_id.as_str(), fingerprint],
        map_arrival,
    )?;
    let incoming_list = list_is_set(&message.list_id);
    let mut incoming_keys = None;
    let mut fallback_roles = None;
    let mut found = None;
    for row in rows {
        let stored = row?;
        let stored_list = list_is_set(&stored.list_id);
        if exclude == Some(stored.canonical_id.as_str())
            || (!incoming_list && !stored_list && !unlabeled_ok)
            || !same_sender(&message.headers.from.email, &stored.from_email)
            || !dates_within_echo_window(stored.date_unix, message.headers.date_unix)
            || !proton_compatible(
                &stored.proton_conversation_id,
                &message.headers.proton_conversation_id,
            )
        {
            continue;
        }
        let (subject, short) = &*incoming_keys.get_or_insert_with(|| {
            (
                subject_key(&message.headers.subject),
                message
                    .body_text
                    .as_deref()
                    .is_some_and(is_short_reply_body),
            )
        });
        if *subject != subject_key(&stored.subject) {
            continue;
        }
        let short = *short;
        let same_parent = parents_match(message, &stored);
        let either_list = incoming_list || stored_list;
        let both_list = incoming_list && stored_list;
        if short && (both_list || !same_parent) {
            continue;
        }
        // A long list echo with the same parent already matches. The Sent
        // role only matters for the remaining plausible Sent/list pairs.
        let matches = if !short && either_list && same_parent {
            true
        } else if !both_list
            && (either_list
                || subject_has_list_prefix(&message.headers.subject)
                || subject_has_list_prefix(&stored.subject))
        {
            let roles = role_lookup.resolve(conn, message, &mut fallback_roles)?;
            sent_list_echo(message, &stored, incoming_list, stored_list, short, roles)
        } else {
            false
        };
        if matches {
            if found.is_some() {
                return Ok(None);
            }
            found = Some(stored);
        }
    }
    let Some(only) = found else {
        return Ok(None);
    };
    // A row that already absorbed another Message-ID is not a unique target.
    // A later message with the same body stays its own row.
    if alias_points_at(conn, &message.account_id, &only.canonical_id)? {
        return Ok(None);
    }
    Ok(Some(only))
}

/// Set a row's attachment marker and names from the part list just stored
/// for it, so they never disagree with `message_parts`.
fn set_attachments_from_list(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    list: &[crate::MimePart],
) -> Result<()> {
    conn.execute(
        "UPDATE messages SET has_attachment = ?1, attachment_names = ?2
         WHERE account_id = ?3 AND message_id = ?4",
        params![
            i64::from(crate::imap::has_attachment(list)),
            union_tokens("", &crate::imap::attachment_names(list), false),
            account_id.as_str(),
            message_id,
        ],
    )?;
    Ok(())
}

/// Fold `message`'s existing row into `keeper`, then drop the duplicate row.
///
/// Local triage lives on the stored row. The sync payload only carries the
/// folder being fetched and `starred: false`, so the doomed row is read here.
fn absorb_duplicate(
    conn: &Connection,
    message: &Message,
    doomed: &Arrival,
    keeper: &Arrival,
) -> Result<ThreadId> {
    let extra_folders = split_field(&doomed.folders);
    let labels = keywords::merge(conn, &message.account_id, doomed, keeper, &message.labels)?;
    let keeper = fold_doomed_local_state(keeper, doomed);
    let thread = write_kept(conn, message, &keeper, true, &extra_folders, Some(labels))?;
    // The stored parts of the doomed row fill what the keeper lacks; the
    // delete below drops the rest with the row.
    if let Some(list) = bodies::move_into(
        conn,
        &message.account_id,
        &doomed.canonical_id,
        &keeper.canonical_id,
    )? {
        set_attachments_from_list(conn, &message.account_id, &keeper.canonical_id, &list)?;
    }
    let old_terms = terms_for_indexed(IndexedText {
        subject: &doomed.subject,
        from_name: doomed.from_name.as_deref(),
        from_email: &doomed.from_email,
        body: &doomed.body,
        to_line: &doomed.to_line,
        cc_line: &doomed.cc_line,
        bcc_line: &doomed.bcc_line,
        folders: &doomed.folders,
        labels: &doomed.labels,
    });
    sync_terms(
        conn,
        message.account_id.as_str(),
        &old_terms,
        &HashSet::new(),
    )?;
    conn.execute(
        "DELETE FROM messages WHERE account_id = ?1 AND message_id = ?2",
        params![message.account_id.as_str(), doomed.canonical_id],
    )?;
    // The removed copy's vector and queue entry go with its row.
    remove_message_embedding(conn, message.account_id.as_str(), &doomed.canonical_id)?;
    dequeue_message(conn, message.account_id.as_str(), &doomed.canonical_id)?;
    conn.execute(
        "UPDATE message_aliases SET canonical_message_id = ?1
         WHERE account_id = ?2 AND canonical_message_id = ?3",
        params![
            keeper.canonical_id,
            message.account_id.as_str(),
            doomed.canonical_id,
        ],
    )?;
    copies::repoint(
        conn,
        &message.account_id,
        &doomed.canonical_id,
        &keeper.canonical_id,
    )?;
    Ok(thread)
}

/// Join fingerprint groups that occur more than once.
///
/// Each batch loads only those groups' rows, then commits. A mailbox is not
/// held in one `Vec` of bodies, and schema version 3 is stamped only after
/// this returns.
fn merge_backfilled_duplicates(conn: &Connection) -> Result<()> {
    let mut after = 0i64;
    loop {
        // Groups and rows are read under the write lock: a mark-read or a
        // folder change committed while this waited is what gets merged.
        let tx = begin_immediate(conn)?;
        let groups = duplicate_fingerprint_groups(&tx, after)?;
        if groups.is_empty() {
            tx.commit()?;
            break;
        }
        let mut batch = Vec::new();
        for group in &groups {
            after = group.min_rowid;
            let mut stmt = tx.prepare(&format!(
                "SELECT {SELECT_MESSAGE_COLS} FROM messages
                 WHERE account_id = ?1 AND body_fingerprint = ?2
                 ORDER BY rowid"
            ))?;
            let rows =
                stmt.query_map(params![group.account_id, group.fingerprint], row_to_message)?;
            for row in rows {
                batch.push(row?);
            }
            drop(stmt);
        }
        for message in &batch {
            let fingerprint = message_fingerprint(message);
            let Some((stored, via_alias)) =
                find_stored(&tx, &message.account_id, &message.headers.message_id)?
            else {
                continue;
            };
            if via_alias || alias_points_at(&tx, &message.account_id, &stored.canonical_id)? {
                continue;
            }
            let Some(other) = find_list_collapse(
                &tx,
                message,
                &fingerprint,
                Some(&stored.canonical_id),
                true,
                &mut MergeRoles::new(None),
            )?
            else {
                continue;
            };
            absorb_duplicate(&tx, message, &stored, &other)?;
        }
        tx.commit()?;
    }
    Ok(())
}

struct FingerprintGroup {
    account_id: String,
    fingerprint: String,
    min_rowid: i64,
}

fn duplicate_fingerprint_groups(conn: &Connection, after: i64) -> Result<Vec<FingerprintGroup>> {
    let mut stmt = conn.prepare(
        "SELECT account_id, body_fingerprint, MIN(rowid)
         FROM messages
         WHERE body_fingerprint != ''
         GROUP BY account_id, body_fingerprint
         HAVING COUNT(*) > 1 AND MIN(rowid) > ?1
         ORDER BY MIN(rowid)
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![after, FINGERPRINT_BATCH as i64], |row| {
        Ok(FingerprintGroup {
            account_id: row.get(0)?,
            fingerprint: row.get(1)?,
            min_rowid: row.get(2)?,
        })
    })?;
    let mut groups = Vec::new();
    for row in rows {
        groups.push(row?);
    }
    Ok(groups)
}

/// Keep a snooze that is set on either copy. A missing incoming snooze does
/// not clear one already stored.
fn kept_snooze(stored: Option<i64>, incoming: Option<i64>) -> Option<i64> {
    match (stored, incoming) {
        (Some(stored), Some(incoming)) => Some(stored.max(incoming)),
        (Some(stored), None) => Some(stored),
        (None, Some(incoming)) => Some(incoming),
        (None, None) => None,
    }
}

/// Copy the deleted row's star, archive bit, and snooze onto the row that stays.
///
/// Folders and labels are not copied here. `write_kept` unions those into the
/// stored value only, so the previous search terms stay the keeper's own.
fn fold_doomed_local_state(keeper: &Arrival, doomed: &Arrival) -> Arrival {
    let mut keeper = keeper.clone();
    keeper.starred |= doomed.starred;
    keeper.archived |= doomed.archived;
    keeper.snooze_until = kept_snooze(keeper.snooze_until, doomed.snooze_until);
    keeper
}

/// Give `into` the labels, star, archive bit, and snooze of `from`, a row
/// for the same mail that is about to go.
fn take_local_state(
    conn: &Connection,
    account_id: &AccountId,
    from: &Arrival,
    into: &Arrival,
) -> Result<()> {
    let mut labels = split_field(&into.labels);
    for label in keywords::local_labels(conn, account_id, from)? {
        keywords::keep_local(conn, account_id, &into.canonical_id, &mut labels, &label)?;
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    let labels = join_tokens(&labels);
    let kept = fold_doomed_local_state(into, from);
    let indexed = |labels: &str| {
        terms_for_indexed(IndexedText {
            subject: &into.subject,
            from_name: into.from_name.as_deref(),
            from_email: &into.from_email,
            body: &into.body,
            to_line: &into.to_line,
            cc_line: &into.cc_line,
            bcc_line: &into.bcc_line,
            folders: &into.folders,
            labels,
        })
    };
    let old_terms = indexed(&into.labels);
    let new_terms = indexed(&labels);
    conn.execute(
        "UPDATE messages SET labels = ?1, starred = ?2, archived = ?3, snooze_until = ?4
         WHERE account_id = ?5 AND message_id = ?6",
        params![
            labels,
            kept.starred,
            kept.archived,
            kept.snooze_until,
            account_id.as_str(),
            into.canonical_id
        ],
    )?;
    sync_terms(conn, account_id.as_str(), &old_terms, &new_terms)?;
    Ok(())
}

fn rethread_accounts(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("SELECT DISTINCT account_id FROM messages")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut accounts = Vec::new();
    for row in rows {
        accounts.push(row?);
    }
    drop(stmt);
    for account_id in accounts {
        threads::rethread_all(conn, &AccountId(account_id))?;
    }
    Ok(())
}

fn alias_points_at(conn: &Connection, account_id: &AccountId, canonical_id: &str) -> Result<bool> {
    let hit: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM message_aliases
             WHERE account_id = ?1 AND canonical_message_id = ?2
             LIMIT 1",
            params![account_id.as_str(), canonical_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(hit.is_some())
}

/// The non-Sent side is the list copy, and the other side is in Sent.
///
/// A subject tag can identify an old list copy. For short replies with a
/// known list id, that id must belong to the non-Sent copy.
fn sent_list_echo(
    incoming: &Message,
    stored: &Arrival,
    incoming_list: bool,
    stored_list: bool,
    short: bool,
    roles: &FolderRoles,
) -> bool {
    let incoming_sent = incoming
        .folders
        .iter()
        .any(|f| roles.is(FolderRole::Sent, f));
    let stored_sent = stored
        .folders
        .split('\u{1e}')
        .any(|f| roles.is(FolderRole::Sent, f));
    if incoming_sent == stored_sent {
        return false;
    }
    let (listed, subject) = if stored_sent {
        (incoming_list, incoming.headers.subject.as_str())
    } else {
        (stored_list, stored.subject.as_str())
    };
    listed || (!(short && (incoming_list || stored_list)) && subject_has_list_prefix(subject))
}

/// Both copies name the same `In-Reply-To`, or both have none.
fn parents_match(incoming: &Message, stored: &Arrival) -> bool {
    let stored_parent = stored.in_reply_to.as_deref().filter(|id| !id.is_empty());
    let incoming_parent = incoming
        .headers
        .in_reply_to
        .as_ref()
        .map(MessageId::as_str)
        .filter(|id| !id.is_empty());
    match (stored_parent, incoming_parent) {
        (Some(stored_id), Some(incoming_id)) => stored_id == incoming_id,
        (None, None) => true,
        _ => false,
    }
}

fn subject_has_list_prefix(subject: &str) -> bool {
    let mut rest = subject.trim();
    while let Some(after) = strip_reply_prefix(rest) {
        rest = after.trim();
    }
    strip_list_prefix(rest).is_some()
}

/// Apply one server copy's read state to the row it backs.
///
/// The row is found by its remote id, an alias, or the copy recorded for it:
/// a second copy of one Message-ID (INBOX and Sent, INBOX and All Mail) has
/// no alias, and `remote_id` names only the copy written last. A row deleted
/// in Fell (Trash expunged) whose copy is still recorded takes the flag on
/// the snapshot undo would restore.
fn apply_flag(
    conn: &Connection,
    account_id: &AccountId,
    flag: &crate::backend::FlagUpdate,
    bridge: bool,
) -> Result<()> {
    let copy = crate::imap::parse_remote_id(&flag.remote_id.0);
    if let (Some(copy), Some(keywords)) = (&copy, &flag.keywords) {
        keywords::observe(conn, account_id, copy, flag.modseq, keywords, bridge)?;
    }
    let fresh = if let Some(copy) = &copy
        && ((flag.unread.is_some() && flag.modseq.is_some()) || (bridge && flag.flagged.is_some()))
    {
        copies::observe_flags(
            conn,
            account_id,
            copy,
            copies::CopyFlags {
                seen: flag.unread.map(|unread| !unread),
                flagged: flag.flagged,
                modseq: flag.modseq,
            },
            copies::FlagSource::Sync,
        )?
    } else {
        true
    };
    if bridge
        && let Some(copy) = &copy
        && let Some(id) = copies::row_of(conn, account_id, copy)?
    {
        copies::reconcile_star(conn, account_id, &id)?;
    }
    if !fresh {
        return Ok(());
    }
    let Some(server_unread) = flag.unread else {
        return Ok(());
    };
    let targets: Vec<String> = {
        let mut stmt = conn.prepare_cached(FLAG_TARGETS)?;
        stmt.query_map(
            params![
                account_id.as_str(),
                flag.remote_id.0,
                copy.as_ref().map(|copy| copy.mailbox.as_str()),
                copy.as_ref().map(|copy| copy.uidvalidity),
                copy.as_ref().map(|copy| copy.uid),
            ],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?
    };
    if targets.is_empty() {
        if let Some(copy) = &copy
            && let Some(message_id) = copies::row_of(conn, account_id, copy)?
        {
            crate::actions::fold_flag_into_snapshots(conn, account_id, &message_id, server_unread)?;
        }
        return Ok(());
    }
    let mut update = conn.prepare_cached(
        "UPDATE messages SET unread = ?1
         WHERE account_id = ?2 AND message_id = ?3 AND unread != ?1",
    )?;
    for message_id in targets {
        // Whichever way the row was found, a read or unread set in Fell
        // stays until every server copy of it agrees.
        let unread = intents::server_unread(
            conn,
            account_id,
            &message_id,
            &flag.remote_id.0,
            server_unread,
        )?;
        update.execute(params![i64::from(unread), account_id.as_str(), message_id])?;
    }
    Ok(())
}

fn remember_alias(conn: &Connection, message: &Message, canonical_id: &str) -> Result<()> {
    let alias_id = message.headers.message_id.as_str();
    if alias_id == canonical_id {
        return Ok(());
    }
    let folder = union_tokens("", &message.folders, false);
    conn.execute(
        "INSERT INTO message_aliases (
            account_id, message_id, canonical_message_id, remote_id, folder
         ) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
            remote_id = excluded.remote_id,
            folder = excluded.folder",
        params![
            message.account_id.as_str(),
            alias_id,
            canonical_id,
            message.remote_id.0,
            folder,
        ],
    )?;
    Ok(())
}

fn message_fingerprint(message: &Message) -> String {
    fingerprint_of(message.body_text.as_deref())
}

/// Fingerprint of a non-empty body. A missing or blank body, including a
/// subject used as the snippet, has no fingerprint.
fn fingerprint_of(body: Option<&str>) -> String {
    match body {
        Some(text) if !text.trim().is_empty() => body_fingerprint(text),
        _ => String::new(),
    }
}

/// Both copies need a real timestamp. Zero is "no date", not the Unix epoch,
/// so two undated messages are not within six hours of each other.
fn dates_within_echo_window(stored: i64, incoming: i64) -> bool {
    stored != 0 && incoming != 0 && stored.abs_diff(incoming) <= LIST_ECHO_WINDOW_SECS
}

fn clean_list_id(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn list_is_set(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|id| !id.trim().is_empty())
}

fn merged_list_id(existing: &Option<String>, incoming: &Option<String>) -> Option<String> {
    clean_list_id(existing).or_else(|| clean_list_id(incoming))
}

fn clean_opt(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn merged_proton(
    stored: &Option<String>,
    incoming: &Option<String>,
    like_alias: bool,
) -> Option<String> {
    let stored = clean_opt(stored);
    let incoming = clean_opt(incoming);
    if like_alias || incoming.is_none() {
        stored.or(incoming)
    } else {
        incoming
    }
}

fn proton_compatible(stored: &Option<String>, incoming: &Option<String>) -> bool {
    match (clean_opt(stored), clean_opt(incoming)) {
        (Some(left), Some(right)) => left == right,
        _ => true,
    }
}

fn prefer_nonempty(primary: &str, fallback: &str) -> String {
    if primary.trim().is_empty() {
        fallback.to_string()
    } else {
        primary.to_string()
    }
}

fn body_is_missing(body: &Option<String>) -> bool {
    match body {
        Some(text) => text.trim().is_empty(),
        None => true,
    }
}

/// A non-empty incoming body replaces. An empty incoming body leaves the stored one.
fn filled_body_canonical(stored: &Arrival, message: &Message) -> (Option<String>, String) {
    if body_is_missing(&message.body_text) {
        let snippet = if stored.snippet.trim().is_empty() && !message.snippet.trim().is_empty() {
            message.snippet.clone()
        } else {
            stored.snippet.clone()
        };
        (stored.body_text.clone(), snippet)
    } else {
        (message.body_text.clone(), message.snippet.clone())
    }
}

/// Keep a stored body. Copy the incoming body only when the stored row has none.
/// The snippet follows that body, so a subject preview does not stick.
fn filled_body_keep_stored(stored: &Arrival, message: &Message) -> (Option<String>, String) {
    let take_incoming = body_is_missing(&stored.body_text) && !body_is_missing(&message.body_text);
    let body = if take_incoming {
        message.body_text.clone()
    } else {
        stored.body_text.clone()
    };
    let snippet = if !message.snippet.trim().is_empty()
        && (take_incoming || stored.snippet.trim().is_empty())
    {
        message.snippet.clone()
    } else {
        stored.snippet.clone()
    };
    (body, snippet)
}

/// Prefer the copy a list did not rewrite. Never blank a subject already stored.
fn merged_subject(
    stored_subject: &str,
    stored_list: bool,
    incoming_subject: &str,
    incoming_list: bool,
    replace_non_list: bool,
) -> String {
    let stored_blank = stored_subject.trim().is_empty();
    let incoming_blank = incoming_subject.trim().is_empty();
    if stored_list && !incoming_list && !incoming_blank {
        return incoming_subject.to_string();
    }
    if replace_non_list && !incoming_blank {
        return incoming_subject.to_string();
    }
    if !stored_blank && (incoming_list || incoming_blank) {
        return stored_subject.to_string();
    }
    if stored_blank {
        return incoming_subject.to_string();
    }
    if replace_non_list {
        return incoming_subject.to_string();
    }
    stored_subject.to_string()
}

fn push_parent_id(ids: &mut Vec<String>, raw: &str) {
    let id = MessageId::new(raw);
    if id.as_str().is_empty() || ids.iter().any(|existing| existing == id.as_str()) {
        return;
    }
    ids.push(id.as_str().to_string());
}

fn merged_display_name(
    stored_name: &Option<String>,
    stored_list: bool,
    incoming_name: &Option<String>,
    incoming_list: bool,
    replace_non_list: bool,
) -> Option<String> {
    let chosen = merged_subject(
        stored_name.as_deref().unwrap_or("").trim(),
        stored_list,
        incoming_name.as_deref().unwrap_or("").trim(),
        incoming_list,
        replace_non_list,
    );
    let chosen = chosen.trim();
    if chosen.is_empty() {
        None
    } else {
        Some(chosen.to_string())
    }
}

/// Same-id resync replaces parents when the incoming copy has any. An alias
/// merge still unions them and prefers the non-list `In-Reply-To`.
fn parents_for_write(stored: &Arrival, message: &Message, alias: bool) -> (Option<String>, String) {
    if !alias {
        let incoming_primary = message
            .headers
            .in_reply_to
            .as_ref()
            .map(|id| id.as_str().to_string())
            .filter(|id| !id.is_empty());
        if incoming_primary.is_some() || !message.headers.references.is_empty() {
            return (incoming_primary, references_header(&message.headers));
        }
        let stored_primary = stored.in_reply_to.clone().filter(|id| !id.is_empty());
        return (stored_primary, stored.references_header.clone());
    }
    union_parents(stored, message)
}

/// Keep every parent id. When they disagree, `In-Reply-To` is the non-list copy.
fn union_parents(stored: &Arrival, message: &Message) -> (Option<String>, String) {
    let stored_primary = stored.in_reply_to.clone().filter(|id| !id.is_empty());
    let incoming_primary = message
        .headers
        .in_reply_to
        .as_ref()
        .map(|id| id.as_str().to_string())
        .filter(|id| !id.is_empty());
    let stored_is_list = list_is_set(&stored.list_id);
    let incoming_is_list = clean_list_id(&message.list_id).is_some();
    let preferred = if stored_is_list && !incoming_is_list {
        incoming_primary.clone().or(stored_primary.clone())
    } else {
        stored_primary.clone().or(incoming_primary.clone())
    };
    let mut ids = Vec::new();
    if let Some(id) = stored_primary.as_deref() {
        push_parent_id(&mut ids, id);
    }
    for id in parse_references(&stored.references_header) {
        push_parent_id(&mut ids, id.as_str());
    }
    if let Some(id) = incoming_primary.as_deref() {
        push_parent_id(&mut ids, id);
    }
    for id in &message.headers.references {
        push_parent_id(&mut ids, id.as_str());
    }
    (preferred, ids.join("\u{1e}"))
}

fn same_sender(left: &str, right: &str) -> bool {
    let left = left.trim();
    !left.is_empty() && left.eq_ignore_ascii_case(right.trim())
}

/// Subject with leading `Re:` / `Fwd:` / `Fw:` and `[list]` tags removed.
///
/// Not a collapse key by itself. Prefixes are matched on bytes so a CJK
/// subject is never sliced inside a character.
fn subject_key(subject: &str) -> String {
    let mut rest = subject.trim();
    loop {
        if rest.is_empty() {
            return String::new();
        }
        if let Some(after) = strip_reply_prefix(rest) {
            rest = after.trim();
            continue;
        }
        if let Some(after) = strip_list_prefix(rest) {
            rest = after.trim();
            continue;
        }
        break;
    }
    rest.to_lowercase()
}

#[cfg_attr(not(test), allow(dead_code))]
fn strip_reply_prefix(subject: &str) -> Option<&str> {
    let bytes = subject.as_bytes();
    // `fwd:` before `fw:` so the longer prefix wins. Match on bytes first.
    for prefix in [b"re:".as_slice(), b"fwd:", b"fw:"] {
        if bytes.len() >= prefix.len() && bytes[..prefix.len()].eq_ignore_ascii_case(prefix) {
            return Some(&subject[prefix.len()..]);
        }
    }
    None
}

#[cfg_attr(not(test), allow(dead_code))]
fn strip_list_prefix(subject: &str) -> Option<&str> {
    let subject = subject.trim_start();
    if !subject.starts_with('[') {
        return None;
    }
    let end = subject.find(']')?;
    let tag = &subject[1..end];
    if tag.is_empty() || tag.len() > 80 {
        return None;
    }
    Some(&subject[end + 1..])
}

fn schema_version(conn: &Connection) -> Result<Option<String>> {
    let value = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'schema_version'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value)
}

const FINGERPRINT_BATCH: usize = 32;

/// Hash bodies in small batches. Each batch commits before the next is read,
/// and rows with no body are skipped by rowid so they are not loaded again.
fn backfill_fingerprints(conn: &Connection) -> Result<()> {
    let mut after = 0i64;
    loop {
        // Read the batch inside the write transaction, so a body another
        // connection changed while this one waited is the body hashed.
        let tx = begin_immediate(conn)?;
        let mut stmt = tx.prepare(
            "SELECT rowid, body_text FROM messages
             WHERE body_fingerprint = '' AND rowid > ?1
             ORDER BY rowid
             LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after, FINGERPRINT_BATCH as i64], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
        })?;
        let mut batch = Vec::new();
        for row in rows {
            batch.push(row?);
        }
        drop(stmt);
        if batch.is_empty() {
            tx.commit()?;
            break;
        }
        for (rowid, body) in &batch {
            after = *rowid;
            let fingerprint = fingerprint_of(body.as_deref());
            if fingerprint.is_empty() {
                continue;
            }
            tx.execute(
                "UPDATE messages SET body_fingerprint = ?1 WHERE rowid = ?2",
                params![fingerprint, rowid],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn load_aliases(conn: &Connection, account_id: &AccountId) -> Result<Vec<(MessageId, MessageId)>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, canonical_message_id FROM message_aliases WHERE account_id = ?1",
    )?;
    let rows = stmt.query_map(params![account_id.as_str()], |row| {
        Ok((MessageId(row.get(0)?), MessageId(row.get(1)?)))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn union_addresses(existing: &str, incoming: &[Address]) -> String {
    let mut out = decode_addresses(existing);
    let mut seen: HashSet<String> = out.iter().map(|addr| addr.email.to_lowercase()).collect();
    for addr in incoming {
        let email = addr.email.trim();
        if email.is_empty() || !seen.insert(email.to_lowercase()) {
            continue;
        }
        out.push(Address {
            name: addr.name.clone(),
            email: email.to_string(),
        });
    }
    encode_addresses(&out)
}

fn parse_references(raw: &str) -> Vec<MessageId> {
    if raw.is_empty() {
        Vec::new()
    } else {
        raw.split('\u{1e}').map(MessageId::new).collect()
    }
}

pub(crate) fn split_field(raw: &str) -> Vec<String> {
    raw.split('\u{1e}')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn references_header(headers: &MessageHeaders) -> String {
    headers
        .references
        .iter()
        .map(MessageId::as_str)
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

pub(crate) fn join_tokens(tokens: &[String]) -> String {
    tokens.join("\u{1e}")
}

fn strip_inbox_tokens(raw: &str) -> String {
    split_field(raw)
        .into_iter()
        .filter(|folder| !folder.eq_ignore_ascii_case("INBOX"))
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

pub(crate) fn union_tokens(existing: &str, incoming: &[String], address: bool) -> String {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for raw in split_field(existing)
        .iter()
        .map(String::as_str)
        .chain(incoming.iter().map(String::as_str))
    {
        let token = if address {
            raw.trim().to_lowercase()
        } else {
            raw.trim().to_string()
        };
        if token.is_empty() || !seen.insert(token.clone()) {
            continue;
        }
        out.push(token);
    }
    out.join("\u{1e}")
}

fn ensure_compose_columns(conn: &Connection) -> Result<()> {
    let have = column_names(conn, "compose_drafts")?;
    for (name, ddl) in [
        (
            "identities",
            "ALTER TABLE compose_drafts ADD COLUMN identities TEXT NOT NULL DEFAULT ''",
        ),
        (
            "owner",
            "ALTER TABLE compose_drafts ADD COLUMN owner TEXT NOT NULL DEFAULT ''",
        ),
        (
            "references_header",
            "ALTER TABLE compose_drafts ADD COLUMN references_header TEXT NOT NULL DEFAULT ''",
        ),
        (
            "quote_text",
            "ALTER TABLE compose_drafts ADD COLUMN quote_text TEXT NOT NULL DEFAULT ''",
        ),
        (
            "quote_html",
            "ALTER TABLE compose_drafts ADD COLUMN quote_html TEXT NOT NULL DEFAULT ''",
        ),
        (
            "reply_to_unverified",
            "ALTER TABLE compose_drafts ADD COLUMN reply_to_unverified INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "discarded_action_id",
            "ALTER TABLE compose_drafts ADD COLUMN discarded_action_id INTEGER",
        ),
    ] {
        if have.iter().any(|column| column == name) {
            continue;
        }
        if let Err(err) = conn.execute(ddl, [])
            && !duplicate_column(&err)
        {
            return Err(err.into());
        }
    }
    Ok(())
}

fn ensure_agent_action_columns(conn: &Connection) -> Result<()> {
    let have = column_names(conn, "agent_action_messages")?;
    if have.iter().any(|column| column == "starred") {
        return Ok(());
    }
    if let Err(err) = conn.execute(
        "ALTER TABLE agent_action_messages ADD COLUMN starred INTEGER",
        [],
    ) && !duplicate_column(&err)
    {
        return Err(err.into());
    }
    Ok(())
}

fn ensure_alias_columns(conn: &Connection) -> Result<()> {
    let have = column_names(conn, "message_aliases")?;
    for (name, ddl) in [
        (
            "remote_id",
            "ALTER TABLE message_aliases ADD COLUMN remote_id TEXT NOT NULL DEFAULT ''",
        ),
        (
            "folder",
            "ALTER TABLE message_aliases ADD COLUMN folder TEXT NOT NULL DEFAULT ''",
        ),
    ] {
        if have.iter().any(|column| column == name) {
            continue;
        }
        if let Err(err) = conn.execute(ddl, [])
            && !duplicate_column(&err)
        {
            return Err(err.into());
        }
    }
    Ok(())
}

fn ensure_message_columns(conn: &Connection) -> Result<()> {
    // Exclusive lock so two processes opening an old file don't both ALTER.
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let result = add_missing_message_columns(conn);
    if result.is_ok() {
        conn.execute_batch("COMMIT")?;
    } else {
        let _ = conn.execute_batch("ROLLBACK");
    }
    result
}

fn add_missing_message_columns(conn: &Connection) -> Result<()> {
    let missing: &[(&str, &str)] = &[
        (
            "folders",
            "ALTER TABLE messages ADD COLUMN folders TEXT NOT NULL DEFAULT ''",
        ),
        (
            "labels",
            "ALTER TABLE messages ADD COLUMN labels TEXT NOT NULL DEFAULT ''",
        ),
        (
            "arrived_via",
            "ALTER TABLE messages ADD COLUMN arrived_via TEXT NOT NULL DEFAULT ''",
        ),
        (
            "to_line",
            "ALTER TABLE messages ADD COLUMN to_line TEXT NOT NULL DEFAULT ''",
        ),
        (
            "cc_line",
            "ALTER TABLE messages ADD COLUMN cc_line TEXT NOT NULL DEFAULT ''",
        ),
        (
            "bcc_line",
            "ALTER TABLE messages ADD COLUMN bcc_line TEXT NOT NULL DEFAULT ''",
        ),
        (
            "starred",
            "ALTER TABLE messages ADD COLUMN starred INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "has_attachment",
            "ALTER TABLE messages ADD COLUMN has_attachment INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "archived",
            "ALTER TABLE messages ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "snooze_until",
            "ALTER TABLE messages ADD COLUMN snooze_until INTEGER",
        ),
        (
            "body_fingerprint",
            "ALTER TABLE messages ADD COLUMN body_fingerprint TEXT NOT NULL DEFAULT ''",
        ),
        ("list_id", "ALTER TABLE messages ADD COLUMN list_id TEXT"),
        (
            "reply_to_line",
            "ALTER TABLE messages ADD COLUMN reply_to_line TEXT NOT NULL DEFAULT ''",
        ),
        (
            "attachment_names",
            "ALTER TABLE messages ADD COLUMN attachment_names TEXT NOT NULL DEFAULT ''",
        ),
        (
            "part_cache_stamp",
            "ALTER TABLE messages ADD COLUMN part_cache_stamp BLOB",
        ),
    ];
    let have = column_names(conn, "messages")?;
    for (name, ddl) in missing {
        if have.iter().any(|column| column == name) {
            continue;
        }
        if let Err(err) = conn.execute(ddl, [])
            && !duplicate_column(&err)
        {
            return Err(err.into());
        }
    }
    Ok(())
}

/// Rebuild a set of triggers when `version` differs from `meta.key`.
///
/// Dropping and creating triggers outside a transaction leaves a moment with
/// no trigger, and another connection's write in that moment is never
/// queued. Here the drop, the create, and the version stamp commit together
/// under a write lock, and a current mailbox does no trigger DDL at all.
pub(crate) fn ensure_triggers(
    conn: &Connection,
    key: &str,
    version: &str,
    sql: &str,
) -> Result<()> {
    let current = |conn: &Connection| -> Result<bool> {
        let stamped: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        Ok(stamped.as_deref() == Some(version))
    };
    if current(conn)? {
        return Ok(());
    }
    let tx = immediate_transaction(conn)?;
    if current(&tx)? {
        return Ok(());
    }
    tx.execute_batch(sql)?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, version],
    )?;
    tx.commit()?;
    Ok(())
}

/// Write lock from the first statement, so an embedding commit cannot
/// invalidate a snapshot this transaction already read.
/// A plain transaction that takes the write lock up front.
///
/// A deferred transaction that reads and then writes cannot wait for
/// another writer: in WAL mode SQLite answers the upgrade with "database is
/// locked" at once, ignoring the busy timeout. Opening the mailbox while the
/// indexer, sync, or `fell-mcp` was writing failed that way. Starting with
/// `BEGIN IMMEDIATE` lets the busy timeout apply. Use it for writes that run
/// before the summary tables exist and for writes that never touch
/// `messages` (compose drafts); other writes use [`immediate_transaction`].
pub(crate) fn begin_immediate(conn: &Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
    rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
}

pub(crate) fn immediate_transaction(conn: &Connection) -> Result<WriteTx<'_>> {
    rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map(WriteTx)
        .map_err(Into::into)
}

/// A write transaction that brings folder-list summaries up to date for the
/// threads it touched before it commits. Use it for every write to `messages`.
pub(crate) struct WriteTx<'a>(rusqlite::Transaction<'a>);

impl<'a> std::ops::Deref for WriteTx<'a> {
    type Target = rusqlite::Transaction<'a>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl WriteTx<'_> {
    pub(crate) fn commit(self) -> Result<()> {
        summaries::refresh_dirty(&self.0)?;
        self.0.commit().map_err(Into::into)
    }

    pub(crate) fn rollback(self) -> Result<()> {
        self.0.rollback().map_err(Into::into)
    }
}

pub(crate) fn ensure_fts(conn: &Connection) -> Result<()> {
    // Most opens change nothing here. Check without the write lock, so an
    // open does not hold up sync or the indexer for two counts.
    if fts_is_current(conn)? {
        return Ok(());
    }
    // Drop, create, and copy commit together. A killed process rolls the
    // upgrade back, so the next open tries again instead of keeping an empty
    // index whose columns merely look current.
    let tx = begin_immediate(conn)?;
    let have = column_names(&tx, "messages_fts")?;
    let current = FTS_COLUMNS
        .iter()
        .all(|name| have.iter().any(|column| column == name));
    let messages: i64 = tx.query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))?;
    if !current {
        tx.execute_batch(&format!(
            "{DROP_FTS_TRIGGERS} DROP TABLE IF EXISTS messages_fts;"
        ))?;
        tx.execute_batch(FTS_TABLE)?;
        tx.execute_batch(FTS_COPY)?;
    } else if messages > 0 {
        let indexed: i64 =
            tx.query_row("SELECT COUNT(*) FROM messages_fts", [], |row| row.get(0))?;
        if indexed == 0 {
            tx.execute_batch(FTS_COPY)?;
        }
    }
    tx.execute_batch(DROP_FTS_TRIGGERS)?;
    tx.execute_batch(FTS_TRIGGERS)?;
    tx.commit()?;
    Ok(())
}

const FTS_COLUMNS: [&str; 6] = [
    "subject",
    "sender",
    "recipients",
    "body",
    "account_id",
    "message_id",
];

/// The index has the current columns, holds rows when messages do, and its
/// triggers are exactly [`FTS_TRIGGERS`]. Reads only.
fn fts_is_current(conn: &Connection) -> Result<bool> {
    let have = column_names(conn, "messages_fts")?;
    if !FTS_COLUMNS
        .iter()
        .all(|name| have.iter().any(|column| column == name))
    {
        return Ok(false);
    }
    let messages: i64 = conn.query_row("SELECT EXISTS(SELECT 1 FROM messages)", [], |row| {
        row.get(0)
    })?;
    if messages != 0 {
        let indexed: i64 =
            conn.query_row("SELECT EXISTS(SELECT 1 FROM messages_fts)", [], |row| {
                row.get(0)
            })?;
        if indexed == 0 {
            return Ok(false);
        }
    }
    let mut stmt = conn.prepare(
        "SELECT sql FROM sqlite_master
         WHERE type = 'trigger' AND name IN ('messages_ai', 'messages_ad', 'messages_au')
         ORDER BY name",
    )?;
    let stored: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    let squash = |sql: &str| sql.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut wanted: Vec<String> = FTS_TRIGGERS
        .split("END;")
        .map(str::trim)
        .filter(|sql| !sql.is_empty())
        .map(|sql| squash(&format!("{sql} END")))
        .collect();
    wanted.sort();
    let mut stored: Vec<String> = stored.iter().map(|sql| squash(sql)).collect();
    stored.sort();
    Ok(stored == wanted)
}

fn duplicate_column(err: &rusqlite::Error) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("duplicate column")
}

fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut names = Vec::new();
    for name in rows {
        names.push(name?);
    }
    Ok(names)
}

pub(crate) fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let references: String = row.get(6)?;
    let in_reply_to: Option<String> = row.get(5)?;
    Ok(Message {
        account_id: AccountId(row.get(0)?),
        headers: MessageHeaders {
            message_id: MessageId(row.get(1)?),
            proton_conversation_id: row.get(4)?,
            in_reply_to: in_reply_to.map(MessageId),
            references: parse_references(&references),
            from: Address {
                name: row.get(7)?,
                email: row.get(8)?,
            },
            subject: row.get(9)?,
            date_unix: row.get(10)?,
        },
        remote_id: RemoteId(row.get(2)?),
        thread_id: ThreadId(row.get(3)?),
        snippet: row.get(11)?,
        body_text: row.get(12)?,
        unread: row.get::<_, i64>(13)? != 0,
        folders: split_field(&row.get::<_, String>(14)?),
        labels: split_field(&row.get::<_, String>(15)?),
        arrived_via: split_field(&row.get::<_, String>(16)?),
        to: decode_addresses(&row.get::<_, String>(17)?),
        cc: decode_addresses(&row.get::<_, String>(18)?),
        bcc: decode_addresses(&row.get::<_, String>(19)?),
        starred: row.get::<_, i64>(20)? != 0,
        has_attachment: row.get::<_, i64>(21)? != 0,
        archived: row.get::<_, i64>(22)? != 0,
        snooze_until: row.get(23)?,
        list_id: row.get(24)?,
        reply_to: crate::message::decode_reply_to(&row.get::<_, String>(25)?),
        attachment_names: split_field(&row.get::<_, String>(26)?),
        parts: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MailboxDelta;
    use crate::backend::SyncCursor;
    use crate::folders::ListedMailbox;

    fn account() -> Account {
        Account {
            id: AccountId("a1".into()),
            display_name: "Caleb".into(),
            email: "c@x".into(),
            backend_kind: BackendKind::Bridge,
        }
    }

    fn account_b() -> Account {
        Account {
            id: AccountId("a2".into()),
            display_name: "Other".into(),
            email: "o@x".into(),
            backend_kind: BackendKind::Bridge,
        }
    }

    fn msg_for(
        account: &str,
        id: &str,
        proton: Option<&str>,
        in_reply_to: Option<&str>,
        subject: &str,
        body: &str,
    ) -> Message {
        Message {
            account_id: AccountId(account.into()),
            headers: MessageHeaders {
                message_id: MessageId::new(id),
                proton_conversation_id: proton.map(str::to_string),
                in_reply_to: in_reply_to.map(MessageId::new),
                references: in_reply_to.map(MessageId::new).into_iter().collect(),
                from: Address {
                    name: None,
                    email: "from@x".into(),
                },
                subject: subject.into(),
                date_unix: 1,
            },
            remote_id: RemoteId(format!("{account}:{id}")),
            thread_id: ThreadId::from_message_id(&MessageId::new(id)),
            snippet: body.into(),
            body_text: Some(body.into()),
            unread: true,
            folders: Vec::new(),
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

    fn msg(
        id: &str,
        proton: Option<&str>,
        in_reply_to: Option<&str>,
        subject: &str,
        body: &str,
    ) -> Message {
        msg_for("a1", id, proton, in_reply_to, subject, body)
    }

    #[test]
    fn message_lookup_seeks_by_account_and_message_id() {
        let store = Store::open_in_memory().unwrap();
        let plan: Vec<String> = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {}", message_lookup_sql()))
            .unwrap()
            .query_map(["a1", "id@x"], |row| row.get::<_, String>(3))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        let plan = plan.join("\n");
        assert!(!plan.contains("SCAN m"), "{plan}");
        for (table, expected) in [("SEARCH m ", 2), ("SEARCH a ", 1)] {
            let seeks = plan
                .lines()
                .filter(|detail| {
                    detail.starts_with(table) && detail.ends_with("(account_id=? AND message_id=?)")
                })
                .count();
            assert_eq!(seeks, expected, "{plan}");
        }
    }

    #[test]
    fn message_lookup_resolves_aliases_within_the_requested_account() {
        let store = Store::open_in_memory().unwrap();
        let first = account();
        let second = account_b();
        store.upsert_account(&first).unwrap();
        store.upsert_account(&second).unwrap();
        store
            .upsert_message(&msg("post@x", None, None, "First account", "first"))
            .unwrap();
        let kept = msg_for("a2", "post@x", None, None, "Second account", "second");
        store.upsert_message(&kept).unwrap();
        let alias = msg_for("a2", "echo@x", None, None, "List echo", "second");
        remember_alias(&store.conn, &alias, kept.headers.message_id.as_str()).unwrap();

        let direct = store
            .message(&second.id, &kept.headers.message_id)
            .unwrap()
            .unwrap();
        assert_eq!(direct.account_id, second.id);
        assert_eq!(direct.headers.subject, kept.headers.subject);
        for id in ["echo@x", " <echo@x> "] {
            let id = MessageId::new(id);
            assert_eq!(store.message(&second.id, &id).unwrap().unwrap(), direct);
            assert!(store.message(&first.id, &id).unwrap().is_none());
        }
    }

    #[test]
    fn per_thread_lookups_use_the_thread_date_index() {
        let store = Store::open_in_memory().unwrap();
        let plan: Vec<String> = store
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT subject FROM messages s
                  WHERE s.account_id = ?1 AND s.thread_id = ?2
                  ORDER BY s.date_unix ASC, s.rowid ASC LIMIT 1",
            )
            .unwrap()
            .query_map(["a1", "t1"], |row| row.get::<_, String>(3))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        let plan = plan.join("\n");
        assert!(plan.contains("idx_messages_thread_date"), "{plan}");
        assert!(!plan.contains("TEMP B-TREE"), "{plan}");
    }

    #[test]
    fn a_sender_is_named_as_their_latest_mail_names_them() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut old = msg("old@x", None, None, "One", "first");
        old.headers.from = Address {
            name: Some("Ada L".into()),
            email: "Ada@X".into(),
        };
        old.headers.date_unix = 10;
        store.upsert_message(&old).unwrap();
        let mut new = msg("new@x", None, None, "Two", "second");
        new.headers.from = Address {
            name: Some("Ada Lovelace".into()),
            email: "ada@x".into(),
        };
        new.headers.date_unix = 20;
        store.upsert_message(&new).unwrap();
        let mut unnamed = msg("bare@x", None, None, "Three", "third");
        unnamed.headers.from = Address {
            name: None,
            email: "sam@x".into(),
        };
        store.upsert_message(&unnamed).unwrap();
        let names = store
            .sender_names(&AccountId("a1".into()), &["ADA@x".into(), "sam@x".into()])
            .unwrap();
        assert_eq!(names.get("ada@x").map(String::as_str), Some("Ada Lovelace"));
        assert_eq!(names.get("sam@x"), None, "no name is made up");
    }

    #[test]
    fn drafts_name_who_the_latest_draft_is_to() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let ada = Address {
            name: Some("Ada Lovelace".into()),
            email: "ada@x".into(),
        };
        let mut root = msg("root@x", None, None, "Plan", "the plan");
        root.headers.from = ada.clone();
        root.folders = vec!["INBOX".into()];
        let thread = store.upsert_message(&root).unwrap();
        // The person's synced reply sits in Drafts in the same thread.
        let mut draft = msg("draft@x", None, Some("root@x"), "Re: Plan", "sounds good");
        draft.headers.from = Address {
            name: None,
            email: "c@x".into(),
        };
        draft.headers.date_unix = 2;
        draft.folders = vec!["Drafts".into()];
        draft.to = vec![ada.clone()];
        draft.cc = vec![Address {
            name: None,
            email: "sam@x".into(),
        }];
        assert_eq!(store.upsert_message(&draft).unwrap(), thread);
        let mut other = msg("other@x", None, None, "Other", "inbox only");
        other.folders = vec!["INBOX".into()];
        let elsewhere = store.upsert_message(&other).unwrap();
        let found = store
            .folder_recipients(
                &AccountId("a1".into()),
                "drafts",
                &[thread.clone(), elsewhere.clone()],
            )
            .unwrap();
        let people: Vec<&str> = found[&thread]
            .iter()
            .map(|person| person.email.as_str())
            .collect();
        // The draft's recipients, not the received message's sender.
        assert_eq!(people, vec!["ada@x", "sam@x"]);
        assert!(!found.contains_key(&elsewhere));
    }

    #[test]
    fn threads_reply_via_in_reply_to() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        assert_eq!(
            store
                .get_account(&AccountId("a1".into()))
                .unwrap()
                .unwrap()
                .email,
            "c@x"
        );
        let root_thread = store
            .upsert_message(&msg("root@x", None, None, "Root", "root body"))
            .unwrap();
        let reply_thread = store
            .upsert_message(&msg(
                "reply@x",
                None,
                Some("root@x"),
                "Re: Root",
                "reply body",
            ))
            .unwrap();
        assert_eq!(root_thread, reply_thread);
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].message_count, 2);
    }

    #[test]
    fn proton_conversation_groups_messages() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let a = store
            .upsert_message(&msg("a@x", Some("conv"), None, "One", "a"))
            .unwrap();
        let b = store
            .upsert_message(&msg("b@x", Some("conv"), None, "Two", "b"))
            .unwrap();
        assert_eq!(a, ThreadId::proton("conv"));
        assert_eq!(a, b);
    }

    #[test]
    fn fts_search_finds_subject_and_body() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg("a@x", None, None, "Invoice March", "pay me"))
            .unwrap();
        store
            .upsert_message(&msg("b@x", None, None, "Hello", "unrelated"))
            .unwrap();
        let hits = store.search(&AccountId("a1".into()), "Invoice").unwrap();
        assert!(hits.did_you_mean.is_none());
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].message_id.as_str(), "a@x");
        let body_hits = store.search(&AccountId("a1".into()), "pay me").unwrap();
        assert_eq!(body_hits.hits.len(), 1);
    }

    #[test]
    fn two_accounts_keep_separate_rows_for_same_message_id() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store.upsert_account(&account_b()).unwrap();
        store
            .upsert_message(&msg_for(
                "a1",
                "shared@x",
                None,
                None,
                "From A",
                "account a body",
            ))
            .unwrap();
        store
            .upsert_message(&msg_for(
                "a2",
                "shared@x",
                None,
                None,
                "From B",
                "account b body",
            ))
            .unwrap();

        let a_threads = store.list_threads(&AccountId("a1".into())).unwrap();
        let b_threads = store.list_threads(&AccountId("a2".into())).unwrap();
        assert_eq!(a_threads.len(), 1);
        assert_eq!(b_threads.len(), 1);
        let a_msgs = store
            .thread_messages(&AccountId("a1".into()), &a_threads[0].thread_id)
            .unwrap();
        let b_msgs = store
            .thread_messages(&AccountId("a2".into()), &b_threads[0].thread_id)
            .unwrap();
        assert_eq!(a_msgs.len(), 1);
        assert_eq!(b_msgs.len(), 1);
        assert_eq!(a_msgs[0].headers.subject, "From A");
        assert_eq!(a_msgs[0].body_text.as_deref(), Some("account a body"));
        assert_eq!(b_msgs[0].headers.subject, "From B");
        assert_eq!(b_msgs[0].body_text.as_deref(), Some("account b body"));
        assert_eq!(a_msgs[0].remote_id.0, "a1:shared@x");
        assert_eq!(b_msgs[0].remote_id.0, "a2:shared@x");
    }

    #[test]
    fn reply_before_parent_joins_one_thread() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let child_tid = store
            .upsert_message(&msg("zzz@x", None, Some("aaa@x"), "Re: Root", "reply"))
            .unwrap();
        assert_eq!(
            store
                .thread_messages(&AccountId("a1".into()), &child_tid)
                .unwrap()
                .len(),
            1
        );
        let parent_tid = store
            .upsert_message(&msg("aaa@x", None, None, "Root", "root"))
            .unwrap();
        assert_eq!(parent_tid, child_tid);
        let msgs = store
            .thread_messages(&AccountId("a1".into()), &child_tid)
            .unwrap();
        assert_eq!(msgs.len(), 2);
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, child_tid);
        assert_eq!(threads[0].message_count, 2);
    }

    #[test]
    fn parent_before_reply_keeps_parent_thread_id() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let parent_tid = store
            .upsert_message(&msg("zzz@x", None, None, "Root", "root"))
            .unwrap();
        let child_tid = store
            .upsert_message(&msg("aaa@x", None, Some("zzz@x"), "Re: Root", "reply"))
            .unwrap();
        assert_eq!(child_tid, parent_tid);
        assert_eq!(
            store
                .thread_messages(&AccountId("a1".into()), &parent_tid)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn later_proton_id_moves_header_siblings() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![
                        msg("root@x", None, None, "Root", "root"),
                        msg("reply@x", None, Some("root@x"), "Re: Root", "reply"),
                    ],
                    cursor: None,
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![msg("root@x", Some("conv-9"), None, "Root", "root")],
                    cursor: None,
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, ThreadId::proton("conv-9"));
        assert_eq!(threads[0].message_count, 2);
    }

    #[test]
    fn reply_carries_proton_id_joins_parent_in_same_delta() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![
                        msg("root@x", None, None, "Root", "root"),
                        msg("reply@x", Some("conv"), Some("root@x"), "Re: Root", "reply"),
                    ],
                    cursor: None,
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, ThreadId::proton("conv"));
        assert_eq!(threads[0].message_count, 2);
    }

    #[test]
    fn later_reply_proton_id_joins_existing_header_thread() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![
                        msg("root@x", None, None, "Root", "root"),
                        msg("reply@x", None, Some("root@x"), "Re: Root", "reply"),
                    ],
                    cursor: None,
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        store
            .upsert_message(&msg(
                "reply@x",
                Some("conv"),
                Some("root@x"),
                "Re: Root",
                "reply",
            ))
            .unwrap();
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, ThreadId::proton("conv"));
        assert_eq!(threads[0].message_count, 2);
    }

    #[test]
    fn cursor_only_delta_leaves_threads_in_place() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg("root@x", None, None, "Root", "root"))
            .unwrap();
        store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![],
                    cursor: Some(SyncCursor {
                        account_key: "a1".into(),
                        token: "cursor-only".into(),
                    }),
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(
            store
                .sync_cursor(&AccountId("a1".into()))
                .unwrap()
                .unwrap()
                .token,
            "cursor-only"
        );
    }

    #[test]
    fn apply_delta_rolls_back_on_error() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let ok = msg("ok@x", None, None, "Ok", "ok");
        let bad = msg_for("missing", "bad@x", None, None, "Bad", "bad");
        let err = store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![ok, bad],
                    cursor: Some(SyncCursor {
                        account_key: "a1".into(),
                        token: "should-not-stick".into(),
                    }),
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, crate::Error::Backend(_)));
        assert!(
            store
                .list_threads(&AccountId("a1".into()))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .sync_cursor(&AccountId("a1".into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn apply_delta_rejects_other_account_messages() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store.upsert_account(&account_b()).unwrap();
        let err = store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![
                        msg("a@x", None, None, "From A", "a"),
                        msg_for("a2", "b@x", None, Some("a@x"), "From B", "b"),
                    ],
                    cursor: Some(SyncCursor {
                        account_key: "a1".into(),
                        token: "must-not-advance".into(),
                    }),
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap_err();
        assert!(matches!(err, crate::Error::Backend(_)));
        assert!(
            store
                .list_threads(&AccountId("a1".into()))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .list_threads(&AccountId("a2".into()))
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .sync_cursor(&AccountId("a1".into()))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn thread_id_update_does_not_rewrite_fts_bodies() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg("aaa@x", None, None, "Root", "rootbody"))
            .unwrap();
        store
            .upsert_message(&msg("zzz@x", None, Some("aaa@x"), "Re: Root", "replybody"))
            .unwrap();
        store.conn.execute("DELETE FROM messages_fts", []).unwrap();
        store
            .upsert_message(&msg("aaa@x", Some("conv"), None, "Root", "rootbody"))
            .unwrap();
        assert!(
            store
                .search(&AccountId("a1".into()), "replybody")
                .unwrap()
                .hits
                .is_empty()
        );
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "rootbody")
                .unwrap()
                .hits
                .len(),
            1
        );
        let sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'trigger' AND name = 'messages_au'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("AFTER UPDATE OF"));
        assert!(sql.contains("subject"));
        assert!(sql.contains("from_email"));
        assert!(sql.contains("body_text"));
    }

    fn assert_first_reply_id_survives(first_id: &str, second_id: &str, root_id: &str) {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let first = store
            .upsert_message(&msg(first_id, None, Some(root_id), "Re", "reply-a"))
            .unwrap();
        let second = store
            .upsert_message(&msg(second_id, None, Some(root_id), "Re", "reply-b"))
            .unwrap();
        assert_ne!(first, second);
        let parent = store
            .upsert_message(&msg(root_id, None, None, "Root", "root"))
            .unwrap();
        assert_eq!(parent, first);
        let msgs = store
            .thread_messages(&AccountId("a1".into()), &first)
            .unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(
            store
                .thread_messages(&AccountId("a1".into()), &second)
                .unwrap()
                .is_empty()
        );
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].thread_id, first);
        assert_eq!(threads[0].message_count, 3);
    }

    #[test]
    fn two_replies_before_parent_keep_first_thread_id() {
        assert_first_reply_id_survives("child-a@x", "child-b@x", "root@x");
        // First id sorts after the second, so lexical order would keep the wrong one.
        assert_first_reply_id_survives("zzz-child@x", "aaa-child@x", "mmm-root@x");
    }

    #[test]
    fn distinct_proton_conversations_are_not_unioned() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let root = store
            .upsert_message(&msg("root@x", Some("conv-a"), None, "Root", "root body"))
            .unwrap();
        let reply = store
            .upsert_message(&msg(
                "reply@x",
                Some("conv-b"),
                Some("root@x"),
                "Re: Root",
                "reply body",
            ))
            .unwrap();
        assert_eq!(root, ThreadId::proton("conv-a"));
        assert_eq!(reply, ThreadId::proton("conv-b"));
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 2);
        let conv_a = store
            .thread_messages(&AccountId("a1".into()), &ThreadId::proton("conv-a"))
            .unwrap();
        let conv_b = store
            .thread_messages(&AccountId("a1".into()), &ThreadId::proton("conv-b"))
            .unwrap();
        assert_eq!(conv_a.len(), 1);
        assert_eq!(conv_a[0].headers.message_id.as_str(), "root@x");
        assert_eq!(conv_b.len(), 1);
        assert_eq!(conv_b[0].headers.message_id.as_str(), "reply@x");
    }

    #[test]
    fn two_proton_replies_do_not_pull_each_other_onto_one_thread() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![
                        msg("root@x", None, None, "Root", "root"),
                        msg("reply-a@x", Some("conv-a"), Some("root@x"), "Re: Root", "a"),
                        msg("reply-b@x", Some("conv-b"), Some("root@x"), "Re: Root", "b"),
                    ],
                    cursor: None,
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 2);
        let conv_a = store
            .thread_messages(&AccountId("a1".into()), &ThreadId::proton("conv-a"))
            .unwrap();
        let conv_b = store
            .thread_messages(&AccountId("a1".into()), &ThreadId::proton("conv-b"))
            .unwrap();
        let mut conv_a_ids: Vec<&str> = conv_a
            .iter()
            .map(|row| row.headers.message_id.as_str())
            .collect();
        let mut conv_b_ids: Vec<&str> = conv_b
            .iter()
            .map(|row| row.headers.message_id.as_str())
            .collect();
        conv_a_ids.sort_unstable();
        conv_b_ids.sort_unstable();
        assert_eq!(conv_a_ids, ["reply-a@x", "root@x"]);
        assert_eq!(conv_b_ids, ["reply-b@x"]);
    }

    fn with_arrival(mut message: Message, folder: &str, label: &str, via: &str) -> Message {
        if !folder.is_empty() {
            message.folders.push(folder.to_string());
        }
        if !label.is_empty() {
            message.labels.push(label.to_string());
        }
        if !via.is_empty() {
            message.arrived_via.push(via.to_string());
        }
        message
    }

    #[test]
    fn sent_and_alias_copies_collapse_on_message_id() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let sent = with_arrival(
            msg("same@x", None, None, "Hello", "hello body"),
            "Sent",
            "work",
            "Me@x.com",
        );
        let alias_a = with_arrival(
            msg("same@x", None, None, "Hello", "hello body"),
            "INBOX",
            "alias",
            "Alias-A@x.com",
        );
        let mut alias_b = with_arrival(
            msg("same@x", None, None, "Hello", "hello body"),
            "INBOX",
            "alias",
            "alias-b@x.com",
        );
        alias_b.remote_id = RemoteId("a1:alias-b".into());
        store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![sent, alias_a, alias_b],
                    cursor: None,
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].message_count, 1);
        let msgs = store
            .thread_messages(&AccountId("a1".into()), &threads[0].thread_id)
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].folders, vec!["Sent", "INBOX"]);
        assert_eq!(msgs[0].labels, vec!["work", "alias"]);
        assert_eq!(
            msgs[0].arrived_via,
            vec!["me@x.com", "alias-a@x.com", "alias-b@x.com"]
        );
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "hello body")
                .unwrap()
                .hits
                .len(),
            1
        );
    }

    #[test]
    fn different_message_ids_do_not_collapse() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg("a@x", None, None, "Hello", "hello body"))
            .unwrap();
        store
            .upsert_message(&msg("b@x", None, None, "Hello", "hello body"))
            .unwrap();
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        assert_eq!(threads.len(), 2);
        assert!(threads.iter().all(|thread| thread.message_count == 1));
    }

    fn seed_search() -> Store {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store.upsert_account(&account_b()).unwrap();

        let mut ada = msg(
            "ada@msg",
            None,
            None,
            "March invoice",
            "please pay the march invoice",
        );
        ada.headers.from = Address {
            name: Some("Ada Lovelace".into()),
            email: "ada@x.com".into(),
        };
        ada.headers.date_unix = crate::search::parse_ymd("2024-03-02").unwrap() + 3600;
        ada.to.push(Address {
            name: Some("Bo Lee".into()),
            email: "bo@x.com".into(),
        });
        ada.cc.push(Address {
            name: None,
            email: "cc@x.com".into(),
        });
        ada.folders = vec!["INBOX".into()];
        ada.labels = vec!["work".into()];
        ada.unread = true;
        ada.has_attachment = true;

        let mut reply = msg(
            "reply-ada@msg",
            None,
            Some("ada@msg"),
            "Re: March invoice",
            "thanks will pay",
        );
        reply.headers.from = Address {
            name: Some("Bo Lee".into()),
            email: "bo@x.com".into(),
        };
        reply.headers.date_unix = crate::search::parse_ymd("2024-03-03").unwrap();
        reply.folders = vec!["INBOX".into()];
        reply.unread = false;

        let mut archived = msg(
            "other@msg",
            None,
            None,
            "Weekend plan",
            "the word work appears in the body but not as a label",
        );
        archived.headers.from = Address {
            name: Some("Cara".into()),
            email: "cara@x.com".into(),
        };
        archived.headers.date_unix = crate::search::parse_ymd("2024-04-01").unwrap();
        archived.to.push(Address {
            name: None,
            email: "ada@x.com".into(),
        });
        archived.folders = vec!["Archive".into()];
        archived.labels = vec!["workspace".into()];
        archived.unread = false;
        archived.snippet = archived.body_text.clone().unwrap_or_default();

        let mut quiet = msg("bcc@msg", None, None, "Quiet note", "secret plan");
        quiet.bcc.push(Address {
            name: None,
            email: "hidden@x.com".into(),
        });
        quiet.folders = vec!["INBOX".into()];
        quiet.unread = true;
        quiet.headers.date_unix = crate::search::parse_ymd("2024-03-04").unwrap();

        let mut foreign = msg_for(
            "a2",
            "foreign@msg",
            None,
            None,
            "March invoice",
            "please pay the march invoice",
        );
        foreign.folders = vec!["INBOX".into()];
        foreign.labels = vec!["work".into()];
        foreign.has_attachment = true;

        let mut subject_rank = msg("subj@msg", None, None, "zebraunique", "hello there");
        subject_rank.headers.date_unix = crate::search::parse_ymd("2024-05-01").unwrap();
        let mut body_rank = msg(
            "body@msg",
            None,
            None,
            "ordinary note",
            &format!("{} zebraunique", "padding ".repeat(40)),
        );
        body_rank.headers.date_unix = crate::search::parse_ymd("2024-05-02").unwrap();
        body_rank.snippet = "zebraunique".into();

        for message in [
            ada,
            reply,
            archived,
            quiet,
            foreign,
            subject_rank,
            body_rank,
        ] {
            store.upsert_message(&message).unwrap();
        }
        store
    }

    #[test]
    fn search_previews_and_message_navigation_use_the_indexed_hit_and_filters() {
        let store = seed_search();
        let account = AccountId("a1".into());
        let ranked = store.search(&account, "zebraunique").unwrap();
        let ids: Vec<_> = ranked
            .hits
            .iter()
            .map(|hit| hit.message_id.clone())
            .collect();
        let previews = store
            .search_previews(&account, "zebraunique", &ids)
            .unwrap();
        let body = &previews[&MessageId::new("body@msg")];
        assert!(body.text.starts_with('…'));
        assert_eq!(&body.text[body.highlights[0].clone()], "zebraunique");
        let subject = &previews[&MessageId::new("subj@msg")];
        assert_eq!(subject.text, "zebraunique");
        assert_eq!(subject.highlights, vec![0..11]);

        let thread = ThreadId::from_message_id(&MessageId::new("ada@msg"));
        let hits = store
            .search_thread_matches(&account, "invoice", &thread)
            .unwrap();
        assert_eq!(hits.len(), 2);
        let filtered = store
            .search_thread_matches(&account, "invoice is:unread", &thread)
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].message, MessageId::new("ada@msg"));
        let operator_only = store
            .search_thread_matches(&account, "label:work", &thread)
            .unwrap();
        assert_eq!(operator_only.len(), 1);
        assert_eq!(operator_only[0].message, filtered[0].message);
        assert!(operator_only[0].preview.is_none());
    }

    #[test]
    fn filtered_search_ranks_matching_messages_and_checks_dirty_folders() {
        let store = seed_search();
        let account = AccountId("a1".into());
        let thread = ThreadId::from_message_id(&MessageId::new("ada@msg"));
        let inbox = store.search(&account, "in:Inbox").unwrap();
        let winning_message = |hits: Vec<SearchHit>| {
            hits.into_iter()
                .find(|hit| hit.thread_id == thread)
                .unwrap()
                .message_id
        };
        assert_eq!(winning_message(inbox.hits), MessageId::new("reply-ada@msg"));

        store
            .conn
            .execute(
                "UPDATE messages SET folders = 'MÜNCHEN'
                 WHERE account_id = 'a1' AND message_id = 'reply-ada@msg'",
                [],
            )
            .unwrap();
        let inbox = store.search(&account, "in:Inbox").unwrap();
        assert_eq!(winning_message(inbox.hits), MessageId::new("ada@msg"));
        let munich = || store.search(&account, "in:München").unwrap();
        assert_eq!(
            winning_message(munich().hits),
            MessageId::new("reply-ada@msg")
        );

        immediate_transaction(&store.conn)
            .unwrap()
            .commit()
            .unwrap();
        assert_eq!(
            winning_message(munich().hits),
            MessageId::new("reply-ada@msg")
        );
        assert!(
            store
                .search(&account, "in:NoSuchFolder")
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn localized_folder_filter_scopes_actions_to_the_stored_name() {
        let store = Store::open_in_memory().unwrap();
        let account = account();
        store.upsert_account(&account).unwrap();
        let mut message = msg("munich@msg", None, None, "Train", "travel details");
        message.folders = vec!["INBOX".into(), "MÜNCHEN".into()];
        message.unread = true;
        let thread = store.upsert_message(&message).unwrap();
        let query = "in:München";
        let hits = store.search(&account.id, query).unwrap().hits;
        assert_eq!(hits.len(), 1);
        let with_text = "train in:münchen";
        assert_eq!(store.search(&account.id, with_text).unwrap().hits, hits);
        let message_id = hits[0].message_id.clone();
        assert!(
            store
                .search_previews(&account.id, with_text, std::slice::from_ref(&message_id))
                .unwrap()
                .contains_key(&message_id)
        );
        let matches = store
            .search_thread_matches(&account.id, with_text, &thread)
            .unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].message, message_id);
        let locations = || {
            store
                .search_locations(&account.id, query, "INBOX", &hits)
                .unwrap()
        };

        let before = locations();
        let scope = &before[&hits[0].message_id];
        assert_eq!(scope.folder.as_deref(), Some("MÜNCHEN"));
        assert!(scope.unread);

        let plain = "train";
        let plain_hits = store.search(&account.id, plain).unwrap().hits;
        assert_eq!(plain_hits, hits);
        let from_open_folder = store
            .search_locations(&account.id, plain, "München", &plain_hits)
            .unwrap();
        let scope = &from_open_folder[&message_id];
        assert_eq!(scope.folder.as_deref(), Some("MÜNCHEN"));
        assert!(scope.unread);
        store
            .archive_thread(&account.id, &thread, scope.folder.as_deref().unwrap())
            .unwrap();
        let messages = store.thread_messages(&account.id, &thread).unwrap();
        assert_eq!(messages[0].folders, ["INBOX", "Archive"]);
        assert_eq!(locations()[&hits[0].message_id].folder, None);
    }

    #[test]
    fn search_stars_follow_the_result_folder() {
        use crate::FlagChange;

        let store = Store::open_in_memory().unwrap();
        let account = account();
        store.upsert_account(&account).unwrap();
        let mut inbox = msg("inbox@msg", None, None, "Trip", "The original message");
        inbox.folders = vec!["INBOX".into()];
        inbox.starred = true;
        let thread = store.upsert_message(&inbox).unwrap();
        let mut archive = msg(
            "archive@msg",
            None,
            Some("inbox@msg"),
            "Re: Trip",
            "The reply",
        );
        archive.folders = vec!["Archive".into()];
        store.upsert_message(&archive).unwrap();
        let mut other = msg(
            "other@msg",
            None,
            Some("inbox@msg"),
            "Re: Trip",
            "Another reply",
        );
        other.folders = vec!["Archived".into()];
        other.starred = true;
        other.has_attachment = true;
        store.upsert_message(&other).unwrap();

        let location = |folder: &str| {
            let query = format!("in:{folder}");
            let hits = store.search(&account.id, &query).unwrap().hits;
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].thread_id, thread);
            let mut locations = store
                .search_locations(&account.id, &query, "INBOX", &hits)
                .unwrap();
            locations.remove(&hits[0].message_id).unwrap()
        };
        let archive = location("Archive");
        assert!(!archive.starred);
        assert!(archive.has_attachment);
        let target = [(thread.clone(), archive.folder.unwrap())];
        store
            .flag_scoped_threads(&account.id, &target, FlagChange::Star)
            .unwrap()
            .unwrap();
        assert!(location("Archive").starred);

        let unstar = store
            .flag_scoped_threads(&account.id, &target, FlagChange::Unstar)
            .unwrap()
            .unwrap();
        assert!(!location("Archive").starred);
        assert!(location("INBOX").starred);
        assert!(location("Archived").starred);
        assert_eq!(store.undo(&account.id).unwrap().id, unstar.id);
        assert!(location("Archive").starred);

        assert!(
            store
                .flag_scoped_threads(
                    &account.id,
                    &[(thread.clone(), "Missing".into())],
                    FlagChange::Unstar
                )
                .is_err()
        );
        assert!(location("Archive").starred);
        assert!(location("INBOX").starred);
        assert!(location("Archived").starred);
    }

    #[test]
    fn search_operator_suggestions_read_actual_account_values() {
        use crate::SearchField::*;
        let store = seed_search();
        let account = AccountId("a1".into());
        let suggest = |field, prefix| {
            store
                .search_suggestions(&account, field, prefix, 4)
                .unwrap()
        };
        assert_eq!(suggest(Folder, "ar"), ["Archive"]);
        assert_eq!(suggest(Label, "work"), ["work", "workspace"]);
        assert_eq!(suggest(Sender, "Ada Lo"), ["ada@x.com"]);
        assert_eq!(suggest(Recipient, "Bo L"), ["bo@x.com"]);
        assert_eq!(suggest(Recipient, "cc@"), ["cc@x.com"]);
        assert!(
            store
                .search_suggestions(&AccountId("a2".into()), Folder, "ar", 4)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn search_groups_threads_and_ranks_subject_over_body() {
        let store = seed_search();
        let account = AccountId("a1".into());
        let invoice = store.search(&account, "invoice").unwrap();
        assert!(invoice.did_you_mean.is_none());
        assert_eq!(invoice.hits.len(), 1);
        assert_eq!(
            invoice.hits[0].thread_id,
            ThreadId::from_message_id(&MessageId::new("ada@msg"))
        );
        let ranked = store.search(&account, "zebraunique").unwrap();
        assert_eq!(ranked.hits.len(), 2);
        assert_eq!(ranked.hits[0].message_id.as_str(), "subj@msg");
        assert_eq!(ranked.hits[1].message_id.as_str(), "body@msg");
    }

    #[test]
    fn search_operators_filter_the_same_message() {
        let store = seed_search();
        let account = AccountId("a1".into());
        let from_ada = store.search(&account, "from:ada invoice").unwrap();
        assert_eq!(from_ada.hits.len(), 1);
        assert!(
            store
                .search(&account, "from:cara invoice")
                .unwrap()
                .hits
                .is_empty()
        );
        assert_eq!(
            store
                .search(&account, "from:\"Ada Lovelace\"")
                .unwrap()
                .hits
                .len(),
            1
        );
        assert_eq!(store.search(&account, "to:bo@x.com").unwrap().hits.len(), 1);
        assert_eq!(store.search(&account, "to:cc@x.com").unwrap().hits.len(), 1);
        assert_eq!(
            store
                .search(&account, "to:hidden@x.com")
                .unwrap()
                .hits
                .len(),
            1
        );
        assert!(
            store
                .search(&account, "to:hidden@x.com from:cara")
                .unwrap()
                .hits
                .is_empty()
        );
        assert_eq!(
            store
                .search(&account, "subject:invoice")
                .unwrap()
                .hits
                .len(),
            1
        );
        assert!(
            store
                .search(&account, "subject:secret")
                .unwrap()
                .hits
                .is_empty()
        );
        assert_eq!(
            store
                .search(&account, "in:inbox label:work")
                .unwrap()
                .hits
                .len(),
            1
        );
        assert!(
            store
                .search(&account, "label:work")
                .unwrap()
                .hits
                .iter()
                .all(|hit| hit.message_id.as_str() != "other@msg")
        );
        assert!(
            store
                .search(&account, "in:inbox")
                .unwrap()
                .hits
                .iter()
                .all(|hit| hit.message_id.as_str() != "other@msg")
        );
        assert_eq!(
            store.search(&account, "has:attachment").unwrap().hits.len(),
            1
        );
        assert_eq!(
            store.search(&account, "has:attachment").unwrap().hits[0]
                .message_id
                .as_str(),
            "ada@msg"
        );
        let unread_work = store.search(&account, "is:unread label:work").unwrap();
        assert_eq!(unread_work.hits.len(), 1);
        assert_eq!(unread_work.hits[0].message_id.as_str(), "ada@msg");
        assert!(
            store
                .search(&account, "is:unread label:workspace")
                .unwrap()
                .hits
                .is_empty()
        );
        let march = store
            .search(&account, "after:2024-03-03 before:2024-03-05")
            .unwrap();
        let ids: Vec<_> = march
            .hits
            .iter()
            .map(|hit| hit.message_id.as_str())
            .collect();
        assert!(ids.contains(&"reply-ada@msg"));
        assert!(ids.contains(&"bcc@msg"));
        assert!(!ids.contains(&"ada@msg"));
        assert!(
            store
                .search(&account, "before:2024-03-03 after:2024-04-01")
                .unwrap()
                .hits
                .is_empty()
        );
        assert!(
            store
                .search(&account, "in:INBOX' OR '1'='1")
                .unwrap()
                .hits
                .is_empty()
        );
        let quoted = store.search(&account, "\"pay the march\"").unwrap();
        assert_eq!(quoted.hits.len(), 1);
        assert!(
            store
                .search(&account, "\"march the pay\"")
                .unwrap()
                .hits
                .is_empty()
        );
        assert_eq!(
            store.search(&account, "march the pay").unwrap().hits.len(),
            1
        );
    }

    #[test]
    fn search_stays_inside_the_account() {
        let store = seed_search();
        let local = store.search(&AccountId("a1".into()), "invoice").unwrap();
        let foreign = store.search(&AccountId("a2".into()), "invoice").unwrap();
        assert_eq!(local.hits.len(), 1);
        assert_eq!(foreign.hits.len(), 1);
        assert_eq!(foreign.hits[0].message_id.as_str(), "foreign@msg");
        assert_ne!(local.hits[0].message_id, foreign.hits[0].message_id);
    }

    #[test]
    fn trigram_did_you_mean_rewrites_a_miss_and_keeps_operators() {
        let store = seed_search();
        let account = AccountId("a1".into());
        let trigrams: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM search_trigrams WHERE account_id = 'a1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(trigrams > 0);
        let transposed = store.search(&account, "invioce").unwrap();
        assert_eq!(transposed.did_you_mean.as_deref(), Some("invoice"));
        assert_eq!(transposed.hits.len(), 1);
        let distant = store.search(&account, "invose").unwrap();
        assert_eq!(distant.did_you_mean.as_deref(), Some("invoice"));
        assert_eq!(distant.hits.len(), 1);
        let with_op = store.search(&account, "from:ada invose").unwrap();
        assert_eq!(with_op.did_you_mean.as_deref(), Some("from:ada invoice"));
        assert_eq!(with_op.hits.len(), 1);
        let flag = store.search(&account, "has:attachement").unwrap();
        assert_eq!(flag.did_you_mean.as_deref(), Some("has:attachment"));
        assert_eq!(flag.hits[0].message_id.as_str(), "ada@msg");
        let folder = store.search(&account, "in:inbx").unwrap();
        assert_eq!(folder.did_you_mean.as_deref(), Some("in:inbox"));
        assert!(!folder.hits.is_empty());
        let label = store.search(&account, "label:wrk").unwrap();
        assert_eq!(label.did_you_mean.as_deref(), Some("label:work"));
        assert_eq!(label.hits.len(), 1);
        assert!(store.search(&account, "").unwrap().hits.is_empty());
        assert!(
            store
                .search(&account, "from:ada invoice")
                .unwrap()
                .did_you_mean
                .is_none()
        );
    }

    #[test]
    fn search_follows_local_writes_without_a_backend() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut message = msg(
            "local@msg",
            None,
            None,
            "Alpha subject",
            "alphaonly uniquebody",
        );
        store.upsert_message(&message).unwrap();
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "uniquebody")
                .unwrap()
                .hits
                .len(),
            1
        );
        message.headers.subject = "Beta subject".into();
        message.body_text = Some("betaonly uniquebody".into());
        message.snippet = "betaonly uniquebody".into();
        store.upsert_message(&message).unwrap();
        assert!(
            store
                .search(&AccountId("a1".into()), "alphaonly")
                .unwrap()
                .hits
                .is_empty()
        );
        assert!(
            store
                .search(&AccountId("a1".into()), "subject:Alpha")
                .unwrap()
                .hits
                .is_empty()
        );
        let hits = store.search(&AccountId("a1".into()), "betaonly").unwrap();
        assert!(hits.did_you_mean.is_none());
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].message_id.as_str(), "local@msg");
    }

    #[test]
    fn same_id_unions_recipients_and_attachment_flag() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut first = msg("same@x", None, None, "Hello", "body");
        first.to.push(Address {
            name: Some("Ada".into()),
            email: "ada@x.com".into(),
        });
        let mut second = msg("same@x", None, None, "Hello", "body");
        second.cc.push(Address {
            name: None,
            email: "bob@x.com".into(),
        });
        second.has_attachment = true;
        store.upsert_message(&first).unwrap();
        store.upsert_message(&second).unwrap();
        let threads = store.list_threads(&AccountId("a1".into())).unwrap();
        let msgs = store
            .thread_messages(&AccountId("a1".into()), &threads[0].thread_id)
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].to, first.to);
        assert_eq!(msgs[0].cc[0].email, "bob@x.com");
        assert!(msgs[0].has_attachment);
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "to:bob@x.com")
                .unwrap()
                .hits
                .len(),
            1
        );
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "has:attachment")
                .unwrap()
                .hits
                .len(),
            1
        );
    }

    #[test]
    fn search_reads_a_database_from_before_recipient_columns() {
        let path = std::env::temp_dir().join(format!(
            "fell-m2-migrate-{}-pre-recipients.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '1');
                 CREATE TABLE accounts (
                    id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL,
                    email TEXT NOT NULL,
                    backend_kind TEXT NOT NULL
                 );
                 CREATE TABLE messages (
                    rowid INTEGER PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    remote_id TEXT NOT NULL,
                    thread_id TEXT NOT NULL,
                    proton_conversation_id TEXT,
                    in_reply_to TEXT,
                    references_header TEXT NOT NULL,
                    from_name TEXT,
                    from_email TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    date_unix INTEGER NOT NULL,
                    snippet TEXT NOT NULL,
                    body_text TEXT,
                    unread INTEGER NOT NULL DEFAULT 1,
                    folders TEXT NOT NULL DEFAULT '',
                    labels TEXT NOT NULL DEFAULT '',
                    arrived_via TEXT NOT NULL DEFAULT '',
                    UNIQUE (account_id, message_id)
                 );
                 CREATE VIRTUAL TABLE messages_fts USING fts5(
                    subject, sender, body, account_id UNINDEXED, message_id UNINDEXED
                 );
                 INSERT INTO accounts (id, display_name, email, backend_kind)
                 VALUES ('a1', 'Caleb', 'c@x', 'bridge');
                 INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread, folders
                 ) VALUES (
                    'a1', 'old@x', 'r1', 'msgid:old@x', '',
                    'ada@x.com', 'March invoice', 10, 'please pay', 'please pay the invoice', 1, 'INBOX'
                 );",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let hits = store.search(&AccountId("a1".into()), "invioce").unwrap();
        assert_eq!(hits.did_you_mean.as_deref(), Some("invoice"));
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].message_id.as_str(), "old@x");
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "in:inbox from:ada")
                .unwrap()
                .hits
                .len(),
            1
        );
        let version: String = store
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_fts_index_is_refilled_on_open() {
        let path = std::env::temp_dir().join(format!(
            "fell-m2-empty-fts-{}-refill.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let store = Store::open(&path).unwrap();
            store.upsert_account(&account()).unwrap();
            store
                .upsert_message(&msg(
                    "kept@x",
                    None,
                    None,
                    "March invoice",
                    "please pay the invoice",
                ))
                .unwrap();
            store.conn.execute("DELETE FROM messages_fts", []).unwrap();
            assert!(
                store
                    .search(&AccountId("a1".into()), "invoice")
                    .unwrap()
                    .hits
                    .is_empty()
            );
        }
        let store = Store::open(&path).unwrap();
        let hits = store.search(&AccountId("a1".into()), "invoice").unwrap();
        assert_eq!(hits.hits.len(), 1);
        assert_eq!(hits.hits[0].message_id.as_str(), "kept@x");
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "is:unread")
                .unwrap()
                .hits
                .len(),
            1
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn vocabulary_rebuild_retries_after_a_partial_fill() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg("a@x", None, None, "Invoice", "please pay the invoice"))
            .unwrap();
        store
            .upsert_message(&msg("b@x", None, None, "Calendar", "team calendar"))
            .unwrap();
        store
            .conn
            .execute("DELETE FROM search_trigrams", [])
            .unwrap();
        store.conn.execute("DELETE FROM search_terms", []).unwrap();

        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::search::testing_stop_vocabulary_rebuild_at(None);
            }
        }
        let _reset = Reset;
        crate::search::testing_stop_vocabulary_rebuild_at(Some(1));
        let err = crate::search::rebuild_vocabulary_if_empty(&store.conn).unwrap_err();
        assert!(matches!(err, crate::Error::Search(_)));
        let terms: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM search_terms", [], |row| row.get(0))
            .unwrap();
        assert_eq!(terms, 0);
        crate::search::testing_stop_vocabulary_rebuild_at(None);
        crate::search::rebuild_vocabulary_if_empty(&store.conn).unwrap();
        let hits = store.search(&AccountId("a1".into()), "invioce").unwrap();
        assert_eq!(hits.did_you_mean.as_deref(), Some("invoice"));
        assert_eq!(hits.hits.len(), 1);
    }

    #[test]
    fn deleting_mail_drops_its_terms_from_did_you_mean() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg(
                "gone@x",
                None,
                None,
                "Invoice",
                "please pay the invoice",
            ))
            .unwrap();
        store
            .upsert_message(&msg("stay@x", None, None, "Calendar", "team calendar"))
            .unwrap();
        assert!(
            store
                .delete_message(&AccountId("a1".into()), &MessageId::new("gone@x"))
                .unwrap()
        );
        let invoice_terms: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM search_terms WHERE term = 'invoice'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let invoice_trigrams: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM search_trigrams WHERE term = 'invoice'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(invoice_terms, 0);
        assert_eq!(invoice_trigrams, 0);
        let missed = store.search(&AccountId("a1".into()), "invioce").unwrap();
        assert_ne!(missed.did_you_mean.as_deref(), Some("invoice"));
        assert!(missed.hits.is_empty());
        let live = store.search(&AccountId("a1".into()), "calendr").unwrap();
        assert_eq!(live.did_you_mean.as_deref(), Some("calendar"));
        assert_eq!(live.hits[0].message_id.as_str(), "stay@x");

        store
            .upsert_message(&msg("c1@x", None, None, "Budget", "sharedbudget"))
            .unwrap();
        store
            .upsert_message(&msg("c2@x", None, None, "Budget two", "sharedbudget"))
            .unwrap();
        store
            .delete_message(&AccountId("a1".into()), &MessageId::new("c1@x"))
            .unwrap();
        let df: i64 = store
            .conn
            .query_row(
                "SELECT df FROM search_terms WHERE account_id = 'a1' AND term = 'sharedbudget'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(df, 1);
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "sharedbudget")
                .unwrap()
                .hits
                .len(),
            1
        );
        assert!(
            !store
                .delete_message(&AccountId("a1".into()), &MessageId::new("missing@x"))
                .unwrap()
        );
    }

    #[test]
    fn search_page_returns_twenty_then_the_rest() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        for n in 0..25 {
            store
                .upsert_message(&msg(&format!("m{n}@x"), None, None, "Paged", "pageword"))
                .unwrap();
        }
        let first = store
            .search_page(&AccountId("a1".into()), "pageword", None)
            .unwrap();
        assert_eq!(first.hits.len(), 20);
        let cursor = first.next_cursor.expect("next page");
        let second = store
            .search_page(&AccountId("a1".into()), "pageword", Some(&cursor))
            .unwrap();
        assert_eq!(second.hits.len(), 5);
        assert!(second.next_cursor.is_none());
        let mut ids: Vec<_> = first
            .hits
            .iter()
            .chain(second.hits.iter())
            .map(|hit| hit.message_id.as_str().to_string())
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 25);
        assert!(
            store
                .search_page(&AccountId("a1".into()), "pageword", Some("nope"))
                .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn database_wal_and_shm_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!(
            "fell-store-mode-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.sqlite");
        let store = Store::open(&path).unwrap();
        store.upsert_account(&account()).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let file = if suffix.is_empty() {
                path.clone()
            } else {
                let mut name = path.as_os_str().to_os_string();
                name.push(suffix);
                std::path::PathBuf::from(name)
            };
            assert!(file.exists(), "missing {}", file.display());
            let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "{}", file.display());
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migrates_archive_columns_onto_existing_messages() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                rowid INTEGER PRIMARY KEY,
                account_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                remote_id TEXT NOT NULL,
                thread_id TEXT NOT NULL,
                proton_conversation_id TEXT,
                in_reply_to TEXT,
                references_header TEXT NOT NULL,
                from_name TEXT,
                from_email TEXT NOT NULL,
                subject TEXT NOT NULL,
                date_unix INTEGER NOT NULL,
                snippet TEXT NOT NULL,
                body_text TEXT,
                unread INTEGER NOT NULL DEFAULT 1,
                folders TEXT NOT NULL DEFAULT '',
                labels TEXT NOT NULL DEFAULT '',
                arrived_via TEXT NOT NULL DEFAULT ''
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (
                account_id, message_id, remote_id, thread_id, references_header,
                from_email, subject, date_unix, snippet, unread
             ) VALUES ('a', 'm@x', 'r', 't', '', 'f@x', 's', 1, 'snip', 1)",
            [],
        )
        .unwrap();
        ensure_message_columns(&conn).unwrap();
        let names = column_names(&conn, "messages").unwrap();
        assert!(names.iter().any(|name| name == "archived"));
        assert!(names.iter().any(|name| name == "snooze_until"));
        let archived: i64 = conn
            .query_row("SELECT archived FROM messages", [], |row| row.get(0))
            .unwrap();
        let snooze: Option<i64> = conn
            .query_row("SELECT snooze_until FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(archived, 0);
        assert!(snooze.is_none());
    }

    #[test]
    fn concurrent_column_migration_succeeds() {
        let dir = std::env::temp_dir().join(format!(
            "fell-migrate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                rowid INTEGER PRIMARY KEY,
                account_id TEXT NOT NULL,
                message_id TEXT NOT NULL
            );",
        )
        .unwrap();
        drop(conn);
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let path = path.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                let conn = rusqlite::Connection::open(&path).unwrap();
                conn.busy_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                barrier.wait();
                ensure_message_columns(&conn).unwrap();
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        let conn = rusqlite::Connection::open(&path).unwrap();
        let names = column_names(&conn, "messages").unwrap();
        assert!(names.iter().any(|name| name == "archived"));
        assert!(names.iter().any(|name| name == "snooze_until"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn listed(store: &Store) -> Vec<Message> {
        store
            .list_threads(&AccountId("a1".into()))
            .unwrap()
            .into_iter()
            .flat_map(|thread| {
                store
                    .thread_messages(&AccountId("a1".into()), &thread.thread_id)
                    .unwrap()
            })
            .collect()
    }

    #[test]
    fn list_rewritten_headers_collapse_on_body_fingerprint() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut sent = with_arrival(
            msg("orig@x", None, None, "Hello", "hello body"),
            "Sent",
            "",
            "me@x.com",
        );
        sent.to.push(Address {
            name: Some("Ada".into()),
            email: "ada@x.com".into(),
        });
        let sent_thread = store.upsert_message(&sent).unwrap();
        let mut echo = with_arrival(
            msg(
                "list-rewrite@x",
                None,
                None,
                "[list] Hello",
                "hello   body\n",
            ),
            "INBOX",
            "lists",
            "list@x.com",
        );
        echo.list_id = Some("list.example".into());
        echo.to.push(Address {
            name: None,
            email: "bob@x.com".into(),
        });
        assert_eq!(store.upsert_message(&echo).unwrap(), sent_thread);
        let collapsed = listed(&store);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].headers.message_id.as_str(), "orig@x");
        assert_eq!(collapsed[0].headers.subject, "Hello");
        assert_eq!(collapsed[0].folders, vec!["Sent", "INBOX"]);
        assert_eq!(collapsed[0].labels, vec!["lists"]);
        assert_eq!(collapsed[0].arrived_via, vec!["me@x.com", "list@x.com"]);
        assert_eq!(collapsed[0].list_id.as_deref(), Some("list.example"));
        assert_eq!(collapsed[0].to[0].email, "ada@x.com");
        assert_eq!(collapsed[0].to[1].email, "bob@x.com");

        let mut again = echo.clone();
        again.headers.subject = "[list] Hello REWRITE".into();
        again.to.clear();
        again.body_text = None;
        store.upsert_message(&again).unwrap();
        let collapsed = listed(&store);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].headers.subject, "Hello");
        assert_eq!(collapsed[0].body_text.as_deref(), Some("hello body"));
        assert_eq!(collapsed[0].to[0].email, "ada@x.com");
        assert_eq!(collapsed[0].to[1].email, "bob@x.com");

        let reply_thread = store
            .upsert_message(&msg(
                "reply@x",
                None,
                Some("list-rewrite@x"),
                "Re: Hello",
                "a reply",
            ))
            .unwrap();
        assert_eq!(reply_thread, sent_thread);
        assert_eq!(listed(&store).len(), 2);
    }

    #[test]
    fn list_copy_then_sent_keeps_non_list_subject_and_proton_id() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut echo = with_arrival(
            msg("list-rewrite@x", None, None, "[list] Hello", "hello body"),
            "INBOX",
            "lists",
            "list@x.com",
        );
        echo.list_id = Some("list.example".into());
        store.upsert_message(&echo).unwrap();
        let sent = with_arrival(
            msg(
                "orig@x",
                Some("conv-1"),
                Some("root@x"),
                "Hello",
                "hello   body\n",
            ),
            "Sent",
            "",
            "me@x.com",
        );
        store
            .upsert_message(&msg("root@x", Some("conv-1"), None, "Root", "root body"))
            .unwrap();
        let sent_thread = store.upsert_message(&sent).unwrap();
        assert_eq!(sent_thread, ThreadId::proton("conv-1"));
        let collapsed = listed(&store)
            .into_iter()
            .find(|message| message.list_id.as_deref() == Some("list.example"))
            .unwrap();
        assert_eq!(collapsed.headers.subject, "Hello");
        assert_eq!(
            collapsed.headers.proton_conversation_id.as_deref(),
            Some("conv-1")
        );
        assert_eq!(collapsed.body_text.as_deref(), Some("hello body"));
        assert_eq!(
            collapsed
                .headers
                .in_reply_to
                .as_ref()
                .map(MessageId::as_str),
            Some("root@x")
        );

        echo.body_text = None;
        echo.headers.in_reply_to = None;
        echo.headers.references.clear();
        echo.headers.proton_conversation_id = None;
        echo.headers.subject = "[list] wiped".into();
        echo.unread = false;
        store.upsert_message(&echo).unwrap();
        let collapsed = listed(&store)
            .into_iter()
            .find(|message| message.list_id.as_deref() == Some("list.example"))
            .unwrap();
        assert_eq!(collapsed.headers.subject, "Hello");
        assert_eq!(collapsed.body_text.as_deref(), Some("hello body"));
        assert_eq!(
            collapsed
                .headers
                .in_reply_to
                .as_ref()
                .map(MessageId::as_str),
            Some("root@x")
        );
        assert_eq!(
            collapsed.headers.proton_conversation_id.as_deref(),
            Some("conv-1")
        );
        assert!(!collapsed.unread);
        assert_eq!(
            store
                .thread_messages(&AccountId("a1".into()), &ThreadId::proton("conv-1"))
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn folder_threads_include_sent_copies_and_skip_other_folders() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut root = msg("root@x", None, None, "Threading", "root body");
        root.folders = vec!["INBOX".into()];
        root.headers.from.name = Some("Jon".into());
        root.headers.from.email = "jon@x".into();
        root.headers.date_unix = 10;
        root.unread = false;
        root.labels = vec!["work".into()];
        let mut reply = msg(
            "reply@x",
            None,
            Some("root@x"),
            "Re: Threading",
            "sent body",
        );
        reply.folders = vec!["Sent".into()];
        reply.headers.from.name = Some("Caleb".into());
        reply.headers.from.email = "c@x".into();
        reply.headers.date_unix = 20;
        reply.snippet = "sent body".into();
        reply.starred = true;
        reply.has_attachment = true;
        let mut draft = msg("draft@x", None, None, "Draft", "only a draft");
        draft.folders = vec!["Drafts".into()];
        draft.headers.date_unix = 30;
        store.upsert_message(&root).unwrap();
        store.upsert_message(&reply).unwrap();
        store.upsert_message(&draft).unwrap();

        let inbox = store
            .folder_threads(&AccountId("a1".into()), "inbox")
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].message_count, 2);
        assert_eq!(inbox[0].snippet, "sent body");
        assert!(inbox[0].starred);
        assert!(inbox[0].has_attachment);
        assert_eq!(inbox[0].labels, vec!["work".to_string()]);
        assert_eq!(inbox[0].latest_email, "c@x");
        assert_eq!(inbox[0].participants.len(), 2);
        let sent = store
            .folder_threads(&AccountId("a1".into()), "Sent")
            .unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].thread_id, inbox[0].thread_id);
        assert!(
            store
                .folder_threads(&AccountId("a1".into()), "Drafts")
                .unwrap()
                .len()
                == 1
        );
        let msgs = store
            .thread_messages(&AccountId("a1".into()), &inbox[0].thread_id)
            .unwrap();
        assert!(msgs.iter().any(|message| message.folders == ["Sent"]));
        assert!(msgs.iter().any(|message| message.starred));
    }

    #[test]
    fn archived_inbox_only_mail_is_not_inbox() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut kept = msg("kept@x", None, None, "Still here", "inbox body");
        kept.folders = vec!["INBOX".into()];
        let mut archived = msg("gone@x", None, None, "Archived", "gone body");
        archived.folders = vec!["INBOX".into()];
        archived.archived = true;
        store.upsert_message(&kept).unwrap();
        store.upsert_message(&archived).unwrap();

        let inbox = store
            .folder_threads(&AccountId("a1".into()), "INBOX")
            .unwrap();
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].subject, "Still here");
        assert!(
            store
                .folder_threads(&AccountId("a1".into()), "Archive")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn junk_mail_opens_with_spam() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut junk = msg("junk@x", None, None, "Filtered", "junk body");
        junk.folders = vec!["Junk".into()];
        store.upsert_message(&junk).unwrap();
        let spam = store
            .folder_threads(&AccountId("a1".into()), "Spam")
            .unwrap();
        assert_eq!(spam.len(), 1);
        assert_eq!(spam[0].subject, "Filtered");
        assert!(
            store
                .folder_threads(&AccountId("a1".into()), "INBOX")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn sent_parent_survives_list_rewritten_in_reply_to() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let parent_thread = store
            .upsert_message(&msg("root@x", None, None, "Root", "root body"))
            .unwrap();
        let mut echo = with_arrival(
            msg(
                "list-rewrite@x",
                None,
                Some("rewritten@x"),
                "[list] Hello",
                "hello body",
            ),
            "INBOX",
            "lists",
            "list@x.com",
        );
        echo.list_id = Some("list.example".into());
        store.upsert_message(&echo).unwrap();
        let sent = with_arrival(
            msg("orig@x", None, Some("root@x"), "Hello", "hello body"),
            "Sent",
            "",
            "me@x.com",
        );
        assert_eq!(store.upsert_message(&sent).unwrap(), parent_thread);
        let collapsed = listed(&store)
            .into_iter()
            .find(|message| message.list_id.is_some())
            .unwrap();
        assert_eq!(
            collapsed
                .headers
                .in_reply_to
                .as_ref()
                .map(MessageId::as_str),
            Some("root@x")
        );
        assert!(
            collapsed
                .headers
                .references
                .iter()
                .any(|id| id.as_str() == "rewritten@x")
        );
        assert_eq!(collapsed.body_text.as_deref(), Some("hello body"));

        echo.body_text = Some("rewritten list body".into());
        echo.snippet = "rewritten list body".into();
        echo.headers.in_reply_to = Some(MessageId::new("rewritten@x"));
        echo.headers.references = vec![MessageId::new("rewritten@x")];
        store.upsert_message(&echo).unwrap();
        let collapsed = listed(&store)
            .into_iter()
            .find(|message| message.list_id.is_some())
            .unwrap();
        assert_eq!(
            collapsed
                .headers
                .in_reply_to
                .as_ref()
                .map(MessageId::as_str),
            Some("root@x")
        );
        assert_eq!(collapsed.body_text.as_deref(), Some("hello body"));
        assert_eq!(collapsed.headers.subject, "Hello");
    }

    #[test]
    fn filled_body_replaces_a_subject_snippet() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut echo = msg("list-rewrite@x", None, None, "[list] Hello", "hello body");
        echo.list_id = Some("list.example".into());
        echo.snippet = "[list] Hello".into();
        store.upsert_message(&echo).unwrap();
        let mut sent = msg("orig@x", None, None, "Hello", "hello body");
        sent.snippet = "plain preview".into();
        store.upsert_message(&sent).unwrap();
        store
            .conn
            .execute(
                "UPDATE messages SET body_text = NULL WHERE message_id = 'list-rewrite@x'",
                [],
            )
            .unwrap();
        store.upsert_message(&sent).unwrap();
        let collapsed = listed(&store);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].body_text.as_deref(), Some("hello body"));
        assert_eq!(collapsed[0].snippet, "plain preview");
    }

    #[test]
    fn same_sender_outside_echo_window_stays_distinct() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut first = msg("week-a@x", None, None, "Digest", "same weekly body");
        first.headers.date_unix = 1_700_000_000;
        let mut second = msg("week-b@x", None, None, "[list] Digest", "same weekly body");
        second.list_id = Some("list.example".into());
        second.headers.date_unix = first.headers.date_unix + (7 * 24 * 60 * 60);
        store.upsert_message(&first).unwrap();
        store.upsert_message(&second).unwrap();
        assert_eq!(listed(&store).len(), 2);

        let mut near = msg("near@x", None, None, "Hello", "near body");
        near.headers.date_unix = 1_700_000_000;
        near.folders = vec!["INBOX".into()];
        let near_thread = store.upsert_message(&near).unwrap();
        let mut echo = msg("near-list@x", None, None, "unrelated subject", "near body");
        echo.list_id = Some("list.example".into());
        echo.headers.date_unix = near.headers.date_unix + (5 * 60 * 60);
        echo.folders = vec!["INBOX".into()];
        let echo_thread = store.upsert_message(&echo).unwrap();
        // Not a Sent copy, and the subject key differs, so the list post stays.
        assert_ne!(echo_thread, near_thread);
        assert_eq!(listed(&store).len(), 4);
    }

    /// Inbox "Sounds good to me" on Patch, and a list post of that body on
    /// another subject and parent. Both orders, then a resync of each id.
    fn deliver_sounds_good(list_first: bool) -> Store {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let at = 1_700_000_000;
        store
            .upsert_message(&msg("patch@x", None, None, "Patch", "patch notes"))
            .unwrap();
        store
            .upsert_message(&msg("budget@x", None, None, "Budget", "budget notes"))
            .unwrap();
        let mut inbox = msg(
            "sounds-inbox@x",
            None,
            Some("patch@x"),
            "Re: Patch",
            "Sounds good to me",
        );
        inbox.headers.date_unix = at;
        inbox.folders = vec!["INBOX".into()];
        let mut list = msg(
            "sounds-list@x",
            None,
            Some("budget@x"),
            "[list] Re: Budget",
            "Sounds good to me",
        );
        list.list_id = Some("list.example".into());
        list.headers.date_unix = at + 10 * 60;
        list.folders = vec!["INBOX".into()];
        if list_first {
            store.upsert_message(&list).unwrap();
            store.upsert_message(&inbox).unwrap();
        } else {
            store.upsert_message(&inbox).unwrap();
            store.upsert_message(&list).unwrap();
        }
        store
    }

    fn assert_sounds_good_keeps_both_threads(store: &Store) {
        let rows = listed(store);
        let inbox = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "sounds-inbox@x")
            .unwrap();
        let list = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "sounds-list@x")
            .unwrap();
        let patch = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "patch@x")
            .unwrap();
        let budget = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "budget@x")
            .unwrap();
        assert_eq!(
            inbox.headers.in_reply_to.as_ref().map(MessageId::as_str),
            Some("patch@x")
        );
        assert_eq!(
            list.headers.in_reply_to.as_ref().map(MessageId::as_str),
            Some("budget@x")
        );
        assert_eq!(inbox.thread_id, patch.thread_id);
        assert_eq!(list.thread_id, budget.thread_id);
        assert_ne!(inbox.thread_id, list.thread_id);
    }

    #[test]
    fn long_body_list_post_keeps_its_thread_beside_a_different_inbox_message() {
        for list_first in [false, true] {
            let store = deliver_sounds_good(list_first);
            assert_sounds_good_keeps_both_threads(&store);
        }
    }

    #[test]
    fn resync_does_not_absorb_a_long_body_list_post_into_a_different_inbox() {
        for list_first in [false, true] {
            let store = deliver_sounds_good(list_first);
            let at = 1_700_000_000;
            let mut inbox = msg(
                "sounds-inbox@x",
                None,
                Some("patch@x"),
                "Re: Patch",
                "Sounds good to me",
            );
            inbox.headers.date_unix = at;
            inbox.folders = vec!["INBOX".into()];
            let mut list = msg(
                "sounds-list@x",
                None,
                Some("budget@x"),
                "[list] Re: Budget",
                "Sounds good to me",
            );
            list.list_id = Some("list.example".into());
            list.headers.date_unix = at + 10 * 60;
            list.folders = vec!["INBOX".into()];
            store.upsert_message(&inbox).unwrap();
            store.upsert_message(&list).unwrap();
            assert_sounds_good_keeps_both_threads(&store);
        }
    }

    #[test]
    fn long_body_same_subject_different_parent_stays_apart() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let at = 1_700_000_000;
        store
            .upsert_message(&msg("patch@x", None, None, "Patch", "patch notes"))
            .unwrap();
        store
            .upsert_message(&msg("other@x", None, None, "Other", "other notes"))
            .unwrap();
        let mut inbox = msg(
            "inbox@x",
            None,
            Some("patch@x"),
            "Re: Patch",
            "Sounds good to me",
        );
        inbox.headers.date_unix = at;
        inbox.folders = vec!["INBOX".into()];
        let mut list = msg(
            "list@x",
            None,
            Some("other@x"),
            "[list] Re: Patch",
            "Sounds good to me",
        );
        list.list_id = Some("list.example".into());
        list.headers.date_unix = at + 10 * 60;
        list.folders = vec!["INBOX".into()];
        store.upsert_message(&list).unwrap();
        store.upsert_message(&inbox).unwrap();
        store.upsert_message(&list).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 4);
        let inbox_row = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "inbox@x")
            .unwrap();
        let list_row = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "list@x")
            .unwrap();
        assert_eq!(
            inbox_row
                .headers
                .in_reply_to
                .as_ref()
                .map(MessageId::as_str),
            Some("patch@x")
        );
        assert_eq!(
            list_row.headers.in_reply_to.as_ref().map(MessageId::as_str),
            Some("other@x")
        );
        assert_ne!(inbox_row.thread_id, list_row.thread_id);
    }

    /// Sent copy A and list copy B share a long body and a sender. The subjects
    /// do not. A different parent is allowed on the Sent path; the subject is not.
    fn sent_a_and_list_b() -> (Message, Message) {
        let at = 1_700_000_000;
        let mut sent = msg(
            "sent-a@x",
            None,
            Some("patch@x"),
            "Re: Patch",
            "Sounds good to me",
        );
        sent.headers.date_unix = at;
        sent.folders = vec!["Sent".into()];
        let mut list = msg(
            "list-b@x",
            None,
            Some("budget@x"),
            "[list] Re: Budget",
            "Sounds good to me",
        );
        list.list_id = Some("list.example".into());
        list.headers.date_unix = at + 10 * 60;
        list.folders = vec!["INBOX".into()];
        (sent, list)
    }

    fn message_row_present(store: &Store, message_id: &str) -> bool {
        store
            .conn
            .query_row(
                "SELECT 1 FROM messages WHERE account_id = 'a1' AND message_id = ?1",
                params![message_id],
                |row| row.get::<_, i64>(0),
            )
            .is_ok()
    }

    fn alias_target(store: &Store, message_id: &str) -> Option<String> {
        store
            .conn
            .query_row(
                "SELECT canonical_message_id FROM message_aliases
                 WHERE account_id = 'a1' AND message_id = ?1",
                params![message_id],
                |row| row.get(0),
            )
            .ok()
    }

    #[test]
    fn long_list_copy_second_does_not_alias_a_sent_copy_on_another_subject() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let (sent, list) = sent_a_and_list_b();
        store.upsert_message(&sent).unwrap();
        store.upsert_message(&list).unwrap();
        assert!(message_row_present(&store, "sent-a@x"));
        assert!(message_row_present(&store, "list-b@x"));
        assert_eq!(alias_target(&store, "list-b@x"), None);
    }

    #[test]
    fn long_list_copy_first_does_not_fold_in_a_sent_copy_on_another_subject() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let (sent, list) = sent_a_and_list_b();
        store.upsert_message(&list).unwrap();
        store.upsert_message(&sent).unwrap();
        assert!(message_row_present(&store, "list-b@x"));
        assert!(message_row_present(&store, "sent-a@x"));
        assert_eq!(alias_target(&store, "sent-a@x"), None);
    }

    #[test]
    fn resync_does_not_delete_a_sent_copy_beside_a_list_post_on_another_subject() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let fingerprint = crate::message::body_fingerprint("Sounds good to me");
        let at = 1_700_000_000_i64;
        for (id, subject, parent, folder, list_id, date) in [
            ("sent-a@x", "Re: Patch", "patch@x", "Sent", None, at),
            (
                "list-b@x",
                "[list] Re: Budget",
                "budget@x",
                "INBOX",
                Some("list.example"),
                at + 10 * 60,
            ),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO messages (
                        account_id, message_id, remote_id, thread_id, in_reply_to,
                        references_header, from_email, subject, date_unix, snippet, body_text,
                        unread, folders, body_fingerprint, list_id
                     ) VALUES (
                        'a1', ?1, ?1, ?2, ?3, ?3, 'from@x', ?4, ?5, 'Sounds good to me',
                        'Sounds good to me', 1, ?6, ?7, ?8
                     )",
                    params![
                        id,
                        format!("msgid:{id}"),
                        parent,
                        subject,
                        date,
                        folder,
                        fingerprint,
                        list_id
                    ],
                )
                .unwrap();
        }
        let mut again = msg(
            "sent-a@x",
            None,
            Some("patch@x"),
            "Re: Patch",
            "Sounds good to me",
        );
        again.headers.date_unix = at;
        again.folders = vec!["Sent".into()];
        store.upsert_message(&again).unwrap();
        assert!(message_row_present(&store, "sent-a@x"));
        assert!(message_row_present(&store, "list-b@x"));
    }

    #[test]
    fn a_list_parent_rewritten_to_the_list_copy_of_the_same_parent_still_joins() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let root_thread = store
            .upsert_message(&msg("patch@x", None, None, "Patch", "patch notes"))
            .unwrap();
        // The list's own copy of the parent, under the id the list gave it.
        let mut root_copy = msg(
            "patch-list@x",
            None,
            Some("patch@x"),
            "[list] Patch",
            "patch notes as the list delivered them",
        );
        root_copy.list_id = Some("list.example".into());
        assert_eq!(store.upsert_message(&root_copy).unwrap(), root_thread);
        let at = 1_700_000_000;
        let mut sent = msg(
            "sent-a@x",
            None,
            Some("patch@x"),
            "Re: Patch",
            "Looks right to me, merging this afternoon.",
        );
        sent.headers.date_unix = at;
        sent.folders = vec!["Sent".into()];
        let mut echo = msg(
            "echo@x",
            None,
            Some("patch-list@x"),
            "[list] Re: Patch",
            "Looks right to me, merging this afternoon.",
        );
        echo.list_id = Some("list.example".into());
        echo.headers.date_unix = at + 60;
        echo.folders = vec!["INBOX".into()];
        store.upsert_message(&sent).unwrap();
        store.upsert_message(&echo).unwrap();
        assert_eq!(alias_target(&store, "echo@x").as_deref(), Some("sent-a@x"));
        assert!(!message_row_present(&store, "echo@x"));
    }

    /// Insert a row as an older Fell stored it, before the copies were joined.
    fn insert_raw_row(
        store: &Store,
        (id, subject, parent, folder, list_id, date): (&str, &str, &str, &str, Option<&str>, i64),
    ) {
        let fingerprint = crate::message::body_fingerprint("Sounds good to me");
        store
            .conn
            .execute(
                "INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, in_reply_to,
                    references_header, from_email, subject, date_unix, snippet, body_text,
                    unread, folders, body_fingerprint, list_id
                 ) VALUES (
                    'a1', ?1, ?1, ?2, ?3, ?3, 'from@x', ?4, ?5, 'Sounds good to me',
                    'Sounds good to me', 1, ?6, ?7, ?8
                 )",
                params![
                    id,
                    format!("msgid:{id}"),
                    parent,
                    subject,
                    date,
                    folder,
                    fingerprint,
                    list_id
                ],
            )
            .unwrap();
    }

    #[test]
    fn resync_from_three_rows_joins_only_the_send_and_its_own_echo() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let at = 1_700_000_000_i64;
        insert_raw_row(
            &store,
            ("sent-a@x", "Re: Patch", "patch@x", "Sent", None, at),
        );
        insert_raw_row(
            &store,
            (
                "sent-b@x",
                "Re: Budget",
                "budget@x",
                "Sent",
                None,
                at + 5 * 60,
            ),
        );
        insert_raw_row(
            &store,
            (
                "list-b@x",
                "[list] Re: Budget",
                "budget@x",
                "INBOX",
                Some("list.example"),
                at + 10 * 60,
            ),
        );
        for (id, subject, parent, folder, date) in [
            ("sent-a@x", "Re: Patch", "patch@x", "Sent", at),
            ("sent-b@x", "Re: Budget", "budget@x", "Sent", at + 5 * 60),
        ] {
            let mut again = msg(id, None, Some(parent), subject, "Sounds good to me");
            again.headers.date_unix = date;
            again.folders = vec![folder.into()];
            store.upsert_message(&again).unwrap();
        }
        // Sent A is another post and keeps its row. Sent B and its list echo
        // are one message: exactly one of the two rows stays.
        assert!(message_row_present(&store, "sent-a@x"));
        assert_eq!(alias_target(&store, "sent-a@x"), None);
        let b_rows = ["sent-b@x", "list-b@x"]
            .iter()
            .filter(|id| message_row_present(&store, id))
            .count();
        assert_eq!(b_rows, 1);
        assert_eq!(listed(&store).len(), 2);
    }

    #[test]
    fn a_sent_copy_reusing_the_list_posts_id_joins_that_post() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let (sent_a, list_b) = sent_a_and_list_b();
        store.upsert_message(&sent_a).unwrap();
        store.upsert_message(&list_b).unwrap();
        // The Sent original of B: the list kept its Message-ID.
        let mut sent_b = msg(
            "list-b@x",
            None,
            Some("budget@x"),
            "Re: Budget",
            "Sounds good to me",
        );
        sent_b.headers.date_unix = list_b.headers.date_unix - 60;
        sent_b.folders = vec!["Sent".into()];
        store.upsert_message(&sent_b).unwrap();
        assert!(message_row_present(&store, "sent-a@x"));
        assert!(message_row_present(&store, "list-b@x"));
        assert_eq!(alias_target(&store, "sent-a@x"), None);
        assert_eq!(alias_target(&store, "list-b@x"), None);
        let rows = listed(&store);
        assert_eq!(rows.len(), 2);
        let b = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "list-b@x")
            .unwrap();
        assert!(
            b.folders.iter().any(|folder| folder == "Sent"),
            "{:?}",
            b.folders
        );
        assert!(
            b.folders.iter().any(|folder| folder == "INBOX"),
            "{:?}",
            b.folders
        );
        let a = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "sent-a@x")
            .unwrap();
        assert_eq!(a.folders, vec!["Sent"]);
    }

    #[test]
    fn upgrade_keeps_a_sent_copy_beside_a_list_post_on_another_subject() {
        let path = std::env::temp_dir().join(format!(
            "fell-schema-v2-{}-sent-subject.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let fingerprint = crate::message::body_fingerprint("Sounds good to me");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '2');
                 CREATE TABLE accounts (
                    id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL,
                    email TEXT NOT NULL,
                    backend_kind TEXT NOT NULL
                 );
                 INSERT INTO accounts (id, display_name, email, backend_kind)
                    VALUES ('a1', 'Caleb', 'c@x', 'bridge');
                 CREATE TABLE messages (
                    rowid INTEGER PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    remote_id TEXT NOT NULL,
                    thread_id TEXT NOT NULL,
                    proton_conversation_id TEXT,
                    in_reply_to TEXT,
                    references_header TEXT NOT NULL,
                    from_name TEXT,
                    from_email TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    date_unix INTEGER NOT NULL,
                    snippet TEXT NOT NULL,
                    body_text TEXT,
                    unread INTEGER NOT NULL DEFAULT 1,
                    folders TEXT NOT NULL DEFAULT '',
                    labels TEXT NOT NULL DEFAULT '',
                    arrived_via TEXT NOT NULL DEFAULT '',
                    to_line TEXT NOT NULL DEFAULT '',
                    cc_line TEXT NOT NULL DEFAULT '',
                    bcc_line TEXT NOT NULL DEFAULT '',
                    starred INTEGER NOT NULL DEFAULT 0,
                    has_attachment INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0,
                    snooze_until INTEGER,
                    body_fingerprint TEXT NOT NULL DEFAULT '',
                    list_id TEXT,
                    UNIQUE (account_id, message_id)
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, in_reply_to, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread, folders,
                    body_fingerprint
                 ) VALUES (
                    'a1', 'sent-a@x', 'imap:sent', 'msgid:sent-a@x', 'patch@x', 'patch@x',
                    'from@x', 'Re: Patch', 1700000000, 'Sounds good to me', 'Sounds good to me',
                    1, 'Sent', ?1
                 )",
                params![fingerprint],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, in_reply_to, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread, folders,
                    body_fingerprint, list_id
                 ) VALUES (
                    'a1', 'list-b@x', 'imap:inbox', 'msgid:list-b@x', 'budget@x', 'budget@x',
                    'from@x', '[list] Re: Budget', 1700000600, 'Sounds good to me',
                    'Sounds good to me', 1, 'INBOX', ?1, 'list.example'
                 )",
                params![fingerprint],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert!(message_row_present(&store, "sent-a@x"));
        assert!(message_row_present(&store, "list-b@x"));
        let rows = listed(&store);
        assert!(
            rows.iter()
                .any(|message| message.headers.message_id.as_str() == "sent-a@x")
        );
        assert!(
            rows.iter()
                .any(|message| message.headers.subject == "Re: Patch")
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn subject_alone_does_not_collapse() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg("a@x", None, None, "Hello", "one body"))
            .unwrap();
        let mut echo = msg("b@x", None, None, "Hello", "other body");
        echo.list_id = Some("list.example".into());
        store.upsert_message(&echo).unwrap();
        assert_eq!(listed(&store).len(), 2);
    }

    #[test]
    fn same_body_without_list_id_stays_distinct() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg("a@x", None, None, "Thanks", "Thanks"))
            .unwrap();
        let mut later = msg("b@x", None, None, "Thanks", "Thanks");
        later.headers.date_unix = 60;
        store.upsert_message(&later).unwrap();
        assert_eq!(listed(&store).len(), 2);
    }

    #[test]
    fn empty_body_same_subject_does_not_collapse() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut first = msg("a@x", None, None, "Hello", "Hello");
        first.body_text = None;
        first.headers.date_unix = 1_700_000_000;
        let mut echo = msg("b@x", None, None, "Hello", "Hello");
        echo.body_text = None;
        echo.list_id = Some("list.example".into());
        echo.headers.date_unix = first.headers.date_unix + 60;
        store.upsert_message(&first).unwrap();
        store.upsert_message(&echo).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|message| message.body_text.is_none()));
    }

    #[test]
    fn zero_dates_do_not_collapse() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut first = msg("a@x", None, None, "Hello", "hello body");
        first.headers.date_unix = 0;
        let mut echo = msg("b@x", None, None, "[list] Hello", "hello body");
        echo.list_id = Some("list.example".into());
        echo.headers.date_unix = 0;
        store.upsert_message(&first).unwrap();
        store.upsert_message(&echo).unwrap();
        assert_eq!(listed(&store).len(), 2);

        let mut dated = msg("c@x", None, None, "Hello", "hello body");
        dated.headers.date_unix = 1_700_000_000;
        store.upsert_message(&dated).unwrap();
        let mut undated_echo = msg("d@x", None, None, "[list] Hello", "hello body");
        undated_echo.list_id = Some("list.example".into());
        undated_echo.headers.date_unix = 0;
        store.upsert_message(&undated_echo).unwrap();
        assert_eq!(listed(&store).len(), 4);
    }

    #[test]
    fn later_same_body_stays_beside_an_aliased_row() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let sent_at = 1_700_000_000;
        let mut first = msg(
            "thanks-a@x",
            None,
            None,
            "First thanks",
            "Thanks for the detailed notes",
        );
        first.headers.date_unix = sent_at;
        store.upsert_message(&first).unwrap();
        let mut echo = msg(
            "thanks-list@x",
            None,
            None,
            "[list] First thanks",
            "Thanks for the detailed notes",
        );
        echo.list_id = Some("list.example".into());
        echo.headers.date_unix = sent_at + 60;
        store.upsert_message(&echo).unwrap();
        assert_eq!(listed(&store).len(), 1);

        let mut later = msg(
            "thanks-b@x",
            None,
            None,
            "Second thanks",
            "Thanks for the detailed notes",
        );
        later.headers.date_unix = sent_at + (2 * 60 * 60);
        store.upsert_message(&later).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 2);
        let kept = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "thanks-a@x")
            .unwrap();
        assert_eq!(kept.headers.subject, "First thanks");
        assert!(
            rows.iter()
                .any(|message| message.headers.message_id.as_str() == "thanks-b@x")
        );
    }

    #[test]
    fn distinct_short_list_replies_stay_separate() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let sent_at = 1_700_000_000;
        let mut first = msg(
            "plus-a@x",
            None,
            Some("patch-a@x"),
            "[list] Re: Patch A",
            "+1",
        );
        first.list_id = Some("list.example".into());
        first.headers.date_unix = sent_at;
        first.folders = vec!["INBOX".into()];
        store.upsert_message(&first).unwrap();
        // Same thread and body, both delivered by the list.
        let mut again = msg(
            "plus-c@x",
            None,
            Some("patch-a@x"),
            "Re: [list] Patch A",
            "+1",
        );
        again.list_id = Some("list.example".into());
        again.headers.date_unix = sent_at + (3 * 60 * 60);
        again.folders = vec!["INBOX".into()];
        store.upsert_message(&again).unwrap();
        let mut second = msg(
            "plus-b@x",
            None,
            Some("patch-b@x"),
            "[list] Re: Patch B",
            "+1",
        );
        second.list_id = Some("list.example".into());
        second.headers.date_unix = sent_at + (2 * 60 * 60);
        second.folders = vec!["INBOX".into()];
        store.upsert_message(&second).unwrap();
        // A Sent "+1" on another thread shares the subject with one list post
        // and a different parent. It is not that post's echo.
        let mut list_budget = msg(
            "plus-d@x",
            None,
            Some("budget@x"),
            "[list] Re: Budget",
            "+1",
        );
        list_budget.list_id = Some("list.example".into());
        list_budget.headers.date_unix = sent_at + 60;
        store.upsert_message(&list_budget).unwrap();
        let mut sent_other = with_arrival(
            msg(
                "plus-sent-budget@x",
                None,
                Some("dinner@x"),
                "Re: Budget",
                "+1",
            ),
            "Sent",
            "",
            "me@x.com",
        );
        sent_other.headers.date_unix = sent_at + 90;
        store.upsert_message(&sent_other).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 5);
        for id in [
            "plus-a@x",
            "plus-b@x",
            "plus-c@x",
            "plus-d@x",
            "plus-sent-budget@x",
        ] {
            assert!(
                rows.iter()
                    .any(|message| message.headers.message_id.as_str() == id),
                "{id}"
            );
        }

        let echo = Store::open_in_memory().unwrap();
        echo.upsert_account(&account()).unwrap();
        let list_thread = echo.upsert_message(&first).unwrap();
        let mut sent = with_arrival(
            msg("plus-sent@x", None, Some("patch-a@x"), "Re: Patch A", "+1"),
            "Sent",
            "",
            "me@x.com",
        );
        sent.headers.date_unix = sent_at + 30;
        assert_eq!(echo.upsert_message(&sent).unwrap(), list_thread);
        let collapsed = listed(&echo);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].headers.message_id.as_str(), "plus-a@x");
        assert!(collapsed[0].folders.iter().any(|folder| folder == "Sent"));
        assert!(collapsed[0].folders.iter().any(|folder| folder == "INBOX"));
    }

    #[test]
    fn second_sent_short_reply_stays_after_the_echo_joins() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let sent_at = 1_700_000_000;
        let mut list = msg(
            "plus-list@x",
            None,
            Some("patch@x"),
            "[list] Re: Patch",
            "+1",
        );
        list.list_id = Some("list.example".into());
        list.headers.date_unix = sent_at;
        list.folders = vec!["INBOX".into()];
        store.upsert_message(&list).unwrap();
        let mut first = with_arrival(
            msg("plus-sent-1@x", None, Some("patch@x"), "Re: Patch", "+1"),
            "Sent",
            "",
            "me@x.com",
        );
        first.headers.date_unix = sent_at + 30;
        first.remote_id = RemoteId("imap:sent-1".into());
        store.upsert_message(&first).unwrap();
        assert_eq!(listed(&store).len(), 1);

        let mut second = with_arrival(
            msg("plus-sent-2@x", None, Some("patch@x"), "Re: Patch", "+1"),
            "Sent",
            "",
            "me@x.com",
        );
        second.headers.date_unix = sent_at + (60 * 60);
        second.remote_id = RemoteId("imap:sent-2".into());
        store.upsert_message(&second).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 2);
        let kept = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "plus-list@x")
            .unwrap();
        let later = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "plus-sent-2@x")
            .unwrap();
        assert!(later.folders.iter().any(|folder| folder == "Sent"));
        assert_eq!(later.remote_id.0, "imap:sent-2");
        assert_ne!(kept.remote_id.0, "imap:sent-2");
    }

    fn gmail_sent_listing() -> ListedMailbox {
        ListedMailbox {
            name: "[Gmail]/Sent Mail".into(),
            raw: "[Gmail]/Sent Mail".into(),
            delimiter: Some('/'),
            attributes: vec!["\\Sent".into()],
        }
    }

    #[test]
    fn short_list_echoes_recognize_sent_folders_in_either_arrival_order() {
        let gmail_roles = FolderRoles::detect(&[gmail_sent_listing()], &[]);
        for (folder, listing) in [
            ("sEnT", None),
            ("Sent Items", None),
            ("INBOX.Sent", None),
            ("[gmail]/Sent Mail", Some(&gmail_roles)),
        ] {
            for sent_first in [true, false] {
                let store = Store::open_in_memory().unwrap();
                store.upsert_account(&account()).unwrap();
                if let Some(roles) = listing {
                    store
                        .apply_delta(
                            &account().id,
                            &MailboxDelta {
                                roles: Some(roles.clone()),
                                ..MailboxDelta::default()
                            },
                        )
                        .unwrap();
                }
                let mut sent = msg("sent@x", None, Some("patch@x"), "Re: Patch", "+1");
                sent.folders = vec![folder.into()];
                let mut echo = msg("list@x", None, Some("patch@x"), "[list] Re: Patch", "+1");
                echo.folders = vec!["INBOX".into()];
                echo.list_id = Some("list.example".into());
                let (first, second) = if sent_first {
                    (sent, echo)
                } else {
                    (echo, sent)
                };
                store.upsert_message(&first).unwrap();
                store.upsert_message(&second).unwrap();

                let rows = listed(&store);
                assert_eq!(rows.len(), 1, "{folder}, sent first: {sent_first}");
                assert_eq!(rows[0].headers.message_id, first.headers.message_id);
                assert_eq!(
                    rows[0].folders,
                    [first.folders[0].clone(), second.folders[0].clone()]
                );
            }
        }
    }

    #[test]
    fn short_list_echoes_require_exactly_one_copy_in_the_sent_role() {
        let gmail_roles = FolderRoles::detect(&[gmail_sent_listing()], &[]);
        for (original_folder, echo_folder) in [
            ("Sent", "INBOX"),
            ("[Gmail]/Sent Mail", "[Gmail]/Sent Mail"),
        ] {
            let store = Store::open_in_memory().unwrap();
            store.upsert_account(&account()).unwrap();
            let mut original = msg("sent@x", None, Some("patch@x"), "Re: Patch", "+1");
            original.folders = vec![original_folder.into()];
            let mut echo = msg("list@x", None, Some("patch@x"), "[list] Re: Patch", "+1");
            echo.folders = vec![echo_folder.into()];
            echo.list_id = Some("list.example".into());
            store
                .apply_delta(
                    &account().id,
                    &MailboxDelta {
                        roles: Some(gmail_roles.clone()),
                        messages: vec![original, echo],
                        ..MailboxDelta::default()
                    },
                )
                .unwrap();

            assert_eq!(listed(&store).len(), 2, "{original_folder}, {echo_folder}");
        }
    }

    #[test]
    fn long_list_echo_uses_the_sent_override_from_its_delta() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let roles = FolderRoles::detect(
            &[gmail_sent_listing()],
            &[(FolderRole::Sent, "Envoyés".into())],
        );
        let (mut sent, mut echo) = sent_a_and_list_b();
        sent.folders = vec!["Envoyés".into()];
        echo.headers.subject = "[list] Re: Patch".into();
        store
            .apply_delta(
                &account().id,
                &MailboxDelta {
                    roles: Some(roles),
                    messages: vec![sent, echo],
                    ..MailboxDelta::default()
                },
            )
            .unwrap();

        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].headers.message_id.as_str(), "sent-a@x");
        assert_eq!(rows[0].folders, ["Envoyés", "INBOX"]);
    }

    #[test]
    fn backfill_joins_an_unlabeled_list_echo_from_sent_items() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let (mut sent, mut echo) = sent_a_and_list_b();
        sent.folders = vec!["Sent Items".into()];
        echo.headers.subject = "[list] Re: Patch".into();
        echo.list_id = None;
        store.upsert_message(&sent).unwrap();
        store.upsert_message(&echo).unwrap();
        assert_eq!(listed(&store).len(), 2);

        merge_backfilled_duplicates(&store.conn).unwrap();

        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].folders.contains(&"Sent Items".into()));
        assert!(rows[0].folders.contains(&"INBOX".into()));
    }

    fn sent_repair_fixture(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fell-sent-repair-{}-{tag}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        dir.join("mail.sqlite")
    }

    fn split_sent_pair(tag: &str, folder: &str, uid: u32) -> (Message, Message) {
        let mut sent = msg(&format!("{tag}-sent@x"), None, Some("patch@x"), tag, "+1");
        sent.unread = false;
        sent.folders = vec![folder.into()];
        sent.remote_id = RemoteId(format!("imap:77:{uid}:{folder}"));
        let mut echo = msg(
            &format!("{tag}-echo@x"),
            None,
            Some("patch@x"),
            &format!("[list] {tag}"),
            "+1",
        );
        echo.folders = vec!["INBOX".into()];
        echo.remote_id = RemoteId(format!("imap:77:{uid}:INBOX"));
        echo.list_id = Some("list.example".into());
        (sent, echo)
    }

    // Write the rows a pre-repair version left behind, with the current
    // schema and real fingerprint, threading, body and copy records.
    fn seed_split_messages(store: &Store, messages: &[Message]) {
        let tx = immediate_transaction(&store.conn).unwrap();
        for message in messages {
            insert_new(&tx, message, &message_fingerprint(message)).unwrap();
            copies::record(&tx, message).unwrap();
        }
        store.rethread_written(&tx, &account().id).unwrap();
        tx.commit().unwrap();
    }

    fn predate_sent_echo_repair(store: &Store) {
        store
            .conn
            .execute("DELETE FROM meta WHERE key = 'sent_echo_repair_v1'", [])
            .unwrap();
    }

    #[test]
    fn sent_echo_repair_on_open_preserves_stored_mail_and_runs_once() {
        let gmail_roles = FolderRoles::detect(&[gmail_sent_listing()], &[]);
        for (tag, folder, listing) in [
            ("alternate", "Sent Items", None),
            ("nested", "INBOX.Sent", None),
            ("special-use", "[Gmail]/Sent Mail", Some(&gmail_roles)),
        ] {
            let path = sent_repair_fixture(tag);
            let acct = account();
            let store = Store::open(&path).unwrap();
            store.upsert_account(&acct).unwrap();
            if let Some(roles) = listing {
                roles::replace(&store.conn, &acct.id, roles).unwrap();
            }
            let (mut sent, mut echo) = split_sent_pair("Patch", folder, 1);
            if tag == "nested" {
                echo.list_id = None;
            }
            sent.labels.push("work".into());
            echo.folders.push("Archive".into());
            echo.labels.push("list-label".into());
            echo.starred = true;
            echo.archived = true;
            echo.snooze_until = Some(1_893_456_000);
            let attachment = crate::imap::mime_fixtures::mixed_two_attachments("Patch-echo@x");
            let parts = crate::imap::parts_from_raw(&attachment.raw);
            echo.parts = Some(parts.clone());
            let sent_id = sent.headers.message_id.clone();
            let echo_id = echo.headers.message_id.clone();
            let (mut unrelated, mut unrelated_echo) = split_sent_pair("Different", folder, 2);
            unrelated.headers.subject = "Unrelated subject".into();
            unrelated_echo.folders = vec!["Lists".into()];
            let (ambiguous_sent, ambiguous_echo) = split_sent_pair("Ambiguous", folder, 3);
            let (mut second_send, _) = split_sent_pair("Ambiguous", folder, 4);
            second_send.headers.message_id = MessageId::new("second-send@x");
            let (pending_sent, mut pending_echo) = split_sent_pair("Pending", folder, 5);
            pending_echo.unread = false;
            let pending_id = pending_echo.headers.message_id.clone();
            seed_split_messages(
                &store,
                &[
                    sent,
                    echo,
                    unrelated,
                    unrelated_echo,
                    ambiguous_sent,
                    ambiguous_echo,
                    second_send,
                    pending_sent,
                    pending_echo,
                ],
            );
            intents::record(
                &store.conn,
                &acct.id,
                pending_id.as_str(),
                Some(false),
                None,
            )
            .unwrap();
            store
                .conn
                .execute(
                    "UPDATE message_copies SET modseq = x'0000000000000007', seen = 1, flagged = 1
                 WHERE account_id = ?1 AND message_id = ?2",
                    params![acct.id.as_str(), echo_id.as_str()],
                )
                .unwrap();
            assert!(message_row_present(&store, echo_id.as_str()));
            let version = schema_version(&store.conn).unwrap();
            predate_sent_echo_repair(&store);
            drop(store);

            let store = Store::open(&path).unwrap();
            assert_eq!(schema_version(&store.conn).unwrap(), version);
            assert!(!message_row_present(&store, echo_id.as_str()));
            let kept = store.message(&acct.id, &echo_id).unwrap().unwrap();
            assert_eq!(kept.headers.message_id, sent_id);
            assert_eq!(kept.folders, [folder, "Archive"]);
            assert_eq!(kept.labels, ["work", "list-label"]);
            assert!(kept.unread && kept.starred && kept.archived);
            assert_eq!(kept.snooze_until, Some(1_893_456_000));
            assert_eq!(
                store.message_attachments(&acct.id, &echo_id).unwrap(),
                Some(parts.mime_parts)
            );
            let copy_owner: String = store.conn.query_row(
                "SELECT message_id FROM message_copies WHERE account_id = ?1 AND mailbox = 'INBOX'
                   AND uidvalidity = 77 AND uid = 1 AND seen = 1 AND flagged = 1
                   AND modseq = x'0000000000000007'",
                [acct.id.as_str()], |row| row.get(0),
            ).unwrap();
            assert_eq!(copy_owner, sent_id.as_str());
            assert_eq!(
                store.stored_uids(&acct.id, folder, 77, (1, 1)).unwrap(),
                [1]
            );
            for id in [
                "Different-sent@x",
                "Different-echo@x",
                "Ambiguous-sent@x",
                "Ambiguous-echo@x",
                "second-send@x",
                pending_id.as_str(),
            ] {
                assert!(message_row_present(&store, id), "{id}");
            }
            assert_eq!(
                store.pending_flag_changes(&acct.id).unwrap()[0].unread,
                Some(false)
            );

            let (new_sent, new_echo) = split_sent_pair("Later", folder, 6);
            let new_echo_id = new_echo.headers.message_id.clone();
            seed_split_messages(&store, &[new_sent, new_echo]);
            drop(store);
            let store = Store::open(&path).unwrap();
            assert!(message_row_present(&store, new_echo_id.as_str()));
            if listing.is_none() {
                // A first listing that confirms the name already tried
                // at open does not rescan that folder.
                store
                    .apply_delta(
                        &acct.id,
                        &MailboxDelta {
                            roles: Some(FolderRoles::from_names(&[folder.into()])),
                            ..MailboxDelta::default()
                        },
                    )
                    .unwrap();
                assert!(message_row_present(&store, new_echo_id.as_str()));
            }
            drop(store);
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn sent_echo_repair_waits_for_the_first_applicable_listing() {
        let path = sent_repair_fixture("listing");
        let acct = account();
        let folder = "[Gmail]/Sent Mail";
        let store = Store::open(&path).unwrap();
        store.upsert_account(&acct).unwrap();
        let (sent, echo) = split_sent_pair("Patch", folder, 1);
        let reply = msg(
            "reply@x",
            None,
            Some("Patch-echo@x"),
            "Re: Patch",
            "A reply",
        );
        seed_split_messages(&store, &[sent, echo, reply]);
        predate_sent_echo_repair(&store);
        drop(store);

        let store = Store::open(&path).unwrap();
        let list = |store: &Store, roles: FolderRoles| {
            store
                .apply_delta(
                    &acct.id,
                    &MailboxDelta {
                        roles: Some(roles),
                        ..MailboxDelta::default()
                    },
                )
                .unwrap();
        };
        assert!(message_row_present(&store, "Patch-echo@x"));
        assert_ne!(
            store
                .message(&acct.id, &MessageId::new("Patch-sent@x"))
                .unwrap()
                .unwrap()
                .thread_id,
            store
                .message(&acct.id, &MessageId::new("reply@x"))
                .unwrap()
                .unwrap()
                .thread_id,
        );
        list(&store, FolderRoles::from_names(&["INBOX".into()]));
        assert!(message_row_present(&store, "Patch-echo@x"));
        assert_eq!(
            store.stored_uids(&acct.id, folder, 77, (1, 1)).unwrap(),
            [1]
        );
        assert_eq!(
            store.stored_uids(&acct.id, "INBOX", 77, (1, 1)).unwrap(),
            [1]
        );

        let gmail_roles = FolderRoles::detect(&[gmail_sent_listing()], &[]);
        list(&store, gmail_roles.clone());
        assert!(!message_row_present(&store, "Patch-echo@x"));
        let repaired = store
            .message(&acct.id, &MessageId::new("Patch-echo@x"))
            .unwrap()
            .unwrap();
        let reply = store
            .message(&acct.id, &MessageId::new("reply@x"))
            .unwrap()
            .unwrap();
        assert_eq!(repaired.headers.message_id.as_str(), "Patch-sent@x");
        assert_eq!(repaired.folders, [folder, "INBOX"]);
        assert_eq!(reply.thread_id, repaired.thread_id);

        let (new_sent, new_echo) = split_sent_pair("Later", folder, 2);
        seed_split_messages(&store, &[new_sent, new_echo]);
        drop(store);
        let store = Store::open(&path).unwrap();
        list(&store, gmail_roles);
        assert!(message_row_present(&store, "Later-echo@x"));
        drop(store);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn list_short_reply_without_a_parent_does_not_absorb_a_sent_reply() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let sent_at = 1_700_000_000;
        let mut list = msg("plus-list@x", None, None, "[list] Re: Patch", "+1");
        list.list_id = Some("list.example".into());
        list.headers.date_unix = sent_at;
        list.folders = vec!["INBOX".into()];
        store.upsert_message(&list).unwrap();
        let mut sent = with_arrival(
            msg("plus-sent@x", None, Some("other@x"), "Re: Patch", "+1"),
            "Sent",
            "",
            "me@x.com",
        );
        sent.headers.date_unix = sent_at + 60;
        sent.remote_id = RemoteId("imap:sent".into());
        store.upsert_message(&sent).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|message| message.headers.message_id.as_str() == "plus-list@x")
        );
        let sent_row = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "plus-sent@x")
            .unwrap();
        assert_eq!(sent_row.remote_id.0, "imap:sent");
        assert_eq!(
            sent_row.headers.in_reply_to.as_ref().map(MessageId::as_str),
            Some("other@x")
        );
    }

    #[test]
    fn inbox_short_reply_without_list_id_stays_beside_a_list_reply() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let sent_at = 1_700_000_000;
        let mut inbox = msg("plus-inbox@x", None, Some("patch@x"), "Re: Patch", "+1");
        inbox.headers.date_unix = sent_at;
        inbox.folders = vec!["INBOX".into()];
        store.upsert_message(&inbox).unwrap();
        let mut list = msg(
            "plus-list@x",
            None,
            Some("patch@x"),
            "[list] Re: Patch",
            "+1",
        );
        list.list_id = Some("list.example".into());
        list.headers.date_unix = sent_at + (2 * 60 * 60);
        list.folders = vec!["INBOX".into()];
        store.upsert_message(&list).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|message| message.headers.message_id.as_str() == "plus-inbox@x")
        );
        assert!(
            rows.iter()
                .any(|message| message.headers.message_id.as_str() == "plus-list@x")
        );

        let echo = Store::open_in_memory().unwrap();
        echo.upsert_account(&account()).unwrap();
        let mut bare = msg("plus-bare@x", None, None, "Re: Patch", "+1");
        bare.headers.date_unix = sent_at + 60;
        bare.folders = vec!["INBOX".into()];
        echo.upsert_message(&bare).unwrap();
        let mut listed_echo = msg("plus-echo@x", None, None, "[list] Re: Patch", "+1");
        listed_echo.list_id = Some("list.example".into());
        listed_echo.headers.date_unix = sent_at + 90;
        listed_echo.folders = vec!["INBOX".into()];
        echo.upsert_message(&listed_echo).unwrap();
        assert_eq!(listed(&echo).len(), 2);
    }

    #[test]
    fn merged_copy_keeps_its_alias_when_the_keeper_has_no_server_id() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut sent = msg("sent@x", None, None, "Hello", "sent body");
        sent.headers.date_unix = 1_700_000_000;
        sent.folders = vec!["Sent".into()];
        store.upsert_message(&sent).unwrap();
        let mut list = msg("list@x", None, None, "[list] Hello", "list body");
        list.headers.date_unix = 1_700_000_000;
        list.list_id = Some("list.example".into());
        list.folders = vec!["INBOX".into()];
        store.upsert_message(&list).unwrap();
        let fingerprint = crate::message::body_fingerprint("hello body");
        store
            .conn
            .execute(
                "UPDATE messages SET body_text = 'hello body', snippet = 'hello body',
                    body_fingerprint = ?1
                 WHERE account_id = 'a1'",
                params![fingerprint],
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE messages SET remote_id = '' WHERE message_id = 'list@x'",
                [],
            )
            .unwrap();
        let mut again = msg("sent@x", None, None, "Hello", "hello body");
        again.headers.date_unix = 1_700_000_000;
        again.folders = vec!["Sent".into()];
        store.upsert_message(&again).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].headers.message_id.as_str(), "list@x");
        let canonical: String = store
            .conn
            .query_row(
                "SELECT canonical_message_id FROM message_aliases WHERE message_id = 'sent@x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canonical, "list@x");
        store.upsert_message(&again).unwrap();
        assert_eq!(listed(&store).len(), 1);
        let reply = store
            .upsert_message(&msg(
                "reply@x",
                None,
                Some("sent@x"),
                "Re: Hello",
                "following up on the sent copy",
            ))
            .unwrap();
        assert_eq!(reply, rows[0].thread_id);
        assert_eq!(listed(&store).len(), 2);
    }

    #[test]
    fn same_id_resync_replaces_a_different_parent() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let old_thread = store
            .upsert_message(&msg("old@x", None, None, "Old", "old body"))
            .unwrap();
        let new_thread = store
            .upsert_message(&msg("new@x", None, None, "New", "new body"))
            .unwrap();
        let mut note = msg("note@x", None, Some("old@x"), "Note", "note body");
        store.upsert_message(&note).unwrap();
        note.headers.in_reply_to = Some(MessageId::new("new@x"));
        note.headers.references = vec![MessageId::new("new@x")];
        assert_eq!(store.upsert_message(&note).unwrap(), new_thread);
        let stored = listed(&store)
            .into_iter()
            .find(|message| message.headers.message_id.as_str() == "note@x")
            .unwrap();
        assert_eq!(
            stored.headers.in_reply_to.as_ref().map(MessageId::as_str),
            Some("new@x")
        );
        assert_eq!(stored.headers.references, vec![MessageId::new("new@x")]);
        assert!(
            !store
                .thread_messages(&AccountId("a1".into()), &old_thread)
                .unwrap()
                .iter()
                .any(|message| message.headers.message_id.as_str() == "note@x")
        );

        note.headers.in_reply_to = None;
        note.headers.references.clear();
        store.upsert_message(&note).unwrap();
        let stored = listed(&store)
            .into_iter()
            .find(|message| message.headers.message_id.as_str() == "note@x")
            .unwrap();
        assert_eq!(
            stored.headers.in_reply_to.as_ref().map(MessageId::as_str),
            Some("new@x")
        );
    }

    #[test]
    fn alias_keeps_the_other_remote_id_and_folder() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut inbox = with_arrival(
            msg("list-rewrite@x", None, None, "[list] Hello", "hello body"),
            "INBOX",
            "",
            "list@x.com",
        );
        inbox.list_id = Some("list.example".into());
        inbox.remote_id = RemoteId("imap:inbox".into());
        inbox.unread = true;
        store.upsert_message(&inbox).unwrap();
        let mut sent = with_arrival(
            msg("orig@x", None, None, "Hello", "hello body"),
            "Sent",
            "",
            "me@x.com",
        );
        sent.remote_id = RemoteId("imap:sent".into());
        sent.unread = true;
        store.upsert_message(&sent).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].remote_id.0, "imap:inbox");
        let (remote, folder): (String, String) = store
            .conn
            .query_row(
                "SELECT remote_id, folder FROM message_aliases WHERE message_id = 'orig@x'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(remote, "imap:sent");
        assert_eq!(folder, "Sent");
        store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: Vec::new(),
                    flags: vec![crate::backend::FlagUpdate {
                        remote_id: RemoteId("imap:sent".into()),
                        unread: Some(false),
                        flagged: None,
                        keywords: None,
                        modseq: None,
                    }],
                    cursor: None,
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].unread);
        assert!(
            crate::actions::message_row_exists(
                store.connection(),
                &AccountId("a1".into()),
                "orig@x",
            )
            .unwrap()
        );
    }

    #[test]
    fn same_id_list_update_replaces_subject_and_display_name() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut first = msg("digest@x", None, None, "Digest 1", "hello body");
        first.list_id = Some("list.example".into());
        first.headers.from.name = Some("Ann via Dev".into());
        store.upsert_message(&first).unwrap();
        let mut next = first.clone();
        next.headers.subject = "Digest 2".into();
        next.headers.from.name = Some("Ann".into());
        store.upsert_message(&next).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].headers.subject, "Digest 2");
        assert_eq!(rows[0].headers.from.name.as_deref(), Some("Ann"));
    }

    #[test]
    fn same_id_update_replaces_a_proton_conversation_id() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg("a@x", Some("conv-old"), None, "Hello", "hello body"))
            .unwrap();
        let moved = store
            .upsert_message(&msg("a@x", Some("conv-new"), None, "Hello", "hello body"))
            .unwrap();
        assert_eq!(moved, ThreadId::proton("conv-new"));
        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].headers.proton_conversation_id.as_deref(),
            Some("conv-new")
        );
        let mut cleared = msg("a@x", None, None, "Hello", "hello body");
        cleared.headers.proton_conversation_id = None;
        store.upsert_message(&cleared).unwrap();
        let rows = listed(&store);
        assert_eq!(
            rows[0].headers.proton_conversation_id.as_deref(),
            Some("conv-new")
        );
    }

    #[test]
    fn list_first_prefers_the_sent_display_name() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut echo = msg("list-rewrite@x", None, None, "[list] Hello", "hello body");
        echo.list_id = Some("list.example".into());
        echo.headers.from.name = Some("Ann via Dev".into());
        store.upsert_message(&echo).unwrap();
        let mut sent = msg("orig@x", None, None, "Hello", "hello body");
        sent.headers.from.name = Some("Ann".into());
        store.upsert_message(&sent).unwrap();
        let collapsed = listed(&store);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].headers.from.name.as_deref(), Some("Ann"));

        sent.headers.from.name = Some("Ann".into());
        store.upsert_message(&sent).unwrap();
        let collapsed = listed(&store);
        assert_eq!(collapsed.len(), 1);
        assert_eq!(collapsed[0].headers.from.name.as_deref(), Some("Ann"));
    }

    #[test]
    fn different_proton_ids_do_not_collapse() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .upsert_message(&msg("a@x", Some("conv-a"), None, "Hello", "hello body"))
            .unwrap();
        let mut echo = msg("b@x", Some("conv-b"), None, "[list] Hello", "hello body");
        echo.list_id = Some("list.example".into());
        store.upsert_message(&echo).unwrap();
        assert_eq!(listed(&store).len(), 2);
        assert_eq!(
            store.list_threads(&AccountId("a1".into())).unwrap().len(),
            2
        );
    }

    #[test]
    fn ambiguous_fingerprint_matches_stay_separate() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut older = msg("older@x", None, None, "Hello", "hello body");
        older.headers.date_unix = 1_700_000_000;
        let mut newer = msg("newer@x", None, None, "Hello", "hello body");
        newer.headers.date_unix = older.headers.date_unix + (10 * 60 * 60);
        store.upsert_message(&older).unwrap();
        store.upsert_message(&newer).unwrap();
        let mut echo = msg("list-rewrite@x", None, None, "[list] Hello", "hello body");
        echo.list_id = Some("list.example".into());
        echo.headers.date_unix = older.headers.date_unix + (5 * 60 * 60);
        store.upsert_message(&echo).unwrap();
        assert_eq!(listed(&store).len(), 3);
    }

    #[test]
    fn subject_key_strips_list_and_reply_prefixes() {
        assert_eq!(
            super::subject_key("Hello"),
            super::subject_key("[list] Hello")
        );
        assert_eq!(
            super::subject_key("Hello"),
            super::subject_key("Re: [list] Hello")
        );
        assert_eq!(
            super::subject_key("Hello"),
            super::subject_key("[list] Re: Hello")
        );
        assert_ne!(
            super::subject_key("Hello"),
            super::subject_key("[list] Other")
        );
        assert_eq!(super::subject_key("会議"), "会議");
        assert_eq!(super::subject_key("[list] 会議"), "会議");
        assert_eq!(super::subject_key("Re: [list] 会議"), "会議");
        assert_eq!(super::subject_key("fwd: 会議"), "会議");
        assert_eq!(super::subject_key("FWD: 会議"), "会議");
    }

    #[test]
    fn v2_store_backfills_fingerprints_before_list_echo() {
        let path =
            std::env::temp_dir().join(format!("fell-schema-v2-{}-fp.sqlite", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '2');
                 CREATE TABLE accounts (
                    id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL,
                    email TEXT NOT NULL,
                    backend_kind TEXT NOT NULL
                 );
                 CREATE TABLE messages (
                    rowid INTEGER PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    remote_id TEXT NOT NULL,
                    thread_id TEXT NOT NULL,
                    proton_conversation_id TEXT,
                    in_reply_to TEXT,
                    references_header TEXT NOT NULL,
                    from_name TEXT,
                    from_email TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    date_unix INTEGER NOT NULL,
                    snippet TEXT NOT NULL,
                    body_text TEXT,
                    unread INTEGER NOT NULL DEFAULT 1,
                    folders TEXT NOT NULL DEFAULT '',
                    labels TEXT NOT NULL DEFAULT '',
                    arrived_via TEXT NOT NULL DEFAULT '',
                    to_line TEXT NOT NULL DEFAULT '',
                    cc_line TEXT NOT NULL DEFAULT '',
                    bcc_line TEXT NOT NULL DEFAULT '',
                    has_attachment INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0,
                    snooze_until INTEGER,
                    UNIQUE (account_id, message_id)
                 );
                 INSERT INTO accounts (id, display_name, email, backend_kind)
                    VALUES ('a1', 'Caleb', 'c@x', 'bridge');
                 INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread, folders
                 ) VALUES (
                    'a1', 'orig@x', 'r1', 'msgid:orig@x', '',
                    'from@x', 'Hello', 1, 'hello body', 'hello body', 1, 'Sent'
                 );",
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let version: String = store
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");
        let fingerprint: String = store
            .conn
            .query_row(
                "SELECT body_fingerprint FROM messages WHERE message_id = 'orig@x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fingerprint, crate::message::body_fingerprint("hello body"));
        let mut echo = msg("list-rewrite@x", None, None, "[list] Hello", "hello body");
        echo.list_id = Some("list.example".into());
        echo.folders = vec!["INBOX".into()];
        store.upsert_message(&echo).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].headers.message_id.as_str(), "orig@x");
        assert_eq!(rows[0].headers.subject, "Hello");
        assert_eq!(rows[0].folders, vec!["Sent", "INBOX"]);
        assert_eq!(rows[0].list_id.as_deref(), Some("list.example"));
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn resync_joins_a_pair_that_was_stored_separately() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let fingerprint = crate::message::body_fingerprint("hello body");
        for (id, subject, folder, list_id) in [
            ("sent@x", "Hello", "Sent", None),
            ("list@x", "[list] Hello", "INBOX", Some("list.example")),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO messages (
                        account_id, message_id, remote_id, thread_id, references_header,
                        from_email, subject, date_unix, snippet, body_text, unread, folders,
                        body_fingerprint, list_id
                     ) VALUES (
                        'a1', ?1, ?1, ?2, '', 'from@x', ?3, 1700000000, 'hello body', 'hello body',
                        1, ?4, ?5, ?6
                     )",
                    params![
                        id,
                        format!("msgid:{id}"),
                        subject,
                        folder,
                        fingerprint,
                        list_id
                    ],
                )
                .unwrap();
        }
        assert_eq!(listed(&store).len(), 2);
        let mut again = msg("sent@x", None, None, "Hello", "hello body");
        again.headers.date_unix = 1_700_000_000;
        again.folders = vec!["Sent".into()];
        store.upsert_message(&again).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].headers.subject, "Hello");
        assert_eq!(rows[0].headers.message_id.as_str(), "list@x");
        assert!(rows[0].folders.iter().any(|folder| folder == "Sent"));
        assert!(rows[0].folders.iter().any(|folder| folder == "INBOX"));
    }

    #[test]
    fn backfill_hashes_bodies_in_batches_before_version_3() {
        let path = std::env::temp_dir().join(format!(
            "fell-schema-v2-{}-batch.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '2');
                 CREATE TABLE accounts (
                    id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL,
                    email TEXT NOT NULL,
                    backend_kind TEXT NOT NULL
                 );
                 CREATE TABLE messages (
                    rowid INTEGER PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    remote_id TEXT NOT NULL,
                    thread_id TEXT NOT NULL,
                    proton_conversation_id TEXT,
                    in_reply_to TEXT,
                    references_header TEXT NOT NULL,
                    from_name TEXT,
                    from_email TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    date_unix INTEGER NOT NULL,
                    snippet TEXT NOT NULL,
                    body_text TEXT,
                    unread INTEGER NOT NULL DEFAULT 1,
                    folders TEXT NOT NULL DEFAULT '',
                    UNIQUE (account_id, message_id)
                 );
                 INSERT INTO accounts (id, display_name, email, backend_kind)
                    VALUES ('a1', 'Caleb', 'c@x', 'bridge');",
            )
            .unwrap();
            for n in 0..40 {
                conn.execute(
                    "INSERT INTO messages (
                        account_id, message_id, remote_id, thread_id, references_header,
                        from_email, subject, date_unix, snippet, body_text, unread
                     ) VALUES ('a1', ?1, ?1, ?1, '', 'from@x', 'S', 1, ?2, ?2, 1)",
                    params![format!("m{n}@x"), format!("body {n}")],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread
                 ) VALUES ('a1', 'empty@x', 'empty@x', 'empty@x', '', 'from@x', 'Empty', 1, 'Empty', NULL, 1)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let version: String = store
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");
        let filled: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE body_fingerprint != ''",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(filled, 40);
        let empty: String = store
            .conn
            .query_row(
                "SELECT body_fingerprint FROM messages WHERE message_id = 'empty@x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(empty.is_empty());
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn absorb_keeps_local_state_from_the_deleted_row() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let fingerprint = crate::message::body_fingerprint("hello body");
        let folders = join_tokens(&["Sent".into(), "Archive".into()]);
        store
            .conn
            .execute(
                "INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread, folders,
                    labels, starred, archived, snooze_until, body_fingerprint, list_id
                 ) VALUES (
                    'a1', 'sent@x', 'imap:sent', 'sent-thread', '', 'from@x', 'Hello',
                    1700000000, 'hello body', 'hello body', 1, ?1, 'work', 1, 1, 1893456000,
                    ?2, NULL
                 )",
                params![folders, fingerprint],
            )
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread, folders,
                    body_fingerprint, list_id
                 ) VALUES (
                    'a1', 'list@x', 'imap:inbox', 'list-thread', '', 'from@x', '[list] Hello',
                    1700000000, 'hello body', 'hello body', 1, 'INBOX', ?1, 'list.example'
                 )",
                params![fingerprint],
            )
            .unwrap();
        let mut again = msg("sent@x", None, None, "Hello", "hello body");
        again.headers.date_unix = 1_700_000_000;
        again.folders = vec!["Sent".into()];
        again.remote_id = RemoteId("imap:sent".into());
        store.upsert_message(&again).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        let kept = &rows[0];
        assert_eq!(kept.headers.message_id.as_str(), "list@x");
        assert!(kept.starred);
        assert!(kept.archived);
        assert_eq!(kept.snooze_until, Some(1_893_456_000));
        assert_eq!(kept.labels, vec!["work".to_string()]);
        assert!(kept.folders.iter().any(|folder| folder == "Sent"));
        assert!(kept.folders.iter().any(|folder| folder == "Archive"));
        assert!(!kept.folders.iter().any(|folder| folder == "INBOX"));
    }

    #[test]
    fn absorb_moves_the_deleted_rows_parts_onto_the_kept_row() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let fingerprint = crate::message::body_fingerprint("hello body");
        for (id, remote, subject, folders, list) in [
            ("sent@x", "imap:sent", "Hello", "Sent", None),
            (
                "list@x",
                "imap:inbox",
                "[list] Hello",
                "INBOX",
                Some("list.example"),
            ),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO messages (
                        account_id, message_id, remote_id, thread_id, references_header,
                        from_email, subject, date_unix, snippet, body_text, unread, folders,
                        body_fingerprint, list_id
                     ) VALUES (
                        'a1', ?1, ?2, ?1, '', 'from@x', ?3, 1700000000, 'hello body',
                        'hello body', 1, ?4, ?5, ?6
                     )",
                    params![id, remote, subject, folders, fingerprint, list],
                )
                .unwrap();
        }
        // The Sent copy has the plain part, the list copy only HTML.
        let write = |id: &str, parts: crate::MessageParts| {
            bodies::write(
                &store.conn,
                &account().id,
                id,
                &bodies::StoredParts::new(&parts),
                bodies::Write::Replace,
            )
            .unwrap();
        };
        write(
            "sent@x",
            crate::MessageParts {
                plain: Some("hello body".into()),
                ..Default::default()
            },
        );
        write(
            "list@x",
            crate::MessageParts {
                html: Some("<p>hello body</p>".into()),
                bulk_headers: vec![("List-Id".into(), "<list.example>".into())],
                ..Default::default()
            },
        );
        let mut again = msg("sent@x", None, None, "Hello", "hello body");
        again.headers.date_unix = 1_700_000_000;
        again.folders = vec!["Sent".into()];
        again.remote_id = RemoteId("imap:sent".into());
        store.upsert_message(&again).unwrap();
        assert_eq!(listed(&store).len(), 1);
        let kept = store
            .message_body(&account().id, &MessageId::new("sent@x"))
            .unwrap()
            .unwrap();
        assert_eq!(
            kept,
            crate::MessageParts {
                mime_parts: Vec::new(),
                html: Some("<p>hello body</p>".into()),
                plain: Some("hello body".into()),
                bulk_headers: vec![("List-Id".into(), "<list.example>".into())],
            }
        );
    }

    #[test]
    fn absorb_moves_the_deleted_rows_part_list_onto_a_kept_row_without_one() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let fingerprint = crate::message::body_fingerprint("hello body");
        for (id, remote, subject, folders, list) in [
            ("sent@x", "imap:sent", "Hello", "Sent", None),
            (
                "list@x",
                "imap:inbox",
                "[list] Hello",
                "INBOX",
                Some("list.example"),
            ),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO messages (
                        account_id, message_id, remote_id, thread_id, references_header,
                        from_email, subject, date_unix, snippet, body_text, unread, folders,
                        body_fingerprint, list_id
                     ) VALUES (
                        'a1', ?1, ?2, ?1, '', 'from@x', ?3, 1700000000, 'hello body',
                        'hello body', 1, ?4, ?5, ?6
                     )",
                    params![id, remote, subject, folders, fingerprint, list],
                )
                .unwrap();
        }
        let fixture = crate::imap::mime_fixtures::mixed_two_attachments("sent@x");
        let parsed = crate::imap::parts_from_raw(&fixture.raw);
        assert_eq!(parsed.mime_parts.len(), 2);
        bodies::write(
            &store.conn,
            &account().id,
            "sent@x",
            &bodies::StoredParts::new(&parsed),
            bodies::Write::Replace,
        )
        .unwrap();
        // The list copy, which the merge keeps, was stored before part
        // lists were kept.
        store
            .conn
            .execute(
                "INSERT INTO message_bodies (account_id, message_id, codec, plain)
                 VALUES ('a1', 'list@x', 1, NULL)",
                [],
            )
            .unwrap();
        let mut again = msg("sent@x", None, None, "Hello", "hello body");
        again.headers.date_unix = 1_700_000_000;
        again.folders = vec!["Sent".into()];
        again.remote_id = RemoteId("imap:sent".into());
        store.upsert_message(&again).unwrap();
        assert_eq!(listed(&store).len(), 1);
        let kept: String = store
            .conn
            .query_row("SELECT message_id FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kept, "list@x");
        assert_eq!(
            store
                .message_attachments(&account().id, &MessageId::new("list@x"))
                .unwrap(),
            Some(parsed.mime_parts.clone())
        );
        // The marker and names come with the list.
        let row = store
            .message(&account().id, &MessageId::new("list@x"))
            .unwrap()
            .unwrap();
        assert!(row.has_attachment);
        assert_eq!(row.attachment_names, vec!["report.pdf", "totals.csv"]);
        // The deleted row's own rows went with it.
        let left: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM message_parts WHERE message_id = 'sent@x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn absorb_moves_the_embedding_to_the_kept_row() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let fingerprint = crate::message::body_fingerprint("hello body");
        for (id, remote, subject, folders, list) in [
            ("sent@x", "imap:sent", "Hello", "Sent", None),
            (
                "list@x",
                "imap:inbox",
                "[list] Hello",
                "INBOX",
                Some("list.example"),
            ),
        ] {
            store
                .conn
                .execute(
                    "INSERT INTO messages (
                        account_id, message_id, remote_id, thread_id, references_header,
                        from_email, subject, date_unix, snippet, body_text, unread, folders,
                        body_fingerprint, list_id
                     ) VALUES (
                        'a1', ?1, ?2, ?1, '', 'from@x', ?3, 1700000000, 'hello body',
                        'hello body', 1, ?4, ?5, ?6
                     )",
                    params![id, remote, subject, folders, fingerprint, list],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO message_embeddings
                        (account_id, message_id, model, model_version, dims, body_hash, vector)
                     VALUES ('a1', ?1, 'local-hash-v1', '1', 1, 'old', x'00000000')",
                    params![id],
                )
                .unwrap();
        }
        let mut again = msg("sent@x", None, None, "Hello", "hello body");
        again.headers.date_unix = 1_700_000_000;
        again.folders = vec!["Sent".into()];
        again.remote_id = RemoteId("imap:sent".into());
        store.upsert_message(&again).unwrap();
        assert_eq!(listed(&store)[0].headers.message_id.as_str(), "list@x");
        let count = |sql: &str, id: &str| -> i64 {
            store
                .conn
                .query_row(sql, params![id], |row| row.get(0))
                .unwrap()
        };
        let vectors = "SELECT COUNT(*) FROM message_embeddings WHERE message_id = ?1";
        let queued = "SELECT COUNT(*) FROM embedding_queue WHERE message_id = ?1";
        assert_eq!(
            count(vectors, "sent@x"),
            0,
            "no orphan vector for the removed copy"
        );
        assert_eq!(
            count(queued, "sent@x"),
            0,
            "nothing queued under the removed id"
        );
        assert_eq!(
            count(queued, "list@x"),
            1,
            "the kept row is queued for a new vector"
        );
    }

    #[test]
    fn absorb_keeps_a_label_term_that_lived_only_on_the_deleted_row() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut sent = msg("sent@x", None, None, "Hello", "sent body");
        sent.headers.date_unix = 1_700_000_000;
        sent.labels = vec!["work".into()];
        sent.folders = vec!["Sent".into()];
        store.upsert_message(&sent).unwrap();
        let mut list = msg("list@x", None, None, "[list] Hello", "list body");
        list.headers.date_unix = 1_700_000_000;
        list.list_id = Some("list.example".into());
        list.folders = vec!["INBOX".into()];
        store.upsert_message(&list).unwrap();
        let fingerprint = crate::message::body_fingerprint("hello body");
        store
            .conn
            .execute(
                "UPDATE messages SET body_text = 'hello body', snippet = 'hello body',
                    body_fingerprint = ?1
                 WHERE account_id = 'a1'",
                params![fingerprint],
            )
            .unwrap();
        let mut again = msg("sent@x", None, None, "Hello", "hello body");
        again.headers.date_unix = 1_700_000_000;
        again.folders = vec!["Sent".into()];
        store.upsert_message(&again).unwrap();
        let work: i64 = store
            .conn
            .query_row(
                "SELECT df FROM search_terms WHERE account_id = 'a1' AND term = 'work'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(work, 1);
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "label:work")
                .unwrap()
                .hits
                .len(),
            1
        );
    }

    #[test]
    fn upgrade_joins_a_stored_pair_and_rethreads_the_reply() {
        let path = std::env::temp_dir().join(format!(
            "fell-schema-v2-{}-rethread.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let fingerprint = crate::message::body_fingerprint("hello body");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '2');
                 CREATE TABLE accounts (
                    id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL,
                    email TEXT NOT NULL,
                    backend_kind TEXT NOT NULL
                 );
                 INSERT INTO accounts (id, display_name, email, backend_kind)
                    VALUES ('a1', 'Caleb', 'c@x', 'bridge');
                 CREATE TABLE messages (
                    rowid INTEGER PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    remote_id TEXT NOT NULL,
                    thread_id TEXT NOT NULL,
                    proton_conversation_id TEXT,
                    in_reply_to TEXT,
                    references_header TEXT NOT NULL,
                    from_name TEXT,
                    from_email TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    date_unix INTEGER NOT NULL,
                    snippet TEXT NOT NULL,
                    body_text TEXT,
                    unread INTEGER NOT NULL DEFAULT 1,
                    folders TEXT NOT NULL DEFAULT '',
                    labels TEXT NOT NULL DEFAULT '',
                    starred INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0,
                    snooze_until INTEGER,
                    body_fingerprint TEXT NOT NULL DEFAULT '',
                    list_id TEXT,
                    UNIQUE (account_id, message_id)
                 );",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread, folders,
                    labels, starred, archived, snooze_until, body_fingerprint
                 ) VALUES (
                    'a1', 'sent@x', 'imap:sent', 'sent-thread', '', 'from@x', 'Hello',
                    1700000000, 'hello body', 'hello body', 1, 'Sent', 'work', 1, 1,
                    1893456000, ?1
                 )",
                params![fingerprint],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread, folders,
                    body_fingerprint, list_id
                 ) VALUES (
                    'a1', 'list@x', 'imap:inbox', 'list-thread', '', 'from@x', '[list] Hello',
                    1700000000, 'hello body', 'hello body', 1, 'INBOX', ?1, 'list.example'
                 )",
                params![fingerprint],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO messages (
                    account_id, message_id, remote_id, thread_id, in_reply_to, references_header,
                    from_email, subject, date_unix, snippet, body_text, unread, folders,
                    body_fingerprint
                 ) VALUES (
                    'a1', 'reply@x', 'imap:reply', 'reply-thread', 'sent@x', 'sent@x',
                    'from@x', 'Re: Hello', 1700001000, 'thanks', 'thanks', 1, 'INBOX', ''
                 )",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let version: String = store
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");
        let rows = listed(&store);
        assert_eq!(rows.len(), 2);
        let kept = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "list@x")
            .unwrap();
        let reply = rows
            .iter()
            .find(|message| message.headers.message_id.as_str() == "reply@x")
            .unwrap();
        assert_eq!(reply.thread_id, kept.thread_id);
        assert!(kept.starred);
        assert!(kept.archived);
        assert_eq!(kept.snooze_until, Some(1_893_456_000));
        assert_eq!(kept.labels, vec!["work".to_string()]);
        assert!(!kept.folders.iter().any(|folder| folder == "INBOX"));
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upgrade_joins_a_v2_pair_that_has_no_list_id() {
        let path = std::env::temp_dir().join(format!(
            "fell-schema-v2-{}-no-list.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let fingerprint = crate::message::body_fingerprint("hello body");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '2');
                 CREATE TABLE accounts (
                    id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL,
                    email TEXT NOT NULL,
                    backend_kind TEXT NOT NULL
                 );
                 INSERT INTO accounts (id, display_name, email, backend_kind)
                    VALUES ('a1', 'Caleb', 'c@x', 'bridge');
                 CREATE TABLE messages (
                    rowid INTEGER PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    remote_id TEXT NOT NULL,
                    thread_id TEXT NOT NULL,
                    proton_conversation_id TEXT,
                    in_reply_to TEXT,
                    references_header TEXT NOT NULL,
                    from_name TEXT,
                    from_email TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    date_unix INTEGER NOT NULL,
                    snippet TEXT NOT NULL,
                    body_text TEXT,
                    unread INTEGER NOT NULL DEFAULT 1,
                    folders TEXT NOT NULL DEFAULT '',
                    labels TEXT NOT NULL DEFAULT '',
                    arrived_via TEXT NOT NULL DEFAULT '',
                    to_line TEXT NOT NULL DEFAULT '',
                    cc_line TEXT NOT NULL DEFAULT '',
                    bcc_line TEXT NOT NULL DEFAULT '',
                    starred INTEGER NOT NULL DEFAULT 0,
                    has_attachment INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0,
                    snooze_until INTEGER,
                    body_fingerprint TEXT NOT NULL DEFAULT '',
                    UNIQUE (account_id, message_id)
                 );",
            )
            .unwrap();
            for (id, subject, folder) in [
                ("sent@x", "Hello", "Sent"),
                ("list@x", "[list] Hello", "INBOX"),
            ] {
                conn.execute(
                    "INSERT INTO messages (
                        account_id, message_id, remote_id, thread_id, references_header,
                        from_email, subject, date_unix, snippet, body_text, unread, folders,
                        body_fingerprint
                     ) VALUES (
                        'a1', ?1, ?1, ?1, '', 'from@x', ?2, 1700000000, 'hello body',
                        'hello body', 1, ?3, ?4
                     )",
                    params![id, subject, folder, fingerprint],
                )
                .unwrap();
            }
        }
        let store = Store::open(&path).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 1);
        assert!(rows[0].list_id.is_none());
        assert!(rows[0].folders.iter().any(|folder| folder == "Sent"));
        assert!(rows[0].folders.iter().any(|folder| folder == "INBOX"));
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upgrade_from_v2_queues_the_merged_row_for_its_parts() {
        let path = std::env::temp_dir().join(format!(
            "fell-schema-v2-{}-bodies.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let fingerprint = crate::message::body_fingerprint("hello body");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '2');
                 CREATE TABLE accounts (
                    id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL,
                    email TEXT NOT NULL,
                    backend_kind TEXT NOT NULL
                 );
                 INSERT INTO accounts (id, display_name, email, backend_kind)
                    VALUES ('a1', 'Caleb', 'c@x', 'bridge');
                 CREATE TABLE messages (
                    rowid INTEGER PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    remote_id TEXT NOT NULL,
                    thread_id TEXT NOT NULL,
                    proton_conversation_id TEXT,
                    in_reply_to TEXT,
                    references_header TEXT NOT NULL,
                    from_name TEXT,
                    from_email TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    date_unix INTEGER NOT NULL,
                    snippet TEXT NOT NULL,
                    body_text TEXT,
                    unread INTEGER NOT NULL DEFAULT 1,
                    folders TEXT NOT NULL DEFAULT '',
                    labels TEXT NOT NULL DEFAULT '',
                    arrived_via TEXT NOT NULL DEFAULT '',
                    to_line TEXT NOT NULL DEFAULT '',
                    cc_line TEXT NOT NULL DEFAULT '',
                    bcc_line TEXT NOT NULL DEFAULT '',
                    starred INTEGER NOT NULL DEFAULT 0,
                    has_attachment INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0,
                    snooze_until INTEGER,
                    body_fingerprint TEXT NOT NULL DEFAULT '',
                    UNIQUE (account_id, message_id)
                 );",
            )
            .unwrap();
            // Only the copy the merge deletes names a server copy.
            for (id, remote, subject, folder) in [
                ("sent@x", "imap:7:1:Sent", "Hello", "Sent"),
                ("list@x", "local-list", "[list] Hello", "INBOX"),
            ] {
                conn.execute(
                    "INSERT INTO messages (
                        account_id, message_id, remote_id, thread_id, references_header,
                        from_email, subject, date_unix, snippet, body_text, unread, folders,
                        body_fingerprint
                     ) VALUES (
                        'a1', ?1, ?2, ?1, '', 'from@x', ?3, 1700000000, 'hello body',
                        'hello body', 1, ?4, ?5
                     )",
                    params![id, remote, subject, folder, fingerprint],
                )
                .unwrap();
            }
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(listed(&store).len(), 1, "the v3 upgrade merged the pair");
        let wanted = store.missing_bodies(&account().id, 10).unwrap();
        assert_eq!(wanted.len(), 1, "{wanted:?}");
        assert_eq!(wanted[0].copies, vec![RemoteId("imap:7:1:Sent".into())]);
        let version: String = store
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "5");

        // A crash after the tables exist but before version 4 is stamped:
        // the next open queues again.
        store
            .conn
            .execute_batch(
                "DELETE FROM body_backfill;
                 UPDATE meta SET value = '3' WHERE key = 'schema_version';",
            )
            .unwrap();
        drop(store);
        let store = Store::open(&path).unwrap();
        assert_eq!(store.missing_bodies(&account().id, 10).unwrap(), wanted);
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn upgrade_keeps_two_sent_replies_that_share_a_short_body() {
        let path = std::env::temp_dir().join(format!(
            "fell-schema-v2-{}-thanks.sqlite",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let fingerprint = crate::message::body_fingerprint("Thanks!");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO meta (key, value) VALUES ('schema_version', '2');
                 CREATE TABLE accounts (
                    id TEXT PRIMARY KEY,
                    display_name TEXT NOT NULL,
                    email TEXT NOT NULL,
                    backend_kind TEXT NOT NULL
                 );
                 INSERT INTO accounts (id, display_name, email, backend_kind)
                    VALUES ('a1', 'Caleb', 'c@x', 'bridge');
                 CREATE TABLE messages (
                    rowid INTEGER PRIMARY KEY,
                    account_id TEXT NOT NULL,
                    message_id TEXT NOT NULL,
                    remote_id TEXT NOT NULL,
                    thread_id TEXT NOT NULL,
                    proton_conversation_id TEXT,
                    in_reply_to TEXT,
                    references_header TEXT NOT NULL,
                    from_name TEXT,
                    from_email TEXT NOT NULL,
                    subject TEXT NOT NULL,
                    date_unix INTEGER NOT NULL,
                    snippet TEXT NOT NULL,
                    body_text TEXT,
                    unread INTEGER NOT NULL DEFAULT 1,
                    folders TEXT NOT NULL DEFAULT '',
                    labels TEXT NOT NULL DEFAULT '',
                    arrived_via TEXT NOT NULL DEFAULT '',
                    to_line TEXT NOT NULL DEFAULT '',
                    cc_line TEXT NOT NULL DEFAULT '',
                    bcc_line TEXT NOT NULL DEFAULT '',
                    starred INTEGER NOT NULL DEFAULT 0,
                    has_attachment INTEGER NOT NULL DEFAULT 0,
                    archived INTEGER NOT NULL DEFAULT 0,
                    snooze_until INTEGER,
                    body_fingerprint TEXT NOT NULL DEFAULT '',
                    UNIQUE (account_id, message_id)
                 );",
            )
            .unwrap();
            for (id, subject, date) in [
                ("dinner@x", "Re: Dinner Friday", 1_700_000_000_i64),
                ("budget@x", "Re: Q3 budget", 1_700_000_000 + 2 * 60 * 60),
            ] {
                conn.execute(
                    "INSERT INTO messages (
                        account_id, message_id, remote_id, thread_id, references_header,
                        from_email, subject, date_unix, snippet, body_text, unread, folders,
                        body_fingerprint
                     ) VALUES (
                        'a1', ?1, ?1, ?1, '', 'from@x', ?2, ?3, 'Thanks!', 'Thanks!', 1, 'Sent', ?4
                     )",
                    params![id, subject, date, fingerprint],
                )
                .unwrap();
            }
        }
        let store = Store::open(&path).unwrap();
        let rows = listed(&store);
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|message| message.headers.subject == "Re: Dinner Friday")
        );
        assert!(
            rows.iter()
                .any(|message| message.headers.subject == "Re: Q3 budget")
        );
        drop(store);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn reply_to_and_attachment_names_survive_a_second_copy() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut first = msg_for("a1", "rt@x", None, None, "Report", "see attached");
        first.reply_to = Some(vec![Address {
            name: Some("List".into()),
            email: "list@x".into(),
        }]);
        first.attachment_names = vec!["report.pdf".into()];
        first.has_attachment = true;
        store.upsert_message(&first).unwrap();
        let read = store
            .message(&account().id, &MessageId::new("<rt@x>"))
            .unwrap()
            .unwrap();
        assert_eq!(read.reply_to, first.reply_to);
        assert_eq!(read.attachment_names, vec!["report.pdf"]);
        // A later copy with no Reply-To keeps the first; names are unioned.
        let mut second = first.clone();
        second.reply_to = Some(Vec::new());
        second.attachment_names = vec!["report.pdf".into(), "chart.png".into()];
        store.upsert_message(&second).unwrap();
        let read = store
            .message(&account().id, &MessageId::new("rt@x"))
            .unwrap()
            .unwrap();
        assert_eq!(read.reply_to, first.reply_to);
        assert_eq!(read.attachment_names, vec!["report.pdf", "chart.png"]);
        assert!(
            store
                .message(&account().id, &MessageId::new("missing@x"))
                .unwrap()
                .is_none()
        );
    }
}

#[cfg(test)]
mod fts_check {
    /// After the first open sets up the index and its triggers, later opens
    /// find nothing to change, so they take no write lock there.
    #[test]
    fn a_settled_mailbox_needs_no_fts_write() {
        let store = super::Store::open_in_memory().unwrap();
        assert!(super::fts_is_current(&store.conn).unwrap());
        store
            .conn
            .execute_batch("DROP TRIGGER messages_au;")
            .unwrap();
        assert!(!super::fts_is_current(&store.conn).unwrap());
        super::ensure_fts(&store.conn).unwrap();
        assert!(super::fts_is_current(&store.conn).unwrap());
    }
}

#[cfg(test)]
mod open_while_writing {
    /// Opens of one mailbox from several threads, beside a held store whose
    /// indexer writes, all succeed. Each open runs schema checks that write;
    /// a deferred transaction there failed at once with "database is locked"
    /// instead of waiting, about one open in five.
    #[test]
    fn concurrent_opens_wait_instead_of_failing() {
        let root = std::env::temp_dir().join(format!(
            "fell-open-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("mail.db");
        let _held = super::Store::open(&db).unwrap();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let db = db.clone();
                std::thread::spawn(move || {
                    (0..15)
                        .filter_map(|_| super::Store::open(&db).err().map(|err| err.to_string()))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        let failures: Vec<String> = workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect();
        assert!(
            failures.is_empty(),
            "{} failed: {:?}",
            failures.len(),
            failures.first()
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
