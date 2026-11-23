//! A draft has one current server copy. An APPEND row keeps its new UID
//! until the previous copy is gone. The next revision waits behind it, so
//! a crash between APPEND and cleanup leaves at most two copies.
use rusqlite::{Connection, OptionalExtension, params};

use super::{OutboxEntry, enabled, generation, now_unix};
use crate::{AccountId, FolderRole, MessageId, Result, Store};

pub(super) fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS server_draft_copies (
            account_id TEXT NOT NULL,
            draft_id TEXT NOT NULL,
            mailbox TEXT NOT NULL,
            uidvalidity INTEGER NOT NULL,
            uid INTEGER NOT NULL,
            digest BLOB NOT NULL,
            PRIMARY KEY (account_id, draft_id),
            UNIQUE (account_id, mailbox, uidvalidity, uid)
         ) WITHOUT ROWID;
         CREATE TABLE IF NOT EXISTS server_draft_rescans (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             account_id TEXT NOT NULL,
             mailbox TEXT NOT NULL,
             UNIQUE (account_id, mailbox)
         );
         CREATE INDEX IF NOT EXISTS idx_outbox_draft
             ON server_outbox(account_id, draft_id, id) WHERE draft_id IS NOT NULL;",
    )?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Copy {
    /// Raw IMAP mailbox name.
    pub mailbox: String,
    pub validity: u32,
    pub uid: u32,
}

pub(crate) fn save_status(
    store: &Store,
    account: &AccountId,
    draft: &str,
    digest: &[u8; 32],
) -> Result<Option<crate::compose::DraftSaveStatus>> {
    use crate::compose::DraftSaveStatus;
    let conn = store.connection();
    let (state, confirmed): (Option<String>, bool) = conn
        .prepare_cached(
            "SELECT (SELECT state FROM server_outbox WHERE account_id = ?1 AND draft_id = ?2
                    AND kind = 'append_draft' AND draft_digest = ?3 AND generation = ?4
                    ORDER BY id DESC LIMIT 1),
                EXISTS (SELECT 1 FROM server_draft_copies
                    WHERE account_id = ?1 AND draft_id = ?2 AND digest = ?3)",
        )?
        .query_row(
            params![account.as_str(), draft, digest, generation(conn, account)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
    Ok(match state.as_deref() {
        Some("dropped") => Some(DraftSaveStatus::ServerFailed),
        Some("pending") => None,
        _ if confirmed => Some(DraftSaveStatus::Server),
        _ => None,
    })
}

pub(crate) fn has_copy(store: &Store, account: &AccountId, draft: &str) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS (SELECT 1 FROM server_draft_copies WHERE account_id = ?1 AND draft_id = ?2
            UNION ALL SELECT 1 FROM server_outbox WHERE account_id = ?1 AND draft_id = ?2
                AND kind = 'append_draft' AND state = 'pending')",
        params![account.as_str(), draft],
        |row| row.get(0),
    )?)
}

