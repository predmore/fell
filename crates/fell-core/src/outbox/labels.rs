//! Label edits share the outbox's ordering, generation, and retry policy.
//! A keyword row targets one copy. A Bridge row targets one message's
//! membership in a label mailbox, whose copy may only exist after replay.
use rusqlite::{Connection, OptionalExtension, params};

use crate::account::AccountId;
use crate::imap::{escape_mailbox, unescape_mailbox};
use crate::store::{Store, join_tokens, keywords::LabelEdit, split_field};
use crate::{Error, Result};

use super::{ENTRY_COLS, OutboxEntry, OutboxKind, enabled, generation, now_unix, select_entries};

pub(crate) type LabelCopy = (String, String, u32, u32);

pub(crate) fn queue(
    conn: &Connection,
    account: &AccountId,
    action_id: i64,
    message: &str,
    edit: LabelEdit<'_>,
) -> Result<()> {
    if !enabled(conn, account)? {
        return Ok(());
    }
    let label = edit.label();
    let backend: String = conn.query_row(
        "SELECT backend_kind FROM accounts WHERE id = ?1",
        [account.as_str()],
        |row| row.get(0),
    )?;
    let (kind, target) = match backend.as_str() {
        "bridge" if bridge_label_name(label) => {
            (OutboxKind::BridgeLabel, format!("Labels/{label}"))
        }
        "imap" if keyword_atom(label) => (OutboxKind::Keyword, label.to_owned()),
        // A local label may have a name that the account cannot represent.
        _ => return Ok(()),
    };
    let value = matches!(edit, LabelEdit::Added(_));
    let generation = generation(conn, account)?;
    let copy_rows = if kind == OutboxKind::Keyword {
        copies(conn, account, message)?
    } else {
        Vec::new()
    };
    let mut queue = conn.prepare_cached(
        "INSERT INTO server_outbox
         (account_id, kind, state, action_id, message_id, source_folder, source_mailbox,
          uidvalidity, uid, target_folder, created_unix, generation, flag_value)
         VALUES (?1, ?2, 'pending', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for copy in copy_rows
        .iter()
        .map(Some)
        .chain(copy_rows.is_empty().then_some(None))
    {
        queue.execute(params![
            account.as_str(),
            kind.as_str(),
            action_id,
            message,
            copy.map_or("".into(), |c| c.0.clone()),
            copy.map(|c| &c.1),
            copy.map(|c| c.2),
            copy.map(|c| c.3),
            target,
            now_unix(),
            generation,
            value,
        ])?;
    }
    Ok(())
}

pub(crate) fn keyword_atom(label: &str) -> bool {
    !label.is_empty()
        && label
            .bytes()
            .all(|ch| ch.is_ascii_graphic() && !b"\\(){%*\"]".contains(&ch))
        && !crate::imap::is_reserved_keyword(label)
}

pub(crate) fn bridge_label_name(label: &str) -> bool {
    !label.is_empty()
        && !label.chars().any(char::is_control)
        && !label.split('/').any(|part| matches!(part, "." | ".."))
}

/// Label writes hold the visible local value until replay settles, even
/// after a STORE or COPY has been sent and its reply is still unknown.
pub(crate) fn unsettled_labels(
    conn: &Connection,
    account: &AccountId,
    message: &str,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT CASE kind WHEN 'bridge_label' THEN substr(target_folder, 8) ELSE target_folder END
         FROM server_outbox
         WHERE account_id = ?1 AND message_id = ?2
           AND kind IN ('keyword', 'bridge_label') AND state = 'pending'
           AND generation = (SELECT generation FROM write_back_accounts WHERE account_id = ?1)",
    )?;
    let rows = stmt.query_map(params![account.as_str(), message], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn copies(
    conn: &Connection,
    account: &AccountId,
    message: &str,
) -> Result<Vec<LabelCopy>> {
    let mut stmt = conn.prepare_cached(
        "SELECT folder, mailbox, uidvalidity, uid FROM message_copies
         WHERE account_id = ?1 AND message_id = ?2
         ORDER BY CASE WHEN folder = 'INBOX' COLLATE NOCASE THEN 0 ELSE 1 END,
                  folder, uidvalidity DESC, uid DESC",
    )?;
    let rows = stmt.query_map(params![account.as_str(), message], |row| {
        Ok((
            row.get(0)?,
            row.get::<_, String>(1)?,
            row.get(2)?,
            row.get(3)?,
        ))
    })?;
    rows.map(|row| {
        let (folder, raw, validity, uid) = row?;
        let raw =
            unescape_mailbox(&raw).ok_or_else(|| Error::Action("invalid label mailbox".into()))?;
        Ok((folder, raw, validity, uid))
    })
    .collect()
}

/// Expand early keyword edits before replay takes its queue snapshot. Keep
/// the original row's position for every copy so later edits and moves
/// cannot overtake part of the write.
pub(crate) fn bind_keyword_copies(
    conn: &Connection,
    account: &AccountId,
    generation: i64,
) -> Result<()> {
    let mut waiting = conn.prepare_cached(
        "SELECT id, message_id FROM server_outbox
         WHERE account_id = ?1 AND generation = ?2 AND kind = 'keyword' AND state = 'pending'
           AND source_mailbox IS NULL AND uidvalidity IS NULL AND uid IS NULL AND sent = 0",
    )?;
    let waiting = waiting
        .query_map(params![account.as_str(), generation], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if waiting.is_empty() {
        return Ok(());
    }
    let mut bind = conn.prepare_cached(
        "UPDATE server_outbox
         SET source_folder = ?2, source_mailbox = ?3, uidvalidity = ?4, uid = ?5,
             attempts = 0, retry_unix = 0, error = ''
         WHERE id = ?1",
    )?;
    let mut add = conn.prepare_cached(
        "INSERT INTO server_outbox
            (account_id, kind, state, action_id, message_id, source_folder, source_mailbox,
             uidvalidity, uid, target_folder, created_unix, generation, flag_value, keyword_parent)
         SELECT account_id, kind, state, action_id, message_id, ?2, ?3, ?4, ?5,
                target_folder, created_unix, generation, flag_value, id
         FROM server_outbox WHERE id = ?1",
    )?;
    for (id, message) in waiting {
        let copies = copies(conn, account, &message)?;
        let Some(((folder, raw, validity, uid), rest)) = copies.split_first() else {
            continue;
        };
        bind.execute(params![id, folder, raw, validity, uid])?;
        for (folder, raw, validity, uid) in rest {
            add.execute(params![id, folder, raw, validity, uid])?;
        }
    }
    Ok(())
}

// Local and agent logs have separate counters. Agent action IDs use the
// negative half of the outbox's action_id; MOVE only uses local IDs.
pub(crate) fn undo(
    conn: &Connection,
    account: &AccountId,
    action_id: i64,
    message: &str,
) -> Result<bool> {
    let replayed = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM server_outbox
         WHERE account_id = ?1 AND action_id = ?2 AND message_id = ?3
           AND kind IN ('keyword', 'bridge_label') AND state = 'done' AND sent = 1)",
        params![account.as_str(), action_id, message],
        |row| row.get::<_, bool>(0),
    )?;
    if !enabled(conn, account)? {
        return Ok(replayed);
    }
    let generation = generation(conn, account)?;
    let scope = params![account.as_str(), action_id, message, generation];
    let mut undoable = conn.execute(
        "INSERT INTO server_outbox
          (account_id, kind, state, message_id, source_folder, source_mailbox, uidvalidity, uid,
           target_folder, created_unix, generation, flag_value, reverses)
         SELECT account_id, kind, 'pending', message_id, source_folder, source_mailbox, uidvalidity, uid,
                target_folder, unixepoch(), generation, NOT flag_value, id
         FROM server_outbox WHERE account_id = ?1 AND action_id = ?2 AND message_id = ?3
           AND generation = ?4 AND kind IN ('keyword', 'bridge_label')
           AND state IN ('pending', 'done') AND sent = 1
         ORDER BY id",
        scope,
    )? != 0;
    undoable |= conn.execute(
        "DELETE FROM server_outbox WHERE account_id = ?1 AND action_id = ?2 AND message_id = ?3
           AND generation = ?4 AND kind IN ('keyword', 'bridge_label')
           AND state = 'pending' AND sent = 0",
        scope,
    )? != 0;
    Ok(undoable || replayed)
}

pub(crate) fn pending(store: &Store, account: &AccountId, id: i64) -> Result<Option<OutboxEntry>> {
    if !enabled(store.connection(), account)? {
        return Ok(None);
    }
    let generation = generation(store.connection(), account)?;
    Ok(select_entries(
        store.connection(),
        &format!(
            "SELECT {ENTRY_COLS} FROM server_outbox
             WHERE account_id = ?1 AND id = {id} AND state = 'pending' AND generation = {generation}"
        ),
        account,
    )?.pop())
}

pub(crate) fn settled_original(store: &Store, id: i64) -> Result<Option<bool>> {
    let state: Option<String> = store
        .connection()
        .query_row(
            "SELECT state FROM server_outbox WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match state.as_deref() {
        Some("pending") => None,
        Some("done") => Some(true),
        _ => Some(false),
    })
}

pub(crate) enum Applied<'a> {
    Unchanged,
    Keyword(&'a LabelCopy),
    Copied(&'a str, u32, u32),
    Removed(&'a LabelCopy),
}

pub(crate) fn finish(
    store: &Store,
    account: &AccountId,
    entry: &OutboxEntry,
    applied: Applied<'_>,
) -> Result<()> {
    let tx = crate::store::immediate_transaction(store.connection())?;
    if !enabled(&tx, account)? || entry.generation != generation(&tx, account)? {
        return Ok(());
    }
    match applied {
        Applied::Unchanged => {}
        Applied::Keyword((_, raw, validity, uid)) => {
            let mailbox = escape_mailbox(raw);
            let before: Option<String> = tx
                .query_row(
                    "SELECT keywords FROM message_copies
                 WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
                    params![account.as_str(), mailbox, validity, uid],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(before) = before {
                let mut keywords = split_field(&before);
                keywords.retain(|flag| !flag.eq_ignore_ascii_case(&entry.target_folder));
                if entry.flag_value == Some(true) {
                    keywords.push(entry.target_folder.clone());
                }
                // Replay owns only this keyword. Sync will keep the other flags
                // and reconcile future changes from complete FLAGS replies.
                tx.execute(
                    "UPDATE message_copies SET keywords = ?5
                     WHERE account_id = ?1 AND mailbox = ?2 AND uidvalidity = ?3 AND uid = ?4",
                    params![
                        account.as_str(),
                        mailbox,
                        validity,
                        uid,
                        join_tokens(&keywords)
                    ],
                )?;
            }
        }
        Applied::Copied(raw, validity, uid) => {
            tx.execute(
                "INSERT OR IGNORE INTO message_copies (account_id, mailbox, uidvalidity, uid, message_id, folder, keywords)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![account.as_str(), escape_mailbox(raw), validity, uid, entry.message_id,
                    entry.target_folder, entry.target_folder.strip_prefix("Labels/").unwrap_or_default()],
            )?;
        }
        Applied::Removed((_, raw, validity, uid)) => {
            tx.execute(
                "DELETE FROM message_copies
                 WHERE account_id = ?1 AND message_id = ?2 AND mailbox = ?3 AND uidvalidity = ?4 AND uid = ?5",
                params![account.as_str(), entry.message_id, escape_mailbox(raw), validity, uid],
            )?;
            // Other copies in the same label mailbox can still need removal.
            tx.commit()?;
            return Ok(());
        }
    };
    tx.execute(
        "UPDATE server_outbox SET state = 'done', settled_unix = ?2, error = ''
         WHERE id = ?1 AND state = 'pending'",
        params![entry.id, now_unix()],
    )?;
    tx.commit()
}
