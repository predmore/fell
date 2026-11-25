//! Local thread visibility. Folder membership and server copies remain intact.

use rusqlite::{Connection, OptionalExtension, params};

use crate::actions::{ActionKind, FOLDER_INBOX, ensure_action_schema, insert_local_action};
use crate::{AccountId, Error, Result, Store, ThreadId};

/// Internal list keys, distinct from server folder names and absent from the folder picker.
pub const SNOOZED: &str = "\x1fsnoozed";
pub const MUTED: &str = "\x1fmuted";
pub(super) const INBOX_KEY: &str = "inbox";

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS muted_thread_messages (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
) WITHOUT ROWID;

CREATE TABLE IF NOT EXISTS snooze_returns (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    returned_at INTEGER NOT NULL,
    PRIMARY KEY (account_id, message_id)
) WITHOUT ROWID;

-- Per-message proof that a NULL snooze came from the timer. One event
-- can clear several messages, with at most one Inbox return.
CREATE TABLE IF NOT EXISTS snooze_wakes (
    account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    message_id TEXT NOT NULL,
    until_unix INTEGER NOT NULL,
    event INTEGER NOT NULL,
    return_id TEXT,
    returned_at INTEGER NOT NULL,
    set_unread INTEGER NOT NULL,
    PRIMARY KEY (account_id, message_id)
) WITHOUT ROWID;
CREATE INDEX IF NOT EXISTS idx_snooze_wakes_return ON snooze_wakes(account_id, return_id, event);

CREATE TRIGGER IF NOT EXISTS snooze_wake_changed AFTER UPDATE OF snooze_until ON messages
WHEN old.snooze_until IS NOT new.snooze_until BEGIN
    DELETE FROM snooze_wakes WHERE account_id = old.account_id AND message_id = old.message_id;
END;

CREATE TRIGGER IF NOT EXISTS snooze_wake_read_changed AFTER UPDATE OF unread ON messages
WHEN old.unread IS NOT new.unread BEGIN
    UPDATE snooze_wakes SET set_unread = 0
    WHERE account_id = old.account_id AND return_id = old.message_id AND set_unread = 1;
END;

CREATE TABLE IF NOT EXISTS snooze_revision (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL
);
INSERT OR IGNORE INTO snooze_revision VALUES (1, 0);

CREATE TRIGGER IF NOT EXISTS snooze_revision_update AFTER UPDATE OF snooze_until ON messages
WHEN old.snooze_until IS NOT new.snooze_until BEGIN
    UPDATE snooze_revision SET revision = revision + 1 WHERE id = 1;
END;

CREATE INDEX IF NOT EXISTS idx_message_aliases_canonical
    ON message_aliases(account_id, canonical_message_id);
";

// Resolve the current thread through both canonical Message-IDs and copy aliases.
const ANCHORS: &str = "SELECT message_id FROM messages WHERE account_id = ?1 AND thread_id = ?2
    UNION ALL
    SELECT a.message_id FROM messages m JOIN message_aliases a
      ON a.account_id = m.account_id AND a.canonical_message_id = m.message_id
    WHERE m.account_id = ?1 AND m.thread_id = ?2";

pub(super) fn muted(conn: &Connection, account: &str, thread: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT EXISTS (SELECT 1 FROM ({ANCHORS}) ids CROSS JOIN muted_thread_messages marks
           ON marks.account_id = ?1 AND marks.message_id = ids.message_id)"
        ))?
        .query_row(params![account, thread], |row| row.get(0))?)
}

pub(super) fn returned_at(conn: &Connection, account: &str, thread: &str) -> Result<Option<i64>> {
    Ok(conn
        .prepare_cached(&format!(
            "SELECT max(r.returned_at) FROM ({ANCHORS}) ids CROSS JOIN snooze_returns r
           ON r.account_id = ?1 AND r.message_id = ids.message_id"
        ))?
        .query_row(params![account, thread], |row| row.get(0))?)
}

