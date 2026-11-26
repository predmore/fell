//! A manual edit pins both sides of a split, or every member of a merge.
//! Automatic edges can add mail to a pinned group, but cannot unite two of
//! them or rename the destination. Undo restores the previous assignments.

use std::collections::{BTreeSet, HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, params};

use super::{
    ManualThreads, direct_parent, header_parent, rows_in_thread, set_member_thread, stored_parent,
};
use crate::actions::{ActionKind, LocalAction, ensure_action_schema, insert_local_action};
use crate::store::{ListKey, Store, immediate_transaction};
use crate::threading::{subject_is_generic, thread_subject};
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
CREATE INDEX IF NOT EXISTS idx_local_action_thread_members_message
    ON local_action_thread_members(message_id, action_id DESC);
CREATE TABLE IF NOT EXISTS local_action_thread_notes (
    action_id INTEGER NOT NULL REFERENCES local_actions(id),
    note_id INTEGER NOT NULL,
    before_thread TEXT NOT NULL,
    PRIMARY KEY (action_id, note_id)
) WITHOUT ROWID;
";

#[derive(Debug)]
pub struct MergeCandidate {
    pub thread: ThreadId,
    pub subject: String,
    pub sender_name: Option<String>,
    pub sender_email: String,
    pub date_unix: i64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ThreadEditOutcome {
    /// The manual edit was recorded for undo.
    Applied(ThreadId),
    /// Existing or automatic grouping left no manual edit to record.
    AlreadyGrouped(ThreadId),
}

impl Store {
    /// Rank account-wide merge destinations by topic, people, and recency.
    pub fn merge_candidates(
        &self,
        account: &AccountId,
        sources: &[String],
        query: &str,
        own_email: &str,
    ) -> Result<Vec<MergeCandidate>> {
        let words: Vec<_> = query
            .split_whitespace()
            .take(20)
            .map(|word| {
                let word = word.to_ascii_lowercase();
                let mut pattern = String::from("*[^a-z0-9]");
                for ch in word.chars() {
                    if matches!(ch, '[' | '*' | '?') {
                        pattern.push('[');
                        pattern.push(ch);
                        pattern.push(']');
                    } else {
                        pattern.push(ch);
                    }
                }
                pattern.push('*');
                [word, pattern]
            })
            .collect();
        let source_ids = serde_json::json!(sources).to_string();
        let mut keys = HashSet::new();
        let mut people = HashSet::new();
        let mut source = self.conn.prepare_cached(
            "SELECT subject, participants FROM json_each(?2) s CROSS JOIN thread_summary t
               ON t.account_id = ?1 AND t.thread_id = s.value",
        )?;
        let source = source.query_map(params![account.as_str(), source_ids], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in source {
            let (subject, participants) = row?;
            let (key, _) = thread_subject(&subject);
            if !subject_is_generic(&key) {
                keys.insert(key);
            }
            people.extend(
                super::super::decode_participants(&participants)
                    .into_iter()
                    .map(|person| person.email.to_ascii_lowercase())
                    .filter(|email| !email.eq_ignore_ascii_case(own_email)),
            );
        }
        let candidates = if words.is_empty() {
            "candidates AS MATERIALIZED (
                 SELECT thread_id, 0 AS match_rank FROM thread_summary
                 WHERE account_id = ?1 AND thread_id NOT IN (SELECT value FROM json_each(?2))
                 ORDER BY last_date DESC, thread_id LIMIT 32
             )"
        } else {
            "terms AS MATERIALIZED (
                 SELECT value ->> 0 AS word, value ->> 1 AS pattern FROM json_each(?3)
             ),
             candidates AS MATERIALIZED (
                 SELECT t.thread_id, CASE
                    WHEN NOT EXISTS (SELECT 1 FROM terms
                         WHERE (' ' || lower(t.subject)) NOT GLOB pattern) THEN 0
                    WHEN NOT EXISTS (SELECT 1 FROM terms
                         WHERE NOT instr(lower(COALESCE(t.latest_name, '')), word)) THEN 1
                    WHEN NOT EXISTS (SELECT 1 FROM terms
                         WHERE NOT instr(lower(t.latest_email), word)) THEN 2
                    ELSE 3 END AS match_rank
                 FROM thread_summary t
                 WHERE t.account_id = ?1 AND t.thread_id NOT IN (SELECT value FROM json_each(?2))
                   AND NOT EXISTS (
                       SELECT 1 FROM terms WHERE NOT instr(lower(t.subject), word)
                         AND NOT instr(lower(t.latest_email), word)
                         AND NOT instr(lower(COALESCE(t.latest_name, '')), word)
                   )
                 ORDER BY match_rank, t.last_date DESC, t.thread_id LIMIT 32
             )"
        };
        let mut stmt = self.conn.prepare_cached(&format!(
            "WITH {candidates}
             SELECT t.thread_id, t.subject, t.latest_name, t.latest_email, t.last_date
             FROM candidates c
             JOIN thread_summary t ON t.account_id = ?1 AND t.thread_id = c.thread_id
             ORDER BY c.match_rank,
               EXISTS (SELECT 1 FROM messages m
                       CROSS JOIN subject_index s ON s.message_rowid = m.rowid
                       WHERE m.account_id = t.account_id AND m.thread_id = t.thread_id
                         AND s.subject_key IN (SELECT value FROM json_each(?4))) DESC,
               (SELECT COUNT(*) FROM json_each(?5) p
                WHERE instr(char(31) || lower(t.participants) || char(30),
                            char(31) || p.value || char(30))) DESC,
               t.last_date DESC, t.thread_id LIMIT 32"
        ))?;
        Ok(stmt
            .query_map(
                params![
                    account.as_str(),
                    source_ids,
                    serde_json::json!(words).to_string(),
                    serde_json::json!(keys).to_string(),
                    serde_json::json!(people).to_string()
                ],
                |row| {
                    Ok(MergeCandidate {
                        thread: ThreadId(row.get(0)?),
                        subject: row.get(1)?,
                        sender_name: row.get(2)?,
                        sender_email: row.get(3)?,
                        date_unix: row.get(4)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// Messages whose current explicit assignment comes from a merge.
    pub fn thread_manual_merges(
        &self,
        account: &AccountId,
        thread: &ThreadId,
    ) -> Result<HashSet<MessageId>> {
        if !self.conn.prepare_cached(
            "SELECT EXISTS (SELECT 1 FROM thread_overrides WHERE account_id = ?1 AND thread_id = ?2)",
        )?.query_row(params![account.as_str(), thread.as_str()], |row| row.get::<_, bool>(0))? {
            return Ok(HashSet::new());
        }
        let mut stmt = self.conn.prepare_cached(
            "SELECT o.message_id FROM thread_overrides o
             WHERE o.account_id = ?1 AND o.thread_id = ?2
               AND (SELECT a.kind FROM local_action_thread_members s JOIN local_actions a ON a.id = s.action_id
                    WHERE s.message_id = o.message_id AND s.after_thread = o.thread_id
                      AND s.before_thread != s.after_thread
                      AND a.account_id = o.account_id AND a.undone = 0
                    ORDER BY s.action_id DESC LIMIT 1) = 'merge_threads'",
        )?;
        Ok(stmt
            .query_map(params![account.as_str(), thread.as_str()], |row| {
                row.get(0).map(MessageId)
            })?
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
    pub fn split_thread(&self, account: &AccountId, root: &MessageId) -> Result<ThreadEditOutcome> {
        let tx = immediate_transaction(self.connection())?;
        self.rethread_written(&tx, account)?;
        let root = stored_parent(&tx, account.as_str(), root)?.ok_or(Error::UnknownThread)?;
        let thread = &root.thread;
        let rows = rows_in_thread(&tx, account, thread)?;

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
        let mut todo = vec![&root.id];
        while let Some(id) = todo.pop() {
            if branch.insert(id)
                && let Some(replies) = children.get(id)
            {
                todo.extend(replies);
            }
        }
        if branch.len() == rows.len() {
            tx.commit()?;
            return Ok(ThreadEditOutcome::AlreadyGrouped(root.thread));
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
        Ok(ThreadEditOutcome::Applied(detached))
    }

    /// Merge the complete selection into the destination conversation.
    pub fn merge_threads(
        &self,
        account: &AccountId,
        sources: &[ThreadId],
        target: &ThreadId,
    ) -> Result<ThreadEditOutcome> {
        let tx = immediate_transaction(self.connection())?;
        // Remember a member of each selection before processing the queue:
        // the pass can replace a whole conversation's list key.
        let target = thread_anchor(&tx, account, target)?;
        let source_anchors = sources
            .iter()
            .map(|source| thread_anchor(&tx, account, source))
            .collect::<Result<Vec<_>>>()?;
        self.rethread_written(&tx, account)?;
        let current_thread = |id| {
            stored_parent(&tx, account.as_str(), id)?
                .map(|member| member.thread)
                .ok_or(Error::UnknownThread)
        };
        let target = current_thread(&target)?;
        let mut sources = source_anchors
            .iter()
            .map(|id| current_thread(id).map(|thread| thread.0))
            .collect::<Result<BTreeSet<_>>>()?;
        sources.remove(target.as_str());
        if sources.is_empty() {
            // The automatic pass already joined the selection. Keep its
            // result even though there is no manual edit to record.
            tx.commit()?;
            return Ok(ThreadEditOutcome::AlreadyGrouped(target));
        }
        ensure_action_schema(&tx)?;
        let action = insert_local_action(
            &tx,
            account,
            ActionKind::MergeThreads,
            "Conversations merged",
        )?;
        for source in std::iter::once(target.as_str()).chain(sources.iter().map(String::as_str)) {
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
                assign(&tx, account, action.id, id, &from, &target)?;
            }
        }
        for source in sources {
            tx.execute(
                "INSERT INTO local_action_thread_notes (action_id, note_id, before_thread)
                 SELECT ?1, id, thread_id FROM thread_notes WHERE account_id = ?2 AND thread_id = ?3",
                params![action.id, account.as_str(), source.as_str()],
            )?;
            move_notes(&tx, account, source.as_str(), target.as_str())?;
        }
        self.rethread_written(&tx, account)?;
        tx.commit()?;
        Ok(ThreadEditOutcome::Applied(target))
    }
}

fn thread_anchor(conn: &Connection, account: &AccountId, thread: &ThreadId) -> Result<MessageId> {
    conn.prepare_cached(
        "SELECT message_id FROM messages WHERE account_id = ?1 AND thread_id = ?2
         ORDER BY date_unix DESC, rowid DESC LIMIT 1",
    )?
    .query_row(params![account.as_str(), thread.as_str()], |row| {
        row.get(0).map(MessageId)
    })
    .optional()?
    .ok_or(Error::UnknownThread)
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
    let mut after_threads = BTreeSet::new();
    let mut snapshots = HashSet::new();
    for (id, _, _, after) in &members {
        if let Some(parent) = stored_parent(conn, account_id, id)? {
            snapshots.insert(parent.id);
        }
        after_threads.insert(after.as_str());
    }
    let mut arrivals = Vec::new();
    let mut late_parents = HashSet::new();
    for after in after_threads {
        for row in rows_in_thread(conn, account, &ThreadId(after.to_owned()))? {
            if !snapshots.contains(&row.headers.message_id) {
                arrivals.push(row);
            } else if let Some(parent) = header_parent(conn, account_id, &row)?
                && parent.thread.as_str() == after
                && !snapshots.contains(&parent.id)
            {
                late_parents.insert(parent.id);
            }
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

    // Resolve against the restored snapshots. Another arrival still carries
    // its temporary key here, so let the pass reconnect those reply chains.
    let arrival_ids: HashSet<_> = arrivals.iter().map(|row| &row.headers.message_id).collect();
    for row in &arrivals {
        let destination = crate::threading::resolve_thread_id(&row.headers, |parent| {
            Ok::<_, Error>(
                stored_parent(conn, account_id, parent)?
                    .filter(|parent| !arrival_ids.contains(&parent.id))
                    .map(|parent| parent.thread),
            )
        })?;
        restore_member(conn, account_id, &row.headers.message_id, &destination)?;
    }
    if action.kind == ActionKind::SplitThread {
        if let Some((_, before, _, _)) = members.first() {
            move_notes(conn, account, &format!("split:{}", action.id), before)?;
        }
    } else {
        restore_notes(conn, account, action.id)?;
    }
    let header_parents = store.rethread_written(conn, account)?;
    if !arrivals.is_empty() {
        // A late reply follows its restored ancestor even if Proton gave it
        // another id. Active edits still place replies and late header parents.
        let manual = ManualThreads::load(conn, account_id)?;
        let mut anchors = ManualThreads::default();
        for id in &restored {
            if let Some(member) = stored_parent(conn, account_id, id)? {
                anchors.overrides.insert(member.id, member.thread);
            }
        }
        let mut read = conn.prepare_cached(&format!(
            "SELECT {} FROM messages WHERE account_id = ?1 AND message_id = ?2",
            super::THREADING_COLS,
        ))?;
        let mut active = HashMap::new();
        let mut resolved = HashMap::new();
        for row in &arrivals {
            if !late_parents.contains(&row.headers.message_id)
                && !header_parents.contains_key(&row.headers.message_id)
                && manual
                    .destination(conn, account_id, row, &mut read, &mut active)?
                    .is_none()
                && let Some(destination) =
                    anchors.destination(conn, account_id, row, &mut read, &mut resolved)?
            {
                set_member_thread(conn, account_id, &row.headers.message_id, &destination)?;
            }
        }
    }
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

/// A UIDVALIDITY repair proved these two synthetic IDs name the same mail.
pub(in crate::store) fn retarget_override(
    conn: &Connection,
    account: &AccountId,
    from: &str,
    into: &str,
) -> Result<()> {
    let account = account.as_str();
    let Some(thread) = super::stored_override(conn, account, &MessageId(from.into()))? else {
        return Ok(());
    };
    conn.prepare_cached(
        "INSERT OR IGNORE INTO thread_overrides (account_id, message_id, thread_id)
         VALUES (?1, ?2, ?3)",
    )?
    .execute(params![account, into, thread.as_str()])?;
    conn.prepare_cached("DELETE FROM thread_overrides WHERE account_id = ?1 AND message_id = ?2")?
        .execute(params![account, from])?;
    conn.prepare_cached(
        "UPDATE OR IGNORE local_action_thread_members SET message_id = ?3 WHERE message_id = ?2
         AND action_id IN (SELECT id FROM local_actions WHERE account_id = ?1 AND undone = 0)",
    )?
    .execute(params![account, from, into])?;
    let id = MessageId(into.into());
    if let Some(onto) = super::stored_override(conn, account, &id)? {
        restore_member(conn, account, &id, &onto)?;
    }
    Ok(())
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
    set_member_thread(conn, account, id, thread)
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
