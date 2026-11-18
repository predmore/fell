//! Compose drafts and their attachment bytes live in the mailbox store.
//!
//! The runtime folder can disappear. A save copies the resolved files into
//! SQLite so a restart loses nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};

use crate::store::Store;
use crate::{AccountId, Result};

use super::frontmatter::{AttachmentRef, DraftFields, write_draft};
use super::mime::ResolvedFile;
use super::session::SendLock;
use super::workspace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCompose {
    pub id: String,
    pub account_id: AccountId,
    pub fields: DraftFields,
    pub body: String,
    pub files: Vec<ResolvedFile>,
    pub identities: Vec<String>,
    pub owner: String,
}

/// One compose draft as the inbox's drafts list shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSummary {
    pub id: String,
    pub from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    /// The person's own Bcc, shown only to them in the preview.
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// The agent the draft belongs to; empty for the person's own.
    pub owner: String,
    pub updated_unix: i64,
    /// A send that may have gone out. Sent drafts are not listed.
    pub maybe_sent: bool,
}

/// Agent hunks waiting for review, and the serialized draft their byte
/// ranges point into. Kept in the mailbox, beside the compose row it is
/// saved with, because the draft folder and its neighbours are writable by
/// the agents being reviewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StoredReview {
    pub text: String,
    pub hunks: String,
    pub next_hunk: u64,
}

const REVIEW_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS compose_review (
    draft_id TEXT PRIMARY KEY,
    text TEXT NOT NULL,
    hunks TEXT NOT NULL,
    next_hunk INTEGER NOT NULL DEFAULT 1,
    FOREIGN KEY(draft_id) REFERENCES compose_drafts(id) ON DELETE CASCADE
);
";

/// `compose_send.state` values. The draft folder is writable by agents, so
/// the record of a send lives here: deleting files there cannot unlock a
/// draft that went out.
const SEND_MAYBE_SENT: &str = "maybe-sent";
const SEND_SENT: &str = "sent";
const SEND_CLEAR: &str = "clear";

const UPSERT_COMPOSE: &str = "INSERT INTO compose_drafts (
        id, account_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
        subject, body, in_reply_to, identities, owner, updated_unix
     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
     ON CONFLICT(id) DO UPDATE SET
        from_addr = excluded.from_addr,
        to_addrs = excluded.to_addrs,
        cc_addrs = excluded.cc_addrs,
        bcc_addrs = excluded.bcc_addrs,
        subject = excluded.subject,
        body = excluded.body,
        in_reply_to = excluded.in_reply_to,
        identities = excluded.identities,
        owner = CASE WHEN excluded.owner = '' THEN compose_drafts.owner ELSE excluded.owner END,
        updated_unix = excluded.updated_unix";

impl Store {
    pub fn save_compose(&self, draft: &StoredCompose) -> Result<()> {
        self.save_compose_parts(draft, Some(&draft.files), None)
    }

    /// Update the draft text and leave stored attachment bytes in place.
    pub fn save_compose_text(&self, draft: &StoredCompose) -> Result<()> {
        self.save_compose_parts(draft, None, None)
    }

