use std::collections::{HashMap, HashSet};
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
use crate::message::{
    Address, Message, MessageHeaders, MessageId, RemoteId, body_fingerprint, decode_addresses,
    encode_addresses,
};
use crate::search::{
    IndexedText, SearchHit, SearchResults, fts5_phrase, sync_terms, terms_for_indexed,
};
use crate::threading::{ThreadId, header_parent_thread_id, resolve_thread_id};

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

const DROP_FTS_TRIGGERS: &str = r#"
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
        to_line, cc_line, bcc_line, starred, has_attachment, archived, snooze_until, list_id";

/// On-disk mailbox. The UI reads this; sync writes it.
pub struct Store {
    /// File-backed stores fill body vectors here. In-memory stores leave this
    /// empty so tests can index on the calling thread.
    indexer: Option<crate::embed::Indexer>,
    pub(crate) conn: Connection,
    path: Option<std::path::PathBuf>,
    /// Shared lock on the mailbox inode, held until this store is dropped.
    /// Dropped after the SQLite connection so a new opener cannot migrate
    /// the WAL while this connection still has the file.
    #[cfg(unix)]
    _mailbox_lock: Option<std::fs::File>,
}

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

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let requested = path.as_ref();
        // The mailbox holds message text. Create it owner-only before SQLite writes.
        create_private_file(requested)?;
        // One path string per inode. A symlink or a second hard link must not
        // open a different `-wal` while another connection still has the file.
        let claimed = claim_mailbox(requested)?;
        let mut store = Self::from_connection(claimed.conn)?;
        store.path = Some(claimed.path.clone());
        #[cfg(unix)]
        {
            store._mailbox_lock = Some(claimed.lock);
        }
        store.seal_private_files()?;
        // Embeddings are filled off this thread, and only in one process.
        // A lock problem skips the indexer. Open still returns the mailbox.
        store.indexer = crate::embed::Indexer::try_spawn(&claimed.path);
        Ok(store)
    }

    pub fn db_path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        ensure_message_columns(&conn)?;
        ensure_compose_columns(&conn)?;
        ensure_alias_columns(&conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_messages_fingerprint
                ON messages(account_id, body_fingerprint);",
        )?;
        ensure_fts(&conn)?;
        crate::search::ensure_vocabulary(&conn)?;
        crate::search::rebuild_vocabulary_if_empty(&conn)?;
        // Version 3 is stamped only after the fingerprint backfill finishes.
        // Batches commit on their own so a large mailbox is not one transaction.
        if schema_version(&conn)?.as_deref() != Some("3") {
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
        crate::embed::ensure_embedding_schema(&conn)?;
        // 3 adds message_embeddings, the local body-vector index.
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('schema_version', '3')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )?;
        Ok(Self {
            indexer: None,
            conn,
            path: None,
            #[cfg(unix)]
            _mailbox_lock: None,
        })
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

    pub fn apply_delta(&self, account_id: &AccountId, delta: &MailboxDelta) -> Result<()> {
        let tx = immediate_transaction(&self.conn)?;
        for message in &delta.messages {
            if &message.account_id != account_id {
                return Err(crate::Error::Backend(format!(
                    "delta for account {} includes message for {}",
                    account_id.as_str(),
                    message.account_id.as_str()
                )));
            }
            // Skip a folder archive or delete already took off this message.
            let Some(message) = crate::actions::message_for_delta(&tx, message)? else {
                continue;
            };
            insert_message(&tx, &message)?;
        }
        for flag in &delta.flags {
            tx.execute(
                "UPDATE messages SET unread = ?1
                 WHERE account_id = ?2 AND (
                     remote_id = ?3
                     OR message_id IN (
                         SELECT canonical_message_id FROM message_aliases
                         WHERE account_id = ?2 AND remote_id = ?3
                     )
                 )",
                params![
                    i64::from(flag.unread),
                    account_id.as_str(),
                    flag.remote_id.0
                ],
            )?;
        }
        if !delta.messages.is_empty() {
            rethread(&tx, account_id)?;
        }
        if let Some(cursor) = &delta.cursor {
            set_sync_cursor_on(&tx, account_id, cursor)?;
        }
        tx.commit()?;
        self.seal_private_files()?;
        Ok(())
    }

    pub fn upsert_message(&self, message: &Message) -> Result<ThreadId> {
        let tx = immediate_transaction(&self.conn)?;
        insert_message(&tx, message)?;
        rethread(&tx, &message.account_id)?;
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
        let Some((stored, _)) = find_stored(&tx, account_id, message_id)? else {
            tx.rollback()?;
            return Ok(false);
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
        sync_terms(&tx, account_id.as_str(), &old_terms, &HashSet::new())?;
        remove_message_embedding(&tx, account_id.as_str(), message_id.as_str())?;
        dequeue_message(&tx, account_id.as_str(), message_id.as_str())?;
        let removed = tx.execute(
            "DELETE FROM messages WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), stored.canonical_id],
        )?;
        tx.execute(
            "DELETE FROM message_aliases
             WHERE account_id = ?1
               AND (message_id = ?2 OR canonical_message_id = ?2 OR message_id = ?3)",
            params![
                account_id.as_str(),
                stored.canonical_id,
                message_id.as_str()
            ],
        )?;
        tx.commit()?;
        Ok(removed > 0)
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
        Ok(out)
    }

    /// True when `token` is already a message id, thread id, reply id, or address.
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

    /// Search one account in the local store.
    ///
    /// This reads SQLite only. It never calls a backend, so the hits change
    /// only after a local write. One hit per thread, best BM25 message first.
    /// When the typed query misses and a trigram correction hits, `did_you_mean`
    /// is set and `hits` are the corrected results.
    pub fn search(&self, account_id: &AccountId, query: &str) -> Result<SearchResults> {
        crate::search::execute(&self.conn, account_id, query)
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
    pub fn folder_names(&self, account_id: &AccountId) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT folders FROM messages WHERE account_id = ?1")?;
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

    /// Threads with at least one message in `folder`, newest activity first.
    ///
    /// A message with no folder is Inbox only when it is not archived. Matching
    /// is case-insensitive. `message_count` is the whole thread, including
    /// copies in other folders. SQLite returns one row per matching thread.
    pub fn folder_threads(
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

/// Pick the SQLite path for this inode and hold a shared lock on it.
///
/// SQLite names `-wal` and `-shm` from the path string. Two strings for one
/// inode (a symlink and its target, or two hard links) do not share locks.
/// Replay the requested path's WAL only while this process holds an exclusive
/// lock on the inode, and open the canonical path only when that checkpoint
/// reports `busy=0` and every frame was copied. Otherwise keep the path that
/// was requested so an existing connection keeps its WAL.
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
    for attempt in 0..8 {
        let db = std::fs::File::open(requested)?;
        let nlink = db.metadata()?.nlink();
        let exclusive = match db.try_lock_exclusive() {
            Ok(held) => held,
            // The filesystem cannot record a flock. A single name cannot split
            // its WAL, so the canonical file still opens.
            Err(_) if nlink <= 1 => {
                let conn = open_mailbox_db(&canonical)?;
                return Ok(ClaimedMailbox {
                    path: canonical,
                    conn,
                    lock: db,
                });
            }
            Err(err) => {
                return Err(crate::Error::Invalid(format!("cannot lock mailbox: {err}")));
            }
        };
        if !exclusive {
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
            // The holder may still be publishing. Retry while the lock is held
            // and the attribute is absent. The only name can still open after that.
            drop(db);
            if attempt == 7 && nlink <= 1 && absolute_opened(requested)? == canonical {
                let db = std::fs::File::open(requested)?;
                let conn = open_mailbox_db(&canonical)?;
                return Ok(ClaimedMailbox {
                    path: canonical,
                    conn,
                    lock: db,
                });
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            continue;
        }
        delay_publish_for_test();
        let requested_abs = absolute_opened(requested)?;
        let (path, conn) = if requested_abs == canonical {
            (canonical.clone(), open_mailbox_db(&canonical)?)
        } else {
            // Frames committed under the requested name live in its WAL.
            // Switch only when that checkpoint copied every frame.
            open_requested_or_canonical(&requested_abs, &canonical)?
        };
        let _ = publish_mailbox_path(&db, &path);
        // The exclusive lock stays granted. Converting it to shared drops it
        // on Linux before the shared lock exists.
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
    Err(crate::Error::Invalid(
        "mailbox is already open and its path was not published".into(),
    ))
}

#[cfg(unix)]
fn open_requested_or_canonical(
    requested: &Path,
    canonical: &Path,
) -> Result<(std::path::PathBuf, Connection)> {
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
        conn.pragma_update(None, "journal_mode", "WAL")?;
        Ok((requested.to_path_buf(), conn))
    }
}

fn open_mailbox_db(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
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

/// Picker name for a stored folder. Aliases share one row.
pub fn canonical_folder_name(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "inbox" => Some("INBOX"),
        "sent" => Some("Sent"),
        "drafts" => Some("Drafts"),
        "archive" | "archived" => Some("Archive"),
        "spam" | "junk" => Some("Spam"),
        "trash" | "deleted" => Some("Trash"),
        _ => None,
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

fn insert_message(conn: &Connection, message: &Message) -> Result<ThreadId> {
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
            )?
        {
            return absorb_duplicate(conn, message, &stored, &other);
        }
        return write_kept(conn, message, &stored, via_alias, &[], &[]);
    }
    if let Some(stored) = find_list_collapse(conn, message, &fingerprint, None, false)? {
        remember_alias(conn, message, &stored.canonical_id)?;
        return write_kept(conn, message, &stored, true, &[], &[]);
    }
    insert_new(conn, message, &fingerprint)
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
    conn.execute(
        "INSERT INTO messages (
            account_id, message_id, remote_id, thread_id, proton_conversation_id,
            in_reply_to, references_header, from_name, from_email, subject,
            date_unix, snippet, body_text, unread, folders, labels, arrived_via,
            to_line, cc_line, bcc_line, starred, has_attachment, archived, snooze_until,
            body_fingerprint, list_id
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
            ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26
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
            i64::from(message.has_attachment),
            i64::from(message.archived),
            message.snooze_until,
            fingerprint,
            list_id,
        ],
    )?;
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
    extra_labels: &[String],
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
    let mut label_in = message.labels.clone();
    label_in.extend(extra_labels.iter().cloned());
    // `extra_*` is the deleted copy. It belongs in the value written here, not
    // in the previous terms, or those tokens are both sides of the diff and
    // then disappear when the deleted row's postings are dropped.
    let mut folders = union_tokens(&stored.folders, &folder_in, false);
    if archived {
        folders = strip_inbox_tokens(&folders);
    }
    let labels = union_tokens(&stored.labels, &label_in, false);
    let arrived_via = union_tokens(&stored.arrived_via, &message.arrived_via, true);
    let to_line = union_addresses(&stored.to_line, &message.to);
    let cc_line = union_addresses(&stored.cc_line, &message.cc);
    let bcc_line = union_addresses(&stored.bcc_line, &message.bcc);
    let has_attachment = stored.has_attachment || message.has_attachment;
    let list_id = merged_list_id(&stored.list_id, &message.list_id);
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
    // and folder live on the alias, so neither is discarded.
    let remote_id = if alias && !stored.remote_id.is_empty() {
        remember_alias(conn, message, &stored.canonical_id)?;
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
            list_id = ?24
         WHERE account_id = ?25 AND message_id = ?26",
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
            i64::from(message.unread),
            folders,
            labels,
            arrived_via,
            to_line,
            cc_line,
            bcc_line,
            i64::from(stored.starred || message.starred),
            i64::from(has_attachment),
            i64::from(stored.archived || message.archived),
            kept_snooze(stored.snooze_until, message.snooze_until),
            fingerprint,
            list_id,
            message.account_id.as_str(),
            stored.canonical_id,
        ],
    )?;
    sync_terms(conn, message.account_id.as_str(), &old_terms, &new_terms)?;
    invalidate_message_embedding(
        conn,
        message.account_id.as_str(),
        message.headers.message_id.as_str(),
        body,
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
}

const ARRIVAL_COLS: &str = "m.message_id, m.remote_id, m.proton_conversation_id, m.in_reply_to,
    m.references_header, m.from_name, m.from_email, m.subject, m.date_unix, m.snippet,
    m.body_text, m.folders, m.labels, m.arrived_via, m.to_line, m.cc_line, m.bcc_line,
    m.has_attachment, m.archived, m.starred, m.list_id, m.snooze_until";

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
    })
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
            let via_alias: i64 = row.get(22)?;
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
/// or equal. Subject is not an identity. More than one candidate stays as
/// separate rows.
fn find_list_collapse(
    conn: &Connection,
    message: &Message,
    fingerprint: &str,
    exclude: Option<&str>,
    unlabeled_ok: bool,
) -> Result<Option<Arrival>> {
    if fingerprint.is_empty() {
        return Ok(None);
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT {ARRIVAL_COLS}
         FROM messages m
         WHERE m.account_id = ?1 AND m.body_fingerprint = ?2 AND m.body_fingerprint != ''"
    ))?;
    let rows = stmt.query_map(
        params![message.account_id.as_str(), fingerprint],
        map_arrival,
    )?;
    let mut matches = Vec::new();
    for row in rows {
        let stored = row?;
        if exclude == Some(stored.canonical_id.as_str()) {
            continue;
        }
        if is_fingerprint_echo(message, &stored, unlabeled_ok) {
            matches.push(stored);
        }
    }
    let Some(only) = matches.pop() else {
        return Ok(None);
    };
    if !matches.is_empty() {
        return Ok(None);
    }
    // A row that already absorbed another Message-ID is not a unique target.
    // A later message with the same body stays its own row.
    if alias_points_at(conn, &message.account_id, &only.canonical_id)? {
        return Ok(None);
    }
    Ok(Some(only))
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
    let extra_labels = split_field(&doomed.labels);
    let keeper = fold_doomed_local_state(keeper, doomed);
    let thread = write_kept(conn, message, &keeper, true, &extra_folders, &extra_labels)?;
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
    conn.execute(
        "UPDATE message_aliases SET canonical_message_id = ?1
         WHERE account_id = ?2 AND canonical_message_id = ?3",
        params![
            keeper.canonical_id,
            message.account_id.as_str(),
            doomed.canonical_id,
        ],
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
        let groups = duplicate_fingerprint_groups(conn, after)?;
        if groups.is_empty() {
            break;
        }
        let mut batch = Vec::new();
        for group in &groups {
            after = group.min_rowid;
            let mut stmt = conn.prepare(&format!(
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
        let tx = conn.unchecked_transaction()?;
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
            let Some(other) =
                find_list_collapse(&tx, message, &fingerprint, Some(&stored.canonical_id), true)?
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

fn rethread_accounts(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("SELECT DISTINCT account_id FROM messages")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut accounts = Vec::new();
    for row in rows {
        accounts.push(row?);
    }
    drop(stmt);
    for account_id in accounts {
        rethread(conn, &AccountId(account_id))?;
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

fn is_fingerprint_echo(incoming: &Message, stored: &Arrival, unlabeled_ok: bool) -> bool {
    let incoming_list = clean_list_id(&incoming.list_id).is_some();
    let stored_list = list_is_set(&stored.list_id);
    // Old rows have no list id. Join those only when one copy is in Sent and
    // the other subject still has a list tag for the same underlying subject.
    // Two plain messages with the same body stay apart.
    let listed =
        incoming_list || stored_list || (unlabeled_ok && unlabeled_sent_and_list(incoming, stored));
    same_sender(&incoming.headers.from.email, &stored.from_email)
        && dates_within_echo_window(stored.date_unix, incoming.headers.date_unix)
        && listed
        && proton_compatible(
            &stored.proton_conversation_id,
            &incoming.headers.proton_conversation_id,
        )
}

fn unlabeled_sent_and_list(incoming: &Message, stored: &Arrival) -> bool {
    let incoming_sent = incoming
        .folders
        .iter()
        .any(|folder| folder.eq_ignore_ascii_case("Sent"));
    let stored_sent = split_field(&stored.folders)
        .iter()
        .any(|folder| folder.eq_ignore_ascii_case("Sent"));
    if incoming_sent == stored_sent {
        return false;
    }
    let list_subject = if incoming_sent {
        &stored.subject
    } else {
        incoming.headers.subject.as_str()
    };
    subject_has_list_prefix(list_subject)
        && subject_key(&incoming.headers.subject) == subject_key(&stored.subject)
}

fn subject_has_list_prefix(subject: &str) -> bool {
    let mut rest = subject.trim();
    while let Some(after) = strip_reply_prefix(rest) {
        rest = after.trim();
    }
    strip_list_prefix(rest).is_some()
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
        let mut stmt = conn.prepare(
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
            break;
        }
        let tx = conn.unchecked_transaction()?;
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

/// Threading columns only — bodies stay on disk.
struct ThreadingRow {
    rowid: i64,
    thread_id: ThreadId,
    headers: MessageHeaders,
}

/// Re-resolve thread ids for an account in one in-memory pass.
///
/// A proton id absorbs a non-proton header thread. Two different proton ids
/// stay apart. Non-proton merges keep the earliest already-assigned id.
fn rethread(conn: &Connection, account_id: &AccountId) -> Result<()> {
    let rows = load_threading_rows(conn, account_id)?;
    if rows.is_empty() {
        return Ok(());
    }

    let mut by_message: HashMap<MessageId, ThreadId> = rows
        .iter()
        .map(|row| (row.headers.message_id.clone(), row.thread_id.clone()))
        .collect();
    for (alias, canonical) in load_aliases(conn, account_id)? {
        if let Some(thread) = by_message.get(&canonical).cloned() {
            by_message.insert(alias, thread);
        }
    }

    let mut union = ThreadUnion::default();
    for row in &rows {
        union.note(row.thread_id.as_str(), row.rowid);
    }
    for row in &rows {
        let onto = resolve_thread_id(&row.headers, |parent| {
            Ok::<_, crate::Error>(by_message.get(parent).cloned())
        })?;
        let header = header_parent_thread_id(&row.headers, |parent| {
            Ok::<_, crate::Error>(by_message.get(parent).cloned())
        })?;
        union.union_prefer(row.thread_id.as_str(), onto.as_str());
        if let Some(header) = header {
            union.union_prefer(header.as_str(), onto.as_str());
        }
    }

    let mut written = HashSet::new();
    for row in &rows {
        let from = row.thread_id.as_str();
        let onto = union.find(from);
        if from == onto || !written.insert(from.to_string()) {
            continue;
        }
        conn.execute(
            "UPDATE messages SET thread_id = ?1 WHERE account_id = ?2 AND thread_id = ?3",
            params![onto.as_str(), account_id.as_str(), from],
        )?;
    }
    Ok(())
}

fn load_threading_rows(conn: &Connection, account_id: &AccountId) -> Result<Vec<ThreadingRow>> {
    let mut stmt = conn.prepare(
        "SELECT rowid, message_id, thread_id, proton_conversation_id, in_reply_to, references_header
         FROM messages WHERE account_id = ?1
         ORDER BY rowid ASC",
    )?;
    let rows = stmt.query_map(params![account_id.as_str()], |row| {
        let rowid: i64 = row.get(0)?;
        let message_id: String = row.get(1)?;
        let thread_id: String = row.get(2)?;
        let proton_conversation_id: Option<String> = row.get(3)?;
        let in_reply_to: Option<String> = row.get(4)?;
        let references: String = row.get(5)?;
        Ok(ThreadingRow {
            rowid,
            thread_id: ThreadId(thread_id),
            headers: MessageHeaders {
                message_id: MessageId(message_id),
                proton_conversation_id,
                in_reply_to: in_reply_to.map(MessageId),
                references: parse_references(&references),
                from: Address {
                    name: None,
                    email: String::new(),
                },
                subject: String::new(),
                date_unix: 0,
            },
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

#[derive(Default)]
struct ThreadUnion {
    parent: HashMap<String, String>,
    /// Lowest message rowid currently wearing this thread id.
    earliest: HashMap<String, i64>,
}

impl ThreadUnion {
    fn note(&mut self, thread_id: &str, rowid: i64) {
        self.earliest
            .entry(thread_id.to_string())
            .and_modify(|rank| *rank = (*rank).min(rowid))
            .or_insert(rowid);
    }

    fn find(&mut self, id: &str) -> String {
        let parent = self
            .parent
            .get(id)
            .cloned()
            .unwrap_or_else(|| id.to_string());
        if parent == id {
            self.parent.entry(id.to_string()).or_insert(id.to_string());
            return parent;
        }
        let root = self.find(&parent);
        self.parent.insert(id.to_string(), root.clone());
        root
    }

    fn rank(&self, root: &str) -> i64 {
        self.earliest.get(root).copied().unwrap_or(i64::MAX)
    }

    /// Merge `member` with `preferred`.
    ///
    /// Two different `proton:` roots stay apart. A proton root absorbs a
    /// non-proton one. Otherwise the earliest already-assigned id wins, so a
    /// caller that saved the first id still lists the messages.
    fn union_prefer(&mut self, member: &str, preferred: &str) {
        let member_root = self.find(member);
        let preferred_root = self.find(preferred);
        if member_root == preferred_root {
            return;
        }
        let member_proton = member_root.starts_with("proton:");
        let preferred_proton = preferred_root.starts_with("proton:");
        if member_proton && preferred_proton {
            return;
        }
        if preferred_proton || member_proton {
            let (from, onto) = if preferred_proton {
                (member_root, preferred_root)
            } else {
                (preferred_root, member_root)
            };
            self.parent.insert(from, onto);
            return;
        }
        let member_rank = self.rank(&member_root);
        let preferred_rank = self.rank(&preferred_root);
        let (keep, drop, rank) = if member_rank <= preferred_rank {
            (member_root, preferred_root, member_rank)
        } else {
            (preferred_root, member_root, preferred_rank)
        };
        self.parent.insert(drop, keep.clone());
        self.earliest.insert(keep, rank);
    }
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

fn union_tokens(existing: &str, incoming: &[String], address: bool) -> String {
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

/// Write lock from the first statement, so an embedding commit cannot
/// invalidate a snapshot this transaction already read.
pub(crate) fn immediate_transaction(conn: &Connection) -> Result<rusqlite::Transaction<'_>> {
    rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(Into::into)
}

fn ensure_fts(conn: &Connection) -> Result<()> {
    // Drop, create, and copy commit together. A killed process rolls the
    // upgrade back, so the next open tries again instead of keeping an empty
    // index whose columns merely look current.
    let tx = conn.unchecked_transaction()?;
    let have = column_names(&tx, "messages_fts")?;
    let needed = [
        "subject",
        "sender",
        "recipients",
        "body",
        "account_id",
        "message_id",
    ];
    let current = needed
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MailboxDelta;
    use crate::backend::SyncCursor;

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
        assert_eq!(version, "3");
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
        let near_thread = store.upsert_message(&near).unwrap();
        let mut echo = msg("near-list@x", None, None, "unrelated subject", "near body");
        echo.list_id = Some("list.example".into());
        echo.headers.date_unix = near.headers.date_unix + (5 * 60 * 60);
        assert_eq!(store.upsert_message(&echo).unwrap(), near_thread);
        assert_eq!(
            store
                .thread_messages(&AccountId("a1".into()), &near_thread)
                .unwrap()
                .len(),
            1
        );
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
        let mut first = msg("thanks-a@x", None, None, "First thanks", "Thanks");
        first.headers.date_unix = sent_at;
        store.upsert_message(&first).unwrap();
        let mut echo = msg("thanks-list@x", None, None, "[list] First thanks", "Thanks");
        echo.list_id = Some("list.example".into());
        echo.headers.date_unix = sent_at + 60;
        store.upsert_message(&echo).unwrap();
        assert_eq!(listed(&store).len(), 1);

        let mut later = msg("thanks-b@x", None, None, "Second thanks", "Thanks");
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
                        unread: false,
                    }],
                    cursor: None,
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
        assert_eq!(version, "3");
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
        assert_eq!(version, "3");
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
        assert_eq!(version, "3");
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
}