pub(crate) fn queue(
    store: &Store,
    account: &AccountId,
    draft: &str,
    digest: &[u8],
    message: MessageId,
    mime: &[u8],
) -> Result<bool> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    if !enabled(&tx, account)? {
        return Ok(false);
    }
    let active: bool = tx.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM compose_drafts d
             WHERE d.account_id = ?1 AND d.id = ?2 AND d.discarded_action_id IS NULL
               AND NOT EXISTS (SELECT 1 FROM compose_send s
                   WHERE s.account_id = d.account_id AND s.draft_id = d.id AND s.state != 'clear')
         )",
        params![account.as_str(), draft],
        |row| row.get(0),
    )?;
    if !active {
        return Ok(false);
    }
    let rearmed = rearm_recovery(&tx, account, draft)?;
    let latest: Option<Vec<u8>> = tx.query_row(
        "WITH cleanup AS (
             SELECT max(id) AS last_delete FROM server_outbox
             WHERE account_id = ?1 AND draft_id = ?2 AND kind = 'delete_draft' AND state = 'pending'
         )
         SELECT COALESCE(
             (SELECT draft_digest FROM server_outbox WHERE account_id = ?1 AND draft_id = ?2
                AND kind = 'append_draft' AND state = 'pending' AND append_mime IS NOT NULL
                AND id > COALESCE(cleanup.last_delete, 0) ORDER BY id DESC LIMIT 1),
             (SELECT digest FROM server_draft_copies WHERE account_id = ?1 AND draft_id = ?2
                AND cleanup.last_delete IS NULL)
         ) FROM cleanup",
        params![account.as_str(), draft],
        |row| row.get(0),
    )?;
    if !rearmed && latest.as_deref() == Some(digest) {
        return Ok(false);
    }
    // Give the replacement its own row ID. A replay that already read the
    // superseded payload can no longer mark that row sent.
    cancel_unsent(&tx, account, draft)?;
    tx.execute(
        "INSERT INTO server_outbox (account_id, kind, state, message_id, source_folder,
             target_folder, generation, created_unix, append_mime, draft_id, draft_digest)
         VALUES (?1, 'append_draft', 'pending', ?2, '', ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            account.as_str(),
            message.as_str(),
            FolderRole::Drafts.default_name(),
            generation(&tx, account)?,
            now_unix(),
            mime,
            draft,
            digest
        ],
    )?;
    tx.commit()?;
    Ok(true)
}

/// Give an unconfirmed APPEND a fresh bounded lookup on the next save or
/// send. Its MIME was cleared when recovery stopped. Only the newer row is
/// allowed to APPEND if the recovered UID range proves the earlier copy absent.
fn rearm_recovery(conn: &Connection, account: &AccountId, draft: &str) -> Result<bool> {
    Ok(conn.execute(
        "UPDATE server_outbox SET state = 'pending', append_checks = 0,
             retry_unix = 0, error = '', settled_unix = NULL
         WHERE account_id = ?1 AND draft_id = ?2 AND generation = ?3
           AND kind = 'append_draft' AND state = 'dropped' AND append_checks > 0",
        params![account.as_str(), draft, generation(conn, account)?],
    )? != 0)
}

pub(crate) fn cancel_unsent(conn: &Connection, account: &AccountId, draft: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM server_outbox WHERE account_id = ?1 AND draft_id = ?2
         AND kind = 'append_draft' AND state = 'pending' AND NOT sent",
        params![account.as_str(), draft],
    )?;
    Ok(())
}

pub(crate) fn discard(conn: &Connection, account: &AccountId, draft: &str) -> Result<()> {
    if enabled(conn, account)? {
        queue_delete(conn, account, draft)
    } else {
        release_copy(conn, account, draft)
    }
}

/// The next sync must revisit the mailbox: its cursor can already be
/// past the UID whose synced representation was removed on import.
fn release_copy(conn: &Connection, account: &AccountId, draft: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO server_draft_rescans (account_id, mailbox)
         SELECT account_id, mailbox FROM server_draft_copies
         WHERE account_id = ?1 AND draft_id = ?2",
        params![account.as_str(), draft],
    )?;
    conn.execute(
        "DELETE FROM server_draft_copies WHERE account_id = ?1 AND draft_id = ?2",
        params![account.as_str(), draft],
    )?;
    Ok(())
}

