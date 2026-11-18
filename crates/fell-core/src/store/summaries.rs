//! One row per thread for folder lists, kept current by writes.
//!
//! `thread_summary` holds what a list row shows: subject, latest snippet and
//! sender, participants, labels, and markers. `thread_folders` has one row
//! per (thread, folder) with the folder's unread state, so a folder list is
//! an index range instead of a scan of every message.
//!
//! Triggers queue every thread a message write touches in
//! `thread_summary_dirty`, whatever code made the write. Each write
//! transaction recomputes its queued threads before it commits (see
//! [`super::WriteTx`]). A reader treats a thread still in the queue as
//! unknown and computes it from `messages`, so a list is right even when a
//! writer skipped the refresh.

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use super::{FolderThread, decode_participants, folder_query_names, split_field, unique_tokens};
use crate::Result;
use crate::account::AccountId;
use crate::threading::ThreadId;

const SUMMARY_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS thread_summary (
    account_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    subject TEXT NOT NULL,
    snippet TEXT NOT NULL,
    last_date INTEGER NOT NULL,
    message_count INTEGER NOT NULL,
    starred INTEGER NOT NULL,
    has_attachment INTEGER NOT NULL,
    latest_email TEXT NOT NULL,
    latest_name TEXT,
    participants TEXT NOT NULL,
    labels TEXT NOT NULL,
    PRIMARY KEY (account_id, thread_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS thread_folders (
    account_id TEXT NOT NULL,
    folder_key TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    -- Name as stored on the earliest message with it. NULL for Inbox implied
    -- by a message with no folder, which is not a folder name.
    display TEXT,
    first_rowid INTEGER NOT NULL,
    -- Position of the name in that message's folder list.
    first_pos INTEGER NOT NULL,
    unread INTEGER NOT NULL,
    PRIMARY KEY (account_id, thread_id, folder_key)
) WITHOUT ROWID;
-- Folder lists read this, and the earliest copy of a folder name is its
-- first entry. A refresh deletes through the primary key.
CREATE INDEX IF NOT EXISTS idx_thread_folders_folder
    ON thread_folders(account_id, folder_key, first_rowid, first_pos);

-- One row per folder name in the account: its earliest stored spelling.
CREATE TABLE IF NOT EXISTS account_folders (
    account_id TEXT NOT NULL,
    folder_key TEXT NOT NULL,
    display TEXT NOT NULL,
    first_rowid INTEGER NOT NULL,
    first_pos INTEGER NOT NULL,
    PRIMARY KEY (account_id, folder_key)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS thread_summary_dirty (
    account_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    PRIMARY KEY (account_id, thread_id)
) WITHOUT ROWID;
"#;

/// Bump when [`SUMMARY_TRIGGERS`] changes. They are rebuilt only then, in
/// one transaction (see [`super::ensure_triggers`]).
const SUMMARY_TRIGGERS_VERSION: &str = "1";

// Inserts check NOT EXISTS instead of OR IGNORE: an UPSERT that fires a
// trigger overrides the trigger's own conflict clause.
const SUMMARY_TRIGGERS: &str = r#"
DROP TRIGGER IF EXISTS thread_summary_insert;
DROP TRIGGER IF EXISTS thread_summary_update;
DROP TRIGGER IF EXISTS thread_summary_delete;

CREATE TRIGGER thread_summary_insert AFTER INSERT ON messages BEGIN
    INSERT INTO thread_summary_dirty (account_id, thread_id)
    SELECT new.account_id, new.thread_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_summary_dirty
        WHERE account_id = new.account_id AND thread_id = new.thread_id
    );
END;

CREATE TRIGGER thread_summary_update AFTER UPDATE OF
    account_id, thread_id, subject, snippet, date_unix, unread, folders, archived,
    starred, has_attachment, from_email, from_name, labels
ON messages BEGIN
    INSERT INTO thread_summary_dirty (account_id, thread_id)
    SELECT old.account_id, old.thread_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_summary_dirty
        WHERE account_id = old.account_id AND thread_id = old.thread_id
    );
    INSERT INTO thread_summary_dirty (account_id, thread_id)
    SELECT new.account_id, new.thread_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_summary_dirty
        WHERE account_id = new.account_id AND thread_id = new.thread_id
    );
END;

