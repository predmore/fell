//! The server copies behind each row, so sync can take off what vanished.
//!
//! One message can live in several mailboxes (INBOX and All Mail, a Sent
//! copy, a list echo merged by fingerprint). Each copy is one row here, keyed
//! by mailbox, UIDVALIDITY, and UID, and points at the stored message it
//! joined. A removal drops copies and queues the folder they backed in
//! `copy_orphans`. [`settle`] runs at the end of a sync pass: a folder comes
//! off a row only when no copy of that row is left in it, and a row is deleted
//! only when it has no folder and no copy left. A server move adds the new
//! copy during the same pass, so the row (and its local state) stays.
//!
//! A folder a write-back move not yet settled is bringing the row into
//! stays too (see [`crate::outbox`]). Undo puts the row back in its folder
//! at once and clears what kept sync off it, while the server still has
//! the copy where the action moved it until the reverse move is sent.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::account::AccountId;
use crate::backend::{FolderRemoval, RemovalKind, Settle};
use crate::message::{Message, MessageId};
use crate::search::{IndexedText, sync_terms, terms_for_indexed};

use super::{
    canonical_id_on, delete_stored, find_stored, join_tokens, split_field, take_local_state,
};

const COPIES_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS message_copies (
    account_id TEXT NOT NULL,
    mailbox TEXT NOT NULL,
    uidvalidity INTEGER NOT NULL,
    uid INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    folder TEXT NOT NULL,
    -- Big-endian u64, so SQLite orders the full unsigned MODSEQ range.
    modseq BLOB,
    PRIMARY KEY (account_id, mailbox, uidvalidity, uid)
);

CREATE INDEX IF NOT EXISTS idx_message_copies_row ON message_copies(account_id, message_id);

CREATE TABLE IF NOT EXISTS copy_orphans (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    folder TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id, folder)
);
";

/// Create the tables. The first time, record the one copy each row and alias
/// already names, so mail synced before this table existed can still go.
pub(super) fn ensure_copies(conn: &Connection) -> Result<()> {
    if has_modseq(conn)? {
        return conn.execute_batch(COPIES_SCHEMA).map_err(Into::into);
    }
    // One transaction: a large mailbox is not one commit per row, and a
    // crash before the end leaves no table, so the next open starts over.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let existed = table_exists(&tx, "message_copies")?;
    tx.execute_batch(COPIES_SCHEMA)?;
    if !has_modseq(&tx)? {
        tx.execute_batch("ALTER TABLE message_copies ADD COLUMN modseq BLOB")?;
    }
    if !existed {
        backfill(&tx)?;
    }
    tx.commit()?;
    Ok(())
}

fn has_modseq(conn: &Connection) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM pragma_table_info('message_copies') WHERE name = 'modseq')",
        [],
        |row| row.get(0),
    )?)
}

/// Record an observation from sync or STORE. An older or equal revision
/// cannot bring any new flag information from this copy.
pub(crate) fn observe_modseq(
    conn: &Connection,
    account_id: &AccountId,
    copy: &crate::imap::RemoteCopy,
    modseq: u64,
) -> Result<bool> {
    let updated = conn
        .prepare_cached(
            "UPDATE message_copies SET modseq = ?5
         WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4
           AND (modseq IS NULL OR modseq < ?5)",
        )?
        .execute(params![
            account_id.as_str(),
            copy.mailbox,
            copy.uidvalidity,
            copy.uid,
            modseq.to_be_bytes(),
        ])?;
    Ok(updated != 0 || row_of(conn, account_id, copy)?.is_none())
}

fn backfill(conn: &Connection) -> Result<()> {
    let mut known = Vec::new();
    for sql in [
        "SELECT account_id, message_id, remote_id FROM messages",
        "SELECT account_id, canonical_message_id, remote_id FROM message_aliases",
    ] {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            known.push(row?);
        }
    }
    for (account_id, message_id, remote_id) in known {
        let Some(copy) = crate::imap::parse_remote_id(&remote_id) else {
            continue;
        };
        conn.execute(
            "INSERT OR IGNORE INTO message_copies
                (account_id, mailbox, uidvalidity, uid, message_id, folder)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                account_id,
                copy.mailbox,
                copy.uidvalidity,
                copy.uid,
                message_id,
                copy.folder
            ],
        )?;
    }
    Ok(())
}

