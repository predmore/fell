//! A draft has one current server copy. An APPEND row keeps its new UID
//! until the previous copy is gone. The next revision waits behind it, so
//! a crash between APPEND and cleanup leaves at most two copies.
use rusqlite::{Connection, OptionalExtension, params};

use super::{OutboxEntry, Settled, enabled, generation, now_unix};
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
         CREATE TABLE IF NOT EXISTS discarded_server_draft_copies (
             account_id TEXT NOT NULL,
             action_id INTEGER NOT NULL,
             draft_id TEXT NOT NULL,
             mailbox TEXT NOT NULL,
             uidvalidity INTEGER NOT NULL,
             uid INTEGER NOT NULL,
             digest BLOB NOT NULL,
             message_id TEXT,
             needs_rescan INTEGER NOT NULL DEFAULT 1,
             PRIMARY KEY (account_id, action_id, draft_id)
         ) WITHOUT ROWID;",
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
            "SELECT (SELECT CASE WHEN o.state = 'pending' AND o.placed_uid IS NOT NULL THEN 'placed'
                                 WHEN o.state = 'pending' AND (o.error != '' OR
                                    (SELECT (p.state = 'pending' AND p.sent AND p.placed_uid IS NULL)
                                            OR (p.state = 'dropped' AND p.append_checks > 0)
                                     FROM server_outbox p
                                     WHERE p.account_id = o.account_id AND p.draft_id = o.draft_id
                                       AND p.generation = o.generation AND p.kind = 'append_draft'
                                       AND p.id < o.id ORDER BY p.id DESC LIMIT 1))
                                 THEN 'retrying' ELSE o.state END
                    FROM server_outbox o WHERE o.account_id = ?1 AND o.draft_id = ?2
                    AND o.kind = 'append_draft' AND o.draft_digest = ?3 AND o.generation = ?4
                    ORDER BY o.id DESC LIMIT 1),
                EXISTS (SELECT 1 FROM server_draft_copies
                    WHERE account_id = ?1 AND draft_id = ?2 AND digest = ?3)",
        )?
        .query_row(
            params![account.as_str(), draft, digest, generation(conn, account)?],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
    Ok(match state.as_deref() {
        _ if confirmed => Some(DraftSaveStatus::Server),
        Some("placed") => Some(DraftSaveStatus::Server),
        Some("dropped") => Some(DraftSaveStatus::ServerFailed),
        Some("retrying") => Some(DraftSaveStatus::ServerRetrying),
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
    if latest.as_deref() == Some(digest) {
        return Ok(false);
    }
    // Give the replacement its own row ID. A replay that already read the
    // superseded payload can no longer mark that row sent.
    cancel_unsent(&tx, account, draft)?;
    insert_append(&tx, account, draft, digest, message, mime)?;
    tx.commit()?;
    Ok(true)
}

fn insert_append(
    conn: &Connection,
    account: &AccountId,
    draft: &str,
    digest: &[u8],
    message: MessageId,
    mime: &[u8],
) -> Result<()> {
    conn.execute(
        "INSERT INTO server_outbox (account_id, kind, state, message_id, source_folder,
             target_folder, generation, created_unix, append_mime, draft_id, draft_digest)
         VALUES (?1, 'append_draft', 'pending', ?2, '', ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            account.as_str(),
            message.as_str(),
            FolderRole::Drafts.default_name(),
            generation(conn, account)?,
            now_unix(),
            mime,
            draft,
            digest
        ],
    )?;
    Ok(())
}

/// If a newer revision is already queued, give the older transfer one last
/// lookup with no payload. If that cannot confirm the old transfer, retire
/// its row quietly and let the newer revision upload.
pub(super) fn recover_for_waiting_save(
    conn: &Connection,
    account: &AccountId,
    entry: &OutboxEntry,
    (checks, limit): (u32, u32),
    error: &str,
) -> Result<Option<Settled>> {
    let waiting: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM server_outbox WHERE account_id = ?2 AND generation = ?3
             AND draft_id = (SELECT draft_id FROM server_outbox WHERE id = ?1)
             AND id > ?1 AND kind = 'append_draft' AND state = 'pending' AND NOT sent)",
        params![entry.id, account.as_str(), entry.generation],
        |row| row.get(0),
    )?;
    if !waiting {
        return Ok(None);
    }
    if checks >= limit {
        return Ok(Some(Settled::Retired));
    }
    conn.execute(
        "UPDATE server_outbox SET append_checks = ?2, retry_unix = 0,
             append_mime = NULL, error = ?3 WHERE id = ?1",
        params![entry.id, limit, error],
    )?;
    Ok(Some(Settled::Recovering))
}

