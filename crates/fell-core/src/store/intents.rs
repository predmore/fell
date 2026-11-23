//! Read and star changes made in Fell that the server has not seen yet.
//!
//! Fell keeps a read state the reader (or an agent) set until the server
//! reports the same value. Until then a sync pass that still reports the old
//! one does not write it over the local change. One row can stand for several
//! server copies (INBOX and Sent, an alias merged by fingerprint), so the
//! intent is dropped only once every copy recorded for the row has reported
//! the new value since it was set. After that, later server changes win again.
//!
//! The write-back outbox settles both flags. Read-only sync can settle a
//! local read intent when no write is queued.

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::account::AccountId;
use crate::outbox::{self, OutboxKind};

pub(super) const INTENT_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_flag_intents (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    -- NULL: no pending change to that flag.
    unread INTEGER,
    starred INTEGER,
    PRIMARY KEY (account_id, message_id)
);

-- Server copies (by remote id) that have reported the pending read state
-- since it was set.
CREATE TABLE IF NOT EXISTS local_flag_intent_copies (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    remote_id TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id, remote_id)
);

CREATE TRIGGER IF NOT EXISTS local_flag_intents_gone AFTER DELETE ON messages BEGIN
    DELETE FROM local_flag_intents
    WHERE account_id = old.account_id AND message_id = old.message_id;
    DELETE FROM local_flag_intent_copies
    WHERE account_id = old.account_id AND message_id = old.message_id;
END;
"#;

/// Remember that Fell set these flags on `message_id`. `None` leaves that
/// flag's intent as it was. A new read state starts over: no copy has
/// reported it yet.
pub(crate) fn record(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    unread: Option<bool>,
    starred: Option<bool>,
) -> Result<()> {
    if unread.is_none() && starred.is_none() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO local_flag_intents (account_id, message_id, unread, starred)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
            unread = COALESCE(excluded.unread, unread),
            starred = COALESCE(excluded.starred, starred)",
        params![
            account_id.as_str(),
            message_id,
            unread.map(i64::from),
            starred.map(i64::from),
        ],
    )?;
    if unread.is_some() {
        conn.execute(
            "DELETE FROM local_flag_intent_copies WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), message_id],
        )?;
    }
    outbox::flags::queue(conn, account_id, message_id, unread, starred)
}

/// Merge intents before the duplicate is deleted. Keep the survivor's
/// value for a flag edited on both rows and carry over independent edits.
pub(super) fn merge(conn: &Connection, account: &AccountId, from: &str, to: &str) -> Result<()> {
    let scope = params![account.as_str(), from, to];
    let (unread, starred): (Option<bool>, Option<bool>) = conn
        .prepare_cached(
            "SELECT CASE WHEN k.unread <> d.unread THEN k.unread END,
                    CASE WHEN k.starred <> d.starred THEN k.starred END
             FROM local_flag_intents d
             JOIN local_flag_intents k ON k.account_id = d.account_id AND k.message_id = ?3
             WHERE d.account_id = ?1 AND d.message_id = ?2",
        )?
        .query_row(scope, |row| Ok((row.get(0)?, row.get(1)?)))
        .optional()?
        .unwrap_or_default();
    let merged = conn
        .prepare_cached(
            "INSERT INTO local_flag_intents (account_id, message_id, unread, starred)
         SELECT account_id, ?3, unread, starred FROM local_flag_intents
         WHERE account_id = ?1 AND message_id = ?2
         ON CONFLICT(account_id, message_id) DO UPDATE SET
             unread = COALESCE(local_flag_intents.unread, excluded.unread),
             starred = COALESCE(local_flag_intents.starred, excluded.starred)",
        )?
        .execute(scope)?;
    if merged != 0 && unread.is_none() {
        conn.prepare_cached(
            "INSERT OR IGNORE INTO local_flag_intent_copies (account_id, message_id, remote_id)
             SELECT account_id, ?3, remote_id FROM local_flag_intent_copies
             WHERE account_id = ?1 AND message_id = ?2",
        )?
        .execute(scope)?;
    }
    outbox::flags::repoint(conn, account, from, to, None)?;
    // Conflicting values cannot both play on the merged set of copies.
    // Reuse the normal queue to retain recovery for any STORE already sent.
    record(conn, account, to, unread, starred)
}