/// Note the server copy `message` arrived as, on the row it was written to.
pub(super) fn record(conn: &Connection, message: &Message) -> Result<()> {
    let Some(copy) = crate::imap::parse_remote_id(&message.remote_id.0) else {
        return Ok(());
    };
    let Some((stored, _)) = find_stored(conn, &message.account_id, &message.headers.message_id)?
    else {
        return Ok(());
    };
    conn.execute(
        "INSERT INTO message_copies
            (account_id, mailbox, uidvalidity, uid, message_id, folder)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account_id, mailbox, uidvalidity, uid) DO UPDATE SET
            message_id = excluded.message_id,
            folder = excluded.folder",
        params![
            message.account_id.as_str(),
            copy.mailbox,
            copy.uidvalidity,
            copy.uid,
            stored.canonical_id,
            copy.folder
        ],
    )?;
    Ok(())
}

/// UIDs from `low` to `high` that have a stored copy in `mailbox` (escaped
/// as remote ids spell it) under `uidvalidity`, ascending.
///
/// A copy counts while its flags have somewhere to land, on its row or an
/// undo snapshot, or when a purge tombstone suppresses the whole copy.
/// Any other copy is fetched again like new mail.
pub(super) fn stored_uids(
    conn: &Connection,
    account_id: &AccountId,
    mailbox: &str,
    uidvalidity: u32,
    (low, high): (u32, u32),
) -> Result<Vec<u32>> {
    let local = if table_exists(conn, "local_action_messages")? {
        "OR EXISTS (
             SELECT 1 FROM local_action_messages s
             JOIN local_actions a ON a.id = s.action_id
             WHERE a.account_id = c.account_id AND a.undone = 0
               AND s.message_id = c.message_id
         ) OR EXISTS (
             SELECT 1 FROM local_folder_tombstones t
             JOIN local_actions a ON a.id = t.action_id
             WHERE t.account_id = c.account_id AND t.message_id = c.message_id
               AND t.folder = c.folder COLLATE NOCASE AND a.kind = 'purge'
         )"
    } else {
        ""
    };
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT c.uid FROM message_copies c
         WHERE c.account_id = ?1 AND c.mailbox = ?2 AND c.uidvalidity = ?3
           AND c.uid BETWEEN ?4 AND ?5
           AND (
               EXISTS (
                   SELECT 1 FROM messages m
                   WHERE m.account_id = c.account_id AND m.message_id = c.message_id
               )
               {local}
           )
         ORDER BY c.uid"
    ))?;
    let uids = stmt
        .query_map(
            params![account_id.as_str(), mailbox, uidvalidity, low, high],
            |row| row.get::<_, u32>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(uids)
}

/// The row a recorded server copy backs, if the copy is recorded.
pub(super) fn row_of(
    conn: &Connection,
    account_id: &AccountId,
    copy: &crate::imap::RemoteCopy,
) -> Result<Option<String>> {
    conn.query_row(
        "SELECT message_id FROM message_copies
         WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
        params![
            account_id.as_str(),
            copy.mailbox,
            copy.uidvalidity,
            copy.uid
        ],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

/// A merge folded `from` into `to`: its copies now back `to`.
pub(super) fn repoint(
    conn: &Connection,
    account_id: &AccountId,
    from: &str,
    to: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE message_copies SET message_id = ?3 WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), from, to],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO copy_orphans (account_id, message_id, folder)
         SELECT account_id, ?3, folder FROM copy_orphans
         WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), from, to],
    )?;
    forget(conn, account_id, from)
}

/// Drop every copy and queued folder of a row that is gone.
pub(super) fn forget(conn: &Connection, account_id: &AccountId, message_id: &str) -> Result<()> {
    for sql in [
        "DELETE FROM message_copies WHERE account_id = ?1 AND message_id = ?2",
        "DELETE FROM copy_orphans WHERE account_id = ?1 AND message_id = ?2",
    ] {
        conn.execute(sql, params![account_id.as_str(), message_id])?;
    }
    Ok(())
}

