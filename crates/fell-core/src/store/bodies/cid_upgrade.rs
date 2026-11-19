//! Repair MIME roles stored before HTML-aware CID classification.

use rusqlite::{Connection, OptionalExtension, params};

use super::{CODEC_DEFLATE, MAX_PART_BYTES, decompress, list, role_code};
use crate::imap::CidReferences;
use crate::store::{immediate_transaction, set_attachments_from_list};
use crate::{AccountId, MimePart, PartRole, Result};

#[cfg(test)]
mod tests;

const MIGRATION_KEY: &str = "cid_image_attachment_roles";
const BATCH: i64 = 32;

fn cursor(conn: &Connection) -> Result<Option<i64>> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM meta WHERE key = ?1",
            params![MIGRATION_KEY],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match value.as_deref() {
        Some("done") => None,
        value => Some(value.and_then(|value| value.parse().ok()).unwrap_or(0)),
    })
}

pub(in crate::store) fn run(conn: &Connection) -> Result<()> {
    if cursor(conn)?.is_none() {
        return Ok(());
    }
    loop {
        // Progress, part roles and their summaries commit together. Concurrent
        // openers resume at the last committed batch under the write lock.
        let tx = immediate_transaction(conn)?;
        let Some(after) = cursor(&tx)? else {
            return Ok(());
        };
        let batch = {
            let mut stmt = tx.prepare(
                "SELECT b.rowid, b.account_id, b.message_id FROM message_bodies b
                 WHERE b.rowid > ?1 AND b.parts_listed = 1 AND EXISTS (
                     SELECT 1 FROM message_parts p
                     WHERE p.account_id = b.account_id AND p.message_id = b.message_id
                       AND p.mime_type GLOB 'image/*' AND p.content_id IS NOT NULL
                       AND p.disposition != 2 AND p.role IN (0, 1)
                       AND (b.html IS NOT NULL OR p.nested != 0 OR p.role = 1)
                 )
                 ORDER BY b.rowid LIMIT ?2",
            )?;
            stmt.query_map(params![after, BATCH], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    AccountId(row.get(1)?),
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (_, account, id) in &batch {
            reclassify(&tx, account, id)?;
        }
        let progress = batch
            .last()
            .map_or_else(|| "done".to_string(), |(rowid, _, _)| rowid.to_string());
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![MIGRATION_KEY, progress],
        )?;
        tx.commit()?;
        if batch.is_empty() {
            return Ok(());
        }
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

fn reclassify(conn: &Connection, account: &AccountId, id: &str) -> Result<()> {
    let Some(mut parts) = list(conn, account, id)? else {
        return Ok(());
    };
    let html = CachedHtml::read(conn, account, id, &parts)?;
    if let CachedHtml::Available(html) = &html {
        let cids = CidReferences::new(Some(html));
        update_roles(conn, account, id, &mut parts, |part| {
            if !part.nested && candidate(part) {
                cids.role_for(part)
            } else {
                part.role
            }
        })?;
    }
    // Nested parts belong to an attached message's HTML. A previous inline
    // role is the only evidence of missing HTML when no HTML blob was kept.
    if parts.iter().any(|part| html.needs_source(part)) {
        conn.execute(
            "INSERT INTO body_backfill (account_id, message_id, refresh_cid_roles)
             SELECT ?1, ?2, 1 WHERE EXISTS (
                 SELECT 1 FROM message_copies WHERE account_id = ?1 AND message_id = ?2
             )
             ON CONFLICT(account_id, message_id) DO UPDATE SET refresh_cid_roles = 1",
            params![account.as_str(), id],
        )?;
    }
    Ok(())
}

/// Consume a CID-only backfill answer, even when the source is gone. It may
/// resolve uncertain roles, but a different copy cannot replace stored parts
/// or change anything already resolved from their own HTML.
pub(in crate::store) fn apply_fetched(
    conn: &Connection,
    account: &AccountId,
    id: &str,
    fetched: Option<&[MimePart]>,
) -> Result<bool> {
    let queued = conn.execute(
        "DELETE FROM body_backfill
         WHERE account_id = ?1 AND message_id = ?2 AND refresh_cid_roles = 1",
        params![account.as_str(), id],
    )? != 0;
    if queued
        && let Some(fetched) = fetched
        && let Some(mut parts) = list(conn, account, id)?
    {
        let html = CachedHtml::read(conn, account, id, &parts)?;
        update_roles(conn, account, id, &mut parts, |part| {
            if html.needs_source(part)
                && let Some(other) = fetched.iter().find(|other| other.section == part.section)
                && other.role != part.role
                && *part
                    == (MimePart {
                        role: part.role,
                        ..other.clone()
                    })
            {
                return other.role;
            }
            part.role
        })?;
    }
    Ok(queued)
}

fn update_roles(
    conn: &Connection,
    account: &AccountId,
    id: &str,
    parts: &mut [MimePart],
    role_for: impl Fn(&MimePart) -> PartRole,
) -> Result<()> {
    let mut changed = false;
    for part in &mut *parts {
        let role = role_for(part);
        if role != part.role {
            conn.prepare_cached(
                "UPDATE message_parts SET role = ?4
                 WHERE account_id = ?1 AND message_id = ?2 AND section = ?3",
            )?
            .execute(params![account.as_str(), id, part.section, role_code(role)])?;
            part.role = role;
            changed = true;
        }
    }
    if changed {
        set_attachments_from_list(conn, account, id, parts)?;
    }
    Ok(())
}
