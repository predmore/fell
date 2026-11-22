//! Account-scoped activity pages and the interactive undo guard.

use rusqlite::{Connection, OptionalExtension, params};

use super::{ACTION_COLS, RecordedAction, UndoSelection, map_action};
use crate::{AccountId, Error, Result, Store};

pub const BULK_UNDO_THRESHOLD: u32 = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityDirection {
    Latest,
    Older(i64),
    Newer(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentActivity {
    pub action: RecordedAction,
    pub subject: Option<String>,
    pub conversations: u32,
}

#[derive(Clone, Debug, Default)]
pub struct AgentActivityPage {
    pub entries: Vec<AgentActivity>,
    pub has_newer: bool,
    pub has_older: bool,
}

const AGENT_ROWS: &str = "account_id = ?1 AND actor GLOB 'agent:*'";

// Older rows have no saved target. Recover it from the action's surviving
// message snapshots, review draft, or thread note. New rows keep their
// subject even after undo or after the underlying mail has been removed.
const CONVERSATIONS: &str = "COALESCE(conversation_count, (
    SELECT MAX(1, COUNT(DISTINCT m.thread_id))
    FROM agent_action_messages s
    JOIN messages m ON m.account_id = a.account_id AND m.message_id = s.message_id
    WHERE s.action_id = a.id
))";
const SUBJECT: &str = "COALESCE(target_subject,
    (SELECT d.subject
     FROM agent_action_drafts s JOIN review_drafts d ON d.id = s.draft_id
     WHERE s.action_id = a.id LIMIT 1),
    (SELECT m.subject FROM thread_notes n
     JOIN messages m ON m.account_id = n.account_id AND m.thread_id = n.thread_id
     WHERE n.id = a.id ORDER BY m.date_unix, m.rowid LIMIT 1),
    (SELECT m.subject FROM agent_action_messages s
     JOIN messages m ON m.account_id = a.account_id AND m.message_id = s.message_id
     WHERE s.action_id = a.id ORDER BY m.date_unix, m.rowid LIMIT 1))";

impl Store {
    /// A bounded page ordered by descending action id, with stable cursors
    /// even when another process logs an action while the page is open.
    pub fn agent_activity(
        &self,
        account: &AccountId,
        agent: Option<&str>,
        direction: ActivityDirection,
        limit: usize,
    ) -> Result<AgentActivityPage> {
        let actor = agent.map(|name| format!("agent:{name}"));
        let (bound, order, anchor) = match direction {
            ActivityDirection::Latest => ("", "DESC", None),
            ActivityDirection::Older(id) => ("AND id < ?3", "DESC", Some(id)),
            ActivityDirection::Newer(id) => ("AND id > ?3", "ASC", Some(id)),
        };
        let limit = limit.clamp(1, 100);
        let sql_limit = (limit + 1) as i64;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ACTION_COLS}, {SUBJECT}, {CONVERSATIONS}
             FROM agent_actions a
             WHERE {AGENT_ROWS} AND (?2 IS NULL OR actor = ?2) {bound}
             ORDER BY id {order} LIMIT ?4"
        ))?;
        let entries =
            stmt.query_map(params![account.as_str(), actor, anchor, sql_limit], |row| {
                Ok(AgentActivity {
                    action: map_action(row)?,
                    subject: row.get(7)?,
                    conversations: row.get(8)?,
                })
            })?;
        let mut entries = entries.collect::<rusqlite::Result<Vec<_>>>()?;
        let more = entries.len() > limit;
        entries.truncate(limit);
        let (has_newer, has_older) = match direction {
            ActivityDirection::Latest => (false, more),
            ActivityDirection::Older(_) => (true, more),
            ActivityDirection::Newer(_) => {
                entries.reverse();
                (more, true)
            }
        };
        Ok(AgentActivityPage {
            entries,
            has_newer,
            has_older,
        })
    }

    /// Undo the selected activity on this account through the same write
    /// transaction and grant check as other agent undos.
    pub fn undo_agent_activity(
        &self,
        account: &AccountId,
        id: i64,
        confirmed_bulk: Option<u32>,
    ) -> Result<RecordedAction> {
        self.undo_checked(
            account,
            None,
            UndoSelection::Activity { id, confirmed_bulk },
        )
    }
}

pub(super) fn check_undo(
    conn: &Connection,
    account: &AccountId,
    id: i64,
    confirmed_bulk: Option<u32>,
) -> Result<RecordedAction> {
    let (action, conversations) = conn
        .query_row(
            &format!(
                "SELECT {ACTION_COLS}, {CONVERSATIONS} FROM agent_actions a
                  WHERE {AGENT_ROWS} AND id = ?2"
            ),
            params![account.as_str(), id],
            |row| Ok((map_action(row)?, row.get::<_, u32>(7)?)),
        )
        .optional()?
        .ok_or_else(|| Error::Action("Action is no longer available".into()))?;
    if action.undone {
        return Err(Error::Action("Action already undone".into()));
    }
    let newer: bool = conn.query_row(
        &format!(
            "SELECT EXISTS (
             SELECT 1 FROM agent_actions WHERE {AGENT_ROWS} AND undone = 0 AND id > ?2
         )"
        ),
        params![account.as_str(), id],
        |row| row.get(0),
    )?;
    if newer {
        return Err(Error::Action("Undo the newer action first".into()));
    }
    if conversations > BULK_UNDO_THRESHOLD
        && confirmed_bulk.is_none_or(|approved| conversations > approved)
    {
        return Err(Error::Action("Confirm undo for this bulk action".into()));
    }
    Ok(action)
}
