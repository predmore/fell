//! A manual edit pins both sides of a split, or every member of a merge.
//! Automatic edges can add mail to a pinned group, but cannot unite two of
//! them or rename the destination. Undo restores the previous assignments.

use std::collections::{BTreeSet, HashMap, HashSet};

use rusqlite::{Connection, params};

use super::{direct_parent, rows_in_thread, stored_parent};
use crate::actions::{ActionKind, LocalAction, ensure_action_schema, insert_local_action};
use crate::store::{ListKey, Store, immediate_transaction};
use crate::{AccountId, Error, MessageId, Result, ThreadId};

pub(crate) const ACTION_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS local_action_thread_members (
    action_id INTEGER NOT NULL REFERENCES local_actions(id),
    message_id TEXT NOT NULL,
    before_thread TEXT NOT NULL,
    before_override TEXT,
    after_thread TEXT NOT NULL,
    PRIMARY KEY (action_id, message_id)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS local_action_thread_notes (
    action_id INTEGER NOT NULL REFERENCES local_actions(id),
    note_id INTEGER NOT NULL,
    before_thread TEXT NOT NULL,
    PRIMARY KEY (action_id, note_id)
) WITHOUT ROWID;
";

impl Store {
    /// Recent account-wide merge destinations, filtered by subject or sender.
    pub fn merge_candidates(
        &self,
        account: &AccountId,
        sources: &[String],
        query: &str,
    ) -> Result<Vec<(ThreadId, String, String, i64)>> {
        let words: Vec<_> = query.split_whitespace().take(20).collect();
        let mut stmt = self.conn.prepare_cached(
            "SELECT thread_id, subject, COALESCE(NULLIF(latest_name, ''), latest_email), last_date
             FROM thread_summary WHERE account_id = ?1
               AND thread_id NOT IN (SELECT value FROM json_each(?2))
               AND NOT EXISTS (
                   SELECT 1 FROM json_each(?3)
                   WHERE NOT instr(lower(subject || ' ' || latest_email || ' ' || COALESCE(latest_name, '')),
                                   lower(value))
               )
             ORDER BY last_date DESC, thread_id LIMIT 32",
        )?;
        Ok(stmt
            .query_map(
                params![
                    account.as_str(),
                    serde_json::json!(sources).to_string(),
                    serde_json::json!(words).to_string()
                ],
                |row| Ok((ThreadId(row.get(0)?), row.get(1)?, row.get(2)?, row.get(3)?)),
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Locate the edited conversation in the current list.
    pub fn threading_list_key(
        &self,
        account: &AccountId,
        thread: &ThreadId,
        folder: &str,
        label: Option<&str>,
    ) -> Result<Option<ListKey>> {
        use rusqlite::OptionalExtension;
        if let Some(label) = label {
            Ok(self.conn.prepare_cached(
                "SELECT last_date FROM thread_labels WHERE account_id = ?1 AND thread_id = ?2 AND label = ?3",
            )?.query_row(params![account.as_str(), thread.as_str(), label], |row| {
                Ok(ListKey { date_unix: row.get(0)?, thread_id: thread.clone() })
            }).optional()?)
        } else {
            self.folder_key_of(account, folder, thread)
        }
    }

    /// Detach a message and its reply descendants within this conversation.
    pub fn split_thread(
        &self,
        account: &AccountId,
        thread: &ThreadId,
        root: &MessageId,
    ) -> Result<ThreadId> {
        let tx = immediate_transaction(self.connection())?;
        self.rethread_written(&tx, account)?;
        let rows = rows_in_thread(&tx, account, thread)?;
        if !rows.iter().any(|row| row.headers.message_id == *root) {
            return Err(Error::UnknownThread);
        }

        let mut children: HashMap<_, Vec<_>> = HashMap::new();
        for row in &rows {
            if let Some(parent) =
                direct_parent(&tx, account.as_str(), row)?.filter(|parent| parent.thread == *thread)
            {
                children
                    .entry(parent.id)
                    .or_default()
                    .push(&row.headers.message_id);
            }
        }
        let mut branch = HashSet::new();
        let mut todo = vec![root];
        while let Some(id) = todo.pop() {
            if branch.insert(id)
                && let Some(replies) = children.get(id)
            {
                todo.extend(replies);
            }
        }
        if branch.len() == rows.len() {
            return Err(Error::Action(
                "This message and its replies already make up the conversation".into(),
            ));
        }

        ensure_action_schema(&tx)?;
        let action =
            insert_local_action(&tx, account, ActionKind::SplitThread, "Conversation split")?;
        let detached = ThreadId(format!("split:{}", action.id));
        for row in &rows {
            let id = &row.headers.message_id;
            let onto = if branch.contains(id) {
                &detached
            } else {
                thread
            };
            assign(&tx, account, action.id, id, thread, onto)?;
        }
        self.rethread_written(&tx, account)?;
        tx.commit()?;
        Ok(detached)
    }

    /// Merge the complete selection into the destination conversation.
    pub fn merge_threads(
        &self,
        account: &AccountId,
        sources: &[ThreadId],
        target: &ThreadId,
    ) -> Result<()> {
        let sources: BTreeSet<_> = sources
            .iter()
            .filter(|id| *id != target)
            .map(ThreadId::as_str)
            .collect();
        if sources.is_empty() {
            return Err(Error::Action("Choose another conversation".into()));
        }
        let tx = immediate_transaction(self.connection())?;
        self.rethread_written(&tx, account)?;
        ensure_action_schema(&tx)?;
        let action = insert_local_action(
            &tx,
            account,
            ActionKind::MergeThreads,
            "Conversations merged",
        )?;
        for source in std::iter::once(target.as_str()).chain(sources.iter().copied()) {
            let ids = tx
                .prepare_cached(
                    "SELECT message_id FROM messages WHERE account_id = ?1 AND thread_id = ?2",
                )?
                .query_map(params![account.as_str(), source], |row| {
                    row.get::<_, String>(0).map(MessageId)
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if ids.is_empty() {
                return Err(Error::UnknownThread);
            }
            let from = ThreadId(source.to_owned());
            for id in &ids {
                assign(&tx, account, action.id, id, &from, target)?;
            }
        }
        for source in sources {
            tx.execute(
                "INSERT INTO local_action_thread_notes (action_id, note_id, before_thread)
                 SELECT ?1, id, thread_id FROM thread_notes WHERE account_id = ?2 AND thread_id = ?3",
                params![action.id, account.as_str(), source],
            )?;
            move_notes(&tx, account, source, target.as_str())?;
        }
        self.rethread_written(&tx, account)?;
        tx.commit()?;
        Ok(())
    }
}

fn assign(
    conn: &Connection,
    account: &AccountId,
    action: i64,
    id: &MessageId,
    from: &ThreadId,
    onto: &ThreadId,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO local_action_thread_members
         VALUES (?1, ?2, ?3, (SELECT thread_id FROM thread_overrides
                WHERE account_id = ?4 AND message_id = ?2), ?5)",
    )?
    .execute(params![
        action,
        id.as_str(),
        from.as_str(),
        account.as_str(),
        onto.as_str()
    ])?;
    conn.prepare_cached(
        "INSERT INTO thread_overrides VALUES (?1, ?2, ?3)
         ON CONFLICT (account_id, message_id) DO UPDATE SET thread_id = excluded.thread_id",
    )?
    .execute(params![account.as_str(), id.as_str(), onto.as_str()])?;
    restore_member(conn, account.as_str(), id, onto)
}

pub(crate) fn undo(
    store: &Store,
    conn: &Connection,
    account: &AccountId,
    action: &LocalAction,
) -> Result<Vec<ThreadId>> {
    let account_id = account.as_str();
    let members = conn
        .prepare_cached(
            "SELECT message_id, before_thread, before_override, after_thread
         FROM local_action_thread_members WHERE action_id = ?1 ORDER BY message_id",
        )?
        .query_map([action.id], |row| {
            Ok((
                row.get::<_, String>(0).map(MessageId)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let message_ids: HashSet<_> = members.iter().map(|(id, ..)| id).collect();
    let mut arrivals = HashMap::new();
    let mut snapshots = HashSet::new();
    for (id, before, _, after) in &members {
        if let Some(parent) = stored_parent(conn, account_id, id)? {
            snapshots.insert(parent.id);
        }
        // Only a split creates an otherwise empty thread. After undo, rows
        // that arrived there without a snapshot return to the old group.
        let fallback = (action.kind == ActionKind::SplitThread).then_some(before.as_str());
        arrivals.insert(after.as_str(), fallback);
    }
    for (after, fallback) in arrivals {
        let thread = ThreadId(after.to_owned());
        for row in rows_in_thread(conn, account, &thread)? {
            if snapshots.contains(&row.headers.message_id) {
                continue;
            }
            // A reply that arrived during a merge may belong to either old
            // thread. Reset to its independent key and let the pass resolve it.
            let destination = match fallback {
                Some(before) => ThreadId(before.to_owned()),
                None => {
                    crate::threading::resolve_thread_id(&row.headers, |_| Ok::<_, Error>(None))?
                }
            };
            restore_member(conn, account_id, &row.headers.message_id, &destination)?;
        }
    }

    let mut restored = Vec::new();
    for (id, before, saved, _) in &members {
        if let Some(saved) = saved {
            conn.prepare_cached(
                "UPDATE thread_overrides SET thread_id = ?1
                WHERE account_id = ?2 AND message_id = ?3",
            )?
            .execute(params![saved, account_id, id.as_str()])?;
        } else {
            conn.prepare_cached(
                "DELETE FROM thread_overrides WHERE account_id = ?1 AND message_id = ?2",
            )?
            .execute(params![account_id, id.as_str()])?;
        }
        if let Some(current) = stored_parent(conn, account_id, id)? {
            // If two snapshots folded into one row, the canonical snapshot
            // wins. An alias can still restore the only surviving copy.
            if current.id == *id || !message_ids.contains(&current.id) {
                restore_member(conn, account_id, &current.id, &ThreadId(before.clone()))?;
            }
            restored.push(current.id);
        }
    }

    if action.kind == ActionKind::SplitThread {
        if let Some((_, before, _, _)) = members.first() {
            move_notes(conn, account, &format!("split:{}", action.id), before)?;
        }
    } else {
        restore_notes(conn, account, action.id)?;
    }
    store.rethread_written(conn, account)?;
    conn.execute(
        "DELETE FROM local_action_thread_members WHERE action_id = ?1",
        [action.id],
    )?;
    conn.execute(
        "DELETE FROM local_action_thread_notes WHERE action_id = ?1",
        [action.id],
    )?;

    let mut threads = Vec::new();
    for id in restored {
        if let Some(parent) = stored_parent(conn, account_id, &id)?
            && !threads.contains(&parent.thread)
        {
            threads.push(parent.thread);
        }
    }
    Ok(threads)
}

fn restore_member(
    conn: &Connection,
    account: &str,
    id: &MessageId,
    thread: &ThreadId,
) -> Result<()> {
    // Recheck ancestry and later arrivals using the same dirty queue a
    // mailbox write uses. Changing a thread ID alone doesn't queue it.
    conn.prepare_cached(
        "INSERT OR IGNORE INTO thread_dirty_rows
         SELECT rowid FROM messages WHERE account_id = ?1 AND message_id = ?2",
    )?
    .execute(params![account, id.as_str()])?;
    conn.prepare_cached(
        "UPDATE messages SET thread_id = ?1 WHERE account_id = ?2 AND message_id = ?3 AND thread_id != ?1",
    )?
    .execute(params![thread.as_str(), account, id.as_str()])?;
    Ok(())
}

fn move_notes(conn: &Connection, account: &AccountId, from: &str, onto: &str) -> Result<()> {
    conn.execute(
        "UPDATE thread_notes SET thread_id = ?1, active = 0 WHERE account_id = ?2 AND thread_id = ?3",
        params![onto, account.as_str(), from],
    )?;
    settle_notes(conn, account, onto)
}

fn restore_notes(conn: &Connection, account: &AccountId, action: i64) -> Result<()> {
    let notes = conn
        .prepare_cached(
            "SELECT DISTINCT n.thread_id, s.before_thread FROM local_action_thread_notes s
         JOIN thread_notes n ON n.id = s.note_id WHERE s.action_id = ?1",
        )?
        .query_map([action], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    conn.execute(
        "UPDATE thread_notes SET thread_id = s.before_thread, active = 0
         FROM local_action_thread_notes s WHERE s.action_id = ?1 AND s.note_id = thread_notes.id",
        [action],
    )?;
    let threads: BTreeSet<_> = notes.iter().flat_map(|(from, onto)| [from, onto]).collect();
    for thread in threads {
        settle_notes(conn, account, thread)?;
    }
    Ok(())
}

/// Like a new annotation, the newest live note for each actor supplies the
/// visible note. Older notes, seen state and suggestion resolutions survive.
fn settle_notes(conn: &Connection, account: &AccountId, thread: &str) -> Result<()> {
    conn.execute(
        "UPDATE thread_notes SET active = 0 WHERE account_id = ?1 AND thread_id = ?2 AND active = 1",
        params![account.as_str(), thread],
    )?;
    conn.execute(
        "UPDATE thread_notes SET active = 1 WHERE id IN (
             SELECT MAX(n.id) FROM thread_notes n JOIN agent_actions a ON a.id = n.id
             WHERE n.account_id = ?1 AND n.thread_id = ?2 AND a.undone = 0 GROUP BY n.actor
         )",
        params![account.as_str(), thread],
    )?;
    Ok(())
}
