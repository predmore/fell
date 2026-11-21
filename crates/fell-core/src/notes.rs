//! Agent-authored notes. Only a person acting through the UI can resolve a suggestion.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::actions::{ensure_action_schema, flags::apply_flags_in, insert_local_action};
use crate::agent::{WriteScope, canonical_label, check_write_grant, insert_action};
use crate::{
    AccountId, ActionKind, Error, FlagChange, LabelChange, LocalAction, RecordedAction, Result,
    Store, ThreadId,
};

pub const MAX_NOTE_BYTES: usize = 1024;
pub const MAX_SUGGESTIONS: usize = 6;

const ANNOTATION: i64 = -1;
const DISMISSAL: i64 = -2;
const REPLY_OPENED: &str = "Suggested reply opened";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS thread_notes (
    id INTEGER PRIMARY KEY REFERENCES agent_actions(id),
    account_id TEXT NOT NULL REFERENCES accounts(id),
    thread_id TEXT NOT NULL,
    actor TEXT NOT NULL,
    created_unix INTEGER NOT NULL,
    summary TEXT NOT NULL CHECK (length(CAST(summary AS BLOB)) <= 1024),
    suggestions TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    seen INTEGER NOT NULL DEFAULT 0
);
CREATE UNIQUE INDEX IF NOT EXISTS thread_notes_current
    ON thread_notes(account_id, thread_id, actor) WHERE active = 1;
CREATE INDEX IF NOT EXISTS thread_notes_history
    ON thread_notes(account_id, thread_id, actor, id);
