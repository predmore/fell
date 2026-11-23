//! Check the folder state under the same transaction as undo.

use rusqlite::{Connection, OptionalExtension, params};

use super::activity::{ActivityBlocker, BLOCKER_MAILBOX_CHANGED, map_blocker};
use super::{RecordedAction, move_folders, role_with_alias, same_folder};
use crate::Result;
use crate::folders::{FolderRole, FolderRoles};
use crate::store::{join_tokens, listed_folder_roles, split_field};

/// Scope the guard to the same message fields as the undo writer.
#[derive(Clone, Copy)]
enum Field {
    Folder,
    Label,
    Read(i64),
    Star(i64),
    Snooze,
}

impl Field {
    fn for_action(kind: &str) -> Option<Self> {
        Some(match kind {
            "archive" | "move" => Self::Folder,
            "label" | "unlabel" => Self::Label,
            "mark_read" => Self::Read(0),
            "mark_unread" => Self::Read(1),
            "star" => Self::Star(1),
            "unstar" => Self::Star(0),
            "snooze" | "unsnooze" => Self::Snooze,
            _ => return None,
        })
    }

    fn agent_kinds(self) -> &'static str {
        match self {
            Self::Folder => "'archive', 'move'",
            Self::Label => "'label', 'unlabel'",
            Self::Read(_) => "'mark_read', 'mark_unread'",
            Self::Star(_) => "'star', 'unstar'",
            Self::Snooze => "'snooze', 'unsnooze'",
        }
    }

    fn local_scope(self) -> Option<(&'static str, &'static str, &'static str)> {
        Some(match self {
            Self::Folder => (
                "local_action_messages",
                "'archive', 'move', 'delete', 'spam', 'not_spam'",
                "(s.after_folders IS NULL OR s.folders <> s.after_folders)
                 AND (l.removed OR l.folders <> l.after_folders)",
            ),
            Self::Label => (
                "local_action_messages",
                "'label'",
                "l.labels <> l.after_labels",
            ),
            Self::Read(_) => ("local_action_flags", "'read', 'unread'", "1"),
            Self::Star(_) => (
                "local_action_flags",
                "'star', 'unstar', 'delete', 'spam', 'not_spam'",
                "s.starred IS NOT NULL",
            ),
            Self::Snooze => return None,
        })
    }
}

pub(super) fn blocker(
    conn: &Connection,
    action: &RecordedAction,
) -> Result<Option<ActivityBlocker>> {
    let Some(field) = Field::for_action(&action.kind) else {
        return Ok(None);
    };
    let (agent, local) = newer_actions(conn, action, field)?;
    let outbox = if matches!(field, Field::Folder) {
        pending_move(conn, action)?
    } else {
        None
    };
    if let Some(blocker) = agent
        .into_iter()
        .chain(local.or(outbox))
        .max_by_key(|b| b.created_unix)
    {
        return Ok(Some(blocker));
    }

    let unchanged = match field {
        Field::Folder => folder_snapshots(conn, action)?
            .iter()
            .all(FolderSnapshot::unchanged),
        Field::Label => crate::store::keywords::agent_label_unchanged(
            conn,
            &action.account_id,
            action.id,
            &action.summary,
        )?,
        Field::Read(expected) => flag_unchanged(conn, action, "unread", expected)?,
        Field::Star(expected) => flag_unchanged(conn, action, "starred", expected)?,
        // A snooze with no open replacement can be retired without a write.
        Field::Snooze => true,
    };
    Ok((!unchanged).then(|| ActivityBlocker {
        id: action.id,
        actor: String::new(),
        kind: BLOCKER_MAILBOX_CHANGED.into(),
        created_unix: 0,
    }))
}

