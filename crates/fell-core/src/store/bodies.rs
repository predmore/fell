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
//! parts takes a message off the queue. The CID upgrade also queues listed
//! parts that need their source to be reclassified. New mail never joins
//! the queue, so a mailbox that is caught up pays nothing per pass.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::account::AccountId;
use crate::imap::{SNIPPET_BYTES, cap_snippet, html_snippet_text};
use crate::message::{
    Disposition, MessageId, MessageParts, MimePart, PartRole, RemoteId, TransferEncoding,
};
use crate::search::{sync_terms, terms_for_indexed};
use crate::{BodyState, Error, Result, ThreadId};

pub(super) mod cid_upgrade;

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
    -- 0 unknown, 1 parsed from the message, 2/3 from BODYSTRUCTURE
    -- (without/with HTML). A known list may be empty.
    parts_listed INTEGER NOT NULL DEFAULT 0,
    -- Only this copy can serve sections from its provisional structure.
    structure_copy TEXT,
    -- 1 when sync kept no body: the message is larger than it downloads.
    withheld INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (account_id, message_id)
);

-- Revisions live apart from the blobs so updating one need not rewrite the body.
CREATE TABLE IF NOT EXISTS message_body_stamps (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    stamp BLOB NOT NULL,
    PRIMARY KEY (account_id, message_id)
) WITHOUT ROWID;

CREATE TRIGGER IF NOT EXISTS message_body_stamps_ai AFTER INSERT ON message_bodies BEGIN
    INSERT INTO message_body_stamps VALUES (new.account_id, new.message_id, randomblob(16))
    ON CONFLICT(account_id, message_id) DO UPDATE SET stamp = excluded.stamp;
END;

CREATE TRIGGER IF NOT EXISTS message_body_stamps_au
AFTER UPDATE OF codec, html, plain, bulk_headers ON message_bodies
WHEN old.codec IS NOT new.codec OR old.html IS NOT new.html
  OR old.plain IS NOT new.plain OR old.bulk_headers IS NOT new.bulk_headers
BEGIN
    INSERT INTO message_body_stamps VALUES (new.account_id, new.message_id, randomblob(16))
    ON CONFLICT(account_id, message_id) DO UPDATE SET stamp = excluded.stamp;
END;

CREATE TRIGGER IF NOT EXISTS message_body_stamps_ad AFTER DELETE ON message_bodies BEGIN
    DELETE FROM message_body_stamps
    WHERE account_id = old.account_id AND message_id = old.message_id;
END;

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