pub(crate) fn rescans(store: &Store, account: &AccountId) -> Result<Vec<(i64, String)>> {
    let mut stmt = store
        .connection()
        .prepare_cached("SELECT id, mailbox FROM server_draft_rescans WHERE account_id = ?1")?;
    Ok(stmt
        .query_map([account.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn finish_rescans(store: &Store, ids: impl Iterator<Item = i64>) -> Result<()> {
    for id in ids {
        // If a discard raced this sync, REPLACE assigned it a different ID.
        store
            .connection()
            .execute("DELETE FROM server_draft_rescans WHERE id = ?1", [id])?;
    }
    Ok(())
}

pub(crate) fn queue_delete(conn: &Connection, account: &AccountId, draft: &str) -> Result<()> {
    if !enabled(conn, account)? {
        return Ok(());
    }
    cancel_unsent(conn, account, draft)?;
    rearm_recovery(conn, account, draft)?;
    conn.execute(
        "INSERT INTO server_outbox (account_id, kind, state, message_id, source_folder,
             target_folder, generation, created_unix, draft_id)
         SELECT ?1, 'delete_draft', 'pending', '', ?3, '', ?4, ?5, ?2
         WHERE NOT EXISTS (SELECT 1 FROM server_outbox WHERE account_id = ?1
                     AND draft_id = ?2 AND kind = 'delete_draft' AND state = 'pending')
           AND (EXISTS (SELECT 1 FROM server_draft_copies WHERE account_id = ?1 AND draft_id = ?2)
             OR EXISTS (SELECT 1 FROM server_outbox WHERE account_id = ?1 AND draft_id = ?2
                        AND kind = 'append_draft' AND state = 'pending'))",
        params![
            account.as_str(),
            draft,
            FolderRole::Drafts.default_name(),
            generation(conn, account)?,
            now_unix()
        ],
    )?;
    Ok(())
}

pub(crate) struct Pending {
    pub current: Option<Copy>,
    pub placed: Option<Copy>,
    pub can_append: bool,
}

pub(crate) fn pending(store: &Store, entry: &OutboxEntry) -> Result<Option<Pending>> {
    Ok(store.connection().query_row(
        "SELECT c.mailbox, c.uidvalidity, c.uid, o.target_mailbox, o.placed_uidvalidity, o.placed_uid,
                EXISTS (SELECT 1 FROM compose_drafts d WHERE d.account_id = o.account_id
                        AND d.id = o.draft_id AND d.discarded_action_id IS NULL)
                AND NOT EXISTS (SELECT 1 FROM compose_send s WHERE s.account_id = o.account_id
                        AND s.draft_id = o.draft_id AND s.state = 'sent')
         FROM server_outbox o JOIN write_back_accounts a USING (account_id)
         LEFT JOIN server_draft_copies c ON c.account_id = o.account_id AND c.draft_id = o.draft_id
         WHERE o.id = ?1 AND a.enabled AND o.generation = a.generation AND o.state = 'pending'
           AND NOT EXISTS (SELECT 1 FROM server_outbox t WHERE t.account_id = o.account_id
               AND t.draft_id = o.draft_id AND t.generation = o.generation AND t.id < o.id
               AND (t.state = 'pending' OR
                    (o.kind = 'append_draft' AND t.state = 'dropped' AND t.append_checks > 0)))",
        [entry.id],
        |row| {
            let copy = |offset| -> rusqlite::Result<Option<Copy>> {
                let Some(uid) = row.get(offset + 2)? else { return Ok(None) };
                Ok(Some(Copy { mailbox: row.get(offset)?, validity: row.get(offset + 1)?, uid }))
            };
            Ok(Pending { current: copy(0)?, placed: copy(3)?, can_append: row.get(6)? })
        },
    ).optional()?)
}

/// Commit APPENDUID (or the recovered UID) before issuing either delete command.
pub(crate) fn placed(store: &Store, entry: &OutboxEntry, copy: &Copy) -> Result<bool> {
    Ok(store.connection().execute(
        "UPDATE server_outbox SET placed_uidvalidity = ?2, placed_uid = ?3
         WHERE id = ?1 AND state = 'pending' AND generation IN
             (SELECT generation FROM write_back_accounts WHERE account_id = server_outbox.account_id AND enabled)",
        params![entry.id, copy.validity, copy.uid],
    )? != 0)
}

pub(crate) fn finish(store: &Store, entry: &OutboxEntry) -> Result<bool> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    let changed = super::complete_append_in(&tx, entry)?;
    if changed {
        if entry.kind == super::OutboxKind::DeleteDraft {
            tx.execute(
                "DELETE FROM server_draft_copies WHERE (account_id, draft_id) =
                    (SELECT account_id, draft_id FROM server_outbox WHERE id = ?1)",
                [entry.id],
            )?;
        } else {
            promote_placed(store, &tx, entry)?;
        }
    }
    tx.commit()?;
    Ok(changed)
}

/// Stop retrying a refused cleanup. The old copy can be rediscovered
/// if the server still has it. An already confirmed APPEND remains the
/// current copy so later saves replace that UID.
pub(super) fn abandon_cleanup(
    store: &Store,
    tx: &Connection,
    account: &AccountId,
    entry: &OutboxEntry,
) -> Result<()> {
    let draft: String = tx.query_row(
        "SELECT draft_id FROM server_outbox WHERE id = ?1",
        [entry.id],
        |row| row.get(0),
    )?;
    release_copy(tx, account, &draft)?;
    promote_placed(store, tx, entry)
}

fn promote_placed(store: &Store, tx: &Connection, entry: &OutboxEntry) -> Result<()> {
    let placed = tx
        .query_row(
            "SELECT account_id, target_mailbox, placed_uidvalidity, placed_uid
             FROM server_outbox WHERE id = ?1 AND placed_uid IS NOT NULL",
            [entry.id],
            |row| {
                Ok((
                    AccountId(row.get(0)?),
                    Copy {
                        mailbox: row.get(1)?,
                        validity: row.get(2)?,
                        uid: row.get(3)?,
                    },
                ))
            },
        )
        .optional()?;
    if let Some((account, copy)) = placed {
        store.hide_server_draft_copy(tx, &account, &copy)?;
        tx.execute(
            "INSERT INTO server_draft_copies (account_id, draft_id, mailbox, uidvalidity, uid, digest)
             SELECT account_id, draft_id, target_mailbox, placed_uidvalidity, placed_uid, draft_digest
             FROM server_outbox WHERE id = ?1
             ON CONFLICT(account_id, draft_id) DO UPDATE SET mailbox = excluded.mailbox,
                 uidvalidity = excluded.uidvalidity, uid = excluded.uid, digest = excluded.digest",
            [entry.id],
        )?;
    }
    Ok(())
}

pub(crate) fn local_id(
    conn: &Connection,
    account: &AccountId,
    copy: &Copy,
) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT draft_id FROM server_draft_copies
         WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
        )?
        .query_row(
            params![account.as_str(), copy.mailbox, copy.validity, copy.uid],
            |row| row.get(0),
        )
        .optional()?)
}

