//! Local archive, label, delete, and undo against the SQLite mailbox.
//!
//! These calls update rows the UI already reads. There is no socket, no IMAP
//! session, and no send. A successful call stores the previous folders and
//! labels on that account's undo stack. [`Store::undo`] restores the latest
//! entry and leaves every other account as it was.
//!
//! Archive and delete change one folder on the messages in a thread. Any
//! other folder on the same row, such as Sent, stays. Labels are a second
//! list, not folders. Archive and Trash are the account's folders for those
//! roles ([`Store::folder_roles`]), not fixed names.
//!
//! A folder those calls remove is recorded on that message. A later
//! [`Store::apply_delta`] still unions other folders and labels, and leaves
//! the removed folder out. Undo puts the saved row back, then deletes that
//! saved copy so the mailbox file does not keep the body or BCC.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::account::AccountId;
use crate::embed::{
    dequeue_message, enqueue_message, invalidate_message_embedding, remove_message_embedding,
};
use crate::folders::FolderRole;
use crate::message::{Message, encode_addresses};
use crate::search::{IndexedText, sync_terms, terms_for_indexed};
use crate::store::{Store, ThreadSummary};
use crate::threading::ThreadId;
use crate::{Error, Result};

/// IMAP inbox (RFC 3501 fixes this name). Matching a folder ignores ASCII case.
pub const FOLDER_INBOX: &str = "INBOX";

const SELECT_SAVED: &str = "message_id, remote_id, thread_id, proton_conversation_id,
    in_reply_to, references_header, from_name, from_email, subject,
    date_unix, snippet, body_text, unread, folders, labels, arrived_via,
    to_line, cc_line, bcc_line, has_attachment, list_id, reply_to_line, attachment_names";

const ACTION_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS local_actions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    account_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    summary TEXT NOT NULL,
    undone INTEGER NOT NULL DEFAULT 0,
    created_unix INTEGER NOT NULL,
    FOREIGN KEY(account_id) REFERENCES accounts(id)
);

CREATE INDEX IF NOT EXISTS idx_local_actions_account ON local_actions(account_id, id);

CREATE TABLE IF NOT EXISTS local_action_messages (
    action_id INTEGER NOT NULL,
    removed INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    remote_id TEXT NOT NULL,
    thread_id TEXT NOT NULL,
    proton_conversation_id TEXT,
    in_reply_to TEXT,
    references_header TEXT NOT NULL,
    from_name TEXT,
    from_email TEXT NOT NULL,
    subject TEXT NOT NULL,
    date_unix INTEGER NOT NULL,
    snippet TEXT NOT NULL,
    body_text TEXT,
    unread INTEGER NOT NULL,
    folders TEXT NOT NULL,
    labels TEXT NOT NULL,
    after_folders TEXT NOT NULL DEFAULT '',
    after_labels TEXT NOT NULL DEFAULT '',
    arrived_via TEXT NOT NULL,
    to_line TEXT NOT NULL,
    cc_line TEXT NOT NULL,
    bcc_line TEXT NOT NULL,
    has_attachment INTEGER NOT NULL,
    list_id TEXT,
    reply_to_line TEXT NOT NULL DEFAULT '',
    attachment_names TEXT NOT NULL DEFAULT '',
    FOREIGN KEY(action_id) REFERENCES local_actions(id)
);

CREATE INDEX IF NOT EXISTS idx_local_action_messages ON local_action_messages(action_id);

CREATE TABLE IF NOT EXISTS local_action_aliases (
    action_id INTEGER NOT NULL,
    canonical_message_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    remote_id TEXT NOT NULL DEFAULT '',
    folder TEXT NOT NULL DEFAULT '',
    FOREIGN KEY(action_id) REFERENCES local_actions(id)
);

CREATE TABLE IF NOT EXISTS local_folder_tombstones (
    action_id INTEGER NOT NULL,
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    folder TEXT NOT NULL,
    PRIMARY KEY (action_id, message_id, folder),
    FOREIGN KEY(action_id) REFERENCES local_actions(id)
);

CREATE INDEX IF NOT EXISTS idx_local_folder_tombstones
    ON local_folder_tombstones(account_id, message_id);

-- The message_bodies row of a message an action removed, as stored. No row
-- here means it had none, and undo queues it to be fetched again.
CREATE TABLE IF NOT EXISTS local_action_bodies (
    action_id INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    codec INTEGER NOT NULL,
    html BLOB,
    plain BLOB,
    bulk_headers TEXT NOT NULL DEFAULT '',
    parts_listed INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (action_id, message_id),
    FOREIGN KEY(action_id) REFERENCES local_actions(id)
);

-- The message_parts list of a message an action removed, as stored.
CREATE TABLE IF NOT EXISTS local_action_parts (
    action_id INTEGER NOT NULL,
    message_id TEXT NOT NULL,
    section TEXT NOT NULL,
    position INTEGER NOT NULL,
    role INTEGER NOT NULL,
    mime_type TEXT NOT NULL,
    filename TEXT,
    size INTEGER NOT NULL,
    encoded_size INTEGER NOT NULL,
    encoding INTEGER NOT NULL,
    content_id TEXT,
    disposition INTEGER NOT NULL,
    nested INTEGER NOT NULL,
    PRIMARY KEY (action_id, message_id, section),
    FOREIGN KEY(action_id) REFERENCES local_actions(id)
);
"#;

/// One recorded local edit. [`Store::undo`] returns the entry it restored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAction {
    pub id: i64,
    pub kind: ActionKind,
    pub summary: String,
}

/// What [`LocalAction`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionKind {
    Archive,
    Move,
    Label,
    Delete,
}

impl ActionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Archive => "archive",
            Self::Move => "move",
            Self::Label => "label",
            Self::Delete => "delete",
        }
    }

    fn parse(raw: &str) -> Result<Self> {
        match raw {
            "archive" => Ok(Self::Archive),
            "move" => Ok(Self::Move),
            "label" => Ok(Self::Label),
            "delete" => Ok(Self::Delete),
            _ => Err(Error::Action(format!("unknown action {raw}"))),
        }
    }
}

/// Add or remove one label on every message in a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LabelChange {
    Add(String),
    Remove(String),
}

#[derive(Debug, Clone)]
struct SavedRow {
    message_id: String,
    remote_id: String,
    thread_id: String,
    proton_conversation_id: Option<String>,
    in_reply_to: Option<String>,
    references_header: String,
    from_name: Option<String>,
    from_email: String,
    subject: String,
    date_unix: i64,
    snippet: String,
    body_text: Option<String>,
    unread: i64,
    folders: String,
    labels: String,
    arrived_via: String,
    to_line: String,
    cc_line: String,
    bcc_line: String,
    has_attachment: i64,
    list_id: Option<String>,
    reply_to_line: String,
    attachment_names: String,
}

#[derive(Clone)]
struct SavedAlias {
    message_id: String,
    remote_id: String,
    folder: String,
}

struct Planned {
    before: SavedRow,
    folders: String,
    labels: String,
    remove_row: bool,
}

impl Store {
    /// Move `from_folder` to the account's archive folder on messages in the
    /// thread that have it: Archive, or All Mail on a server with no Archive
    /// (see [`FolderRoles::archive_name`](crate::folders::FolderRoles::archive_name)).
    ///
    /// Other folders on those rows stay. The call is one undo entry.
    pub fn archive_thread(
        &self,
        account_id: &AccountId,
        thread_id: &ThreadId,
        from_folder: &str,
    ) -> Result<LocalAction> {
        let from_folder = canonical_folder(from_folder)?;
        let roles = self.folder_roles(account_id)?;
        let archive = roles.archive_name().to_string();
        if from_folder.eq_ignore_ascii_case(&archive) {
            return Err(Error::Action(format!(
                "archive moves messages into {}",
                roles.label(&archive)
            )));
        }
        self.apply_thread(account_id, thread_id, ActionKind::Archive, |rows| {
            let plans = plan_relocate(rows, &from_folder, &archive);
            if plans.is_empty() {
                return Err(Error::Action(format!("no messages in {from_folder}")));
            }
            let summary = format!("archived {} from {from_folder}", plans.len());
            Ok((summary, plans))
        })
    }

    /// Move `from_folder` to `to_folder` on messages in the thread that have it.
    ///
    /// Other folders on those rows stay, so a Sent copy is left in Sent when
    /// the caller moves the copy that is on screen. The call is one undo entry.
    pub fn move_thread(
        &self,
        account_id: &AccountId,
        thread_id: &ThreadId,
        from_folder: &str,
        to_folder: &str,
    ) -> Result<LocalAction> {
        let from_folder = canonical_folder(from_folder)?;
        let to_folder = canonical_folder(to_folder)?;
        if from_folder.eq_ignore_ascii_case(&to_folder) {
            return Err(Error::Action("move needs a different folder".into()));
        }
        self.apply_thread(account_id, thread_id, ActionKind::Move, |rows| {
            let plans = plan_relocate(rows, &from_folder, &to_folder);
            if plans.is_empty() {
                return Err(Error::Action(format!("no messages in {from_folder}")));
            }
            let summary = format!("moved {} from {from_folder} to {to_folder}", plans.len());
            Ok((summary, plans))
        })
    }

    /// Add or remove `change` on every message in the thread that needs it.
    ///
    /// Folders are left as they are. A label that is already present, or
    /// absent when removing, is skipped. The call fails when nothing changes.
    pub fn label_thread(
        &self,
        account_id: &AccountId,
        thread_id: &ThreadId,
        change: LabelChange,
    ) -> Result<LocalAction> {
        let (adding, raw) = match &change {
            LabelChange::Add(label) => (true, label.as_str()),
            LabelChange::Remove(label) => (false, label.as_str()),
        };
        let label = canonical_label(raw)?;
        self.apply_thread(account_id, thread_id, ActionKind::Label, |rows| {
            let plans = plan_label(rows, adding, &label);
            if plans.is_empty() {
                let message = if adding {
                    "label is already applied"
                } else {
                    "label is not on this thread"
                };
                return Err(Error::Action(message.into()));
            }
            let summary = if adding {
                format!("added label {label}")
            } else {
                format!("removed label {label}")
            };
            Ok((summary, plans))
        })
    }

    /// Move `from_folder` to the account's Trash folder on messages in the
    /// thread that have it.
    ///
    /// When `from_folder` is Trash, that folder is dropped. A row whose last
    /// folder was Trash is deleted, and undo inserts it again.
    pub fn delete_thread(
        &self,
        account_id: &AccountId,
        thread_id: &ThreadId,
        from_folder: &str,
    ) -> Result<LocalAction> {
        let from_folder = canonical_folder(from_folder)?;
        let roles = self.folder_roles(account_id)?;
        let trash = roles.name(FolderRole::Trash).to_string();
        self.apply_thread(account_id, thread_id, ActionKind::Delete, |rows| {
            let plans = if from_folder.eq_ignore_ascii_case(&trash) {
                plan_expunge(rows, &from_folder)
            } else {
                plan_relocate(rows, &from_folder, &trash)
            };
            if plans.is_empty() {
                return Err(Error::Action(format!("no messages in {from_folder}")));
            }
            let summary = format!("deleted {} from {from_folder}", plans.len());
            Ok((summary, plans))
        })
    }

