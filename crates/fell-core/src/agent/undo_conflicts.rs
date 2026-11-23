//! Check the folder state under the same transaction as undo.

use rusqlite::{Connection, OptionalExtension, params};

use super::activity::{ActivityBlocker, BLOCKER_MAILBOX_CHANGED, map_blocker};
use super::{RecordedAction, move_folders, role_with_alias, same_folder};
use crate::Result;
use crate::folders::{FolderRole, FolderRoles};
use crate::store::{join_tokens, listed_folder_roles, split_field};

pub(super) fn folder_blocker(
    conn: &Connection,
    action: &RecordedAction,
) -> Result<Option<ActivityBlocker>> {
    let args = params![action.id, action.account_id.as_str()];
    let agent = conn.prepare_cached(
        "SELECT n.id, n.actor, n.kind, n.created_unix FROM agent_action_messages s
         JOIN agent_action_messages newer ON newer.message_id = s.message_id AND newer.action_id > ?1
         JOIN agent_actions n ON n.id = newer.action_id AND n.account_id = ?2 AND n.undone = 0
         WHERE s.action_id = ?1 AND n.kind IN ('archive', 'move')
         ORDER BY n.id DESC LIMIT 1",
    )?.query_row(args, |row| map_blocker(row, 0)).optional()?.flatten();

    // The local undo log is initialized lazily. The watermarks provide ordering
    // across logs even when actions share the same wall-clock second.
    let has_local: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE name = 'local_actions')",
        [],
        |row| row.get(0),
    )?;
    let local = if has_local {
        conn.prepare_cached(
            "SELECT n.id, 'local', n.kind, n.created_unix FROM agent_actions a
             JOIN agent_action_messages s ON s.action_id = a.id
             JOIN local_action_messages l ON l.message_id = s.message_id
             JOIN local_actions n ON n.id = l.action_id AND n.account_id = ?2 AND n.undone = 0
             WHERE a.id = ?1 AND n.kind IN ('archive', 'move')
               AND (n.id > a.local_action_floor OR
                    (a.local_action_floor IS NULL AND n.created_unix >= a.created_unix))
             ORDER BY n.id DESC LIMIT 1",
        )?
        .query_row(args, |row| map_blocker(row, 0))
        .optional()?
        .flatten()
    } else {
        None
    };
    let outbox = conn
        .prepare_cached(
            "SELECT n.id, 'outbox', n.kind, n.created_unix FROM agent_actions a
         JOIN agent_action_messages s ON s.action_id = a.id
         JOIN server_outbox n ON n.message_id = s.message_id AND n.account_id = ?2
         WHERE a.id = ?1 AND n.kind = 'move' AND n.state = 'pending'
           AND (n.id > a.outbox_floor OR
                (a.outbox_floor IS NULL AND n.created_unix >= a.created_unix))
         ORDER BY n.id DESC LIMIT 1",
        )?
        .query_row(args, |row| map_blocker(row, 0))
        .optional()?
        .flatten();
    if let Some(blocker) = agent
        .into_iter()
        .chain(local.or(outbox))
        .max_by_key(|b| b.created_unix)
    {
        return Ok(Some(blocker));
    }

    let mut statement = conn.prepare_cached(
        "SELECT s.folders, s.archived, s.after_folders, s.after_archived, m.folders, m.archived
         FROM agent_action_messages s
         JOIN messages m ON m.account_id = ?2 AND m.message_id = s.message_id
         WHERE s.action_id = ?1",
    )?;
    let mut rows = statement.query(args)?;
    while let Some(row) = rows.next()? {
        let live: (String, bool) = (row.get(4)?, row.get(5)?);
        let written = match (
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<bool>>(3)?,
        ) {
            (Some(folders), Some(archived)) => Some((folders, archived)),
            _ => legacy_folder_result(conn, action, row.get(0)?, row.get(1)?)?,
        };
        if !written.is_some_and(|(folders, archived)| {
            archived == live.1 && same_folders(&folders, &live.0)
        }) {
            return Ok(Some(ActivityBlocker {
                id: action.id,
                actor: String::new(),
                kind: BLOCKER_MAILBOX_CHANGED.into(),
                created_unix: 0,
            }));
        }
    }
    Ok(None)
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
    before: String,
    archived: bool,
) -> Result<Option<(String, bool)>> {
    let mut folders = split_field(&before);
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