-- Messages still to fetch again, including a targeted CID role refresh.
CREATE TABLE IF NOT EXISTS body_backfill (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    refresh_cid_roles INTEGER NOT NULL DEFAULT 0,
    structure_unavailable INTEGER NOT NULL DEFAULT 0,
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
    let tx = super::begin_immediate(conn)?;
    tx.execute_batch(BODIES_SCHEMA)?;
    tx.execute_batch(super::part_cache::SCHEMA)?;
    if !has_column(&tx, "message_bodies", "parts_listed")? {
        tx.execute_batch(
            "ALTER TABLE message_bodies ADD COLUMN parts_listed INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if !has_column(&tx, "message_bodies", "structure_copy")? {
        tx.execute_batch("ALTER TABLE message_bodies ADD COLUMN structure_copy TEXT")?;
    }
    if !has_column(&tx, "message_bodies", "withheld")? {
        tx.execute_batch(
            "ALTER TABLE message_bodies ADD COLUMN withheld INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if !has_column(&tx, "body_backfill", "refresh_cid_roles")? {
        tx.execute_batch(
            "ALTER TABLE body_backfill ADD COLUMN refresh_cid_roles INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if !has_column(&tx, "body_backfill", "structure_unavailable")? {
        tx.execute_batch(
            "ALTER TABLE body_backfill ADD COLUMN structure_unavailable INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    tx.commit()?;
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
               AND (b.parts_listed = 1 OR b.withheld = 1)
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
    trusted_sender_delivery: bool,
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
            trusted_sender_delivery: parts.trusted_sender_delivery,
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
                bulk_headers = excluded.bulk_headers,
                withheld = 0"
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
                "UPDATE local_action_bodies SET parts_listed = 1, structure_copy = NULL
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

/// What the reader knows before it downloads the full message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ReaderParts {
    #[default]
    Ready,
    Unknown,
    Listed {
        html: bool,
    },
    Unavailable {
        html: bool,
    },
}

// A complete list comes from a full source. Only other states need to
// check whether there is still a server copy to read from.
const READER_STATES_SQL: &str = "
    SELECT m.message_id, typeof(m.body_text) = 'null' AND m.snippet = '',
           b.parts_listed, typeof(b.html) != 'null', typeof(b.plain) != 'null',
           b.withheld IS 1,
           CASE WHEN b.parts_listed = 1 THEN 0 ELSE EXISTS (
               SELECT 1 FROM message_copies c
               WHERE c.account_id = m.account_id AND c.message_id = m.message_id
           ) END,
           CASE WHEN b.parts_listed IN (2, 3) THEN EXISTS (
               SELECT 1 FROM message_copies c
               WHERE c.account_id = m.account_id AND c.message_id = m.message_id
                 AND b.structure_copy = 'imap:' || c.uidvalidity || ':' || c.uid || ':' || c.mailbox
           ) ELSE 0 END
    FROM messages m LEFT JOIN message_bodies b USING (account_id, message_id)
    WHERE m.account_id = ?1";

fn reader_states_from_row(row: &Row<'_>) -> rusqlite::Result<(BodyState, ReaderParts)> {
    let no_preview: bool = row.get(1)?;
    let code: Option<i64> = row.get(2)?;
    let html: bool = row.get(3)?;
    let plain: bool = row.get(4)?;
    let withheld: bool = row.get(5)?;
    let has_copy: bool = row.get(6)?;
    let has_structure_copy: bool = row.get(7)?;
    let provisional = matches!(code, Some(2 | 3));
    let has_body = code.is_some() && (!provisional || html || plain);
    let state = if no_preview && withheld {
        BodyState::TooLarge
    } else if no_preview && !has_body && has_copy {
        BodyState::Pending
    } else {
        BodyState::Stored
    };
    let needs_html = code == Some(3) && !html;
    let parts = match (code.unwrap_or(0), has_copy, has_structure_copy) {
        (0, true, _) | (2 | 3, true, false) => ReaderParts::Unknown,
        (2 | 3, true, true) => ReaderParts::Listed { html: needs_html },
        (2 | 3, false, _) => ReaderParts::Unavailable { html: needs_html },
        _ => ReaderParts::Ready,
    };
    Ok((state, parts))
}

pub(super) fn reader_states(
    conn: &Connection,
    account: &AccountId,
    id: &str,
) -> Result<(BodyState, ReaderParts)> {
    let states = conn
        .prepare_cached(&format!("{READER_STATES_SQL} AND m.message_id = ?2"))?
        .query_row(params![account.as_str(), id], reader_states_from_row)
        .optional()?;
    Ok(states.unwrap_or((BodyState::Stored, ReaderParts::Ready)))
}

pub(super) fn thread_reader_states(
    conn: &Connection,
    account: &AccountId,
    thread: &ThreadId,
) -> Result<HashMap<String, (BodyState, ReaderParts)>> {
    let mut stmt = conn.prepare_cached(&format!("{READER_STATES_SQL} AND m.thread_id = ?2"))?;
    let states = stmt
        .query_map(params![account.as_str(), thread.as_str()], |row| {
            Ok((row.get(0)?, reader_states_from_row(row)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(states)
}

fn list_code(conn: &Connection, account_id: &AccountId, message_id: &str) -> Result<i64> {
    Ok(conn
        .prepare_cached(
            "SELECT parts_listed FROM message_bodies WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_row(params![account_id.as_str(), message_id], |row| row.get(0))
        .optional()?
        .unwrap_or(0))
}

/// Whether the part list was parsed from the full source.
pub(crate) fn parts_listed(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<bool> {
    Ok(list_code(conn, account_id, message_id)? == 1)
}

/// Write provisional metadata from a live copy, replacing a provisional list
/// whose source disappeared. A full source stored by sync always wins.
pub(super) fn write_structure(
    conn: &Connection,
    account: &AccountId,
    id: &str,
    copy: &RemoteId,
    structure: &crate::imap::BodyStructure,
) -> Result<bool> {
    let (code, source) = structure_source(conn, account, id)?;
    if code == 1 {
        return Ok(false);
    }
    let copies = copies_of(conn, account, id)?;
    if !copies.contains(copy)
        || source.is_some_and(|source| copies.iter().any(|copy| copy.0 == source))
    {
        return Ok(false);
    }
    conn.execute(
        "INSERT OR IGNORE INTO message_bodies (account_id, message_id, codec)
         VALUES (?1, ?2, ?3)",
        params![account.as_str(), id, CODEC_DEFLATE],
    )?;
    replace_list(
        conn,
        account,
        id,
        &structure.parts,
        if structure.html { 3 } else { 2 },
        Some(&copy.0),
    )?;
    Ok(true)
}

/// Replace the part list of a message whose `message_bodies` row exists.
fn replace_list(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    list: &[MimePart],
    code: i64,
    source: Option<&str>,
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
        "UPDATE message_bodies SET parts_listed = ?3, structure_copy = ?4
         WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), message_id, code, source],
    )?;
    Ok(())
}

/// Copies safe to use for the listed sections. A BODYSTRUCTURE list belongs
/// to the copy that supplied it, and can report only the encoded byte size.
pub(super) fn part_copies(
    conn: &Connection,
    account: &AccountId,
    id: &str,
) -> Result<(Vec<RemoteId>, bool)> {
    let (code, source) = structure_source(conn, account, id)?;
    let mut copies = copies_of(conn, account, id)?;
    if code != 1 {
        copies.retain(|copy| Some(&copy.0) == source.as_ref());
    }
    Ok((copies, code == 1))
}

fn structure_source(
    conn: &Connection,
    account: &AccountId,
    id: &str,
) -> Result<(i64, Option<String>)> {
    Ok(conn
        .prepare_cached(
            "SELECT parts_listed, structure_copy FROM message_bodies
         WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_row(params![account.as_str(), id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .optional()?
        .unwrap_or_default())
}

/// The recorded part list of a canonical message id, in document order.
/// `None` when it was never taken.
pub(super) fn list(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Option<Vec<MimePart>>> {
    if list_code(conn, account_id, message_id)? == 0 {
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

/// Store `parts` for `message_id` and clear the regular body backfill. An
/// empty `parts` (no HTML, no plain) still records that it was looked at.
/// A CID refresh belongs to the existing part list until it is replaced.
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
    let mut headers = std::borrow::Cow::Borrowed(parts.bulk_headers.as_str());
    if mode == Write::Replace
        && parts.has_body()
        && !parts.trusted_sender_delivery
        && let Some(previous) = read_headers(conn, account_id, message_id)?
            .and_then(|headers| sender_attestation(&headers))
    {
        headers = with_attestation(&headers, Some(previous)).into();
    }
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
            headers.as_ref(),
        ],
    )?;
    if mode != Write::Replace || !parts.has_body() {
        refresh_sender_attestation(conn, account_id, message_id, parts)?;
    }
    let mut replaced = false;
    if let Some(list) = &parts.mime_parts
        && ((mode == Write::Replace && parts.has_body())
            || !parts_listed(conn, account_id, message_id)?)
    {
        replace_list(conn, account_id, message_id, list, 1, None)?;
        replaced = true;
    }
    conn.execute(
        "DELETE FROM body_backfill WHERE account_id = ?1 AND message_id = ?2
         AND (refresh_cid_roles = 0 OR ?3)",
        params![account_id.as_str(), message_id, replaced],
    )?;
    Ok(replaced)
}

fn read_headers(conn: &Connection, account: &AccountId, id: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT bulk_headers FROM message_bodies
            WHERE account_id = ?1 AND message_id = ?2",
        )?
        .query_row(params![account.as_str(), id], |row| row.get(0))
        .optional()?)
}

fn sender_attestation(headers: &str) -> Option<(String, String)> {
    decode_headers(headers)
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(crate::message::VERIFIED_SENDER_HEADER))
}

fn with_attestation(headers: &str, attestation: Option<(String, String)>) -> String {
    let mut headers = decode_headers(headers);
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case(crate::message::VERIFIED_SENDER_HEADER));
    headers.extend(attestation);
    encode_headers(&headers)
}

pub(super) fn refresh_sender_attestation(
    conn: &Connection,
    account: &AccountId,
    id: &str,
    parts: &StoredParts,
) -> Result<()> {
    if parts.trusted_sender_delivery
        && let Some(headers) = read_headers(conn, account, id)?
    {
        let attestation = sender_attestation(&parts.bulk_headers);
        if sender_attestation(&headers) != attestation {
            conn.execute(
                "UPDATE message_bodies SET bulk_headers = ?3
                 WHERE account_id = ?1 AND message_id = ?2",
                params![
                    account.as_str(),
                    id,
                    with_attestation(&headers, attestation)
                ],
            )?;
        }
    }
    Ok(())
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
        "DELETE FROM body_backfill WHERE account_id = ?1 AND message_id = ?2
         AND refresh_cid_roles = 0",
        params![account_id.as_str(), message_id],
    )?;
    Ok(())
}

/// A merge is about to delete row `from` in favour of `to`: give `to` the
/// parts it lacks and carry over any size-limit marker. The kept row leaves
/// the regular backfill queue when it gains a body or a size-limit marker.
/// A pending CID repair follows the part list it describes. Returns the
/// part list when `to` took `from`'s.
pub(super) fn move_into(
    conn: &Connection,
    account_id: &AccountId,
    from: &str,
    to: &str,
) -> Result<Option<Vec<MimePart>>> {
    let moved = conn.execute(
        "INSERT INTO message_bodies
            (account_id, message_id, codec, html, plain, bulk_headers, withheld)
         SELECT account_id, ?3, codec, html, plain, bulk_headers, withheld FROM message_bodies
         WHERE account_id = ?1 AND message_id = ?2
           AND (html IS NOT NULL OR plain IS NOT NULL OR withheld = 1)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
            html = COALESCE(message_bodies.html, excluded.html),
            plain = COALESCE(message_bodies.plain, excluded.plain),
            bulk_headers = CASE WHEN message_bodies.bulk_headers = ''
                THEN excluded.bulk_headers ELSE message_bodies.bulk_headers END,
            withheld = CASE WHEN COALESCE(message_bodies.html, excluded.html) IS NULL
                              AND COALESCE(message_bodies.plain, excluded.plain) IS NULL
                THEN MAX(message_bodies.withheld, excluded.withheld) ELSE 0 END
         WHERE message_bodies.codec = excluded.codec",
        params![account_id.as_str(), from, to],
    )?;
    if moved > 0 {
        conn.execute(
            "DELETE FROM body_backfill WHERE account_id = ?1 AND message_id = ?2
             AND refresh_cid_roles = 0",
            params![account_id.as_str(), to],
        )?;
    }
    // The part list goes with it when the kept row has none.
    let source_list = list_code(conn, account_id, from)?;
    let kept_list = list_code(conn, account_id, to)?;
    if (kept_list == 0 || (kept_list != 1 && source_list == 1))
        && let Some(list) = list(conn, account_id, from)?
    {
        conn.execute(
            "INSERT OR IGNORE INTO message_bodies (account_id, message_id, codec)
             VALUES (?1, ?2, ?3)",
            params![account_id.as_str(), to, CODEC_DEFLATE],
        )?;
        let source: Option<String> = conn.prepare_cached(
            "SELECT structure_copy FROM message_bodies WHERE account_id = ?1 AND message_id = ?2",
        )?.query_row(params![account_id.as_str(), from], |row| row.get(0))?;
        replace_list(conn, account_id, to, &list, source_list, source.as_deref())?;
        cid_upgrade::inherited_list(conn, account_id, from, to)?;
        conn.execute(
            "INSERT INTO body_backfill (account_id, message_id, refresh_cid_roles)
             SELECT account_id, ?3, 1 FROM body_backfill
             WHERE account_id = ?1 AND message_id = ?2 AND refresh_cid_roles = 1
             ON CONFLICT(account_id, message_id) DO UPDATE SET refresh_cid_roles = 1",
            params![account_id.as_str(), from, to],
        )?;
        return Ok(Some(list));
    }
    Ok(None)
}

pub(super) fn read_body_text(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Option<MessageParts>> {
    read_text(
        conn,
        "SELECT codec, html, plain, bulk_headers FROM message_bodies
         WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), message_id],
    )
}

/// Stored parts for a canonical message id, or `None` when there is no row.
pub(super) fn read(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<Option<MessageParts>> {
    let mut parts = read_body_text(conn, account_id, message_id)?;
    if let Some(parts) = &mut parts {
        parts.mime_parts = list(conn, account_id, message_id)?.unwrap_or_default();
    }
    Ok(parts)
}

/// The text parts only, with the stamp checked in the same SQLite read.
pub(super) fn read_at_stamp(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    stamp: PartStamp,
) -> Result<Option<MessageParts>> {
    read_text(
        conn,
        "SELECT b.codec, b.html, b.plain, b.bulk_headers
         FROM message_bodies b LEFT JOIN message_body_stamps s USING (account_id, message_id)
         WHERE b.account_id = ?1 AND b.message_id = ?2
           AND coalesce(s.stamp, zeroblob(16)) = ?3",
        params![account_id.as_str(), message_id, stamp.0.as_slice()],
    )
}

pub(super) fn repair_html_snippets(conn: &Connection) -> Result<Option<bool>> {
    const KEY: &str = "html_snippet_block_spacing_v2";
    let progress = |conn: &Connection| {
        conn.query_row(
            "SELECT CAST(value AS INTEGER) FROM meta WHERE key = ?1",
            [KEY],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map(|value| value.unwrap_or(0))
    };
    let cursor = progress(conn)?;
    if cursor < 0 {
        return Ok(None);
    }

    // Decode and render outside the write transaction, keeping just the
    // small replacement snippets for the transaction.
    let mut stmt = conn.prepare_cached(
        "SELECT b.rowid, b.account_id, b.message_id, b.html, m.snippet, s.stamp
         FROM message_bodies b JOIN messages m USING (account_id, message_id)
         LEFT JOIN message_body_stamps s USING (account_id, message_id)
         WHERE b.rowid > ?1 AND b.codec = ?2 AND b.html IS NOT NULL AND b.plain IS NULL
         ORDER BY b.rowid LIMIT 16",
    )?;
    let mut rows = stmt.query(params![cursor, CODEC_DEFLATE])?;
    let mut next = -1;
    let mut updates = Vec::new();
    while let Some(row) = rows.next()? {
        next = row.get(0)?;
        let html: Vec<u8> = row.get(3)?;
        let old: String = row.get(4)?;
        if let Ok(html) = decompress(&html)
            && let Some(full) = html_snippet_text(&html)
            && let Some(snippet) = spacing_repair(&old, &full)
        {
            let account: String = row.get(1)?;
            let message: String = row.get(2)?;
            let stamp: Option<Vec<u8>> = row.get(5)?;
            updates.push((account, message, old, stamp, snippet.to_owned()));
        }
    }
    drop(rows);
    drop(stmt);

    let tx = super::immediate_transaction(conn)?;
    if progress(&tx)? != cursor {
        return Ok(Some(false));
    }
    let mut changed = false;
    for (account, message, old, stamp, snippet) in updates {
        let wrote = tx.execute(
            "UPDATE messages SET snippet = ?5
             WHERE account_id = ?1 AND message_id = ?2 AND snippet = ?3
               AND (SELECT stamp FROM message_body_stamps
                    WHERE account_id = ?1 AND message_id = ?2) IS ?4",
            params![account, message, old, stamp, snippet],
        )? > 0;
        if wrote {
            sync_snippet_terms(&tx, &account, &message, &old, &snippet)?;
            changed = true;
        }
    }
    tx.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![KEY, next],
    )?;
    tx.commit()?;
    Ok((next >= 0).then_some(changed))
}

fn non_whitespace(text: &str) -> impl Iterator<Item = char> + '_ {
    text.chars().filter(|ch| !ch.is_whitespace())
}

fn spacing_repair<'a>(old: &str, full: &'a str) -> Option<&'a str> {
    let new = cap_snippet(full);
    if old == new {
        return None;
    }
    // The native renderer can omit hidden preheaders or include image alt
    // text. Compare visible characters against the full text first.
    let mut full_chars = non_whitespace(full);
    if !non_whitespace(old).all(|ch| full_chars.next() == Some(ch)) {
        return None;
    }
    let Some(next) = full_chars.next() else {
        return Some(new);
    };
    // A legacy snippet can end just below the byte cap at a UTF-8 boundary.
    // Added spacing can push characters out, but must never bring any in.
    if old.len() + next.len_utf8() <= SNIPPET_BYTES {
        return None;
    }
    let mut old_chars = non_whitespace(old);
    non_whitespace(new)
        .all(|ch| old_chars.next() == Some(ch))
        .then_some(new)
}

impl StoredParts {
    pub(super) fn matches_search_body(
        &self,
        conn: &Connection,
        account: &AccountId,
        message: &str,
    ) -> Result<bool> {
        Ok(conn
            .prepare_cached(
                "SELECT EXISTS (
                     SELECT 1 FROM message_bodies
                     WHERE account_id = ?1 AND message_id = ?2 AND codec = ?3
                       AND (plain = ?4 OR (?4 IS NULL AND plain IS NULL AND html = ?5))
                 )",
            )?
            .query_row(
                params![
                    account.as_str(),
                    message,
                    CODEC_DEFLATE,
                    self.plain,
                    self.html
                ],
                |row| row.get(0),
            )?)
    }
}

pub(super) fn fill_fetched_html_snippet(
    conn: &Connection,
    account: &AccountId,
    message: &str,
    parts: &StoredParts,
    full_snippet: &str,
) -> Result<()> {
    // FillGap can keep parts from a different copy. Use the preview only
    // when its HTML was kept and the stored message is still HTML-only.
    let old: Option<String> = conn
        .prepare_cached(
            "SELECT m.snippet FROM messages m JOIN message_bodies b USING (account_id, message_id)
             WHERE m.account_id = ?1 AND m.message_id = ?2
               AND b.codec = ?3 AND b.plain IS NULL AND b.html = ?4",
        )?
        .query_row(
            params![account.as_str(), message, CODEC_DEFLATE, parts.html],
            |row| row.get(0),
        )
        .optional()?;
    let Some(old) = old else {
        return Ok(());
    };
    let snippet = if old.is_empty() {
        Some(cap_snippet(full_snippet))
    } else {
        spacing_repair(&old, full_snippet)
    };
    if let Some(snippet) = snippet {
        conn.prepare_cached(
            "UPDATE messages SET snippet = ?3 WHERE account_id = ?1 AND message_id = ?2",
        )?
        .execute(params![account.as_str(), message, snippet])?;
        sync_snippet_terms(conn, account.as_str(), message, &old, snippet)?;
    }
    Ok(())
}

fn sync_snippet_terms(
    conn: &Connection,
    account: &str,
    message: &str,
    old: &str,
    new: &str,
) -> Result<()> {
    // Search indexes the snippet until the full text is stored.
    if let Some(row) = super::arrival_without_body(conn, account, message)? {
        let old = terms_for_indexed(row.indexed_text(old));
        let new = terms_for_indexed(row.indexed_text(new));
        sync_terms(conn, account, &old, &new)?;
    }
    Ok(())
}

fn read_text(
    conn: &Connection,
    sql: &str,
    args: impl rusqlite::Params,
) -> Result<Option<MessageParts>> {
    let row = conn
        .prepare_cached(sql)?
        .query_row(args, |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
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
        trusted_sender_delivery: false,
        mime_parts: Vec::new(),
    }))
}