/// Drop the copies each removal names and queue the folders they backed.
/// Folders and rows do not change until [`settle`]. Returns whether a row
/// took over another's local state (see [`take_over_reset_row`]).
pub(super) fn apply_removals(
    conn: &Connection,
    account_id: &AccountId,
    removals: &[FolderRemoval],
) -> Result<bool> {
    let mut changed = false;
    for removal in removals {
        match &removal.kind {
            RemovalKind::Listing {
                uidvalidity,
                present,
                through,
                found,
                sweep,
            } => {
                for (uid, message_id) in found {
                    record_found(conn, account_id, removal, *uidvalidity, *uid, message_id)?;
                }
                let listed = |uid: u32| uid > *through || present.binary_search(&uid).is_ok();
                let stored = {
                    let mut stmt = conn.prepare(
                        "SELECT uidvalidity, uid, message_id FROM message_copies
                         WHERE account_id = ?1 AND mailbox = ?2",
                    )?;
                    stmt.query_map(params![account_id.as_str(), removal.mailbox], |row| {
                        Ok((
                            row.get::<_, u32>(0)?,
                            row.get::<_, u32>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    })?
                    .collect::<std::result::Result<Vec<_>, _>>()?
                };
                // Rows with a copy here under the current UIDVALIDITY.
                let current: HashSet<&str> = stored
                    .iter()
                    .filter(|(validity, _, _)| validity == uidvalidity)
                    .map(|(_, _, id)| id.as_str())
                    .collect();
                let mut doomed = Vec::new();
                for (validity, uid, message_id) in &stored {
                    let gone = if validity == uidvalidity {
                        !listed(*uid)
                    } else if current.contains(message_id.as_str()) {
                        // The row has its copy under the new UIDVALIDITY.
                        true
                    } else if take_over_reset_row(
                        conn,
                        account_id,
                        removal,
                        *uidvalidity,
                        *validity,
                        *uid,
                        message_id,
                        found,
                    )? {
                        changed = true;
                        true
                    } else {
                        // Nothing names the row under the new UIDVALIDITY: the
                        // body fetch can skip mail (flagged \Deleted, too
                        // large), and mail with no Message-ID has a new id. Its
                        // UID is still taken, so keep the copy until it is not.
                        present.binary_search(uid).is_err()
                    };
                    if gone {
                        doomed.push((*validity, *uid));
                    }
                }
                for (validity, uid) in doomed {
                    drop_copy(conn, account_id, &removal.mailbox, validity, uid, uid)?;
                }
                if *sweep {
                    sweep_folder(conn, account_id, removal, *uidvalidity)?;
                }
            }
            RemovalKind::Vanished {
                uidvalidity,
                ranges,
            } => {
                for (low, high) in ranges {
                    drop_copy(
                        conn,
                        account_id,
                        &removal.mailbox,
                        *uidvalidity,
                        *low,
                        *high,
                    )?;
                }
            }
        }
    }
    Ok(changed)
}

/// After a UIDVALIDITY change, mail with no Message-ID is stored again under
/// a new id (it is made from the UIDVALIDITY). When `message_id` is such a
/// row for the old copy, find the new row the same mail made in this mailbox
/// (same sender, subject, date, and body) and give it the old row's labels,
/// star, archive bit, and snooze. Returns whether it did; the old row then
/// has no copy left and goes at [`settle`].
#[allow(clippy::too_many_arguments)]
fn take_over_reset_row(
    conn: &Connection,
    account_id: &AccountId,
    removal: &FolderRemoval,
    uidvalidity: u32,
    old_validity: u32,
    old_uid: u32,
    message_id: &str,
    found: &[(u32, MessageId)],
) -> Result<bool> {
    let synthetic =
        |validity: u32, uid: u32| format!("uid.{validity}.{uid}.{}@imap.fell", removal.mailbox);
    if message_id != synthetic(old_validity, old_uid) {
        return Ok(false);
    }
    let Some((old, _)) = find_stored(conn, account_id, &MessageId(message_id.into()))? else {
        return Ok(false);
    };
    let old_fingerprint = fingerprint(conn, account_id, &old.canonical_id)?;
    // The same UID first: a server that rebuilt the mailbox in order often
    // keeps it.
    let mut candidates: Vec<&(u32, MessageId)> = found
        .iter()
        .filter(|(uid, id)| id.as_str() == synthetic(uidvalidity, *uid))
        .collect();
    candidates.sort_by_key(|(uid, _)| (*uid != old_uid, *uid));
    for (_, id) in candidates {
        let Some((new, via_alias)) = find_stored(conn, account_id, id)? else {
            continue;
        };
        let same = !via_alias
            && new.canonical_id != old.canonical_id
            && new.from_email == old.from_email
            && new.subject == old.subject
            && new.date_unix == old.date_unix
            && fingerprint(conn, account_id, &new.canonical_id)? == old_fingerprint;
        if same {
            take_local_state(conn, account_id, &old, &new)?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn fingerprint(conn: &Connection, account_id: &AccountId, message_id: &str) -> Result<String> {
    Ok(conn.query_row(
        "SELECT body_fingerprint FROM messages WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), message_id],
        |row| row.get(0),
    )?)
}

fn record_found(
    conn: &Connection,
    account_id: &AccountId,
    removal: &FolderRemoval,
    uidvalidity: u32,
    uid: u32,
    message_id: &MessageId,
) -> Result<()> {
    // A copy sync never stored (oversized, flagged deleted, removed here) has
    // no row to back.
    let Some(row) = canonical_id_on(conn, account_id, message_id)? else {
        return Ok(());
    };
    conn.prepare_cached(
        "INSERT OR IGNORE INTO message_copies
            (account_id, mailbox, uidvalidity, uid, message_id, folder)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?
    .execute(params![
        account_id.as_str(),
        removal.mailbox,
        uidvalidity,
        uid,
        row,
        removal.folder
    ])?;
    Ok(())
}

fn drop_copy(
    conn: &Connection,
    account_id: &AccountId,
    mailbox: &str,
    uidvalidity: u32,
    low: u32,
    high: u32,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO copy_orphans (account_id, message_id, folder)
         SELECT account_id, message_id, folder FROM message_copies
         WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid BETWEEN ?4 AND ?5",
        params![account_id.as_str(), mailbox, uidvalidity, low, high],
    )?;
    conn.execute(
        "DELETE FROM message_copies
         WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid BETWEEN ?4 AND ?5",
        params![account_id.as_str(), mailbox, uidvalidity, low, high],
    )?;
    Ok(())
}

/// Rows in folder `?2` with no copy in mailbox `?3` under UIDVALIDITY `?4`.
pub(super) const SWEEP_STALE: &str = "SELECT m.message_id FROM messages m
     WHERE m.account_id = ?1
       AND instr(char(30) || m.folders || char(30), char(30) || ?2 || char(30)) > 0
       AND NOT EXISTS (
           SELECT 1 FROM message_copies c INDEXED BY idx_message_copies_row
           WHERE c.account_id = m.account_id AND c.message_id = m.message_id
             AND c.mailbox = ?3 AND c.uidvalidity = ?4
       )";

/// Rows that list this folder but have no copy in the mailbox. Only for a
/// listing that recorded every present copy. A row a local action touched
/// keeps its folders: that folder may be one the action added.
///
/// The copy lookup goes by row. Left to itself SQLite reads every copy in
/// the mailbox for each row, and a 50,000-message mailbox held the write
/// lock for minutes.
fn sweep_folder(
    conn: &Connection,
    account_id: &AccountId,
    removal: &FolderRemoval,
    uidvalidity: u32,
) -> Result<()> {
    let mut stmt = conn.prepare(SWEEP_STALE)?;
    let stale = stmt
        .query_map(
            params![
                account_id.as_str(),
                removal.folder,
                removal.mailbox,
                uidvalidity
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let local = LocalTouch::new(conn)?;
    for message_id in stale {
        if local.touched(conn, account_id, &message_id)? {
            continue;
        }
        conn.execute(
            "INSERT OR IGNORE INTO copy_orphans (account_id, message_id, folder)
             VALUES (?1, ?2, ?3)",
            params![account_id.as_str(), message_id, removal.folder],
        )?;
    }
    Ok(())
}

/// Local archive, move, delete, and label state lives in tables that exist
/// only once an action has run.
struct LocalTouch {
    tombstones: bool,
    actions: bool,
}

impl LocalTouch {
    fn new(conn: &Connection) -> Result<Self> {
        Ok(Self {
            tombstones: table_exists(conn, "local_folder_tombstones")?,
            actions: table_exists(conn, "local_action_messages")?
                && table_exists(conn, "local_actions")?,
        })
    }

    fn touched(&self, conn: &Connection, account_id: &AccountId, message_id: &str) -> Result<bool> {
        if self.tombstones {
            let hit: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM local_folder_tombstones
                     WHERE account_id = ?1 AND message_id = ?2 LIMIT 1",
                    params![account_id.as_str(), message_id],
                    |row| row.get(0),
                )
                .optional()?;
            if hit.is_some() {
                return Ok(true);
            }
        }
        if self.actions {
            let hit: Option<i64> = conn
                .query_row(
                    "SELECT 1 FROM local_action_messages m
                     JOIN local_actions a ON a.id = m.action_id
                     WHERE a.account_id = ?1 AND m.message_id = ?2 AND a.undone = 0
                     LIMIT 1",
                    params![account_id.as_str(), message_id],
                    |row| row.get(0),
                )
                .optional()?;
            if hit.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Take queued folders off their rows. Returns whether a row changed.
///
/// A folder stays while any copy of the row is still in it. A row that would
/// be left with no folder is deleted when it has no copy left either and
/// `settle` is [`Settle::All`]; otherwise it keeps its last folder.
pub(super) fn settle(conn: &Connection, account_id: &AccountId, settle: Settle) -> Result<bool> {
    if settle == Settle::Later {
        return Ok(false);
    }
    let queued = {
        let mut stmt = conn.prepare(
            "SELECT message_id, folder FROM copy_orphans WHERE account_id = ?1
             ORDER BY message_id, folder",
        )?;
        stmt.query_map(params![account_id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut changed = false;
    let mut deleted = HashSet::new();
    for (message_id, folder) in queued {
        if deleted.contains(&message_id) {
            continue;
        }
        let done = settle_one(conn, account_id, &message_id, &folder, settle, &mut changed)?;
        match done {
            Outcome::Deleted => {
                deleted.insert(message_id);
            }
            Outcome::Done => {
                conn.execute(
                    "DELETE FROM copy_orphans
                     WHERE account_id = ?1 AND message_id = ?2 AND folder = ?3",
                    params![account_id.as_str(), message_id, folder],
                )?;
            }
            Outcome::Wait => {}
        }
    }
    Ok(changed)
}

enum Outcome {
    Done,
    Deleted,
    Wait,
}

fn settle_one(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    folder: &str,
    settle: Settle,
    changed: &mut bool,
) -> Result<Outcome> {
    let Some((stored, via_alias)) = find_stored(conn, account_id, &MessageId(message_id.into()))?
    else {
        return Ok(Outcome::Done);
    };
    if via_alias {
        // Queued before a merge renamed the row; the copies moved with it.
        repoint(conn, account_id, message_id, &stored.canonical_id)?;
        return Ok(Outcome::Done);
    }
    let still_here: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM message_copies
             WHERE account_id = ?1 AND message_id = ?2 AND folder = ?3 LIMIT 1",
            params![account_id.as_str(), stored.canonical_id, folder],
            |row| row.get(0),
        )
        .optional()?;
    if still_here.is_some() {
        return Ok(Outcome::Done);
    }
    let tokens = split_field(&stored.folders);
    let kept: Vec<String> = tokens
        .iter()
        .filter(|token| token.as_str() != folder)
        .cloned()
        .collect();
    if kept.len() == tokens.len() {
        return Ok(Outcome::Done);
    }
    // Asked again at the next settle, once the move has placed its copy.
    if moving_into(conn, account_id, &stored.canonical_id, folder)? {
        return Ok(Outcome::Wait);
    }
    if kept.is_empty() {
        let any_copy: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM message_copies WHERE account_id = ?1 AND message_id = ?2 LIMIT 1",
                params![account_id.as_str(), stored.canonical_id],
                |row| row.get(0),
            )
            .optional()?;
        if any_copy.is_some() {
            // Still on the server under a folder the row no longer lists
            // (a local action took it off). Keep the last folder.
            return Ok(Outcome::Done);
        }
        if settle != Settle::All {
            return Ok(Outcome::Wait);
        }
        delete_stored(conn, account_id, &MessageId(stored.canonical_id.clone()))?;
        *changed = true;
        return Ok(Outcome::Deleted);
    }
    let folders = join_tokens(&kept);
    let indexed = |folders: &str| {
        terms_for_indexed(IndexedText {
            subject: &stored.subject,
            from_name: stored.from_name.as_deref(),
            from_email: &stored.from_email,
            body: &stored.body,
            to_line: &stored.to_line,
            cc_line: &stored.cc_line,
            bcc_line: &stored.bcc_line,
            folders,
            labels: &stored.labels,
        })
    };
    let old_terms = indexed(&stored.folders);
    let new_terms = indexed(&folders);
    conn.execute(
        "UPDATE messages SET folders = ?1 WHERE account_id = ?2 AND message_id = ?3",
        params![folders, account_id.as_str(), stored.canonical_id],
    )?;
    sync_terms(conn, account_id.as_str(), &old_terms, &new_terms)?;
    *changed = true;
    Ok(Outcome::Done)
}

/// A write-back row not yet settled moves the message into `folder`.
fn moving_into(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    folder: &str,
) -> Result<bool> {
    if !table_exists(conn, "server_outbox")? {
        return Ok(false);
    }
    let hit: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM server_outbox
             WHERE account_id = ?1 AND message_id = ?2 AND kind = 'move' AND state = 'pending'
               AND target_folder = ?3 COLLATE NOCASE
             LIMIT 1",
            params![account_id.as_str(), message_id, folder],
            |row| row.get(0),
        )
        .optional()?;
    Ok(hit.is_some())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let hit: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(hit.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, BackendKind};
    use crate::backend::MailboxDelta;
    use crate::message::{Address, MessageHeaders, RemoteId};
    use crate::store::Store;
    use crate::threading::ThreadId;

    fn message(id: &str, remote: &str, folder: &str) -> Message {
        let message_id = MessageId::new(id);
        Message {
            account_id: AccountId("a".into()),
            headers: MessageHeaders {
                message_id: message_id.clone(),
                proton_conversation_id: None,
                in_reply_to: None,
                references: Vec::new(),
                from: Address {
                    name: None,
                    email: "ann@x".into(),
                },
                subject: format!("subject {id}"),
                date_unix: 1_700_000_000,
            },
            remote_id: RemoteId(remote.into()),
            thread_id: ThreadId::from_message_id(&message_id),
            snippet: String::new(),
            body_text: Some(format!("body of {id}")),
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
            list_id: None,
            reply_to: Vec::new(),
            attachment_names: Vec::new(),
            parts: None,
        }
    }

    fn copies(store: &Store) -> Vec<(String, u32, u32, String, String)> {
        let mut stmt = store
            .conn
            .prepare(
                "SELECT mailbox, uidvalidity, uid, message_id, folder FROM message_copies
                 ORDER BY mailbox, uid",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
    }

    #[test]
    fn a_copy_counts_as_stored_only_while_its_flags_can_land() {
        let store = Store::open_in_memory().unwrap();
        let account = AccountId("a".into());
        store
            .upsert_account(&Account {
                id: account.clone(),
                display_name: "A".into(),
                email: "a@x".into(),
                backend_kind: BackendKind::Imap,
            })
            .unwrap();
        store
            .apply_delta(
                &account,
                &MailboxDelta {
                    messages: vec![message("bin@x", "imap:7:3:Trash", "Trash")],
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let stored = || store.stored_uids(&account, "Trash", 7, (1, 10)).unwrap();
        assert_eq!(stored(), vec![3]);
        // Emptied from Trash in Fell: the open action's snapshot holds it.
        let thread = ThreadId::from_message_id(&MessageId::new("bin@x"));
        store.delete_thread(&account, &thread, "Trash").unwrap();
        assert_eq!(stored(), vec![3]);
        store.undo(&account).unwrap();
        assert_eq!(stored(), vec![3]);
        // A row gone with nothing to restore it: fetch the copy like new mail.
        store
            .conn
            .execute("DELETE FROM messages WHERE message_id = 'bin@x'", [])
            .unwrap();
        assert!(stored().is_empty());
        assert_eq!(copies(&store).len(), 1);
    }

    #[test]
    fn an_older_mailbox_learns_the_copy_each_row_names() {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_account(&Account {
                id: AccountId("a".into()),
                display_name: "A".into(),
                email: "a@x".into(),
                backend_kind: BackendKind::Imap,
            })
            .unwrap();
        store
            .apply_delta(
                &AccountId("a".into()),
                &MailboxDelta {
                    messages: vec![
                        message("one@x", "imap:7:3:INBOX", "INBOX"),
                        message("two@x", "imap:9:1:&Jjo-", "☺"),
                        message("local@x", "fixture-1", "INBOX"),
                    ],
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        let recorded = copies(&store);
        assert_eq!(recorded.len(), 2);
        // A mailbox file from before the table: it is rebuilt from the rows.
        store
            .conn
            .execute_batch("DROP TABLE message_copies")
            .unwrap();
        ensure_copies(&store.conn).unwrap();
        assert_eq!(copies(&store), recorded);
        assert_eq!(
            recorded[1],
            ("INBOX".into(), 7, 3, "one@x".into(), "INBOX".into())
        );
        assert_eq!(recorded[0].4, "☺");
        // A later open leaves the table as it is.
        store
            .conn
            .execute_batch("DELETE FROM message_copies")
            .unwrap();
        ensure_copies(&store.conn).unwrap();
        assert!(copies(&store).is_empty());
    }
}
