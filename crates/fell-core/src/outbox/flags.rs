//! Durable flag writes share the MOVE outbox and its write-back generation.

use rusqlite::{Connection, OptionalExtension, params};

use super::{ENTRY_COLS, OutboxEntry, OutboxKind, enabled, generation, now_unix, select_entries};
use crate::account::AccountId;
use crate::imap::{escape_mailbox, parse_remote_id, unescape_mailbox};
use crate::store::{Store, copies, immediate_transaction, intents};
use crate::{Error, Result};

/// Record the new intent in the same transaction as the local edit. A sent
/// row may have reached the server and is kept only to recover its revision.
pub(crate) fn queue(
    conn: &Connection,
    account: &AccountId,
    message: &str,
    unread: Option<bool>,
    starred: Option<bool>,
) -> Result<()> {
    if !enabled(conn, account)? {
        return Ok(());
    }
    let generation = generation(conn, account)?;
    for (kind, value) in [
        (OutboxKind::Seen, unread.map(|unread| !unread)),
        (OutboxKind::Flagged, starred),
    ] {
        let Some(value) = value else { continue };
        let scope = params![account.as_str(), message, kind.as_str()];
        conn.execute(
            "DELETE FROM server_outbox
             WHERE account_id = ?1 AND message_id = ?2 AND kind = ?3
               AND state = 'pending' AND sent = 0",
            scope,
        )?;
        conn.execute(
            "UPDATE server_outbox SET flag_superseded = 1, retry_unix = 0
             WHERE account_id = ?1 AND message_id = ?2 AND kind = ?3 AND state = 'pending'",
            scope,
        )?;
        let mut stmt = conn.prepare_cached(
            "SELECT mailbox, uidvalidity, uid, folder, modseq FROM message_copies
             WHERE account_id = ?1 AND message_id = ?2 ORDER BY mailbox, uidvalidity, uid",
        )?;
        let rows = stmt.query_map(params![account.as_str(), message], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<[u8; 8]>>(4)?,
            ))
        })?;
        let mut insert = conn.prepare_cached(
            "INSERT INTO server_outbox
                (account_id, kind, state, message_id, source_folder, source_mailbox,
                 uidvalidity, uid, target_folder, created_unix, generation, flag_value, flag_modseq)
             VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, '', ?8, ?9, ?10, ?11)",
        )?;
        let now = now_unix();
        for row in rows {
            let (escaped, uidvalidity, uid, folder, modseq) = row?;
            let mailbox = unescape_mailbox(&escaped)
                .ok_or_else(|| Error::Action("invalid mailbox in flag copy".into()))?;
            insert.execute(params![
                account.as_str(),
                kind.as_str(),
                message,
                folder,
                mailbox,
                uidvalidity,
                uid,
                now,
                generation,
                value,
                modseq,
            ])?;
        }
    }
    Ok(())
}

/// Sync keeps a queued read edit until replay has settled it, even when the
/// server happens to report matching flags while the write is still queued.
pub(crate) fn pending_seen(conn: &Connection, account: &AccountId, message: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS(
             SELECT 1 FROM server_outbox
             WHERE account_id = ?1 AND message_id = ?2 AND kind = 'seen'
               AND state = 'pending' AND flag_superseded = 0
         )",
        )?
        .query_row(params![account.as_str(), message], |row| row.get(0))?)
}

/// A preceding MOVE may have placed this copy since the pass took its snapshot.
pub(crate) fn pending(store: &Store, account: &AccountId, id: i64) -> Result<Option<OutboxEntry>> {
    Ok(select_entries(
        store.connection(),
        &format!(
            "SELECT {ENTRY_COLS} FROM server_outbox
                  WHERE account_id = ?1 AND id = {id} AND state = 'pending'"
        ),
        account,
    )?
    .pop())
}

/// Check the row is still current immediately before STORE, and retain the
/// revision used on the wire in case replay is interrupted.
pub(crate) fn mark_sent(
    store: &Store,
    account: &AccountId,
    entry: &OutboxEntry,
    modseq: Option<u64>,
) -> Result<bool> {
    let tx = immediate_transaction(store.connection())?;
    let changed = tx.execute(
        "UPDATE server_outbox SET sent = 1, flag_modseq = ?3
         WHERE account_id = ?1 AND id = ?2 AND state = 'pending' AND flag_superseded = 0
           AND generation = (SELECT generation FROM write_back_accounts
                             WHERE account_id = ?1 AND enabled = 1)",
        params![account.as_str(), entry.id, modseq.map(u64::to_be_bytes)],
    )?;
    tx.commit()?;
    Ok(changed != 0)
}

pub(crate) enum Observed {
    /// STORE succeeded, or replay confirmed a previous attempt's final value.
    Applied {
        modseq: Option<u64>,
        from_store: bool,
    },
    Conflict {
        value: bool,
        modseq: u64,
    },
    /// A replacement is queued and the old write's value was absent.
    Superseded,
}

#[derive(PartialEq, Eq)]
pub(crate) enum Finished {
    Stale,
    Superseded,
    Applied,
    Conflict { changed: bool },
}

