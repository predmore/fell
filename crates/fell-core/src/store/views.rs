//! Virtual mailbox membership uses the same server copies as body lookup.

use rusqlite::{Connection, params};

use crate::backend::{MailboxView, MailboxViewKind};
use crate::{AccountId, Message, MessageId, Result};

use super::{copies, find_stored, split_field};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mailbox_views (
    account_id TEXT NOT NULL,
    mailbox TEXT NOT NULL,
    folder TEXT NOT NULL,
    kind TEXT NOT NULL,
    PRIMARY KEY (account_id, mailbox)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_mailbox_views_kind ON mailbox_views(account_id, kind);

-- An undone action can restore All Mail before its server copy is back.
CREATE TABLE IF NOT EXISTS local_all_mail_restores (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    folder TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id, folder),
    FOREIGN KEY (account_id, message_id) REFERENCES messages(account_id, message_id) ON DELETE CASCADE
) WITHOUT ROWID;
";

/// Scope names for a message, using the caller's SQL alias for its row.
pub(crate) fn scopes(message: &str) -> String {
    format!(
        "SELECT group_concat(v.folder, char(30))
         FROM mailbox_views v WHERE v.account_id = {message}.account_id AND (
            (v.kind = 'starred' AND {message}.starred) OR
            (v.kind = 'all' AND (EXISTS (
                SELECT 1 FROM message_copies c WHERE c.account_id = {message}.account_id
                  AND c.message_id = {message}.message_id AND c.mailbox = v.mailbox
            ) OR EXISTS (
                SELECT 1 FROM local_all_mail_restores r WHERE r.account_id = {message}.account_id
                  AND r.message_id = {message}.message_id AND r.folder = v.folder
            )) AND NOT EXISTS (
                SELECT 1 FROM local_folder_tombstones t WHERE t.account_id = {message}.account_id
                  AND t.message_id = {message}.message_id AND t.folder = v.folder COLLATE NOCASE
            ))
         )"
    )
}

pub(crate) fn has_view_copy(message: &str) -> String {
    format!(
        "EXISTS (
            SELECT 1 FROM message_copies c JOIN mailbox_views v
              ON v.account_id = c.account_id AND v.mailbox = c.mailbox
            WHERE c.account_id = {message}.account_id AND c.message_id = {message}.message_id
         ) OR EXISTS (
            SELECT 1 FROM copy_orphans q
            WHERE q.account_id = {message}.account_id AND q.message_id = {message}.message_id
         ) OR EXISTS (
            SELECT 1 FROM local_all_mail_restores r
            WHERE r.account_id = {message}.account_id AND r.message_id = {message}.message_id
         )"
    )
}

pub(crate) fn scope_names(conn: &Connection, account_id: &AccountId) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT folder FROM mailbox_views WHERE account_id = ?1 AND kind IN ('all', 'starred')
         ORDER BY folder",
    )?;
    let rows = stmt.query_map([account_id.as_str()], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(super) fn move_exclusions(conn: &Connection, account: &AccountId) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT folder FROM mailbox_views WHERE account_id = ?1 ORDER BY folder",
    )?;
    let rows = stmt.query_map([account.as_str()], |row| row.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// A move from a view applies only to messages shown in that scope.
pub(crate) fn in_scope(
    conn: &Connection,
    account: &AccountId,
    message_id: &str,
    folder: &str,
) -> Result<bool> {
    let sql = format!(
        "SELECT EXISTS(SELECT 1 FROM messages m
         WHERE m.account_id = ?1 AND m.message_id = ?2
           AND instr(lower(char(30) || ({}) || char(30)), lower(char(30) || ?3 || char(30))) > 0)",
        scopes("m")
    );
    Ok(conn
        .prepare_cached(&sql)?
        .query_row(params![account.as_str(), message_id, folder], |row| {
            row.get(0)
        })?)
}

pub(crate) fn has_view(conn: &Connection, account_id: &AccountId, mailbox: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM mailbox_views WHERE account_id = ?1 AND mailbox = ?2)",
        )?
        .query_row(params![account_id.as_str(), mailbox], |row| row.get(0))?)
}

pub(crate) fn copy_folders(
    conn: &Connection,
    account: &AccountId,
    message_id: &str,
) -> Result<Vec<(String, bool)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT c.folder, v.mailbox IS NOT NULL
         FROM message_copies c LEFT JOIN mailbox_views v
           ON v.account_id = c.account_id AND v.mailbox = c.mailbox
         WHERE c.account_id = ?1 AND c.message_id = ?2",
    )?;
    let rows = stmt.query_map(params![account.as_str(), message_id], |row| {
        Ok((row.get(0)?, row.get(1)?))
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub(crate) fn restore_all(conn: &Connection, account: &AccountId, action_id: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO local_all_mail_restores (account_id, message_id, folder)
         SELECT t.account_id, m.message_id, v.folder FROM local_folder_tombstones t
         JOIN mailbox_views v ON v.account_id = t.account_id
           AND v.folder = t.folder COLLATE NOCASE AND v.kind = 'all'
         LEFT JOIN message_aliases a ON a.account_id = t.account_id AND a.message_id = t.message_id
         JOIN messages m ON m.account_id = t.account_id
           AND m.message_id = COALESCE(a.canonical_message_id, t.message_id)
         WHERE t.account_id = ?1 AND t.action_id = ?2
           AND NOT EXISTS (
               SELECT 1 FROM message_copies c WHERE c.account_id = m.account_id
                 AND c.message_id = m.message_id AND c.mailbox = v.mailbox
           )",
    )?
    .execute(params![account.as_str(), action_id])?;
    Ok(())
}

pub(super) fn retire_restore(
    conn: &Connection,
    account: &AccountId,
    message_id: &str,
    folder: &str,
) -> Result<()> {
    conn.prepare_cached(
        "DELETE FROM local_all_mail_restores WHERE account_id = ?1 AND message_id = ?2 AND folder = ?3",
    )?
    .execute(params![account.as_str(), message_id, folder])?;
    Ok(())
}

pub(super) fn is_bridge(conn: &Connection, account_id: &AccountId) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1 AND backend_kind = 'bridge')",
        )?
        .query_row([account_id.as_str()], |row| row.get(0))?)
}

