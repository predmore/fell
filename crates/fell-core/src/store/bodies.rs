//! The HTML and plain-text parts of received mail, kept apart from `messages`.
//!
//! A row here holds what the conversation view renders: the decoded body
//! parts as sent (HTML is sanitized when it is rendered, not here) and the
//! headers the renderer's bulk-mail check reads. Parts are compressed. The
//! `messages` row keeps `body_text`, which search and snippets use, so a
//! folder list and the FTS index never read this table.
//!
//! Rows are keyed like `messages`, by account and canonical Message-ID, and
//! go when the message row goes. Attachment parts belong in a sibling
//! `message_parts` table keyed the same way, with a part id.
//!
//! Mail stored before this table existed is queued in `body_backfill` once,
//! when the table is created; sync fetches those messages again from a
//! server copy ([`crate::sync::fetch_missing_bodies`]) and any write of
//! parts takes a message off the queue. New mail never joins the queue, so a
//! mailbox that is caught up pays nothing per pass.

use rusqlite::{Connection, OptionalExtension, params};

use crate::account::AccountId;
use crate::message::{MessageId, MessageParts, RemoteId};
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
    PRIMARY KEY (account_id, message_id)
);

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
";

/// Create the tables. The first time, queue every stored message that has a
/// server copy: its parts were never kept.
pub(super) fn ensure_bodies(conn: &Connection) -> Result<()> {
    if table_exists(conn, "message_bodies")? {
        return conn.execute_batch(BODIES_SCHEMA).map_err(Into::into);
    }
    // One transaction, so a second opener or a crash cannot leave the table
    // without its queue.
    let tx = super::begin_immediate(conn)?;
    let existed = table_exists(&tx, "message_bodies")?;
    tx.execute_batch(BODIES_SCHEMA)?;
    if !existed {
        tx.execute(
            "INSERT OR IGNORE INTO body_backfill (account_id, message_id)
             SELECT m.account_id, m.message_id FROM messages m
             WHERE EXISTS (
                 SELECT 1 FROM message_copies c
                 WHERE c.account_id = m.account_id AND c.message_id = m.message_id
             )",
            [],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let found: Option<i64> = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![name],
            |row| row.get(0),
        )
        .optional()?;
    Ok(found.is_some())
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
    /// A collapsed copy: parts are written only where none with a body are.
    FillGap,
}

/// Store `parts` for `message_id` and take it off the backfill queue. An
/// empty `parts` (no HTML, no plain) still writes a row when there is none,
/// recording that it was looked at, but never replaces parts with a body.
pub(super) fn write(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
    parts: &MessageParts,
    mode: Write,
) -> Result<()> {
    conn.execute(
        "DELETE FROM body_backfill WHERE account_id = ?1 AND message_id = ?2",
        params![account_id.as_str(), message_id],
    )?;
    let incoming_has_body = has_body(parts);
    let stored_has_body: Option<bool> = conn
        .query_row(
            "SELECT html IS NOT NULL OR plain IS NOT NULL FROM message_bodies
             WHERE account_id = ?1 AND message_id = ?2",
            params![account_id.as_str(), message_id],
            |row| row.get(0),
        )
        .optional()?;
    let write = match stored_has_body {
        None => true,
        Some(stored) => incoming_has_body && (mode == Write::Replace || !stored),
    };
    if !write {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO message_bodies (account_id, message_id, codec, html, plain, bulk_headers)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
            codec = excluded.codec,
            html = excluded.html,
            plain = excluded.plain,
            bulk_headers = excluded.bulk_headers",
        params![
            account_id.as_str(),
            message_id,
            CODEC_DEFLATE,
            parts.html.as_deref().map(compress),
            parts.plain.as_deref().map(compress),
            encode_headers(&parts.bulk_headers),
        ],
    )?;
    Ok(())
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

fn has_body(parts: &MessageParts) -> bool {
    parts.html.is_some() || parts.plain.is_some()
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
                     DROP TABLE body_backfill;",
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
