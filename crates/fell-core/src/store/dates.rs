use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{AccountId, MessageId, RemoteId, Result};

use super::{BodyRequest, Store, bodies, immediate_transaction};

const MAX_CLOCK_ATTEMPTS: i64 = 5;
const MAX_UNCHECKED_ATTEMPTS: i64 = 5;
const CLOCK_RETRY_SECONDS: i64 = 10 * 60;
const MAX_PLAUSIBLE_CLOCK_SKEW_SECONDS: i64 = 30 * 86_400;

pub(super) fn ensure_date_repair(conn: &Connection) -> Result<()> {
    ensure_date_repair_through(conn, crate::latest_mail_date())
}

pub(super) fn ensure_date_repair_through(conn: &Connection, latest: i64) -> Result<()> {
    let seeded = |conn: &Connection| {
        conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM meta WHERE key = 'internal_date_repair_v1')",
            [],
            |row| row.get::<_, bool>(0),
        )
    };
    let has_budgets =
        |conn: &Connection| bodies::has_column(conn, "internal_date_repair", "last_clock_latest");
    if seeded(conn)? && has_budgets(conn)? {
        return Ok(());
    }
    let tx = immediate_transaction(conn)?;
    if !has_budgets(&tx)? {
        // The old counter mixed retry reasons. Keep its queue order and
        // start each new budget at zero.
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
                 ON internal_date_repair (account_id, attempts, message_id);
             ALTER TABLE internal_date_repair ADD COLUMN unchecked_attempts INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE internal_date_repair ADD COLUMN clock_attempts INTEGER NOT NULL DEFAULT 0;
             ALTER TABLE internal_date_repair ADD COLUMN last_clock_latest INTEGER;",
        )?;
    }
    if seeded(&tx)? {
        return tx.commit();
    }
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
        fetch_complete: bool,
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
            let Some((previous, unchecked_attempts, clock_attempts, last_clock_latest)) = tx
                .query_row(
                    "SELECT previous_date_unix, unchecked_attempts, clock_attempts, last_clock_latest
                     FROM internal_date_repair
                     WHERE account_id = ?1 AND message_id = ?2",
                    key,
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                        ))
                    },
                )
                .optional()?
            else {
                continue;
            };
            let copies = bodies::copies_of(&tx, account, id)?;
            let unchecked = copies.iter().any(|copy| !checked.contains_key(copy));
            let unchecked_attempts = unchecked_attempts
                + i64::from(
                    fetch_complete && unchecked && unchecked_attempts < MAX_UNCHECKED_ATTEMPTS,
                );
            let clock_may_be_behind = previous > latest && previous <= latest_plausible;
            let advance_clock = fetch_complete
                && clock_may_be_behind
                && clock_attempts < MAX_CLOCK_ATTEMPTS
                && last_clock_latest
                    .is_none_or(|last| latest >= last.saturating_add(CLOCK_RETRY_SECONDS));
            let clock_attempts = clock_attempts + i64::from(advance_clock);
            let arrival = copies.iter().find_map(|copy| {
                checked
                    .get(copy)
                    .copied()
                    .flatten()
                    .filter(|date| valid_dates.contains(date))
            });
            if (clock_may_be_behind && (clock_attempts < MAX_CLOCK_ATTEMPTS || arrival.is_none()))
                || (unchecked && unchecked_attempts < MAX_UNCHECKED_ATTEMPTS)
            {
                // Rotate the page even when neither retry budget advances.
                tx.execute(
                    "UPDATE internal_date_repair SET attempts = attempts + 1,
                         unchecked_attempts = ?3, clock_attempts = ?4, last_clock_latest = ?5
                     WHERE account_id = ?1 AND message_id = ?2",
                    params![
                        account.as_str(),
                        id,
                        unchecked_attempts,
                        clock_attempts,
                        if advance_clock {
                            Some(latest)
                        } else {
                            last_clock_latest
                        }
                    ],
                )?;
                continue;
            }
            let date = valid_dates
                .contains(&previous)
                .then_some(previous)
                .or(arrival);
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
