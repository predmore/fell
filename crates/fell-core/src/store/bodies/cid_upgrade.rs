//! Repair MIME roles stored before HTML-aware CID classification.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::{CODEC_DEFLATE, MAX_PART_BYTES, PartStamp, decompress, list, role_code};
use crate::imap::CidReferences;
use crate::store::{Store, immediate_transaction, set_attachments_from_list};
use crate::{AccountId, MimePart, PartRole, Result, ThreadId};

#[cfg(test)]
mod tests;

const MIGRATION_KEY: &str = "cid_image_attachment_roles";
const BATCH: usize = 32;
const SCAN: i64 = 256;

/// A committed position in the cached CID role repair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CidRepairProgress(Option<Cursor>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Cursor {
    after: i64,
    generation: u64,
}

impl CidRepairProgress {
    pub fn is_done(self) -> bool {
        self.0.is_none()
    }
}

pub struct CidRepairBatch {
    pub progress: CidRepairProgress,
    /// Threads whose part roles changed in this commit.
    pub changed_threads: Vec<(AccountId, ThreadId)>,
    /// Another connection advanced the cursor. Refresh active reads because
    /// the threads from those commits are only known to that connection.
    pub external_progress: bool,
}

fn cursor(conn: &Connection) -> Result<Option<Cursor>> {
    let value: Option<String> = conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")?
        .query_row(params![MIGRATION_KEY], |row| row.get(0))
        .optional()?;
    let value = value.as_deref().unwrap_or("0");
    if value == "done" {
        return Ok(None);
    }
    let (after, generation) = value.split_once(':').unwrap_or((value, "0"));
    Ok(Some(Cursor {
        after: after.parse().unwrap_or(0),
        generation: generation.parse().unwrap_or(0),
    }))
}

fn save_cursor(conn: &Connection, cursor: Option<Cursor>) -> Result<()> {
    let progress = match cursor {
        None => "done".into(),
        Some(Cursor {
            after,
            generation: 0,
        }) => after.to_string(),
        Some(Cursor { after, generation }) => format!("{after}:{generation}"),
    };
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![MIGRATION_KEY, progress],
    )?;
    Ok(())
}

/// An as yet unexamined list can move to a body the scan already visited.
/// Keep it in range and invalidate any snapshot classifying the old range.
pub(super) fn inherited_list(
    conn: &Connection,
    account: &AccountId,
    from: &str,
    to: &str,
) -> Result<()> {
    let Some(mut progress) = cursor(conn)? else {
        return Ok(());
    };
    let (source, kept): (i64, i64) = conn
        .prepare_cached(
            "SELECT src.rowid, dst.rowid FROM message_bodies src
             JOIN message_bodies dst USING (account_id)
             WHERE src.account_id = ?1 AND src.message_id = ?2 AND dst.message_id = ?3",
        )?
        .query_row(params![account.as_str(), from, to], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    if source > progress.after {
        progress.after = progress.after.min(kept.saturating_sub(1));
        progress.generation = progress.generation.wrapping_add(1);
        save_cursor(conn, Some(progress))?;
    }
    Ok(())
}

impl Store {
    pub fn cid_repair_progress(&self) -> Result<CidRepairProgress> {
        Ok(CidRepairProgress(cursor(&self.conn)?))
    }

    /// Repair one bounded slice of cached mail. Reads and HTML parsing run
    /// on a WAL snapshot. During the short write, recheck the cursor and
    /// inputs so sync or another process can write while we parse.
    pub fn repair_cid_batch(&self, previous: CidRepairProgress) -> Result<CidRepairBatch> {
        let read = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let current = CidRepairProgress(cursor(&read)?);
        let mut batch = CidRepairBatch {
            progress: current,
            changed_threads: Vec::new(),
            external_progress: current != previous,
        };
        let Some(start) = current.0 else {
            return Ok(batch);
        };
        let mut through = start.after;
        let mut repairs = Vec::new();
        {
            // Cap rows examined too: a sparse mailbox can go a long way
            // between candidates, and the last slice may have none at all.
            let mut stmt = read.prepare_cached(
                "SELECT b.rowid, b.account_id, b.message_id, b.parts_listed = 1 AND EXISTS (
                     SELECT 1 FROM message_parts p
                     WHERE p.account_id = b.account_id AND p.message_id = b.message_id
                       AND p.mime_type GLOB 'image/*' AND p.content_id IS NOT NULL
                       AND p.disposition != 2 AND p.role IN (0, 1)
                       AND (b.html IS NOT NULL OR p.nested != 0 OR p.role = 1)
                 )
                 FROM message_bodies b WHERE b.rowid > ?1
                 ORDER BY b.rowid LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![start.after, SCAN])?;
            while let Some(row) = rows.next()? {
                through = row.get(0)?;
                if row.get(3)?
                    && let Some(repair) =
                        Repair::prepare(&read, through, AccountId(row.get(1)?), row.get(2)?, None)?
                {
                    repairs.push(repair);
                    if repairs.len() == BATCH {
                        break;
                    }
                }
            }
        }
        read.commit()?;

        let tx = self
            .lock_wait
            .behind_waiting_writes(|| immediate_transaction(&self.conn))?;
        let current = CidRepairProgress(cursor(&tx)?);
        if current.0 != Some(start) {
            batch.progress = current;
            batch.external_progress = true;
            return Ok(batch);
        }
        let mut next = (through != start.after).then_some(Cursor {
            after: through,
            ..start
        });
        for repair in repairs {
            if !repair.is_current(&tx)? {
                // Commit the prefix and reread this row on the next slice.
                // With no prefix the cursor stays put.
                next = Some(Cursor {
                    after: repair.rowid - 1,
                    ..start
                });
                break;
            }
            if let Some(thread) = repair.apply(&tx)? {
                batch.changed_threads.push(thread);
            }
        }
        save_cursor(&tx, next)?;
        tx.commit()?;
        batch.progress = CidRepairProgress(next);
        Ok(batch)
    }

    /// A headless reader has no background store lane. Finish the pending
    /// slices so its first query sees current attachment metadata.
    pub fn finish_cid_repair(&self) -> Result<()> {
        let mut progress = self.cid_repair_progress()?;
        while !progress.is_done() {
            progress = self.repair_cid_batch(progress)?.progress;
        }
        Ok(())
    }
}

