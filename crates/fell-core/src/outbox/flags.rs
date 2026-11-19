//! Durable flag writes share the MOVE outbox and its write-back generation.

use rusqlite::{Connection, OptionalExtension, params};

use super::{
    ENTRY_COLS, OutboxEntry, OutboxKind, Settled, enabled, generation, now_unix, select_entries,
};
use crate::account::AccountId;
use crate::imap::{escape_mailbox, parse_remote_id, unescape_mailbox};
use crate::store::{
    Store,
    copies::{self, CopyFlags, FlagSource},
    immediate_transaction, intents,
};
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
            "SELECT mailbox, uidvalidity, uid, folder, modseq,
                    CASE ?3 WHEN 'seen' THEN seen ELSE flagged END
             FROM message_copies WHERE account_id = ?1 AND message_id = ?2 ORDER BY mailbox, uidvalidity, uid",
        )?;
        let rows = stmt.query_map(scope, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<[u8; 8]>>(4)?,
                row.get::<_, Option<bool>>(5)?,
            ))
        })?;
        let mut insert = conn.prepare_cached(
            "INSERT INTO server_outbox
                (account_id, kind, state, message_id, source_folder, source_mailbox,
                 uidvalidity, uid, target_folder, created_unix, generation, flag_value, flag_modseq, flag_base)
             VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, '', ?8, ?9, ?10, ?11, ?12)",
        )?;
        let now = now_unix();
        for row in rows {
            let (escaped, uidvalidity, uid, folder, modseq, base) = row?;
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
                base,
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

/// Check the row immediately before STORE. A replacement during SELECT
/// may have already deleted it since it had never been sent.
pub(crate) fn mark_sent(
    store: &Store,
    account: &AccountId,
    entry: &OutboxEntry,
) -> Result<Settled> {
    let tx = immediate_transaction(store.connection())?;
    if !current(&tx, account, entry)? {
        return Ok(Settled::Stale);
    }
    let changed = tx.execute(
        "UPDATE server_outbox SET sent = 1, flag_modseq = ?3, flag_base = ?4
         WHERE account_id = ?1 AND id = ?2 AND state = 'pending' AND flag_superseded = 0",
        params![
            account.as_str(),
            entry.id,
            entry.flag_modseq.map(u64::to_be_bytes),
            entry.flag_base,
        ],
    )?;
    tx.commit()?;
    Ok(if changed == 0 {
        Settled::Retired
    } else {
        Settled::Recorded
    })
}

fn current(conn: &Connection, account: &AccountId, entry: &OutboxEntry) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM write_back_accounts
                       WHERE account_id = ?1 AND generation = ?2 AND enabled = 1)",
        )?
        .query_row(params![account.as_str(), entry.generation], |row| {
            row.get(0)
        })?)
}

pub(crate) enum Observation {
    Stored(Option<u64>),
    Fetched(CopyFlags, FetchReason),
}

#[derive(Clone, Copy)]
pub(crate) enum FetchReason {
    BeforeStore,
    Modified,
    /// Tagged OK without a STORE MODSEQ. Fetch to confirm the result.
    AfterStore,
}

pub(crate) enum Progress {
    Stale,
    Retired,
    Ready,
    /// FETCH lacks current evidence for the target. Try again with backoff.
    Unconfirmed,
    Applied,
    Conflict {
        changed: bool,
    },
}

