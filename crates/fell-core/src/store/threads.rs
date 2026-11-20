//! Thread ids after a write.
//!
//! A write marks what it touched: triggers queue each inserted or re-parented
//! row in `thread_dirty_rows`, and each Message-ID that came or went (a
//! deleted row, a new alias) in `thread_dirty_ids`. [`rethread_dirty`] then
//! resolves only those rows plus the stored replies that name a queued id,
//! found through `message_refs`. The cost of a sync batch follows the batch,
//! not the mailbox.
//!
//! A reply whose sender dropped `In-Reply-To` and `References` still joins
//! its original when the subjects match after reply markers, the two
//! senders appear in each other's participants, and the original is at most
//! [`SUBJECT_WINDOW_SECS`] older. `subject_index` finds those candidates in
//! both directions, so it works whichever copy syncs first.
//!
//! The merge rules are the ones [`rethread_all`] applies to a whole account,
//! in the same row order. Every row a write did not touch already sits on the
//! thread those rules give it, so leaving it out changes nothing. The upgrade
//! path runs [`rethread_all`] once to start from that state.

use std::collections::{BTreeSet, HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};

use super::{immediate_transaction, load_aliases, parse_references};
use crate::Result;
use crate::account::AccountId;
use crate::message::{Address, MessageHeaders, MessageId, decode_addresses};
use crate::threading::{
    SUBJECT_WINDOW_SECS, ThreadId, header_parent_thread_id, resolve_thread_id, subject_is_generic,
    thread_subject,
};

/// Tables behind [`rethread_dirty`]. Safe to run on every open.
const THREAD_INDEX_TABLES: &str = r#"
CREATE TABLE IF NOT EXISTS message_refs (
    account_id TEXT NOT NULL,
    ref_id TEXT NOT NULL,
    child_rowid INTEGER NOT NULL,
    PRIMARY KEY (account_id, ref_id, child_rowid)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_message_refs_child ON message_refs(child_rowid);
CREATE INDEX IF NOT EXISTS idx_message_aliases_canonical
    ON message_aliases(account_id, canonical_message_id);

-- Non-generic subjects with reply markers stripped, for replies that lost
-- their threading headers.
CREATE TABLE IF NOT EXISTS subject_index (
    account_id TEXT NOT NULL,
    subject_key TEXT NOT NULL,
    date_unix INTEGER NOT NULL,
    message_rowid INTEGER NOT NULL,
    PRIMARY KEY (account_id, subject_key, date_unix, message_rowid)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_subject_index_row ON subject_index(message_rowid);

-- Subjects of deleted rows. Replies that matched them may match another.
CREATE TABLE IF NOT EXISTS thread_dirty_subjects (
    account_id TEXT NOT NULL,
    subject_key TEXT NOT NULL,
    date_unix INTEGER NOT NULL,
    PRIMARY KEY (account_id, subject_key, date_unix)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS thread_dirty_rows (
    rowid INTEGER PRIMARY KEY
);
CREATE TABLE IF NOT EXISTS thread_dirty_ids (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
) WITHOUT ROWID;
"#;

/// Bump when [`THREAD_INDEX_TRIGGERS`] changes. Triggers are rebuilt only
/// then, in one transaction, so no other connection writes while they are
/// missing.
const THREAD_TRIGGERS_VERSION: &str = "4";

// Inserts check NOT EXISTS instead of OR IGNORE: an UPSERT that fires a
// trigger overrides the trigger's own conflict clause.
const THREAD_INDEX_TRIGGERS: &str = r#"
DROP TRIGGER IF EXISTS thread_dirty_insert;
DROP TRIGGER IF EXISTS thread_dirty_update;
DROP TRIGGER IF EXISTS thread_dirty_delete;
DROP TRIGGER IF EXISTS thread_dirty_alias_insert;
DROP TRIGGER IF EXISTS thread_dirty_alias_update;
DROP TRIGGER IF EXISTS thread_dirty_alias_delete;

CREATE TRIGGER thread_dirty_insert AFTER INSERT ON messages BEGIN
    INSERT INTO thread_dirty_rows (rowid) SELECT new.rowid
        WHERE NOT EXISTS (SELECT 1 FROM thread_dirty_rows WHERE rowid = new.rowid);
END;

CREATE TRIGGER thread_dirty_update AFTER UPDATE OF
    message_id, in_reply_to, references_header, proton_conversation_id,
    subject, date_unix, from_email, to_line, cc_line
ON messages BEGIN
    INSERT INTO thread_dirty_rows (rowid) SELECT new.rowid
        WHERE NOT EXISTS (SELECT 1 FROM thread_dirty_rows WHERE rowid = new.rowid);
    INSERT INTO thread_dirty_ids (account_id, message_id)
    SELECT old.account_id, old.message_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_dirty_ids
        WHERE account_id = old.account_id AND message_id = old.message_id
    );
END;

CREATE TRIGGER thread_dirty_delete AFTER DELETE ON messages BEGIN
    DELETE FROM message_refs WHERE child_rowid = old.rowid;
    INSERT INTO thread_dirty_subjects (account_id, subject_key, date_unix)
        SELECT s.account_id, s.subject_key, s.date_unix FROM subject_index s
        WHERE s.message_rowid = old.rowid
          AND NOT EXISTS (
              SELECT 1 FROM thread_dirty_subjects d
              WHERE d.account_id = s.account_id AND d.subject_key = s.subject_key
                AND d.date_unix = s.date_unix
          );
    DELETE FROM subject_index WHERE message_rowid = old.rowid;
    DELETE FROM thread_dirty_rows WHERE rowid = old.rowid;
    INSERT INTO thread_dirty_ids (account_id, message_id)
    SELECT old.account_id, old.message_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_dirty_ids
        WHERE account_id = old.account_id AND message_id = old.message_id
    );
END;

CREATE TRIGGER thread_dirty_alias_insert AFTER INSERT ON message_aliases BEGIN
    INSERT INTO thread_dirty_ids (account_id, message_id)
    SELECT new.account_id, new.message_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_dirty_ids
        WHERE account_id = new.account_id AND message_id = new.message_id
    );
