use std::collections::HashMap;

use rusqlite::{Connection, params};

use crate::{AccountId, MessageId, RemoteId, Result};

use super::{BodyRequest, Store, bodies, immediate_transaction};

pub(super) fn ensure_date_repair(conn: &Connection) -> Result<()> {
    let done = |conn: &Connection| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key = 'internal_date_repair_v1')",
            [],
            |row| row.get::<_, bool>(0),
        )
    };
    if done(conn)? {
        return Ok(());
    }
    let tx = immediate_transaction(conn)?;
    if done(&tx)? {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS internal_date_repair (
             account_id TEXT NOT NULL,
             message_id TEXT NOT NULL,
             PRIMARY KEY (account_id, message_id),
             FOREIGN KEY (account_id, message_id) REFERENCES messages(account_id, message_id)
                 ON DELETE CASCADE
         ) WITHOUT ROWID;",
    )?;
    let latest = crate::latest_mail_date();
    tx.execute(
        "INSERT OR IGNORE INTO internal_date_repair (account_id, message_id)
         SELECT account_id, message_id FROM messages AS m
         WHERE (date_unix <= 0 OR date_unix > ?1)
           AND (remote_id GLOB 'imap:*' OR EXISTS (
               SELECT 1 FROM message_copies AS c
               WHERE c.account_id = m.account_id AND c.message_id = m.message_id
           ))",
        [latest],
    )?;
    // Old caches never stored INTERNALDATE. Stop impossible dates pinning
    // the list even offline, then restore arrival dates after the next sync.
    tx.execute(
        "UPDATE messages SET date_unix = 0
         WHERE date_unix != 0 AND (account_id, message_id) IN (
             SELECT account_id, message_id FROM internal_date_repair
         )",
        [],
    )?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('internal_date_repair_v1', '1')",
        [],
    )?;
    tx.commit()
}

impl Store {
    pub(crate) fn missing_internal_dates(&self, account: &AccountId) -> Result<Vec<BodyRequest>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT message_id FROM internal_date_repair WHERE account_id = ?1
             ORDER BY message_id LIMIT 64",
        )?;
        let ids = stmt.query_map([account.as_str()], |row| row.get::<_, String>(0))?;
        ids.map(|id| {
            let id = id?;
            let copies = bodies::copies_of(&self.conn, account, &id)?;
            Ok(BodyRequest {
                message_id: MessageId(id),
                copies,
            })
        })
        .collect()
    }

    pub(crate) fn finish_internal_dates(
        &self,
        account: &AccountId,
        batch: &[BodyRequest],
        dates: &HashMap<RemoteId, i64>,
    ) -> Result<bool> {
        if batch.is_empty() {
            return Ok(false);
        }
        let tx = self.begin_sync_write()?;
        let mut changed = false;
        for request in batch {
            let id = request.message_id.as_str();
            let copies = bodies::copies_of(&tx, account, id)?;
            // Copies can move between the fetch and the write. Try again
            // with the current copy set if that happened.
            if copies != request.copies {
                continue;
            }
            if let Some(date) = copies.iter().find_map(|copy| dates.get(copy)) {
                changed |= tx.execute(
                    "UPDATE messages SET date_unix = ?3
                     WHERE account_id = ?1 AND message_id = ?2 AND date_unix = 0",
                    params![account.as_str(), id, date],
                )? > 0;
            }
            tx.execute(
                "DELETE FROM internal_date_repair WHERE account_id = ?1 AND message_id = ?2",
                params![account.as_str(), id],
            )?;
        }
        if changed {
            self.rethread_written(&tx, account)?;
        }
        tx.commit()?;
        Ok(changed)
    }
}
