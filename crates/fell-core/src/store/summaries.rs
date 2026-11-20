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

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::ops::Bound::{self, Excluded, Included, Unbounded};
use std::ops::RangeBounds;

use rusqlite::types::Value;
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

/// Created apart from [`SUMMARY_TABLES`]: a mailbox summarized by version 1
/// has this table without `last_date`, and its indexes cannot be built until
/// [`ensure_thread_summaries`] has added the column.
const FOLDER_TABLE: &str = r#"
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
    -- The thread's latest message date, copied from its summary, so a folder
    -- list can be read in order a window at a time. The default lets a
    -- version 1 binary, which does not know the column, still write rows.
    -- The next open of this version copies the dates back in.
    last_date INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, thread_id, folder_key)
) WITHOUT ROWID;
-- The earliest copy of a folder name is the first entry here. A refresh
-- deletes through the primary key.
CREATE INDEX IF NOT EXISTS idx_thread_folders_folder
    ON thread_folders(account_id, folder_key, first_rowid, first_pos);
-- A folder list in list order: newest activity first, then thread id.
CREATE INDEX IF NOT EXISTS idx_thread_folders_list
    ON thread_folders(account_id, folder_key, last_date DESC, thread_id);
-- Only the unread rows, so the header's count reads nothing else.
CREATE INDEX IF NOT EXISTS idx_thread_folders_unread
    ON thread_folders(account_id, folder_key, thread_id) WHERE unread = 1;
"#;

/// Value of `meta.thread_summary` once the tables match this code. Version 2
/// added `thread_folders.last_date`.
const SUMMARY_VERSION: &str = "2";

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
        conn.execute_batch(FOLDER_TABLE)?;
        return Ok(());
    }
    let tx = super::immediate_transaction(conn)?;
    let version = summary_version(&tx)?;
    if version.as_deref() == Some(SUMMARY_VERSION) {
        tx.execute_batch(FOLDER_TABLE)?;
        return tx.commit();
    }
    if version.as_deref() == Some("1") && table_exists(&tx, "thread_folders")? {
        // Version 1 rows are right apart from the date. Add it from the
        // summaries instead of recomputing every thread under the write
        // lock: fell and fell-mcp share the file, and a recompute of a large
        // mailbox would outlast the other opener's busy timeout. Threads
        // still queued are recomputed at commit, as with any write.
        if !has_column(&tx, "thread_folders", "last_date")? {
            tx.execute_batch(
                "ALTER TABLE thread_folders ADD COLUMN last_date INTEGER NOT NULL DEFAULT 0;",
            )?;
        }
        // Also repairs rows a version 1 binary wrote after an upgrade,
        // which took the default.
        tx.execute_batch(
            "UPDATE thread_folders SET last_date = COALESCE((
                 SELECT s.last_date FROM thread_summary s
                 WHERE s.account_id = thread_folders.account_id
                   AND s.thread_id = thread_folders.thread_id
             ), 0);",
        )?;
        // The indexes go in after the dates, so the update does not keep
        // them current row by row.
        tx.execute_batch(FOLDER_TABLE)?;
        stamp_version(&tx)?;
        return tx.commit();
    }
    // No summaries yet, or a version this code does not know: build them
    // from `messages`. An unknown `thread_folders` may lack columns, and
    // every row is rebuilt below, so replace the table.
    tx.execute_batch("DROP TABLE IF EXISTS thread_folders;")?;
    tx.execute_batch(FOLDER_TABLE)?;
    tx.execute_batch(
        "DELETE FROM thread_summary;
         DELETE FROM account_folders;
         INSERT OR IGNORE INTO thread_summary_dirty (account_id, thread_id)
             SELECT DISTINCT account_id, thread_id FROM messages;",
    )?;
    stamp_version(&tx)?;
    // Commit recomputes every queued thread.
    tx.commit()
}

fn stamp_version(conn: &Connection) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES ('thread_summary', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SUMMARY_VERSION],
    )?;
    Ok(())
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2",
            params![table, column],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn summary_version(conn: &Connection) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT value FROM meta WHERE key = 'thread_summary'",
            [],
            |row| row.get(0),
        )
        .optional()?)
}

fn summaries_are_current(conn: &Connection) -> Result<bool> {
    Ok(summary_version(conn)?.as_deref() == Some(SUMMARY_VERSION))
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
            account_id, folder_key, thread_id, display, first_rowid, first_pos, unread,
            last_date
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
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
                    summary.last_date,
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
    /// Any message in the thread is unread, whatever its folder.
    unread: bool,
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
                    unread,
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
                summary.unread |= unread;
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

/// Where a thread sits in a folder list. Lists are ordered newest activity
/// first, then by thread id, and `Ord` follows that order: a key that sorts
/// lower is higher on screen.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ListKey {
    pub date_unix: i64,
    pub thread_id: ThreadId,
}

impl ListKey {
    pub fn of(thread: &FolderThread) -> Self {
        Self {
            date_unix: thread.last_date_unix,
            thread_id: thread.thread_id.clone(),
        }
    }
}

impl Ord for ListKey {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .date_unix
            .cmp(&self.date_unix)
            .then_with(|| self.thread_id.0.cmp(&other.thread_id.0))
    }
}

impl PartialOrd for ListKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Which part of a folder list to read. Positions are keys, not offsets, so
/// rows that sync adds elsewhere in the folder never shift a window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FolderSpan {
    /// Up to `rows` rows from the top of the list.
    Top { rows: usize },
    /// The last `rows` rows of the list.
    Bottom { rows: usize },
    /// Every row from `first` through `last`, both included (`first: None`
    /// starts at the top), plus up to `before` rows above `first` and up to
    /// `after` rows below `last`.
    Window {
        first: Option<ListKey>,
        last: ListKey,
        before: usize,
        after: usize,
    },
}

/// One window of a folder list, read in one snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderPage {
    pub threads: Vec<FolderThread>,
    /// The folder has rows above the first one returned.
    pub more_before: bool,
    /// The folder has rows below the last one returned.
    pub more_after: bool,
    /// Unread threads in the whole folder, not only this window.
    pub unread: usize,
}