END;

CREATE TRIGGER thread_dirty_alias_update AFTER UPDATE OF
    message_id, canonical_message_id
ON message_aliases BEGIN
    INSERT INTO thread_dirty_ids (account_id, message_id)
    SELECT old.account_id, old.message_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_dirty_ids
        WHERE account_id = old.account_id AND message_id = old.message_id
    );
    INSERT INTO thread_dirty_ids (account_id, message_id)
    SELECT new.account_id, new.message_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_dirty_ids
        WHERE account_id = new.account_id AND message_id = new.message_id
    );
END;

-- Replies stored under an alias id stop resolving through it.
CREATE TRIGGER thread_dirty_alias_delete AFTER DELETE ON message_aliases BEGIN
    INSERT INTO thread_dirty_ids (account_id, message_id)
    SELECT old.account_id, old.message_id
    WHERE NOT EXISTS (
        SELECT 1 FROM thread_dirty_ids
        WHERE account_id = old.account_id AND message_id = old.message_id
    );
END;
"#;

/// Tables and triggers only. Runs before any upgrade that re-resolves
/// threads, since the whole-account pass reads `subject_index`.
pub(super) fn ensure_thread_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(THREAD_INDEX_TABLES)?;
    super::ensure_triggers(
        conn,
        "thread_triggers",
        THREAD_TRIGGERS_VERSION,
        THREAD_INDEX_TRIGGERS,
    )
}

/// Rows read per batch while the upgrade fills `message_refs`.
const BACKFILL_BATCH: i64 = 2_000;

/// Create the reference index and its triggers, and fill the index the first
/// time a mailbox opens with this code.
///
/// The fill re-resolves every account once with [`rethread_all`] so later
/// writes can rely on untouched rows already being on their final thread.
pub(super) fn ensure_thread_index(conn: &Connection) -> Result<()> {
    ensure_thread_schema(conn)?;
    let current = |conn: &Connection| -> Result<bool> {
        Ok(flag_is_set(conn, "thread_index")? && flag_is_set(conn, "thread_subjects")?)
    };
    if current(conn)? {
        return Ok(());
    }
    let tx = immediate_transaction(conn)?;
    // Another connection may have finished the fill while this one waited.
    if current(&tx)? {
        return Ok(());
    }
    // Fill what is missing, then re-resolve every account once.
    if !flag_is_set(&tx, "thread_index")? {
        fill_refs(&tx)?;
    }
    if !flag_is_set(&tx, "thread_subjects")? {
        fill_subjects(&tx)?;
    }
    settle_all(&tx, &["thread_index", "thread_subjects"])?;
    tx.commit()?;
    Ok(())
}

