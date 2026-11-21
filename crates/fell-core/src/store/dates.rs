use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{AccountId, MessageId, RemoteId, Result};

use super::{BodyRequest, Store, bodies, immediate_transaction};

pub(super) const MAX_DATE_REPAIR_ATTEMPTS: i64 = 5;
const MAX_PLAUSIBLE_CLOCK_SKEW_SECONDS: i64 = 30 * 86_400;

pub(super) fn ensure_date_repair(conn: &Connection) -> Result<()> {
    ensure_date_repair_through(conn, crate::latest_mail_date())
}

pub(super) fn ensure_date_repair_through(conn: &Connection, latest: i64) -> Result<()> {
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
             previous_date_unix INTEGER NOT NULL,
             attempts INTEGER NOT NULL DEFAULT 0,
             PRIMARY KEY (account_id, message_id),
             FOREIGN KEY (account_id, message_id) REFERENCES messages(account_id, message_id)
                 ON DELETE CASCADE
         ) WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS internal_date_repair_order
             ON internal_date_repair (account_id, attempts, message_id);",
    )?;
    tx.execute(
        "INSERT OR IGNORE INTO internal_date_repair (account_id, message_id, previous_date_unix)
         SELECT account_id, message_id, date_unix FROM messages AS m
         WHERE (date_unix <= 0 OR date_unix > ?1)
           AND (remote_id GLOB 'imap:*' OR EXISTS (
               SELECT 1 FROM message_copies AS c
               WHERE c.account_id = m.account_id AND c.message_id = m.message_id
           ))",
        [latest],
    )?;
    // Hide impossible dates even offline, keeping the original in case
    // the local clock was behind when the cache was opened.
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
             ORDER BY attempts, message_id LIMIT 64",
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
        checked: &HashMap<RemoteId, Option<i64>>,
        latest: i64,
    ) -> Result<bool> {
        if batch.is_empty() {
            return Ok(false);
        }
        let tx = self.begin_sync_write()?;
        let mut changed = false;
        let valid_dates = 1..=latest;
        let latest_plausible = latest.saturating_add(MAX_PLAUSIBLE_CLOCK_SKEW_SECONDS);
        for request in batch {
            let id = request.message_id.as_str();
            let key = params![account.as_str(), id];
            let Some((previous, attempts)) = tx
                .query_row(
                    "SELECT previous_date_unix, attempts FROM internal_date_repair
                     WHERE account_id = ?1 AND message_id = ?2",
                    key,
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?
            else {
                continue;
            };
            let copies = bodies::copies_of(&tx, account, id)?;
            let clock_may_be_behind = previous > latest && previous <= latest_plausible;
            let incomplete =
                copies != request.copies || !copies.iter().all(|copy| checked.contains_key(copy));
            // Give the clock and unchecked copies another chance after
            // untried rows. On the last attempt, use what we have.
            if attempts < MAX_DATE_REPAIR_ATTEMPTS - 1 && (clock_may_be_behind || incomplete) {
                tx.execute(
                    "UPDATE internal_date_repair SET attempts = attempts + 1
                     WHERE account_id = ?1 AND message_id = ?2",
                    key,
                )?;
                continue;
            }
            let date = valid_dates
                .contains(&previous)
                .then_some(previous)
                .or_else(|| {
                    copies.iter().find_map(|copy| {
                        checked
                            .get(copy)
                            .copied()
                            .flatten()
                            .filter(|date| valid_dates.contains(date))
                    })
                });
            if let Some(date) = date {
                changed |= tx.execute(
                    "UPDATE messages SET date_unix = ?3
                     WHERE account_id = ?1 AND message_id = ?2 AND date_unix = 0",
                    params![account.as_str(), id, date],
                )? > 0;
            }
            tx.execute(
                "DELETE FROM internal_date_repair WHERE account_id = ?1 AND message_id = ?2",
                key,
            )?;
        }
        if changed {
            self.rethread_written(&tx, account)?;
        }
        tx.commit()?;
        Ok(changed)
    }
}