/// Narrows a list read. The default keeps every row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ListFilter {
    /// Only threads unread in the list: in a folder, unread in that folder;
    /// under a label, any message unread.
    pub unread_only: bool,
    /// Only threads whose latest message is at or after this unix second.
    pub since: Option<i64>,
}

impl ListFilter {
    fn keeps(&self, thread: &FolderThread) -> bool {
        (!self.unread_only || thread.unread)
            && self
                .since
                .is_none_or(|since| thread.last_date_unix >= since)
    }
}

/// What a [`ThreadList`] lists.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListSource<'a> {
    /// A folder, matched as [`crate::Store::folder_page`] matches it.
    Folder(&'a str),
    /// Threads with a message carrying this exact label.
    Label(&'a str),
}

/// Rows after a list position, newest activity first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadList {
    pub threads: Vec<FolderThread>,
    /// More rows pass the filter below the last one returned.
    pub more: bool,
    /// Unread threads in the whole folder, ignoring the filter. `None` for a
    /// label, which has no unread index.
    pub unread: Option<usize>,
}

/// Reads one folder's list. Built inside a snapshot, so every read through
/// it sees the same moment.
struct ListReader<'a> {
    conn: &'a Connection,
    account: &'a str,
    /// Folder keys the name matches: one, or a few aliases.
    keys: Vec<String>,
    /// Threads still in the refresh queue that are in this folder, computed
    /// from `messages`, in list order. Usually empty.
    queued: Vec<FolderThread>,
    /// The account has threads in the refresh queue, in any folder.
    any_queued: bool,
    /// Applies to rows, not to the unread count.
    filter: ListFilter,
}

impl<'a> ListReader<'a> {
    fn new(conn: &'a Connection, account_id: &'a AccountId, folder: &str) -> Result<Self> {
        Self::filtered(conn, account_id, folder, ListFilter::default())
    }

    fn filtered(
        conn: &'a Connection,
        account_id: &'a AccountId,
        folder: &str,
        filter: ListFilter,
    ) -> Result<Self> {
        let account = account_id.as_str();
        let keys: Vec<String> = folder_query_names(folder)
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let mut queued = Vec::new();
        let waiting = queued_threads(conn, account)?;
        let any_queued = !waiting.is_empty();
        for thread in waiting {
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
                queued.push(folder_thread(&thread, &summary, unread));
            }
        }
        queued.sort_by_key(ListKey::of);
        Ok(Self {
            conn,
            account,
            keys,
            queued,
            any_queued,
            filter,
        })
    }

    /// Rows between `lower` and `upper` in list order, at most `limit` of
    /// them: the first ones, or the last ones when `from_end` is set.
    /// Returned in list order either way.
    fn rows(
        &self,
        lower: Bound<&ListKey>,
        upper: Bound<&ListKey>,
        from_end: bool,
        limit: Option<usize>,
    ) -> Result<Vec<FolderThread>> {
        if limit == Some(0) {
            return Ok(Vec::new());
        }
        let mut found: Vec<FolderThread> = Vec::new();
        for key in &self.keys {
            let (sql, bind) = window_sql(
                self.account,
                key,
                lower,
                upper,
                from_end,
                limit,
                &self.filter,
            );
            let mut stmt = self.conn.prepare_cached(&sql)?;
            let rows = stmt.query_map(params_from_iter(bind.iter()), list_row)?;
            for row in rows {
                found.push(row?);
            }
        }
        found.extend(
            self.queued
                .iter()
                .filter(|thread| {
                    (lower, upper).contains(&ListKey::of(thread)) && self.filter.keeps(thread)
                })
                .cloned(),
        );
        found.sort_by_key(ListKey::of);
        // A thread in two alias folders comes back once per folder, with the
        // same key. Keep one row, unread if either copy is.
        let mut threads: Vec<FolderThread> = Vec::with_capacity(found.len());
        for thread in found {
            match threads.last_mut() {
                Some(last) if last.thread_id == thread.thread_id => last.unread |= thread.unread,
                _ => threads.push(thread),
            }
        }
        if let Some(limit) = limit
            && threads.len() > limit
        {
            if from_end {
                threads.drain(..threads.len() - limit);
            } else {
                threads.truncate(limit);
            }
        }
        Ok(threads)
    }

    fn any(&self, lower: Bound<&ListKey>, upper: Bound<&ListKey>) -> Result<bool> {
        Ok(!self.rows(lower, upper, false, Some(1))?.is_empty())
    }

    fn page(&self, span: &FolderSpan) -> Result<FolderPage> {
        let threads = match span {
            FolderSpan::Top { rows } => self.rows(Unbounded, Unbounded, false, Some(*rows))?,
            FolderSpan::Bottom { rows } => self.rows(Unbounded, Unbounded, true, Some(*rows))?,
            FolderSpan::Window {
                first,
                last,
                before,
                after,
            } => {
                // A start below the end would list the rows between twice.
                let first = first.as_ref().map(|first| first.min(last));
                let mut threads = match first {
                    Some(first) => self.rows(Unbounded, Excluded(first), true, Some(*before))?,
                    None => Vec::new(),
                };
                let lower = first.map_or(Unbounded, Included);
                threads.extend(self.rows(lower, Included(last), false, None)?);
                threads.extend(self.rows(Excluded(last), Unbounded, false, Some(*after))?);
                threads
            }
        };
        let (more_before, more_after) = match (threads.first(), threads.last()) {
            (Some(top), Some(bottom)) => (
                self.any(Unbounded, Excluded(&ListKey::of(top)))?,
                self.any(Excluded(&ListKey::of(bottom)), Unbounded)?,
            ),
            // Nothing left inside the window. Say which side still has rows.
            _ => match span {
                FolderSpan::Window { first, last, .. } => (
                    match first {
                        Some(first) => self.any(Unbounded, Excluded(first))?,
                        None => false,
                    },
                    self.any(Excluded(last), Unbounded)?,
                ),
                FolderSpan::Top { .. } => (false, self.any(Unbounded, Unbounded)?),
                FolderSpan::Bottom { .. } => (self.any(Unbounded, Unbounded)?, false),
            },
        };
        Ok(FolderPage {
            threads,
            more_before,
            more_after,
            unread: self.unread()?,
        })
    }

    /// Unread threads in the folder. Reads only the unread rows.
    fn unread(&self) -> Result<usize> {
        let sql = unread_sql(self.keys.len(), self.any_queued);
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut bind = Vec::with_capacity(1 + self.keys.len());
        bind.push(self.account.to_string());
        bind.extend(self.keys.iter().cloned());
        let listed: i64 = stmt.query_row(params_from_iter(bind.iter()), |row| row.get(0))?;
        let queued = self.queued.iter().filter(|thread| thread.unread).count();
        Ok(listed as usize + queued)
    }
}