CREATE TRIGGER thread_summary_delete AFTER DELETE ON messages BEGIN
    INSERT INTO thread_summary_dirty (account_id, thread_id)
    SELECT old.account_id, old.thread_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_summary_dirty
        WHERE account_id = old.account_id AND thread_id = old.thread_id
    );
END;
"#;

/// Create the tables and triggers, and fill them the first time a mailbox
/// opens with this code.
pub(super) fn ensure_thread_summaries(conn: &Connection) -> Result<()> {
    conn.execute_batch(SUMMARY_TABLES)?;
    super::ensure_triggers(
        conn,
        "summary_triggers",
        SUMMARY_TRIGGERS_VERSION,
        SUMMARY_TRIGGERS,
    )?;
    if summaries_are_current(conn)? {
        return Ok(());
    }
    let tx = super::immediate_transaction(conn)?;
    if summaries_are_current(&tx)? {
        return Ok(());
    }
    tx.execute_batch(
        "DELETE FROM thread_summary;
         DELETE FROM thread_folders;
         DELETE FROM account_folders;
         INSERT OR IGNORE INTO thread_summary_dirty (account_id, thread_id)
             SELECT DISTINCT account_id, thread_id FROM messages;
         INSERT INTO meta (key, value) VALUES ('thread_summary', '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value;",
    )?;
    // Commit recomputes every queued thread.
    tx.commit()
}

fn summaries_are_current(conn: &Connection) -> Result<bool> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'thread_summary'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.as_deref() == Some("1"))
}