CREATE TABLE IF NOT EXISTS local_thread_note_actions (
    action_id INTEGER PRIMARY KEY REFERENCES local_actions(id),
    note_id INTEGER NOT NULL REFERENCES thread_notes(id),
    operation INTEGER NOT NULL,
    related_agent_action INTEGER REFERENCES agent_actions(id),
    undone INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS thread_note_resolutions
    ON local_thread_note_actions(note_id, operation) WHERE undone = 0;
CREATE INDEX IF NOT EXISTS thread_note_links ON local_thread_note_actions(note_id);
CREATE VIEW IF NOT EXISTS visible_thread_notes AS
    SELECT n.* FROM thread_notes n WHERE n.active = 1 AND NOT EXISTS (
        SELECT 1 FROM local_thread_note_actions r WHERE r.note_id = n.id AND r.operation = -2 AND r.undone = 0
    );
CREATE VIEW IF NOT EXISTS pending_thread_note_suggestions AS
    SELECT n.account_id, n.thread_id, n.id AS note_id, s.key AS position, s.value AS suggestion
    FROM visible_thread_notes n, json_each(n.suggestions) s
    WHERE NOT EXISTS (
        SELECT 1 FROM local_thread_note_actions r
        WHERE r.note_id = n.id AND r.operation = s.key AND r.undone = 0
    );
";

pub(crate) fn ensure_schema(conn: &Connection) -> Result<()> {
    if !crate::store::bodies::has_column(conn, "agent_grants", "can_annotate")?
        && let Err(err) = conn.execute(
            "ALTER TABLE agent_grants ADD COLUMN can_annotate INTEGER NOT NULL DEFAULT 0",
            [],
        )
        && !crate::store::duplicate_column(&err)
    {
        return Err(err.into());
    }
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Suggestion {
    Archive,
    Label(String),
    Reply,
    Snooze(i64),
    MarkRead,
    Star,
}

impl Suggestion {
    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "archive" => Ok(Self::Archive),
            "reply" => Ok(Self::Reply),
            "mark_read" => Ok(Self::MarkRead),
            "star" => Ok(Self::Star),
            _ => match raw.split_once(':') {
                Some(("label", label)) if label.len() <= 80 => {
                    if display_safe(label, false) != label {
                        return Err(Error::Invalid("invalid suggestion label".into()));
                    }
                    let label = canonical_label(label)?;
                    Ok(Self::Label(label))
                }
                Some(("snooze", until)) => until
                    .parse::<i64>()
                    .ok()
                    .filter(|until| (1..=253_402_300_799).contains(until))
                    .map(Self::Snooze)
                    .ok_or_else(|| Error::Invalid("snooze needs a positive Unix timestamp".into())),
                _ => Err(Error::Invalid("unknown suggested action".into())),
            },
        }
    }

    pub fn title(&self) -> String {
        match self {
            Self::Archive => "Archive".into(),
            Self::Label(label) => format!("Label {label}"),
            Self::Reply => "Reply".into(),
            Self::Snooze(_) => "Snooze".into(),
            Self::MarkRead => "Mark read".into(),
            Self::Star => "Star".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSuggestion {
    pub position: i64,
    pub action: Suggestion,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadNote {
    pub id: i64,
    pub actor: String,
    pub created_unix: i64,
    pub summary: String,
    pub suggestions: Vec<PendingSuggestion>,
}

/// The exact suggestion shown to the user. A newer annotation cannot change this target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteDecision {
    Accept(i64),
    Dismiss,
}

impl Store {
    pub fn annotate_thread(
        &self,
        account: &AccountId,
        thread: &ThreadId,
        agent: &str,
        summary: Option<&str>,
        suggestions: &[String],
        reason: &str,
    ) -> Result<RecordedAction> {
        // The grant is also the note's identity. Reject invisible characters
        // instead of silently coalescing two distinct grants.
        if display_safe(agent, false) != agent {
            return Err(Error::Invalid("invalid note author".into()));
        }
        let summary = clean_text(summary.unwrap_or_default(), "summary")?;
        let reason = clean_text(reason, "reason")?;
        if reason.is_empty() {
            return Err(Error::Invalid("reason is required".into()));
        }
        if summary.is_empty() && suggestions.is_empty() {
            return Err(Error::Invalid("provide a summary or suggestions".into()));
        }
        if suggestions.len() > MAX_SUGGESTIONS {
            return Err(Error::Invalid(
                "at most six suggested actions are allowed".into(),
            ));
        }
        let mut unique = HashSet::new();
        for raw in suggestions {
            let suggestion = Suggestion::parse(raw)?;
            if !unique.insert(suggestion) {
                return Err(Error::Invalid("suggested actions must be distinct".into()));
            }
        }
        let suggestions =
            serde_json::to_string(suggestions).map_err(|err| Error::Invalid(err.to_string()))?;
        let tx = crate::store::immediate_transaction(self.connection())?;
        check_write_grant(&tx, account, agent, WriteScope::Annotate)?;
        for suggestion in &unique {
            if let Suggestion::Label(label) = suggestion {
                crate::store::keywords::validate_label(&tx, account, label)?;
            }
        }
        let exists = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM messages WHERE account_id = ?1 AND thread_id = ?2)",
            params![account.as_str(), thread.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Err(Error::UnknownThread);
        }
        let action = insert_action(&tx, account, &format!("agent:{agent}"), "annotate", &reason)?;
        tx.execute(
            "UPDATE thread_notes SET active = 0
             WHERE account_id = ?1 AND thread_id = ?2 AND actor = ?3 AND active = 1",
            params![account.as_str(), thread.as_str(), agent],
        )?;
        tx.execute(
            "INSERT INTO thread_notes (id, account_id, thread_id, actor, created_unix, summary, suggestions)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![action.id, account.as_str(), thread.as_str(), agent, action.created_unix, summary, suggestions],
        )?;
        ensure_action_schema(&tx)?;
        let local =
            insert_local_action(&tx, account, ActionKind::AnnotateThread, "Agent note added")?;
        link_action(&tx, local.id, action.id, ANNOTATION, None)?;
        tx.commit()?;
        Ok(action)
    }

    /// Includes seen notes until dismissed. Every actor contributes at most one.
    pub fn thread_notes(&self, account: &AccountId, thread: &ThreadId) -> Result<Vec<ThreadNote>> {
        let mut stmt = self.connection().prepare_cached(
            "SELECT n.id, n.actor, n.created_unix, n.summary, s.key, s.value
             FROM visible_thread_notes n
             LEFT JOIN json_each(n.suggestions) s ON NOT EXISTS (
                 SELECT 1 FROM local_thread_note_actions r
                 WHERE r.note_id = n.id AND r.operation = s.key AND r.undone = 0
             )
             WHERE n.account_id = ?1 AND n.thread_id = ?2
             ORDER BY n.id DESC, s.key",
        )?;
        let mut rows = stmt.query(params![account.as_str(), thread.as_str()])?;
        let mut notes: Vec<ThreadNote> = Vec::new();
        while let Some(row) = rows.next()? {
            let id = row.get(0)?;
            if notes.last().is_none_or(|note| note.id != id) {
                notes.push(ThreadNote {
                    id,
                    actor: row.get(1)?,
                    created_unix: row.get(2)?,
                    summary: row.get(3)?,
                    suggestions: Vec::new(),
                });
            }
            if let Some(position) = row.get(4)? {
                let raw: String = row.get(5)?;
                // Older notes may contain labels we now reject. Keep the
                // rest of the note readable and leave those actions unavailable.
                if let Ok(action) = Suggestion::parse(&raw) {
                    notes
                        .last_mut()
                        .expect("note was loaded")
                        .suggestions
                        .push(PendingSuggestion { position, action });
                }
            }
        }
        Ok(notes)
    }

    pub fn unread_note_threads(&self, account: &AccountId) -> Result<HashSet<String>> {
        let mut stmt = self.connection().prepare_cached(
            "SELECT DISTINCT thread_id FROM visible_thread_notes WHERE account_id = ?1 AND seen = 0",
        )?;
        let rows = stmt.query_map([account.as_str()], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn pending_note_threads(&self, account: &AccountId) -> Result<HashSet<String>> {
        let mut stmt = self.connection().prepare_cached(
            "SELECT DISTINCT thread_id FROM pending_thread_note_suggestions WHERE account_id = ?1",
        )?;
        let rows = stmt.query_map([account.as_str()], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn see_thread_note(&self, account: &AccountId, note: i64) -> Result<()> {
        self.connection().execute(
            "UPDATE thread_notes SET seen = 1 WHERE account_id = ?1 AND id = ?2 AND seen = 0",
            params![account.as_str(), note],
        )?;
        Ok(())
    }

    /// The mail edit and its suggestion resolution share one transaction and undo entry.
    pub fn resolve_thread_note(
        &self,
        account: &AccountId,
        thread: &ThreadId,
        folder: &str,
        note: i64,
        decision: NoteDecision,
        scoped: bool,
    ) -> Result<LocalAction> {
        let tx = crate::store::immediate_transaction(self.connection())?;
        ensure_action_schema(&tx)?;
        let stale = || Error::Action("The agent note changed".into());
        let (action, operation, related) = match decision {
            NoteDecision::Dismiss => {
                let found: bool = tx.query_row(
                    "SELECT EXISTS (SELECT 1 FROM visible_thread_notes
                     WHERE account_id = ?1 AND thread_id = ?2 AND id = ?3)",
                    params![account.as_str(), thread.as_str(), note],
                    |row| row.get(0),
                )?;
                if !found {
                    return Err(stale());
                }
                (
                    insert_local_action(
                        &tx,
                        account,
                        ActionKind::DismissNote,
                        "Agent note dismissed",
                    )?,
                    DISMISSAL,
                    None,
                )
            }
            NoteDecision::Accept(position) => {
                let suggestion = pending_suggestion(&tx, account, thread, note, position)?;
                let mut related = None;
                let action = match suggestion {
                    Suggestion::Archive => {
                        Some(self.archive_threads_in(&tx, account, std::slice::from_ref(thread), folder)?)
                    }
                    Suggestion::Label(label) => {
                        Some(self.label_thread_in(&tx, account, thread, LabelChange::Add(label))?)
                    }
                    Suggestion::MarkRead | Suggestion::Star => {
                        let change = if suggestion == Suggestion::MarkRead {
                            FlagChange::Read
                        } else {
                            FlagChange::Star
                        };
                        {
                            let roles = scoped.then(|| self.folder_roles(account)).transpose()?;
                            apply_flags_in(
                                &tx,
                                account,
                                [(thread, folder)],
                                change,
                                roles.as_ref(),
                            )?
                        }
                    }
                    Suggestion::Snooze(until) => {
                        if until <= crate::agent::now_unix() {
                            return Err(Error::Action("The suggested snooze has expired".into()));
                        }
                        related = crate::agent::snooze_in(&tx, account, thread, until, "user")?
                            .map(|action| action.id);
                        None
                    }
                    Suggestion::Reply => {
                        return Err(Error::Action("Reply requires a compose draft".into()));
                    }
                };
                let action = match action {
                    Some(action) => action,
                    None => insert_local_action(
                        &tx,
                        account,
                        ActionKind::AcceptSuggestion,
                        "Suggestion accepted",
                    )?,
                };
                (action, position, related)
            }
        };
        link_action(&tx, action.id, note, operation, related)?;
        tx.commit()?;
        Ok(action)
    }
}

fn pending_suggestion(
    conn: &Connection,
    account: &AccountId,
    thread: &ThreadId,
    note: i64,
    position: i64,
) -> Result<Suggestion> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT suggestion FROM pending_thread_note_suggestions
         WHERE account_id = ?1 AND thread_id = ?2 AND note_id = ?3 AND position = ?4",
            params![account.as_str(), thread.as_str(), note, position],
            |row| row.get(0),
        )
        .optional()?;
    Suggestion::parse(&raw.ok_or_else(|| Error::Action("The agent note changed".into()))?)
}

pub(crate) fn accept_reply(
    conn: &Connection,
    account: &AccountId,
    thread: &ThreadId,
    note: i64,
    position: i64,
) -> Result<i64> {
    if pending_suggestion(conn, account, thread, note, position)? != Suggestion::Reply {
        return Err(Error::Action("The agent note changed".into()));
    }
    ensure_action_schema(conn)?;
    let action = insert_local_action(conn, account, ActionKind::AcceptSuggestion, REPLY_OPENED)?;
    link_action(conn, action.id, note, position, None)?;
    Ok(action.id)
}

fn link_action(
    conn: &Connection,
    action: i64,
    note: i64,
    operation: i64,
    related: Option<i64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO local_thread_note_actions (action_id, note_id, operation, related_agent_action)
         VALUES (?1, ?2, ?3, ?4)",
        params![action, note, operation, related],
    )?;
    Ok(())
}

pub(crate) enum NoteUndo {
    Annotation,
    Dismissal,
    Acceptance { snoozed: bool },
}

impl NoteUndo {
    pub(crate) fn notice(self, action: &LocalAction) -> &'static str {
        match self {
            Self::Annotation => "Note removed",
            Self::Dismissal => "Note restored",
            Self::Acceptance { snoozed: true } => "Snooze undone · suggestion restored",
            Self::Acceptance { snoozed: false } => match action.kind {
                ActionKind::Archive => "Unarchived · suggestion restored",
                ActionKind::Label => "Label removed · suggestion restored",
                ActionKind::Read => "Marked unread · suggestion restored",
                ActionKind::Star => "Unstarred · suggestion restored",
                ActionKind::AcceptSuggestion if action.summary == REPLY_OPENED => {
                    "Reply draft kept · suggestion restored"
                }
                _ => "Suggestion restored",
            },
        }
    }
}

pub(crate) fn undo_local(
    conn: &Connection,
    account: &AccountId,
    action: i64,
) -> Result<Option<NoteUndo>> {
    let link: Option<(i64, i64, Option<i64>)> = conn.query_row(
        "SELECT note_id, operation, related_agent_action FROM local_thread_note_actions WHERE action_id = ?1 AND undone = 0",
        [action], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    if let Some((note, operation, related)) = link {
        if operation == ANNOTATION {
            crate::agent::undo_recorded(conn, account, note)?;
        }
        if let Some(id) = related {
            crate::agent::undo_recorded(conn, account, id)?;
        }
        conn.execute(
            "UPDATE local_thread_note_actions SET undone = 1 WHERE action_id = ?1",
            [action],
        )?;
        return Ok(Some(match operation {
            ANNOTATION => NoteUndo::Annotation,
            DISMISSAL => NoteUndo::Dismissal,
            _ => NoteUndo::Acceptance {
                snoozed: related.is_some(),
            },
        }));
    }
    Ok(None)
}

pub(crate) fn drop_thread(conn: &Connection, account: &str, thread: &str) -> Result<()> {
    let ids = conn
        .prepare_cached("SELECT id FROM thread_notes WHERE account_id = ?1 AND thread_id = ?2")?
        .query_map(params![account, thread], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // The local action schema is installed on the first action. A mailbox
    // without notes may not have it yet, so do not touch the referencing table.
    if ids.is_empty() {
        return Ok(());
    }
    conn.prepare_cached(
        "DELETE FROM local_thread_note_actions WHERE note_id IN (
            SELECT id FROM thread_notes WHERE account_id = ?1 AND thread_id = ?2
         )",
    )?
    .execute(params![account, thread])?;
    conn.prepare_cached("DELETE FROM thread_notes WHERE account_id = ?1 AND thread_id = ?2")?
        .execute(params![account, thread])?;
    let mut actions =
        conn.prepare_cached("DELETE FROM agent_actions WHERE id = ?1 AND kind = 'annotate'")?;
    for id in ids {
        actions.execute([id])?;
    }
    Ok(())
}

pub(crate) fn undo_annotation(conn: &Connection, note: i64) -> Result<()> {
    if conn.execute(
        "UPDATE thread_notes SET active = 0 WHERE id = ?1 AND active = 1",
        [note],
    )? > 0
    {
        conn.execute(
            "UPDATE thread_notes SET active = 1 WHERE id = (
                SELECT older.id FROM thread_notes AS older
                JOIN thread_notes AS current ON current.id = ?1
                JOIN agent_actions AS action ON action.id = older.id AND action.undone = 0
                WHERE older.account_id = current.account_id AND older.thread_id = current.thread_id
                  AND older.actor = current.actor AND older.id < current.id
                ORDER BY older.id DESC LIMIT 1
             )",
            [note],
        )?;
    }
    conn.execute(
        "UPDATE local_actions SET undone = 1 WHERE id IN (
            SELECT action_id FROM local_thread_note_actions WHERE note_id = ?1 AND operation = -1
         )",
        [note],
    )?;
    conn.execute(
        "UPDATE local_thread_note_actions SET undone = 1 WHERE note_id = ?1 AND operation = -1",
        [note],
    )?;
    Ok(())
}