fn list_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FolderThread> {
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
}

/// Rows of one folder key between two list positions. Without statistics
/// SQLite would walk every row in the account and probe each one, so the
/// plan is pinned: the list index first (`INDEXED BY`), in its own order so
/// `LIMIT` stops the walk, then each summary by key (`CROSS JOIN`).
///
/// A bound on `(date, thread)` is a range on the date column, which the
/// index seeks to, plus a filter that only drops rows tied on that date.
fn window_sql(
    account: &str,
    key: &str,
    lower: Bound<&ListKey>,
    upper: Bound<&ListKey>,
    from_end: bool,
    limit: Option<usize>,
    filter: &ListFilter,
) -> (String, Vec<Value>) {
    let mut bind: Vec<Value> = vec![account.to_string().into(), key.to_string().into()];
    let mut sql = String::from(
        "SELECT ts.thread_id, ts.subject, ts.snippet, tf.last_date, ts.message_count,
                tf.unread, ts.starred, ts.has_attachment, ts.latest_email,
                ts.latest_name, ts.participants, ts.labels
         FROM thread_folders tf INDEXED BY idx_thread_folders_list
         CROSS JOIN thread_summary ts
           ON ts.account_id = tf.account_id AND ts.thread_id = tf.thread_id
         WHERE tf.account_id = ?1 AND tf.folder_key = ?2",
    );
    if filter.unread_only {
        sql.push_str("\n           AND tf.unread = 1");
    }
    // A range on the index's date column, so the walk stops at `since`.
    if let Some(since) = filter.since {
        bind.push(since.into());
        sql.push_str(&format!("\n           AND tf.last_date >= ?{}", bind.len()));
    }
    // Below `lower` in list order: older, or the same date and a later id.
    if let Included(at) | Excluded(at) = lower {
        let id = if matches!(lower, Included(_)) {
            ">="
        } else {
            ">"
        };
        bind.push(at.date_unix.into());
        bind.push(at.thread_id.0.clone().into());
        let (date, thread) = (bind.len() - 1, bind.len());
        sql.push_str(&format!(
            "\n           AND tf.last_date <= ?{date}
           AND (tf.last_date < ?{date} OR tf.thread_id {id} ?{thread})"
        ));
    }
    // Above `upper` in list order: newer, or the same date and an earlier id.
    if let Included(at) | Excluded(at) = upper {
        let id = if matches!(upper, Included(_)) {
            "<="
        } else {
            "<"
        };
        bind.push(at.date_unix.into());
        bind.push(at.thread_id.0.clone().into());
        let (date, thread) = (bind.len() - 1, bind.len());
        sql.push_str(&format!(
            "\n           AND tf.last_date >= ?{date}
           AND (tf.last_date > ?{date} OR tf.thread_id {id} ?{thread})"
        ));
    }
    let order = if from_end {
        "tf.last_date ASC, tf.thread_id DESC"
    } else {
        "tf.last_date DESC, tf.thread_id ASC"
    };
    // SQLite reads a negative limit as no limit.
    bind.push(
        limit
            .map_or(-1, |rows| rows.min(i64::MAX as usize) as i64)
            .into(),
    );
    sql.push_str(&format!(
        "\n           AND NOT EXISTS (
               SELECT 1 FROM thread_summary_dirty d
               WHERE d.account_id = tf.account_id AND d.thread_id = tf.thread_id
           )
         ORDER BY {order}
         LIMIT ?{}",
        bind.len()
    ));
    (sql, bind)
}

/// `skip_queued` leaves out threads in the refresh queue, whose rows may be
/// stale. With the queue empty, the per-row check is skipped.
fn unread_sql(keys: usize, skip_queued: bool) -> String {
    let placeholders = (0..keys)
        .map(|index| format!("?{}", index + 2))
        .collect::<Vec<_>>()
        .join(", ");
    // One key has one row per thread. Aliases can share a thread, and
    // DISTINCT costs a sort, so only they pay for it.
    let count = if keys == 1 {
        "COUNT(*)"
    } else {
        "COUNT(DISTINCT tf.thread_id)"
    };
    let queued = if skip_queued {
        "AND NOT EXISTS (
               SELECT 1 FROM thread_summary_dirty d
               WHERE d.account_id = tf.account_id AND d.thread_id = tf.thread_id
           )"
    } else {
        ""
    };
    format!(
        "SELECT {count}
         FROM thread_folders tf INDEXED BY idx_thread_folders_unread
         WHERE tf.account_id = ?1 AND tf.folder_key IN ({placeholders}) AND tf.unread = 1
           {queued}"
    )
}

/// Every thread with at least one message in `folder`, in list order.
pub(super) fn folder_threads(
    conn: &Connection,
    account_id: &AccountId,
    folder: &str,
) -> Result<Vec<FolderThread>> {
    in_one_snapshot(conn, || {
        ListReader::new(conn, account_id, folder)?.rows(Unbounded, Unbounded, false, None)
    })
}

/// One window of `folder`'s list, its edges, and the folder's unread count.
pub(super) fn folder_page(
    conn: &Connection,
    account_id: &AccountId,
    folder: &str,
    span: &FolderSpan,
) -> Result<FolderPage> {
    in_one_snapshot(conn, || {
        ListReader::new(conn, account_id, folder)?.page(span)
    })
}