struct Repair {
    rowid: i64,
    account: AccountId,
    id: String,
    stamp: Option<PartStamp>,
    parts: Vec<MimePart>,
    roles: Vec<PartRole>,
    needs_source: bool,
}

fn body_stamp(conn: &Connection, account: &AccountId, id: &str) -> Result<Option<PartStamp>> {
    Ok(conn
        .prepare_cached(
            "SELECT stamp FROM message_body_stamps WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_row(params![account.as_str(), id], |row| {
            Ok(PartStamp(row.get(0)?))
        })
        .optional()?)
}

impl Repair {
    fn prepare(
        conn: &Connection,
        rowid: i64,
        account: AccountId,
        id: String,
        fetched: Option<&[MimePart]>,
    ) -> Result<Option<Self>> {
        let Some(parts) = list(conn, &account, &id)? else {
            return Ok(None);
        };
        let html = CachedHtml::read(conn, &account, &id, &parts)?;
        let cids = match &html {
            CachedHtml::Available(html) => Some(CidReferences::new(Some(html))),
            _ => None,
        };
        let roles = parts
            .iter()
            .map(|part| {
                if !part.nested
                    && candidate(part)
                    && let Some(cids) = &cids
                {
                    cids.role_for(part)
                } else if html.needs_source(part)
                    && let Some(other) = fetched
                        .and_then(|parts| parts.iter().find(|other| other.section == part.section))
                    && *part
                        == (MimePart {
                            role: part.role,
                            ..other.clone()
                        })
                {
                    other.role
                } else {
                    part.role
                }
            })
            .collect();
        Ok(Some(Self {
            rowid,
            stamp: body_stamp(conn, &account, &id)?,
            needs_source: parts.iter().any(|part| html.needs_source(part)),
            account,
            id,
            parts,
            roles,
        }))
    }

    fn is_current(&self, conn: &Connection) -> Result<bool> {
        Ok(body_stamp(conn, &self.account, &self.id)? == self.stamp
            && list(conn, &self.account, &self.id)?.as_deref() == Some(&self.parts))
    }

    fn apply_roles(&mut self, conn: &Connection) -> Result<bool> {
        let mut changed = false;
        for (part, &role) in self.parts.iter_mut().zip(&self.roles) {
            if role != part.role {
                conn.prepare_cached(
                    "UPDATE message_parts SET role = ?4
                     WHERE account_id = ?1 AND message_id = ?2 AND section = ?3",
                )?
                .execute(params![
                    self.account.as_str(),
                    self.id,
                    part.section,
                    role_code(role)
                ])?;
                part.role = role;
                changed = true;
            }
        }
        if changed {
            set_attachments_from_list(conn, &self.account, &self.id, &self.parts)?;
        }
        Ok(changed)
    }

