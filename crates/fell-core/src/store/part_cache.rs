//! Keep enough of the part list to unlink cache entries after a committed delete.

use std::fs::File;

use rusqlite::{OptionalExtension, params, params_from_iter};

use crate::parts::{DEFAULT_CACHE_BYTES, PartCache, PartRequest};
use crate::{Result, Store};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS part_cache_sections (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    section TEXT NOT NULL,
    pending INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, message_id, section)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS part_cache_pending ON part_cache_sections(account_id, message_id, section)
    WHERE pending = 1;

-- Old cache keys include the section in their hash. Keep sections that
-- disappear during a part-list refresh, until the message itself is deleted.
CREATE TRIGGER IF NOT EXISTS part_cache_part_removed AFTER DELETE ON message_parts
WHEN EXISTS (SELECT 1 FROM messages WHERE account_id = old.account_id AND message_id = old.message_id)
BEGIN
    INSERT OR IGNORE INTO part_cache_sections (account_id, message_id, section)
    VALUES (old.account_id, old.message_id, old.section);
END;

-- Record the current list before message_parts_ad drops it. This also works
-- for files downloaded before the cleanup table existed.
CREATE TRIGGER IF NOT EXISTS part_cache_message_removed BEFORE DELETE ON messages BEGIN
    INSERT OR IGNORE INTO part_cache_sections (account_id, message_id, section)
    SELECT account_id, message_id, section FROM message_parts
    WHERE account_id = old.account_id AND message_id = old.message_id;
    UPDATE part_cache_sections SET pending = 1
    WHERE account_id = old.account_id AND message_id = old.message_id;
END;
";

impl Store {
    /// Retry committed cache removals on a background store connection.
    pub fn clean_default_part_cache(&self) -> Result<()> {
        if let Some(root) = PartCache::default_root()
            && self.conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM part_cache_sections WHERE pending = 1)",
                [],
                |row| row.get(0),
            )?
        {
            self.clean_part_cache(&PartCache::open(root, DEFAULT_CACHE_BYTES)?)?;
        }
        Ok(())
    }

    pub(crate) fn clean_part_cache(&self, cache: &PartCache) -> Result<()> {
        // A fetcher may be writing a large cached file. Try again on the next
        // sweep instead of blocking other work on this connection.
        if let Some(lock) = cache.try_lock()? {
            self.lock_wait
                .without_waiting(|| self.clean_part_cache_locked(cache, &lock, None))?;
        }
        Ok(())
    }

    /// The cache lock spans reading the queue, unlinking, and acknowledging
    /// it. A fetch cannot publish between unlink and acknowledgement.
    pub(crate) fn clean_part_cache_locked(
        &self,
        cache: &PartCache,
        lock: &File,
        request: Option<&PartRequest>,
    ) -> Result<()> {
        if !self.conn.is_autocommit() {
            return Err(crate::Error::Invalid(
                "cached parts can only be removed after the store commits".into(),
            ));
        }
        let key = request.map(|request| {
            [
                request.account_id.as_str(),
                request.message_id.as_str(),
                request.part.section.as_str(),
            ]
        });
        let (sql, args) = match &key {
            Some(key) => (
                "SELECT account_id, message_id, section FROM part_cache_sections
                 WHERE pending = 1 AND account_id = ?1 AND message_id = ?2 AND section = ?3",
                key.as_slice(),
            ),
            None => (
                "SELECT account_id, message_id, section FROM part_cache_sections WHERE pending = 1",
                &[][..],
            ),
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let pending: Vec<(String, String, String)> = stmt
            .query_map(params_from_iter(args), |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        let mut removed = Vec::new();
        let mut failure = None;
        for (account, message, section) in pending {
            match cache.remove_section(&account, &message, &section) {
                Ok(()) => removed.push((account, message, section)),
                Err(err) => {
                    failure.get_or_insert(err);
                }
            }
        }
        if !removed.is_empty() {
            lock.sync_all()?;
            let tx = super::begin_immediate(&self.conn)?;
            let mut ack = tx.prepare_cached(
                "DELETE FROM part_cache_sections
                 WHERE account_id = ?1 AND message_id = ?2 AND section = ?3 AND pending = 1",
            )?;
            for (account, message, section) in removed {
                ack.execute(params![account, message, section])?;
            }
            drop(ack);
            tx.commit()?;
        }
        failure.map_or(Ok(()), |err| Err(err.into()))
    }

    /// Message row stamp and pending cleanup flag for the exact section.
    /// The stamp catches a delete and restore even after cleanup is acknowledged.
    pub(crate) fn cached_part_state(
        &self,
        request: &PartRequest,
    ) -> Result<Option<([u8; 16], bool)>> {
        Ok(self
            .conn
            .prepare_cached(
                "SELECT coalesce(m.part_cache_stamp, zeroblob(16)), c.pending IS 1
                 FROM message_parts p JOIN messages m USING (account_id, message_id)
                 LEFT JOIN part_cache_sections c USING (account_id, message_id, section)
                 WHERE p.account_id = ?1 AND p.message_id = ?2 AND p.section = ?3",
            )?
            .query_row(
                params![
                    request.account_id.as_str(),
                    request.message_id.as_str(),
                    request.part.section,
                ],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?)
    }
}