pub(crate) fn finish(
    store: &Store,
    account: &AccountId,
    entry: &OutboxEntry,
    observed: Observed,
) -> Result<Finished> {
    let tx = immediate_transaction(store.connection())?;
    let superseded: Option<bool> = tx
        .query_row(
            "SELECT flag_superseded FROM server_outbox
         WHERE account_id = ?1 AND id = ?2 AND state = 'pending'
           AND generation = (SELECT generation FROM write_back_accounts
                             WHERE account_id = ?1 AND enabled = 1)",
            params![account.as_str(), entry.id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(superseded) = superseded else {
        return Ok(Finished::Stale);
    };
    let message = &entry.message_id;
    let kind = entry.kind;
    let (mailbox, validity, uid) = match (&entry.source_mailbox, entry.uidvalidity, entry.uid) {
        (Some(mailbox), Some(validity), Some(uid)) => (mailbox, validity, uid),
        _ => return Err(Error::Action("flag write needs its server copy".into())),
    };
    let remote_id = format!("imap:{validity}:{uid}:{}", escape_mailbox(mailbox));
    let copy =
        parse_remote_id(&remote_id).ok_or_else(|| Error::Action("invalid flag copy".into()))?;
    let now = now_unix();
    tx.execute(
        "UPDATE server_outbox SET state = 'done', error = '', settled_unix = ?2 WHERE id = ?1",
        params![entry.id, now],
    )?;
    let result = match observed {
        Observed::Applied { modseq, from_store } => {
            if let Some(modseq) = modseq {
                // Advance other queued edits past this write, provided they
                // were based on the same observation.
                advance(&tx, account, entry, modseq, from_store)?;
                if from_store || kind == OutboxKind::Seen {
                    copies::observe_modseq(&tx, account, &copy, modseq)?;
                }
            }
            if superseded {
                Finished::Superseded
            } else {
                if kind == OutboxKind::Seen {
                    intents::acknowledge_unread(
                        &tx,
                        account,
                        message,
                        &remote_id,
                        entry.flag_value == Some(false),
                    )?;
                } else {
                    let pending: bool = tx.query_row(
                        "SELECT EXISTS(
                             SELECT 1 FROM server_outbox
                             WHERE account_id = ?1 AND message_id = ?2
                               AND kind = 'flagged' AND state = 'pending'
                               AND flag_superseded = 0
                         )",
                        params![account.as_str(), message],
                        |row| row.get(0),
                    )?;
                    if !pending {
                        intents::clear_flag(&tx, account, message, kind)?;
                    }
                }
                Finished::Applied
            }
        }
        Observed::Conflict { modseq, .. } if superseded => {
            // The reply belongs to an older edit. Let the replacement run
            // against this observation, without changing its local value.
            advance(&tx, account, entry, modseq, false)?;
            Finished::Superseded
        }
        Observed::Conflict { value, modseq } => {
            // The row represents all its copies. This server change wins
            // over the same edit on the remaining copies, too.
            tx.execute(
                "UPDATE server_outbox
                 SET state = 'dropped', error = 'server flag won a conflict', settled_unix = ?4
                 WHERE account_id = ?1 AND message_id = ?2 AND kind = ?3 AND state = 'pending'",
                params![account.as_str(), message, kind.as_str(), now],
            )?;
            intents::clear_flag(&tx, account, message, kind)?;
            let (column, value) = if kind == OutboxKind::Seen {
                copies::observe_modseq(&tx, account, &copy, modseq)?;
                ("unread", !value)
            } else {
                ("starred", value)
            };
            let changed = tx.execute(
                &format!(
                    "UPDATE messages SET {column} = ?3
                          WHERE account_id = ?1 AND message_id = ?2 AND {column} != ?3"
                ),
                params![account.as_str(), message, value],
            )?;
            Finished::Conflict {
                changed: changed != 0,
            }
        }
        Observed::Superseded => Finished::Superseded,
    };
    tx.commit()?;
    Ok(result)
}

fn advance(
    conn: &Connection,
    account: &AccountId,
    entry: &OutboxEntry,
    modseq: u64,
    all_flags: bool,
) -> Result<()> {
    conn.execute(
        "UPDATE server_outbox SET flag_modseq = ?7
         WHERE account_id = ?1 AND message_id = ?2 AND source_mailbox = ?3
           AND uidvalidity = ?4 AND uid = ?5 AND state = 'pending'
           AND kind IN ('seen', 'flagged') AND (kind = ?6 OR ?9)
           AND (flag_modseq IS NULL OR (flag_modseq = ?8 AND flag_modseq < ?7))",
        params![
            account.as_str(),
            entry.message_id,
            entry.source_mailbox,
            entry.uidvalidity,
            entry.uid,
            entry.kind.as_str(),
            modseq.to_be_bytes(),
            entry.flag_modseq.map(u64::to_be_bytes),
            all_flags,
        ],
    )?;
    Ok(())
}

/// The identity stays attached to the copy when a prior queued MOVE places it.
pub(super) fn follow_move(
    conn: &Connection,
    account: &AccountId,
    sent: &OutboxEntry,
) -> Result<()> {
    conn.execute(
        "UPDATE server_outbox
         SET source_mailbox = ?6, uidvalidity = ?7, uid = ?8, source_folder = ?9, flag_modseq = NULL
         WHERE account_id = ?1 AND message_id = ?2 AND source_mailbox = ?3
           AND uidvalidity = ?4 AND uid = ?5 AND kind IN ('seen', 'flagged') AND state = 'pending'",
        params![
            account.as_str(),
            sent.message_id,
            sent.source_mailbox,
            sent.uidvalidity,
            sent.uid,
            sent.target_mailbox,
            sent.placed_uidvalidity,
            sent.placed_uid,
            sent.target_folder,
        ],
    )?;
    Ok(())
}
