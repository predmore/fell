use std::collections::{HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::account::{Account, AccountId, BackendKind};
use crate::backend::MailboxDelta;
use crate::message::{Address, Message, MessageHeaders, MessageId, RemoteId};
use crate::search::{SearchHit, fts5_phrase};
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
    FOREIGN KEY(account_id) REFERENCES accounts(id),
    UNIQUE (account_id, message_id)
);

CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(account_id, thread_id);
CREATE INDEX IF NOT EXISTS idx_messages_account_date ON messages(account_id, date_unix DESC);

CREATE VIRTUAL TABLE IF NOT EXISTS messages_fts USING fts5(
    subject,
    sender,
    body,
    account_id UNINDEXED,
    message_id UNINDEXED
);

CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
    INSERT INTO messages_fts(subject, sender, body, account_id, message_id)
    VALUES (new.subject, new.from_email, COALESCE(new.body_text, new.snippet),
            new.account_id, new.message_id);
END;

CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
    DELETE FROM messages_fts
    WHERE account_id = old.account_id AND message_id = old.message_id;
END;

DROP TRIGGER IF EXISTS messages_au;
CREATE TRIGGER messages_au AFTER UPDATE OF subject, from_email, body_text, snippet ON messages BEGIN
    DELETE FROM messages_fts
    WHERE account_id = old.account_id AND message_id = old.message_id;
    INSERT INTO messages_fts(subject, sender, body, account_id, message_id)
    VALUES (new.subject, new.from_email, COALESCE(new.body_text, new.snippet),
            new.account_id, new.message_id);
END;
"#;

const SELECT_MESSAGE_COLS: &str =
    "account_id, message_id, remote_id, thread_id, proton_conversation_id,
        in_reply_to, references_header, from_name, from_email, subject,
        date_unix, snippet, body_text, unread, folders, labels, arrived_via";

/// On-disk mailbox. The UI reads this; sync writes it.
pub struct Store {
    conn: Connection,
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
        let conn = Connection::open(path)?;
        Self::from_connection(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        conn.execute_batch(SCHEMA)?;
        ensure_message_columns(&conn)?;
        conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', '1')",
            [],
        )?;
        Ok(Self { conn })
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
        let tx = self.conn.unchecked_transaction()?;
        for message in &delta.messages {
            if &message.account_id != account_id {
                return Err(crate::Error::Backend(format!(
                    "delta for account {} includes message for {}",
                    account_id.as_str(),
                    message.account_id.as_str()
                )));
            }
            insert_message(&tx, message)?;
        }
        if !delta.messages.is_empty() {
            rethread(&tx, account_id)?;
        }
        if let Some(cursor) = &delta.cursor {
            set_sync_cursor_on(&tx, account_id, cursor)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn upsert_message(&self, message: &Message) -> Result<ThreadId> {
        let tx = self.conn.unchecked_transaction()?;
        insert_message(&tx, message)?;
        rethread(&tx, &message.account_id)?;
        let thread_id =
            thread_id_for_message_id(&tx, &message.account_id, &message.headers.message_id)?
                .ok_or_else(|| {
                    crate::Error::Backend("upserted message missing after rethread".into())
                })?;
        tx.commit()?;
        Ok(thread_id)
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

    pub fn search(&self, account_id: &AccountId, query: &str) -> Result<Vec<SearchHit>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let phrase = fts5_phrase(query);
        let mut stmt = self.conn.prepare(
            "SELECT m.message_id, m.thread_id, m.subject, m.snippet
             FROM messages_fts
             JOIN messages m
               ON m.message_id = messages_fts.message_id
              AND m.account_id = messages_fts.account_id
             WHERE m.account_id = ?1 AND messages_fts MATCH ?2
             ORDER BY rank",
        )?;
        let rows = stmt.query_map(params![account_id.as_str(), phrase], |row| {
            Ok(SearchHit {
                message_id: MessageId(row.get(0)?),
                thread_id: ThreadId(row.get(1)?),
                subject: row.get(2)?,
                snippet: row.get(3)?,
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }
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
            "SELECT thread_id FROM messages WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), message_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(id.map(ThreadId))
}

fn insert_message(conn: &Connection, message: &Message) -> Result<ThreadId> {
    if let Some(stored) = find_arrival(conn, &message.account_id, &message.headers.message_id)? {
        return update_same_id(conn, message, &stored);
    }
    insert_new(conn, message)
}

fn insert_new(conn: &Connection, message: &Message) -> Result<ThreadId> {
    let account_id = &message.account_id;
    let thread_id = resolve_thread_id(&message.headers, |parent| {
        thread_id_for_message_id(conn, account_id, parent)
    })?;
    let references = references_header(&message.headers);
    conn.execute(
        "INSERT INTO messages (
            account_id, message_id, remote_id, thread_id, proton_conversation_id,
            in_reply_to, references_header, from_name, from_email, subject,
            date_unix, snippet, body_text, unread, folders, labels, arrived_via
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17
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
            union_tokens("", &message.folders, false),
            union_tokens("", &message.labels, false),
            union_tokens("", &message.arrived_via, true),
        ],
    )?;
    Ok(thread_id)
}

fn update_same_id(conn: &Connection, message: &Message, stored: &Arrival) -> Result<ThreadId> {
    let account_id = &message.account_id;
    let thread_id = resolve_thread_id(&message.headers, |parent| {
        thread_id_for_message_id(conn, account_id, parent)
    })?;
    let references = references_header(&message.headers);
    let folders = union_tokens(&stored.folders, &message.folders, false);
    let labels = union_tokens(&stored.labels, &message.labels, false);
    let arrived_via = union_tokens(&stored.arrived_via, &message.arrived_via, true);
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
            arrived_via = ?15
         WHERE account_id = ?16 AND message_id = ?17",
        params![
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
            arrived_via,
            message.account_id.as_str(),
            message.headers.message_id.as_str(),
        ],
    )?;
    Ok(thread_id)
}

struct Arrival {
    folders: String,
    labels: String,
    arrived_via: String,
}

fn find_arrival(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &MessageId,
) -> Result<Option<Arrival>> {
    let hit = conn
        .query_row(
            "SELECT folders, labels, arrived_via FROM messages
             WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), message_id.as_str()],
            |row| {
                Ok(Arrival {
                    folders: row.get(0)?,
                    labels: row.get(1)?,
                    arrived_via: row.get(2)?,
                })
            },
        )
        .optional()?;
    Ok(hit)
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

    let by_message: HashMap<MessageId, ThreadId> = rows
        .iter()
        .map(|row| (row.headers.message_id.clone(), row.thread_id.clone()))
        .collect();

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

fn split_field(raw: &str) -> Vec<String> {
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

fn ensure_message_columns(conn: &Connection) -> Result<()> {
    let have = column_names(conn, "messages")?;
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
    ];
    for (name, ddl) in missing {
        if !have.iter().any(|column| column == name) {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
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

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
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
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message_id.as_str(), "a@x");
        let body_hits = store.search(&AccountId("a1".into()), "pay me").unwrap();
        assert_eq!(body_hits.len(), 1);
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
                },
            )
            .unwrap();
        store
            .apply_delta(
                &AccountId("a1".into()),
                &MailboxDelta {
                    messages: vec![msg("root@x", Some("conv-9"), None, "Root", "root")],
                    cursor: None,
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
                .is_empty()
        );
        assert_eq!(
            store
                .search(&AccountId("a1".into()), "rootbody")
                .unwrap()
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
}
