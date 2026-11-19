//! The HTML and plain-text parts of received mail, kept apart from `messages`.
//!
//! A row here holds what the conversation view renders: the decoded body
//! parts as sent (HTML is sanitized when it is rendered, not here) and the
//! headers the renderer's bulk-mail check reads. Parts are compressed. The
//! `messages` row keeps `body_text`, which search and snippets use, so a
//! folder list and the FTS index never read this table.
//!
//! Rows are keyed like `messages`, by account and canonical Message-ID, and
//! go when the message row goes. A merge first moves the parts the kept row
//! lacks onto it; an expunge saves them for undo (`local_action_bodies`).
//!
//! The other MIME parts (files, `cid:` images, signatures) are listed in
//! `message_parts`, keyed the same way plus the IMAP section, metadata only.
//! `message_bodies.parts_listed` says whether that list was taken: a row
//! written before it existed has an unknown list, not an empty one. A list
//! moves, merges and is saved for undo (`local_action_parts`) with the body.
//!
//! Mail stored before this table existed is queued in `body_backfill` once,
//! by the schema version 4 upgrade; sync fetches those messages again from a
//! server copy ([`crate::sync::fetch_missing_bodies`]) and any write of
//! parts takes a message off the queue. New mail never joins the queue, so a
//! mailbox that is caught up pays nothing per pass.

use rusqlite::{Connection, OptionalExtension, params};

use crate::account::AccountId;
use crate::message::{
    Disposition, MessageId, MessageParts, MimePart, PartRole, RemoteId, TransferEncoding,
};
use crate::{Error, Result};

const BODIES_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS message_bodies (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    -- How html and plain are stored: 1 is raw DEFLATE of UTF-8.
    codec INTEGER NOT NULL,
    html BLOB,
    plain BLOB,
    -- Bulk-mail headers: name US value, records joined by RS.
    bulk_headers TEXT NOT NULL DEFAULT '',
    -- 1 once message_parts holds this message's part list (maybe none).
    parts_listed INTEGER NOT NULL DEFAULT 0,
    -- 1 when sync kept no body: the message is larger than it downloads.
    withheld INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, message_id)
);

-- The non-body MIME parts of a message: metadata, never bytes.
CREATE TABLE IF NOT EXISTS message_parts (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    -- IMAP part number, as BODY.PEEK[section] takes it.
    section TEXT NOT NULL,
    -- Document order.
    position INTEGER NOT NULL,
    -- 0 attachment, 1 inline (cid:), 2 signature.
    role INTEGER NOT NULL,
    mime_type TEXT NOT NULL,
    filename TEXT,
    size INTEGER NOT NULL,
    encoded_size INTEGER NOT NULL,
    -- 0 identity, 1 quoted-printable, 2 base64.
    encoding INTEGER NOT NULL,
    content_id TEXT,
    -- 0 none given, 1 inline, 2 attachment.
    disposition INTEGER NOT NULL,
    nested INTEGER NOT NULL,
    PRIMARY KEY (account_id, message_id, section)
) WITHOUT ROWID;

-- Messages stored before message_bodies, still to fetch again.
CREATE TABLE IF NOT EXISTS body_backfill (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
) WITHOUT ROWID;

CREATE TRIGGER IF NOT EXISTS message_bodies_ad AFTER DELETE ON messages BEGIN
    DELETE FROM message_bodies
    WHERE account_id = old.account_id AND message_id = old.message_id;
    DELETE FROM body_backfill
    WHERE account_id = old.account_id AND message_id = old.message_id;
END;

CREATE TRIGGER IF NOT EXISTS message_parts_ad AFTER DELETE ON messages BEGIN
    DELETE FROM message_parts
    WHERE account_id = old.account_id AND message_id = old.message_id;
END;
";

/// Create the tables and the trigger that drops parts with their message.
/// Queueing mail stored before them is [`queue_stored_mail`], which runs
/// after the version 3 merge so it sees only the rows that survive it.
pub(super) fn ensure_bodies(conn: &Connection) -> Result<()> {
    conn.execute_batch(BODIES_SCHEMA)?;
    if !has_column(conn, "message_bodies", "parts_listed")? {
        conn.execute_batch(
            "ALTER TABLE message_bodies ADD COLUMN parts_listed INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if !has_column(conn, "message_bodies", "withheld")? {
        conn.execute_batch(
            "ALTER TABLE message_bodies ADD COLUMN withheld INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    Ok(())
}

pub(crate) fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|name| name == column))
}