/// Persist copy evidence, rebase eligible rows, and settle the current
/// intent in one transaction. A superseded row can still carry evidence
/// for its replacement and for the other flag.
pub(crate) fn observe(
    store: &Store,
    account: &AccountId,
    entry: &OutboxEntry,
    observation: Observation,
) -> Result<Progress> {
    let tx = immediate_transaction(store.connection())?;
    if !current(&tx, account, entry)? {
        return Ok(Progress::Stale);
    }
    let live: Option<bool> = tx
        .query_row(
            "SELECT NOT flag_superseded FROM server_outbox
             WHERE account_id = ?1 AND id = ?2 AND state = 'pending'",
            params![account.as_str(), entry.id],
            |row| row.get(0),
        )
        .optional()?;
    let (Some(mailbox), Some(validity), Some(uid), Some(value)) = (
        &entry.source_mailbox,
        entry.uidvalidity,
        entry.uid,
        entry.flag_value,
    ) else {
        return Err(Error::Action(
            "flag write needs its server copy and value".into(),
        ));
    };
    let remote_id = format!("imap:{validity}:{uid}:{}", escape_mailbox(mailbox));
    let copy =
        parse_remote_id(&remote_id).ok_or_else(|| Error::Action("invalid flag copy".into()))?;
    let (flags, before_store, confirms_previous, progress) = match observation {
        Observation::Stored(modseq) => (
            CopyFlags {
                modseq,
                seen: (entry.kind == OutboxKind::Seen).then_some(value),
                flagged: (entry.kind == OutboxKind::Flagged).then_some(value),
            },
            entry.flag_modseq,
            false,
            Progress::Applied,
        ),
        Observation::Fetched(flags, reason) => {
            let server = flag_value(flags, entry.kind);
            let matches = server == Some(value);
            let recovering = match reason {
                FetchReason::BeforeStore => entry.sent,
                FetchReason::AfterStore => true,
                FetchReason::Modified => false,
            };
            let stale = flags.modseq.is_some() && flags.modseq < entry.flag_modseq;
            let progress = match reason {
                _ if stale || server.is_none() => Progress::Unconfirmed,
                FetchReason::Modified if flags.modseq <= entry.flag_modseq => Progress::Unconfirmed,
                FetchReason::BeforeStore if !entry.sent => Progress::Ready,
                _ if matches => Progress::Applied,
                FetchReason::BeforeStore
                    if flags.modseq.is_none()
                        || entry.flag_modseq.is_none()
                        || flags.modseq == entry.flag_modseq =>
                {
                    Progress::Ready
                }
                FetchReason::BeforeStore | FetchReason::Modified
                    if server.is_some() && server == entry.flag_base =>
                {
                    Progress::Ready
                }
                _ => Progress::Conflict { changed: false },
            };
            let confirmed = recovering && matches!(progress, Progress::Applied);
            (flags, None, confirmed, progress)
        }
    };
    let active = live == Some(true);
    copies::observe_flags(
        &tx,
        account,
        &copy,
        flags,
        FlagSource::Replay {
            seen_handled: active
                && entry.kind == OutboxKind::Seen
                && matches!(progress, Progress::Applied | Progress::Conflict { .. }),
            before_store,
        },
    )?;
    advance(&tx, account, entry, flags, before_store, confirms_previous)?;

    let result = match progress {
        Progress::Unconfirmed if live.is_some() => Progress::Unconfirmed,
        _ if !active => {
            complete(&tx, entry.id)?;
            Progress::Retired
        }
        Progress::Ready => {
            // No unresolved attempt remains. A replacement can coalesce
            // this row up until we mark the next STORE on the wire.
            tx.execute(
                "UPDATE server_outbox SET sent = 0 WHERE id = ?1",
                [entry.id],
            )?;
            Progress::Ready
        }
        Progress::Applied => {
            complete(&tx, entry.id)?;
            acknowledge(&tx, account, entry, &remote_id, value)?;
            Progress::Applied
        }
        Progress::Conflict { .. } => {
            let server = flag_value(flags, entry.kind)
                .ok_or_else(|| Error::Action("flag conflict needs a server value".into()))?;
            Progress::Conflict {
                changed: settle_conflict(&tx, account, entry, server)?,
            }
        }
        other => other,
    };
    tx.commit()?;
    Ok(result)
}

fn flag_value(flags: CopyFlags, kind: OutboxKind) -> Option<bool> {
    match kind {
        OutboxKind::Seen => flags.seen,
        OutboxKind::Flagged => flags.flagged,
        OutboxKind::Move => None,
    }
}

fn complete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute(
        "UPDATE server_outbox SET state = 'done', error = '', settled_unix = ?2
         WHERE id = ?1 AND state = 'pending'",
        params![id, now_unix()],
    )?;
    Ok(())
}