/// Up to `limit` rows of `source` below `after` (from the top when `None`)
/// that pass `filter`. A folder reads its list index, so the cost follows the
/// rows read; a label reads every thread summary in the account.
pub(super) fn thread_list(
    conn: &Connection,
    account_id: &AccountId,
    source: ListSource<'_>,
    filter: &ListFilter,
    after: Option<&ListKey>,
    limit: usize,
) -> Result<ThreadList> {
    in_one_snapshot(conn, || {
        let lower = after.map_or(Unbounded, Excluded);
        let (mut threads, unread) = match source {
            ListSource::Folder(folder) => {
                let reader = ListReader::filtered(conn, account_id, folder, filter.clone())?;
                let rows = reader.rows(lower, Unbounded, false, Some(limit.saturating_add(1)))?;
                (rows, Some(reader.unread()?))
            }
            ListSource::Label(label) => (
                label_rows(
                    conn,
                    account_id,
                    label,
                    filter,
                    after,
                    limit.saturating_add(1),
                )?,
                None,
            ),
        };
        let more = threads.len() > limit;
        threads.truncate(limit);
        Ok(ThreadList {
            threads,
            more,
            unread,
        })
    })
}

/// Threads carrying `label`, in list order. Label tokens sit in
/// `thread_summary.labels`, which has no index, so this reads every summary
/// in the account and sorts the matches.
fn label_rows(
    conn: &Connection,
    account_id: &AccountId,
    label: &str,
    filter: &ListFilter,
    after: Option<&ListKey>,
    limit: usize,
) -> Result<Vec<FolderThread>> {
    let account = account_id.as_str();
    let any_unread = "EXISTS (
               SELECT 1 FROM messages m
               WHERE m.account_id = ts.account_id AND m.thread_id = ts.thread_id
                 AND m.unread = 1
           )";
    let mut bind: Vec<Value> = vec![account.to_string().into(), label.to_string().into()];
    let mut sql = format!(
        "SELECT ts.thread_id, ts.subject, ts.snippet, ts.last_date, ts.message_count,
                {any_unread}, ts.starred, ts.has_attachment, ts.latest_email,
                ts.latest_name, ts.participants, ts.labels
         FROM thread_summary ts
         WHERE ts.account_id = ?1
           AND instr(char(30) || ts.labels || char(30), char(30) || ?2 || char(30)) > 0
           AND NOT EXISTS (
               SELECT 1 FROM thread_summary_dirty d
               WHERE d.account_id = ts.account_id AND d.thread_id = ts.thread_id
           )"
    );
    if let Some(since) = filter.since {
        bind.push(since.into());
        sql.push_str(&format!("\n           AND ts.last_date >= ?{}", bind.len()));
    }
    if let Some(at) = after {
        bind.push(at.date_unix.into());
        bind.push(at.thread_id.0.clone().into());
        let (date, thread) = (bind.len() - 1, bind.len());
        sql.push_str(&format!(
            "\n           AND (ts.last_date < ?{date} OR (ts.last_date = ?{date} AND ts.thread_id > ?{thread}))"
        ));
    }
    if filter.unread_only {
        sql.push_str(&format!("\n           AND {any_unread}"));
    }
    bind.push((limit.min(i64::MAX as usize) as i64).into());
    sql.push_str(&format!(
        "\n         ORDER BY ts.last_date DESC, ts.thread_id ASC LIMIT ?{}",
        bind.len()
    ));
    let mut found: Vec<FolderThread> = {
        let mut stmt = conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params_from_iter(bind.iter()), list_row)?;
        rows.collect::<std::result::Result<_, _>>()?
    };
    for thread in queued_threads(conn, account)? {
        let Some(summary) = compute(conn, account, &thread)? else {
            continue;
        };
        if !summary.labels.split('\u{1e}').any(|token| token == label) {
            continue;
        }
        let row = folder_thread(&thread, &summary, summary.unread);
        let below = after.is_none_or(|at| ListKey::of(&row) > *at);
        if below && filter.keeps(&row) {
            found.push(row);
        }
    }
    found.sort_by_key(ListKey::of);
    found.truncate(limit);
    Ok(found)
}

