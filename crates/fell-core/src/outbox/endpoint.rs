use rusqlite::params;

use crate::{AccountConfig, AccountId, Result, Store};

use super::{ENTRY_COLS, OutboxEntry, enabled, generation, select_entries};

impl Store {
    pub fn held_write_count(&self, account: &AccountId) -> Result<usize> {
        count(self.connection(), account, "state = 'held'")
    }

    pub fn queued_write_count(&self, account: &AccountId) -> Result<usize> {
        count(self.connection(), account, "state = 'pending'")
    }

    /// Drop parked writes without performing a server operation. Release the
    /// local folder changes in reverse order, preserving other folder actions.
    pub fn discard_held_writes(&self, account: &AccountId) -> Result<usize> {
        let tx = crate::store::immediate_transaction(self.connection())?;
        let rows = held(self, account)?;
        tx.execute(
            "DELETE FROM server_outbox WHERE account_id = ?1 AND state = 'held'",
            [account.as_str()],
        )?;
        for row in rows
            .iter()
            .rev()
            .filter(|row| row.kind == super::OutboxKind::Move)
        {
            crate::store::copies::discard_held_move(&tx, account, row)?;
        }
        tx.commit()?;
        Ok(rows.len())
    }
}

fn count(conn: &rusqlite::Connection, account: &AccountId, predicate: &str) -> Result<usize> {
    if !crate::store::copies::table_exists(conn, "server_outbox")? {
        return Ok(0);
    }
    Ok(conn.query_row(
        &format!("SELECT COUNT(*) FROM server_outbox WHERE account_id = ?1 AND {predicate}"),
        [account.as_str()],
        |row| row.get::<_, i64>(0),
    )? as usize)
}

pub(crate) fn held(store: &Store, account: &AccountId) -> Result<Vec<OutboxEntry>> {
    select_entries(
        store.connection(),
        &format!(
            "SELECT {ENTRY_COLS} FROM server_outbox
        WHERE account_id = ?1 AND state = 'held' ORDER BY id"
        ),
        account,
    )
}

pub(crate) fn stamp_mailbox_epochs(
    conn: &rusqlite::Connection,
    account: &AccountId,
    cursor: Option<&crate::SyncCursor>,
) -> Result<()> {
    for (mailbox, checkpoint) in crate::imap::decode_cursor(cursor) {
        conn.execute(
            "UPDATE server_outbox SET
                 held_source_epoch = CASE WHEN source_mailbox = ?2 THEN ?3 ELSE held_source_epoch END,
                 held_target_epoch = CASE WHEN target_mailbox = ?2 THEN ?3 ELSE held_target_epoch END
             WHERE account_id = ?1 AND state = 'pending' AND (source_mailbox = ?2 OR target_mailbox = ?2)",
            params![account.as_str(), mailbox, checkpoint.uidvalidity],
        )?;
    }
    Ok(())
}

pub(crate) fn can_resume(store: &Store, config: &AccountConfig) -> Result<bool> {
    Ok(store.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM server_outbox WHERE account_id = ?1
            AND state = 'held' AND held_endpoint = ?2 AND held_server IS NOT NULL
            AND held_server = COALESCE((SELECT value FROM meta WHERE key = 'imap:server:' || ?1), held_server)
            AND held_from = 'pending'
            AND (uidvalidity IS NOT NULL OR target_uidvalidity > 0 OR held_source_epoch IS NOT NULL OR held_target_epoch IS NOT NULL)
            AND (?3 OR kind = 'unflag_trash'))",
        params![config.id, config.imap_endpoint_key(), config.write_back], |row| row.get(0),
    )?)
}

