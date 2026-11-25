//! Repair split Sent/list pairs whose UIDs sync already knows.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, params};

use crate::Result;
use crate::account::AccountId;
use crate::folders::{FolderRole, FolderRoles};
use crate::message::Message;

use super::{
    MergeRoles, SELECT_MESSAGE_COLS, absorb_duplicate, find_list_collapse, immediate_transaction,
    list_is_set, message_fingerprint, roles, row_to_message, subject_has_list_prefix, summaries,
    threads,
};

// Independent of schema_version and of other one-time data repairs.
const REPAIR_KEY: &str = "sent_echo_repair_v1";

fn pending_key(account_id: &AccountId) -> String {
    format!("{REPAIR_KEY}:pending:{}", account_id.as_str())
}

fn meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row([key], |row| row.get(0))
        .optional()?)
}

pub(super) fn on_open(conn: &Connection) -> Result<()> {
    if meta(conn, REPAIR_KEY)?.is_some() {
        return Ok(());
    }
    let tx = immediate_transaction(conn)?;
    if meta(&tx, REPAIR_KEY)?.is_some() {
        return Ok(());
    }
    let accounts = {
        let mut stmt = tx.prepare(
            "SELECT a.id FROM accounts a
             WHERE EXISTS (SELECT 1 FROM messages m WHERE m.account_id = a.id)",
        )?;
        stmt.query_map([], |row| Ok(AccountId(row.get(0)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for account_id in accounts {
        let recorded = roles::listed(&tx, &account_id)?;
        let had_sent_listing = recorded
            .as_ref()
            .is_some_and(|roles| roles.get(FolderRole::Sent).is_some());
        let roles = match recorded {
            Some(roles) => roles,
            None => FolderRoles::from_names(&summaries::folder_names(&tx, &account_id)?),
        };
        if repair_account(&tx, &account_id, &roles)? {
            threads::rethread_dirty(&tx, &account_id)?;
        }
        if !had_sent_listing {
            // A later listing may be the first to identify a custom Sent
            // folder. Remember the name tried here so that a listing that
            // confirms the fallback does not scan the same folder again.
            let tried = roles.get(FolderRole::Sent).map_or("", |r| r.name.as_str());
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)",
                params![pending_key(&account_id), tried],
            )?;
        }
    }
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, '1')",
        [REPAIR_KEY],
    )?;
    tx.commit()
}

/// Run in the roles delta's write transaction so the listing, changes and
/// pending marker commit together. Each later delta does only a keyed read.
pub(super) fn on_listing(
    conn: &Connection,
    account_id: &AccountId,
    roles: &FolderRoles,
) -> Result<bool> {
    let Some(sent) = roles.get(FolderRole::Sent) else {
        return Ok(false);
    };
    let key = pending_key(account_id);
    let Some(tried) = meta(conn, &key)? else {
        return Ok(false);
    };
    let changed =
        !sent.name.eq_ignore_ascii_case(&tried) && repair_account(conn, account_id, roles)?;
    conn.execute("DELETE FROM meta WHERE key = ?1", [key])?;
    Ok(changed)
}

fn repair_account(conn: &Connection, account_id: &AccountId, roles: &FolderRoles) -> Result<bool> {
    let Some(sent) = roles.get(FolderRole::Sent) else {
        return Ok(false);
    };
    if !roles.is(FolderRole::Sent, &sent.name) {
        return Ok(false);
    }
    // Load only rowids of Sent candidates, with exact folder token matching.
    // Other copies are found through the account/fingerprint index.
    let rowids = {
        let mut stmt = conn.prepare_cached(
            "SELECT m.rowid FROM messages m
             WHERE m.account_id = ?1 AND m.body_fingerprint != ''
               AND instr(lower(char(30) || m.folders || char(30)),
                         lower(char(30) || ?2 || char(30))) > 0
               AND EXISTS (
                   SELECT 1 FROM messages other
                   WHERE other.account_id = m.account_id
                     AND other.body_fingerprint = m.body_fingerprint AND other.rowid != m.rowid
               )
             ORDER BY m.rowid",
        )?;
        stmt.query_map(params![account_id.as_str(), sent.name], |row| {
            row.get::<_, i64>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut sent_by_rowid = conn.prepare(&format!(
        "SELECT {SELECT_MESSAGE_COLS} FROM messages WHERE rowid = ?1"
    ))?;
    let mut echo_by_id = conn.prepare(&format!(
        "SELECT {SELECT_MESSAGE_COLS} FROM messages WHERE account_id = ?1 AND message_id = ?2"
    ))?;
    let mut merge_roles = MergeRoles::new(Some(roles));
    let mut protected = None;
    let mut changed = false;
    for rowid in rowids {
        let original = sent_by_rowid.query_row([rowid], row_to_message)?;
        if list_is_set(&original.list_id) {
            continue;
        }
        let original_id = original.headers.message_id.as_str();
        let fingerprint = message_fingerprint(&original);
        let Some(echo) = find_list_collapse(
            conn,
            &original,
            &fingerprint,
            Some(original_id),
            true,
            &mut merge_roles,
        )?
        else {
            continue;
        };
        // The general finder also handles same-parent list echoes outside
        // Sent. This repair requires the Sent original and a distinct echo.
        if echo
            .folders
            .split('\u{1e}')
            .any(|folder| roles.is(FolderRole::Sent, folder))
            || !(list_is_set(&echo.list_id) || subject_has_list_prefix(&echo.subject))
        {
            continue;
        }
        let mut incoming: Message = echo_by_id.query_row(
            params![account_id.as_str(), echo.canonical_id],
            row_to_message,
        )?;
        // Require a unique match in both directions. Two sends of the same
        // short reply may each see the same echo as their only match.
        let Some(keeper) = find_list_collapse(
            conn,
            &incoming,
            &fingerprint,
            Some(&echo.canonical_id),
            true,
            &mut merge_roles,
        )?
        else {
            continue;
        };
        if keeper.canonical_id != original_id {
            continue;
        }
        let protected = match &protected {
            Some(ids) => ids,
            None => protected.insert(protected_messages(conn, account_id)?),
        };
        if protected.contains(original_id) || protected.contains(&echo.canonical_id) {
            continue;
        }
        // Do not mark unread mail read as a side effect of upgrading. The
        // IMAP copy flags and MODSEQs move intact with the copies.
        incoming.unread |= original.unread;
        absorb_duplicate(conn, &incoming, &echo, &keeper, &mut merge_roles)?;
        changed = true;
    }
    Ok(changed)
}

/// Keep rows with pending writes or live undo snapshots independently
/// addressable. Those operations were recorded against the split rows.
fn protected_messages(conn: &Connection, account_id: &AccountId) -> Result<HashSet<String>> {
    let mut protected = HashSet::new();
    for sql in [
        "SELECT message_id FROM local_flag_intents WHERE account_id = ?1",
        "SELECT message_id FROM server_outbox WHERE account_id = ?1 AND state = 'pending'",
        "SELECT s.message_id FROM agent_actions a JOIN agent_action_messages s ON s.action_id = a.id
         WHERE a.account_id = ?1 AND a.undone = 0",
    ] {
        let mut stmt = conn.prepare_cached(sql)?;
        for id in stmt.query_map([account_id.as_str()], |row| row.get(0))? {
            protected.insert(id?);
        }
    }
    crate::actions::protect_undo_messages(conn, account_id, &mut protected)?;
    Ok(protected)
}