    /// Restore the latest archive, move, label, or delete on this account.
    pub fn undo(&self, account_id: &AccountId) -> Result<LocalAction> {
        self.require_account(account_id)?;
        let tx = crate::store::immediate_transaction(self.connection())?;
        ensure_action_schema(&tx)?;
        let Some(action) = latest_open_action(&tx, account_id)? else {
            return Err(Error::Action("nothing to undo".into()));
        };
        let rows = load_snapshots(&tx, action.id)?;
        if rows.is_empty() {
            return Err(Error::Action("action has no messages".into()));
        }
        for row in &rows {
            restore_row(&tx, account_id, row)?;
        }
        tx.execute(
            "DELETE FROM local_folder_tombstones WHERE action_id = ?1",
            params![action.id],
        )?;
        tx.execute(
            "DELETE FROM local_action_messages WHERE action_id = ?1",
            params![action.id],
        )?;
        tx.execute(
            "DELETE FROM local_action_aliases WHERE action_id = ?1",
            params![action.id],
        )?;
        tx.execute(
            "DELETE FROM local_action_bodies WHERE action_id = ?1",
            params![action.id],
        )?;
        tx.execute(
            "DELETE FROM local_action_parts WHERE action_id = ?1",
            params![action.id],
        )?;
        let changed = tx.execute(
            "UPDATE local_actions SET undone = 1
             WHERE id = ?1 AND account_id = ?2 AND undone = 0",
            params![action.id, account_id.as_str()],
        )?;
        expect_one(changed, "undo")?;
        tx.commit()?;
        Ok(action)
    }