    fn apply(mut self, conn: &Connection) -> Result<Option<(AccountId, ThreadId)>> {
        let changed = self.apply_roles(conn)?;
        // Nested parts belong to an attached message's HTML. A previous
        // inline role is the only evidence of missing HTML when no blob
        // was kept. Queue only when there is a server copy to consult.
        if self.needs_source {
            conn.execute(
                "INSERT INTO body_backfill (account_id, message_id, refresh_cid_roles)
                 SELECT ?1, ?2, 1 WHERE EXISTS (
                     SELECT 1 FROM message_copies WHERE account_id = ?1 AND message_id = ?2
                 )
                 ON CONFLICT(account_id, message_id) DO UPDATE SET refresh_cid_roles = 1",
                params![self.account.as_str(), self.id],
            )?;
        }
        if !changed {
            return Ok(None);
        }
        let thread = conn
            .prepare_cached(
                "SELECT thread_id FROM messages WHERE account_id = ?1 AND message_id = ?2",
            )?
            .query_row(params![self.account.as_str(), self.id], |row| row.get(0))
            .optional()?;
        Ok(thread.map(|id| (self.account, ThreadId(id))))
    }
}

fn candidate(part: &MimePart) -> bool {
    part.role != PartRole::Signature && part.cid_image_id().is_some()
}

enum CachedHtml {
    Available(String),
    Missing,
    Unreadable,
}

impl CachedHtml {
    fn read(conn: &Connection, account: &AccountId, id: &str, parts: &[MimePart]) -> Result<Self> {
        if !parts.iter().any(|part| !part.nested && candidate(part)) {
            return Ok(Self::Missing);
        }
        // Load only HTML and inflate one body at a time, using the usual cap.
        let (present, bytes) = conn
            .prepare_cached(
                "SELECT html IS NOT NULL,
                        CASE WHEN codec = ?3 AND length(html) <= ?4 THEN html END
                 FROM message_bodies WHERE account_id = ?1 AND message_id = ?2",
            )?
            .query_row(
                params![account.as_str(), id, CODEC_DEFLATE, MAX_PART_BYTES as i64],
                |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<Vec<u8>>>(1)?)),
            )?;
        Ok(
            match bytes.as_deref().and_then(|bytes| decompress(bytes).ok()) {
                Some(html) => Self::Available(html),
                None if present => Self::Unreadable,
                None => Self::Missing,
            },
        )
    }

    fn needs_source(&self, part: &MimePart) -> bool {
        candidate(part)
            && (part.nested
                || match self {
                    Self::Available(_) => false,
                    Self::Missing => part.role == PartRole::Inline,
                    Self::Unreadable => true,
                })
    }
}

fn queued(conn: &Connection, account: &AccountId, id: &str) -> Result<bool> {
    Ok(conn
        .prepare_cached(
            "SELECT refresh_cid_roles FROM body_backfill WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_row(params![account.as_str(), id], |row| row.get(0))
        .optional()?
        .unwrap_or(false))
}

pub(in crate::store) struct FetchedRepair {
    canonical: String,
    repair: Option<Repair>,
}

pub(in crate::store) fn prepare_fetched(
    conn: &Connection,
    account: &AccountId,
    id: String,
    fetched: Option<&[MimePart]>,
) -> Result<Option<FetchedRepair>> {
    if !queued(conn, account, &id)? {
        return Ok(None);
    }
    let repair = match fetched {
        Some(parts) => Repair::prepare(conn, 0, account.clone(), id.clone(), Some(parts))?,
        None => None,
    };
    Ok(Some(FetchedRepair {
        canonical: id,
        repair,
    }))
}

/// Consume a CID-only answer for the list it was read against. If sync
/// moved the list or its body changed while preparing the roles, leave the
/// request queued so the next backfill can use the current copy and cache.
pub(in crate::store) fn apply_fetched(
    conn: &Connection,
    account: &AccountId,
    id: &str,
    prepared: Option<FetchedRepair>,
) -> Result<bool> {
    if !queued(conn, account, id)? {
        return Ok(prepared.is_some());
    }
    let Some(mut prepared) = prepared.filter(|prepared| prepared.canonical == id) else {
        return Ok(true);
    };
    if let Some(repair) = &mut prepared.repair {
        if !repair.is_current(conn)? {
            return Ok(true);
        }
        repair.apply_roles(conn)?;
    }
    conn.execute(
        "DELETE FROM body_backfill
         WHERE account_id = ?1 AND message_id = ?2 AND refresh_cid_roles = 1",
        params![account.as_str(), id],
    )?;
    Ok(true)
}
