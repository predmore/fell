use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};

use crate::backend::DeferredMailDate;
use crate::{AccountId, MessageId, RemoteId, Result, plausible_future_mail_date};

use super::{Store, bodies, immediate_transaction};

const MAX_CLOCK_ATTEMPTS: i64 = 5;
const MAX_UNCHECKED_ATTEMPTS: i64 = 5;
const CLOCK_RETRY_SECONDS: i64 = 10 * 60;

pub(super) fn queue_deferred_date(
    conn: &Connection,
    account: &AccountId,
    id: &str,
    date: DeferredMailDate,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO internal_date_repair
             (account_id, message_id, previous_date_unix, deferred_arrival_unix)
         SELECT account_id, message_id, ?3, ?4 FROM messages
         WHERE account_id = ?1 AND message_id = ?2 AND date_unix = 0
         ON CONFLICT(account_id, message_id) DO UPDATE SET
             previous_date_unix = COALESCE(NULLIF(previous_date_unix, 0), excluded.previous_date_unix),
             deferred_arrival_unix = COALESCE(
                 MIN(deferred_arrival_unix, excluded.deferred_arrival_unix),
                 deferred_arrival_unix, excluded.deferred_arrival_unix)",
    )?
    .execute(params![account.as_str(), id, date.header.unwrap_or(0), date.arrival])?;
    Ok(())
}

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
    let has_deferred_arrival = |conn: &Connection| {
        bodies::has_column(conn, "internal_date_repair", "deferred_arrival_unix")
    };
    if seeded(conn)? && has_budgets(conn)? && has_deferred_arrival(conn)? {
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
    if !has_deferred_arrival(&tx)? {
        tx.execute_batch(
            "ALTER TABLE internal_date_repair ADD COLUMN deferred_arrival_unix INTEGER;",
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

pub(crate) struct DateRepairRequest {
    message_id: MessageId,
    pub(crate) copies: Vec<RemoteId>,
    previous: i64,
    saved_arrival: Option<i64>,
}

impl DateRepairRequest {
    pub(crate) fn copies_to_fetch(&self, latest: i64) -> &[RemoteId] {
        if date_ready_from_cache(self.previous, self.saved_arrival, self.copies.len(), latest) {
            &[]
        } else {
            &self.copies
        }
    }
}

fn date_ready_from_cache(
    previous: i64,
    arrival: Option<i64>,
    copy_count: usize,
    latest: i64,
) -> bool {
    let valid = 1..=latest;
    // A usable header wins outright. An arrival can stand on its own
    // when there is no other copy to check and no plausible header to wait for.
    valid.contains(&previous)
        || (copy_count <= 1
            && !plausible_future_mail_date(previous, latest)
            && arrival.is_some_and(|date| valid.contains(&date)))
}

impl Store {
    pub(crate) fn missing_internal_dates(
        &self,
        account: &AccountId,
    ) -> Result<Vec<DateRepairRequest>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT message_id, previous_date_unix, deferred_arrival_unix
             FROM internal_date_repair WHERE account_id = ?1
             ORDER BY attempts, message_id LIMIT 64",
        )?;
        let rows = stmt.query_map([account.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?))
        })?;
        rows.map(|row| {
            let (id, previous, saved_arrival) = row?;
            let copies = bodies::copies_of(&self.conn, account, &id)?;
            Ok(DateRepairRequest {
                message_id: MessageId(id),
                copies,
                previous,
                saved_arrival,
            })
        })
        .collect()
    }

    pub(crate) fn finish_internal_dates(
        &self,
        account: &AccountId,
        batch: &[DateRepairRequest],
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
        let plausible_future = |date| plausible_future_mail_date(date, latest);
        for request in batch {
            let id = request.message_id.as_str();
            let key = params![account.as_str(), id];
            let Some((
                previous,
                unchecked_attempts,
                clock_attempts,
                last_clock_latest,
                saved_arrival,
            )) = tx
                .query_row(
                    "SELECT previous_date_unix, unchecked_attempts, clock_attempts,
                            last_clock_latest, deferred_arrival_unix
                     FROM internal_date_repair
                     WHERE account_id = ?1 AND message_id = ?2",
                    key,
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<i64>>(3)?,
                            row.get::<_, Option<i64>>(4)?,
                        ))
                    },
                )
                .optional()?
            else {
                continue;
            };
            let copies = bodies::copies_of(&tx, account, id)?;
            let unchecked = !date_ready_from_cache(previous, saved_arrival, copies.len(), latest)
                && copies.iter().any(|copy| !checked.contains_key(copy));
            let unchecked_attempts = unchecked_attempts
                + i64::from(
                    fetch_complete && unchecked && unchecked_attempts < MAX_UNCHECKED_ATTEMPTS,
                );
            let reported_arrivals = || copies.iter().filter_map(|copy| checked.get(copy)?.as_ref());
            let arrival = reported_arrivals()
                .copied()
                .chain(saved_arrival)
                .find(|date| valid_dates.contains(date));
            // Keep a verified future arrival even if the next fetch omits it.
            // Of several future copies, the earliest can become usable first.
            let deferred_arrival = reported_arrivals()
                .copied()
                .chain(saved_arrival)
                .filter(|date| plausible_future(*date))
                .min();
            let header = valid_dates.contains(&previous).then_some(previous);
            let clock_may_be_behind = plausible_future(previous)
                || (header.is_none() && arrival.is_none() && deferred_arrival.is_some());
            let advance_clock = fetch_complete
                && clock_may_be_behind
                && clock_attempts < MAX_CLOCK_ATTEMPTS
                && last_clock_latest
                    .is_none_or(|last| latest >= last.saturating_add(CLOCK_RETRY_SECONDS));
            let clock_attempts = clock_attempts + i64::from(advance_clock);
            if (clock_may_be_behind && (clock_attempts < MAX_CLOCK_ATTEMPTS || arrival.is_none()))
                || (unchecked && unchecked_attempts < MAX_UNCHECKED_ATTEMPTS)
            {
                // Rotate the page even when neither retry budget advances.
                tx.execute(
                    "UPDATE internal_date_repair SET attempts = attempts + 1,
                         unchecked_attempts = ?3, clock_attempts = ?4, last_clock_latest = ?5,
                         deferred_arrival_unix = ?6
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
                        },
                        saved_arrival
                            .filter(|date| valid_dates.contains(date))
                            .or(deferred_arrival)
                    ],
                )?;
                continue;
            }
            if let Some(date) = header.or(arrival) {
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