/// Revision of one stored body. Legacy rows start at zero, and the first
/// change, including deletion and reinsertion, assigns a fresh revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartStamp(pub(super) [u8; 16]);

/// Which parts one stored row has. A row with neither records that no
/// server copy had a body to give.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PartKinds {
    pub html: bool,
    pub plain: bool,
    /// Bytes of compressed HTML as stored, for a caller deciding where to
    /// prepare it. 0 without HTML.
    pub html_bytes: u64,
    /// Bytes of compressed plain text in the same read.
    pub plain_bytes: u64,
    /// Whether there are delivery headers that can affect the renderer choice.
    pub has_delivery_headers: bool,
    /// Changes when the stored parts change: a resync that replaces them,
    /// or a backfill that fills HTML into a plain row. A cache of prepared
    /// bodies keys on it as well as on the message id.
    pub stamp: PartStamp,
}

/// The stored rows among `message_ids`, by primary key. SQLite answers the
/// HTML and plain text sizes from the record header without reading the blobs.
pub(super) fn kinds(
    conn: &Connection,
    account_id: &AccountId,
    message_ids: &[&str],
) -> Result<std::collections::HashMap<String, PartKinds>> {
    let mut stmt = conn.prepare_cached(
        "SELECT b.html IS NOT NULL, b.plain IS NOT NULL, coalesce(length(b.html), 0),
                coalesce(length(b.plain), 0),
                coalesce(s.stamp, zeroblob(16)), b.bulk_headers
         FROM message_bodies b LEFT JOIN message_body_stamps s USING (account_id, message_id)
         WHERE b.account_id = ?1 AND b.message_id = ?2",
    )?;
    let mut out = std::collections::HashMap::new();
    for id in message_ids {
        let found = stmt
            .query_row(params![account_id.as_str(), id], |row| {
                let html_bytes: i64 = row.get(2)?;
                let plain_bytes: i64 = row.get(3)?;
                let headers: String = row.get(5)?;
                Ok(PartKinds {
                    html: row.get(0)?,
                    plain: row.get(1)?,
                    html_bytes: html_bytes.max(0) as u64,
                    plain_bytes: plain_bytes.max(0) as u64,
                    stamp: PartStamp(row.get(4)?),
                    has_delivery_headers: header_records(&headers).any(|(name, _)| {
                        !name.eq_ignore_ascii_case(crate::message::VERIFIED_SENDER_HEADER)
                    }),
                })
            })
            .optional()?;
        if let Some(kinds) = found {
            out.insert((*id).to_string(), kinds);
        }
    }
    Ok(out)
}