    /// One transaction: the compose row, the attachment bytes when `files`
    /// is given, and the review state when `review` is given.
    pub(crate) fn save_compose_parts(
        &self,
        draft: &StoredCompose,
        files: Option<&[ResolvedFile]>,
        review: Option<&StoredReview>,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            UPSERT_COMPOSE,
            params![
                draft.id,
                draft.account_id.as_str(),
                draft.fields.from,
                join_list(&draft.fields.to),
                join_list(&draft.fields.cc),
                join_list(&draft.fields.bcc),
                draft.fields.subject,
                draft.body,
                draft.fields.in_reply_to,
                join_list(&draft.identities),
                draft.owner,
                now_unix(),
            ],
        )?;
        if let Some(files) = files {
            tx.execute(
                "DELETE FROM compose_attachments WHERE draft_id = ?1",
                params![draft.id],
            )?;
            for file in files {
                tx.execute(
                    "INSERT INTO compose_attachments (draft_id, path, name, bytes)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![draft.id, file.path, file.display_name, file.bytes],
                )?;
            }
        }
        if let Some(review) = review {
            tx.execute_batch(REVIEW_SCHEMA)?;
            tx.execute(
                "INSERT INTO compose_review (draft_id, text, hunks, next_hunk)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(draft_id) DO UPDATE SET
                    text = excluded.text,
                    hunks = excluded.hunks,
                    next_hunk = excluded.next_hunk",
                params![
                    draft.id,
                    review.text,
                    review.hunks,
                    i64::try_from(review.next_hunk).unwrap_or(i64::MAX),
                ],
            )?;
        }
        tx.commit()?;
        self.seal_private_files()?;
        Ok(())
    }

    /// The review state saved with this draft, if Fell has saved one.
    pub(crate) fn load_compose_review(&self, id: &str) -> Result<Option<StoredReview>> {
        let exists = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'compose_review'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !exists {
            return Ok(None);
        }
        Ok(self
            .conn
            .query_row(
                "SELECT text, hunks, next_hunk FROM compose_review WHERE draft_id = ?1",
                params![id],
                |row| {
                    Ok(StoredReview {
                        text: row.get(0)?,
                        hunks: row.get(1)?,
                        next_hunk: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(1),
                    })
                },
            )
            .optional()?)
    }

    /// Claim `draft_id` for one send, before anything is transmitted. The
    /// record reads "may have been sent" until [`Self::finish_compose_send`]
    /// says otherwise, so a crash mid-send leaves the draft locked.
    ///
    /// One statement, so two claims cannot both win. A draft that went, or
    /// may have gone, out is not claimed again: its lock is returned instead.
    pub(crate) fn claim_compose_send(
        &self,
        account_id: &AccountId,
        draft_id: &str,
        message_id: Option<&str>,
    ) -> Result<Option<SendLock>> {
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        let claimed = self.conn.execute(
            "INSERT INTO compose_send (account_id, draft_id, state, message_id, updated_unix)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(account_id, draft_id) DO UPDATE SET
                state = excluded.state,
                message_id = excluded.message_id,
                updated_unix = excluded.updated_unix
             WHERE compose_send.state = ?6",
            params![
                account_id.as_str(),
                draft_id,
                SEND_MAYBE_SENT,
                message_id,
                now_unix(),
                SEND_CLEAR,
            ],
        )?;
        if claimed == 1 {
            return Ok(None);
        }
        Ok(Some(
            self.load_compose_send(account_id, draft_id)?
                .unwrap_or(SendLock::MaybeSent),
        ))
    }

    /// Record how a send ended: `None` when nothing was transmitted and the
    /// draft may be sent again. Only a "may have been sent" claim can be
    /// cleared; a recorded `sent` is never undone by a late or stray clear.
    pub(crate) fn finish_compose_send(
        &self,
        account_id: &AccountId,
        draft_id: &str,
        lock: Option<SendLock>,
    ) -> Result<()> {
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        match lock {
            None => {
                self.conn.execute(
                    "UPDATE compose_send SET state = ?3, updated_unix = ?4
                     WHERE account_id = ?1 AND draft_id = ?2 AND state = ?5",
                    params![
                        account_id.as_str(),
                        draft_id,
                        SEND_CLEAR,
                        now_unix(),
                        SEND_MAYBE_SENT
                    ],
                )?;
            }
            Some(lock) => {
                let state = match lock {
                    SendLock::Sent => SEND_SENT,
                    SendLock::MaybeSent | SendLock::InFlight => SEND_MAYBE_SENT,
                };
                // `sent` only moves up; "may have been sent" never replaces it.
                self.conn.execute(
                    "INSERT INTO compose_send (account_id, draft_id, state, updated_unix)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(account_id, draft_id) DO UPDATE SET
                        state = excluded.state,
                        updated_unix = excluded.updated_unix
                     WHERE compose_send.state != ?5",
                    params![account_id.as_str(), draft_id, state, now_unix(), SEND_SENT],
                )?;
            }
        }
        Ok(())
    }

    /// First record of a draft's send state, from an old `send-state` file.
    /// A record that is already there wins, whoever wrote it, and is returned:
    /// two opens racing to migrate cannot replace `sent` with `clear`.
    pub(crate) fn migrate_compose_send(
        &self,
        account_id: &AccountId,
        draft_id: &str,
        legacy: Option<SendLock>,
    ) -> Result<Option<SendLock>> {
        let state = match legacy {
            None => SEND_CLEAR,
            Some(SendLock::Sent) => SEND_SENT,
            Some(SendLock::MaybeSent | SendLock::InFlight) => SEND_MAYBE_SENT,
        };
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        self.conn.execute(
            "INSERT INTO compose_send (account_id, draft_id, state, updated_unix)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(account_id, draft_id) DO NOTHING",
            params![account_id.as_str(), draft_id, state, now_unix()],
        )?;
        Ok(self
            .compose_send_record(account_id, draft_id)?
            .unwrap_or(Some(SendLock::MaybeSent)))
    }

    /// The send record of a draft: `Ok(None)` when it has none, and
    /// `Ok(Some(None))` when its last send transmitted nothing.
    pub(crate) fn compose_send_record(
        &self,
        account_id: &AccountId,
        draft_id: &str,
    ) -> Result<Option<Option<SendLock>>> {
        let state = self
            .conn
            .query_row(
                "SELECT state FROM compose_send WHERE account_id = ?1 AND draft_id = ?2",
                params![account_id.as_str(), draft_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(state.map(|state| match state.as_str() {
            SEND_CLEAR => None,
            SEND_SENT => Some(SendLock::Sent),
            // Anything else, damaged or not, errs toward "may have been sent".
            _ => Some(SendLock::MaybeSent),
        }))
    }

    /// The lock the mailbox holds on a draft, if any.
    pub(crate) fn load_compose_send(
        &self,
        account_id: &AccountId,
        draft_id: &str,
    ) -> Result<Option<SendLock>> {
        Ok(self.compose_send_record(account_id, draft_id)?.flatten())
    }

    pub fn load_compose(&self, id: &str) -> Result<Option<StoredCompose>> {
        let row = self
            .conn
            .query_row(
                "SELECT account_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
                        subject, body, in_reply_to, identities, owner
                 FROM compose_drafts WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((account, from, to, cc, bcc, subject, body, reply, identities, owner)) = row
        else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT path, name, bytes FROM compose_attachments WHERE draft_id = ?1 ORDER BY path",
        )?;
        let files = stmt
            .query_map(params![id], |row| {
                Ok(ResolvedFile {
                    path: row.get(0)?,
                    display_name: row.get(1)?,
                    bytes: row.get(2)?,
                    inline: false,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some(StoredCompose {
            id: id.to_string(),
            account_id: AccountId(account),
            fields: DraftFields {
                from,
                to: split_list(&to),
                cc: split_list(&cc),
                bcc: split_list(&bcc),
                subject,
                in_reply_to: reply.filter(|value| !value.is_empty()),
                attachments: files
                    .iter()
                    .map(|file| AttachmentRef {
                        path: file.path.clone(),
                        name: Some(file.display_name.clone()),
                        line: 0,
                    })
                    .collect(),
            },
            body,
            files,
            identities: split_list(&identities),
            owner,
        }))
    }

    /// The account's compose drafts that have not gone out, newest first.
    ///
    /// A draft whose send went out leaves the list; one that may have gone
    /// out stays, marked, so the person checks Sent. An agent's draft is
    /// listed while its review draft is (an undone one is not).
    pub fn unsent_compose_drafts(&self, account_id: &AccountId) -> Result<Vec<DraftSummary>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.id, d.from_addr, d.to_addrs, d.cc_addrs, d.subject, d.body, d.owner,
                    d.updated_unix, s.state, d.bcc_addrs
             FROM compose_drafts d
             LEFT JOIN compose_send s ON s.account_id = d.account_id AND s.draft_id = d.id
             WHERE d.account_id = ?1
               AND IFNULL(s.state, ?2) != ?3
               AND (d.owner = '' OR EXISTS (
                   SELECT 1 FROM review_drafts r
                   WHERE r.account_id = d.account_id AND 'r' || r.id = d.id
               ))
             ORDER BY d.updated_unix DESC, d.id DESC",
        )?;
        let rows = stmt.query_map(params![account_id.as_str(), SEND_CLEAR, SEND_SENT], |row| {
            let state: Option<String> = row.get(8)?;
            Ok(DraftSummary {
                id: row.get(0)?,
                from: row.get(1)?,
                to: split_list(&row.get::<_, String>(2)?),
                cc: split_list(&row.get::<_, String>(3)?),
                bcc: split_list(&row.get::<_, String>(9)?),
                subject: row.get(4)?,
                body: row.get(5)?,
                owner: row.get(6)?,
                updated_unix: row.get(7)?,
                // Anything but a clean "clear" errs toward "may have been sent".
                maybe_sent: state.is_some_and(|state| state != SEND_CLEAR),
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn compose_owner(&self, id: &str) -> Result<Option<String>> {
        Ok(self.load_compose(id)?.map(|draft| draft.owner))
    }

    /// Addresses this account has already mailed, plus its own address.
    /// Sender, subject, and date of a stored message, by id or an id that
    /// collapsed into it. For the compose window's "In reply to" row.
    pub fn message_summary(
        &self,
        account_id: &AccountId,
        message_id: &crate::MessageId,
    ) -> Result<Option<super::ReplyContext>> {
        self.conn
            .query_row(
                "SELECT m.from_name, m.from_email, m.subject, m.date_unix FROM messages m
                 WHERE m.account_id = ?1 AND (
                     m.message_id = ?2 OR m.message_id IN (
                         SELECT canonical_message_id FROM message_aliases
                         WHERE account_id = ?1 AND message_id = ?2
                     )
                 )
                 LIMIT 1",
                params![account_id.as_str(), message_id.as_str()],
                |row| {
                    let name: Option<String> = row.get(0)?;
                    let email: String = row.get(1)?;
                    Ok(super::ReplyContext {
                        sender: name.filter(|name| !name.trim().is_empty()).unwrap_or(email),
                        subject: row.get(2)?,
                        date_unix: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn mailed_addresses(&self, account_id: &AccountId) -> Result<HashSet<String>> {
        let mut known = HashSet::new();
        if let Some(account) = self.get_account(account_id)? {
            known.insert(account.email.to_ascii_lowercase());
        }
        let mut stmt = self.conn.prepare(
            "SELECT folders, to_line, cc_line, bcc_line FROM messages WHERE account_id = ?1",
        )?;
        let rows = stmt.query_map(params![account_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (folders, to, cc, bcc) = row?;
            if !split_list(&folders).iter().any(|folder| {
                folder.eq_ignore_ascii_case("sent") || folder.eq_ignore_ascii_case("sent mail")
            }) {
                continue;
            }
            for line in [&to, &cc, &bcc] {
                for address in decode_address_line(line) {
                    known.insert(address);
                }
            }
        }
        // Bcc of mail sent from Fell, which the Sent copy does not carry.
        for address in crate::store::sent_bcc::recorded_bcc_addresses(&self.conn, account_id)? {
            known.insert(address.email.to_ascii_lowercase());
        }
        Ok(known)
    }

    pub fn restore_compose_folder(&self, id: &str, folder: &Path) -> Result<Option<StoredCompose>> {
        let Some(stored) = self.load_compose(id)? else {
            return Ok(None);
        };
        write_compose_folder(&stored, id, folder)?;
        Ok(Some(stored))
    }

    /// Ready draft `id`'s folder for opening. The mailbox row is Fell's own
    /// copy, so the folder is written from it, except when `draft.md` holds
    /// the text Fell saved last: a save whose mailbox row did not land is
    /// newer than the row, and is kept. What it has over the row is then
    /// marked for review when the draft opens, as for any outside change.
    /// `None` when the mailbox has no such draft.
    pub fn refresh_compose_folder(&self, id: &str, folder: &Path) -> Result<Option<StoredCompose>> {
        let Some(stored) = self.load_compose(id)? else {
            return Ok(None);
        };
        let own = workspace::read_draft_file(folder)
            .is_ok_and(|text| workspace::is_own_save(folder, &text));
        if own {
            // The folder's files belong with the text kept: only ones it has
            // lost come back from the row, never older bytes over newer.
            for file in &stored.files {
                let present =
                    workspace::resolve_inside(folder, &file.path).is_ok_and(|path| path.exists());
                if !present {
                    workspace::write_inside(folder, &file.path, &file.bytes)
                        .map_err(crate::Error::Invalid)?;
                }
            }
            return Ok(Some(stored));
        }
        write_compose_folder(&stored, id, folder)?;
        // The folder is the row again; an older note no longer applies.
        let _ = workspace::note_own_save(folder, &write_draft(&stored.fields, &stored.body));
        Ok(Some(stored))
    }

    /// Forget draft `id`: its row, attachments, review and send record.
    pub(crate) fn delete_compose(&self, id: &str) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute_batch(REVIEW_SCHEMA)?;
        tx.execute(
            "DELETE FROM compose_review WHERE draft_id = ?1",
            params![id],
        )?;
        tx.execute(
            "DELETE FROM compose_attachments WHERE draft_id = ?1",
            params![id],
        )?;
        tx.execute("DELETE FROM compose_send WHERE draft_id = ?1", params![id])?;
        tx.execute("DELETE FROM compose_drafts WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }
}

/// Write `stored` out as draft `id`'s folder, attachments included.
fn write_compose_folder(stored: &StoredCompose, id: &str, folder: &Path) -> Result<()> {
    {
        workspace::materialize_in(
            folder.parent().unwrap_or(folder),
            id,
            &stored.fields,
            &stored.body,
            &stored.account_id,
        )
        .map_err(crate::Error::Invalid)?;
        for file in &stored.files {
            workspace::write_inside(folder, &file.path, &file.bytes)
                .map_err(crate::Error::Invalid)?;
        }
        Ok(())
    }
}

fn join_list(items: &[String]) -> String {
    items.join("\u{1e}")
}

fn split_list(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        Vec::new()
    } else {
        raw.split('\u{1e}')
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn decode_address_line(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split('\u{1e}')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let email = part.split('\u{1f}').nth(1).unwrap_or(part);
            let email = email.trim();
            if email.contains('@') {
                Some(email.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect()
}

pub fn remember_compose_store(id: &str, store: &Path) -> std::result::Result<(), String> {
    let Some(path) = compose_index_file(id) else {
        return Err("no durable directory for the compose index".into());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    workspace::atomic_write(&path, store.display().to_string().as_bytes())
}

pub fn remembered_store_path(id: &str) -> Option<PathBuf> {
    let path = compose_index_file(id)?;
    let text = std::fs::read_to_string(path).ok()?;
    let store = PathBuf::from(text.trim());
    store.is_file().then_some(store)
}

pub fn stored_compose_account(id: &str) -> Option<String> {
    let store = open_store_for(id)?;
    store
        .load_compose(id)
        .ok()
        .flatten()
        .map(|draft| draft.account_id.as_str().to_string())
}

pub fn stored_compose_owner(id: &str) -> Option<String> {
    let store = open_store_for(id)?;
    let owner = store.compose_owner(id).ok().flatten()?;
    if owner.is_empty() { None } else { Some(owner) }
}

pub fn open_owned_draft(
    id: &str,
    agent: &str,
    account: &str,
) -> std::result::Result<PathBuf, String> {
    let folder = workspace::draft_folder(id)?;
    if draft_file_is_regular(&folder) {
        return Ok(folder);
    }
    let Some(store) = open_store_for(id) else {
        return Err(format!("draft {id} is not open"));
    };
    let Some(stored) = store.load_compose(id).map_err(|err| err.to_string())? else {
        return Err(format!("draft {id} is not open"));
    };
    if stored.account_id.as_str() != account {
        return Err("this draft belongs to another account".into());
    }
    if stored.owner != agent {
        if stored.owner.is_empty() {
            return Err(format!("draft {id} is not open"));
        }
        return Err("this draft belongs to another agent".into());
    }
    store
        .restore_compose_folder(id, &folder)
        .map_err(|err| err.to_string())?;
    Ok(folder)
}

pub fn mailbox_db_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FELL_STORE") {
        return Some(PathBuf::from(path));
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(data).join("fell/mail.db"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/fell/mail.db"))
}

pub(crate) fn compose_index_file(id: &str) -> Option<PathBuf> {
    let root = if let Some(dir) = std::env::var_os("FELL_COMPOSE_INDEX") {
        PathBuf::from(dir)
    } else if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data).join("fell/compose-stores")
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".local/share/fell/compose-stores")
    };
    Some(root.join(id))
}

pub(crate) fn open_store_for_draft(id: &str) -> Option<Store> {
    open_store_for(id)
}

fn open_store_for(id: &str) -> Option<Store> {
    let path = remembered_store_path(id).or_else(mailbox_db_path)?;
    if !path.is_file() {
        return None;
    }
    Store::open(path).ok()
}

/// Recreate the runtime folder from the mailbox when `draft.md` is gone.
pub fn ensure_draft_folder(id: &str) -> std::result::Result<PathBuf, String> {
    let folder = workspace::draft_folder(id)?;
    if draft_file_is_regular(&folder) {
        return Ok(folder);
    }
    let Some(store) = open_store_for(id) else {
        return Err(format!("draft {id} is not open"));
    };
    match store
        .restore_compose_folder(id, &folder)
        .map_err(|err| err.to_string())?
    {
        Some(_) => Ok(folder),
        None => Err(format!("draft {id} is not open")),
    }
}

fn draft_file_is_regular(folder: &Path) -> bool {
    std::fs::symlink_metadata(folder.join("draft.md"))
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_names_its_parent_by_id_or_alias() {
        let store = Store::open_in_memory().unwrap();
        let account = crate::Account {
            id: AccountId("a".into()),
            display_name: "Me".into(),
            email: "me@x".into(),
            backend_kind: crate::BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        let id = crate::MessageId::new("parent@x");
        store
            .upsert_message(&crate::Message {
                account_id: account.id.clone(),
                headers: crate::MessageHeaders {
                    message_id: id.clone(),
                    proton_conversation_id: None,
                    in_reply_to: None,
                    references: Vec::new(),
                    from: crate::Address {
                        name: Some("Jon".into()),
                        email: "jon@x".into(),
                    },
                    subject: "Plans".into(),
                    date_unix: 42,
                },
                remote_id: crate::RemoteId("r".into()),
                thread_id: crate::ThreadId::from_message_id(&id),
                snippet: "hi".into(),
                body_text: Some("hi".into()),
                unread: false,
                folders: vec!["INBOX".into()],
                labels: Vec::new(),
                arrived_via: Vec::new(),
                to: Vec::new(),
                cc: Vec::new(),
                bcc: Vec::new(),
                starred: false,
                has_attachment: false,
                archived: false,
                snooze_until: None,
                list_id: None,
            })
            .unwrap();
        let summary = store.message_summary(&account.id, &id).unwrap().unwrap();
        assert_eq!(summary.sender, "Jon");
        assert_eq!(summary.subject, "Plans");
        assert_eq!(summary.date_unix, 42);
        store
            .conn
            .execute(
                "INSERT INTO message_aliases (account_id, message_id, canonical_message_id)
                 VALUES ('a', 'echo@list', 'parent@x')",
                [],
            )
            .unwrap();
        let echo = crate::MessageId::new("echo@list");
        assert_eq!(
            store.message_summary(&account.id, &echo).unwrap(),
            Some(summary)
        );
        let missing = crate::MessageId::new("missing@x");
        assert_eq!(store.message_summary(&account.id, &missing).unwrap(), None);
    }
    use crate::{Account, BackendKind};

    #[test]
    fn the_drafts_list_drops_sent_mail_and_marks_maybe_sent() {
        let store = Store::open_in_memory().unwrap();
        let account = Account {
            id: AccountId("a".into()),
            display_name: "Me".into(),
            email: "me@x".into(),
            backend_kind: BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        let save = |id: &str, owner: &str| {
            store
                .save_compose(&StoredCompose {
                    id: id.into(),
                    account_id: account.id.clone(),
                    fields: DraftFields {
                        from: "me@x".into(),
                        to: vec!["Ada <ada@x>".into()],
                        subject: format!("about {id}"),
                        ..DraftFields::default()
                    },
                    body: "hello\n".into(),
                    files: Vec::new(),
                    identities: vec!["me@x".into()],
                    owner: owner.into(),
                })
                .unwrap();
        };
        save("open", "");
        save("went", "");
        save("unsure", "");
        save("retried", "");
        // An agent's draft whose review draft was undone.
        save("r99", "agent:bot");
        let claim = |id: &str, lock: Option<SendLock>| {
            assert_eq!(
                store.claim_compose_send(&account.id, id, None).unwrap(),
                None
            );
            if lock != Some(SendLock::MaybeSent) {
                store.finish_compose_send(&account.id, id, lock).unwrap();
            }
        };
        claim("went", Some(SendLock::Sent));
        claim("unsure", Some(SendLock::MaybeSent));
        claim("retried", None);
        let listed = store.unsent_compose_drafts(&account.id).unwrap();
        let mut ids: Vec<(&str, bool)> = listed
            .iter()
            .map(|draft| (draft.id.as_str(), draft.maybe_sent))
            .collect();
        ids.sort();
        assert_eq!(
            ids,
            vec![("open", false), ("retried", false), ("unsure", true)]
        );
        let open = listed.iter().find(|draft| draft.id == "open").unwrap();
        assert_eq!(open.to, vec!["Ada <ada@x>".to_string()]);
        assert_eq!(open.subject, "about open");
        assert!(
            store
                .unsent_compose_drafts(&AccountId("other".into()))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn attachments_survive_a_reload() {
        let store = Store::open_in_memory().unwrap();
        let account = Account {
            id: AccountId("a".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        store
            .save_compose(&StoredCompose {
                id: "d1".into(),
                account_id: account.id.clone(),
                fields: DraftFields {
                    from: "jon@example.com".into(),
                    to: vec!["ada@example.com".into()],
                    subject: "Q3".into(),
                    ..DraftFields::default()
                },
                body: "![chart](chart.png)\n".into(),
                files: vec![ResolvedFile {
                    path: "chart.png".into(),
                    display_name: "chart.png".into(),
                    bytes: b"png".to_vec(),
                    inline: true,
                }],
                identities: vec!["jon@example.com".into()],
                owner: String::new(),
            })
            .unwrap();
        let loaded = store.load_compose("d1").unwrap().unwrap();
        assert_eq!(loaded.body, "![chart](chart.png)\n");
        assert_eq!(loaded.files[0].bytes, b"png");
        assert_eq!(loaded.fields.to, vec!["ada@example.com".to_string()]);
    }

    #[test]
    fn keeping_fells_own_save_keeps_its_newer_attachments() {
        let store = Store::open_in_memory().unwrap();
        let account = Account {
            id: AccountId("a".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        let file = |path: &str, bytes: &[u8]| ResolvedFile {
            path: path.into(),
            display_name: path.into(),
            bytes: bytes.to_vec(),
            inline: true,
        };
        store
            .save_compose(&StoredCompose {
                id: "d1".into(),
                account_id: account.id.clone(),
                fields: DraftFields {
                    from: "jon@example.com".into(),
                    ..DraftFields::default()
                },
                body: "![chart](chart.png) ![logo](logo.png)\n".into(),
                files: vec![file("chart.png", b"old"), file("logo.png", b"logo")],
                identities: vec!["jon@example.com".into()],
                owner: String::new(),
            })
            .unwrap();
        let folder = std::env::temp_dir().join(format!(
            "fell-own-save-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        // Fell saved a new chart and its text; the mailbox row missed both.
        let text = "---\nfrom: jon@example.com\n---\n![chart](chart.png) ![logo](logo.png) v2\n";
        std::fs::write(folder.join("draft.md"), text).unwrap();
        workspace::note_own_save(&folder, text).unwrap();
        std::fs::write(folder.join("chart.png"), b"new").unwrap();
        store.refresh_compose_folder("d1", &folder).unwrap();
        assert_eq!(std::fs::read(folder.join("chart.png")).unwrap(), b"new");
        // A file the folder lost still comes back from the row.
        assert_eq!(std::fs::read(folder.join("logo.png")).unwrap(), b"logo");
        let _ = std::fs::remove_dir_all(&folder);
    }

    #[test]
    fn a_flushed_edit_restores_when_the_runtime_folder_is_gone() {
        let dir = std::env::temp_dir().join(format!(
            "fell-persist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let drafts = dir.join("drafts");
        let db = dir.join("mail.db");
        let store = Store::open(&db).unwrap();
        let account = Account {
            id: AccountId("a".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hello".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&drafts, "keep", &fields, "original\n", &account.id).unwrap();
        let mut session = super::super::session::DraftSession::create(
            folder.clone(),
            "keep",
            account.id.clone(),
            fields,
            "original\n",
        )
        .unwrap();
        session.bind_store(&db);
        session.set_body_from_user("edited line\n![chart](chart.png)\n".into());
        std::fs::write(folder.join("chart.png"), b"png-bytes").unwrap();
        session.flush().unwrap();
        drop(store);
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(drafts.join(".keep.private"));
        let store = Store::open(&db).unwrap();
        store
            .restore_compose_folder("keep", &drafts.join("keep"))
            .unwrap()
            .unwrap();
        let text = std::fs::read_to_string(drafts.join("keep/draft.md")).unwrap();
        assert!(text.contains("edited line"), "{text}");
        assert_eq!(
            std::fs::read(drafts.join("keep/chart.png")).unwrap(),
            b"png-bytes"
        );
    }

    #[test]
    fn another_account_or_agent_cannot_open_the_folder() {
        let dir = std::env::temp_dir().join(format!(
            "fell-owned-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            bcc: vec!["secret@example.com".into()],
            subject: "Hello".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&dir, "r1", &fields, "body\n", &AccountId("acct-a".into()))
                .unwrap();
        let control = workspace::control_dir(&folder);
        std::fs::create_dir_all(&control).unwrap();
        std::fs::write(control.join("owner"), "agent:bob\n").unwrap();
        std::fs::write(control.join("account"), "acct-b\n").unwrap();
        let db = dir.join("mail.db");
        let store = Store::open(&db).unwrap();
        store
            .upsert_account(&Account {
                id: AccountId("acct-a".into()),
                display_name: "Ada".into(),
                email: "jon@example.com".into(),
                backend_kind: BackendKind::Bridge,
            })
            .unwrap();
        store
            .save_compose(&StoredCompose {
                id: "r1".into(),
                account_id: AccountId("acct-a".into()),
                fields: fields.clone(),
                body: "body\n".into(),
                files: Vec::new(),
                identities: vec!["jon@example.com".into()],
                owner: "agent:ada".into(),
            })
            .unwrap();
        let opened = super::super::session::DraftSession::open_with_store(
            &folder,
            AccountId("acct-b".into()),
            &store,
        );
        match opened {
            Ok(_) => panic!("opened with the wrong account"),
            Err(err) => assert!(err.contains("account"), "{err}"),
        }
        let opened = super::super::session::DraftSession::open_with_store(
            &folder,
            AccountId("acct-a".into()),
            &store,
        )
        .unwrap();
        assert_eq!(opened.owner(), "agent:ada");
        assert_eq!(opened.account_id().as_str(), "acct-a");
    }

    #[test]
    fn restore_does_not_follow_an_attachment_symlink() {
        let dir = std::env::temp_dir().join(format!(
            "fell-restore-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.db");
        let store = Store::open(&db).unwrap();
        let account = Account {
            id: AccountId("a".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hello".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&dir, "keep", &fields, "body\n", &account.id).unwrap();
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, b"keep-me").unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("chart.png")).unwrap();
        store
            .save_compose(&StoredCompose {
                id: "keep".into(),
                account_id: account.id.clone(),
                fields,
                body: "body\n".into(),
                files: vec![crate::compose::ResolvedFile {
                    path: "chart.png".into(),
                    display_name: "chart.png".into(),
                    bytes: b"local".to_vec(),
                    inline: false,
                }],
                identities: vec!["jon@example.com".into()],
                owner: "agent:ada".into(),
            })
            .unwrap();
        let err = store.restore_compose_folder("keep", &folder).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("symlink"), "{text}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep-me");
    }
}