fn acknowledge(
    conn: &Connection,
    account: &AccountId,
    entry: &OutboxEntry,
    remote_id: &str,
    value: bool,
) -> Result<()> {
    if entry.kind == OutboxKind::Seen {
        return intents::acknowledge_unread(conn, account, &entry.message_id, remote_id, !value);
    }
    let pending: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM server_outbox
             WHERE account_id = ?1 AND message_id = ?2
               AND kind = 'flagged' AND state = 'pending' AND flag_superseded = 0
         )",
        params![account.as_str(), entry.message_id],
        |row| row.get(0),
    )?;
    if !pending {
        intents::clear_flag(conn, account, &entry.message_id, entry.kind)?;
    }
    Ok(())
}

fn settle_conflict(
    conn: &Connection,
    account: &AccountId,
    entry: &OutboxEntry,
    server: bool,
) -> Result<bool> {
    let message = &entry.message_id;
    let kind = entry.kind;
    conn.execute(
        "UPDATE server_outbox
         SET state = 'dropped', error = 'server flag won a conflict', settled_unix = ?4
         WHERE account_id = ?1 AND message_id = ?2 AND kind = ?3 AND state = 'pending'",
        params![account.as_str(), message, kind.as_str(), now_unix()],
    )?;
    intents::clear_flag(conn, account, message, kind)?;
    // A row is starred if any live copy is flagged (Bridge's star
    // mapping), and unread if any observed copy is unread. Include copies
    // already written and copies whose remaining STOREs were cancelled.
    let (column, value) = if kind == OutboxKind::Seen {
        ("unread", !server)
    } else {
        ("starred", server)
    };
    let aggregate: Option<bool> = conn.query_row(
        "SELECT MAX(CASE ?3 WHEN 'seen' THEN NOT seen ELSE flagged END)
         FROM message_copies WHERE account_id = ?1 AND message_id = ?2",
        params![account.as_str(), message, kind.as_str()],
        |row| row.get(0),
    )?;
    let changed = conn.execute(
        &format!(
            "UPDATE messages SET {column} = ?3
             WHERE account_id = ?1 AND message_id = ?2 AND {column} != ?3"
        ),
        params![account.as_str(), message, aggregate.unwrap_or(value)],
    )?;
    Ok(changed != 0)
}

fn advance(
    conn: &Connection,
    account: &AccountId,
    entry: &OutboxEntry,
    flags: CopyFlags,
    before_store: Option<u64>,
    confirms_previous: bool,
) -> Result<()> {
    let Some(modseq) = flags.modseq else {
        return Ok(());
    };
    // A conditional STORE proves that it stepped from its request's
    // revision. A FETCH proves only the bits it returned. When it
    // confirms an interrupted write, its replacement can also step
    // past that attempt on the same flag.
    let mut update = conn.prepare_cached(
        "UPDATE server_outbox SET flag_modseq = ?7, flag_base = COALESCE(?8, flag_base)
         WHERE account_id = ?1 AND message_id = ?2 AND source_mailbox = ?3
           AND uidvalidity = ?4 AND uid = ?5 AND kind = ?6 AND state = 'pending'
           AND (flag_modseq IS NULL OR flag_modseq <= ?7)
           AND ((?8 IS NOT NULL AND (flag_modseq IS NULL OR flag_modseq = ?7
                                     OR flag_base = ?8 OR flag_value = ?8))
                OR flag_modseq = ?9
                OR (?10 AND id > ?11 AND flag_modseq IS ?12))",
    )?;
    for kind in [OutboxKind::Seen, OutboxKind::Flagged] {
        update.execute(params![
            account.as_str(),
            entry.message_id,
            entry.source_mailbox,
            entry.uidvalidity,
            entry.uid,
            kind.as_str(),
            modseq.to_be_bytes(),
            flag_value(flags, kind),
            before_store.map(u64::to_be_bytes),
            confirms_previous && kind == entry.kind,
            entry.id,
            entry.flag_modseq.map(u64::to_be_bytes),
        ])?;
    }
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
         SET source_mailbox = ?6, uidvalidity = ?7, uid = ?8, source_folder = ?9,
             flag_modseq = NULL, flag_base = NULL
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