/// Schema version 5: queue every stored message with a server copy whose
/// part list was never taken, so the backfill fetches it once more and
/// lists its attachments. Stamped in the same transaction, as version 4.
pub(super) fn queue_unlisted_mail(conn: &Connection) -> Result<()> {
    let tx = super::begin_immediate(conn)?;
    tx.execute(
        "INSERT OR IGNORE INTO body_backfill (account_id, message_id)
         SELECT m.account_id, m.message_id FROM messages m
         WHERE EXISTS (
             SELECT 1 FROM message_copies c
             WHERE c.account_id = m.account_id AND c.message_id = m.message_id
         )
           AND NOT EXISTS (
             SELECT 1 FROM message_bodies b
             WHERE b.account_id = m.account_id AND b.message_id = m.message_id
               AND b.parts_listed = 1
         )",
        [],
    )?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', '5')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// Schema version 4: queue every stored message that has a server copy and no
/// parts, and stamp the version in the same transaction, so a crash before the
/// stamp queues again on the next open. `INSERT OR IGNORE` keeps that retry,
/// and an open after a downgrade, from doing harm.
pub(super) fn queue_stored_mail(conn: &Connection) -> Result<()> {
    let tx = super::begin_immediate(conn)?;
    tx.execute(
        "INSERT OR IGNORE INTO body_backfill (account_id, message_id)
         SELECT m.account_id, m.message_id FROM messages m
         WHERE EXISTS (
             SELECT 1 FROM message_copies c
             WHERE c.account_id = m.account_id AND c.message_id = m.message_id
         )
           AND NOT EXISTS (
             SELECT 1 FROM message_bodies b
             WHERE b.account_id = m.account_id AND b.message_id = m.message_id
         )",
        [],
    )?;
    tx.execute(
        "INSERT INTO meta (key, value) VALUES ('schema_version', '4')
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

const CODEC_DEFLATE: i64 = 1;

/// Balanced: about as fast as zstd's default on mail HTML, a little larger.
const DEFLATE_LEVEL: u8 = 6;

/// A stored part never inflates past this; anything larger is corrupt.
const MAX_PART_BYTES: usize = 64 * 1024 * 1024;

/// One server copy of a message whose parts are not stored yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BodyRequest {
    pub message_id: MessageId,
    /// Every recorded copy, so a copy the server lost can fall back to another.
    pub copies: Vec<RemoteId>,
}

/// Whether an incoming copy's parts replace the stored ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Write {
    /// A same-id resync: parts with a body replace what is stored.
    Replace,
    /// A collapsed copy or a fetch for a gap: HTML, plain and the bulk
    /// headers are each written only where the stored row has none, so a
    /// plain-only Sent copy still gains the HTML of its list echo.
    FillGap,
}

/// Parts compressed and encoded as `message_bodies` keeps them. Built before
/// a write transaction, so deflate does not run under the write lock.
#[derive(Debug, Clone, Default)]
pub(crate) struct StoredParts {
    html: Option<Vec<u8>>,
    plain: Option<Vec<u8>>,
    bulk_headers: String,
    /// `None` when no source was parsed (nothing could be fetched), so the
    /// stored list, if any, stays.
    mime_parts: Option<Vec<MimePart>>,
}

impl StoredParts {
    pub(crate) fn new(parts: &MessageParts) -> Self {
        Self {
            html: parts.html.as_deref().map(compress),
            plain: parts.plain.as_deref().map(compress),
            bulk_headers: encode_headers(&parts.bulk_headers),
            mime_parts: Some(parts.mime_parts.clone()),
        }
    }

    pub(crate) fn has_body(&self) -> bool {
        self.html.is_some() || self.plain.is_some()
    }

    /// The part list this source gave, when one was parsed.
    pub(crate) fn mime_parts(&self) -> Option<&[MimePart]> {
        self.mime_parts.as_deref()
    }

    /// Replace the parts saved for undo on every open action that removed
    /// `message_id`, as a later copy of it replaces the saved body text. A
    /// copy with no text body (only files) replaces the part list and
    /// leaves any saved body alone.
    pub(crate) fn save_for_undo(
        &self,
        conn: &Connection,
        account_id: &AccountId,
        message_id: &str,
    ) -> Result<()> {
        let on_conflict = if self.has_body() {
            "ON CONFLICT(action_id, message_id) DO UPDATE SET
                codec = excluded.codec,
                html = excluded.html,
                plain = excluded.plain,
                bulk_headers = excluded.bulk_headers"
        } else {
            "ON CONFLICT(action_id, message_id) DO NOTHING"
        };
        conn.execute(
            &format!(
                "INSERT INTO local_action_bodies
                    (action_id, message_id, codec, html, plain, bulk_headers)
                 SELECT m.action_id, m.message_id, ?3, ?4, ?5, ?6
                 FROM local_action_messages m
                 JOIN local_actions a ON a.id = m.action_id
                 WHERE m.message_id = ?2 AND m.removed = 1
                   AND a.account_id = ?1 AND a.undone = 0
                 {on_conflict}"
            ),
            params![
                account_id.as_str(),
                message_id,
                CODEC_DEFLATE,
                self.html,
                self.plain,
                self.bulk_headers,
            ],
        )?;
        let Some(list) = &self.mime_parts else {
            return Ok(());
        };
        let actions = {
            let mut stmt = conn.prepare(
                "SELECT m.action_id FROM local_action_messages m
                 JOIN local_actions a ON a.id = m.action_id
                 WHERE m.message_id = ?2 AND m.removed = 1
                   AND a.account_id = ?1 AND a.undone = 0",
            )?;
            stmt.query_map(params![account_id.as_str(), message_id], |row| {
                row.get::<_, i64>(0)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };
        for action_id in actions {
            conn.execute(
                "UPDATE local_action_bodies SET parts_listed = 1
                 WHERE action_id = ?1 AND message_id = ?2",
                params![action_id, message_id],
            )?;
            conn.execute(
                "DELETE FROM local_action_parts WHERE action_id = ?1 AND message_id = ?2",
                params![action_id, message_id],
            )?;
            for (position, part) in list.iter().enumerate() {
                insert_part(
                    conn,
                    "INSERT INTO local_action_parts (action_id, message_id, section, position,
                        role, mime_type, filename, size, encoded_size, encoding, content_id,
                        disposition, nested)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                    rusqlite::types::Value::Integer(action_id),
                    message_id,
                    position,
                    part,
                )?;
            }
        }
        Ok(())
    }
}

/// Columns of `message_parts` after the key, as `local_action_parts` copies them.
pub(crate) const PART_COLUMNS: &str = "section, position, role, mime_type, filename, size,
    encoded_size, encoding, content_id, disposition, nested";

fn insert_part(
    conn: &Connection,
    sql: &str,
    owner: rusqlite::types::Value,
    message_id: &str,
    position: usize,
    part: &MimePart,
) -> Result<()> {
    conn.prepare_cached(sql)?.execute(params![
        owner,
        message_id,
        part.section,
        position as i64,
        role_code(part.role),
        part.mime_type,
        part.filename,
        part.size as i64,
        part.encoded_size as i64,
        encoding_code(part.encoding),
        part.content_id,
        disposition_code(part.disposition),
        i64::from(part.nested),
    ])?;
    Ok(())
}

fn role_code(role: PartRole) -> i64 {
    match role {
        PartRole::Attachment => 0,
        PartRole::Inline => 1,
        PartRole::Signature => 2,
    }
}

fn encoding_code(encoding: TransferEncoding) -> i64 {
    match encoding {
        TransferEncoding::Identity => 0,
        TransferEncoding::QuotedPrintable => 1,
        TransferEncoding::Base64 => 2,
    }
}

fn disposition_code(disposition: Disposition) -> i64 {
    match disposition {
        Disposition::Unspecified => 0,
        Disposition::Inline => 1,
        Disposition::Attachment => 2,
    }
}

fn part_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MimePart> {
    Ok(MimePart {
        section: row.get(0)?,
        role: match row.get::<_, i64>(2)? {
            1 => PartRole::Inline,
            2 => PartRole::Signature,
            _ => PartRole::Attachment,
        },
        mime_type: row.get(3)?,
        filename: row.get(4)?,
        size: row.get::<_, i64>(5)?.max(0) as u64,
        encoded_size: row.get::<_, i64>(6)?.max(0) as u64,
        encoding: match row.get::<_, i64>(7)? {
            1 => TransferEncoding::QuotedPrintable,
            2 => TransferEncoding::Base64,
            _ => TransferEncoding::Identity,
        },
        content_id: row.get(8)?,
        disposition: match row.get::<_, i64>(9)? {
            1 => Disposition::Inline,
            2 => Disposition::Attachment,
            _ => Disposition::Unspecified,
        },
        nested: row.get::<_, i64>(10)? != 0,
    })
}

/// Whether `message_id` has its part list recorded.
pub(crate) fn parts_listed(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<bool> {
    let listed: Option<i64> = conn
        .prepare_cached(
            "SELECT parts_listed FROM message_bodies WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_row(params![account_id.as_str(), message_id], |row| row.get(0))
        .optional()?;
    Ok(listed == Some(1))
}

/// Replace the part list of a message whose `message_bodies` row exists.
fn replace_list(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    list: &[MimePart],
) -> Result<()> {
    conn.execute(
        "DELETE FROM message_parts WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), message_id],
    )?;
    for (position, part) in list.iter().enumerate() {
        insert_part(
            conn,
            "INSERT INTO message_parts (account_id, message_id, section, position, role,
                mime_type, filename, size, encoded_size, encoding, content_id, disposition,
                nested)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            rusqlite::types::Value::Text(account_id.as_str().to_string()),
            message_id,
            position,
            part,
        )?;
    }
    conn.execute(
        "UPDATE message_bodies SET parts_listed = 1 WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), message_id],
    )?;
    Ok(())
}

/// The recorded part list of a canonical message id, in document order.
/// `None` when it was never taken.
pub(super) fn list(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Option<Vec<MimePart>>> {
    if !parts_listed(conn, account_id, message_id)? {
        return Ok(None);
    }
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {PART_COLUMNS} FROM message_parts
         WHERE account_id = ?1 AND message_id = ?2
         ORDER BY position"
    ))?;
    let parts = stmt
        .query_map(params![account_id.as_str(), message_id], part_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Some(parts))
}

/// Store `parts` for `message_id` and take it off the backfill queue. An
/// empty `parts` (no HTML, no plain) still writes a row when there is none,
/// recording that it was looked at, but never replaces parts with a body.
///
/// The part list follows the body: `Replace` with a body replaces it, and
/// otherwise a list is written only where none was taken yet. Returns
/// whether this write set the list.
pub(super) fn write(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    parts: &StoredParts,
    mode: Write,
) -> Result<bool> {
    conn.execute(
        "DELETE FROM body_backfill WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), message_id],
    )?;
    let sql = match mode {
        Write::Replace if parts.has_body() => {
            "INSERT INTO message_bodies (account_id, message_id, codec, html, plain, bulk_headers)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(account_id, message_id) DO UPDATE SET
                codec = excluded.codec,
                html = excluded.html,
                plain = excluded.plain,
                bulk_headers = excluded.bulk_headers,
                withheld = 0"
        }
        // Nothing to replace with: only record a row when there is none.
        Write::Replace => {
            "INSERT OR IGNORE INTO message_bodies
                (account_id, message_id, codec, html, plain, bulk_headers)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
        }
        Write::FillGap => FILL_GAP_SQL,
    };
    conn.execute(
        sql,
        params![
            account_id.as_str(),
            message_id,
            CODEC_DEFLATE,
            parts.html,
            parts.plain,
            parts.bulk_headers,
        ],
    )?;
    let Some(list) = &parts.mime_parts else {
        return Ok(false);
    };
    let replace = (mode == Write::Replace && parts.has_body())
        || !parts_listed(conn, account_id, message_id)?;
    if replace {
        replace_list(conn, account_id, message_id, list)?;
    }
    Ok(replace)
}

/// Insert a row, or fill each empty column of the stored one. Rows of
/// another codec are left alone rather than mixed.
const FILL_GAP_SQL: &str = "INSERT INTO message_bodies
        (account_id, message_id, codec, html, plain, bulk_headers)
     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
     ON CONFLICT(account_id, message_id) DO UPDATE SET
        html = COALESCE(message_bodies.html, excluded.html),
        plain = COALESCE(message_bodies.plain, excluded.plain),
        bulk_headers = CASE WHEN message_bodies.bulk_headers = ''
            THEN excluded.bulk_headers ELSE message_bodies.bulk_headers END,
        withheld = CASE WHEN excluded.html IS NOT NULL OR excluded.plain IS NOT NULL
            THEN 0 ELSE message_bodies.withheld END
     WHERE message_bodies.codec = excluded.codec";

/// Record that sync kept no body for `message_id` because it is larger than
/// sync downloads, so later passes count the copy as read and the reader
/// can say why there is no body. A body stored already is kept.
pub(super) fn mark_withheld(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO message_bodies (account_id, message_id, codec, withheld)
         VALUES (?1, ?2, ?3, 1)
         ON CONFLICT(account_id, message_id) DO UPDATE SET withheld = 1
         WHERE message_bodies.html IS NULL AND message_bodies.plain IS NULL",
        params![account_id.as_str(), message_id, CODEC_DEFLATE],
    )?;
    conn.execute(
        "DELETE FROM body_backfill WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), message_id],
    )?;
    Ok(())
}

/// Whether a stored message has its body: see [`crate::BodyState`]. A row
/// with no body text, no snippet and no `message_bodies` row is one a
/// first sync listed from its headers; mail with no server copy (a local
/// write) is never pending.
pub(super) fn state(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<crate::BodyState> {
    let row = conn
        .prepare_cached(
            "SELECT m.body_text IS NULL AND m.snippet = '',
                    b.message_id IS NOT NULL,
                    IFNULL(b.withheld, 0),
                    EXISTS (
                        SELECT 1 FROM message_copies c
                        WHERE c.account_id = m.account_id AND c.message_id = m.message_id
                    )
             FROM messages m
             LEFT JOIN message_bodies b
               ON b.account_id = m.account_id AND b.message_id = m.message_id
             WHERE m.account_id = ?1 AND m.message_id = ?2",
        )?
        .query_row(params![account_id.as_str(), message_id], |row| {
            Ok((
                row.get::<_, bool>(0)?,
                row.get::<_, bool>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })
        .optional()?;
    Ok(match row {
        Some((_, _, 1, _)) => crate::BodyState::TooLarge,
        Some((true, false, _, true)) => crate::BodyState::Pending,
        _ => crate::BodyState::Stored,
    })
}

/// A merge is about to delete row `from` in favour of `to`: give `to` the
/// parts it lacks from `from`, as stored. `to` leaves the backfill queue
/// when it gains a body. Returns the part list when `to` took `from`'s.
pub(super) fn move_into(
    conn: &Connection,
    account_id: &AccountId,
    from: &str,
    to: &str,
) -> Result<Option<Vec<MimePart>>> {
    let moved = conn.execute(
        "INSERT INTO message_bodies (account_id, message_id, codec, html, plain, bulk_headers)
         SELECT account_id, ?3, codec, html, plain, bulk_headers FROM message_bodies
         WHERE account_id = ?1 AND message_id = ?2
           AND (html IS NOT NULL OR plain IS NOT NULL)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
            html = COALESCE(message_bodies.html, excluded.html),
            plain = COALESCE(message_bodies.plain, excluded.plain),
            bulk_headers = CASE WHEN message_bodies.bulk_headers = ''
                THEN excluded.bulk_headers ELSE message_bodies.bulk_headers END
         WHERE message_bodies.codec = excluded.codec",
        params![account_id.as_str(), from, to],
    )?;
    if moved > 0 {
        conn.execute(
            "DELETE FROM body_backfill WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), to],
        )?;
    }
    // The part list goes with it when the kept row has none.
    if !parts_listed(conn, account_id, to)?
        && let Some(list) = list(conn, account_id, from)?
    {
        conn.execute(
            "INSERT OR IGNORE INTO message_bodies (account_id, message_id, codec)
             VALUES (?1, ?2, ?3)",
            params![account_id.as_str(), to, CODEC_DEFLATE],
        )?;
        replace_list(conn, account_id, to, &list)?;
        return Ok(Some(list));
    }
    Ok(None)
}

/// Stored parts for a canonical message id, or `None` when there is no row.
pub(super) fn read(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Option<MessageParts>> {
    let row = conn
        .query_row(
            "SELECT codec, html, plain, bulk_headers FROM message_bodies
             WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), message_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((codec, html, plain, headers)) = row else {
        return Ok(None);
    };
    if codec != CODEC_DEFLATE {
        return Err(Error::Backend(format!(
            "stored body uses unknown codec {codec}"
        )));
    }
    Ok(Some(MessageParts {
        html: html.as_deref().map(decompress).transpose()?,
        plain: plain.as_deref().map(decompress).transpose()?,
        bulk_headers: decode_headers(&headers),
        mime_parts: list(conn, account_id, message_id)?.unwrap_or_default(),
    }))
}

/// CROSS JOIN keeps the queue as the outer loop. Otherwise SQLite walks every
/// message in date order to skip the sort, even with an empty queue.
const MISSING_SQL: &str = "SELECT q.message_id FROM body_backfill q
     CROSS JOIN messages m
       ON m.account_id = q.account_id AND m.message_id = q.message_id
     WHERE q.account_id = ?1
     ORDER BY m.date_unix DESC
     LIMIT ?2";

/// Up to `limit` queued messages, newest first, with their server copies.
/// A queued message with no copy left is dropped from the queue.
pub(super) fn missing(
    conn: &Connection,
    account_id: &AccountId,
    limit: usize,
) -> Result<Vec<BodyRequest>> {
    let mut stmt = conn.prepare(MISSING_SQL)?;
    let ids = stmt
        .query_map(params![account_id.as_str(), limit as i64], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        let copies = copies_of(conn, account_id, &id)?;
        if copies.is_empty() {
            conn.execute(
                "DELETE FROM body_backfill WHERE account_id = ?1 AND message_id = ?2",
                params![account_id.as_str(), id],
            )?;
            continue;
        }
        out.push(BodyRequest {
            message_id: MessageId(id),
            copies,
        });
    }
    Ok(out)
}

/// The recorded server copies of one canonical message, Inbox first.
pub(super) fn copies_of(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Vec<RemoteId>> {
    let mut stmt = conn.prepare_cached(
        "SELECT mailbox, uidvalidity, uid FROM message_copies
         WHERE account_id = ?1 AND message_id = ?2
         ORDER BY mailbox != 'INBOX', mailbox, uid",
    )?;
    let rows = stmt.query_map(params![account_id.as_str(), message_id], |row| {
        Ok(RemoteId(format!(
            "imap:{}:{}:{}",
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(0)?
        )))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn compress(text: &str) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(text.as_bytes(), DEFLATE_LEVEL)
}

fn decompress(bytes: &[u8]) -> Result<String> {
    let raw = miniz_oxide::inflate::decompress_to_vec_with_limit(bytes, MAX_PART_BYTES)
        .map_err(|err| Error::Backend(format!("stored body does not inflate: {err}")))?;
    String::from_utf8(raw).map_err(|_| Error::Backend("stored body is not UTF-8".into()))
}

fn encode_headers(headers: &[(String, String)]) -> String {
    headers
        .iter()
        .map(|(name, value)| {
            let clean = |text: &str| text.replace(['\u{1e}', '\u{1f}'], " ");
            format!("{}\u{1f}{}", clean(name), clean(value))
        })
        .collect::<Vec<_>>()
        .join("\u{1e}")
}

fn decode_headers(raw: &str) -> Vec<(String, String)> {
    raw.split('\u{1e}')
        .filter(|record| !record.is_empty())
        .filter_map(|record| {
            let (name, value) = record.split_once('\u{1f}')?;
            Some((name.to_string(), value.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use crate::account::{Account, BackendKind};
    use crate::backend::MailboxDelta;
    use crate::message::Message;

    fn account() -> Account {
        Account {
            id: AccountId("a1".into()),
            display_name: "Caleb".into(),
            email: "c@x".into(),
            backend_kind: BackendKind::Bridge,
        }
    }

    /// A newsletter as IMAP sync parses it, from `mailbox` at `uid`.
    fn newsletter(id: &str, mailbox: &str, uid: u32, html: &str) -> Message {
        let raw = format!(
            "From: Shop <news@shop.example>\r\n\
             Subject: Sale\r\n\
             Message-ID: <{id}>\r\n\
             Date: Mon, 16 Nov 2026 12:00:00 +0000\r\n\
             List-Unsubscribe: <https://shop.example/u>\r\n\
             MIME-Version: 1.0\r\n\
             Content-Type: multipart/alternative; boundary=\"b\"\r\n\
             \r\n\
             --b\r\n\
             Content-Type: text/plain\r\n\
             \r\n\
             Sale today\r\n\
             --b\r\n\
             Content-Type: text/html\r\n\
             \r\n\
             {html}\r\n\
             --b--\r\n"
        );
        crate::imap::to_message(&account().id, mailbox, mailbox, 7, uid, &[], raw.as_bytes())
    }

    fn store_with(messages: Vec<Message>) -> Store {
        let store = Store::open_in_memory().unwrap();
        store.upsert_account(&account()).unwrap();
        store
            .apply_delta(
                &account().id,
                &MailboxDelta {
                    messages,
                    ..MailboxDelta::default()
                },
            )
            .unwrap();
        store
    }

    fn body(store: &Store, id: &str) -> Option<MessageParts> {
        store
            .message_body(&account().id, &MessageId::new(id))
            .unwrap()
    }

    #[test]
    fn parts_round_trip_compressed_and_lists_carry_no_body() {
        let html = format!(
            "<table>{}</table>",
            "<tr><td>Sale today</td></tr>".repeat(400)
        );
        let message = newsletter("n1@x", "INBOX", 1, &html);
        let expected = message.parts.clone().unwrap();
        let store = store_with(vec![message.clone()]);

        assert_eq!(body(&store, "n1@x"), Some(expected.clone()));
        assert_eq!(expected.html.as_deref().map(str::trim), Some(html.as_str()));
        let stored: i64 = store
            .conn
            .query_row(
                "SELECT length(html) FROM message_bodies WHERE message_id = 'n1@x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            (stored as usize) * 10 < html.len(),
            "{stored} compressed bytes for {} bytes of HTML",
            html.len()
        );

        // Lists and the conversation's message rows never carry parts.
        let thread = store
            .thread_messages(&account().id, &message.thread_id)
            .unwrap();
        assert_eq!(thread.len(), 1);
        assert!(thread.iter().all(|message| message.parts.is_none()));
        let listed = store
            .message(&account().id, &MessageId::new("n1@x"))
            .unwrap()
            .unwrap();
        assert!(listed.parts.is_none());
        // Search still reads the plain text, not the HTML.
        let hits = store.search(&account().id, "today").unwrap();
        assert_eq!(hits.hits.len(), 1);
        assert!(store.search(&account().id, "td").unwrap().hits.is_empty());
    }

    #[test]
    fn a_resync_replaces_parts_but_a_copy_without_a_body_does_not() {
        let store = store_with(vec![newsletter("n1@x", "INBOX", 1, "<p>first</p>")]);
        let mut again = newsletter("n1@x", "INBOX", 1, "<p>second</p>");
        store.upsert_message(&again).unwrap();
        assert_eq!(
            body(&store, "n1@x").unwrap().html.as_deref().map(str::trim),
            Some("<p>second</p>")
        );
        again.parts = Some(MessageParts::default());
        store.upsert_message(&again).unwrap();
        again.parts = None;
        store.upsert_message(&again).unwrap();
        assert_eq!(
            body(&store, "n1@x").unwrap().html.as_deref().map(str::trim),
            Some("<p>second</p>")
        );
    }

    #[test]
    fn deleting_the_message_deletes_its_parts() {
        let store = store_with(vec![newsletter("n1@x", "INBOX", 1, "<p>x</p>")]);
        assert!(
            store
                .delete_message(&account().id, &MessageId::new("n1@x"))
                .unwrap()
        );
        let left: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM message_bodies", [], |row| row.get(0))
            .unwrap();
        assert_eq!(left, 0);
        assert_eq!(body(&store, "n1@x"), None);
    }

    #[test]
    fn an_old_row_is_listed_for_backfill_newest_first_until_recorded() {
        let mut older = newsletter("old@x", "INBOX", 1, "<p>old</p>");
        older.headers.date_unix -= 60;
        let newer = newsletter("new@x", "Archive", 2, "<p>new</p>");
        let store = store_with(vec![older, newer]);
        // As the migration leaves mail stored before this table.
        store
            .conn
            .execute_batch(
                "DELETE FROM message_bodies;
                 INSERT INTO body_backfill SELECT account_id, message_id FROM messages;",
            )
            .unwrap();

        let wanted = store.missing_bodies(&account().id, 10).unwrap();
        assert_eq!(
            wanted,
            vec![
                BodyRequest {
                    message_id: MessageId::new("new@x"),
                    copies: vec![RemoteId("imap:7:2:Archive".into())],
                },
                BodyRequest {
                    message_id: MessageId::new("old@x"),
                    copies: vec![RemoteId("imap:7:1:INBOX".into())],
                },
            ]
        );
        assert_eq!(store.missing_bodies(&account().id, 1).unwrap().len(), 1);

        let parts = newsletter("new@x", "Archive", 2, "<p>new</p>").parts;
        let filled = store
            .store_fetched_parts(
                &account().id,
                &[
                    (MessageId::new("new@x"), parts.clone()),
                    (MessageId::new("old@x"), None),
                ],
            )
            .unwrap();
        assert_eq!(filled, 1);
        assert_eq!(body(&store, "new@x"), parts);
        assert!(store.missing_bodies(&account().id, 10).unwrap().is_empty());
        // A message nothing could be fetched for has an empty record, and
        // opening it still asks for it.
        assert_eq!(body(&store, "old@x"), Some(MessageParts::default()));
        assert!(
            store
                .body_request(&account().id, &MessageId::new("old@x"))
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .body_request(&account().id, &MessageId::new("new@x"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn opening_a_store_from_before_bodies_adds_the_table_and_keeps_mail() {
        let dir = std::env::temp_dir().join(format!(
            "fell-bodies-migrate-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.db");
        let old = newsletter("old@x", "INBOX", 1, "<p>old</p>");
        {
            let store = Store::open(&path).unwrap();
            store.upsert_account(&account()).unwrap();
            let mut synced_before = old.clone();
            synced_before.parts = None;
            store.upsert_message(&synced_before).unwrap();
            // What the mailbox looks like on main before this table: the same
            // schema without it.
            store
                .conn
                .execute_batch(
                    "DROP TRIGGER message_bodies_ad;
                     DROP TABLE message_bodies;
                     DROP TABLE body_backfill;
                     UPDATE meta SET value = '3' WHERE key = 'schema_version';",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let tables: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE name IN ('message_bodies', 'body_backfill', 'message_bodies_ad')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 3);
        let kept = store
            .message(&account().id, &MessageId::new("old@x"))
            .unwrap()
            .unwrap();
        assert_eq!(kept.body_text, old.body_text);
        assert_eq!(body(&store, "old@x"), None);
        assert_eq!(
            store.missing_bodies(&account().id, 10).unwrap(),
            vec![BodyRequest {
                message_id: MessageId::new("old@x"),
                copies: vec![RemoteId("imap:7:1:INBOX".into())],
            }]
        );
        // The next sync of the same UID fills it and takes it off the queue.
        store.upsert_message(&old).unwrap();
        assert_eq!(body(&store, "old@x"), old.parts);
        assert!(store.missing_bodies(&account().id, 10).unwrap().is_empty());
        drop(store);
        // A later open does not queue anything again.
        let store = Store::open(&path).unwrap();
        assert!(store.missing_bodies(&account().id, 10).unwrap().is_empty());
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_collapsed_list_copy_fills_a_gap_but_keeps_stored_parts() {
        // The list echo of a sent message: same body, rewritten id and From.
        let mut sent = newsletter("post@x", "Sent", 1, "<p>sent</p>");
        sent.folders = vec!["Sent".into()];
        let mut echo = newsletter("echo@lists.x", "INBOX", 2, "<p>with footer</p>");
        echo.list_id = Some("list.x".into());
        echo.headers.subject = "[list] Sale".into();
        let store = store_with(vec![sent]);
        store.upsert_message(&echo).unwrap();
        let kept = store
            .message(&account().id, &MessageId::new("echo@lists.x"))
            .unwrap()
            .unwrap();
        assert_eq!(kept.headers.message_id.as_str(), "post@x", "collapsed");
        assert_eq!(
            body(&store, "echo@lists.x")
                .unwrap()
                .html
                .as_deref()
                .map(str::trim),
            Some("<p>sent</p>")
        );
    }

    #[test]
    fn a_list_echo_adds_html_to_a_plain_only_sent_copy() {
        // The Sent copy went out as plain text; the list added an HTML part.
        let mut sent = newsletter("post@x", "Sent", 1, "<p>unused</p>");
        sent.folders = vec!["Sent".into()];
        let sent_parts = sent.parts.as_mut().unwrap();
        sent_parts.html = None;
        sent_parts.bulk_headers.clear();
        let plain = sent_parts.plain.clone();
        let mut echo = newsletter("echo@lists.x", "INBOX", 2, "<p>with footer</p>");
        echo.list_id = Some("list.x".into());
        echo.headers.subject = "[list] Sale".into();
        let echo_parts = echo.parts.clone().unwrap();
        let store = store_with(vec![sent]);
        store.upsert_message(&echo).unwrap();
        let kept = body(&store, "post@x").unwrap();
        assert_eq!(kept.plain, plain, "the stored plain part stays");
        assert_eq!(kept.html, echo_parts.html, "the echo fills the HTML");
        assert_eq!(kept.bulk_headers, echo_parts.bulk_headers);

        // A fetch for the gap fills the same way and keeps what is stored.
        let fetched = MessageParts {
            mime_parts: Vec::new(),
            html: Some("<p>other</p>".into()),
            plain: Some("other".into()),
            bulk_headers: Vec::new(),
        };
        store
            .store_fetched_parts(&account().id, &[(MessageId::new("post@x"), Some(fetched))])
            .unwrap();
        assert_eq!(body(&store, "post@x").unwrap(), kept);
    }

    fn with_attachments(id: &str, mailbox: &str, uid: u32) -> Message {
        let fixture = crate::imap::mime_fixtures::mixed_two_attachments(id);
        crate::imap::to_message(&account().id, mailbox, mailbox, 7, uid, &[], &fixture.raw)
    }

    fn attachments(store: &Store, id: &str) -> Option<Vec<crate::MimePart>> {
        store
            .message_attachments(&account().id, &MessageId::new(id))
            .unwrap()
    }

    fn part_rows(store: &Store) -> i64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM message_parts", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn part_lists_are_kept_read_back_and_deleted_with_the_message() {
        let message = with_attachments("pdf@x", "INBOX", 1);
        let list = message.parts.clone().unwrap().mime_parts;
        assert_eq!(list.len(), 2);
        let store = store_with(vec![message.clone()]);
        assert_eq!(attachments(&store, "pdf@x"), Some(list.clone()));
        assert_eq!(body(&store, "pdf@x").unwrap().mime_parts, list);
        let stored = store
            .message(&account().id, &MessageId::new("pdf@x"))
            .unwrap()
            .unwrap();
        assert!(stored.has_attachment);
        assert_eq!(stored.attachment_names, vec!["report.pdf", "totals.csv"]);
        // A part request carries the part and the copies to fetch it from.
        let request = store
            .part_request(&account().id, &MessageId::new("pdf@x"), "3")
            .unwrap()
            .unwrap();
        assert_eq!(request.part, list[1]);
        assert_eq!(request.copies, vec![RemoteId("imap:7:1:INBOX".into())]);
        assert!(
            store
                .part_request(&account().id, &MessageId::new("pdf@x"), "9")
                .unwrap()
                .is_none()
        );
        // Plain mail has an empty list, not an unknown one.
        let plain = newsletter("plain@x", "INBOX", 2, "<p>x</p>");
        store.upsert_message(&plain).unwrap();
        assert_eq!(attachments(&store, "plain@x"), Some(Vec::new()));

        assert!(
            store
                .delete_message(&account().id, &MessageId::new("pdf@x"))
                .unwrap()
        );
        assert_eq!(part_rows(&store), 0);
        assert_eq!(attachments(&store, "pdf@x"), None);
    }

    #[test]
    fn a_resync_replaces_the_list_but_a_copy_without_a_body_keeps_it() {
        let store = store_with(vec![with_attachments("pdf@x", "INBOX", 1)]);
        let mut signed = {
            let fixture = crate::imap::mime_fixtures::pgp_signed("pdf@x");
            crate::imap::to_message(&account().id, "INBOX", "INBOX", 7, 1, &[], &fixture.raw)
        };
        let mut bodiless = signed.clone();
        bodiless.parts = Some(MessageParts::default());
        store.upsert_message(&bodiless).unwrap();
        assert_eq!(attachments(&store, "pdf@x").unwrap().len(), 2);
        signed.has_attachment = false;
        store.upsert_message(&signed).unwrap();
        let list = attachments(&store, "pdf@x").unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].role, crate::PartRole::Signature);
        // The marker and names follow the list that replaced the old one.
        let kept = store
            .message(&account().id, &MessageId::new("pdf@x"))
            .unwrap()
            .unwrap();
        assert!(!kept.has_attachment);
        assert!(kept.attachment_names.is_empty());
        assert!(
            store
                .search(&account().id, "has:attachment")
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn a_collapsed_copy_whose_list_is_not_kept_adds_no_names() {
        let mut sent = with_attachments("post@x", "Sent", 1);
        sent.folders = vec!["Sent".into()];
        // The list echo of it, with a footer file the Sent copy never had.
        let mut echo = with_attachments("echo@lists.x", "INBOX", 2);
        echo.list_id = Some("list.x".into());
        echo.headers.subject = format!("[list] {}", sent.headers.subject);
        let mut footer = echo.parts.as_ref().unwrap().mime_parts[1].clone();
        footer.section = "4".into();
        footer.filename = Some("footer.txt".into());
        echo.parts.as_mut().unwrap().mime_parts.push(footer);
        echo.attachment_names.push("footer.txt".into());
        let store = store_with(vec![sent]);
        store.upsert_message(&echo).unwrap();
        let kept = store
            .message(&account().id, &MessageId::new("echo@lists.x"))
            .unwrap()
            .unwrap();
        assert_eq!(kept.headers.message_id.as_str(), "post@x", "collapsed");
        assert_eq!(attachments(&store, "post@x").unwrap().len(), 2);
        assert_eq!(kept.attachment_names, vec!["report.pdf", "totals.csv"]);
        assert!(kept.has_attachment);
    }

    #[test]
    fn the_first_list_replaces_a_marker_an_older_classifier_set() {
        // Issue #148 on mail stored before part lists: a signed message got
        // has_attachment and the name signature.asc.
        let fixture = crate::imap::mime_fixtures::pgp_signed("sig@x");
        let mut old =
            crate::imap::to_message(&account().id, "INBOX", "INBOX", 7, 1, &[], &fixture.raw);
        let parsed = old.parts.take().unwrap();
        old.has_attachment = true;
        old.attachment_names = vec!["signature.asc".into()];
        let other = {
            let mut other = old.clone();
            other.headers.message_id = MessageId::new("sig2@x");
            other.remote_id = RemoteId("imap:7:2:INBOX".into());
            other.thread_id = crate::ThreadId::from_message_id(&other.headers.message_id);
            other
        };
        let store = store_with(vec![old.clone(), other.clone()]);
        let marker = |id: &str| {
            let stored = store
                .message(&account().id, &MessageId::new(id))
                .unwrap()
                .unwrap();
            (stored.has_attachment, stored.attachment_names)
        };
        assert_eq!(marker("sig@x"), (true, vec!["signature.asc".to_string()]));
        assert_eq!(attachments(&store, "sig@x"), None);

        // The backfill lists it.
        store
            .store_fetched_parts(&account().id, &[(MessageId::new("sig@x"), Some(parsed))])
            .unwrap();
        assert_eq!(marker("sig@x"), (false, Vec::new()));
        let threads = store.folder_threads(&account().id, "INBOX").unwrap();
        let row = threads
            .iter()
            .find(|thread| thread.thread_id == old.thread_id)
            .unwrap();
        assert!(!row.has_attachment, "the folder list follows");
        assert!(
            store
                .search(&account().id, "has:attachment")
                .unwrap()
                .hits
                .len()
                == 1
        );

        // A sync of the other one does the same.
        let resynced = crate::imap::to_message(&account().id, "INBOX", "INBOX", 7, 2, &[], &{
            crate::imap::mime_fixtures::pgp_signed("sig2@x").raw
        });
        store.upsert_message(&resynced).unwrap();
        assert_eq!(marker("sig2@x"), (false, Vec::new()));
        assert!(
            store
                .search(&account().id, "has:attachment")
                .unwrap()
                .hits
                .is_empty()
        );
    }

    #[test]
    fn mail_stored_before_part_lists_is_queued_once_by_version_5() {
        let dir = std::env::temp_dir().join(format!(
            "fell-parts-migrate-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.db");
        let old = with_attachments("pdf@x", "INBOX", 1);
        {
            let store = Store::open(&path).unwrap();
            store.upsert_account(&account()).unwrap();
            store.upsert_message(&old).unwrap();
            // A version 4 mailbox: bodies without part lists.
            store
                .conn
                .execute_batch(
                    "DROP TRIGGER message_parts_ad;
                     DROP TABLE message_parts;
                     ALTER TABLE message_bodies DROP COLUMN parts_listed;
                     UPDATE meta SET value = '4' WHERE key = 'schema_version';",
                )
                .unwrap();
        }
        let store = Store::open(&path).unwrap();
        assert_eq!(attachments(&store, "pdf@x"), None);
        let queued = store.missing_bodies(&account().id, 10).unwrap();
        assert_eq!(queued.len(), 1);
        // Opening it asks too, though its body is stored.
        assert!(
            store
                .body_request(&account().id, &MessageId::new("pdf@x"))
                .unwrap()
                .is_some()
        );
        let parts = old.parts.clone();
        store
            .store_fetched_parts(&account().id, &[(MessageId::new("pdf@x"), parts.clone())])
            .unwrap();
        assert_eq!(
            attachments(&store, "pdf@x"),
            Some(parts.unwrap().mime_parts)
        );
        assert!(store.missing_bodies(&account().id, 10).unwrap().is_empty());
        drop(store);
        let store = Store::open(&path).unwrap();
        assert!(store.missing_bodies(&account().id, 10).unwrap().is_empty());
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn plan(store: &Store, sql: &str) -> String {
        store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .unwrap()
            .query_map(["a1", "m@x"], |row| row.get::<_, String>(3))
            .unwrap()
            .map(|row| row.unwrap())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn body_lookups_never_scan_messages() {
        let store = Store::open_in_memory().unwrap();
        for sql in [MISSING_SQL, super::super::CANONICAL_ID_SQL] {
            let plan = plan(&store, sql);
            assert!(!plan.contains("SCAN m"), "{sql}\n{plan}");
        }
        let missing = plan(&store, MISSING_SQL);
        assert!(missing.lines().next().unwrap().contains("q"), "{missing}");
    }

    #[test]
    fn bulk_headers_round_trip_through_the_record_separators() {
        let headers = vec![
            ("List-Unsubscribe".to_string(), "<a>, <b>".to_string()),
            ("X-Odd".to_string(), "x\u{1f}y\u{1e}z".to_string()),
        ];
        let decoded = decode_headers(&encode_headers(&headers));
        assert_eq!(decoded[0], headers[0]);
        assert_eq!(decoded[1], ("X-Odd".to_string(), "x y z".to_string()));
        assert!(decode_headers("").is_empty());
    }
}
