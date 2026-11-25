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

use super::{Store, immediate_transaction, load_aliases, parse_references};
use crate::Result;
use crate::account::AccountId;
use crate::message::{Address, MessageHeaders, MessageId, decode_addresses};
use crate::threading::{
    SUBJECT_WINDOW_SECS, ThreadId, header_parent_thread_id, resolve_thread_id, subject_is_generic,
    thread_subject,
};

mod manual;
pub use manual::MergeCandidate;
pub(super) use manual::retarget_override;
pub(crate) use manual::{ACTION_SCHEMA, undo};

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

-- No message foreign key: a decision must survive a row disappearing
-- and the same Message-ID coming back from sync.
CREATE TABLE IF NOT EXISTS thread_overrides (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_thread_overrides_thread
    ON thread_overrides(account_id, thread_id);

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
fn subject_parent(conn: &Connection, account: &str, row: &ThreadingRow) -> Result<Option<Parent>> {
    let (key, reply) = thread_subject(&row.headers.subject);
    if !reply || subject_is_generic(&key) {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(
        "SELECT m.message_id, m.thread_id, m.from_email, m.to_line, m.cc_line
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
        let from = candidate.get::<_, String>(2)?.to_ascii_lowercase();
        let to: String = candidate.get(3)?;
        let cc: String = candidate.get(4)?;
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
        return Ok(Some(read_parent(candidate)?));
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

    let manual = ManualThreads::load(conn, account)?;
    let mut rows = manual.apply(conn, account, affected_rows(conn, account, &affected)?)?;
    let descendants = manual_descendants(conn, account, &manual, &rows)?;
    if !descendants.is_empty() {
        let added: BTreeSet<_> = descendants.difference(&affected).copied().collect();
        rows.extend(manual.apply(conn, account, affected_rows(conn, account, &added)?)?);
        rows.sort_unstable_by_key(|row| row.rowid);
    }
    manual.move_header_parents(conn, account, &mut rows)?;

    let mut parents: HashMap<MessageId, Option<ThreadId>> = HashMap::new();
    let mut lookup = |parent: &MessageId| -> Result<Option<ThreadId>> {
        if let Some(found) = parents.get(parent) {
            return Ok(found.clone());
        }
        let found = stored_parent(conn, account, parent)?.map(|parent| parent.thread);
        parents.insert(parent.clone(), found.clone());
        Ok(found)
    };
    let edges = thread_edges(&rows, &manual, &mut lookup, &mut |row| {
        Ok(subject_parent(conn, account, row)?.map(|parent| parent.thread))
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
        union.union_prefer(member, preferred, &manual);
    }
    let nodes: Vec<String> = edge_nodes(&edges).map(str::to_string).collect();
    relabel(
        conn,
        account_id,
        &mut union,
        nodes.iter().map(String::as_str),
    )
}

fn affected_rows(
    conn: &Connection,
    account: &str,
    affected: &BTreeSet<i64>,
) -> Result<Vec<ThreadingRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {THREADING_COLS} FROM messages WHERE rowid = ?1 AND account_id = ?2"
    ))?;
    let mut rows = Vec::with_capacity(affected.len());
    for rowid in affected {
        if let Some(row) = stmt
            .query_row(params![rowid, account], threading_row)
            .optional()?
        {
            rows.push(row);
        }
    }
    Ok(rows)
}

/// Only manually routed rows can carry replies beyond the ordinary dirty
/// set. Walk their indexed references and subject windows in one query.
/// UNION visits each row at most once even for cyclical headers.
fn manual_descendants(
    conn: &Connection,
    account: &str,
    manual: &ManualThreads,
    rows: &[ThreadingRow],
) -> Result<BTreeSet<i64>> {
    let mut seeds = rows
        .iter()
        .filter(|row| manual.pinned(row.thread_id.as_str()))
        .peekable();
    if seeds.peek().is_none() {
        return Ok(BTreeSet::new());
    }
    let seeds: Vec<_> = seeds.map(|row| row.rowid).collect();
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE reachable(rowid) AS (
             SELECT value FROM json_each(?2)
             UNION
             SELECT r.child_rowid FROM reachable d
             JOIN messages m ON m.rowid = d.rowid
             JOIN message_refs r ON r.account_id = ?1 AND r.ref_id = m.message_id
             UNION
             SELECT r.child_rowid FROM reachable d
             JOIN messages m ON m.rowid = d.rowid
             JOIN message_aliases a ON a.account_id = ?1 AND a.canonical_message_id = m.message_id
             JOIN message_refs r ON r.account_id = ?1 AND r.ref_id = a.message_id
             UNION
             SELECT later.message_rowid FROM reachable d
             JOIN subject_index s ON s.message_rowid = d.rowid
             JOIN subject_index later ON later.account_id = s.account_id
               AND later.subject_key = s.subject_key
               AND later.date_unix BETWEEN s.date_unix AND s.date_unix + ?3
             JOIN messages m ON m.rowid = later.message_rowid
             WHERE (m.in_reply_to IS NULL OR m.in_reply_to = '') AND m.references_header = ''
               AND (m.proton_conversation_id IS NULL OR trim(m.proton_conversation_id) = '')
         )
         SELECT rowid FROM reachable",
    )?;
    Ok(stmt
        .query_map(
            params![
                account,
                serde_json::json!(seeds).to_string(),
                SUBJECT_WINDOW_SECS
            ],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<_>>()?)
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

struct Parent {
    id: MessageId,
    thread: ThreadId,
}

fn read_parent(row: &rusqlite::Row<'_>) -> rusqlite::Result<Parent> {
    Ok(Parent {
        id: MessageId(row.get(0)?),
        thread: ThreadId(row.get(1)?),
    })
}

/// An alias whose canonical row is stored wins over a row with that id.
fn stored_parent(conn: &Connection, account: &str, id: &MessageId) -> Result<Option<Parent>> {
    let via_alias = conn
        .prepare_cached(
            "SELECT m.message_id, m.thread_id FROM message_aliases a
             JOIN messages m
               ON m.account_id = a.account_id AND m.message_id = a.canonical_message_id
             WHERE a.account_id = ?1 AND a.message_id = ?2",
        )?
        .query_row(params![account, id.as_str()], read_parent)
        .optional()?;
    if via_alias.is_some() {
        return Ok(via_alias);
    }
    Ok(conn
        .prepare_cached(
            "SELECT message_id, thread_id FROM messages WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_row(params![account, id.as_str()], read_parent)
        .optional()?)
}

fn header_parent(conn: &Connection, account: &str, row: &ThreadingRow) -> Result<Option<Parent>> {
    for id in row.headers.parent_ids() {
        if let Some(parent) = stored_parent(conn, account, id)? {
            return Ok(Some(parent));
        }
    }
    Ok(None)
}

fn direct_parent(conn: &Connection, account: &str, row: &ThreadingRow) -> Result<Option<Parent>> {
    if let Some(parent) = header_parent(conn, account, row)? {
        return Ok(Some(parent));
    }
    if subject_eligible(&row.headers) {
        return subject_parent(conn, account, row);
    }
    Ok(None)
}

fn has_proton(headers: &MessageHeaders) -> bool {
    headers
        .proton_conversation_id
        .as_deref()
        .is_some_and(|id| !id.trim().is_empty())
}

fn subject_eligible(headers: &MessageHeaders) -> bool {
    headers.parent_ids().next().is_none() && !has_proton(headers)
}

impl Store {
    /// Recompute which messages have a same-thread subject parent from the
    /// stored headers and participants, using the resolver's own predicate.
    pub fn thread_subject_joins(
        &self,
        account: &AccountId,
        thread: &ThreadId,
    ) -> Result<HashSet<MessageId>> {
        let mut joined = HashSet::new();
        for row in rows_in_thread(&self.conn, account, thread)? {
            if subject_eligible(&row.headers)
                && subject_parent(&self.conn, account.as_str(), &row)?
                    .is_some_and(|parent| parent.thread == *thread)
            {
                joined.insert(row.headers.message_id);
            }
        }
        Ok(joined)
    }
}

/// Assign before inserting or updating so no sibling in the batch sees an
/// overridden message temporarily wearing its automatic thread id.
pub(super) fn initial_thread(
    conn: &Connection,
    account: &AccountId,
    headers: &MessageHeaders,
) -> Result<ThreadId> {
    if let Some(thread) = stored_override(conn, account.as_str(), &headers.message_id)? {
        return Ok(thread);
    }
    resolve_thread_id(headers, |parent| {
        super::thread_id_for_message_id(conn, account, parent)
    })
}

fn stored_override(conn: &Connection, account: &str, id: &MessageId) -> Result<Option<ThreadId>> {
    Ok(conn
        .prepare_cached(
            "SELECT thread_id FROM (
             SELECT thread_id, 0 AS priority, message_id FROM thread_overrides
             WHERE account_id = ?1 AND message_id = ?2
             UNION ALL
             SELECT o.thread_id, 1, a.message_id FROM message_aliases a JOIN thread_overrides o
               ON o.account_id = a.account_id AND o.message_id = a.message_id
             WHERE a.account_id = ?1 AND a.canonical_message_id = ?2
         ) ORDER BY priority, message_id LIMIT 1",
        )?
        .query_row(params![account, id.as_str()], |row| {
            row.get::<_, String>(0).map(ThreadId)
        })
        .optional()?)
}

#[derive(Default)]
struct ManualThreads {
    overrides: HashMap<MessageId, ThreadId>,
    pinned: HashSet<String>,
}

impl ManualThreads {
    fn load(conn: &Connection, account: &str) -> Result<Self> {
        let mut stmt = conn.prepare_cached(
            "SELECT message_id, thread_id FROM thread_overrides WHERE account_id = ?1",
        )?;
        let mut rows = stmt.query([account])?;
        let mut manual = Self::default();
        while let Some(row) = rows.next()? {
            let thread = ThreadId(row.get(1)?);
            manual.pinned.insert(thread.0.clone());
            manual.overrides.insert(MessageId(row.get(0)?), thread);
        }
        if !manual.overrides.is_empty() {
            let mut stmt = conn.prepare_cached(
                "SELECT a.canonical_message_id, o.thread_id
                 FROM thread_overrides o JOIN message_aliases a
                   ON a.account_id = o.account_id AND a.message_id = o.message_id
                 WHERE o.account_id = ?1 ORDER BY a.message_id",
            )?;
            let mut rows = stmt.query([account])?;
            while let Some(row) = rows.next()? {
                manual
                    .overrides
                    .entry(MessageId(row.get(0)?))
                    .or_insert(ThreadId(row.get(1)?));
            }
        }
        Ok(manual)
    }

    fn apply(
        &self,
        conn: &Connection,
        account: &str,
        mut rows: Vec<ThreadingRow>,
    ) -> Result<Vec<ThreadingRow>> {
        if self.overrides.is_empty() {
            return Ok(rows);
        }
        let mut resolved = HashMap::new();
        let mut read = conn.prepare_cached(&format!(
            "SELECT {THREADING_COLS} FROM messages WHERE account_id = ?1 AND message_id = ?2"
        ))?;
        for row in &mut rows {
            let destination = self.destination(conn, account, row, &mut read, &mut resolved)?;
            if let Some(thread) = destination
                && row.thread_id != thread
            {
                conn.prepare_cached(
                    "UPDATE messages SET thread_id = ?1 WHERE account_id = ?2 AND rowid = ?3",
                )?
                .execute(params![thread.as_str(), account, row.rowid])?;
                row.thread_id = thread;
            }
        }
        Ok(rows)
    }

    /// Follow real message identities to the first explicitly placed ancestor.
    /// A new reply can follow a manual group even when Proton still reports
    /// its old id. Moving a reply never takes its previous group along.
    fn destination(
        &self,
        conn: &Connection,
        account: &str,
        first: &ThreadingRow,
        read: &mut rusqlite::Statement<'_>,
        resolved: &mut HashMap<MessageId, Option<ThreadId>>,
    ) -> Result<Option<ThreadId>> {
        let mut ancestors = Vec::new();
        let mut next = None;
        let destination = loop {
            let row = next.as_ref().unwrap_or(first);
            let id = &row.headers.message_id;
            if let Some(thread) = self.overrides.get(id) {
                break Some(thread.clone());
            }
            if let Some(known) = resolved.get(id) {
                break known.clone();
            }
            // Mark the row as visited before walking on. A cycle with no
            // explicit ancestor is resolved by the ordinary union pass.
            resolved.insert(id.clone(), None);
            ancestors.push(id.clone());
            let Some(parent) = direct_parent(conn, account, row)? else {
                break None;
            };
            next = read
                .query_row(params![account, parent.id.as_str()], threading_row)
                .optional()?;
            if next.is_none() {
                break None;
            }
        };
        for id in ancestors {
            resolved.insert(id, destination.clone());
        }
        Ok(destination)
    }

    fn pinned(&self, thread: &str) -> bool {
        self.pinned.contains(thread)
    }

    /// An explicitly placed child can claim a late header parent. Move the
    /// canonical message alone, leaving its previous thread's other rows.
    fn move_header_parents(
        &self,
        conn: &Connection,
        account: &str,
        rows: &mut [ThreadingRow],
    ) -> Result<()> {
        let mut moved = HashMap::new();
        for row in rows
            .iter()
            .filter(|row| self.overrides.contains_key(&row.headers.message_id))
        {
            if let Some(parent) = header_parent(conn, account, row)?
                && !self.pinned(parent.thread.as_str())
            {
                set_member_thread(conn, account, &parent.id, &row.thread_id)?;
                moved.insert(parent.id, row.thread_id.clone());
            }
        }
        if !moved.is_empty() {
            for row in rows {
                if let Some(thread) = moved.remove(&row.headers.message_id) {
                    row.thread_id = thread;
                }
            }
        }
        Ok(())
    }
}

fn set_member_thread(
    conn: &Connection,
    account: &str,
    id: &MessageId,
    thread: &ThreadId,
) -> Result<()> {
    conn.prepare_cached(
        "UPDATE messages SET thread_id = ?1 WHERE account_id = ?2 AND message_id = ?3 AND thread_id != ?1",
    )?
    .execute(params![thread.as_str(), account, id.as_str()])?;
    Ok(())
}

/// Re-resolve thread ids for an account in one in-memory pass.
///
/// A proton id absorbs a non-proton header thread. Two different proton ids
/// stay apart. Non-proton merges keep the earliest already-assigned id.
/// Reads every row in the account, so only upgrades call it; writes use
/// [`rethread_dirty`].
pub(super) fn rethread_all(conn: &Connection, account_id: &AccountId) -> Result<()> {
    let account = account_id.as_str();
    let manual = ManualThreads::load(conn, account)?;
    let mut rows = manual.apply(conn, account, load_threading_rows(conn, account_id)?)?;
    if rows.is_empty() {
        return Ok(());
    }
    manual.move_header_parents(conn, account, &mut rows)?;

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
    let edges = thread_edges(
        &rows,
        &manual,
        &mut |parent| Ok(by_message.get(parent).cloned()),
        &mut |row| Ok(subject_parent(conn, account, row)?.map(|parent| parent.thread)),
    )?;
    for (member, preferred) in &edges {
        union.union_prefer(member, preferred, &manual);
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
    manual: &ManualThreads,
    lookup: &mut dyn FnMut(&MessageId) -> Result<Option<ThreadId>>,
    by_subject: &mut dyn FnMut(&ThreadingRow) -> Result<Option<ThreadId>>,
) -> Result<Vec<(String, String)>> {
    let mut edges = Vec::with_capacity(rows.len() * 2);
    for row in rows {
        if manual.pinned(row.thread_id.as_str()) {
            continue;
        }
        let header = header_parent_thread_id(&row.headers, &mut *lookup)?;
        let onto = resolve_thread_id(&row.headers, &mut *lookup)?;
        if manual.pinned(onto.as_str()) {
            continue;
        }
        edges.push((row.thread_id.0.clone(), onto.0.clone()));
        if let Some(header) = header.filter(|parent| !manual.pinned(parent.as_str())) {
            edges.push((header.0, onto.0));
        } else if subject_eligible(&row.headers)
            && let Some(original) =
                by_subject(row)?.filter(|parent| !manual.pinned(parent.as_str()))
        {
            edges.push((row.thread_id.0.clone(), original.0));
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

fn rows_in_thread(
    conn: &Connection,
    account: &AccountId,
    thread: &ThreadId,
) -> Result<Vec<ThreadingRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {THREADING_COLS} FROM messages WHERE account_id = ?1 AND thread_id = ?2
         ORDER BY date_unix, rowid"
    ))?;
    Ok(stmt
        .query_map(params![account.as_str(), thread.as_str()], threading_row)?
        .collect::<rusqlite::Result<_>>()?)
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
    fn union_prefer(&mut self, member: &str, preferred: &str, manual: &ManualThreads) {
        let member_root = self.find(member);
        let preferred_root = self.find(preferred);
        if member_root == preferred_root {
            return;
        }
        if manual.pinned(&member_root) || manual.pinned(&preferred_root) {
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

    #[derive(Debug)]
    enum Op {
        Batch(Vec<Message>),
        Delete(String),
        /// Move this message's thread to Trash from its first folder; from
        /// Trash, delete it for good.
        Trash(String),
        Split(String),
        Merge([String; 3]),
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
            if rng.chance(45) {
                ops.push(Op::Split(made[rng.below(made.len())].clone()));
            }
            if rng.chance(45) {
                ops.push(Op::Merge(std::array::from_fn(|_| {
                    made[rng.below(made.len())].clone()
                })));
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
            Op::Split(id) => {
                let id = MessageId::new(id);
                let _ = store.split_thread(&account.id, &id);
            }
            Op::Merge(ids) => {
                let mut threads = ids.iter().filter_map(|id| {
                    store
                        .thread_for_message(&account.id, &MessageId::new(id))
                        .unwrap()
                });
                if let Some(target) = threads.next() {
                    let _ = store.merge_threads(&account.id, &threads.collect::<Vec<_>>(), &target);
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
                let left = threads_of(&incremental);
                let right = threads_of(&full);
                let differences: Vec<_> = left.iter().zip(&right).filter(|(a, b)| a != b).collect();
                let operation = match op {
                    Op::Batch(rows) => format!("batch of {}", rows.len()),
                    other => format!("{other:?}"),
                };
                assert!(
                    left == right,
                    "seed {seed}, step {step} ({operation}): {differences:?}"
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
    fn merge_candidates_choose_topic_and_people_before_newer_mail() {
        let store = Store::open_in_memory().unwrap();
        let account = account();
        store.upsert_account(&account).unwrap();
        let mail = |id: &str, author, subject: &str, date| {
            let mut mail = message(id, None, &[], None, "INBOX", author, id, date, None);
            mail.headers.subject = subject.into();
            store.upsert_message(&mail).unwrap()
        };
        let source = mail("source@x", "a@x", "Re: Warehouse access checklist", 100);
        let topic = mail("topic@x", "b@x", "Warehouse access checklist", 200);
        let person = mail("person@x", "a@x", "Drawings for the workshop", 300);
        let latest = mail("latest@x", "c@x", "Items on sale today", 400);
        let candidates = store
            .merge_candidates(
                &account.id,
                std::slice::from_ref(&source.0),
                "",
                &account.email,
            )
            .unwrap();
        assert_eq!(
            candidates
                .into_iter()
                .map(|row| row.thread)
                .collect::<Vec<_>>(),
            [topic.clone(), person.clone(), latest]
        );
        for index in 0..30 {
            mail(
                &format!("recent-{index}@x"),
                "c@x",
                "Newer mail",
                500 + index,
            );
        }
        let recent = store
            .merge_candidates(&account.id, &[source.0], "", &account.email)
            .unwrap();
        assert_eq!(recent.len(), 32);
        assert_eq!(recent[0].thread, person);
        assert!(recent.iter().all(|row| row.thread != topic));
    }

    #[test]
    fn merge_search_prefers_subject_word_starts_then_names_then_addresses() {
        let store = Store::open_in_memory().unwrap();
        let acct = account();
        store.upsert_account(&acct).unwrap();
        let mail = |id, subject: &str, name: &str, email, date| {
            let mut mail = message(id, None, &[], None, "INBOX", email, id, date, None);
            mail.headers.subject = subject.into();
            mail.headers.from.name = Some(name.into());
            store.upsert_message(&mail).unwrap()
        };
        let subject = mail("s@x", "Checklist:Northpine", "Ada", "a@x", 100);
        let name = mail("n@x", "Delivery update", "Northpine", "b@x", 200);
        let address = mail("a@x", "Dispatch", "Ada", "northpine@x", 300);
        let partial = mail("p@x", "Oldnorthpine", "Ada", "a@x", 400);
        assert_eq!(
            store
                .merge_candidates(&acct.id, &[], "north", &acct.email)
                .unwrap()
                .into_iter()
                .map(|row| row.thread)
                .collect::<Vec<_>>(),
            [subject, name, address, partial],
        );
        assert!(
            store
                .merge_candidates(&acct.id, &[], "north*", &acct.email)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn late_header_parents_follow_both_sides_of_a_split() {
        for whole_account in [false, true] {
            let store = Store::open_in_memory().unwrap();
            store.full_rethread.set(whole_account);
            let account = account();
            store.upsert_account(&account).unwrap();
            let mail = |id, parent, proton, date| {
                message(id, parent, &[], proton, "INBOX", "a@x", id, date, None)
            };
            let retained = store
                .upsert_message(&mail(
                    "retained@x",
                    Some("parent-a@x"),
                    Some("old".into()),
                    200,
                ))
                .unwrap();
            store
                .upsert_message(&mail(
                    "detached@x",
                    Some("parent-b@x"),
                    Some("old".into()),
                    300,
                ))
                .unwrap();
            let detached = store
                .split_thread(&account.id, &MessageId::new("detached@x"))
                .unwrap();
            let parents = Op::Batch(vec![
                mail("parent-a@x", None, None, 100),
                mail("parent-b@x", None, Some("lookup".into()), 110),
            ]);
            run(&store, &parents);
            assert_eq!(
                store
                    .thread_for_message(&account.id, &MessageId::new("parent-a@x"))
                    .unwrap(),
                Some(retained)
            );
            assert_eq!(
                store
                    .thread_for_message(&account.id, &MessageId::new("parent-b@x"))
                    .unwrap(),
                Some(detached)
            );
        }
    }

    #[test]
    fn split_moves_only_the_header_parent_and_undo_resolves_its_proton_key() {
        for whole_account in [false, true] {
            let store = Store::open_in_memory().unwrap();
            store.full_rethread.set(whole_account);
            let acct = account();
            store.upsert_account(&acct).unwrap();
            let mail = |id, parent, proton: &str, date| {
                message(
                    id,
                    parent,
                    &[],
                    Some(proton.into()),
                    "INBOX",
                    "a@x",
                    id,
                    date,
                    None,
                )
            };
            run(
                &store,
                &Op::Batch(vec![
                    mail("s@x", None, "team", 110),
                    mail("r@x", None, "old", 200),
                    mail("c@x", Some("p@x"), "old", 210),
                ]),
            );
            let detached = store
                .split_thread(&acct.id, &MessageId::new("c@x"))
                .unwrap();
            store
                .upsert_message(&mail("p@x", None, "team", 100))
                .unwrap();
            let thread = |id| {
                store
                    .thread_for_message(&acct.id, &MessageId::new(id))
                    .unwrap()
                    .unwrap()
            };
            assert_eq!(thread("p@x"), detached);
            assert_eq!(thread("s@x"), ThreadId::proton("team"));
            assert_eq!(thread("r@x"), ThreadId::proton("old"));
            store.undo(&acct.id).unwrap();
            assert_eq!(thread("p@x"), ThreadId::proton("team"));
            assert_eq!(thread("s@x"), ThreadId::proton("team"));
            assert_eq!(thread("c@x"), ThreadId::proton("old"));
            assert_eq!(thread("r@x"), ThreadId::proton("old"));
        }
    }

    #[test]
    fn manual_edits_use_membership_after_queued_rethreading() {
        let store = Store::open_in_memory().unwrap();
        let acct = account();
        store.upsert_account(&acct).unwrap();
        let mail = |id, date| message(id, None, &[], None, "INBOX", "a@x", id, date, None);
        run(
            &store,
            &Op::Batch(vec![mail("a@x", 100), mail("b@x", 200), mail("c@x", 300)]),
        );
        let thread = |id| {
            store
                .thread_for_message(&acct.id, &MessageId::new(id))
                .unwrap()
                .unwrap()
        };
        store
            .conn
            .execute(
                "UPDATE messages SET in_reply_to = 'a@x' WHERE message_id = 'b@x'",
                [],
            )
            .unwrap();
        let detached = store
            .split_thread(&acct.id, &MessageId::new("b@x"))
            .unwrap();
        assert_eq!(thread("b@x"), detached);
        store.undo(&acct.id).unwrap();
        let stale_source = thread("c@x");
        let destination = store.upsert_message(&mail("target@x", 400)).unwrap();
        store
            .conn
            .execute(
                "UPDATE messages SET in_reply_to = 'a@x' WHERE message_id = 'c@x'",
                [],
            )
            .unwrap();
        assert_eq!(
            store
                .merge_threads(&acct.id, &[stale_source], &destination)
                .unwrap(),
            destination
        );
        for id in ["a@x", "b@x", "c@x"] {
            assert_eq!(thread(id), destination);
        }
        assert_eq!(
            store.thread_manual_merges(&acct.id, &destination).unwrap(),
            ["a@x", "b@x", "c@x"]
                .map(MessageId::new)
                .into_iter()
                .collect(),
        );
    }

    #[test]
    fn an_unrelated_subject_chain_does_not_expand_the_manual_dirty_set() {
        let store = Store::open_in_memory().unwrap();
        let account = account();
        store.upsert_account(&account).unwrap();
        let mail = |id: &str, date| {
            let mut mail = message(id, None, &[], None, "INBOX", "a@x", id, date, None);
            mail.headers.subject = "Re: Repeated billing alerts".into();
            mail
        };
        let rows: Vec<_> = (0..40)
            .map(|index| mail(&format!("alert-{index}@x"), index * 20 * 86_400))
            .collect();
        run(&store, &Op::Batch(rows));
        let source = store
            .upsert_message(&message(
                "source@x",
                None,
                &[],
                None,
                "INBOX",
                "b@x",
                "one",
                100,
                None,
            ))
            .unwrap();
        let target = store
            .upsert_message(&message(
                "target@x",
                None,
                &[],
                None,
                "INBOX",
                "b@x",
                "two",
                200,
                None,
            ))
            .unwrap();
        store
            .merge_threads(&account.id, &[source], &target)
            .unwrap();

        let sql =
            format!("SELECT {THREADING_COLS} FROM messages WHERE rowid = ?1 AND account_id = ?2");
        let row_read_steps = || {
            store
                .conn
                .prepare_cached(&sql)
                .unwrap()
                .reset_status(rusqlite::StatementStatus::VmStep)
        };
        row_read_steps();
        store.conn.execute(
            "INSERT INTO thread_dirty_rows SELECT rowid FROM messages WHERE message_id = 'alert-0@x'",
            [],
        ).unwrap();
        rethread_dirty(&store.conn, &account.id).unwrap();
        let steps = row_read_steps();
        assert!(
            (1..150).contains(&steps),
            "dirty row reads used {steps} SQLite instructions"
        );
    }

    #[test]
    fn splitting_a_subject_branch_survives_resync_and_whole_account_rethread() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("mail.db");
        let store = Store::open(&path).unwrap();
        let mailbox = account();
        let account = mailbox.id.clone();
        store.upsert_account(&mailbox).unwrap();
        let root = message(
            "root@x",
            None,
            &[],
            None,
            "INBOX",
            "ada@x",
            "root",
            100,
            None,
        );
        let mut reply = message(
            "reply@x",
            None,
            &[],
            None,
            "INBOX",
            "tessa@x",
            "reply",
            200,
            None,
        );
        let mut root = root;
        root.headers.subject = "Equipment inventory".into();
        root.to = vec![Address {
            name: None,
            email: "tessa@x".into(),
        }];
        reply.headers.subject = "Re: Equipment inventory".into();
        reply.to = vec![Address {
            name: None,
            email: "ada@x".into(),
        }];
        reply.labels.push("inventory".into());
        reply.starred = true;
        reply.snooze_until = Some(1_893_456_000);
        let child = message(
            "child@x",
            Some("reply@x"),
            &[],
            None,
            "Sent",
            "ada@x",
            "child",
            300,
            None,
        );
        let sibling = message(
            "sibling@x",
            Some("root@x"),
            &[],
            None,
            "INBOX",
            "ada@x",
            "sibling",
            350,
            None,
        );
        let copies = Op::Batch(vec![root, reply, child, sibling]);
        run(&store, &copies);
        let id = MessageId::new("reply@x");
        let original = store.thread_for_message(&account, &id).unwrap().unwrap();
        assert_eq!(
            store.thread_subject_joins(&account, &original).unwrap(),
            HashSet::from([id.clone()])
        );
        let detached = store.split_thread(&account, &id).unwrap();
        assert_eq!(store.thread_messages(&account, &original).unwrap().len(), 2);
        assert!(
            store
                .thread_subject_joins(&account, &detached)
                .unwrap()
                .is_empty()
        );

        drop(store);
        let store = Store::open(&path).unwrap();
        run(&store, &copies);
        let late = message(
            "late@x",
            Some("child@x"),
            &[],
            None,
            "INBOX",
            "ada@x",
            "late",
            400,
            None,
        );
        run(&store, &Op::Batch(vec![late]));
        rethread_all(&store.conn, &account).unwrap();
        let separated = store.thread_messages(&account, &detached).unwrap();
        assert_eq!(separated.len(), 3);
        let reply = &separated[0];
        assert_eq!(reply.headers.message_id, id);
        assert!(reply.starred);
        assert_eq!(reply.snooze_until, Some(1_893_456_000));
        assert_eq!(reply.labels, ["inventory"]);

        assert_eq!(
            store.undo(&account).unwrap().kind,
            crate::ActionKind::SplitThread
        );
        assert!(
            store
                .thread_messages(&account, &detached)
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.thread_messages(&account, &original).unwrap().len(), 5);
        assert_eq!(
            store.thread_subject_joins(&account, &original).unwrap(),
            HashSet::from([id])
        );
    }

    #[test]
    fn merging_proton_conversations_keeps_the_chosen_key_and_undo_routes_new_replies() {
        let store = Store::open_in_memory().unwrap();
        let mailbox = account();
        let account = mailbox.id.clone();
        store.upsert_account(&mailbox).unwrap();
        let a = message(
            "a@x",
            None,
            &[],
            Some("one".into()),
            "INBOX",
            "a@x",
            "first",
            100,
            None,
        );
        let b = message(
            "b@x",
            None,
            &[],
            Some("two".into()),
            "INBOX",
            "b@x",
            "second",
            200,
            None,
        );
        run(&store, &Op::Batch(vec![a.clone(), b.clone()]));
        let source = ThreadId::proton("one");
        let target = ThreadId::proton("two");
        store
            .merge_threads(&account, std::slice::from_ref(&source), &target)
            .unwrap();
        let reply = |id, parent, proton: &str, date| {
            message(
                id,
                Some(parent),
                &[],
                Some(proton.into()),
                "Sent",
                "a@x",
                id,
                date,
                None,
            )
        };
        run(
            &store,
            &Op::Batch(vec![
                a,
                b,
                reply("reply@x", "a@x", "two", 300),
                reply("later@x", "reply@x", "three", 310),
                reply("target-reply@x", "b@x", "three", 320),
            ]),
        );
        rethread_all(&store.conn, &account).unwrap();
        assert_eq!(store.thread_messages(&account, &target).unwrap().len(), 5);
        assert!(store.thread_messages(&account, &source).unwrap().is_empty());
        store.undo(&account).unwrap();
        assert_eq!(store.thread_messages(&account, &target).unwrap().len(), 2);
        assert_eq!(store.thread_messages(&account, &source).unwrap().len(), 3);
        assert!(
            store
                .thread_messages(&account, &ThreadId::proton("three"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn merging_notes_and_undo_restore_both_histories() {
        let store = Store::open_in_memory().unwrap();
        let mailbox = account();
        let account = mailbox.id.clone();
        store.upsert_account(&mailbox).unwrap();
        store
            .set_agent_grant(
                &account,
                "planner",
                crate::AgentGrant::enabled(true, false, false).with_annotation(true),
            )
            .unwrap();
        let source = store
            .upsert_message(&message(
                "source@x",
                None,
                &[],
                None,
                "INBOX",
                "a@x",
                "source",
                100,
                None,
            ))
            .unwrap();
        let target = store
            .upsert_message(&message(
                "target@x",
                None,
                &[],
                None,
                "INBOX",
                "b@x",
                "target",
                200,
                None,
            ))
            .unwrap();
        let original = store
            .annotate_thread(
                &account,
                &target,
                "planner",
                Some("Target note"),
                &[],
                "Review",
            )
            .unwrap();
        let newer = store
            .annotate_thread(
                &account,
                &source,
                "planner",
                Some("Source note"),
                &["reply".into()],
                "Review",
            )
            .unwrap();
        store.see_thread_note(&account, newer.id).unwrap();

        store
            .merge_threads(&account, std::slice::from_ref(&source), &target)
            .unwrap();
        assert!(store.thread_notes(&account, &source).unwrap().is_empty());
        let notes = store.thread_notes(&account, &target).unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].id, newer.id);
        assert!(notes[0].seen);
        assert_eq!(
            store.undo(&account).unwrap().kind,
            crate::ActionKind::MergeThreads
        );
        assert_eq!(
            store.thread_notes(&account, &target).unwrap()[0].id,
            original.id
        );
        assert_eq!(
            store.thread_notes(&account, &source).unwrap()[0].id,
            newer.id
        );
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