/// CROSS JOIN keeps the queue as the outer loop. Otherwise SQLite walks every
/// message in date order to skip the sort, even with an empty queue.
const MISSING_SQL: &str = "SELECT q.message_id FROM body_backfill q
     CROSS JOIN messages m
       ON m.account_id = q.account_id AND m.message_id = q.message_id
     WHERE q.account_id = ?1
     ORDER BY m.date_unix DESC
     LIMIT ?2";

const MISSING_STRUCTURES_SQL: &str = "SELECT q.message_id FROM body_backfill q
     CROSS JOIN messages m
       ON m.account_id = q.account_id AND m.message_id = q.message_id
     WHERE q.account_id = ?1 AND q.structure_unavailable = 0
       AND NOT EXISTS (
           SELECT 1 FROM message_bodies b WHERE b.account_id = q.account_id
            AND b.message_id = q.message_id AND b.parts_listed != 0
       )
     ORDER BY m.date_unix DESC
     LIMIT ?2";

/// Up to `limit` queued messages, newest first, with their server copies.
/// A queued message with no copy left is dropped from the queue.
pub(super) fn missing(
    conn: &Connection,
    account_id: &AccountId,
    limit: usize,
    structure_only: bool,
) -> Result<Vec<BodyRequest>> {
    let sql = if structure_only {
        MISSING_STRUCTURES_SQL
    } else {
        MISSING_SQL
    };
    let mut stmt = conn.prepare(sql)?;
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

fn header_records(raw: &str) -> impl Iterator<Item = (&str, &str)> {
    raw.split('\u{1e}')
        .filter_map(|record| record.split_once('\u{1f}'))
}

fn decode_headers(raw: &str) -> Vec<(String, String)> {
    header_records(raw)
        .map(|(name, value)| (name.to_string(), value.to_string()))
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
    fn thread_reader_states_preserve_pending_and_provisional_states() {
        let root = newsletter("stored@x", "INBOX", 1, "<p>Sale</p>");
        let thread = root.thread_id.clone();
        let parent = root.headers.message_id.clone();
        let mut messages = vec![root];
        for (uid, id) in (2..).zip(["unknown@x", "listed@x", "gone@x", "withheld@x"]) {
            let mut reply = newsletter(id, "INBOX", uid, "<p>Sale</p>");
            reply.headers.in_reply_to = Some(parent.clone());
            reply.headers.references = vec![parent.clone()];
            messages.push(reply);
        }
        messages.push(newsletter("other@x", "INBOX", 6, "<p>Sale</p>"));
        let store = store_with(messages);
        store
            .conn
            .execute_batch(
                "UPDATE messages SET body_text = NULL, snippet = ''
                 WHERE message_id NOT IN ('stored@x', 'other@x');
                 DELETE FROM message_bodies WHERE message_id = 'unknown@x';
                 UPDATE message_bodies SET parts_listed = 3, html = NULL, plain = NULL
                 WHERE message_id IN ('listed@x', 'gone@x');
                 UPDATE message_bodies SET structure_copy = 'imap:7:3:INBOX'
                 WHERE message_id = 'listed@x';
                 UPDATE message_bodies SET parts_listed = 0, html = NULL, plain = NULL, withheld = 1
                 WHERE message_id = 'withheld@x';
                 DELETE FROM message_copies WHERE message_id = 'gone@x';",
            )
            .unwrap();
        let states = store.thread_reader_states(&account().id, &thread).unwrap();
        assert_eq!(states.len(), 5);
        assert_eq!(states["stored@x"], (BodyState::Stored, ReaderParts::Ready));
        assert_eq!(
            states["unknown@x"],
            (BodyState::Pending, ReaderParts::Unknown)
        );
        assert_eq!(
            states["listed@x"],
            (BodyState::Pending, ReaderParts::Listed { html: true })
        );
        assert_eq!(
            states["gone@x"],
            (BodyState::Stored, ReaderParts::Unavailable { html: true })
        );
        assert_eq!(
            states["withheld@x"],
            (BodyState::TooLarge, ReaderParts::Unknown)
        );
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
    fn part_kinds_name_what_each_row_has_without_reading_it() {
        let html = newsletter("h@x", "INBOX", 1, "<p>x</p>");
        let mut plain = newsletter("p@x", "INBOX", 2, "<p>y</p>");
        plain.parts.as_mut().unwrap().html = None;
        let mut empty = newsletter("e@x", "INBOX", 3, "<p>z</p>");
        empty.parts = Some(MessageParts::default());
        let mut bare = newsletter("b@x", "INBOX", 4, "<p>w</p>");
        bare.parts = None;
        let store = store_with(vec![html, plain, empty, bare]);
        let found = store
            .stored_part_kinds(&account().id, &["h@x", "p@x", "e@x", "b@x", "gone@x"])
            .unwrap();
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found["h@x"].html && found["h@x"].plain);
        assert!(found["h@x"].html_bytes > 0);
        assert!(!found["p@x"].html && found["p@x"].plain);
        assert_eq!(found["p@x"].html_bytes, 0);
        assert!(!found["e@x"].html && !found["e@x"].plain);
    }

    #[test]
    fn the_part_stamp_changes_when_the_parts_do() {
        let mut first = newsletter("n@x", "INBOX", 1, "<p>x</p>");
        let parts = first.parts.as_mut().unwrap();
        parts.html = Some("<p>before A after</p>".into());
        parts.plain = Some("before A after".into());
        parts.bulk_headers = vec![("List-Unsubscribe".into(), "<https://x.test/A>".into())];
        let store = store_with(vec![first.clone()]);
        let account = account();
        let stamp = || store.stored_part_kinds(&account.id, &["n@x"]).unwrap()["n@x"].stamp;
        let sizes = || -> [i64; 3] {
            let sql =
                "SELECT length(html), length(plain), length(bulk_headers) FROM message_bodies";
            store
                .conn
                .query_row(sql, [], |row| Ok([row.get(0)?, row.get(1)?, row.get(2)?]))
                .unwrap()
        };
        let mut before = stamp();
        let original_sizes = sizes();
        // The same parts again: the same stamp.
        store.upsert_message(&first).unwrap();
        assert_eq!(stamp(), before);
        let mut changed = first;
        // Each resync replaces one part with content of the same stored size.
        for edit in [
            |parts: &mut MessageParts| parts.html = Some("<p>before B after</p>".into()),
            |parts: &mut MessageParts| parts.plain = Some("before B after".into()),
            |parts: &mut MessageParts| parts.bulk_headers[0].1 = "<https://x.test/B>".into(),
        ] {
            edit(changed.parts.as_mut().unwrap());
            store.upsert_message(&changed).unwrap();
            assert_eq!(sizes(), original_sizes);
            assert_ne!(stamp(), before);
            assert!(
                store
                    .message_body_at_stamp(&account.id, "n@x", before)
                    .unwrap()
                    .is_none()
            );
            before = stamp();
            assert!(
                store
                    .message_body_at_stamp(&account.id, "n@x", before)
                    .unwrap()
                    .is_some()
            );
        }
    }

    #[test]
    fn filling_a_gap_changes_the_stamp() {
        // A backfill filling HTML into a row that had only plain text.
        let mut plain = newsletter("p@x", "INBOX", 2, "<p>y</p>");
        plain.parts.as_mut().unwrap().html = None;
        let store = store_with(vec![plain.clone()]);
        let plain_only = store.stored_part_kinds(&account().id, &["p@x"]).unwrap()["p@x"];
        store
            .store_fetched_parts(
                &account().id,
                &[(
                    MessageId::new("p@x"),
                    Some(MessageParts {
                        html: Some("<p>now with html</p>".into()),
                        plain: Some("now".into()),
                        trusted_sender_delivery: false,
                        bulk_headers: Vec::new(),
                        mime_parts: Vec::new(),
                    }),
                )],
            )
            .unwrap();
        let filled = store.stored_part_kinds(&account().id, &["p@x"]).unwrap()["p@x"];
        assert!(filled.html);
        assert_ne!(filled.stamp, plain_only.stamp);
    }

    #[test]
    fn legacy_bodies_keep_their_parts_and_reinsertions_get_a_new_stamp() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let path = dir.join("mail.db");
        let account = account();
        let message = newsletter("n@x", "INBOX", 1, "<p>x</p>");
        {
            let store = Store::open(&path).unwrap();
            store.upsert_account(&account).unwrap();
            store.upsert_message(&message).unwrap();
            store
                .conn
                .execute_batch(
                    "DROP TRIGGER message_body_stamps_ai;
                 DROP TRIGGER message_body_stamps_au;
                 DROP TRIGGER message_body_stamps_ad;
                 DROP TABLE message_body_stamps;",
                )
                .unwrap();
        }
        let read_stamp =
            |store: &Store| store.stored_part_kinds(&account.id, &["n@x"]).unwrap()["n@x"].stamp;
        let reinserted = {
            let store = Store::open(&path).unwrap();
            let legacy = read_stamp(&store);
            assert_eq!(legacy, PartStamp::default());
            assert_eq!(body(&store, "n@x"), message.parts);
            assert!(
                store
                    .message_body_at_stamp(&account.id, "n@x", legacy)
                    .unwrap()
                    .is_some()
            );
            store
                .delete_message(&account.id, &MessageId::new("n@x"))
                .unwrap();
            store.upsert_message(&message).unwrap();
            let reinserted = read_stamp(&store);
            assert_ne!(reinserted, legacy);
            store
                .delete_message(&account.id, &MessageId::new("n@x"))
                .unwrap();
            store.upsert_message(&message).unwrap();
            assert_ne!(read_stamp(&store), reinserted);
            read_stamp(&store)
        };
        assert_eq!(read_stamp(&Store::open(&path).unwrap()), reinserted);
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
                 INSERT INTO body_backfill (account_id, message_id)
                 SELECT account_id, message_id FROM messages;",
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
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
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
    }

    #[test]
    fn a_row_merge_carries_the_size_limit_marker_until_a_body_is_available() {
        let headers = |id, uid| {
            let mut message = newsletter(id, "INBOX", uid, "");
            message.body_text = None;
            message.snippet.clear();
            message.parts = None;
            message
        };
        let mut full = newsletter("full@x", "INBOX", 3, "<p>Full body</p>");
        // Model the body store separately from the message's text fallback.
        full.body_text = None;
        full.snippet.clear();
        let store = store_with(vec![headers("old@x", 1), headers("kept@x", 2), full]);
        let account = account().id;
        let state = |id| reader_states(&store.conn, &account, id).unwrap().0;
        assert_eq!(state("kept@x"), crate::BodyState::Pending);
        mark_withheld(&store.conn, &account, "old@x").unwrap();

        move_into(&store.conn, &account, "old@x", "kept@x").unwrap();
        assert_eq!(state("kept@x"), crate::BodyState::TooLarge);

        move_into(&store.conn, &account, "old@x", "full@x").unwrap();
        assert_eq!(state("full@x"), crate::BodyState::Stored);
        move_into(&store.conn, &account, "full@x", "kept@x").unwrap();
        assert_eq!(state("kept@x"), crate::BodyState::Stored);
        assert_eq!(body(&store, "kept@x"), body(&store, "full@x"));
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
            trusted_sender_delivery: false,
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
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
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
                    "DROP TRIGGER part_cache_message_removed;
                     DROP TRIGGER message_parts_ad;
                     DROP TABLE message_parts;
                     DROP TABLE part_cache_sections;
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