fn fill_refs(tx: &Connection) -> Result<()> {
    tx.execute("DELETE FROM message_refs", [])?;
    let mut after = 0i64;
    loop {
        let batch = {
            let mut stmt = tx.prepare(
                "SELECT rowid, account_id, in_reply_to, references_header
                 FROM messages WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![after, BACKFILL_BATCH], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let Some(last) = batch.last() else {
            break;
        };
        after = last.0;
        for (rowid, account, in_reply_to, references) in &batch {
            index_refs(tx, account, *rowid, in_reply_to.as_deref(), references)?;
        }
    }
    Ok(())
}

fn fill_subjects(tx: &Connection) -> Result<()> {
    tx.execute("DELETE FROM subject_index", [])?;
    let mut after = 0i64;
    loop {
        let batch = {
            let mut stmt = tx.prepare(
                "SELECT rowid, account_id, subject, date_unix
                 FROM messages WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![after, BACKFILL_BATCH], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            rows.collect::<std::result::Result<Vec<_>, _>>()?
        };
        let Some(last) = batch.last() else {
            break;
        };
        after = last.0;
        for (rowid, account, subject, date) in &batch {
            index_subject(tx, account, *rowid, subject, *date)?;
        }
    }
    Ok(())
}

/// Re-resolve every account once, empty the queues, and record `flags`.
fn settle_all(conn: &Connection, flags: &[&str]) -> Result<()> {
    let accounts = {
        let mut stmt = conn.prepare("SELECT DISTINCT account_id FROM messages")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for account in accounts {
        rethread_all(conn, &AccountId(account))?;
    }
    conn.execute("DELETE FROM thread_dirty_rows", [])?;
    conn.execute("DELETE FROM thread_dirty_ids", [])?;
    conn.execute("DELETE FROM thread_dirty_subjects", [])?;
    for flag in flags {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![flag],
        )?;
    }
    Ok(())
}

fn flag_is_set(conn: &Connection, flag: &str) -> Result<bool> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![flag],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value.as_deref() == Some("1"))
}

/// Replace the `subject_index` entry for one row. Generic subjects and empty
/// ones get none, so they never join anything. Returns the entry it
/// replaced, if any, so replies that matched the old subject are checked.
fn index_subject(
    conn: &Connection,
    account: &str,
    rowid: i64,
    subject: &str,
    date: i64,
) -> Result<Option<(String, i64)>> {
    let old: Option<(String, i64)> = conn
        .prepare_cached(
            "SELECT subject_key, date_unix FROM subject_index WHERE message_rowid = ?1",
        )?
        .query_row(params![rowid], |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?;
    conn.prepare_cached("DELETE FROM subject_index WHERE message_rowid = ?1")?
        .execute(params![rowid])?;
    let (key, _) = thread_subject(subject);
    if !subject_is_generic(&key) {
        conn.prepare_cached(
            "INSERT OR IGNORE INTO subject_index (account_id, subject_key, date_unix, message_rowid)
             VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![account, key, date, rowid])?;
    }
    Ok(old)
}

/// The thread a reply with no stored header parent joins by subject: the
/// latest message in the window with the same subject whose sender and the
/// reply's sender appear in each other's participants.
fn subject_parent(
    conn: &Connection,
    account: &str,
    row: &ThreadingRow,
) -> Result<Option<ThreadId>> {
    let (key, reply) = thread_subject(&row.headers.subject);
    if !reply || subject_is_generic(&key) {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT m.thread_id, m.from_email, m.to_line, m.cc_line
         FROM subject_index s
         JOIN messages m ON m.rowid = s.message_rowid
         WHERE s.account_id = ?1 AND s.subject_key = ?2
           AND s.date_unix BETWEEN ?3 AND ?4 AND s.message_rowid != ?5
         ORDER BY s.date_unix DESC, s.message_rowid DESC",
    )?;
    let date = row.headers.date_unix;
    let mut rows = stmt.query(params![
        account,
        key,
        date.saturating_sub(SUBJECT_WINDOW_SECS),
        date,
        row.rowid
    ])?;
    while let Some(candidate) = rows.next()? {
        let thread: String = candidate.get(0)?;
        let from = candidate.get::<_, String>(1)?.to_ascii_lowercase();
        let to: String = candidate.get(2)?;
        let cc: String = candidate.get(3)?;
        let theirs = participants(&from, &to, &cc);
        if !theirs.contains(&row.from) || !row.participants.contains(&from) {
            continue;
        }
        // One sender writing twice: the notes are one conversation only when
        // they also share someone else, or "me to Ada" would join "me to Bob".
        if from == row.from
            && !row
                .participants
                .iter()
                .any(|address| address != &from && theirs.contains(address))
        {
            continue;
        }
        return Ok(Some(ThreadId(thread)));
    }
    Ok(None)
}

fn participants(from: &str, to: &str, cc: &str) -> HashSet<String> {
    let mut out: HashSet<String> = decode_addresses(to)
        .into_iter()
        .chain(decode_addresses(cc))
        .map(|address| address.email.to_ascii_lowercase())
        .collect();
    out.insert(from.to_ascii_lowercase());
    out
}

/// Replace the parent ids `message_refs` lists for one row.
///
/// Keys are the strings threading compares: `In-Reply-To` as stored, and
/// each `References` id normalized the way [`parse_references`] reads it.
fn index_refs(
    conn: &Connection,
    account: &str,
    rowid: i64,
    in_reply_to: Option<&str>,
    references: &str,
) -> Result<()> {
    conn.prepare_cached("DELETE FROM message_refs WHERE child_rowid = ?1")?
        .execute(params![rowid])?;
    let mut insert = conn.prepare_cached(
        "INSERT OR IGNORE INTO message_refs (account_id, ref_id, child_rowid)
         VALUES (?1, ?2, ?3)",
    )?;
    if let Some(parent) = in_reply_to.filter(|id| !id.is_empty()) {
        insert.execute(params![account, parent, rowid])?;
    }
    for parent in parse_references(references) {
        if !parent.as_str().is_empty() {
            insert.execute(params![account, parent.as_str(), rowid])?;
        }
    }
    Ok(())
}

/// Re-resolve the rows `account_id`'s writes queued since the last pass, and
/// the stored replies that name a queued Message-ID. Clears that account's
/// queue. Other accounts' queued rows wait for their own pass.
pub(super) fn rethread_dirty(conn: &Connection, account_id: &AccountId) -> Result<()> {
    let account = account_id.as_str();
    let dirty = {
        let mut stmt = conn.prepare_cached(
            "SELECT m.rowid, m.message_id, m.in_reply_to, m.references_header,
                    m.subject, m.date_unix
             FROM thread_dirty_rows d
             JOIN messages m ON m.rowid = d.rowid
             WHERE m.account_id = ?1
             ORDER BY m.rowid",
        )?;
        let rows = stmt.query_map(params![account], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut ids: HashSet<String> = {
        let mut stmt =
            conn.prepare_cached("SELECT message_id FROM thread_dirty_ids WHERE account_id = ?1")?;
        let rows = stmt.query_map(params![account], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<HashSet<_>, _>>()?
    };
    // Subject and date of every message that came, went, or changed: later
    // replies without headers in its window may now match differently.
    let mut subjects: HashSet<(String, i64)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT subject_key, date_unix FROM thread_dirty_subjects WHERE account_id = ?1",
        )?;
        let rows = stmt.query_map(params![account], |row| Ok((row.get(0)?, row.get(1)?)))?;
        rows.collect::<std::result::Result<HashSet<_>, _>>()?
    };
    conn.prepare_cached("DELETE FROM thread_dirty_subjects WHERE account_id = ?1")?
        .execute(params![account])?;
    let mut affected = BTreeSet::new();
    {
        let mut clear = conn.prepare_cached("DELETE FROM thread_dirty_rows WHERE rowid = ?1")?;
        // A resync can move a row to another thread directly. Replies that
        // cite one of its aliases instead of its own id must follow it.
        let mut aliases = conn.prepare_cached(
            "SELECT message_id FROM message_aliases
             WHERE account_id = ?1 AND canonical_message_id = ?2",
        )?;
        for (rowid, message_id, in_reply_to, references, subject, date) in &dirty {
            index_refs(conn, account, *rowid, in_reply_to.as_deref(), references)?;
            for alias in
                aliases.query_map(params![account, message_id], |row| row.get::<_, String>(0))?
            {
                ids.insert(alias?);
            }
            if let Some(old) = index_subject(conn, account, *rowid, subject, *date)? {
                subjects.insert(old);
            }
            let (key, _) = thread_subject(subject);
            if !subject_is_generic(&key) {
                subjects.insert((key, *date));
            }
            ids.insert(message_id.clone());
            affected.insert(*rowid);
            clear.execute(params![rowid])?;
        }
    }
    {
        let mut later = conn.prepare_cached(
            "SELECT message_rowid FROM subject_index
             WHERE account_id = ?1 AND subject_key = ?2 AND date_unix BETWEEN ?3 AND ?4",
        )?;
        for (key, date) in &subjects {
            let rows = later.query_map(
                params![account, key, date, date.saturating_add(SUBJECT_WINDOW_SECS)],
                |row| row.get::<_, i64>(0),
            )?;
            for row in rows {
                affected.insert(row?);
            }
        }
    }
    conn.prepare_cached("DELETE FROM thread_dirty_ids WHERE account_id = ?1")?
        .execute(params![account])?;
    {
        let mut children = conn.prepare_cached(
            "SELECT child_rowid FROM message_refs WHERE account_id = ?1 AND ref_id = ?2",
        )?;
        for id in &ids {
            let rows = children.query_map(params![account, id], |row| row.get::<_, i64>(0))?;
            for row in rows {
                affected.insert(row?);
            }
        }
    }
    if affected.is_empty() {
        return Ok(());
    }

    let rows = {
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {THREADING_COLS} FROM messages WHERE rowid = ?1 AND account_id = ?2"
        ))?;
        let mut out = Vec::with_capacity(affected.len());
        for rowid in &affected {
            if let Some(row) = stmt
                .query_row(params![rowid, account], threading_row)
                .optional()?
            {
                out.push(row);
            }
        }
        out
    };

    let mut parents: HashMap<MessageId, Option<ThreadId>> = HashMap::new();
    let mut lookup = |parent: &MessageId| -> Result<Option<ThreadId>> {
        if let Some(found) = parents.get(parent) {
            return Ok(found.clone());
        }
        let found = stored_thread(conn, account, parent.as_str())?;
        parents.insert(parent.clone(), found.clone());
        Ok(found)
    };
    let edges = thread_edges(&rows, &mut lookup, &mut |row| {
        subject_parent(conn, account, row)
    })?;

    let mut union = ThreadUnion::default();
    let mut rank = conn.prepare_cached(
        "SELECT MIN(rowid) FROM messages WHERE account_id = ?1 AND thread_id = ?2",
    )?;
    for node in edge_nodes(&edges) {
        let earliest: Option<i64> = rank.query_row(params![account, node], |row| row.get(0))?;
        if let Some(earliest) = earliest {
            union.note(node, earliest);
        }
    }
    for (member, preferred) in &edges {
        union.union_prefer(member, preferred);
    }
    let nodes: Vec<String> = edge_nodes(&edges).map(str::to_string).collect();
    relabel(
        conn,
        account_id,
        &mut union,
        nodes.iter().map(String::as_str),
    )
}

/// Index the subjects of queued rows without resolving anything. The test
/// oracle runs this before [`rethread_all`], which reads the index.
#[cfg(test)]
pub(super) fn index_queued_subjects(conn: &Connection) -> Result<()> {
    let queued = {
        let mut stmt = conn.prepare(
            "SELECT m.rowid, m.account_id, m.subject, m.date_unix
             FROM thread_dirty_rows d JOIN messages m ON m.rowid = d.rowid",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    for (rowid, account, subject, date) in queued {
        index_subject(conn, &account, rowid, &subject, date)?;
    }
    Ok(())
}

/// The thread a stored parent wears, the way [`rethread_all`] reads it: an
/// alias whose canonical row is stored wins over a row with that id.
fn stored_thread(conn: &Connection, account: &str, id: &str) -> Result<Option<ThreadId>> {
    let via_alias: Option<String> = conn
        .prepare_cached(
            "SELECT m.thread_id FROM message_aliases a
             JOIN messages m
               ON m.account_id = a.account_id AND m.message_id = a.canonical_message_id
             WHERE a.account_id = ?1 AND a.message_id = ?2",
        )?
        .query_row(params![account, id], |row| row.get(0))
        .optional()?;
    if via_alias.is_some() {
        return Ok(via_alias.map(ThreadId));
    }
    let direct: Option<String> = conn
        .prepare_cached("SELECT thread_id FROM messages WHERE account_id = ?1 AND message_id = ?2")?
        .query_row(params![account, id], |row| row.get(0))
        .optional()?;
    Ok(direct.map(ThreadId))
}

/// Re-resolve thread ids for an account in one in-memory pass.
///
/// A proton id absorbs a non-proton header thread. Two different proton ids
/// stay apart. Non-proton merges keep the earliest already-assigned id.
/// Reads every row in the account, so only upgrades call it; writes use
/// [`rethread_dirty`].
pub(super) fn rethread_all(conn: &Connection, account_id: &AccountId) -> Result<()> {
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
    let account = account_id.as_str();
    let edges = thread_edges(
        &rows,
        &mut |parent| Ok(by_message.get(parent).cloned()),
        &mut |row| subject_parent(conn, account, row),
    )?;
    for (member, preferred) in &edges {
        union.union_prefer(member, preferred);
    }
    let nodes: Vec<String> = rows.iter().map(|row| row.thread_id.0.clone()).collect();
    relabel(
        conn,
        account_id,
        &mut union,
        nodes.iter().map(String::as_str),
    )
}

/// Merges to apply, in row order: each row's thread onto the thread its
/// headers resolve to, and its header parent's thread onto that same one.
///
/// A row with no Proton id and no stored header parent may also join by
/// subject (see [`subject_parent`]).
fn thread_edges(
    rows: &[ThreadingRow],
    lookup: &mut dyn FnMut(&MessageId) -> Result<Option<ThreadId>>,
    by_subject: &mut dyn FnMut(&ThreadingRow) -> Result<Option<ThreadId>>,
) -> Result<Vec<(String, String)>> {
    let mut edges = Vec::with_capacity(rows.len() * 2);
    for row in rows {
        let onto = resolve_thread_id(&row.headers, &mut *lookup)?;
        let header = header_parent_thread_id(&row.headers, &mut *lookup)?;
        edges.push((row.thread_id.0.clone(), onto.0.clone()));
        let proton = row
            .headers
            .proton_conversation_id
            .as_deref()
            .is_some_and(|id| !id.trim().is_empty());
        // A reply that names any parent waits for that parent instead: a
        // subject join cannot be undone when the real parent arrives later.
        let headerless = row.headers.parent_ids().next().is_none();
        match header {
            Some(header) => edges.push((header.0, onto.0)),
            None if !proton && headerless => {
                if let Some(original) = by_subject(row)? {
                    edges.push((row.thread_id.0.clone(), original.0));
                }
            }
            None => {}
        }
    }
    Ok(edges)
}

/// Every thread id an edge names, once each.
fn edge_nodes(edges: &[(String, String)]) -> impl Iterator<Item = &str> {
    let mut seen = HashSet::new();
    edges
        .iter()
        .flat_map(|(member, preferred)| [member.as_str(), preferred.as_str()])
        .filter(move |node| seen.insert(*node))
}

/// Move every row on a merged-away thread id onto its root.
fn relabel<'a>(
    conn: &Connection,
    account_id: &AccountId,
    union: &mut ThreadUnion,
    nodes: impl Iterator<Item = &'a str>,
) -> Result<()> {
    let mut written = HashSet::new();
    let mut update = conn.prepare_cached(
        "UPDATE messages SET thread_id = ?1 WHERE account_id = ?2 AND thread_id = ?3",
    )?;
    for from in nodes {
        let onto = union.find(from);
        if from == onto || !written.insert(from.to_string()) {
            continue;
        }
        update.execute(params![onto.as_str(), account_id.as_str(), from])?;
    }
    Ok(())
}

/// Threading columns only — bodies stay on disk.
struct ThreadingRow {
    rowid: i64,
    thread_id: ThreadId,
    /// `subject` and `date_unix` are filled for the subject fallback.
    headers: MessageHeaders,
    /// Sender, lowercased.
    from: String,
    /// Sender, To, and Cc, lowercased.
    participants: HashSet<String>,
}

const THREADING_COLS: &str = "rowid, message_id, thread_id, proton_conversation_id, in_reply_to,
    references_header, subject, date_unix, from_email, to_line, cc_line";

fn threading_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadingRow> {
    let rowid: i64 = row.get(0)?;
    let message_id: String = row.get(1)?;
    let thread_id: String = row.get(2)?;
    let proton_conversation_id: Option<String> = row.get(3)?;
    let in_reply_to: Option<String> = row.get(4)?;
    let references: String = row.get(5)?;
    let subject: String = row.get(6)?;
    let date_unix: i64 = row.get(7)?;
    let from = row.get::<_, String>(8)?.to_ascii_lowercase();
    let to: String = row.get(9)?;
    let cc: String = row.get(10)?;
    Ok(ThreadingRow {
        rowid,
        thread_id: ThreadId(thread_id),
        participants: participants(&from, &to, &cc),
        from,
        headers: MessageHeaders {
            message_id: MessageId(message_id),
            proton_conversation_id,
            in_reply_to: in_reply_to.map(MessageId),
            references: parse_references(&references),
            from: Address {
                name: None,
                email: String::new(),
            },
            subject,
            date_unix,
        },
    })
}

fn load_threading_rows(conn: &Connection, account_id: &AccountId) -> Result<Vec<ThreadingRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {THREADING_COLS} FROM messages WHERE account_id = ?1 ORDER BY rowid ASC"
    ))?;
    let rows = stmt.query_map(params![account_id.as_str()], threading_row)?;
    rows.collect::<std::result::Result<Vec<_>, _>>()
        .map_err(Into::into)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, BackendKind};
    use crate::backend::MailboxDelta;
    use crate::message::{Message, RemoteId};
    use crate::store::Store;

    /// Small deterministic generator so failures replay from the seed.
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

    #[allow(clippy::too_many_arguments)]
    fn message(
        id: &str,
        parent: Option<&str>,
        references: &[String],
        proton: Option<String>,
        folder: &str,
        sender: &str,
        body: &str,
        date: i64,
        list_id: Option<&str>,
    ) -> Message {
        let message_id = MessageId::new(id);
        Message {
            account_id: account().id,
            headers: MessageHeaders {
                message_id: message_id.clone(),
                proton_conversation_id: proton,
                in_reply_to: parent.map(MessageId::new),
                references: references.iter().map(MessageId::new).collect(),
                from: Address {
                    name: None,
                    email: sender.into(),
                },
                subject: format!("subject {id}"),
                date_unix: date,
            },
            remote_id: RemoteId(format!("r-{id}-{folder}")),
            thread_id: ThreadId::from_message_id(&message_id),
            snippet: body.chars().take(20).collect(),
            body_text: Some(body.into()),
            unread: true,
            folders: vec![folder.into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: list_id.map(str::to_string),
            reply_to: Some(Vec::new()),
            attachment_names: Vec::new(),
            parts: None,
        }
    }

    enum Op {
        Batch(Vec<Message>),
        Delete(String),
        /// Move this message's thread to Trash from its first folder; from
        /// Trash, delete it for good.
        Trash(String),
        Undo,
    }

    /// Arrival out of order, parents named only in `References`, Proton ids
    /// that conflict, resyncs with new parents, list echoes, and deletes.
    fn scenario(seed: u64) -> Vec<Op> {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let total = 120 + rng.below(120);
        let folders = ["INBOX", "Sent", "Archive"];
        let mut made: Vec<String> = Vec::new();
        let mut pending: Vec<Message> = Vec::new();
        for index in 0..total {
            let id = format!("m{index}@x");
            let parent =
                (!made.is_empty() && rng.chance(70)).then(|| made[rng.below(made.len())].clone());
            let mut references = Vec::new();
            if let Some(parent) = &parent
                && rng.chance(50)
            {
                if rng.chance(50) && !made.is_empty() {
                    references.push(made[rng.below(made.len())].clone());
                }
                references.push(parent.clone());
            }
            // Sometimes only References names the parent.
            let in_reply_to = if rng.chance(25) { None } else { parent.clone() };
            if in_reply_to.is_none()
                && let Some(parent) = &parent
                && references.is_empty()
            {
                references.push(parent.clone());
            }
            let proton = rng.chance(15).then(|| format!("conv{}", rng.below(4)));
            let sender = format!("s{}@x", rng.below(5));
            let body = format!("body of {id} {}", rng.next());
            // Hours apart, so some originals fall outside the subject window.
            let date = 1_700_000_000 + index as i64 * 3_600 * (1 + rng.below(12) as i64);
            let folder = folders[rng.below(folders.len())];
            let mut sent = message(
                &id,
                in_reply_to.as_deref(),
                &references,
                proton.clone(),
                folder,
                &sender,
                &body,
                date,
                None,
            );
            // A few topics shared by many messages, some as replies that
            // lost their headers, sent among a few people.
            let topic = format!("topic {}", rng.below(6));
            sent.headers.subject = match rng.below(4) {
                0 => topic,
                1 => format!("Re: {topic}"),
                2 => format!("AW: [list] {topic}"),
                _ => format!("subject {id}"),
            };
            if rng.chance(50) {
                sent.headers.in_reply_to = None;
                sent.headers.references.clear();
            }
            sent.to = vec![Address {
                name: None,
                email: format!("s{}@x", rng.below(5)),
            }];
            pending.push(sent);
            made.push(id.clone());
            if rng.chance(8) {
                // A list rewrites the Message-ID of the same post.
                // Same post, list subject tag: collapses into an alias.
                let echo = format!("echo{index}@list");
                let mut copy = message(
                    &echo,
                    in_reply_to.as_deref(),
                    &references,
                    None,
                    "INBOX",
                    &sender,
                    &body,
                    date + 60,
                    Some("list.x"),
                );
                copy.headers.subject = format!("[list] subject {id}");
                pending.push(copy);
                made.push(echo);
            }
            if rng.chance(10) {
                // The same id again later, from another folder, with a new parent.
                let reparent = (!made.is_empty()).then(|| made[rng.below(made.len())].clone());
                pending.push(message(
                    &id,
                    reparent.as_deref(),
                    &[],
                    proton,
                    folders[rng.below(folders.len())],
                    &sender,
                    &body,
                    date,
                    None,
                ));
            }
        }
        // Shuffle so replies often land before their parents.
        for index in (1..pending.len()).rev() {
            let other = rng.below(index + 1);
            pending.swap(index, other);
        }
        let mut ops = Vec::new();
        while !pending.is_empty() {
            let size = (1 + rng.below(40)).min(pending.len());
            ops.push(Op::Batch(pending.drain(..size).collect()));
            if rng.chance(20) {
                ops.push(Op::Delete(made[rng.below(made.len())].clone()));
            }
            if rng.chance(15) {
                ops.push(Op::Trash(made[rng.below(made.len())].clone()));
            }
            if rng.chance(10) {
                ops.push(Op::Undo);
            }
        }
        ops
    }

    fn threads_of(store: &Store) -> Vec<(String, String)> {
        let mut stmt = store
            .conn
            .prepare("SELECT message_id, thread_id FROM messages ORDER BY message_id")
            .unwrap();
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap()
    }

    fn run(store: &Store, op: &Op) {
        let account = account();
        match op {
            Op::Batch(messages) => store
                .apply_delta(
                    &account.id,
                    &MailboxDelta {
                        messages: messages.clone(),
                        flags: Vec::new(),
                        cursor: None,
                        ..MailboxDelta::default()
                    },
                )
                .unwrap(),
            Op::Delete(id) => {
                store
                    .delete_message(&account.id, &MessageId::new(id))
                    .unwrap();
            }
            Op::Trash(id) => {
                let found: Option<(String, String)> = store
                    .conn
                    .query_row(
                        "SELECT thread_id, folders FROM messages WHERE message_id = ?1",
                        params![MessageId::new(id).as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .unwrap();
                if let Some((thread, folders)) = found
                    && let Some(folder) = folders.split('\u{1e}').find(|f| !f.is_empty())
                {
                    // Refusals (nothing in that folder) are fine; both stores refuse alike.
                    let _ = store.delete_thread(&account.id, &ThreadId(thread), folder);
                }
            }
            Op::Undo => {
                let _ = store.undo(&account.id);
            }
        }
    }

    #[test]
    fn incremental_threads_match_the_whole_account_pass() {
        for seed in 1..=60 {
            let incremental = Store::open_in_memory().unwrap();
            let full = Store::open_in_memory().unwrap();
            full.full_rethread.set(true);
            for store in [&incremental, &full] {
                store.upsert_account(&account()).unwrap();
            }
            for (step, op) in scenario(seed).iter().enumerate() {
                run(&incremental, op);
                run(&full, op);
                assert_eq!(
                    threads_of(&incremental),
                    threads_of(&full),
                    "seed {seed}, step {step}"
                );
            }
            // A trailing delete is queued, not yet resolved. Resolve it the
            // way the next sync batch would, then nothing is left for a
            // whole-account pass to merge.
            rethread_dirty(&incremental.conn, &account().id).unwrap();
            rethread_all(&full.conn, &account().id).unwrap();
            let settled = threads_of(&incremental);
            assert_eq!(settled, threads_of(&full), "seed {seed} after the queue");
            rethread_all(&incremental.conn, &account().id).unwrap();
            assert_eq!(threads_of(&incremental), settled, "seed {seed} fixed point");
        }
    }

    #[test]
    fn a_late_parent_finds_replies_that_name_it_only_in_references() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let reply = message(
            "reply@x",
            None,
            &["root@x".to_string()],
            None,
            "INBOX",
            "a@x",
            "reply body",
            1_700_000_100,
            None,
        );
        let root = message(
            "root@x",
            None,
            &[],
            None,
            "Sent",
            "me@x",
            "root body",
            1_700_000_000,
            None,
        );
        run(&store, &Op::Batch(vec![reply]));
        run(&store, &Op::Batch(vec![root]));
        let threads = threads_of(&store);
        assert_eq!(threads[0].1, threads[1].1, "{threads:?}");
        let queued: i64 = store
            .conn
            .query_row(
                "SELECT (SELECT COUNT(*) FROM thread_dirty_rows)
                      + (SELECT COUNT(*) FROM thread_dirty_ids)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(queued, 0, "a pass leaves nothing queued");
    }

    #[test]
    fn an_existing_mailbox_is_indexed_once_on_open() {
        let path = std::env::temp_dir().join(format!(
            "fell-thread-index-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let store = Store::open(&path).unwrap();
            store.upsert_account(&account()).unwrap();
            let reply = message(
                "reply@x",
                Some("root@x"),
                &[],
                None,
                "INBOX",
                "a@x",
                "reply body",
                1_700_000_100,
                None,
            );
            run(&store, &Op::Batch(vec![reply]));
            // A mailbox from before the index: no refs, flag unset.
            store
                .conn
                .execute_batch(
                    "DELETE FROM message_refs; DELETE FROM meta WHERE key = 'thread_index';",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let refs: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM message_refs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(refs, 1);
        let root = message(
            "root@x",
            None,
            &[],
            None,
            "Sent",
            "me@x",
            "root body",
            1_700_000_000,
            None,
        );
        run(&store, &Op::Batch(vec![root]));
        let threads = threads_of(&store);
        assert_eq!(threads[0].1, threads[1].1, "{threads:?}");
        drop(store);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn deleting_an_aliased_message_rethreads_replies_to_the_alias() {
        let start = 1_700_000_000;
        let root = message(
            "root@x",
            None,
            &[],
            None,
            "INBOX",
            "a@x",
            "root body",
            start,
            None,
        );
        let sent = message(
            "sent@x",
            None,
            &[],
            None,
            "Sent",
            "me@x",
            "same post",
            start + 10,
            None,
        );
        let mut echo = message(
            "echo@list",
            None,
            &[],
            None,
            "INBOX",
            "me@x",
            "same post",
            start + 70,
            Some("list.x"),
        );
        echo.headers.subject = "[list] subject sent@x".into();
        let reply = message(
            "reply@x",
            Some("echo@list"),
            &["root@x".to_string(), "echo@list".to_string()],
            None,
            "INBOX",
            "b@x",
            "reply body",
            start + 200,
            None,
        );
        let other = message(
            "other@x",
            None,
            &[],
            None,
            "INBOX",
            "c@x",
            "other",
            start,
            None,
        );
        let incremental = Store::open_in_memory().unwrap();
        let full = Store::open_in_memory().unwrap();
        full.full_rethread.set(true);
        for store in [&incremental, &full] {
            store.upsert_account(&account()).unwrap();
            run(store, &Op::Batch(vec![root.clone(), sent.clone()]));
            run(store, &Op::Batch(vec![echo.clone()]));
            let aliased: i64 = store
                .conn
                .query_row("SELECT COUNT(*) FROM message_aliases", [], |row| row.get(0))
                .unwrap();
            assert_eq!(aliased, 1, "the echo collapsed into an alias");
            run(store, &Op::Batch(vec![reply.clone()]));
            run(store, &Op::Delete("sent@x".into()));
            run(store, &Op::Batch(vec![other.clone()]));
        }
        assert_eq!(threads_of(&incremental), threads_of(&full));
        assert_eq!(
            thread_of(&incremental, "reply@x"),
            thread_of(&incremental, "root@x")
        );
    }

    fn thread_of(store: &Store, id: &str) -> String {
        threads_of(store)
            .into_iter()
            .find(|(message, _)| message == id)
            .map(|(_, thread)| thread)
            .unwrap()
    }

    #[test]
    fn a_current_mailbox_reopens_without_rebuilding_triggers() {
        let path = std::env::temp_dir().join(format!(
            "fell-thread-triggers-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let trigger_sql = |store: &Store| -> String {
            store
                .conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = 'thread_dirty_insert'",
                    [],
                    |row| row.get(0),
                )
                .unwrap()
        };
        let first = Store::open(&path).unwrap();
        // Mark the trigger. A rebuild on open would replace it.
        first
            .conn
            .execute_batch(
                "DROP TRIGGER thread_dirty_insert;
                 CREATE TRIGGER thread_dirty_insert AFTER INSERT ON messages BEGIN
                     -- marker
                     INSERT OR IGNORE INTO thread_dirty_rows (rowid) VALUES (new.rowid);
                 END;",
            )
            .unwrap();
        // A second connection opens while the first is still live, as the
        // window, sync, and fell-mcp do.
        let second = Store::open(&path).unwrap();
        assert!(
            trigger_sql(&second).contains("marker"),
            "no trigger rebuild on a current mailbox"
        );
        drop(second);
        drop(first);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    fn mail(id: &str, subject: &str, from: &str, to: &str, date: i64) -> Message {
        let mut sent = message(
            id,
            None,
            &[],
            None,
            "INBOX",
            from,
            &format!("body {id}"),
            date,
            None,
        );
        sent.headers.subject = subject.into();
        sent.to = vec![Address {
            name: None,
            email: to.into(),
        }];
        sent
    }

    const DAY: i64 = 24 * 60 * 60;

    #[test]
    fn a_reply_without_headers_joins_its_original_by_subject() {
        for reply_first in [false, true] {
            let store = Store::open_in_memory().unwrap();
            store.upsert_account(&account()).unwrap();
            let original = mail("q3@x", "Q3 numbers", "me@x", "ada@x", 1_700_000_000);
            let reply = mail(
                "re@x",
                "RE: Q3 numbers",
                "ada@x",
                "me@x",
                1_700_000_000 + DAY,
            );
            let batches = if reply_first {
                [vec![reply], vec![original]]
            } else {
                [vec![original], vec![reply]]
            };
            for batch in batches {
                run(&store, &Op::Batch(batch));
            }
            assert_eq!(
                thread_of(&store, "q3@x"),
                thread_of(&store, "re@x"),
                "reply first: {reply_first}"
            );
        }
    }

    #[test]
    fn a_subject_alone_does_not_join_strangers_old_mail_or_stock_subjects() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let start = 1_700_000_000;
        run(
            &store,
            &Op::Batch(vec![
                mail("orig@x", "Q3 numbers", "me@x", "ada@x", start),
                // Someone the original never involved.
                mail(
                    "stranger@x",
                    "Re: Q3 numbers",
                    "bob@x",
                    "carol@x",
                    start + DAY,
                ),
                // The right people, but long after the window.
                mail(
                    "late@x",
                    "Re: Q3 numbers",
                    "ada@x",
                    "me@x",
                    start + 60 * DAY,
                ),
                // Not a reply.
                mail("same@x", "Q3 numbers", "ada@x", "me@x", start + 2 * DAY),
                mail("hi@x", "Hi", "me@x", "ada@x", start),
                mail("rehi@x", "Re: Hi", "ada@x", "me@x", start + DAY),
            ]),
        );
        let orig = thread_of(&store, "orig@x");
        for id in ["stranger@x", "late@x", "same@x"] {
            assert_ne!(thread_of(&store, id), orig, "{id}");
        }
        assert_ne!(thread_of(&store, "hi@x"), thread_of(&store, "rehi@x"));
    }

    #[test]
    fn headers_and_proton_ids_win_over_a_subject() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let start = 1_700_000_000;
        let mut proton = mail("p@x", "Re: Q3 numbers", "ada@x", "me@x", start + DAY);
        proton.headers.proton_conversation_id = Some("conv-9".into());
        let mut headed = mail("h@x", "Re: Q3 numbers", "ada@x", "me@x", start + DAY);
        headed.headers.in_reply_to = Some(MessageId::new("other@x"));
        run(
            &store,
            &Op::Batch(vec![
                mail("q3@x", "Q3 numbers", "me@x", "ada@x", start),
                mail("other@x", "Budget", "me@x", "ada@x", start),
                proton,
                headed,
            ]),
        );
        assert_ne!(thread_of(&store, "p@x"), thread_of(&store, "q3@x"));
        assert_eq!(thread_of(&store, "h@x"), thread_of(&store, "other@x"));
        assert_ne!(thread_of(&store, "h@x"), thread_of(&store, "q3@x"));
    }

    #[test]
    fn deleting_a_replys_match_lets_it_join_the_next_one() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let start = 1_700_000_000;
        run(
            &store,
            &Op::Batch(vec![
                mail("first@x", "Q3 numbers", "me@x", "ada@x", start),
                mail("second@x", "Q3 numbers", "me@x", "ada@x", start + DAY),
            ]),
        );
        // Lands on the later of the two originals.
        run(
            &store,
            &Op::Batch(vec![mail(
                "re@x",
                "Re: Q3 numbers",
                "ada@x",
                "me@x",
                start + 2 * DAY,
            )]),
        );
        assert_eq!(thread_of(&store, "re@x"), thread_of(&store, "second@x"));
        run(&store, &Op::Delete("second@x".into()));
        // The next sync batch re-checks replies that matched the deleted one.
        run(
            &store,
            &Op::Batch(vec![mail("other@x", "Budget", "bob@x", "me@x", start)]),
        );
        assert_eq!(thread_of(&store, "re@x"), thread_of(&store, "first@x"));
    }

    #[test]
    fn a_resynced_alias_takes_replies_that_cite_the_alias_along() {
        let start = 1_700_000_000;
        let parent = message(
            "parent@x",
            None,
            &[],
            None,
            "INBOX",
            "p@x",
            "parent",
            start,
            None,
        );
        // The list copy arrives first and keeps the row; the original's id
        // becomes its alias.
        let mut echo = message(
            "echo@list",
            None,
            &[],
            None,
            "INBOX",
            "me@x",
            "same post",
            start + 70,
            Some("list.x"),
        );
        echo.headers.subject = "[list] subject orig@x".into();
        let orig = message(
            "orig@x",
            None,
            &[],
            None,
            "Sent",
            "me@x",
            "same post",
            start + 10,
            None,
        );
        let reply = message(
            "reply@x",
            None,
            &["orig@x".to_string()],
            None,
            "INBOX",
            "b@x",
            "reply",
            start + 200,
            None,
        );
        // The original again, now naming its parent: its row moves threads.
        let resync = message(
            "orig@x",
            Some("parent@x"),
            &["parent@x".to_string()],
            None,
            "Sent",
            "me@x",
            "same post",
            start + 10,
            None,
        );
        let incremental = Store::open_in_memory().unwrap();
        let full = Store::open_in_memory().unwrap();
        full.full_rethread.set(true);
        for store in [&incremental, &full] {
            store.upsert_account(&account()).unwrap();
            run(store, &Op::Batch(vec![parent.clone(), echo.clone()]));
            run(store, &Op::Batch(vec![orig.clone()]));
            run(store, &Op::Batch(vec![reply.clone()]));
            run(store, &Op::Batch(vec![resync.clone()]));
        }
        assert_eq!(threads_of(&incremental), threads_of(&full));
    }

    #[test]
    fn a_real_parent_arriving_later_does_not_drag_the_subject_original() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let start = 1_700_000_000;
        let original = mail("q3@x", "Q3 numbers", "me@x", "ada@x", start);
        // Names a parent that has not synced yet.
        let mut reply = mail("re@x", "Re: Q3 numbers", "ada@x", "me@x", start + DAY);
        reply.headers.in_reply_to = Some(MessageId::new("real@x"));
        let real = mail("real@x", "Budget draft", "me@x", "ada@x", start + 10);
        run(&store, &Op::Batch(vec![original]));
        run(&store, &Op::Batch(vec![reply]));
        run(&store, &Op::Batch(vec![real]));
        assert_eq!(thread_of(&store, "re@x"), thread_of(&store, "real@x"));
        assert_ne!(thread_of(&store, "q3@x"), thread_of(&store, "real@x"));
    }

    #[test]
    fn one_sender_writing_to_two_people_stays_two_threads() {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        let start = 1_700_000_000;
        run(
            &store,
            &Op::Batch(vec![
                mail("ada@x", "Lunch on Friday", "me@x", "ada@ex", start),
                mail(
                    "bob@x",
                    "Re: Lunch on Friday",
                    "me@x",
                    "bob@ex",
                    start + DAY,
                ),
                // A second note to the same person is one conversation.
                mail(
                    "ada2@x",
                    "Re: Lunch on Friday",
                    "me@x",
                    "ada@ex",
                    start + 2 * DAY,
                ),
            ]),
        );
        assert_ne!(thread_of(&store, "ada@x"), thread_of(&store, "bob@x"));
        assert_eq!(thread_of(&store, "ada@x"), thread_of(&store, "ada2@x"));
    }
}