pub(crate) fn resume_epochs(
    store: &Store,
    row: &OutboxEntry,
) -> Result<(Option<u32>, Option<u32>)> {
    Ok(store.connection().query_row(
        "SELECT COALESCE(uidvalidity, held_source_epoch), COALESCE(NULLIF(target_uidvalidity, 0), held_target_epoch)
         FROM server_outbox WHERE id = ?1",
        [row.id], |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

pub(crate) fn resumable(
    store: &Store,
    config: &AccountConfig,
    server: &str,
) -> Result<Vec<OutboxEntry>> {
    let conn = store.connection();
    let ids = {
        let mut stmt = conn.prepare(
            "SELECT id FROM server_outbox WHERE account_id = ?1 AND state = 'held'
              AND held_endpoint = ?2 AND held_server = ?3 AND held_from = 'pending'
              AND (?4 OR kind = 'unflag_trash') ORDER BY id",
        )?;
        stmt.query_map(
            params![
                config.id,
                config.imap_endpoint_key(),
                server,
                config.write_back
            ],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?
    };
    let mut rows = held(store, &AccountId(config.id.clone()))?;
    rows.retain(|row| ids.contains(&row.id));
    Ok(rows)
}

pub(crate) fn resume_verified(
    store: &Store,
    config: &AccountConfig,
    server: &str,
    row: &OutboxEntry,
    valid: bool,
) -> Result<()> {
    let account = AccountId(config.id.clone());
    let tx = crate::store::immediate_transaction(store.connection())?;
    if !crate::store::matches_imap_connection(&tx, config)? {
        return Ok(());
    }
    let new_state =
        if valid && (enabled(&tx, &account)? || row.kind == super::OutboxKind::UnflagTrash) {
            "pending"
        } else {
            "held"
        };
    tx.execute(
        "UPDATE server_outbox SET state = ?4, held_from = NULL, retry_unix = 0, generation = ?5,
             held_source_epoch = NULL, held_target_epoch = NULL
         WHERE account_id = ?1 AND id = ?2 AND held_server = ?3 AND state = 'held' AND held_from = 'pending'
           AND held_server = (SELECT value FROM meta WHERE key = 'imap:server:' || ?1)",
        params![config.id, row.id, server, new_state, generation(&tx, &account)?],
    )?;
    tx.commit()?;
    Ok(())
}

pub(crate) fn requeue(
    store: &Store,
    config: &AccountConfig,
    row: &OutboxEntry,
    source: (&str, u32, u32),
    target_mailbox: Option<&str>,
) -> Result<bool> {
    let account = AccountId(config.id.clone());
    let (mailbox, validity, uid) = source;
    let tx = crate::store::immediate_transaction(store.connection())?;
    if !crate::store::matches_imap_connection(&tx, config)? || !enabled(&tx, &account)? {
        return Ok(false);
    }
    let changed = tx.execute(
        "UPDATE server_outbox SET state = 'pending', held_endpoint = NULL, held_server = NULL, held_from = NULL,
             held_source_epoch = NULL, held_target_epoch = NULL,
             source_mailbox = ?3, uidvalidity = ?4, uid = ?5, target_mailbox = ?6, generation = ?7,
             placed_uidvalidity = NULL, placed_uid = NULL, attempts = 0, retry_unix = 0, error = '',
             settled_unix = NULL, sent = 0, target_uidvalidity = 0, target_uidnext = 0, move_recovery_end = NULL,
             flag_base = NULL, flag_modseq = NULL, flag_superseded = 0, reverses = 0,
             keyword_parent = NULL, trash_purge_action_id = NULL
         WHERE account_id = ?1 AND id = ?2 AND state = 'held'
           AND NOT EXISTS (SELECT 1 FROM server_outbox o WHERE o.account_id = ?1 AND o.state = 'pending'
             AND o.source_mailbox = ?3 AND o.uidvalidity = ?4 AND o.uid = ?5 AND o.kind = server_outbox.kind
             AND (o.kind != 'keyword' OR o.target_folder = server_outbox.target_folder))",
        params![config.id, row.id, mailbox, validity, uid, target_mailbox, generation(&tx, &account)?],
    )?;
    tx.commit()?;
    Ok(changed != 0)
}
