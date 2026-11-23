//! Account-scoped activity pages and the interactive undo guard.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::{
    ACTION_COLS, RecordedAction, SNOOZE_RETIRED, UndoSelection, WriteScope, map_action, read_grant,
    require_token,
};
use crate::{AccountId, Error, Result, Store};

pub const BULK_UNDO_THRESHOLD: u32 = 10;
pub const BLOCKER_MAILBOX_CHANGED: &str = "mailbox_changed";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivityDirection {
    Latest,
    Older(i64),
    Newer(i64),
    At(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivityBlocker {
    pub id: i64,
    pub actor: String,
    pub kind: String,
    pub created_unix: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActivityGrant {
    pub enabled: bool,
    pub scope: &'static str,
}

impl ActivityGrant {
    fn missing(grant: crate::AgentGrant, scope: WriteScope) -> Option<Self> {
        let (allowed, scope) = scope.permission(grant);
        (!grant.enabled || !allowed).then_some(Self {
            enabled: grant.enabled,
            scope,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentActivity {
    pub action: RecordedAction,
    pub subject: Option<String>,
    pub conversations: u32,
    pub blocker: Option<ActivityBlocker>,
    pub draft_discarded: bool,
    pub snooze_retired: bool,
    pub missing_grant: Option<ActivityGrant>,
}

#[derive(Clone, Debug, Default)]
pub struct AgentActivityPage {
    pub entries: Vec<AgentActivity>,
    pub has_newer: bool,
    pub has_older: bool,
}

const AGENT_ROWS: &str = "account_id = ?1 AND actor GLOB 'agent:*'";

// A discarded review draft is reversible through the local discard action,
// so leave its history intact and ignore it in the agent's undo order.
fn discarded_draft(action: &str) -> String {
    format!(
        "{action}.kind = 'draft' AND EXISTS (
            SELECT 1 FROM agent_action_drafts s
            JOIN compose_drafts d ON d.id = 'r' || s.draft_id AND d.account_id = {action}.account_id
            WHERE s.action_id = {action}.id AND d.discarded_action_id IS NOT NULL
        )"
    )
}

fn newer_action() -> String {
    format!(
        "SELECT n.id FROM agent_actions n
         WHERE n.account_id = a.account_id AND n.actor = a.actor
           AND n.undone = 0 AND n.id > a.id AND NOT ({})
         ORDER BY n.id DESC LIMIT 1",
        discarded_draft("n")
    )
}

pub(super) fn map_blocker(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<Option<ActivityBlocker>> {
    row.get::<_, Option<i64>>(index)?
        .map(|id| {
            Ok(ActivityBlocker {
                id,
                actor: row.get(index + 1)?,
                kind: row.get(index + 2)?,
                created_unix: row.get(index + 3)?,
            })
        })
        .transpose()
}

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
            ActivityDirection::At(id) => ("AND id <= ?3", "DESC", Some(id)),
        };
        let limit = limit.clamp(1, 100);
        let sql_limit = (limit + 1) as i64;
        let mut stmt = self.conn.prepare_cached(&format!(
            "SELECT {ACTION_COLS}, {SUBJECT}, {CONVERSATIONS}, ({discarded}),
                    blocking_id, blocking_actor, blocking_kind, blocking_time, (undone = {SNOOZE_RETIRED})
             FROM (
                 SELECT a.*, CASE WHEN a.undone = 0 AND NOT ({discarded})
                                  THEN ({newer}) END AS blocker_id
                 FROM agent_actions a
                 WHERE {AGENT_ROWS} AND (?2 IS NULL OR actor = ?2) {bound}
                 ORDER BY id {order} LIMIT ?4
             ) a
             LEFT JOIN (
                 SELECT id AS blocking_id, actor AS blocking_actor, kind AS blocking_kind,
                        created_unix AS blocking_time
                 FROM agent_actions
             ) b ON b.blocking_id = a.blocker_id
             ORDER BY id {order}",
            discarded = discarded_draft("a"),
            newer = newer_action(),
        ))?;
        let entries =
            stmt.query_map(params![account.as_str(), actor, anchor, sql_limit], |row| {
                Ok(AgentActivity {
                    action: map_action(row)?,
                    subject: row.get(7)?,
                    conversations: row.get(8)?,
                    draft_discarded: row.get(9)?,
                    blocker: map_blocker(row, 10)?,
                    snooze_retired: row.get(14)?,
                    missing_grant: None,
                })
            })?;
        let mut entries = entries.collect::<rusqlite::Result<Vec<_>>>()?;
        let more = entries.len() > limit;
        entries.truncate(limit);
        let mut grants = HashMap::new();
        for entry in &mut entries {
            if entry.action.undone || entry.draft_discarded {
                continue;
            }
            let id = entry
                .action
                .actor
                .strip_prefix("agent:")
                .unwrap_or_default();
            let grant = if let Some(grant) = grants.get(id) {
                *grant
            } else {
                let grant = read_grant(&self.conn, account, id)?;
                grants.insert(id.to_owned(), grant);
                grant
            };
            entry.missing_grant =
                ActivityGrant::missing(grant, WriteScope::for_undo(&entry.action.kind));
            if entry.blocker.is_none() && matches!(entry.action.kind.as_str(), "archive" | "move") {
                entry.blocker = super::undo_conflicts::folder_blocker(&self.conn, &entry.action)?;
            }
        }
        let (has_newer, has_older) = match direction {
            ActivityDirection::Latest => (false, more),
            ActivityDirection::Older(_) => (true, more),
            ActivityDirection::Newer(_) => {
                entries.reverse();
                (more, true)
            }
            ActivityDirection::At(id) => {
                let newer = self.conn.query_row(
                    &format!(
                        "SELECT EXISTS (SELECT 1 FROM agent_actions
                         WHERE {AGENT_ROWS} AND (?2 IS NULL OR actor = ?2) AND id > ?3)"
                    ),
                    params![account.as_str(), actor, id],
                    |row| row.get(0),
                )?;
                (newer, more)
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
    let (action, conversations, snooze_retired) = conn
        .query_row(
            &format!(
                "SELECT {ACTION_COLS}, {CONVERSATIONS}, (undone = {SNOOZE_RETIRED}) FROM agent_actions a
                  WHERE {AGENT_ROWS} AND id = ?2"
            ),
            params![account.as_str(), id],
            |row| Ok((map_action(row)?, row.get::<_, u32>(7)?, row.get(8)?)),
        )
        .optional()?
        .ok_or_else(|| Error::Action("Action is no longer available".into()))?;
    if action.undone {
        return Err(Error::AgentActivityAlreadyUndone { snooze_retired });
    }
    let blocker = conn.query_row(
        &format!(
            "SELECT b.id, b.actor, b.kind, b.created_unix FROM agent_actions a
             LEFT JOIN agent_actions b ON b.id = ({newer})
             WHERE a.id = ?1",
            newer = newer_action(),
        ),
        [id],
        |row| map_blocker(row, 0),
    )?;
    if let Some(blocker) = blocker {
        return Err(Error::AgentActivityBlocked(blocker));
    }
    if conversations > BULK_UNDO_THRESHOLD
        && confirmed_bulk.is_none_or(|approved| conversations > approved)
    {
        return Err(Error::Action("Confirm undo for this bulk action".into()));
    }
    Ok(action)
}

pub(super) fn check_grant(
    conn: &Connection,
    account: &AccountId,
    actor: &str,
    scope: WriteScope,
) -> Result<()> {
    let agent_id = require_token(actor.strip_prefix("agent:").unwrap_or_default(), "agent id")?;
    let grant = read_grant(conn, account, agent_id)?;
    if let Some(ActivityGrant { enabled, scope }) = ActivityGrant::missing(grant, scope) {
        return Err(Error::AgentActivityGrant {
            agent_id: agent_id.into(),
            enabled,
            scope,
        });
    }
    Ok(())
}