    /// Threads with at least one message in `folder`, newest activity first.
    ///
    /// `message_count` counts the whole thread, including copies in other folders.
    pub fn threads_in_folder(
        &self,
        account_id: &AccountId,
        folder: &str,
    ) -> Result<Vec<ThreadSummary>> {
        let folder = canonical_folder(folder)?;
        let mut stmt = self.connection().prepare(
            "SELECT
                m.thread_id,
                MAX(m.date_unix) AS last_date,
                COUNT(*) AS n,
                MAX(m.unread) AS any_unread,
                (SELECT subject FROM messages m2
                 WHERE m2.thread_id = m.thread_id AND m2.account_id = m.account_id
                 ORDER BY date_unix DESC LIMIT 1) AS subject
             FROM messages m
             WHERE m.account_id = ?1
               AND m.thread_id IN (
                   SELECT thread_id FROM messages
                   WHERE account_id = ?1
                     AND (
                         instr(char(30) || lower(folders) || char(30), char(30) || lower(?2) || char(30)) > 0
                         OR instr(char(30) || folders || char(30), char(30) || ?2 || char(30)) > 0
                     )
               )
             GROUP BY m.thread_id
             ORDER BY last_date DESC",
        )?;
        let rows = stmt.query_map(params![account_id.as_str(), folder], |row| {
            Ok(ThreadSummary {
                thread_id: ThreadId(row.get(0)?),
                last_date_unix: row.get(1)?,
                message_count: row.get::<_, i64>(2)? as usize,
                unread: row.get::<_, i64>(3)? != 0,
                subject: row.get(4)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    fn require_account(&self, account_id: &AccountId) -> Result<()> {
        let found: Option<i64> = self
            .connection()
            .query_row(
                "SELECT 1 FROM accounts WHERE id = ?1",
                params![account_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if found.is_some() {
            Ok(())
        } else {
            Err(Error::Action(format!(
                "unknown account {}",
                account_id.as_str()
            )))
        }
    }

    fn apply_thread(
        &self,
        account_id: &AccountId,
        thread_id: &ThreadId,
        kind: ActionKind,
        plans_for: impl FnOnce(&[SavedRow]) -> Result<(String, Vec<Planned>)>,
    ) -> Result<LocalAction> {
        self.require_account(account_id)?;
        let tx = crate::store::immediate_transaction(self.connection())?;
        ensure_action_schema(&tx)?;
        let rows = load_thread(&tx, account_id, thread_id)?;
        if rows.is_empty() {
            return Err(Error::Action("no such thread".into()));
        }
        let (summary, plans) = plans_for(&rows)?;
        if plans.is_empty() {
            return Err(Error::Action("nothing to change".into()));
        }
        let action = record_and_apply(&tx, account_id, kind, &summary, &plans)?;
        tx.commit()?;
        Ok(action)
    }
}

fn ensure_action_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(ACTION_SCHEMA)?;
    ensure_snapshot_columns(conn)?;
    if !crate::store::bodies::has_column(conn, "local_action_bodies", "parts_listed")? {
        conn.execute_batch(
            "ALTER TABLE local_action_bodies ADD COLUMN parts_listed INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    Ok(())
}

fn ensure_snapshot_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(local_action_messages)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut names = Vec::new();
    for name in rows {
        names.push(name?);
    }
    let missing = [
        (
            "after_folders",
            "ALTER TABLE local_action_messages ADD COLUMN after_folders TEXT NOT NULL DEFAULT ''",
        ),
        (
            "after_labels",
            "ALTER TABLE local_action_messages ADD COLUMN after_labels TEXT NOT NULL DEFAULT ''",
        ),
        (
            "list_id",
            "ALTER TABLE local_action_messages ADD COLUMN list_id TEXT",
        ),
        (
            "reply_to_line",
            "ALTER TABLE local_action_messages ADD COLUMN reply_to_line TEXT NOT NULL DEFAULT ''",
        ),
        (
            "attachment_names",
            "ALTER TABLE local_action_messages ADD COLUMN attachment_names TEXT NOT NULL DEFAULT ''",
        ),
    ];
    for (name, ddl) in missing {
        if !names.iter().any(|column| column == name) {
            conn.execute(ddl, [])?;
        }
    }
    Ok(())
}

fn record_and_apply(
    conn: &Connection,
    account_id: &AccountId,
    kind: ActionKind,
    summary: &str,
    plans: &[Planned],
) -> Result<LocalAction> {
    conn.execute(
        "INSERT INTO local_actions (account_id, kind, summary, undone, created_unix)
         VALUES (?1, ?2, ?3, 0, ?4)",
        params![account_id.as_str(), kind.as_str(), summary, now_unix()],
    )?;
    let id = conn.last_insert_rowid();
    for plan in plans {
        insert_snapshot(conn, id, account_id, plan)?;
        record_tombstones(conn, id, account_id, plan)?;
        apply_plan(conn, account_id, plan)?;
    }
    Ok(LocalAction {
        id,
        kind,
        summary: summary.to_string(),
    })
}

fn latest_open_action(conn: &Connection, account_id: &AccountId) -> Result<Option<LocalAction>> {
    let row: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, kind, summary FROM local_actions
             WHERE account_id = ?1 AND undone = 0
             ORDER BY id DESC LIMIT 1",
            params![account_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(id, kind, summary)| {
        Ok(LocalAction {
            id,
            kind: ActionKind::parse(&kind)?,
            summary,
        })
    })
    .transpose()
}

fn apply_plan(conn: &Connection, account_id: &AccountId, plan: &Planned) -> Result<()> {
    let old_terms = terms_of(&plan.before);
    if plan.remove_row {
        sync_terms(conn, account_id.as_str(), &old_terms, &HashSet::new())?;
        remove_message_embedding(conn, account_id.as_str(), &plan.before.message_id)?;
        dequeue_message(conn, account_id.as_str(), &plan.before.message_id)?;
        let changed = conn.execute(
            "DELETE FROM messages WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), plan.before.message_id],
        )?;
        expect_one(changed, &plan.before.message_id)?;
        conn.execute(
            "DELETE FROM message_aliases
             WHERE account_id = ?1
               AND (message_id = ?2 OR canonical_message_id = ?2)",
            params![account_id.as_str(), plan.before.message_id],
        )?;
        return Ok(());
    }
    let new_terms = terms_with(&plan.before, &plan.folders, &plan.labels);
    let changed = conn.execute(
        "UPDATE messages SET folders = ?1, labels = ?2
         WHERE account_id = ?3 AND message_id = ?4",
        params![
            plan.folders,
            plan.labels,
            account_id.as_str(),
            plan.before.message_id
        ],
    )?;
    expect_one(changed, &plan.before.message_id)?;
    sync_terms(conn, account_id.as_str(), &old_terms, &new_terms)?;
    Ok(())
}

fn restore_row(conn: &Connection, account_id: &AccountId, snapshot: &Snapshot) -> Result<()> {
    let saved = &snapshot.row;
    if let Some(current) = load_one(conn, account_id, &saved.message_id)? {
        if snapshot.removed {
            // A snapshot taken before Fell kept Reply-To and attachment names
            // has them empty. Keep what a sync has filled in since: local
            // actions never change them.
            let mut saved = saved.clone();
            if saved.reply_to_line.is_empty() {
                saved.reply_to_line = current.reply_to_line.clone();
            }
            if saved.attachment_names.is_empty() {
                saved.attachment_names = current.attachment_names.clone();
            }
            let saved = &saved;
            replace_message_row(conn, account_id, &current, saved)?;
            restore_aliases(conn, account_id, &saved.message_id, &snapshot.aliases)?;
            restore_bodies(conn, account_id, snapshot.action_id, &saved.message_id)?;
            return Ok(());
        }
        let folders = join_tokens(&invert_tokens(
            &split_tokens(&current.folders),
            &split_tokens(&saved.folders),
            &split_tokens(&snapshot.after_folders),
            true,
        ));
        let labels = join_tokens(&invert_tokens(
            &split_tokens(&current.labels),
            &split_tokens(&saved.labels),
            &split_tokens(&snapshot.after_labels),
            false,
        ));
        let old_terms = terms_of(&current);
        let new_terms = terms_with(&current, &folders, &labels);
        let changed = conn.execute(
            "UPDATE messages SET folders = ?1, labels = ?2
             WHERE account_id = ?3 AND message_id = ?4",
            params![folders, labels, account_id.as_str(), saved.message_id],
        )?;
        expect_one(changed, &saved.message_id)?;
        sync_terms(conn, account_id.as_str(), &old_terms, &new_terms)?;
        return Ok(());
    }
    if let Some(canonical_id) = live_alias_target(conn, account_id, &saved.message_id)? {
        let Some(current) = load_one(conn, account_id, &canonical_id)? else {
            return Ok(());
        };
        // The snapshot is the other copy. Put its folders and labels back on
        // the survivor and reattach the alias. Subject, body, and unread stay.
        let folders = join_tokens(&invert_tokens(
            &split_tokens(&current.folders),
            &split_tokens(&saved.folders),
            &split_tokens(&snapshot.after_folders),
            true,
        ));
        let labels = join_tokens(&invert_tokens(
            &split_tokens(&current.labels),
            &split_tokens(&saved.labels),
            &split_tokens(&snapshot.after_labels),
            false,
        ));
        let old_terms = terms_of(&current);
        let new_terms = terms_with(&current, &folders, &labels);
        let changed = conn.execute(
            "UPDATE messages SET folders = ?1, labels = ?2
             WHERE account_id = ?3 AND message_id = ?4",
            params![folders, labels, account_id.as_str(), canonical_id],
        )?;
        expect_one(changed, &canonical_id)?;
        sync_terms(conn, account_id.as_str(), &old_terms, &new_terms)?;
        let mut aliases = snapshot.aliases.clone();
        aliases.push(SavedAlias {
            message_id: saved.message_id.clone(),
            remote_id: saved.remote_id.clone(),
            folder: saved.folders.clone(),
        });
        restore_aliases(conn, account_id, &canonical_id, &aliases)?;
        return Ok(());
    }
    insert_message_row(conn, account_id, saved)?;
    restore_aliases(conn, account_id, &saved.message_id, &snapshot.aliases)?;
    if snapshot.removed {
        restore_bodies(conn, account_id, snapshot.action_id, &saved.message_id)?;
    }
    let terms = terms_of(saved);
    sync_terms(conn, account_id.as_str(), &HashSet::new(), &terms)?;
    Ok(())
}

/// Put back the parts a removed row had, replacing any a later sync wrote
/// for the same id, as the row itself is, with its part list. A row that had
/// none, or no list, is queued to be fetched again.
fn restore_bodies(
    conn: &Connection,
    account_id: &AccountId,
    action_id: i64,
    message_id: &str,
) -> Result<()> {
    for sql in [
        "DELETE FROM message_bodies WHERE account_id = ?1 AND message_id = ?2",
        "DELETE FROM message_parts WHERE account_id = ?1 AND message_id = ?2",
        "DELETE FROM body_backfill WHERE account_id = ?1 AND message_id = ?2",
    ] {
        conn.execute(sql, params![account_id.as_str(), message_id])?;
    }
    let restored = conn.execute(
        "INSERT INTO message_bodies
            (account_id, message_id, codec, html, plain, bulk_headers, parts_listed)
         SELECT ?1, message_id, codec, html, plain, bulk_headers, parts_listed
         FROM local_action_bodies
         WHERE action_id = ?3 AND message_id = ?2",
        params![account_id.as_str(), message_id, action_id],
    )?;
    let columns = crate::store::bodies::PART_COLUMNS;
    conn.execute(
        &format!(
            "INSERT INTO message_parts (account_id, message_id, {columns})
             SELECT ?1, message_id, {columns} FROM local_action_parts
             WHERE action_id = ?3 AND message_id = ?2"
        ),
        params![account_id.as_str(), message_id, action_id],
    )?;
    let listed = crate::store::bodies::parts_listed(conn, account_id, message_id)?;
    if restored == 0 || !listed {
        conn.execute(
            "INSERT OR IGNORE INTO body_backfill (account_id, message_id) VALUES (?1, ?2)",
            params![account_id.as_str(), message_id],
        )?;
    }
    Ok(())
}

fn live_alias_target(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Option<String>> {
    let canonical: Option<String> = conn
        .query_row(
            "SELECT a.canonical_message_id FROM message_aliases a
             JOIN messages m
               ON m.account_id = a.account_id AND m.message_id = a.canonical_message_id
             WHERE a.account_id = ?1 AND a.message_id = ?2
             LIMIT 1",
            params![account_id.as_str(), message_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(canonical)
}

fn restore_aliases(
    conn: &Connection,
    account_id: &AccountId,
    canonical_id: &str,
    aliases: &[SavedAlias],
) -> Result<()> {
    for alias in aliases {
        if alias.message_id == canonical_id {
            continue;
        }
        conn.execute(
            "INSERT INTO message_aliases (
                account_id, message_id, canonical_message_id, remote_id, folder
             ) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, message_id) DO UPDATE SET
                canonical_message_id = excluded.canonical_message_id,
                remote_id = excluded.remote_id,
                folder = excluded.folder",
            params![
                account_id.as_str(),
                alias.message_id,
                canonical_id,
                alias.remote_id,
                alias.folder,
            ],
        )?;
    }
    Ok(())
}

fn replace_message_row(
    conn: &Connection,
    account_id: &AccountId,
    current: &SavedRow,
    snapshot: &SavedRow,
) -> Result<()> {
    let old_terms = terms_of(current);
    let new_terms = terms_of(snapshot);
    let fingerprint = match snapshot.body_text.as_deref() {
        Some(text) if !text.trim().is_empty() => crate::message::body_fingerprint(text),
        _ => String::new(),
    };
    let mut values = saved_values(snapshot);
    let message_id = values.remove(0);
    values.push(Value::Text(fingerprint));
    values.push(Value::Text(account_id.as_str().to_string()));
    values.push(message_id);
    debug_assert_eq!(values.len(), 25);
    let changed = conn.execute(
        "UPDATE messages SET
            remote_id = ?1,
            thread_id = ?2,
            proton_conversation_id = ?3,
            in_reply_to = ?4,
            references_header = ?5,
            from_name = ?6,
            from_email = ?7,
            subject = ?8,
            date_unix = ?9,
            snippet = ?10,
            body_text = ?11,
            unread = ?12,
            folders = ?13,
            labels = ?14,
            arrived_via = ?15,
            to_line = ?16,
            cc_line = ?17,
            bcc_line = ?18,
            has_attachment = ?19,
            list_id = ?20,
            reply_to_line = ?21,
            attachment_names = ?22,
            body_fingerprint = ?23
         WHERE account_id = ?24 AND message_id = ?25",
        params_from_iter(values),
    )?;
    expect_one(changed, &snapshot.message_id)?;
    sync_terms(conn, account_id.as_str(), &old_terms, &new_terms)?;
    invalidate_message_embedding(
        conn,
        account_id.as_str(),
        &snapshot.message_id,
        snapshot.body_text.as_deref().unwrap_or(&snapshot.snippet),
    )?;
    Ok(())
}

fn expect_one(changed: usize, message_id: &str) -> Result<()> {
    if changed == 1 {
        Ok(())
    } else {
        Err(Error::Action(format!(
            "message {message_id} is not in this account"
        )))
    }
}

fn plan_relocate(rows: &[SavedRow], from: &str, to: &str) -> Vec<Planned> {
    let mut plans = Vec::new();
    for row in rows {
        let Some(next) = relocate(&split_tokens(&row.folders), from, to) else {
            continue;
        };
        plans.push(Planned {
            before: row.clone(),
            folders: join_tokens(&next),
            labels: row.labels.clone(),
            remove_row: false,
        });
    }
    plans
}

fn plan_expunge(rows: &[SavedRow], folder: &str) -> Vec<Planned> {
    let mut plans = Vec::new();
    for row in rows {
        let Some(left) = strip_token(&split_tokens(&row.folders), folder) else {
            continue;
        };
        plans.push(Planned {
            before: row.clone(),
            folders: join_tokens(&left),
            labels: row.labels.clone(),
            remove_row: left.is_empty(),
        });
    }
    plans
}

fn plan_label(rows: &[SavedRow], add: bool, label: &str) -> Vec<Planned> {
    let mut plans = Vec::new();
    for row in rows {
        let tokens = split_tokens(&row.labels);
        let Some(next) = (if add {
            add_token(&tokens, label)
        } else {
            remove_token(&tokens, label)
        }) else {
            continue;
        };
        plans.push(Planned {
            before: row.clone(),
            folders: row.folders.clone(),
            labels: join_tokens(&next),
            remove_row: false,
        });
    }
    plans
}

fn relocate(tokens: &[String], from: &str, to: &str) -> Option<Vec<String>> {
    if from.eq_ignore_ascii_case(to) {
        return None;
    }
    let without = strip_token(tokens, from)?;
    if without.iter().any(|token| token.eq_ignore_ascii_case(to)) {
        Some(without)
    } else {
        let mut out = without;
        out.push(to.to_string());
        Some(out)
    }
}

fn strip_token(tokens: &[String], folder: &str) -> Option<Vec<String>> {
    if !tokens
        .iter()
        .any(|token| token.eq_ignore_ascii_case(folder))
    {
        return None;
    }
    Some(
        tokens
            .iter()
            .filter(|token| !token.eq_ignore_ascii_case(folder))
            .cloned()
            .collect(),
    )
}

fn add_token(tokens: &[String], label: &str) -> Option<Vec<String>> {
    if tokens.iter().any(|token| token == label) {
        return None;
    }
    let mut out = tokens.to_vec();
    out.push(label.to_string());
    Some(out)
}

fn remove_token(tokens: &[String], label: &str) -> Option<Vec<String>> {
    if !tokens.iter().any(|token| token == label) {
        return None;
    }
    Some(
        tokens
            .iter()
            .filter(|token| token.as_str() != label)
            .cloned()
            .collect(),
    )
}

fn split_tokens(raw: &str) -> Vec<String> {
    raw.split('\u{1e}')
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn join_tokens(tokens: &[String]) -> String {
    tokens.join("\u{1e}")
}

fn canonical_folder(raw: &str) -> Result<String> {
    canonical_token(
        raw,
        "folder is empty",
        "folder contains a reserved character",
    )
}

fn canonical_label(raw: &str) -> Result<String> {
    canonical_token(raw, "label is empty", "label contains a reserved character")
}

fn canonical_token(raw: &str, empty: &str, reserved: &str) -> Result<String> {
    let token = raw.trim();
    if token.is_empty() {
        return Err(Error::Action(empty.into()));
    }
    if token.chars().any(|ch| ch == '\u{1e}' || ch == '\u{1f}') {
        return Err(Error::Action(reserved.into()));
    }
    Ok(token.to_string())
}

fn terms_of(row: &SavedRow) -> HashSet<String> {
    terms_with(row, &row.folders, &row.labels)
}

fn terms_with(row: &SavedRow, folders: &str, labels: &str) -> HashSet<String> {
    terms_for_indexed(IndexedText {
        subject: &row.subject,
        from_name: row.from_name.as_deref(),
        from_email: &row.from_email,
        body: row.body_text.as_deref().unwrap_or(&row.snippet),
        to_line: &row.to_line,
        cc_line: &row.cc_line,
        bcc_line: &row.bcc_line,
        folders,
        labels,
    })
}

fn load_thread(
    conn: &Connection,
    account_id: &AccountId,
    thread_id: &ThreadId,
) -> Result<Vec<SavedRow>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_SAVED} FROM messages
         WHERE account_id = ?1 AND thread_id = ?2
         ORDER BY date_unix ASC, message_id ASC"
    ))?;
    let rows = stmt.query_map(params![account_id.as_str(), thread_id.as_str()], read_saved)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn load_one(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Option<SavedRow>> {
    let row = conn
        .query_row(
            &format!(
                "SELECT {SELECT_SAVED} FROM messages
                 WHERE account_id = ?1 AND message_id = ?2"
            ),
            params![account_id.as_str(), message_id],
            read_saved,
        )
        .optional()?;
    Ok(row)
}

struct Snapshot {
    action_id: i64,
    removed: bool,
    row: SavedRow,
    after_folders: String,
    after_labels: String,
    aliases: Vec<SavedAlias>,
}

fn load_snapshots(conn: &Connection, action_id: i64) -> Result<Vec<Snapshot>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT removed, {SELECT_SAVED}, after_folders, after_labels
         FROM local_action_messages
         WHERE action_id = ?1
         ORDER BY message_id ASC"
    ))?;
    let rows = stmt.query_map(params![action_id], read_snapshot)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    drop(stmt);
    let mut aliases = load_saved_aliases(conn, action_id)?;
    for snapshot in &mut out {
        snapshot.action_id = action_id;
        if let Some(found) = aliases.remove(&snapshot.row.message_id) {
            snapshot.aliases = found;
        }
    }
    Ok(out)
}

fn load_saved_aliases(
    conn: &Connection,
    action_id: i64,
) -> Result<std::collections::HashMap<String, Vec<SavedAlias>>> {
    let mut stmt = conn.prepare(
        "SELECT canonical_message_id, message_id, remote_id, folder
         FROM local_action_aliases
         WHERE action_id = ?1
         ORDER BY message_id ASC",
    )?;
    let rows = stmt.query_map(params![action_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            SavedAlias {
                message_id: row.get(1)?,
                remote_id: row.get(2)?,
                folder: row.get(3)?,
            },
        ))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (canonical, alias) = row?;
        out.entry(canonical).or_insert_with(Vec::new).push(alias);
    }
    Ok(out)
}

fn read_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<Snapshot> {
    Ok(Snapshot {
        action_id: 0,
        removed: row.get::<_, i64>(0)? != 0,
        row: read_saved_from(row, 1)?,
        after_folders: row.get(24)?,
        after_labels: row.get(25)?,
        aliases: Vec::new(),
    })
}

fn read_saved(row: &rusqlite::Row<'_>) -> rusqlite::Result<SavedRow> {
    read_saved_from(row, 0)
}

fn read_saved_from(row: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<SavedRow> {
    Ok(SavedRow {
        message_id: row.get(at)?,
        remote_id: row.get(at + 1)?,
        thread_id: row.get(at + 2)?,
        proton_conversation_id: row.get(at + 3)?,
        in_reply_to: row.get(at + 4)?,
        references_header: row.get(at + 5)?,
        from_name: row.get(at + 6)?,
        from_email: row.get(at + 7)?,
        subject: row.get(at + 8)?,
        date_unix: row.get(at + 9)?,
        snippet: row.get(at + 10)?,
        body_text: row.get(at + 11)?,
        unread: row.get(at + 12)?,
        folders: row.get(at + 13)?,
        labels: row.get(at + 14)?,
        arrived_via: row.get(at + 15)?,
        to_line: row.get(at + 16)?,
        cc_line: row.get(at + 17)?,
        bcc_line: row.get(at + 18)?,
        has_attachment: row.get(at + 19)?,
        list_id: row.get(at + 20)?,
        reply_to_line: row.get(at + 21)?,
        attachment_names: row.get(at + 22)?,
    })
}

fn insert_snapshot(
    conn: &Connection,
    action_id: i64,
    account_id: &AccountId,
    plan: &Planned,
) -> Result<()> {
    let mut values = vec![
        Value::Integer(action_id),
        Value::Integer(i64::from(plan.remove_row)),
    ];
    values.extend(saved_values(&plan.before));
    values.push(Value::Text(plan.folders.clone()));
    values.push(Value::Text(plan.labels.clone()));
    debug_assert_eq!(values.len(), 27);
    conn.execute(
        "INSERT INTO local_action_messages (
            action_id, removed, message_id, remote_id, thread_id, proton_conversation_id,
            in_reply_to, references_header, from_name, from_email, subject,
            date_unix, snippet, body_text, unread, folders, labels, arrived_via,
            to_line, cc_line, bcc_line, has_attachment, list_id, reply_to_line,
            attachment_names, after_folders, after_labels
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
            ?26, ?27
         )",
        params_from_iter(values),
    )?;
    if plan.remove_row {
        // The delete below takes the parts with the row (a trigger), so keep
        // them for undo as stored.
        conn.execute(
            "INSERT INTO local_action_bodies
                (action_id, message_id, codec, html, plain, bulk_headers, parts_listed)
             SELECT ?1, message_id, codec, html, plain, bulk_headers, parts_listed
             FROM message_bodies
             WHERE account_id = ?2 AND message_id = ?3",
            params![action_id, account_id.as_str(), plan.before.message_id],
        )?;
        let columns = crate::store::bodies::PART_COLUMNS;
        conn.execute(
            &format!(
                "INSERT INTO local_action_parts (action_id, message_id, {columns})
                 SELECT ?1, message_id, {columns} FROM message_parts
                 WHERE account_id = ?2 AND message_id = ?3"
            ),
            params![action_id, account_id.as_str(), plan.before.message_id],
        )?;
    }
    let aliases = {
        let mut stmt = conn.prepare(
            "SELECT message_id, remote_id, folder FROM message_aliases
             WHERE account_id = ?1 AND canonical_message_id = ?2",
        )?;
        let rows = stmt.query_map(
            params![account_id.as_str(), plan.before.message_id],
            |row| {
                Ok(SavedAlias {
                    message_id: row.get(0)?,
                    remote_id: row.get(1)?,
                    folder: row.get(2)?,
                })
            },
        )?;
        let mut aliases = Vec::new();
        for row in rows {
            aliases.push(row?);
        }
        aliases
    };
    for alias in aliases {
        conn.execute(
            "INSERT INTO local_action_aliases (
                action_id, canonical_message_id, message_id, remote_id, folder
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                action_id,
                plan.before.message_id,
                alias.message_id,
                alias.remote_id,
                alias.folder,
            ],
        )?;
    }
    Ok(())
}

/// Put back what this action removed and drop what it added.
///
/// Tokens that showed up on the row after the action stay, in the order they
/// have now. When nothing else changed, the result follows the earlier list.
fn invert_tokens(
    current: &[String],
    before: &[String],
    after: &[String],
    folder: bool,
) -> Vec<String> {
    let same = |left: &str, right: &str| {
        if folder {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    };
    let has = |list: &[String], token: &str| list.iter().any(|item| same(item, token));
    let added = |token: &str| has(after, token) && !has(before, token);
    let removed = |token: &str| has(before, token) && !has(after, token);
    let mut out = Vec::new();
    let push = |out: &mut Vec<String>, token: &str| {
        if !has(out, token) {
            out.push(token.to_string());
        }
    };
    for token in before {
        if removed(token) || has(current, token) {
            push(&mut out, token);
        }
    }
    for token in current {
        if !added(token) {
            push(&mut out, token);
        }
    }
    out
}

fn record_tombstones(
    conn: &Connection,
    action_id: i64,
    account_id: &AccountId,
    plan: &Planned,
) -> Result<()> {
    for folder in removed_folders(plan) {
        conn.execute(
            "INSERT OR IGNORE INTO local_folder_tombstones
                (action_id, account_id, message_id, folder)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                action_id,
                account_id.as_str(),
                plan.before.message_id,
                folder
            ],
        )?;
    }
    Ok(())
}

fn removed_folders(plan: &Planned) -> Vec<String> {
    let before = split_tokens(&plan.before.folders);
    if plan.remove_row {
        return before;
    }
    let after = split_tokens(&plan.folders);
    before
        .into_iter()
        .filter(|folder| !after.iter().any(|kept| kept.eq_ignore_ascii_case(folder)))
        .collect()
}

/// Folder list to merge from a delta.
///
/// `None` means the row is gone and every folder on the delta was removed
/// locally, so the message stays deleted.
pub(crate) fn message_for_delta(conn: &Connection, message: &Message) -> Result<Option<Message>> {
    let blocked = tombstoned_folders(
        conn,
        &message.account_id,
        message.headers.message_id.as_str(),
    )?;
    if blocked.is_empty() {
        return Ok(Some(message.clone()));
    }
    let mut kept = Vec::new();
    let mut removed = 0;
    for folder in &message.folders {
        if blocked.iter().any(|tomb| tomb.eq_ignore_ascii_case(folder)) {
            removed += 1;
        } else {
            kept.push(folder.clone());
        }
    }
    if removed == 0 {
        return Ok(Some(message.clone()));
    }
    if kept.is_empty()
        && !message_row_exists(
            conn,
            &message.account_id,
            message.headers.message_id.as_str(),
        )?
    {
        fold_deleted_snapshot(conn, message)?;
        return Ok(None);
    }
    let mut updated = message.clone();
    updated.folders = kept;
    Ok(Some(updated))
}

fn tombstoned_folders(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Vec<String>> {
    let present: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'local_folder_tombstones'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if present.is_none() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT folder FROM local_folder_tombstones
         WHERE account_id = ?1 AND message_id = ?2",
    )?;
    let rows = stmt.query_map(params![account_id.as_str(), message_id], |row| row.get(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn fold_deleted_snapshot(conn: &Connection, message: &Message) -> Result<()> {
    let via = join_tokens(
        &message
            .arrived_via
            .iter()
            .map(|address| address.trim().to_lowercase())
            .filter(|address| !address.is_empty())
            .collect::<Vec<_>>(),
    );
    conn.execute(
        "UPDATE local_action_messages SET
            remote_id = ?1,
            from_name = ?2,
            from_email = ?3,
            subject = ?4,
            snippet = ?5,
            body_text = ?6,
            unread = ?7,
            arrived_via = ?8,
            to_line = ?9,
            cc_line = ?10,
            bcc_line = ?11,
            has_attachment = ?12
         WHERE message_id = ?13
           AND action_id IN (
               SELECT id FROM local_actions
               WHERE account_id = ?14 AND undone = 0
           )",
        params![
            message.remote_id.0,
            message.headers.from.name,
            message.headers.from.email,
            message.headers.subject,
            message.snippet,
            message.body_text,
            i64::from(message.unread),
            via,
            encode_addresses(&message.to),
            encode_addresses(&message.cc),
            encode_addresses(&message.bcc),
            i64::from(message.has_attachment),
            message.headers.message_id.as_str(),
            message.account_id.as_str(),
        ],
    )?;
    // The body above came from this copy, so its parts replace the saved ones.
    if let Some(parts) = &message.parts {
        let parts = crate::store::bodies::StoredParts::new(parts);
        if parts.has_body() {
            // An action recorded before Fell kept parts has no table yet.
            ensure_action_schema(conn)?;
            parts.save_for_undo(
                conn,
                &message.account_id,
                message.headers.message_id.as_str(),
            )?;
        }
    }
    Ok(())
}

/// A server copy of a row deleted in Fell changed its read state: write it
/// on the snapshot an undo would restore, as [`fold_deleted_snapshot`] does
/// for a whole copy. A store with no actions yet has nothing to update.
pub(crate) fn fold_flag_into_snapshots(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    unread: bool,
) -> Result<()> {
    let present: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'local_action_messages'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if present.is_none() {
        return Ok(());
    }
    conn.execute(
        "UPDATE local_action_messages SET unread = ?1
         WHERE message_id = ?2
           AND action_id IN (
               SELECT id FROM local_actions
               WHERE account_id = ?3 AND undone = 0
           )",
        params![i64::from(unread), message_id, account_id.as_str()],
    )?;
    Ok(())
}

pub(crate) fn message_row_exists(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM messages WHERE account_id = ?1 AND message_id = ?2
             UNION ALL
             SELECT 1 FROM message_aliases
             WHERE account_id = ?1 AND message_id = ?2
               AND canonical_message_id IN (
                   SELECT message_id FROM messages WHERE account_id = ?1
               )
             LIMIT 1",
            params![account_id.as_str(), message_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
}

fn insert_message_row(conn: &Connection, account_id: &AccountId, row: &SavedRow) -> Result<()> {
    let mut values = vec![Value::Text(account_id.as_str().to_string())];
    values.extend(saved_values(row));
    let fingerprint = match row.body_text.as_deref() {
        Some(text) if !text.trim().is_empty() => crate::message::body_fingerprint(text),
        _ => String::new(),
    };
    values.push(Value::Text(fingerprint));
    debug_assert_eq!(values.len(), 25);
    conn.execute(
        "INSERT INTO messages (
            account_id, message_id, remote_id, thread_id, proton_conversation_id,
            in_reply_to, references_header, from_name, from_email, subject,
            date_unix, snippet, body_text, unread, folders, labels, arrived_via,
            to_line, cc_line, bcc_line, has_attachment, list_id, reply_to_line,
            attachment_names, body_fingerprint
         ) VALUES (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25
         )",
        params_from_iter(values),
    )?;
    remove_message_embedding(conn, account_id.as_str(), &row.message_id)?;
    enqueue_message(conn, account_id.as_str(), &row.message_id)?;
    Ok(())
}

fn saved_values(row: &SavedRow) -> Vec<Value> {
    vec![
        Value::Text(row.message_id.clone()),
        Value::Text(row.remote_id.clone()),
        Value::Text(row.thread_id.clone()),
        opt_text(&row.proton_conversation_id),
        opt_text(&row.in_reply_to),
        Value::Text(row.references_header.clone()),
        opt_text(&row.from_name),
        Value::Text(row.from_email.clone()),
        Value::Text(row.subject.clone()),
        Value::Integer(row.date_unix),
        Value::Text(row.snippet.clone()),
        opt_text(&row.body_text),
        Value::Integer(row.unread),
        Value::Text(row.folders.clone()),
        Value::Text(row.labels.clone()),
        Value::Text(row.arrived_via.clone()),
        Value::Text(row.to_line.clone()),
        Value::Text(row.cc_line.clone()),
        Value::Text(row.bcc_line.clone()),
        Value::Integer(row.has_attachment),
        opt_text(&row.list_id),
        Value::Text(row.reply_to_line.clone()),
        Value::Text(row.attachment_names.clone()),
    ]
}

fn opt_text(value: &Option<String>) -> Value {
    match value {
        Some(text) => Value::Text(text.clone()),
        None => Value::Null,
    }
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, BackendKind};
    use crate::message::{Address, Message, MessageHeaders, MessageId, RemoteId};
    use crate::{MailboxDelta, SyncCursor};

    // An account no server has listed: each role is its default name.
    const FOLDER_ARCHIVE: &str = "Archive";
    const FOLDER_TRASH: &str = "Trash";
    const FOLDER_SENT: &str = "Sent";

    fn open(ids: &[(&str, &str)]) -> Store {
        let store = Store::open_in_memory().unwrap();
        for (id, email) in ids {
            store
                .upsert_account(&Account {
                    id: AccountId((*id).into()),
                    display_name: (*id).into(),
                    email: (*email).into(),
                    backend_kind: BackendKind::Bridge,
                })
                .unwrap();
        }
        store
    }

    fn account(id: &str) -> AccountId {
        AccountId(id.into())
    }

    fn sample(owner: &str, id: &str, subject: &str, body: &str) -> Message {
        Message {
            account_id: AccountId(owner.into()),
            headers: MessageHeaders {
                message_id: MessageId::new(id),
                proton_conversation_id: None,
                in_reply_to: None,
                references: Vec::new(),
                from: Address {
                    name: None,
                    email: "from@x".into(),
                },
                subject: subject.into(),
                date_unix: 1_700_000_000,
            },
            remote_id: RemoteId(format!("{owner}:{id}")),
            thread_id: ThreadId::from_message_id(&MessageId::new(id)),
            snippet: body.into(),
            body_text: Some(body.into()),
            unread: true,
            folders: vec![FOLDER_INBOX.into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
            reply_to: Vec::new(),
            attachment_names: Vec::new(),
            parts: None,
        }
    }

    fn messages(store: &Store, owner: &str, thread: &ThreadId) -> Vec<Message> {
        store.thread_messages(&account(owner), thread).unwrap()
    }

    fn subjects(store: &Store, owner: &str, query: &str) -> Vec<String> {
        store
            .search(&account(owner), query)
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.subject)
            .collect()
    }

    fn term_df(store: &Store, owner: &str, term: &str) -> i64 {
        store
            .connection()
            .query_row(
                "SELECT df FROM search_terms WHERE account_id = ?1 AND term = ?2",
                params![owner, term],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
            .unwrap_or(0)
    }

    fn text<T: std::fmt::Debug>(result: Result<T>) -> String {
        result.unwrap_err().to_string()
    }

    #[test]
    fn archive_keeps_sent_and_undo_restores() {
        let store = open(&[("a1", "a@x"), ("a2", "b@x")]);
        let mut root = sample("a1", "root@x", "Hello", "root body");
        root.folders = vec![FOLDER_INBOX.into(), FOLDER_SENT.into()];
        root.headers.date_unix = 10;
        let thread = store.upsert_message(&root).unwrap();
        let mut reply = sample("a1", "reply@x", "Re: Hello", "reply body");
        reply.folders = vec![FOLDER_SENT.into()];
        reply.headers.date_unix = 20;
        reply.headers.in_reply_to = Some(MessageId::new("root@x"));
        reply.headers.references = vec![MessageId::new("root@x")];
        assert_eq!(store.upsert_message(&reply).unwrap(), thread);

        let other = sample("a2", "root@x", "From B", "other body");
        let other_thread = store.upsert_message(&other).unwrap();

        assert_eq!(term_df(&store, "a1", "inbox"), 1);
        assert_eq!(term_df(&store, "a1", "sent"), 2);
        assert_eq!(term_df(&store, "a1", "archive"), 0);

        let action = store
            .archive_thread(&account("a1"), &thread, "INBOX")
            .unwrap();
        assert_eq!(action.kind, ActionKind::Archive);
        assert_eq!(action.summary, "archived 1 from INBOX");

        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].folders, vec![FOLDER_SENT, FOLDER_ARCHIVE]);
        assert_eq!(rows[1].folders, vec![FOLDER_SENT]);
        assert!(rows.iter().all(|message| message.labels.is_empty()));
        assert!(rows.iter().all(|message| message.unread));
        assert_eq!(rows[0].body_text.as_deref(), Some("root body"));
        assert_eq!(rows[0].thread_id, thread);

        assert!(
            store
                .threads_in_folder(&account("a1"), FOLDER_INBOX)
                .unwrap()
                .is_empty()
        );
        let sent = store
            .threads_in_folder(&account("a1"), FOLDER_SENT)
            .unwrap();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].thread_id, thread);
        assert_eq!(sent[0].message_count, 2);
        assert_eq!(sent[0].subject, "Re: Hello");
        let archived = store.threads_in_folder(&account("a1"), "archive").unwrap();
        assert_eq!(archived.len(), 1);
        assert_eq!(archived[0].message_count, 2);
        assert_eq!(archived[0].subject, "Re: Hello");
        assert_eq!(store.list_threads(&account("a1")).unwrap().len(), 1);

        assert!(subjects(&store, "a1", "in:INBOX").is_empty());
        assert_eq!(
            subjects(&store, "a1", "in:archive"),
            vec!["Hello".to_string()]
        );
        assert_eq!(
            subjects(&store, "a1", "in:Sent"),
            vec!["Re: Hello".to_string()]
        );
        assert_eq!(
            subjects(&store, "a2", "in:INBOX"),
            vec!["From B".to_string()]
        );
        assert_eq!(
            messages(&store, "a2", &other_thread)[0].folders,
            vec![FOLDER_INBOX]
        );

        assert_eq!(term_df(&store, "a1", "inbox"), 0);
        assert_eq!(term_df(&store, "a1", "archive"), 1);
        assert_eq!(term_df(&store, "a1", "sent"), 2);

        assert_eq!(store.undo(&account("a1")).unwrap(), action);
        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].folders, vec![FOLDER_INBOX, FOLDER_SENT]);
        assert_eq!(rows[1].folders, vec![FOLDER_SENT]);
        assert_eq!(term_df(&store, "a1", "inbox"), 1);
        assert_eq!(term_df(&store, "a1", "archive"), 0);
        assert_eq!(
            messages(&store, "a2", &other_thread)[0].headers.subject,
            "From B"
        );
        assert_eq!(text(store.undo(&account("a1"))), "nothing to undo");
    }

    #[test]
    fn move_keeps_sent_and_undo_restores() {
        let store = open(&[("a1", "a@x"), ("a2", "b@x")]);
        let mut root = sample("a1", "root@x", "Hello", "root body");
        root.folders = vec![FOLDER_INBOX.into(), FOLDER_SENT.into()];
        root.headers.date_unix = 10;
        let thread = store.upsert_message(&root).unwrap();
        let mut reply = sample("a1", "reply@x", "Re: Hello", "reply body");
        reply.folders = vec![FOLDER_SENT.into()];
        reply.headers.date_unix = 20;
        reply.headers.in_reply_to = Some(MessageId::new("root@x"));
        reply.headers.references = vec![MessageId::new("root@x")];
        assert_eq!(store.upsert_message(&reply).unwrap(), thread);

        let other = sample("a2", "root@x", "From B", "other body");
        let other_thread = store.upsert_message(&other).unwrap();

        assert_eq!(
            text(store.move_thread(&account("a1"), &thread, "INBOX", "inbox")),
            "move needs a different folder"
        );
        assert_eq!(
            text(store.move_thread(&account("a1"), &thread, "Drafts", "Archive")),
            "no messages in Drafts"
        );

        let action = store
            .move_thread(&account("a1"), &thread, "INBOX", "Projects")
            .unwrap();
        assert_eq!(action.kind, ActionKind::Move);
        assert_eq!(action.summary, "moved 1 from INBOX to Projects");
        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].folders, vec![FOLDER_SENT, "Projects"]);
        assert_eq!(rows[1].folders, vec![FOLDER_SENT]);
        assert!(
            store
                .threads_in_folder(&account("a1"), FOLDER_INBOX)
                .unwrap()
                .iter()
                .all(|summary| summary.thread_id != thread)
        );
        assert!(
            store
                .threads_in_folder(&account("a1"), "Projects")
                .unwrap()
                .iter()
                .any(|summary| summary.thread_id == thread)
        );
        assert_eq!(
            messages(&store, "a2", &other_thread)[0].folders,
            vec![FOLDER_INBOX]
        );

        store.undo(&account("a1")).unwrap();
        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].folders, vec![FOLDER_INBOX, FOLDER_SENT]);
        assert_eq!(rows[1].folders, vec![FOLDER_SENT]);
        assert!(
            store
                .threads_in_folder(&account("a1"), "Projects")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn label_stays_off_the_folder_list_and_undoes_in_order() {
        let store = open(&[("a1", "a@x")]);
        let mut root = sample("a1", "root@x", "Hello", "root body");
        root.labels = vec!["keep".into()];
        root.headers.date_unix = 10;
        let thread = store.upsert_message(&root).unwrap();
        let mut reply = sample("a1", "reply@x", "Re: Hello", "reply body");
        reply.headers.date_unix = 20;
        reply.headers.in_reply_to = Some(MessageId::new("root@x"));
        reply.headers.references = vec![MessageId::new("root@x")];
        assert_eq!(store.upsert_message(&reply).unwrap(), thread);
        let folders_before: Vec<_> = messages(&store, "a1", &thread)
            .into_iter()
            .map(|message| message.folders)
            .collect();

        let added = store
            .label_thread(&account("a1"), &thread, LabelChange::Add(" work ".into()))
            .unwrap();
        assert_eq!(added.summary, "added label work");
        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].labels, vec!["keep", "work"]);
        assert_eq!(rows[1].labels, vec!["work"]);
        assert_eq!(
            rows.into_iter()
                .map(|message| message.folders)
                .collect::<Vec<_>>(),
            folders_before
        );
        assert_eq!(
            subjects(&store, "a1", "label:work"),
            vec!["Re: Hello".to_string()]
        );
        assert_eq!(term_df(&store, "a1", "work"), 2);
        assert_eq!(term_df(&store, "a1", "keep"), 1);

        let removed = store
            .label_thread(&account("a1"), &thread, LabelChange::Remove("keep".into()))
            .unwrap();
        assert_eq!(removed.summary, "removed label keep");
        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].labels, vec!["work"]);
        assert_eq!(rows[1].labels, vec!["work"]);
        assert!(subjects(&store, "a1", "label:keep").is_empty());
        assert_eq!(term_df(&store, "a1", "keep"), 0);

        assert_eq!(store.undo(&account("a1")).unwrap().summary, removed.summary);
        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].labels, vec!["keep", "work"]);
        assert_eq!(rows[1].labels, vec!["work"]);
        assert_eq!(term_df(&store, "a1", "keep"), 1);

        assert_eq!(store.undo(&account("a1")).unwrap().id, added.id);
        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].labels, vec!["keep"]);
        assert!(rows[1].labels.is_empty());
        assert_eq!(term_df(&store, "a1", "work"), 0);
        assert_eq!(text(store.undo(&account("a1"))), "nothing to undo");

        let mut bare = sample("a1", "bare@x", "Bare", "bare body");
        bare.folders.clear();
        let bare_thread = store.upsert_message(&bare).unwrap();
        store
            .label_thread(
                &account("a1"),
                &bare_thread,
                LabelChange::Add("INBOX".into()),
            )
            .unwrap();
        assert!(messages(&store, "a1", &bare_thread)[0].folders.is_empty());
        assert_eq!(
            subjects(&store, "a1", "label:INBOX"),
            vec!["Bare".to_string()]
        );
        assert!(
            subjects(&store, "a1", "in:INBOX")
                .iter()
                .all(|subject| subject != "Bare")
        );
        assert_eq!(
            text(store.label_thread(
                &account("a1"),
                &bare_thread,
                LabelChange::Add("INBOX".into())
            )),
            "label is already applied"
        );
        store
            .label_thread(
                &account("a1"),
                &bare_thread,
                LabelChange::Add("工作".into()),
            )
            .unwrap();
        assert_eq!(
            subjects(&store, "a1", "label:工作"),
            vec!["Bare".to_string()]
        );
        store.undo(&account("a1")).unwrap();
        store.undo(&account("a1")).unwrap();
        assert!(messages(&store, "a1", &bare_thread)[0].labels.is_empty());
        assert!(messages(&store, "a1", &bare_thread)[0].folders.is_empty());
    }

    #[test]
    fn delete_moves_to_trash_then_expunge_restores_the_row() {
        let store = open(&[("a1", "a@x"), ("a2", "b@x")]);
        let mut root = sample("a1", "root@x", "Hello", "root body");
        root.folders = vec![FOLDER_INBOX.into(), FOLDER_SENT.into()];
        root.headers.date_unix = 10;
        let thread = store.upsert_message(&root).unwrap();
        let mut reply = sample("a1", "reply@x", "Re: Hello", "reply body");
        reply.folders = vec![FOLDER_SENT.into()];
        reply.headers.date_unix = 20;
        reply.headers.in_reply_to = Some(MessageId::new("root@x"));
        reply.headers.references = vec![MessageId::new("root@x")];
        assert_eq!(store.upsert_message(&reply).unwrap(), thread);

        let deleted = store
            .delete_thread(&account("a1"), &thread, FOLDER_INBOX)
            .unwrap();
        assert_eq!(deleted.summary, "deleted 1 from INBOX");
        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].folders, vec![FOLDER_SENT, FOLDER_TRASH]);
        assert_eq!(rows[1].folders, vec![FOLDER_SENT]);
        assert!(
            store
                .threads_in_folder(&account("a1"), FOLDER_INBOX)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store
                .threads_in_folder(&account("a1"), FOLDER_TRASH)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .threads_in_folder(&account("a1"), FOLDER_SENT)
                .unwrap()[0]
                .message_count,
            2
        );
        store.undo(&account("a1")).unwrap();
        assert_eq!(
            messages(&store, "a1", &thread)[0].folders,
            vec![FOLDER_INBOX, FOLDER_SENT]
        );

        let moved_sent = store
            .delete_thread(&account("a1"), &thread, FOLDER_SENT)
            .unwrap();
        assert_eq!(moved_sent.summary, "deleted 2 from Sent");
        let rows = messages(&store, "a1", &thread);
        assert_eq!(rows[0].folders, vec![FOLDER_INBOX, FOLDER_TRASH]);
        assert_eq!(rows[1].folders, vec![FOLDER_TRASH]);
        store.undo(&account("a1")).unwrap();
        assert_eq!(
            messages(&store, "a1", &thread)[1].folders,
            vec![FOLDER_SENT]
        );

        let mut note = sample("a1", "note@x", "Quarterly note", "bring a quinzhee");
        note.headers.proton_conversation_id = Some("conv-9".into());
        note.headers.in_reply_to = Some(MessageId::new("parent@x"));
        note.headers.references = vec![MessageId::new("parent@x")];
        note.headers.from.name = Some("Lee".into());
        note.unread = false;
        note.labels = vec!["notes".into()];
        note.arrived_via = vec!["alias@x".into()];
        note.to = vec![Address {
            name: Some("Ada".into()),
            email: "ada@x".into(),
        }];
        note.cc = vec![Address {
            name: None,
            email: "cc@x".into(),
        }];
        note.bcc = vec![Address {
            name: None,
            email: "bcc@x".into(),
        }];
        note.has_attachment = true;
        let note_thread = store.upsert_message(&note).unwrap();
        assert_eq!(note_thread, ThreadId::proton("conv-9"));
        let baseline = messages(&store, "a1", &note_thread);
        let other_thread = store
            .upsert_message(&sample("a2", "note@x", "From B", "other quinzhee"))
            .unwrap();
        assert_eq!(term_df(&store, "a1", "quinzhee"), 1);

        let to_trash = store
            .delete_thread(&account("a1"), &note_thread, "INBOX")
            .unwrap();
        assert_eq!(to_trash.summary, "deleted 1 from INBOX");
        assert_eq!(
            messages(&store, "a1", &note_thread)[0].folders,
            vec![FOLDER_TRASH]
        );
        assert_eq!(
            subjects(&store, "a1", "Quarterly"),
            vec!["Quarterly note".to_string()]
        );
        assert_eq!(
            messages(&store, "a2", &other_thread)[0].headers.subject,
            "From B"
        );

        let expunged = store
            .delete_thread(&account("a1"), &note_thread, "trash")
            .unwrap();
        assert_eq!(expunged.summary, "deleted 1 from trash");
        assert!(messages(&store, "a1", &note_thread).is_empty());
        assert!(subjects(&store, "a1", "Quarterly").is_empty());
        assert_eq!(term_df(&store, "a1", "quinzhee"), 0);
        assert_eq!(term_df(&store, "a2", "quinzhee"), 1);
        assert_eq!(subjects(&store, "a2", "From"), vec!["From B".to_string()]);

        assert_eq!(store.undo(&account("a1")).unwrap().id, expunged.id);
        let mut expected = baseline[0].clone();
        expected.folders = vec![FOLDER_TRASH.into()];
        assert_eq!(messages(&store, "a1", &note_thread), vec![expected]);
        assert_eq!(
            subjects(&store, "a1", "Quarterly"),
            vec!["Quarterly note".to_string()]
        );
        assert_eq!(term_df(&store, "a1", "quinzhee"), 1);
        assert_eq!(
            messages(&store, "a2", &other_thread)[0]
                .body_text
                .as_deref(),
            Some("other quinzhee")
        );

        store.undo(&account("a1")).unwrap();
        assert_eq!(messages(&store, "a1", &note_thread), baseline);
    }

    #[test]
    fn undo_one_account_leaves_the_other() {
        let store = open(&[("a1", "a@x"), ("a2", "b@x")]);
        let thread_a = store
            .upsert_message(&sample("a1", "same@x", "From A", "body a"))
            .unwrap();
        let thread_b = store
            .upsert_message(&sample("a2", "same@x", "From B", "body b"))
            .unwrap();
        store
            .archive_thread(&account("a1"), &thread_a, FOLDER_INBOX)
            .unwrap();
        store
            .archive_thread(&account("a2"), &thread_b, FOLDER_INBOX)
            .unwrap();
        store.undo(&account("a1")).unwrap();
        assert_eq!(
            messages(&store, "a1", &thread_a)[0].folders,
            vec![FOLDER_INBOX]
        );
        assert_eq!(
            messages(&store, "a1", &thread_a)[0].headers.subject,
            "From A"
        );
        assert_eq!(
            messages(&store, "a2", &thread_b)[0].folders,
            vec![FOLDER_ARCHIVE]
        );
        assert_eq!(
            messages(&store, "a2", &thread_b)[0].headers.subject,
            "From B"
        );
        assert!(subjects(&store, "a1", "in:Archive").is_empty());
        assert_eq!(
            subjects(&store, "a2", "in:Archive"),
            vec!["From B".to_string()]
        );
        store.undo(&account("a2")).unwrap();
        assert_eq!(
            messages(&store, "a2", &thread_b)[0].folders,
            vec![FOLDER_INBOX]
        );
        assert_eq!(text(store.undo(&account("a1"))), "nothing to undo");
    }

    #[test]
    fn rejected_actions_leave_mail_where_it_was() {
        let store = open(&[("a1", "a@x")]);
        let owner = account("a1");
        assert_eq!(
            text(store.archive_thread(&account("missing"), &ThreadId("t".into()), FOLDER_INBOX)),
            "unknown account missing"
        );
        let thread = store
            .upsert_message(&sample("a1", "m@x", "Hello", "body"))
            .unwrap();
        assert_eq!(
            text(store.archive_thread(&owner, &ThreadId("nope".into()), FOLDER_INBOX)),
            "no such thread"
        );
        assert_eq!(
            text(store.archive_thread(&owner, &thread, "  ")),
            "folder is empty"
        );
        assert_eq!(
            text(store.archive_thread(&owner, &thread, "a\u{1e}b")),
            "folder contains a reserved character"
        );
        assert_eq!(
            text(store.delete_thread(&owner, &thread, FOLDER_SENT)),
            "no messages in Sent"
        );
        assert_eq!(
            text(store.label_thread(&owner, &thread, LabelChange::Add("  ".into()))),
            "label is empty"
        );
        assert_eq!(
            text(store.label_thread(&owner, &thread, LabelChange::Add("a\u{1f}b".into()))),
            "label contains a reserved character"
        );
        assert_eq!(
            text(store.label_thread(&owner, &thread, LabelChange::Remove("work".into()))),
            "label is not on this thread"
        );
        assert_eq!(
            text(store.threads_in_folder(&owner, " ")),
            "folder is empty"
        );
        assert_eq!(text(store.undo(&owner)), "nothing to undo");
        assert_eq!(
            messages(&store, "a1", &thread)[0].folders,
            vec![FOLDER_INBOX]
        );

        store.archive_thread(&owner, &thread, "inbox").unwrap();
        assert_eq!(
            messages(&store, "a1", &thread)[0].folders,
            vec![FOLDER_ARCHIVE]
        );
        assert_eq!(
            text(store.archive_thread(&owner, &thread, FOLDER_ARCHIVE)),
            "archive moves messages into Archive"
        );
        assert_eq!(
            messages(&store, "a1", &thread)[0].folders,
            vec![FOLDER_ARCHIVE]
        );
        store.undo(&owner).unwrap();
        assert_eq!(
            messages(&store, "a1", &thread)[0].folders,
            vec![FOLDER_INBOX]
        );

        let mut cased = sample("a1", "case@x", "Cased", "cased body");
        cased.folders = vec!["Inbox".into()];
        let cased_thread = store.upsert_message(&cased).unwrap();
        store
            .archive_thread(&owner, &cased_thread, "inbox")
            .unwrap();
        assert_eq!(
            messages(&store, "a1", &cased_thread)[0].folders,
            vec![FOLDER_ARCHIVE]
        );
        let archived = store.threads_in_folder(&owner, "ARCHIVE").unwrap();
        assert!(
            archived
                .iter()
                .any(|summary| summary.thread_id == cased_thread)
        );
        assert!(archived.iter().all(|summary| summary.thread_id != thread));
        store.undo(&owner).unwrap();
        assert_eq!(
            messages(&store, "a1", &cased_thread)[0].folders,
            vec!["Inbox"]
        );

        store
            .label_thread(&owner, &thread, LabelChange::Add("work".into()))
            .unwrap();
        assert_eq!(
            text(store.label_thread(&owner, &thread, LabelChange::Add("work".into()))),
            "label is already applied"
        );
        store.undo(&owner).unwrap();
        assert!(messages(&store, "a1", &thread)[0].labels.is_empty());
        assert_eq!(text(store.undo(&owner)), "nothing to undo");
    }

    #[test]
    fn restoring_an_aliased_id_does_not_insert_a_second_row() {
        let store = open(&[("a1", "c@x")]);
        let mut echo = sample("a1", "list@x", "[list] Hello", "hello body");
        echo.list_id = Some("list.example".into());
        echo.remote_id = RemoteId("imap:inbox".into());
        echo.folders = vec!["INBOX".into()];
        let mut sent = sample("a1", "orig@x", "Hello", "hello body");
        sent.remote_id = RemoteId("imap:sent".into());
        sent.folders = vec!["Sent".into()];
        store.upsert_message(&echo).unwrap();
        store.upsert_message(&sent).unwrap();
        assert_eq!(
            store
                .thread_messages(
                    &account("a1"),
                    &ThreadId::from_message_id(&MessageId::new("list@x"))
                )
                .unwrap()
                .len(),
            1
        );
        restore_row(
            store.connection(),
            &account("a1"),
            &Snapshot {
                action_id: 0,
                removed: true,
                row: SavedRow {
                    message_id: "orig@x".into(),
                    remote_id: "imap:sent".into(),
                    thread_id: "msgid:orig@x".into(),
                    proton_conversation_id: None,
                    in_reply_to: None,
                    references_header: String::new(),
                    from_name: None,
                    from_email: "from@x".into(),
                    subject: "OLD SENT".into(),
                    date_unix: 1,
                    snippet: "old body".into(),
                    body_text: Some("old body".into()),
                    unread: 0,
                    folders: "Sent".into(),
                    labels: String::new(),
                    arrived_via: String::new(),
                    to_line: String::new(),
                    cc_line: String::new(),
                    bcc_line: String::new(),
                    has_attachment: 0,
                    list_id: None,
                    reply_to_line: String::new(),
                    attachment_names: String::new(),
                },
                after_folders: String::new(),
                after_labels: String::new(),
                aliases: Vec::new(),
            },
        )
        .unwrap();
        let count: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let kept = messages(
            &store,
            "a1",
            &ThreadId::from_message_id(&MessageId::new("list@x")),
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].headers.subject, "Hello");
        assert_eq!(kept[0].body_text.as_deref(), Some("hello body"));
        assert!(kept[0].unread);
        assert!(kept[0].folders.iter().any(|folder| folder == "INBOX"));
        assert!(kept[0].folders.iter().any(|folder| folder == "Sent"));
        let remote: String = store
            .connection()
            .query_row(
                "SELECT remote_id FROM message_aliases
                 WHERE account_id = 'a1' AND message_id = 'orig@x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remote, "imap:sent");
    }

    #[test]
    fn replacing_a_row_updates_the_body_fingerprint() {
        let store = open(&[("a1", "c@x")]);
        let message = sample("a1", "a@x", "Hello", "first body");
        store.upsert_message(&message).unwrap();
        let current = load_one(store.connection(), &account("a1"), "a@x")
            .unwrap()
            .unwrap();
        let mut snapshot = current.clone();
        snapshot.body_text = Some("second body".into());
        snapshot.snippet = "second body".into();
        replace_message_row(store.connection(), &account("a1"), &current, &snapshot).unwrap();
        let fingerprint: String = store
            .connection()
            .query_row(
                "SELECT body_fingerprint FROM messages WHERE message_id = 'a@x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(fingerprint, crate::message::body_fingerprint("second body"));
    }

    #[test]
    fn undo_of_collapsed_delete_restores_the_alias_and_list_id() {
        let store = open(&[("a1", "c@x")]);
        let mut echo = sample("a1", "list@x", "[list] Hello", "hello body");
        echo.list_id = Some("list.example".into());
        echo.remote_id = RemoteId("imap:inbox".into());
        echo.folders = vec![FOLDER_INBOX.into()];
        let mut sent = sample("a1", "orig@x", "Hello", "hello body");
        sent.remote_id = RemoteId("imap:sent".into());
        sent.folders = vec![FOLDER_SENT.into()];
        store.upsert_message(&echo).unwrap();
        let thread = store.upsert_message(&sent).unwrap();
        store
            .connection()
            .execute(
                "UPDATE messages SET folders = ?1 WHERE message_id = 'list@x'",
                params![FOLDER_TRASH],
            )
            .unwrap();
        store
            .delete_thread(&account("a1"), &thread, FOLDER_TRASH)
            .unwrap();
        let gone: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(gone, 0);
        let aliases: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM message_aliases", [], |row| row.get(0))
            .unwrap();
        assert_eq!(aliases, 0);
        store.undo(&account("a1")).unwrap();
        let restored = messages(&store, "a1", &thread);
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].headers.message_id.as_str(), "list@x");
        assert_eq!(restored[0].list_id.as_deref(), Some("list.example"));
        let (remote, folder): (String, String) = store
            .connection()
            .query_row(
                "SELECT remote_id, folder FROM message_aliases
                 WHERE account_id = 'a1' AND message_id = 'orig@x'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(remote, "imap:sent");
        assert_eq!(folder, FOLDER_SENT);
    }

    fn apply(store: &Store, message: Message) {
        store
            .apply_delta(
                &message.account_id.clone(),
                &MailboxDelta {
                    messages: vec![message],
                    cursor: None,
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
    }

    fn tombstones(store: &Store, message_id: &str) -> i64 {
        store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM local_folder_tombstones WHERE message_id = ?1",
                params![message_id],
                |row| row.get(0),
            )
            .unwrap()
    }

    fn saved_copies(store: &Store, needle: &str) -> i64 {
        store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM local_action_messages
                 WHERE IFNULL(body_text, '') LIKE '%' || ?1 || '%'
                    OR IFNULL(bcc_line, '') LIKE '%' || ?1 || '%'",
                params![needle],
                |row| row.get(0),
            )
            .unwrap()
    }

    #[test]
    fn delta_skips_a_folder_removed_by_archive_or_delete() {
        let store = open(&[("a1", "a@x"), ("a2", "b@x")]);
        let mut archived = sample("a1", "arch@x", "Archived", "archive body");
        archived.folders = vec![FOLDER_INBOX.into(), FOLDER_SENT.into()];
        let archive_thread = store.upsert_message(&archived).unwrap();
        store
            .archive_thread(&account("a1"), &archive_thread, FOLDER_INBOX)
            .unwrap();
        assert_eq!(tombstones(&store, "arch@x"), 1);

        let mut synced = messages(&store, "a1", &archive_thread)[0].clone();
        synced.folders = vec!["inbox".into(), "Projects".into()];
        synced.body_text = Some("from delta".into());
        synced.snippet = "from delta".into();
        apply(&store, synced);

        let row = &messages(&store, "a1", &archive_thread)[0];
        assert!(
            !row.folders
                .iter()
                .any(|folder| folder.eq_ignore_ascii_case("INBOX"))
        );
        assert_eq!(
            row.folders,
            vec![
                FOLDER_SENT.to_string(),
                FOLDER_ARCHIVE.to_string(),
                "Projects".to_string()
            ]
        );
        assert_eq!(row.body_text.as_deref(), Some("from delta"));
        assert!(subjects(&store, "a1", "in:INBOX").is_empty());
        assert_eq!(
            subjects(&store, "a1", "in:Projects"),
            vec!["Archived".to_string()]
        );

        let other = sample("a2", "arch@x", "From B", "other body");
        let other_thread = store.upsert_message(&other).unwrap();
        let mut other_delta = messages(&store, "a2", &other_thread)[0].clone();
        other_delta.folders = vec![FOLDER_INBOX.into()];
        apply(&store, other_delta);
        assert_eq!(
            messages(&store, "a2", &other_thread)[0].folders,
            vec![FOLDER_INBOX]
        );

        let deleted_thread = store
            .upsert_message(&sample("a1", "del@x", "Deleted", "delete body"))
            .unwrap();
        store
            .delete_thread(&account("a1"), &deleted_thread, "INBOX")
            .unwrap();
        let mut deleted_delta = messages(&store, "a1", &deleted_thread)[0].clone();
        deleted_delta.folders = vec![FOLDER_INBOX.into()];
        deleted_delta.body_text = Some("delete delta".into());
        apply(&store, deleted_delta);
        let deleted = &messages(&store, "a1", &deleted_thread)[0];
        assert_eq!(deleted.folders, vec![FOLDER_TRASH]);
        assert_eq!(deleted.body_text.as_deref(), Some("delete delta"));

        store
            .delete_thread(&account("a1"), &deleted_thread, FOLDER_TRASH)
            .unwrap();
        assert!(messages(&store, "a1", &deleted_thread).is_empty());
        let mut resurrect = sample("a1", "del@x", "Deleted", "should stay gone");
        resurrect.folders = vec!["inbox".into(), FOLDER_TRASH.into()];
        apply(&store, resurrect);
        assert!(messages(&store, "a1", &deleted_thread).is_empty());

        let undone = store.undo(&account("a1")).unwrap();
        assert_eq!(undone.kind, ActionKind::Delete);
        assert_eq!(
            messages(&store, "a1", &deleted_thread)[0].folders,
            vec![FOLDER_TRASH]
        );
        assert_eq!(tombstones(&store, "del@x"), 1);
        store.undo(&account("a1")).unwrap();
        assert_eq!(tombstones(&store, "del@x"), 0);
        assert_eq!(
            messages(&store, "a1", &deleted_thread)[0].folders,
            vec![FOLDER_INBOX]
        );
    }

    #[test]
    fn undo_of_expunge_replaces_a_reinserted_row_and_drops_the_copy() {
        let store = open(&[("a1", "a@x")]);
        let mut note = sample("a1", "note@x", "Quarterly note", "original secret body");
        note.bcc = vec![Address {
            name: None,
            email: "secret@x".into(),
        }];
        note.unread = false;
        note.labels = vec!["notes".into()];
        let thread = store.upsert_message(&note).unwrap();
        let baseline = messages(&store, "a1", &thread);
        store
            .delete_thread(&account("a1"), &thread, FOLDER_INBOX)
            .unwrap();
        let expunged = store
            .delete_thread(&account("a1"), &thread, FOLDER_TRASH)
            .unwrap();
        assert!(messages(&store, "a1", &thread).is_empty());
        assert!(saved_copies(&store, "original secret body") >= 1);
        assert!(saved_copies(&store, "secret@x") >= 1);

        let mut replacement = sample("a1", "note@x", "Replacement", "replacement body");
        replacement.bcc = vec![Address {
            name: None,
            email: "other@x".into(),
        }];
        replacement.unread = true;
        store.upsert_message(&replacement).unwrap();
        assert_eq!(
            messages(&store, "a1", &thread)[0].body_text.as_deref(),
            Some("replacement body")
        );

        assert_eq!(store.undo(&account("a1")).unwrap().id, expunged.id);
        let restored = &messages(&store, "a1", &thread)[0];
        assert_eq!(restored.body_text.as_deref(), Some("original secret body"));
        assert_eq!(restored.headers.subject, "Quarterly note");
        assert_eq!(restored.bcc, baseline[0].bcc);
        assert_eq!(restored.labels, vec!["notes"]);
        assert!(!restored.unread);
        assert_eq!(restored.folders, vec![FOLDER_TRASH]);
        assert_eq!(restored.thread_id, baseline[0].thread_id);
        assert_eq!(saved_copies_for_action(&store, expunged.id), 0);

        store.undo(&account("a1")).unwrap();
        assert_eq!(messages(&store, "a1", &thread), baseline);
        assert_eq!(saved_copies(&store, "original secret body"), 0);
        assert_eq!(saved_copies(&store, "secret@x"), 0);
    }

    fn html(text: &str) -> crate::MessageParts {
        crate::MessageParts {
            mime_parts: Vec::new(),
            html: Some(format!("<p>{text}</p>")),
            plain: Some(text.into()),
            bulk_headers: vec![("List-Id".into(), "<news.x>".into())],
        }
    }

    fn body_of(store: &Store, id: &str) -> Option<crate::MessageParts> {
        store
            .message_body(&account("a1"), &MessageId::new(id))
            .unwrap()
    }

    fn queued(store: &Store, id: &str) -> bool {
        store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM body_backfill WHERE message_id = ?1",
                [id],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            == 1
    }

    /// Delete the thread from Inbox if it is there, then expunge it from Trash.
    fn expunge(store: &Store, thread: &ThreadId) -> LocalAction {
        let _ = store.delete_thread(&account("a1"), thread, FOLDER_INBOX);
        store
            .delete_thread(&account("a1"), thread, FOLDER_TRASH)
            .unwrap()
    }

    #[test]
    fn undo_of_expunge_puts_back_the_parts_it_removed() {
        let store = open(&[("a1", "a@x")]);
        let mut note = sample("a1", "note@x", "Note", "first body");
        note.parts = Some(html("first"));
        let thread = store.upsert_message(&note).unwrap();

        // Expunge, then undo with nothing in between.
        expunge(&store, &thread);
        assert_eq!(body_of(&store, "note@x"), None);
        store.undo(&account("a1")).unwrap();
        assert_eq!(body_of(&store, "note@x"), Some(html("first")));

        // Expunge, then a sync writes the same id with other parts, then undo:
        // the restored row shows its own parts, not the later copy's.
        expunge(&store, &thread);
        let mut later = sample("a1", "note@x", "Later", "later body");
        later.parts = Some(html("later"));
        store.upsert_message(&later).unwrap();
        assert_eq!(body_of(&store, "note@x"), Some(html("later")));
        store.undo(&account("a1")).unwrap();
        let restored = &messages(&store, "a1", &thread)[0];
        assert_eq!(restored.body_text.as_deref(), Some("first body"));
        assert_eq!(body_of(&store, "note@x"), Some(html("first")));
        assert!(!queued(&store, "note@x"));
        let saved: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM local_action_bodies", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(saved, 0, "undo drops the saved parts");

        // A copy arriving only in the expunged folder keeps the row deleted
        // but replaces the saved body, so its parts come back with it.
        expunge(&store, &thread);
        let mut gone = sample("a1", "note@x", "Note", "server body");
        gone.folders = vec![FOLDER_TRASH.into()];
        gone.parts = Some(html("server"));
        store
            .apply_delta(
                &account("a1"),
                &MailboxDelta {
                    messages: vec![gone],
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        assert!(messages(&store, "a1", &thread).is_empty());
        store.undo(&account("a1")).unwrap();
        assert_eq!(
            messages(&store, "a1", &thread)[0].body_text.as_deref(),
            Some("server body")
        );
        assert_eq!(body_of(&store, "note@x"), Some(html("server")));
    }

    fn with_file(text: &str, name: &str) -> crate::MessageParts {
        crate::MessageParts {
            mime_parts: vec![crate::MimePart {
                section: "2".into(),
                role: crate::PartRole::Attachment,
                mime_type: "application/pdf".into(),
                filename: Some(name.into()),
                size: 10,
                encoded_size: 16,
                encoding: crate::TransferEncoding::Base64,
                content_id: None,
                disposition: crate::Disposition::Attachment,
                nested: false,
            }],
            ..html(text)
        }
    }

    fn files_of(store: &Store, id: &str) -> Option<Vec<String>> {
        store
            .message_attachments(&account("a1"), &MessageId::new(id))
            .unwrap()
            .map(|parts| parts.into_iter().filter_map(|part| part.filename).collect())
    }

    #[test]
    fn undo_of_expunge_puts_back_the_part_list() {
        let store = open(&[("a1", "a@x")]);
        let mut note = sample("a1", "note@x", "Note", "first body");
        note.parts = Some(with_file("first", "first.pdf"));
        let thread = store.upsert_message(&note).unwrap();
        assert_eq!(files_of(&store, "note@x"), Some(vec!["first.pdf".into()]));

        expunge(&store, &thread);
        assert_eq!(files_of(&store, "note@x"), None);
        let mut later = sample("a1", "note@x", "Later", "later body");
        later.parts = Some(with_file("later", "later.pdf"));
        store.upsert_message(&later).unwrap();
        assert_eq!(files_of(&store, "note@x"), Some(vec!["later.pdf".into()]));
        store.undo(&account("a1")).unwrap();
        assert_eq!(files_of(&store, "note@x"), Some(vec!["first.pdf".into()]));
        assert!(!queued(&store, "note@x"));
        let saved: i64 = store
            .connection()
            .query_row("SELECT COUNT(*) FROM local_action_parts", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(saved, 0, "undo drops the saved list");

        // A copy arriving only in the expunged folder replaces the saved list.
        expunge(&store, &thread);
        let mut gone = sample("a1", "note@x", "Note", "server body");
        gone.folders = vec![FOLDER_TRASH.into()];
        gone.parts = Some(with_file("server", "server.pdf"));
        store
            .apply_delta(
                &account("a1"),
                &MailboxDelta {
                    messages: vec![gone],
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        assert!(messages(&store, "a1", &thread).is_empty());
        store.undo(&account("a1")).unwrap();
        assert_eq!(files_of(&store, "note@x"), Some(vec!["server.pdf".into()]));
    }

    #[test]
    fn undo_of_expunge_requeues_a_row_whose_list_was_never_taken() {
        let store = open(&[("a1", "a@x")]);
        let mut note = sample("a1", "old@x", "Old", "old body");
        note.parts = Some(html("old"));
        let thread = store.upsert_message(&note).unwrap();
        // Stored before part lists were kept.
        store
            .connection()
            .execute_batch("UPDATE message_bodies SET parts_listed = 0")
            .unwrap();
        expunge(&store, &thread);
        store.undo(&account("a1")).unwrap();
        assert_eq!(body_of(&store, "old@x"), Some(html("old")));
        assert_eq!(files_of(&store, "old@x"), None);
        assert!(queued(&store, "old@x"), "queued to list its parts");
    }

    #[test]
    fn undo_of_expunge_requeues_a_row_that_had_no_parts() {
        let store = open(&[("a1", "a@x")]);
        let thread = store
            .upsert_message(&sample("a1", "old@x", "Old", "old body"))
            .unwrap();
        // Mail stored before parts were kept, waiting for the backfill.
        store
            .connection()
            .execute("INSERT INTO body_backfill VALUES ('a1', 'old@x')", [])
            .unwrap();
        expunge(&store, &thread);
        assert!(!queued(&store, "old@x"));
        let mut later = sample("a1", "old@x", "Old", "old body");
        later.parts = Some(html("later"));
        store.upsert_message(&later).unwrap();
        store.undo(&account("a1")).unwrap();
        assert_eq!(body_of(&store, "old@x"), None, "later parts removed");
        assert!(queued(&store, "old@x"), "queued to fetch again");
    }

    #[test]
    fn undo_keeps_folders_and_labels_added_after_the_action() {
        let store = open(&[("a1", "a@x")]);
        let mut message = sample("a1", "arch@x", "Archived", "archive body");
        message.folders = vec![FOLDER_INBOX.into(), FOLDER_SENT.into()];
        let thread = store.upsert_message(&message).unwrap();
        store
            .archive_thread(&account("a1"), &thread, FOLDER_INBOX)
            .unwrap();
        let mut synced = messages(&store, "a1", &thread)[0].clone();
        synced.folders = vec![FOLDER_SENT.into(), FOLDER_ARCHIVE.into(), "Projects".into()];
        apply(&store, synced);
        store.undo(&account("a1")).unwrap();
        assert_eq!(
            messages(&store, "a1", &thread)[0].folders,
            vec![
                FOLDER_INBOX.to_string(),
                FOLDER_SENT.to_string(),
                "Projects".to_string()
            ]
        );

        let labeled = store
            .upsert_message(&sample("a1", "lab@x", "Labeled", "label body"))
            .unwrap();
        store
            .label_thread(&account("a1"), &labeled, LabelChange::Add("work".into()))
            .unwrap();
        let mut later = messages(&store, "a1", &labeled)[0].clone();
        later.labels = vec!["later".into()];
        apply(&store, later);
        store.undo(&account("a1")).unwrap();
        assert_eq!(messages(&store, "a1", &labeled)[0].labels, vec!["later"]);
        assert_eq!(
            messages(&store, "a1", &labeled)[0].folders,
            vec![FOLDER_INBOX]
        );
    }

    #[test]
    fn deleted_row_absorbs_a_later_body_before_the_cursor_moves() {
        let store = open(&[("a1", "a@x")]);
        let thread = store
            .upsert_message(&sample("a1", "note@x", "Original", "original body"))
            .unwrap();
        store
            .delete_thread(&account("a1"), &thread, FOLDER_INBOX)
            .unwrap();
        store
            .delete_thread(&account("a1"), &thread, FOLDER_TRASH)
            .unwrap();
        assert!(messages(&store, "a1", &thread).is_empty());

        let mut delta = sample("a1", "note@x", "Server subject", "server body");
        delta.folders = vec![FOLDER_INBOX.into(), FOLDER_TRASH.into()];
        delta.unread = false;
        store
            .apply_delta(
                &account("a1"),
                &MailboxDelta {
                    messages: vec![delta],
                    cursor: Some(SyncCursor {
                        account_key: "a1".into(),
                        token: "cursor-2".into(),
                    }),
                    flags: Vec::new(),
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        assert!(messages(&store, "a1", &thread).is_empty());
        assert_eq!(
            store.sync_cursor(&account("a1")).unwrap().unwrap().token,
            "cursor-2"
        );

        store.undo(&account("a1")).unwrap();
        let restored = &messages(&store, "a1", &thread)[0];
        assert_eq!(restored.body_text.as_deref(), Some("server body"));
        assert_eq!(restored.headers.subject, "Server subject");
        assert!(!restored.unread);
        assert_eq!(restored.folders, vec![FOLDER_TRASH]);
    }

    fn saved_copies_for_action(store: &Store, action_id: i64) -> i64 {
        store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM local_action_messages WHERE action_id = ?1",
                params![action_id],
                |row| row.get(0),
            )
            .unwrap()
    }
}