fn newer_actions(
    conn: &Connection,
    action: &RecordedAction,
    field: Field,
) -> Result<(Option<ActivityBlocker>, Option<ActivityBlocker>)> {
    // Label undo resolves merged aliases before changing a label membership.
    // Include the full alias class when looking for overlapping label edits.
    let snapshots = if matches!(field, Field::Label) {
        "WITH canonical AS (
             SELECT s.*, COALESCE(a.canonical_message_id, s.message_id) AS canonical_id
             FROM agent_action_messages s
             LEFT JOIN message_aliases a ON a.account_id = ?2 AND a.message_id = s.message_id
             WHERE s.action_id = ?1
         ), targets AS (
             SELECT canonical_id AS target_id, c.* FROM canonical c
             UNION ALL
             SELECT a.message_id AS target_id, c.* FROM canonical c
             JOIN message_aliases a ON a.account_id = ?2 AND a.canonical_message_id = c.canonical_id
         )"
    } else {
        "WITH targets AS (
             SELECT message_id AS target_id, s.* FROM agent_action_messages s WHERE s.action_id = ?1
         )"
    };
    let overlap = match field {
        Field::Folder => "(((s.after_folders IS NULL OR s.folders <> s.after_folders)
                            AND (later.after_folders IS NULL OR later.folders <> later.after_folders))
                           OR ((s.after_archived IS NULL OR s.archived <> s.after_archived)
                            AND (later.after_archived IS NULL OR later.archived <> later.after_archived)))",
        Field::Star(_) => "s.starred IS NOT NULL",
        _ => "1",
    };
    let args = params![action.id, action.account_id.as_str()];
    let agent_actions = conn
        .prepare_cached(&format!(
            "{snapshots}
         SELECT n.id, n.actor, n.kind, n.created_unix FROM targets s
         JOIN agent_action_messages later ON later.message_id = s.target_id AND later.action_id > ?1
         JOIN agent_actions n ON n.id = later.action_id AND n.account_id = ?2 AND n.undone = 0
         WHERE n.kind IN ({kinds}) AND {overlap}
         ORDER BY n.id DESC LIMIT 1",
            kinds = field.agent_kinds(),
        ))?
        .query_row(args, |row| map_blocker(row, 0))
        .optional()?
        .flatten();

    // The local undo log is initialized lazily. IDs order new actions across
    // logs even within one second. Older snapshots have only their timestamp.
    let mut local_actions = None;
    if let Some((table, kinds, overlap)) = field.local_scope()
        && conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = ?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )?
    {
        local_actions = conn
            .prepare_cached(&format!(
                "{snapshots}
             SELECT n.id, 'local', n.kind, n.created_unix FROM targets s
             JOIN agent_actions a ON a.id = s.action_id
             JOIN {table} l ON l.message_id = s.target_id
             JOIN local_actions n ON n.id = l.action_id AND n.account_id = ?2 AND n.undone = 0
             WHERE n.kind IN ({kinds}) AND {overlap}
               AND n.id > COALESCE(a.local_action_floor, 0)
               AND (a.local_action_floor IS NOT NULL OR n.created_unix >= a.created_unix)
             ORDER BY n.id DESC LIMIT 1"
            ))?
            .query_row(args, |row| map_blocker(row, 0))
            .optional()?
            .flatten();
    }
    Ok((agent_actions, local_actions))
}

fn pending_move(conn: &Connection, action: &RecordedAction) -> Result<Option<ActivityBlocker>> {
    Ok(conn
        .prepare_cached(
            "SELECT n.id, 'outbox', n.kind, n.created_unix FROM agent_actions a
         JOIN agent_action_messages s ON s.action_id = a.id
         JOIN server_outbox n ON n.message_id = s.message_id AND n.account_id = ?2
         WHERE a.id = ?1 AND n.kind = 'move' AND n.state = 'pending'
           AND (s.after_folders IS NULL OR s.folders <> s.after_folders)
           AND n.id > COALESCE(a.outbox_floor, 0)
           AND (a.outbox_floor IS NOT NULL OR n.created_unix >= a.created_unix)
         ORDER BY n.id DESC LIMIT 1",
        )?
        .query_row(params![action.id, action.account_id.as_str()], |row| {
            map_blocker(row, 0)
        })
        .optional()?
        .flatten())
}

fn flag_unchanged(
    conn: &Connection,
    action: &RecordedAction,
    column: &str,
    expected: i64,
) -> Result<bool> {
    Ok(!conn
        .prepare_cached(&format!(
            "SELECT EXISTS (
            SELECT 1 FROM agent_action_messages s
            JOIN messages m ON m.account_id = ?2 AND m.message_id = s.message_id
            WHERE s.action_id = ?1 AND s.{column} IS NOT NULL
              AND s.{column} <> ?3 AND m.{column} IS NOT ?3
        )"
        ))?
        .query_row(
            params![action.id, action.account_id.as_str(), expected],
            |row| row.get::<_, bool>(0),
        )?)
}