pub(super) fn project_arrival(conn: &Connection, message: &mut Message) -> Result<()> {
    if let Some(copy) = crate::imap::parse_remote_id(&message.remote_id.0)
        && has_view(conn, &message.account_id, &copy.mailbox)?
    {
        message.folders.retain(|folder| folder != &copy.folder);
    }
    Ok(())
}

pub(super) fn refresh_message(conn: &Connection, account_id: &AccountId, id: &str) -> Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO thread_summary_dirty (account_id, thread_id)
         SELECT account_id, thread_id FROM messages WHERE account_id = ?1 AND message_id = ?2",
    )?
    .execute(params![account_id.as_str(), id])?;
    Ok(())
}

pub(super) fn replace(
    conn: &Connection,
    account_id: &AccountId,
    listed: &[MailboxView],
) -> Result<bool> {
    let account = account_id.as_str();
    let mut select = conn.prepare_cached(
        "SELECT mailbox, folder, kind FROM mailbox_views WHERE account_id = ?1 ORDER BY mailbox",
    )?;
    let before = select
        .query_map([account], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut after: Vec<_> = listed
        .iter()
        .map(|view| {
            let kind = match view.kind {
                MailboxViewKind::All => "all",
                MailboxViewKind::Starred => "starred",
                MailboxViewKind::Label(_) => "label",
            };
            (view.mailbox.as_str(), view.folder.as_str(), kind)
        })
        .collect();
    after.sort_unstable();
    if before
        .iter()
        .map(|(m, f, k)| (m.as_str(), f.as_str(), k.as_str()))
        .eq(after.iter().copied())
    {
        return Ok(false);
    }
    for (mailbox, _, _) in &before {
        if after.iter().any(|(name, _, _)| *name == mailbox) {
            continue;
        }
        let mut validities = conn.prepare_cached(
            "SELECT DISTINCT uidvalidity FROM message_copies WHERE account_id = ?1 AND mailbox = ?2",
        )?;
        let validities = validities
            .query_map(params![account, mailbox], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for uidvalidity in validities {
            copies::drop_copy(conn, account_id, mailbox, uidvalidity, 1, u32::MAX)?;
        }
    }
    conn.execute("DELETE FROM mailbox_views WHERE account_id = ?1", [account])?;
    for (mailbox, folder, kind) in &after {
        conn.prepare_cached(
            "INSERT INTO mailbox_views (account_id, mailbox, folder, kind) VALUES (?1, ?2, ?3, ?4)",
        )?
        .execute(params![account, mailbox, folder, kind])?;
    }
    let gone = "r.account_id = ?1 AND NOT EXISTS (
        SELECT 1 FROM mailbox_views v
        WHERE v.account_id = r.account_id AND v.kind = 'all' AND v.folder = r.folder
    )";
    conn.execute(
        &format!(
            "INSERT OR IGNORE INTO copy_orphans (account_id, message_id, folder)
             SELECT r.account_id, r.message_id, r.folder FROM local_all_mail_restores r WHERE {gone}"
        ),
        [account],
    )?;
    conn.execute(
        &format!("DELETE FROM local_all_mail_restores AS r WHERE {gone}"),
        [account],
    )?;
    // On upgrade, these copies were stored as folders. Remove those folder
    // names from affected rows and refresh the virtual scopes for them.
    let mut messages = conn.prepare_cached(
        "SELECT m.message_id FROM messages m WHERE m.account_id = ?1 AND (
            m.starred OR EXISTS (
                SELECT 1 FROM mailbox_views v WHERE v.account_id = m.account_id
                  AND instr(char(30) || m.folders || char(30), char(30) || v.folder || char(30)) > 0
            )
        )",
    )?;
    let messages = messages
        .query_map([account], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for id in messages {
        refresh_message(conn, account_id, &id)?;
        if let Some((stored, _)) = find_stored(conn, account_id, &MessageId(id))? {
            let mut folders = split_field(&stored.folders);
            folders.retain(|folder| !after.iter().any(|(_, name, _)| *name == folder));
            copies::write_folders(conn, account_id, &stored, &folders)?;
        }
    }
    Ok(true)
}