pub(crate) fn cancel_unsent(conn: &Connection, account: &AccountId, draft: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM server_outbox WHERE account_id = ?1 AND draft_id = ?2
         AND kind = 'append_draft' AND state = 'pending' AND NOT sent",
        params![account.as_str(), draft],
    )?;
    Ok(())
}

pub(crate) fn discard(
    conn: &Connection,
    account: &AccountId,
    draft: &str,
    action: i64,
) -> Result<()> {
    if enabled(conn, account)? {
        return queue_delete_for(conn, account, draft, Some(action));
    }
    conn.execute(
        "INSERT INTO discarded_server_draft_copies
            (account_id, action_id, draft_id, mailbox, uidvalidity, uid, digest, message_id)
         SELECT account_id, ?3, draft_id, mailbox, uidvalidity, uid, digest, message_id
         FROM server_draft_copies WHERE account_id = ?1 AND draft_id = ?2",
        params![account.as_str(), draft, action],
    )?;
    unlink_copy(conn, account, draft)
}

fn unlink_copy(conn: &Connection, account: &AccountId, draft: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM server_draft_copies WHERE account_id = ?1 AND draft_id = ?2",
        params![account.as_str(), draft],
    )?;
    Ok(())
}

/// Undo keeps an untouched UID. Once deletion could have reached the server,
/// enqueue the exact restored local payload while the outbox row is retired.
pub(crate) fn undo_discard(
    store: &Store,
    conn: &Connection,
    account: &AccountId,
    action: i64,
    draft: &str,
) -> Result<()> {
    let copied = conn.execute(
        "INSERT OR IGNORE INTO server_draft_copies
            (account_id, draft_id, mailbox, uidvalidity, uid, digest, message_id)
         SELECT account_id, draft_id, mailbox, uidvalidity, uid, digest, message_id
         FROM discarded_server_draft_copies
         WHERE account_id = ?1 AND action_id = ?2 AND draft_id = ?3",
        params![account.as_str(), action, draft],
    )?;
    if copied != 0 {
        let copy = conn.query_row(
            "SELECT mailbox, uidvalidity, uid FROM server_draft_copies
             WHERE account_id = ?1 AND draft_id = ?2",
            params![account.as_str(), draft],
            |r| {
                Ok(Copy {
                    mailbox: r.get(0)?,
                    validity: r.get(1)?,
                    uid: r.get(2)?,
                })
            },
        )?;
        store.hide_server_draft_copy(conn, account, &copy)?;
    }
    conn.execute(
        "DELETE FROM discarded_server_draft_copies
         WHERE account_id = ?1 AND action_id = ?2 AND draft_id = ?3",
        params![account.as_str(), action, draft],
    )?;

    let dispatched: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM server_outbox WHERE account_id = ?1 AND draft_id = ?3
             AND action_id = ?2 AND kind = 'delete_draft'
             AND state IN ('done', 'pending') AND sent)",
        params![account.as_str(), action, draft],
        |r| r.get(0),
    )?;
    conn.execute(
        "DELETE FROM server_outbox WHERE account_id = ?1 AND draft_id = ?3 AND kind = 'delete_draft'
            AND state = 'pending' AND (action_id = ?2 OR action_id IS NULL)",
        params![account.as_str(), action, draft],
    )?;
    if dispatched
        && enabled(conn, account)?
        && let Some(mut restored) = store.load_compose(draft)?
    {
        let inline = crate::compose::inline_image_paths(&restored.body);
        for file in &mut restored.files {
            file.inline = inline.contains(&file.path);
        }
        let (digest, outgoing) = crate::compose::server_draft_payload(&restored, &restored.files);
        insert_append(
            conn,
            account,
            draft,
            &digest,
            outgoing.message_id,
            &outgoing.mime,
        )?;
    }
    Ok(())
}

fn release_copy(conn: &Connection, account: &AccountId, draft: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO server_draft_rescans (account_id, mailbox)
         SELECT account_id, mailbox FROM server_draft_copies
         WHERE account_id = ?1 AND draft_id = ?2",
        params![account.as_str(), draft],
    )?;
    unlink_copy(conn, account, draft)
}