struct FolderSnapshot {
    message_id: String,
    before: (String, bool),
    written: Option<(String, bool)>,
    live: (String, bool),
}

impl FolderSnapshot {
    fn unchanged(&self) -> bool {
        self.written.as_ref().is_some_and(|after| {
            (same_folders(&self.before.0, &after.0) || same_folders(&self.live.0, &after.0))
                && (self.before.1 == after.1 || self.live.1 == after.1)
        })
    }
}

pub(super) fn restore_folders(conn: &Connection, action: &RecordedAction) -> Result<()> {
    for row in folder_snapshots(conn, action)? {
        let Some(after) = row.written else {
            continue;
        };
        if !same_folders(&row.before.0, &after.0) {
            conn.prepare_cached(
                "UPDATE messages SET folders = ?3 WHERE account_id = ?1 AND message_id = ?2",
            )?
            .execute(params![
                action.account_id.as_str(),
                row.message_id,
                row.before.0
            ])?;
        }
        if row.before.1 != after.1 {
            conn.prepare_cached(
                "UPDATE messages SET archived = ?3 WHERE account_id = ?1 AND message_id = ?2",
            )?
            .execute(params![
                action.account_id.as_str(),
                row.message_id,
                row.before.1
            ])?;
        }
    }
    Ok(())
}

fn folder_snapshots(conn: &Connection, action: &RecordedAction) -> Result<Vec<FolderSnapshot>> {
    let mut statement = conn.prepare_cached(
        "SELECT s.message_id, s.folders, s.archived, s.after_folders, s.after_archived, m.folders, m.archived
         FROM agent_action_messages s
         JOIN messages m ON m.account_id = ?2 AND m.message_id = s.message_id
         WHERE s.action_id = ?1",
    )?;
    let mut rows = statement.query(params![action.id, action.account_id.as_str()])?;
    let mut snapshots = Vec::new();
    while let Some(row) = rows.next()? {
        let before: (String, bool) = (row.get(1)?, row.get(2)?);
        let written = match (
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<bool>>(4)?,
        ) {
            (Some(folders), Some(archived)) => Some((folders, archived)),
            _ => legacy_folder_result(conn, action, &before.0, before.1)?,
        };
        snapshots.push(FolderSnapshot {
            message_id: row.get(0)?,
            before,
            written,
            live: (row.get(5)?, row.get(6)?),
        });
    }
    Ok(snapshots)
}

fn same_folders(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    let mut a = split_field(a);
    let mut b = split_field(b);
    a.sort_unstable();
    b.sort_unstable();
    a == b
}

// Old archive snapshots have a fixed transformation. Old moves store their
// source and destination in the summary. A folder can itself contain " to ",
// so only reconstruct when exactly one split could have moved the saved row.
fn legacy_folder_result(
    conn: &Connection,
    action: &RecordedAction,
    before: &str,
    archived: bool,
) -> Result<Option<(String, bool)>> {
    let mut folders = split_field(before);
    if action.kind == "archive" {
        folders.retain(|folder| !folder.eq_ignore_ascii_case("INBOX"));
        return Ok(Some((join_tokens(&folders), true)));
    }
    let Some(path) = action.summary.strip_prefix("moved thread from ") else {
        return Ok(None);
    };
    let mut candidates = path.match_indices(" to ").filter_map(|(i, separator)| {
        let (from, to) = (&path[..i], &path[i + separator.len()..]);
        let implied_inbox = same_folder(from, "INBOX") && folders.is_empty() && !archived;
        (implied_inbox || folders.iter().any(|folder| same_folder(folder, from)))
            .then_some((from, to))
    });
    let Some((from, to)) = candidates.next() else {
        return Ok(None);
    };
    if candidates.next().is_some() {
        return Ok(None);
    }
    let roles = listed_folder_roles(conn, &action.account_id)?
        .unwrap_or_else(|| FolderRoles::from_names(&[to.to_owned()]));
    let to_archive =
        role_with_alias(&roles, to) == Some(FolderRole::Archive) || same_folder(to, "Archive");
    let mut archived = archived;
    move_folders(&mut folders, &mut archived, from, to, to_archive);
    Ok(Some((join_tokens(&folders), archived)))
}