fn clean_text(text: &str, field: &str) -> Result<String> {
    if text.len() > MAX_NOTE_BYTES {
        return Err(Error::Invalid(format!(
            "{field} exceeds {MAX_NOTE_BYTES} bytes"
        )));
    }
    Ok(display_safe(text, true).trim().to_string())
}

/// Plain display text with control characters and invisible formatting removed.
/// Paragraphs in notes may retain line feeds.
pub fn display_safe(text: &str, multiline: bool) -> String {
    text.chars()
        .filter(|&ch| {
            (multiline && ch == '\n')
                || (!ch.is_control()
                    && !matches!(
                        ch,
                        '\u{061c}'
                            | '\u{200b}'..='\u{200f}'
                            | '\u{2028}'..='\u{202e}'
                            | '\u{2060}'..='\u{2069}'
                            | '\u{feff}'
                    ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_grants_keep_their_scopes_and_annotation_starts_off() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_grants (
                agent_id TEXT NOT NULL, account_id TEXT NOT NULL, enabled INTEGER NOT NULL,
                can_read INTEGER NOT NULL, can_triage INTEGER NOT NULL, can_draft INTEGER NOT NULL,
                PRIMARY KEY (agent_id, account_id)
             );
             INSERT INTO agent_grants VALUES ('ada', 'old', 1, 1, 1, 0);",
        )
        .unwrap();
        ensure_schema(&conn).unwrap();
        ensure_schema(&conn).unwrap();
        let grant = crate::agent::read_grant(&conn, &AccountId("old".into()), "ada").unwrap();
        assert_eq!(grant, crate::AgentGrant::enabled(true, true, false));
    }
}