impl Store {
    /// Apply one undoable snooze edit to a selection. A missing time clears it.
    pub fn set_thread_snoozes(
        &self,
        account: &AccountId,
        threads: &[ThreadId],
        until: Option<i64>,
    ) -> Result<usize> {
        self.require_account(account)?;
        let tx = super::immediate_transaction(&self.conn)?;
        ensure_action_schema(&tx)?;
        let (kind, summary) = if until.is_some() {
            (ActionKind::Snooze, "Snoozed")
        } else {
            (ActionKind::Unsnooze, "Unsnoozed")
        };
        let mut recorded = Vec::new();
        for thread in threads {
            let action = match until {
                Some(until) => crate::agent::snooze_in(&tx, account, thread, until, "user"),
                None => crate::agent::unsnooze_in(&tx, account, thread, "user"),
            }?;
            if let Some(action) = action {
                recorded.push(action.id);
            }
        }
        if !recorded.is_empty() {
            let action = insert_local_action(&tx, account, kind, summary)?;
            for id in &recorded {
                tx.execute(
                    "INSERT INTO local_snooze_actions VALUES (?1, ?2)",
                    params![action.id, id],
                )?;
            }
        }
        tx.commit()?;
        Ok(recorded.len())
    }

    /// Toggle the whole selection. A mixed selection is muted together.
    pub fn toggle_thread_mutes(&self, account: &AccountId, threads: &[ThreadId]) -> Result<bool> {
        self.require_account(account)?;
        let tx = super::immediate_transaction(&self.conn)?;
        ensure_action_schema(&tx)?;
        let account_key = account.as_str();
        let mut targets = Vec::new();
        for thread in threads {
            let mut stmt = tx.prepare_cached(ANCHORS)?;
            let ids = stmt
                .query_map(params![account_key, thread.as_str()], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            if ids.is_empty() {
                return Err(Error::UnknownThread);
            }
            targets.push((thread, ids, muted(&tx, account_key, thread.as_str())?));
        }
        let mute = targets.iter().any(|(_, _, muted)| !muted);
        let (kind, summary) = if mute {
            (ActionKind::Mute, "Muted")
        } else {
            (ActionKind::Unmute, "Unmuted")
        };
        if !targets.is_empty() {
            let action = insert_local_action(&tx, account, kind, summary)?;
            for (thread, ids, was_muted) in targets {
                if mute == was_muted {
                    continue;
                }
                for id in ids {
                    if write_mute(&tx, account_key, &id, mute)? {
                        tx.execute(
                            "INSERT INTO local_mute_undo VALUES (?1, ?2)",
                            params![action.id, id],
                        )?;
                    }
                }
                dirty(&tx, account_key, thread.as_str())?;
            }
        }
        tx.commit()?;
        Ok(mute)
    }

    /// Clear every due snooze, restoring the Inbox row and its unread state
    /// atomically. `now` is wall time so this also catches up after suspend.
    pub fn wake_snoozes(&self, now: i64) -> Result<()> {
        const DUE: &str = "SELECT account_id, thread_id FROM thread_folders
                           WHERE folder_key = char(31) || 'snoozed' AND last_date >= ?1";
        let since = now.saturating_neg();
        if !self.conn.prepare_cached(DUE)?.exists([since])? {
            return Ok(());
        }
        let tx = super::immediate_transaction(&self.conn)?;
        let due = tx
            .prepare_cached(DUE)?
            .query_map([since], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (account, thread) in due {
            let snoozed = tx
                .prepare_cached(
                    "SELECT message_id, snooze_until FROM messages
                 WHERE account_id = ?1 AND thread_id = ?2 AND snooze_until IS NOT NULL",
                )?
                .query_map(params![account, thread], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let inbox = inbox_for_wake(&tx, &account, &thread)?;
            tx.execute(
                "UPDATE messages SET snooze_until = NULL
                 WHERE account_id = ?1 AND thread_id = ?2 AND snooze_until IS NOT NULL",
                params![account, thread],
            )?;
            if let Some((id, unread)) = &inbox {
                tx.execute(
                    "INSERT INTO snooze_returns VALUES (?1, ?2, ?3)
                     ON CONFLICT(account_id, message_id) DO UPDATE SET returned_at = excluded.returned_at",
                    params![account, id, now],
                )?;
                if !unread {
                    tx.execute(
                        "UPDATE messages SET unread = 1 WHERE account_id = ?1 AND message_id = ?2",
                        params![account, id],
                    )?;
                    // Keep the returned mail unread across sync without writing a server flag.
                    super::intents::remember(
                        &tx,
                        &AccountId(account.clone()),
                        id,
                        Some(true),
                        None,
                    )?;
                }
            }
            let mut record = tx.prepare_cached(
                "INSERT INTO snooze_wakes
                 SELECT ?1, ?2, ?3, revision, ?4, ?5, ?6 FROM snooze_revision WHERE id = 1",
            )?;
            for (id, until) in snoozed {
                record.execute(params![
                    account,
                    id,
                    until,
                    inbox.as_ref().map(|(id, _)| id),
                    now,
                    inbox.as_ref().is_some_and(|(_, unread)| !unread)
                ])?;
            }
        }
        tx.commit()
    }

    /// Local notification mute state, including Message-ID aliases.
    pub fn thread_muted(&self, account: &AccountId, thread: &ThreadId) -> Result<bool> {
        muted(&self.conn, account.as_str(), thread.as_str())
    }

    /// Changes made by another process (including fell-mcp) also advance this.
    pub fn snooze_revision(&self) -> Result<i64> {
        Ok(self
            .conn
            .prepare_cached("SELECT revision FROM snooze_revision WHERE id = 1")?
            .query_row([], |row| row.get(0))?)
    }
}

fn inbox_for_wake(
    conn: &Connection,
    account: &str,
    thread: &str,
) -> Result<Option<(String, bool)>> {
    let inbox = super::roles::listed(conn, &AccountId(account.to_owned()))?;
    let inbox = inbox
        .as_ref()
        .map_or(FOLDER_INBOX, |roles| roles.name(crate::FolderRole::Inbox));
    Ok(conn.prepare_cached(&format!(
            "SELECT message_id, unread FROM messages m
             WHERE account_id = ?1 AND thread_id = ?2 AND (
                instr(char(30) || lower(folders) || char(30), char(30) || 'inbox' || char(30)) > 0
                OR instr(char(30) || lower(folders) || char(30), char(30) || lower(?3) || char(30)) > 0
                OR (folders = '' AND NOT archived AND NOT ({}))
             ) ORDER BY date_unix DESC, rowid DESC LIMIT 1",
            super::views::has_view_copy("m"),
        ))?.query_row(params![account, thread, inbox], |row| Ok((row.get(0)?, row.get(1)?))).optional()?)
}

pub(crate) fn undo_snooze(
    conn: &Connection,
    account: &AccountId,
    action: i64,
) -> Result<(Vec<ThreadId>, Option<String>)> {
    let actions = conn.prepare_cached(
        "SELECT agent_action_id FROM local_snooze_actions WHERE action_id = ?1 ORDER BY agent_action_id DESC"
    )?.query_map([action], |row| row.get::<_, i64>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let total = actions.len();
    let mut changed = 0;
    let mut restored = Vec::new();
    for id in actions {
        match crate::agent::undo_batch_snooze(conn, account, id)? {
            crate::agent::UndoEffect::SnoozeChanged => changed += 1,
            crate::agent::UndoEffect::Restored => {
                let mut stmt = conn.prepare_cached(
                    "SELECT DISTINCT m.thread_id FROM agent_action_messages s
                     LEFT JOIN message_aliases a ON a.account_id = ?2 AND a.message_id = s.message_id
                     JOIN messages m ON m.account_id = ?2 AND m.message_id = COALESCE(a.canonical_message_id, s.message_id)
                     WHERE s.action_id = ?1",
                )?;
                for thread in stmt.query_map(params![id, account.as_str()], |row| {
                    row.get(0).map(ThreadId)
                })? {
                    let thread = thread?;
                    if !restored.contains(&thread) {
                        restored.push(thread);
                    }
                }
            }
        }
    }
    let notice = match changed {
        0 => None,
        n if n == total => Some(crate::agent::SNOOZE_CHANGED_NOTICE.into()),
        n => Some(format!(
            "Undid {} of {total} · {n} {} changed since",
            total - n,
            if n == 1 { "snooze" } else { "snoozes" },
        )),
    };
    Ok((restored, notice))
}

/// Roll back the timer side effects of this wake. A later local read edit
/// takes over the unread state, while the return's ranking is still ours.
pub(crate) fn undo_wake(conn: &Connection, account: &AccountId, event: i64) -> Result<()> {
    let key = account.as_str();
    let returned: Option<(String, i64, bool)> = conn
        .prepare_cached(
            "SELECT return_id, returned_at, set_unread FROM snooze_wakes w
         WHERE account_id = ?1 AND event = ?2 AND return_id IS NOT NULL
           AND NOT EXISTS (
              SELECT 1 FROM snooze_wakes later
              WHERE later.account_id = w.account_id AND later.return_id = w.return_id
                AND later.event > w.event
           ) LIMIT 1",
        )?
        .query_row(params![key, event], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .optional()?;
    if let Some((id, at, set_unread)) = returned {
        let changed = conn.execute(
            "DELETE FROM snooze_returns WHERE account_id = ?1 AND message_id = ?2 AND returned_at = ?3",
            params![key, id, at],
        )?;
        if changed != 0 {
            let live = crate::store::canonical_id_on(conn, account, &crate::MessageId(id))?;
            if let Some(live) = live {
                if set_unread {
                    conn.execute(
                        "UPDATE messages SET unread = 0 WHERE account_id = ?1 AND message_id = ?2 AND unread = 1",
                        params![key, live],
                    )?;
                    super::intents::forget_wake_unread(conn, account, &live)?;
                }
                super::views::refresh_message(conn, account, &live)?;
            }
        }
    }
    conn.execute(
        "DELETE FROM snooze_wakes WHERE account_id = ?1 AND event = ?2",
        params![key, event],
    )?;
    Ok(())
}

pub(crate) fn undo_mute(
    conn: &Connection,
    account: &AccountId,
    action: i64,
    mute: bool,
) -> Result<Vec<ThreadId>> {
    let mut saved =
        conn.prepare_cached("SELECT message_id FROM local_mute_undo WHERE action_id = ?1")?;
    let mut current = conn.prepare_cached(
        "SELECT m.thread_id FROM messages m WHERE m.account_id = ?1 AND m.message_id = ?2
         UNION
         SELECT m.thread_id FROM message_aliases a JOIN messages m
            ON m.account_id = a.account_id AND m.message_id = a.canonical_message_id
         WHERE a.account_id = ?1 AND a.message_id = ?2",
    )?;
    let mut restored = Vec::new();
    for id in saved.query_map([action], |row| row.get::<_, String>(0))? {
        let id = id?;
        write_mute(conn, account.as_str(), &id, mute)?;
        for thread in
            current.query_map(params![account.as_str(), id], |row| row.get::<_, String>(0))?
        {
            let thread = ThreadId(thread?);
            if !restored.contains(&thread) {
                dirty(conn, account.as_str(), thread.as_str())?;
                restored.push(thread);
            }
        }
    }
    conn.execute("DELETE FROM local_mute_undo WHERE action_id = ?1", [action])?;
    Ok(restored)
}

fn write_mute(conn: &Connection, account: &str, id: &str, mute: bool) -> Result<bool> {
    let sql = if mute {
        "INSERT OR IGNORE INTO muted_thread_messages VALUES (?1, ?2)"
    } else {
        "DELETE FROM muted_thread_messages WHERE account_id = ?1 AND message_id = ?2"
    };
    Ok(conn.prepare_cached(sql)?.execute(params![account, id])? != 0)
}

fn dirty(conn: &Connection, account: &str, thread: &str) -> Result<()> {
    conn.prepare_cached("INSERT OR IGNORE INTO thread_summary_dirty VALUES (?1, ?2)")?
        .execute(params![account, thread])?;
    Ok(())
}