/// Where `thread` sits in `folder`'s list now, or `None` when it is not in
/// the folder. Reads the thread's own rows, not the list.
pub(super) fn folder_key_of(
    conn: &Connection,
    account_id: &AccountId,
    folder: &str,
    thread: &ThreadId,
) -> Result<Option<ListKey>> {
    in_one_snapshot(conn, || {
        let account = account_id.as_str();
        let keys: Vec<String> = folder_query_names(folder)
            .iter()
            .map(|name| name.to_ascii_lowercase())
            .collect();
        let queued = conn
            .prepare_cached(
                "SELECT 1 FROM thread_summary_dirty WHERE account_id = ?1 AND thread_id = ?2",
            )?
            .query_row(params![account, thread.0], |_| Ok(()))
            .optional()?
            .is_some();
        if queued {
            // Its stored rows are out of date, as in [`ListReader::new`].
            let Some(summary) = compute(conn, account, &thread.0)? else {
                return Ok(None);
            };
            let listed = summary
                .folders
                .iter()
                .any(|membership| keys.contains(&membership.key));
            return Ok(listed.then(|| ListKey {
                date_unix: summary.last_date,
                thread_id: thread.clone(),
            }));
        }
        let mut stmt = conn.prepare_cached(
            "SELECT last_date FROM thread_folders
             WHERE account_id = ?1 AND thread_id = ?2 AND folder_key = ?3",
        )?;
        for key in &keys {
            let date: Option<i64> = stmt
                .query_row(params![account, thread.0, key], |row| row.get(0))
                .optional()?;
            if let Some(date_unix) = date {
                return Ok(Some(ListKey {
                    date_unix,
                    thread_id: thread.clone(),
                }));
            }
        }
        Ok(None)
    })
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
    // clean memberships and only the folder fields of queued messages.
    let mut earliest = HashMap::new();
    let mut add_name = |key: String, rowid: i64, pos: i64, name: String| {
        let first = earliest
            .entry(key)
            .or_insert_with(|| (rowid, pos, name.clone()));
        if (rowid, pos) < (first.0, first.1) {
            *first = (rowid, pos, name);
        }
    };
    {
        let mut stmt = conn.prepare_cached(
            "SELECT tf.first_rowid, tf.first_pos, tf.folder_key, tf.display
             FROM thread_folders tf
             WHERE tf.account_id = ?1 AND tf.display IS NOT NULL
               AND NOT EXISTS (
                   SELECT 1 FROM thread_summary_dirty d
                   WHERE d.account_id = tf.account_id AND d.thread_id = tf.thread_id
               )",
        )?;
        let mut rows = stmt.query(params![account])?;
        while let Some(row) = rows.next()? {
            add_name(row.get(2)?, row.get(0)?, row.get(1)?, row.get(3)?);
        }
    }
    let mut messages = conn.prepare_cached(
        "SELECT rowid, folders FROM messages WHERE account_id = ?1 AND thread_id = ?2",
    )?;
    for thread in queued {
        let mut rows = messages.query(params![account, thread])?;
        while let Some(row) = rows.next()? {
            let rowid = row.get::<_, i64>(0)?;
            let folders = row.get::<_, String>(1)?;
            for (pos, name) in split_field(&folders).into_iter().enumerate() {
                add_name(name.to_ascii_lowercase(), rowid, pos as i64, name);
            }
        }
    }
    let mut names: Vec<_> = earliest.into_values().collect();
    names.sort_unstable_by_key(|(rowid, pos, _)| (*rowid, *pos));
    Ok(names.into_iter().map(|(_, _, name)| name).collect())
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
            reply_to: Some(Vec::new()),
            attachment_names: Vec::new(),
            parts: None,
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

    /// The page `span` should return, cut from the whole list by key.
    fn expected_page(full: &[FolderThread], span: &FolderSpan) -> (Vec<FolderThread>, bool, bool) {
        let keys: Vec<ListKey> = full.iter().map(ListKey::of).collect();
        let (start, end) = match span {
            FolderSpan::Top { rows } => (0, (*rows).min(full.len())),
            FolderSpan::Bottom { rows } => (full.len().saturating_sub(*rows), full.len()),
            FolderSpan::Window {
                first,
                last,
                before,
                after,
            } => {
                let low = first
                    .as_ref()
                    .map_or(0, |first| keys.iter().filter(|key| *key < first).count());
                let high = keys.iter().filter(|key| *key <= last).count();
                (
                    low.saturating_sub(*before),
                    (high + after).min(full.len()).max(low),
                )
            }
        };
        let page = full[start..end].to_vec();
        if page.is_empty()
            && let FolderSpan::Window { first, last, .. } = span
        {
            let before = first
                .as_ref()
                .is_some_and(|first| keys.iter().any(|key| key < first));
            return (page, before, keys.iter().any(|key| key > last));
        }
        (page, start > 0, end < full.len())
    }

    /// A key that sorts just after `key`: same date, and no stored id lies
    /// between the two.
    fn just_after(key: &ListKey) -> ListKey {
        ListKey {
            date_unix: key.date_unix,
            thread_id: ThreadId(format!("{}\0", key.thread_id.0)),
        }
    }

    /// Windows of every size and position, with bounds on stored rows and
    /// between them, must be the matching cut of the whole list.
    fn assert_pages_match(store: &Store, folder: &str, full: &[FolderThread], context: &str) {
        let account = account().id;
        let unread = full.iter().filter(|thread| thread.unread).count();
        let keys: Vec<ListKey> = full.iter().map(ListKey::of).collect();
        let mut spans = Vec::new();
        for rows in [0, 1, 3, full.len() + 2] {
            spans.push(FolderSpan::Top { rows });
            spans.push(FolderSpan::Bottom { rows });
        }
        // The ends, the rows next to them, and the middle.
        let len = keys.len();
        let mut picked: Vec<usize> = [0, 1, len / 2, len.saturating_sub(2), len.saturating_sub(1)]
            .into_iter()
            .filter(|index| *index < len)
            .collect();
        picked.dedup();
        for index in picked {
            let key = &keys[index];
            let end = &keys[(index + 2).min(len - 1)];
            for (before, after) in [(0, 0), (2, 1)] {
                spans.push(FolderSpan::Window {
                    first: Some(key.clone()),
                    last: end.clone(),
                    before,
                    after,
                });
                spans.push(FolderSpan::Window {
                    first: None,
                    last: key.clone(),
                    before,
                    after,
                });
                spans.push(FolderSpan::Window {
                    first: Some(just_after(key)),
                    last: just_after(key),
                    before,
                    after,
                });
            }
        }
        for span in spans {
            let page = store.folder_page(&account, folder, &span).unwrap();
            let (threads, more_before, more_after) = expected_page(full, &span);
            assert_eq!(
                (page.threads, page.more_before, page.more_after, page.unread),
                (threads, more_before, more_after, unread),
                "{context}: folder {folder}, {span:?}"
            );
        }
    }

    /// `pages` also checks windows of each list, which is much slower.
    fn assert_lists_match(store: &Store, context: &str, pages: bool) {
        let account = account().id;
        for folder in QUERIES {
            let full = store.folder_threads_scan(&account, folder).unwrap();
            assert_eq!(
                store.folder_threads(&account, folder).unwrap(),
                full,
                "{context}: folder {folder}"
            );
            if pages {
                assert_pages_match(store, folder, &full, context);
                let unread = full.iter().filter(|thread| thread.unread).count();
                for filter in filters(&full) {
                    let expected: Vec<FolderThread> = full
                        .iter()
                        .filter(|thread| filter.keeps(thread))
                        .cloned()
                        .collect();
                    let (listed, unreads) = read_all(store, ListSource::Folder(folder), &filter);
                    assert_eq!(listed, expected, "{context}: {folder} {filter:?}");
                    assert!(unreads.iter().all(|count| *count == Some(unread)));
                }
            }
        }
        if pages {
            for label in ["tag0", "tag1", "tag2", "tag", "missing"] {
                let full = label_scan(store, label);
                for filter in filters(&full) {
                    let expected: Vec<FolderThread> = full
                        .iter()
                        .filter(|thread| filter.keeps(thread))
                        .cloned()
                        .collect();
                    let (listed, unreads) = read_all(store, ListSource::Label(label), &filter);
                    assert_eq!(listed, expected, "{context}: label {label} {filter:?}");
                    assert!(unreads.iter().all(Option::is_none));
                }
            }
        }
        assert_eq!(
            store.folder_names(&account).unwrap(),
            store.folder_names_scan(&account).unwrap(),
            "{context}: folder names"
        );
    }

    /// Every filter shape, with `since` in the middle of `full`.
    fn filters(full: &[FolderThread]) -> Vec<ListFilter> {
        let since = full.get(full.len() / 2).map(|thread| thread.last_date_unix);
        vec![
            ListFilter::default(),
            ListFilter {
                unread_only: true,
                since: None,
            },
            ListFilter {
                unread_only: false,
                since,
            },
            ListFilter {
                unread_only: true,
                since,
            },
        ]
    }

    /// Page through `source` three rows at a time, as an agent would.
    fn read_all(
        store: &Store,
        source: ListSource<'_>,
        filter: &ListFilter,
    ) -> (Vec<FolderThread>, Vec<Option<usize>>) {
        let account = account().id;
        let mut out: Vec<FolderThread> = Vec::new();
        let mut unreads = Vec::new();
        loop {
            let after = out.last().map(ListKey::of);
            let page = store
                .thread_list(&account, source, filter, after.as_ref(), 3)
                .unwrap();
            assert!(page.threads.len() <= 3);
            assert!(
                !page.more || page.threads.len() == 3,
                "more needs a full page"
            );
            unreads.push(page.unread);
            out.extend(page.threads);
            if !page.more {
                return (out, unreads);
            }
        }
    }

    /// Threads carrying `label`, from each thread's messages.
    fn label_scan(store: &Store, label: &str) -> Vec<FolderThread> {
        let account = account().id;
        let mut stmt = store
            .conn
            .prepare("SELECT DISTINCT thread_id FROM messages WHERE account_id = ?1")
            .unwrap();
        let threads: Vec<String> = stmt
            .query_map(params![account.as_str()], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        let mut out: Vec<FolderThread> = threads
            .iter()
            .filter_map(|thread| {
                let summary = compute(&store.conn, account.as_str(), thread).unwrap()?;
                summary
                    .labels
                    .split('\u{1e}')
                    .any(|token| token == label)
                    .then(|| folder_thread(thread, &summary, summary.unread))
            })
            .collect();
        out.sort_by_key(ListKey::of);
        out
    }

    #[test]
    fn a_filtered_folder_read_still_walks_the_list_index() {
        let store = Store::open_in_memory().unwrap();
        let key = ListKey {
            date_unix: 1_700_000_000,
            thread_id: ThreadId("t".into()),
        };
        let filter = ListFilter {
            unread_only: true,
            since: Some(1_600_000_000),
        };
        let (sql, bind) = window_sql(
            "acct",
            "inbox",
            Excluded(&key),
            Unbounded,
            false,
            Some(21),
            &filter,
        );
        let plan = plan(&store, &sql, &bind);
        assert!(
            plan.iter().all(|step| !step.contains("messages")),
            "{plan:?}"
        );
        assert!(
            plan.iter()
                .any(|step| step.contains("idx_thread_folders_list")),
            "{plan:?}"
        );
        assert!(
            plan.iter().all(|step| !step.contains("TEMP B-TREE")),
            "{plan:?}"
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
                                    unread: Some(rng.chance(50)),
                                    flagged: None,
                                    keywords: None,
                                    modseq: None,
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
                assert_lists_match(&store, &context, step % 10 == 9);
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
        assert_lists_match(&store, "after a raw write", true);
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
        assert_lists_match(&store, "after the upgrade", true);
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn plan(store: &Store, sql: &str, bind: &[Value]) -> Vec<String> {
        let mut stmt = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap();
        stmt.query_map(params_from_iter(bind.iter()), |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn a_folder_read_does_not_scan_messages() {
        let store = Store::open_in_memory().unwrap();
        let key = ListKey {
            date_unix: 1_700_000_000,
            thread_id: ThreadId("t".into()),
        };
        for (lower, upper, from_end) in [
            (Unbounded, Unbounded, false),
            (Excluded(&key), Unbounded, false),
            (Included(&key), Included(&key), false),
            (Unbounded, Excluded(&key), true),
        ] {
            let (sql, bind) = window_sql(
                "acct",
                "inbox",
                lower,
                upper,
                from_end,
                Some(10),
                &ListFilter::default(),
            );
            let plan = plan(&store, &sql, &bind);
            assert!(
                plan.iter().all(|step| !step.contains("messages")),
                "{plan:?}"
            );
            assert!(
                plan.iter().all(|step| !step.starts_with("SCAN")),
                "{plan:?}"
            );
            assert!(
                plan.iter()
                    .any(|step| step.contains("idx_thread_folders_list")),
                "{plan:?}"
            );
            // Read in index order, so LIMIT ends the walk. A sort would
            // read the whole folder first.
            assert!(
                plan.iter().all(|step| !step.contains("TEMP B-TREE")),
                "{plan:?}"
            );
        }
        let bind: Vec<Value> = vec!["acct".to_string().into(), "inbox".to_string().into()];
        for skip_queued in [false, true] {
            let plan = plan(&store, &unread_sql(1, skip_queued), &bind);
            assert!(
                plan.iter()
                    .any(|step| step.contains("idx_thread_folders_unread")),
                "{plan:?}"
            );
            assert!(
                plan.iter().all(|step| !step.starts_with("SCAN")),
                "{plan:?}"
            );
        }
    }

    /// One thread per message, each in `folder`, a minute apart.
    fn fill_folder(store: &Store, folder: &str, threads: usize) {
        let mut rng = Rng(3);
        for start in (0..threads).step_by(500) {
            let batch: Vec<Message> = (start..(start + 500).min(threads))
                .map(|index| {
                    let mut message = message(&mut rng, index, &[]);
                    message.folders = vec![folder.to_string()];
                    message.archived = false;
                    message.headers.date_unix = 1_700_000_000 + index as i64 * 60;
                    message
                })
                .collect();
            store
                .apply_delta(
                    &account().id,
                    &MailboxDelta {
                        messages: batch,
                        ..MailboxDelta::default()
                    },
                )
                .unwrap();
        }
    }

    /// VM steps SQLite spends reading `limit` rows after the middle of a
    /// folder of `threads` threads.
    fn window_steps(threads: usize, limit: usize) -> i32 {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        fill_folder(&store, "INBOX", threads);
        let full = store.folder_threads(&account().id, "INBOX").unwrap();
        let middle = ListKey::of(&full[threads / 2]);
        let (sql, bind) = window_sql(
            "acct",
            "inbox",
            Excluded(&middle),
            Unbounded,
            false,
            Some(limit),
            &ListFilter::default(),
        );
        let mut stmt = store.conn.prepare(&sql).unwrap();
        let rows = stmt
            .query_map(params_from_iter(bind.iter()), list_row)
            .unwrap()
            .count();
        assert_eq!(rows, limit);
        assert_eq!(stmt.get_status(rusqlite::StatementStatus::FullscanStep), 0);
        stmt.get_status(rusqlite::StatementStatus::VmStep)
    }

    #[test]
    fn a_window_read_touches_only_its_rows() {
        // The same window from a folder twenty times larger costs the same:
        // the read seeks to its first row and stops after its last.
        let small = window_steps(100, 20);
        let large = window_steps(2_000, 20);
        assert!(
            large <= small + small / 10,
            "{small} steps at 100 threads, {large} at 2000"
        );
        // And it grows with the window, not the folder.
        let wider = window_steps(2_000, 40);
        assert!(wider > large, "{large} steps for 20 rows, {wider} for 40");
    }

    fn temp_mailbox(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "fell-{name}-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn remove_mailbox(path: &std::path::Path) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn summary_version_of(store: &Store) -> String {
        store
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'thread_summary'",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn snippet_of(store: &Store, thread: &str) -> String {
        store
            .conn
            .query_row(
                "SELECT snippet FROM thread_summary WHERE thread_id = ?1",
                params![thread],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn fill(store: &Store, seed: u64, count: usize) {
        store.upsert_account(&account()).unwrap();
        let mut rng = Rng(seed);
        let batch: Vec<Message> = (0..count)
            .map(|index| message(&mut rng, index, &[]))
            .collect();
        store
            .apply_delta(
                &account().id,
                &MailboxDelta {
                    messages: batch,
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
    }

    #[test]
    fn a_version_one_mailbox_gets_list_dates_without_a_recompute() {
        let path = temp_mailbox("thread-folders-v1");
        let (kept, queued, kept_snippet) = {
            let store = Store::open(&path).unwrap();
            fill(&store, 13, 30);
            // The table as version 1 left it: the same rows, no `last_date`.
            store
                .conn
                .execute_batch(
                    "CREATE TABLE thread_folders_v1 (
                         account_id TEXT NOT NULL,
                         folder_key TEXT NOT NULL,
                         thread_id TEXT NOT NULL,
                         display TEXT,
                         first_rowid INTEGER NOT NULL,
                         first_pos INTEGER NOT NULL,
                         unread INTEGER NOT NULL,
                         PRIMARY KEY (account_id, thread_id, folder_key)
                     ) WITHOUT ROWID;
                     INSERT INTO thread_folders_v1
                         SELECT account_id, folder_key, thread_id, display, first_rowid,
                                first_pos, unread
                         FROM thread_folders;
                     DROP TABLE thread_folders;
                     ALTER TABLE thread_folders_v1 RENAME TO thread_folders;
                     CREATE INDEX idx_thread_folders_folder
                         ON thread_folders(account_id, folder_key, first_rowid, first_pos);
                     UPDATE meta SET value = '1' WHERE key = 'thread_summary';",
                )
                .unwrap();
            let threads: Vec<String> = store
                .conn
                .prepare("SELECT thread_id FROM thread_summary ORDER BY thread_id")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<std::result::Result<_, _>>()
                .unwrap();
            let (kept, queued) = (threads[0].clone(), threads[1].clone());
            let kept_snippet = snippet_of(&store, &kept);
            // Marks only a recompute would undo. `queued` waits in the
            // refresh queue, as a writer that crashed would leave it.
            store
                .conn
                .execute(
                    "UPDATE thread_summary SET snippet = 'marker' WHERE thread_id IN (?1, ?2)",
                    params![kept, queued],
                )
                .unwrap();
            store
                .conn
                .execute(
                    "INSERT INTO thread_summary_dirty (account_id, thread_id) VALUES (?1, ?2)",
                    params![account().id.0, queued],
                )
                .unwrap();
            (kept, queued, kept_snippet)
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(summary_version_of(&store), SUMMARY_VERSION);
        assert_eq!(
            snippet_of(&store, &kept),
            "marker",
            "the upgrade must not recompute threads from messages"
        );
        assert_ne!(
            snippet_of(&store, &queued),
            "marker",
            "queued threads refresh"
        );
        store
            .conn
            .execute(
                "UPDATE thread_summary SET snippet = ?1 WHERE thread_id = ?2",
                params![kept_snippet, kept],
            )
            .unwrap();
        assert_lists_match(&store, "after the version 1 upgrade", true);
        drop(store);
        remove_mailbox(&path);
    }

    #[test]
    fn a_threads_folder_key_matches_its_list_row() {
        let store = Store::open_in_memory().unwrap();
        fill(&store, 23, 40);
        let account = account().id;
        let threads: Vec<String> = store
            .conn
            .prepare("SELECT thread_id FROM thread_summary")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        // One thread waits in the refresh queue with a stale stored date.
        store
            .conn
            .execute_batch(&format!(
                "UPDATE thread_folders SET last_date = 1 WHERE thread_id = '{0}';
                 INSERT INTO thread_summary_dirty (account_id, thread_id)
                     VALUES ('{1}', '{0}');",
                threads[0], account.0
            ))
            .unwrap();
        for folder in QUERIES {
            let listed = store.folder_threads(&account, folder).unwrap();
            for thread in &threads {
                let thread = ThreadId(thread.clone());
                let expected = listed
                    .iter()
                    .find(|row| row.thread_id == thread)
                    .map(ListKey::of);
                assert_eq!(
                    store.folder_key_of(&account, folder, &thread).unwrap(),
                    expected,
                    "{folder} {}",
                    thread.0
                );
            }
        }
    }

    #[test]
    fn a_version_one_binary_can_still_write_folder_rows() {
        let path = temp_mailbox("thread-folders-old-binary");
        {
            let store = Store::open(&path).unwrap();
            fill(&store, 17, 30);
            // What a version 1 binary does on seeing version 2: rebuild the
            // rows with its own INSERT, which has no `last_date`, and stamp 1.
            store
                .conn
                .execute_batch(
                    "CREATE TEMP TABLE old_rows AS SELECT * FROM thread_folders;
                     DELETE FROM thread_folders;
                     INSERT INTO thread_folders (
                         account_id, folder_key, thread_id, display, first_rowid, first_pos,
                         unread
                     )
                     SELECT account_id, folder_key, thread_id, display, first_rowid,
                            first_pos, unread
                     FROM temp.old_rows;
                     DROP TABLE temp.old_rows;
                     UPDATE meta SET value = '1' WHERE key = 'thread_summary';",
                )
                .unwrap();
        }
        // This version opens it again and puts the dates back.
        let store = Store::open(&path).unwrap();
        assert_eq!(summary_version_of(&store), SUMMARY_VERSION);
        assert_lists_match(&store, "after a version 1 binary wrote rows", true);
        drop(store);
        remove_mailbox(&path);
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

    /// Extra Inbox diagnostics over the budget suite's mailbox.
    /// Run with
    /// `cargo test --release -p fell-core --lib measure_inbox_reads -- --ignored --nocapture`.
    #[test]
    #[ignore = "builds a 100k-message mailbox; run by hand in release"]
    fn measure_inbox_reads_at_100k_messages() {
        use crate::test_support::mailbox;
        use std::time::{Duration, Instant};

        let path = temp_mailbox("measure-inbox");
        let built = Instant::now();
        mailbox::seed(&path).unwrap();
        println!("built 100k messages in {:.1?}", built.elapsed());
        let index_lock = mailbox::hold_indexer(&path).unwrap();
        let store = Store::open(&path).unwrap();
        let account = mailbox::account().id;

        fn median(mut runs: Vec<Duration>) -> Duration {
            runs.sort();
            runs[runs.len() / 2]
        }
        let time = |label: &str, read: &dyn Fn() -> usize| {
            let rows = read();
            let runs: Vec<Duration> = (0..21)
                .map(|_| {
                    let started = Instant::now();
                    read();
                    started.elapsed()
                })
                .collect();
            println!("{label}: {rows} rows, median {:.2?}", median(runs));
        };
        let full = store.folder_threads(&account, "INBOX").unwrap();
        time("whole Inbox (folder_threads)", &|| {
            store.folder_threads(&account, "INBOX").unwrap().len()
        });
        time("first window, Top 100", &|| {
            store
                .folder_page(&account, "INBOX", &FolderSpan::Top { rows: 100 })
                .unwrap()
                .threads
                .len()
        });
        let middle = full.len() / 2;
        time("refresh of 100 rows mid-folder, plus 40 after", &|| {
            store
                .folder_page(
                    &account,
                    "INBOX",
                    &FolderSpan::Window {
                        first: Some(ListKey::of(&full[middle])),
                        last: ListKey::of(&full[middle + 99]),
                        before: 0,
                        after: 40,
                    },
                )
                .unwrap()
                .threads
                .len()
        });
        time("G: Bottom 100", &|| {
            store
                .folder_page(&account, "INBOX", &FolderSpan::Bottom { rows: 100 })
                .unwrap()
                .threads
                .len()
        });
        let filter = ListFilter::default();
        time("thread_list Inbox, 50 rows mid-folder", &|| {
            store
                .thread_list(
                    &account,
                    ListSource::Folder("INBOX"),
                    &filter,
                    Some(&ListKey::of(&full[middle])),
                    50,
                )
                .unwrap()
                .threads
                .len()
        });
        let unread = ListFilter {
            unread_only: true,
            since: None,
        };
        time("thread_list Inbox unread only, 50 rows", &|| {
            store
                .thread_list(&account, ListSource::Folder("INBOX"), &unread, None, 50)
                .unwrap()
                .threads
                .len()
        });
        time("thread_list label tag1, 50 rows", &|| {
            store
                .thread_list(&account, ListSource::Label("tag1"), &filter, None, 50)
                .unwrap()
                .threads
                .len()
        });
        time("unread count only (Top 0)", &|| {
            store
                .folder_page(&account, "INBOX", &FolderSpan::Top { rows: 0 })
                .unwrap()
                .unread
        });
        let thread = mailbox::long_thread(0);
        time("open thread (thread_messages, 50 messages)", &|| {
            store.thread_messages(&account, &thread).unwrap().len()
        });
        time("open thread, then message_body for each of the 50", &|| {
            let messages = store.thread_messages(&account, &thread).unwrap();
            for message in &messages {
                store
                    .message_body(&account, &message.headers.message_id)
                    .unwrap()
                    .expect("parts stored");
            }
            messages.len()
        });
        let one = full[middle].thread_id.clone();
        let first_id = store.thread_messages(&account, &one).unwrap()[0]
            .headers
            .message_id
            .clone();
        time("one message by id (Store::message)", &|| {
            usize::from(store.message(&account, &first_id).unwrap().is_some())
        });
        time(
            "backfill check with nothing missing (missing_bodies)",
            &|| store.missing_bodies(&account, 200).unwrap().len(),
        );
        let stored_bytes: i64 = store
            .conn
            .query_row(
                "SELECT SUM(length(html)) + SUM(length(plain)) FROM message_bodies",
                [],
                |row| row.get(0),
            )
            .unwrap();
        println!("parts: {:.1} MB stored", stored_bytes as f64 / 1e6);
        drop(store);
        drop(index_lock);
        let size = std::fs::metadata(&path).unwrap().len();
        println!("mailbox file {:.1} MB", size as f64 / 1e6);
        remove_mailbox(&path);
        let _ = std::fs::remove_file(format!("{}.embed.lock", path.display()));
    }
}