/// The unread value the server copy `remote_id` may write to `message_id`:
/// the server's, unless Fell changed it and some copy has not caught up.
///
/// A copy that reports the pending value is noted. The intent is dropped
/// once every copy recorded for the row (and this one) has reported it; a
/// copy that still disagrees, or has not been seen since, keeps it.
pub(crate) fn server_unread(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    remote_id: &str,
    server: bool,
) -> Result<bool> {
    let Some(local) = pending_unread(conn, account_id, message_id)? else {
        return Ok(server);
    };
    if !outbox::flags::pending_seen(conn, account_id, message_id)? {
        observe_unread(conn, account_id, message_id, remote_id, server, local)?;
    }
    Ok(local)
}

fn pending_unread(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Option<bool>> {
    let pending: Option<Option<i64>> = conn
        .prepare_cached(
            "SELECT unread FROM local_flag_intents WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_row(params![account_id.as_str(), message_id], |row| row.get(0))
        .optional()?;
    Ok(pending.flatten().map(|value| value != 0))
}

pub(crate) fn acknowledge_unread(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    remote_id: &str,
    server: bool,
) -> Result<()> {
    if let Some(local) = pending_unread(conn, account_id, message_id)? {
        observe_unread(conn, account_id, message_id, remote_id, server, local)?;
    }
    Ok(())
}

fn observe_unread(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    remote_id: &str,
    server: bool,
    local: bool,
) -> Result<()> {
    if local != server {
        conn.execute(
            "DELETE FROM local_flag_intent_copies
             WHERE account_id = ?1 AND message_id = ?2 AND remote_id = ?3",
            params![account_id.as_str(), message_id, remote_id],
        )?;
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO local_flag_intent_copies (account_id, message_id, remote_id)
         VALUES (?1, ?2, ?3)",
        params![account_id.as_str(), message_id, remote_id],
    )?;
    let waiting: i64 = conn.query_row(
        "SELECT COUNT(*) FROM message_copies c
         WHERE c.account_id = ?1 AND c.message_id = ?2
           AND NOT EXISTS (
               SELECT 1 FROM local_flag_intent_copies a
               WHERE a.account_id = c.account_id AND a.message_id = c.message_id
                 AND a.remote_id = 'imap:' || c.uidvalidity || ':' || c.uid || ':' || c.mailbox
           )",
        params![account_id.as_str(), message_id],
        |row| row.get(0),
    )?;
    if waiting == 0 {
        clear_flag(conn, account_id, message_id, OutboxKind::Seen)?;
    }
    Ok(())
}

/// Write-back settled a flag, or every server copy agreed on the read state.
pub(crate) fn clear_flag(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    kind: OutboxKind,
) -> Result<()> {
    let column = match kind {
        OutboxKind::Seen => "unread",
        OutboxKind::Flagged => "starred",
        OutboxKind::Move
        | OutboxKind::AppendDraft
        | OutboxKind::DeleteDraft
        | OutboxKind::AppendSent
        | OutboxKind::Keyword
        | OutboxKind::BridgeLabel => {
            return Err(crate::Error::Action(
                "write has no system flag intent".into(),
            ));
        }
    };
    conn.execute(
        &format!("UPDATE local_flag_intents SET {column} = NULL WHERE account_id = ?1 AND message_id = ?2"),
        params![account_id.as_str(), message_id],
    )?;
    conn.execute(
        "DELETE FROM local_flag_intents
         WHERE account_id = ?1 AND message_id = ?2 AND unread IS NULL AND starred IS NULL",
        params![account_id.as_str(), message_id],
    )?;
    if kind == OutboxKind::Seen {
        conn.execute(
            "DELETE FROM local_flag_intent_copies WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), message_id],
        )?;
    }
    Ok(())
}

/// The star Fell set on `message_id`, if one is pending. The outbox settles
/// the intent. Server observations can update the displayed star afterward.
pub(crate) fn local_star(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Option<bool>> {
    let pending: Option<Option<i64>> = conn
        .query_row(
            "SELECT starred FROM local_flag_intents WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), message_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(pending.flatten().map(|value| value != 0))
}

/// A read or star change made in Fell that the server has not reported back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingFlags {
    pub message_id: String,
    /// `None`: read state is the server's.
    pub unread: Option<bool>,
    /// `None`: the star is the server's.
    pub starred: Option<bool>,
}

/// Every pending change on the account, by Message-ID.
pub(crate) fn pending(conn: &Connection, account_id: &AccountId) -> Result<Vec<PendingFlags>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, unread, starred FROM local_flag_intents
         WHERE account_id = ?1 ORDER BY message_id",
    )?;
    let rows = stmt.query_map(params![account_id.as_str()], |row| {
        Ok(PendingFlags {
            message_id: row.get(0)?,
            unread: row.get::<_, Option<i64>>(1)?.map(|value| value != 0),
            starred: row.get::<_, Option<i64>>(2)?.map(|value| value != 0),
        })
    })?;
    rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
}