/// Recompute every queued thread and empty the queue. Runs inside the write
/// transaction that queued them.
pub(super) fn refresh_dirty(conn: &Connection) -> Result<()> {
    let queued = {
        let mut stmt =
            conn.prepare_cached("SELECT account_id, thread_id FROM thread_summary_dirty")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    if queued.is_empty() {
        return Ok(());
    }
    let mut drop_summary =
        conn.prepare_cached("DELETE FROM thread_summary WHERE account_id = ?1 AND thread_id = ?2")?;
    let mut drop_folders =
        conn.prepare_cached("DELETE FROM thread_folders WHERE account_id = ?1 AND thread_id = ?2")?;
    let mut put_summary = conn.prepare_cached(
        "INSERT INTO thread_summary (
            account_id, thread_id, subject, snippet, last_date, message_count, starred,
            has_attachment, latest_email, latest_name, participants, labels
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    let mut put_folder = conn.prepare_cached(
        "INSERT INTO thread_folders (
            account_id, folder_key, thread_id, display, first_rowid, first_pos, unread
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut unqueue = conn.prepare_cached(
        "DELETE FROM thread_summary_dirty WHERE account_id = ?1 AND thread_id = ?2",
    )?;
    let mut old_keys = conn.prepare_cached(
        "SELECT folder_key FROM thread_folders WHERE account_id = ?1 AND thread_id = ?2",
    )?;
    // Folders whose earliest spelling may have changed.
    let mut touched: HashSet<(String, String)> = HashSet::new();
    for (account, thread) in &queued {
        let keys = old_keys.query_map(params![account, thread], |row| row.get::<_, String>(0))?;
        for key in keys {
            touched.insert((account.clone(), key?));
        }
        drop_summary.execute(params![account, thread])?;
        drop_folders.execute(params![account, thread])?;
        if let Some(summary) = compute(conn, account, thread)? {
            put_summary.execute(params![
                account,
                thread,
                summary.subject,
                summary.snippet,
                summary.last_date,
                summary.message_count as i64,
                i64::from(summary.starred),
                i64::from(summary.has_attachment),
                summary.latest_email,
                summary.latest_name,
                summary.participants,
                summary.labels,
            ])?;
            for folder in &summary.folders {
                touched.insert((account.clone(), folder.key.clone()));
                put_folder.execute(params![
                    account,
                    folder.key,
                    thread,
                    folder.display,
                    folder.first_rowid,
                    folder.first_pos as i64,
                    i64::from(folder.unread),
                ])?;
            }
        }
        unqueue.execute(params![account, thread])?;
    }
    let mut earliest = conn.prepare_cached(
        "SELECT display, first_rowid, first_pos FROM thread_folders
         WHERE account_id = ?1 AND folder_key = ?2 AND display IS NOT NULL
         ORDER BY first_rowid, first_pos
         LIMIT 1",
    )?;
    let mut put_name = conn.prepare_cached(
        "INSERT INTO account_folders (account_id, folder_key, display, first_rowid, first_pos)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(account_id, folder_key) DO UPDATE SET
            display = excluded.display,
            first_rowid = excluded.first_rowid,
            first_pos = excluded.first_pos",
    )?;
    let mut drop_name = conn
        .prepare_cached("DELETE FROM account_folders WHERE account_id = ?1 AND folder_key = ?2")?;
    for (account, key) in &touched {
        let first: Option<(String, i64, i64)> = earliest
            .query_row(params![account, key], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .optional()?;
        match first {
            Some((display, rowid, pos)) => {
                put_name.execute(params![account, key, display, rowid, pos])?;
            }
            None => {
                drop_name.execute(params![account, key])?;
            }
        }
    }
    Ok(())
}

struct Summary {
    subject: String,
    snippet: String,
    last_date: i64,
    message_count: usize,
    starred: bool,
    has_attachment: bool,
    latest_email: String,
    latest_name: Option<String>,
    /// `name \x1f email` per message, oldest first, joined by `\x1e`.
    participants: String,
    /// Non-empty label fields, oldest first, joined by `\x1e`.
    labels: String,
    folders: Vec<Membership>,
}

struct Membership {
    /// ASCII-lowercased, the way SQLite `lower()` folds the stored name.
    key: String,
    display: Option<String>,
    first_rowid: i64,
    first_pos: usize,
    unread: bool,
}

/// Summary of one thread from its messages, or `None` when it has none.
fn compute(conn: &Connection, account: &str, thread: &str) -> Result<Option<Summary>> {
    let mut stmt = conn.prepare_cached(
        "SELECT rowid, subject, snippet, date_unix, unread, folders, archived, starred,
                has_attachment, from_email, from_name, labels
         FROM messages
         WHERE account_id = ?1 AND thread_id = ?2
         ORDER BY date_unix ASC, rowid ASC",
    )?;
    let mut rows = stmt.query(params![account, thread])?;
    let mut summary: Option<Summary> = None;
    let mut participants: Vec<String> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    let mut folders: Vec<Membership> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
    while let Some(row) = rows.next()? {
        let rowid: i64 = row.get(0)?;
        let subject: String = row.get(1)?;
        let snippet: String = row.get(2)?;
        let date: i64 = row.get(3)?;
        let unread = row.get::<_, i64>(4)? != 0;
        let folder_field: String = row.get(5)?;
        let archived = row.get::<_, i64>(6)? != 0;
        let starred = row.get::<_, i64>(7)? != 0;
        let has_attachment = row.get::<_, i64>(8)? != 0;
        let from_email: String = row.get(9)?;
        let from_name: Option<String> = row.get(10)?;
        let label_field: String = row.get(11)?;

        participants.push(format!(
            "{}\u{1f}{from_email}",
            from_name.as_deref().unwrap_or("")
        ));
        if !label_field.is_empty() {
            labels.push(label_field);
        }
        let mut names: Vec<(String, Option<String>)> = split_field(&folder_field)
            .into_iter()
            .map(|name| (name.to_ascii_lowercase(), Some(name)))
            .collect();
        if folder_field.is_empty() && !archived {
            names.push(("inbox".into(), None));
        }
        for (pos, (key, display)) in names.into_iter().enumerate() {
            match at.get(&key) {
                Some(&index) => {
                    let entry = &mut folders[index];
                    entry.unread |= unread;
                    // Rows come in date order, not rowid order. The display
                    // name is the earliest-stored named copy; an implied
                    // Inbox never replaces one.
                    let earlier = (rowid, pos) < (entry.first_rowid, entry.first_pos);
                    if display.is_some() && (entry.display.is_none() || earlier) {
                        entry.first_rowid = rowid;
                        entry.first_pos = pos;
                        entry.display = display;
                    }
                }
                None => {
                    at.insert(key.clone(), folders.len());
                    folders.push(Membership {
                        key,
                        display,
                        first_rowid: rowid,
                        first_pos: pos,
                        unread,
                    });
                }
            }
        }
        match &mut summary {
            None => {
                summary = Some(Summary {
                    subject,
                    snippet,
                    last_date: date,
                    message_count: 1,
                    starred,
                    has_attachment,
                    latest_email: from_email,
                    latest_name: from_name,
                    participants: String::new(),
                    labels: String::new(),
                    folders: Vec::new(),
                });
            }
            Some(summary) => {
                // Rows come oldest first, so the last row is the latest.
                summary.snippet = snippet;
                summary.latest_email = from_email;
                summary.latest_name = from_name;
                summary.last_date = summary.last_date.max(date);
                summary.message_count += 1;
                summary.starred |= starred;
                summary.has_attachment |= has_attachment;
            }
        }
    }
    Ok(summary.map(|mut summary| {
        summary.participants = participants.join("\u{1e}");
        summary.labels = labels.join("\u{1e}");
        summary.folders = folders;
        summary
    }))
}

fn folder_thread(thread: &str, summary: &Summary, unread: bool) -> FolderThread {
    FolderThread {
        thread_id: ThreadId(thread.to_string()),
        subject: summary.subject.clone(),
        snippet: summary.snippet.clone(),
        last_date_unix: summary.last_date,
        message_count: summary.message_count,
        unread,
        participants: decode_participants(&summary.participants),
        latest_email: summary.latest_email.clone(),
        latest_name: summary.latest_name.clone(),
        labels: unique_tokens(&summary.labels),
        starred: summary.starred,
        has_attachment: summary.has_attachment,
    }
}

/// Rows for one folder. Without statistics SQLite would walk every row in
/// the account and probe each one, so the plan is pinned: the folder index
/// first (`INDEXED BY`), then each summary by key (`CROSS JOIN`).
fn folder_list_sql(keys: usize) -> String {
    let placeholders = (0..keys)
        .map(|index| format!("?{}", index + 2))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "SELECT ts.thread_id, ts.subject, ts.snippet, ts.last_date, ts.message_count,
                MAX(tf.unread), ts.starred, ts.has_attachment, ts.latest_email,
                ts.latest_name, ts.participants, ts.labels
         FROM thread_folders tf INDEXED BY idx_thread_folders_folder
         CROSS JOIN thread_summary ts
           ON ts.account_id = tf.account_id AND ts.thread_id = tf.thread_id
         WHERE tf.account_id = ?1 AND tf.folder_key IN ({placeholders})
           AND NOT EXISTS (
               SELECT 1 FROM thread_summary_dirty d
               WHERE d.account_id = tf.account_id AND d.thread_id = tf.thread_id
           )
         GROUP BY ts.thread_id"
    )
}

/// Threads with at least one message in `folder`, newest activity first.
pub(super) fn folder_threads(
    conn: &Connection,
    account_id: &AccountId,
    folder: &str,
) -> Result<Vec<FolderThread>> {
    in_one_snapshot(conn, || folder_threads_in(conn, account_id, folder))
}

/// Run `read` inside one read transaction. The summary rows and the queue
/// of threads to compute live must come from the same moment: otherwise a
/// write that skipped the refresh can commit between the two reads and a
/// thread is dropped or listed twice. A savepoint also nests inside a
/// write transaction.
fn in_one_snapshot<T>(conn: &Connection, read: impl FnOnce() -> Result<T>) -> Result<T> {
    conn.execute_batch("SAVEPOINT fell_list_read")?;
    let out = read();
    let released = conn.execute_batch("RELEASE fell_list_read");
    let out = out?;
    released?;
    Ok(out)
}

fn folder_threads_in(
    conn: &Connection,
    account_id: &AccountId,
    folder: &str,
) -> Result<Vec<FolderThread>> {
    let account = account_id.as_str();
    let keys: Vec<String> = folder_query_names(folder)
        .iter()
        .map(|name| name.to_ascii_lowercase())
        .collect();
    let sql = folder_list_sql(keys.len());
    let mut bind = Vec::with_capacity(1 + keys.len());
    bind.push(account.to_string());
    bind.extend(keys.iter().cloned());
    let mut threads = {
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(bind.iter()), |row| {
            let participants: String = row.get(10)?;
            let labels: String = row.get(11)?;
            Ok(FolderThread {
                thread_id: ThreadId(row.get(0)?),
                subject: row.get(1)?,
                snippet: row.get(2)?,
                last_date_unix: row.get(3)?,
                message_count: row.get::<_, i64>(4)? as usize,
                unread: row.get::<_, i64>(5)? != 0,
                starred: row.get::<_, i64>(6)? != 0,
                has_attachment: row.get::<_, i64>(7)? != 0,
                latest_email: row.get(8)?,
                latest_name: row.get(9)?,
                participants: decode_participants(&participants),
                labels: unique_tokens(&labels),
            })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for thread in queued_threads(conn, account)? {
        let Some(summary) = compute(conn, account, &thread)? else {
            continue;
        };
        let mut unread = None;
        for membership in &summary.folders {
            if keys.contains(&membership.key) {
                unread = Some(unread.unwrap_or(false) | membership.unread);
            }
        }
        if let Some(unread) = unread {
            threads.push(folder_thread(&thread, &summary, unread));
        }
    }
    threads.sort_by(|a, b| {
        b.last_date_unix
            .cmp(&a.last_date_unix)
            .then_with(|| a.thread_id.0.cmp(&b.thread_id.0))
    });
    Ok(threads)
}

/// Distinct folder names, as first stored, in the order they first appeared.
pub(super) fn folder_names(conn: &Connection, account_id: &AccountId) -> Result<Vec<String>> {
    in_one_snapshot(conn, || folder_names_in(conn, account_id))
}

fn folder_names_in(conn: &Connection, account_id: &AccountId) -> Result<Vec<String>> {
    let account = account_id.as_str();
    let queued = queued_threads(conn, account)?;
    if queued.is_empty() {
        let mut stmt = conn.prepare_cached(
            "SELECT display FROM account_folders WHERE account_id = ?1
             ORDER BY first_rowid, first_pos",
        )?;
        let rows = stmt.query_map(params![account], |row| row.get::<_, String>(0))?;
        return rows
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into);
    }
    // A writer skipped the refresh, so `account_folders` may be stale. Read
    // the up-to-date threads' rows and compute the queued ones.
    let mut seen: Vec<(i64, i64, String, String)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT tf.first_rowid, tf.first_pos, tf.folder_key, tf.display
             FROM thread_folders tf
             WHERE tf.account_id = ?1 AND tf.display IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM thread_summary_dirty d
                   WHERE d.account_id = tf.account_id AND d.thread_id = tf.thread_id
               )",
        )?;
        let rows = stmt.query_map(params![account], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for thread in queued {
        if let Some(summary) = compute(conn, account, &thread)? {
            for membership in summary.folders {
                if let Some(display) = membership.display {
                    seen.push((
                        membership.first_rowid,
                        membership.first_pos as i64,
                        membership.key,
                        display,
                    ));
                }
            }
        }
    }
    seen.sort_by_key(|(first, pos, _, _)| (*first, *pos));
    let mut keys = HashSet::new();
    Ok(seen
        .into_iter()
        .filter(|(_, _, key, _)| keys.insert(key.clone()))
        .map(|(_, _, _, display)| display)
        .collect())
}

fn queued_threads(conn: &Connection, account: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare_cached("SELECT thread_id FROM thread_summary_dirty WHERE account_id = ?1")?;
    let rows = stmt.query_map(params![account], |row| row.get::<_, String>(0))?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, BackendKind};
    use crate::actions::LabelChange;
    use crate::backend::{FlagUpdate, FolderRemoval, MailboxDelta, RemovalKind, Settle};
    use crate::message::{Address, Message, MessageHeaders, MessageId, RemoteId};
    use crate::store::Store;

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }

        fn chance(&mut self, percent: u64) -> bool {
            self.next() % 100 < percent
        }
    }

    fn account() -> Account {
        Account {
            id: AccountId("acct".into()),
            display_name: "Me".into(),
            email: "me@x".into(),
            backend_kind: BackendKind::Bridge,
        }
    }

    /// Folder names as servers spell them, including case variants of one
    /// folder and the aliases that list together.
    const FOLDERS: [&str; 9] = [
        "INBOX", "Inbox", "Sent", "Archive", "Archived", "Trash", "Junk", "Work", "",
    ];

    /// What a list reader asks for: canonical names, aliases, and odd case.
    const QUERIES: [&str; 8] = [
        "INBOX", "inbox", "Sent", "archive", "Archived", "Spam", "Trash", "work",
    ];

    fn message(rng: &mut Rng, index: usize, made: &[String]) -> Message {
        let id = format!("m{index}@x");
        let message_id = MessageId::new(&id);
        let parent = (!made.is_empty() && rng.chance(60))
            .then(|| MessageId::new(&made[rng.below(made.len())]));
        let mut folders = vec![FOLDERS[rng.below(FOLDERS.len())].to_string()];
        if rng.chance(20) {
            folders.push(FOLDERS[rng.below(FOLDERS.len())].to_string());
        }
        folders.retain(|folder| !folder.is_empty());
        let sender = rng.below(6);
        Message {
            account_id: account().id,
            headers: MessageHeaders {
                message_id: message_id.clone(),
                proton_conversation_id: None,
                in_reply_to: parent.clone(),
                references: parent.into_iter().collect(),
                from: Address {
                    name: sender.is_multiple_of(2).then(|| format!("Person {sender}")),
                    email: format!("p{sender}@x"),
                },
                subject: format!("subject {}", rng.below(30)),
                // Ties on purpose: order must still match.
                date_unix: 1_700_000_000 + rng.below(50) as i64 * 60,
            },
            // An IMAP copy, UID `index` in its first folder, so removals find it.
            remote_id: RemoteId(match folders.first() {
                Some(folder) => format!("imap:1:{index}:{folder}"),
                None => format!("r{index}"),
            }),
            thread_id: ThreadId::from_message_id(&message_id),
            snippet: format!("snippet {index}"),
            body_text: Some(format!("body {index} {}", rng.next())),
            unread: rng.chance(40),
            folders,
            labels: if rng.chance(15) {
                vec![format!("tag{}", rng.below(3))]
            } else {
                Vec::new()
            },
            arrived_via: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            starred: rng.chance(10),
            has_attachment: rng.chance(10),
            archived: rng.chance(5),
            snooze_until: None,
            list_id: None,
        }
    }

    fn random_thread(store: &Store, rng: &mut Rng) -> Option<ThreadId> {
        let mut stmt = store
            .conn
            .prepare("SELECT DISTINCT thread_id FROM messages ORDER BY thread_id")
            .unwrap();
        let threads: Vec<String> = stmt
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        (!threads.is_empty()).then(|| ThreadId(threads[rng.below(threads.len())].clone()))
    }

    fn assert_lists_match(store: &Store, context: &str) {
        let account = account().id;
        for folder in QUERIES {
            assert_eq!(
                store.folder_threads(&account, folder).unwrap(),
                store.folder_threads_scan(&account, folder).unwrap(),
                "{context}: folder {folder}"
            );
        }
        assert_eq!(
            store.folder_names(&account).unwrap(),
            store.folder_names_scan(&account).unwrap(),
            "{context}: folder names"
        );
    }

    #[test]
    fn folder_lists_match_a_scan_of_every_message() {
        for seed in 1..=20u64 {
            let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
            let store = Store::open_in_memory().unwrap();
            store.upsert_account(&account()).unwrap();
            let account = account().id;
            let mut made = Vec::new();
            let mut copies: Vec<Message> = Vec::new();
            let mut index = 0;
            for step in 0..60 {
                let context = format!("seed {seed}, step {step}");
                match rng.below(12) {
                    0..=4 => {
                        let size = 1 + rng.below(12);
                        let mut batch = Vec::new();
                        for _ in 0..size {
                            let next = message(&mut rng, index, &made);
                            copies.push(next.clone());
                            batch.push(next);
                            made.push(format!("m{index}@x"));
                            index += 1;
                        }
                        store
                            .apply_delta(
                                &account,
                                &MailboxDelta {
                                    messages: batch,
                                    flags: Vec::new(),
                                    cursor: None,
                                    ..MailboxDelta::default()
                                },
                            )
                            .unwrap();
                    }
                    5 => {
                        if let Some(thread) = random_thread(&store, &mut rng) {
                            let from = QUERIES[rng.below(QUERIES.len())];
                            let _ = store.archive_thread(&account, &thread, from);
                        }
                    }
                    6 => {
                        if let Some(thread) = random_thread(&store, &mut rng) {
                            let from = QUERIES[rng.below(QUERIES.len())];
                            let to = QUERIES[rng.below(QUERIES.len())];
                            let _ = store.move_thread(&account, &thread, from, to);
                        }
                    }
                    7 => {
                        if let Some(thread) = random_thread(&store, &mut rng) {
                            let label = format!("tag{}", rng.below(3));
                            let change = if rng.chance(50) {
                                LabelChange::Add(label)
                            } else {
                                LabelChange::Remove(label)
                            };
                            let _ = store.label_thread(&account, &thread, change);
                        }
                    }
                    8 => {
                        if rng.chance(50) {
                            let _ = store.undo(&account);
                        } else if let Some(thread) = random_thread(&store, &mut rng) {
                            let from = QUERIES[rng.below(QUERIES.len())];
                            let _ = store.delete_thread(&account, &thread, from);
                        }
                    }
                    9 => {
                        // Expunged on the server: a UID range, or a listing
                        // that leaves some UIDs out.
                        let folder = FOLDERS[rng.below(FOLDERS.len() - 1)];
                        let kind = if rng.chance(50) {
                            let low = rng.below(index.max(1)) as u32;
                            RemovalKind::Vanished {
                                uidvalidity: 1,
                                ranges: vec![(low, low + rng.below(4) as u32)],
                            }
                        } else {
                            RemovalKind::Listing {
                                uidvalidity: 1,
                                present: (0..index as u32).filter(|_| rng.chance(80)).collect(),
                                through: index as u32,
                                found: Vec::new(),
                                sweep: false,
                            }
                        };
                        let settle = match rng.below(3) {
                            0 => Settle::Later,
                            1 => Settle::KeepRows,
                            _ => Settle::All,
                        };
                        store
                            .apply_delta(
                                &account,
                                &MailboxDelta {
                                    removals: vec![FolderRemoval {
                                        mailbox: folder.into(),
                                        folder: folder.into(),
                                        kind,
                                    }],
                                    settle,
                                    ..MailboxDelta::default()
                                },
                            )
                            .unwrap();
                    }
                    10 => {
                        // Moved on the server: a new copy elsewhere, and the
                        // old UID gone, settled at the end of the pass.
                        if !copies.is_empty() {
                            let mut copy = copies[rng.below(copies.len())].clone();
                            let old = crate::imap::parse_remote_id(&copy.remote_id.0);
                            let folder = FOLDERS[rng.below(FOLDERS.len() - 1)];
                            copy.folders = vec![folder.to_string()];
                            copy.remote_id = RemoteId(format!("imap:1:{index}:{folder}"));
                            index += 1;
                            copies.push(copy.clone());
                            let removals = old
                                .map(|old| FolderRemoval {
                                    mailbox: old.mailbox,
                                    folder: old.folder,
                                    kind: RemovalKind::Vanished {
                                        uidvalidity: old.uidvalidity,
                                        ranges: vec![(old.uid, old.uid)],
                                    },
                                })
                                .into_iter()
                                .collect();
                            store
                                .apply_delta(
                                    &account,
                                    &MailboxDelta {
                                        messages: vec![copy],
                                        removals,
                                        settle: Settle::All,
                                        ..MailboxDelta::default()
                                    },
                                )
                                .unwrap();
                        }
                    }
                    _ => {
                        if !copies.is_empty() {
                            let flags = (0..3)
                                .map(|_| FlagUpdate {
                                    remote_id: copies[rng.below(copies.len())].remote_id.clone(),
                                    unread: rng.chance(50),
                                })
                                .collect();
                            store
                                .apply_delta(
                                    &account,
                                    &MailboxDelta {
                                        messages: Vec::new(),
                                        flags,
                                        cursor: None,
                                        ..MailboxDelta::default()
                                    },
                                )
                                .unwrap();
                        }
                    }
                }
                assert_lists_match(&store, &context);
            }
        }
    }

    #[test]
    fn a_write_that_skips_the_refresh_still_reads_right() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut rng = Rng(7);
        let batch: Vec<Message> = (0..20).map(|index| message(&mut rng, index, &[])).collect();
        store
            .apply_delta(
                &account().id,
                &MailboxDelta {
                    messages: batch,
                    flags: Vec::new(),
                    cursor: None,
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        // A raw statement outside `WriteTx`: the triggers queue the thread,
        // and nothing recomputes it.
        store
            .conn
            .execute_batch(
                "UPDATE messages SET folders = 'Work', unread = 1 WHERE rowid % 3 = 0;
                 DELETE FROM messages WHERE rowid % 7 = 0;",
            )
            .unwrap();
        let queued: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM thread_summary_dirty", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(queued > 0);
        assert_lists_match(&store, "after a raw write");
    }

    #[test]
    fn an_existing_mailbox_is_summarized_once_on_open() {
        let path = std::env::temp_dir().join(format!(
            "fell-thread-summary-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let store = Store::open(&path).unwrap();
            store.upsert_account(&account()).unwrap();
            let mut rng = Rng(11);
            let batch: Vec<Message> = (0..30).map(|index| message(&mut rng, index, &[])).collect();
            store
                .apply_delta(
                    &account().id,
                    &MailboxDelta {
                        messages: batch,
                        flags: Vec::new(),
                        cursor: None,
                        ..MailboxDelta::default()
                    },
                )
                .unwrap();
            // A mailbox from before the summaries.
            store
                .conn
                .execute_batch(
                    "DROP TABLE thread_summary; DROP TABLE thread_folders;
                     DROP TABLE thread_summary_dirty;
                     DELETE FROM meta WHERE key = 'thread_summary';",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let queued: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM thread_summary_dirty", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(queued, 0, "the open filled every summary");
        assert_lists_match(&store, "after the upgrade");
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn a_folder_read_does_not_scan_messages() {
        let store = Store::open_in_memory().unwrap();
        let plan: Vec<String> = {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {}", folder_list_sql(2)))
                .unwrap();
            stmt.query_map(params!["acct", "archive", "archived"], |row| {
                row.get::<_, String>(3)
            })
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
        };
        assert!(
            plan.iter().all(|step| !step.contains("messages")),
            "{plan:?}"
        );
        assert!(
            plan.iter().all(|step| !step.starts_with("SCAN tf")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .any(|step| step.contains("idx_thread_folders_folder")),
            "{plan:?}"
        );
    }

    #[test]
    fn a_refresh_touches_only_its_threads_rows() {
        let store = Store::open_in_memory().unwrap();
        for sql in [
            "DELETE FROM thread_folders WHERE account_id = ?1 AND thread_id = ?2",
            "DELETE FROM thread_summary WHERE account_id = ?1 AND thread_id = ?2",
            "SELECT rowid FROM messages WHERE account_id = ?1 AND thread_id = ?2
             ORDER BY date_unix ASC, rowid ASC",
        ] {
            let mut stmt = store
                .conn
                .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap();
            let plan: Vec<String> = stmt
                .query_map(params!["acct", "thread"], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            assert!(
                plan.iter().all(|step| step.contains("thread_id=?")),
                "{sql}: {plan:?}"
            );
        }
    }

    #[test]
    fn a_folder_read_works_inside_and_outside_a_write() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let mut rng = Rng(5);
        let batch: Vec<Message> = (0..10).map(|index| message(&mut rng, index, &[])).collect();
        store
            .apply_delta(
                &account().id,
                &MailboxDelta {
                    messages: batch,
                    flags: Vec::new(),
                    cursor: None,
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let outside = store.folder_threads(&account().id, "INBOX").unwrap();
        let tx = crate::store::immediate_transaction(&store.conn).unwrap();
        let inside = folder_threads(&tx, &account().id, "INBOX").unwrap();
        let names = folder_names(&tx, &account().id).unwrap();
        tx.commit().unwrap();
        assert_eq!(inside, outside);
        assert_eq!(names, store.folder_names(&account().id).unwrap());
    }

    #[test]
    fn a_current_mailbox_keeps_its_summary_triggers() {
        let path = std::env::temp_dir().join(format!(
            "fell-summary-triggers-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let first = Store::open(&path).unwrap();
        first
            .conn
            .execute_batch(
                "DROP TRIGGER thread_summary_delete;
                 CREATE TRIGGER thread_summary_delete AFTER DELETE ON messages BEGIN
                     -- marker
                     INSERT INTO thread_summary_dirty (account_id, thread_id)
                     SELECT old.account_id, old.thread_id
                     WHERE NOT EXISTS (
                         SELECT 1 FROM thread_summary_dirty
                         WHERE account_id = old.account_id AND thread_id = old.thread_id
                     );
                 END;",
            )
            .unwrap();
        let second = Store::open(&path).unwrap();
        let sql: String = second
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'thread_summary_delete'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            sql.contains("marker"),
            "no trigger rebuild on a current mailbox"
        );
        drop(second);
        drop(first);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