pub(crate) fn rescan_mailboxes(conn: &Connection, account: &AccountId) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT mailbox FROM server_draft_rescans WHERE account_id = ?1
         UNION SELECT mailbox FROM discarded_server_draft_copies
            WHERE account_id = ?1 AND needs_rescan",
    )?;
    Ok(stmt
        .query_map([account.as_str()], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn consume_rescans(conn: &Connection, account: &AccountId) -> Result<()> {
    conn.execute(
        "DELETE FROM server_draft_rescans WHERE account_id = ?1",
        [account.as_str()],
    )?;
    conn.execute(
        "UPDATE discarded_server_draft_copies SET needs_rescan = 0 WHERE account_id = ?1 AND needs_rescan",
        [account.as_str()],
    )?;
    Ok(())
}

pub(crate) fn queue_delete(conn: &Connection, account: &AccountId, draft: &str) -> Result<()> {
    queue_delete_for(conn, account, draft, None)
}

fn queue_delete_for(
    conn: &Connection,
    account: &AccountId,
    draft: &str,
    action: Option<i64>,
) -> Result<()> {
    if !enabled(conn, account)? {
        return Ok(());
    }
    cancel_unsent(conn, account, draft)?;
    conn.execute(
        "INSERT INTO server_outbox (account_id, kind, state, message_id, source_folder,
             target_folder, generation, created_unix, draft_id, action_id)
         SELECT ?1, 'delete_draft', 'pending', '', ?3, '', ?4, ?5, ?2, ?6
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
            now_unix(),
            action,
        ],
    )?;
    Ok(())
}

pub(crate) struct Pending {
    pub current: Option<Copy>,
    pub current_message_id: Option<String>,
    pub placed: Option<Copy>,
    pub can_append: bool,
    pub supersedes_uncertain: bool,
}

pub(crate) fn pending(store: &Store, entry: &OutboxEntry) -> Result<Option<Pending>> {
    Ok(store.connection().query_row(
        "SELECT c.mailbox, c.uidvalidity, c.uid, o.target_mailbox, o.placed_uidvalidity, o.placed_uid,
                EXISTS (SELECT 1 FROM compose_drafts d WHERE d.account_id = o.account_id
                        AND d.id = o.draft_id AND d.discarded_action_id IS NULL)
                AND NOT EXISTS (SELECT 1 FROM compose_send s WHERE s.account_id = o.account_id
                        AND s.draft_id = o.draft_id AND s.state = 'sent'),
                coalesce(c.message_id, (SELECT p.message_id FROM server_outbox p
                    WHERE p.account_id = o.account_id AND p.draft_id = o.draft_id
                      AND p.kind = 'append_draft' AND p.target_mailbox = c.mailbox
                      AND p.placed_uidvalidity = c.uidvalidity AND p.placed_uid = c.uid
                    ORDER BY p.id DESC LIMIT 1)),
                coalesce((SELECT p.state = 'dropped' AND p.append_checks > 0
                    FROM server_outbox p
                    WHERE p.account_id = o.account_id AND p.draft_id = o.draft_id
                      AND p.generation = o.generation AND p.kind = 'append_draft'
                      AND p.id < o.id ORDER BY p.id DESC LIMIT 1), 0)
         FROM server_outbox o JOIN write_back_accounts a USING (account_id)
         LEFT JOIN server_draft_copies c ON c.account_id = o.account_id AND c.draft_id = o.draft_id
         WHERE o.id = ?1 AND a.enabled AND o.generation = a.generation AND o.state = 'pending'
           AND NOT EXISTS (SELECT 1 FROM server_outbox t WHERE t.account_id = o.account_id
               AND t.draft_id = o.draft_id AND t.generation = o.generation AND t.id < o.id
               AND t.state = 'pending')",
        [entry.id],
        |row| {
            let copy = |offset| -> rusqlite::Result<Option<Copy>> {
                let Some(uid) = row.get(offset + 2)? else { return Ok(None) };
                Ok(Some(Copy { mailbox: row.get(offset)?, validity: row.get(offset + 1)?, uid }))
            };
            Ok(Pending {
                current: copy(0)?,
                current_message_id: row.get(7)?,
                placed: copy(3)?,
                can_append: row.get(6)?,
                supersedes_uncertain: row.get(8)?,
            })
        },
    ).optional()?)
}

/// Record that a discard might reach the server before sending UID STORE.
pub(crate) fn begin_delete(store: &Store, entry: &OutboxEntry) -> Result<bool> {
    Ok(store.connection().execute(
        "UPDATE server_outbox SET sent = 1
         WHERE id = ?1 AND state = 'pending' AND generation IN
             (SELECT generation FROM write_back_accounts WHERE account_id = server_outbox.account_id AND enabled)",
        [entry.id],
    )? != 0)
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
            "INSERT INTO server_draft_copies (account_id, draft_id, mailbox, uidvalidity, uid, digest, message_id)
             SELECT account_id, draft_id, target_mailbox, placed_uidvalidity, placed_uid, draft_digest, message_id
             FROM server_outbox WHERE id = ?1
             ON CONFLICT(account_id, draft_id) DO UPDATE SET mailbox = excluded.mailbox,
                 uidvalidity = excluded.uidvalidity, uid = excluded.uid, digest = excluded.digest,
                 message_id = excluded.message_id",
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
    message_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO server_draft_copies (account_id, draft_id, mailbox, uidvalidity, uid, digest, message_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            account.as_str(),
            draft,
            copy.mailbox,
            copy.validity,
            copy.uid,
            digest,
            message_id,
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