pub(crate) fn import(
    conn: &Connection,
    account: &AccountId,
    draft: &str,
    copy: &Copy,
    digest: &[u8],
) -> Result<()> {
    conn.execute(
        "INSERT INTO server_draft_copies (account_id, draft_id, mailbox, uidvalidity, uid, digest)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            account.as_str(),
            draft,
            copy.mailbox,
            copy.validity,
            copy.uid,
            digest
        ],
    )?;
    Ok(())
}

/// Also recognizes a sync delta that raced APPENDUID or is replayed after
/// a crash. In that case APPEND's fresh Message-ID and UIDNEXT bind it.
pub(crate) fn hides(
    conn: &Connection,
    account: &AccountId,
    remote: &crate::imap::RemoteCopy,
    message: &MessageId,
) -> Result<bool> {
    let Some(mailbox) = crate::imap::unescape_mailbox(&remote.mailbox) else {
        return Ok(false);
    };
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS (
             SELECT 1 FROM server_draft_copies WHERE account_id = ?1 AND mailbox = ?2
               AND uidvalidity = ?3 AND uid = ?4
             UNION ALL
             SELECT 1 FROM server_outbox WHERE account_id = ?1 AND message_id = ?5
               AND kind = 'append_draft' AND sent AND target_mailbox = ?2
               AND (state = 'pending' OR (state = 'dropped' AND append_checks > 0))
               AND ((placed_uidvalidity = ?3 AND placed_uid = ?4)
                    OR (placed_uid IS NULL AND target_uidvalidity = ?3 AND target_uidnext <= ?4))
         )",
        )?
        .query_row(
            params![
                account.as_str(),
                mailbox,
                remote.uidvalidity,
                remote.uid,
                message.as_str()
            ],
            |row| row.get(0),
        )?)
}
