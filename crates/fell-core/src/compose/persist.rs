//! Compose drafts and their attachment bytes live in the mailbox store.
//!
//! The runtime folder can disappear. A save copies the resolved files into
//! SQLite so a restart loses nothing.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use fs4::fs_std::FileExt;
use rusqlite::{Connection, OptionalExtension, params};

use crate::store::Store;
use crate::{AccountId, Result};

use super::frontmatter::{AttachmentRef, DraftFields, write_draft};
use super::mime::ResolvedFile;
use super::reply::ReplyParts;
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
    /// The generated delimiter and signature text, empty for a managed draft
    /// with no signature, or absent after an edit to the signature.
    pub managed_signature: Option<String>,
    pub owner: String,
    /// A reply's References and a reply or forward's quoted original.
    pub reply: ReplyParts,
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
    /// With write-back enabled, this draft's current content has no confirmed server copy.
    pub not_on_server: bool,
    /// The save for this content failed and no retry is queued.
    pub server_save_failed: bool,
    /// What a reply answers, when the mailbox still holds the original.
    pub reply_context: Option<super::ReplyContext>,
    /// Whose message a forward passes on.
    pub forwarded: Option<super::reply::ForwardSummary>,
    /// The Message-ID a reply answers, shown when the original is gone.
    pub in_reply_to: Option<String>,
    /// Names the mailbox already has for the draft's recipients, from mail
    /// they sent, by lowercase address. A name is never made up.
    pub known_names: std::collections::BTreeMap<String, String>,
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
pub(super) const SEND_IN_PROGRESS: &str = "Retry after send";

pub(crate) fn mark_discarded(
    conn: &rusqlite::Connection,
    account: &AccountId,
    id: &str,
    action_id: i64,
) -> Result<bool> {
    let changed = conn.execute(
        "UPDATE compose_drafts AS d SET discarded_action_id = ?3
         WHERE d.account_id = ?1 AND d.id = ?2 AND d.discarded_action_id IS NULL
           AND NOT EXISTS (
               SELECT 1 FROM compose_send s
               WHERE s.account_id = d.account_id AND s.draft_id = d.id AND s.state = ?4
           )
           AND (d.owner = '' OR EXISTS (
               SELECT 1 FROM review_drafts r
               WHERE r.account_id = d.account_id AND 'r' || r.id = d.id
           ))",
        params![account.as_str(), id, action_id, SEND_SENT],
    )?;
    if changed == 1 {
        crate::outbox::drafts::discard(conn, account, id, action_id)?;
    }
    Ok(changed == 1)
}

/// Decide what is visible in Drafts under the same write lock that restores
/// the batch. A Sent match arriving during the discard keeps its send lock.
pub(crate) fn undo_discard(
    store: &Store,
    conn: &rusqlite::Connection,
    account: &AccountId,
    action_id: i64,
) -> Result<(i64, i64)> {
    let (restored, confirmed) = conn.query_row(
        "SELECT COUNT(*) - COUNT(s.draft_id), COUNT(s.draft_id)
         FROM compose_drafts d
         LEFT JOIN compose_send s ON s.account_id = d.account_id
             AND s.draft_id = d.id AND s.state = ?3
         WHERE d.account_id = ?1 AND d.discarded_action_id = ?2",
        params![account.as_str(), action_id, SEND_SENT],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let drafts = {
        let mut stmt = conn.prepare(
            "SELECT d.id FROM compose_drafts d
             WHERE d.account_id = ?1 AND d.discarded_action_id = ?2
             AND NOT EXISTS (SELECT 1 FROM compose_send s
                WHERE s.account_id = d.account_id AND s.draft_id = d.id AND s.state = ?3)",
        )?;
        stmt.query_map(params![account.as_str(), action_id, SEND_SENT], |r| {
            r.get::<_, String>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    conn.execute(
        "UPDATE compose_drafts SET discarded_action_id = NULL
         WHERE account_id = ?1 AND discarded_action_id = ?2",
        params![account.as_str(), action_id],
    )?;
    for id in drafts {
        crate::outbox::drafts::undo_discard(store, conn, account, action_id, &id)?;
    }
    Ok((restored, confirmed))
}

/// Commit SMTP confirmation alongside the local Sent copy and any APPEND.
pub(crate) fn record_confirmed_send(
    conn: &rusqlite::Connection,
    account: &AccountId,
    draft_id: &str,
    message_id: &crate::MessageId,
) -> Result<()> {
    conn.execute(
        "UPDATE compose_send SET state = ?4, updated_unix = ?5
         WHERE account_id = ?1 AND draft_id = ?2 AND message_id = ?3 AND state = ?6",
        params![
            account.as_str(),
            draft_id,
            message_id.as_str(),
            SEND_SENT,
            now_unix(),
            SEND_MAYBE_SENT
        ],
    )?;
    crate::outbox::drafts::queue_delete(conn, account, draft_id)?;
    Ok(())
}

/// Confirm only the exact id observed on an IMAP copy in the account's
/// listed Sent mailbox. Stored conversation rows can merge other copies by
/// body fingerprint and can gain folders through local moves.
pub(crate) fn confirm_synced_sends(
    conn: &rusqlite::Connection,
    account_id: &AccountId,
    delta: &crate::MailboxDelta,
) -> Result<Vec<String>> {
    if delta.messages.is_empty() && delta.removals.is_empty() {
        return Ok(Vec::new());
    }
    let account = account_id.as_str();
    let pending = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM compose_send WHERE account_id = ?1
                        AND state = ?2 AND message_id IS NOT NULL)",
        params![account, SEND_MAYBE_SENT],
        |row| row.get::<_, bool>(0),
    )?;
    if !pending {
        return Ok(Vec::new());
    }
    let sent = if let Some(roles) = &delta.roles {
        roles
            .get(crate::FolderRole::Sent)
            .map(|role| role.raw.clone())
    } else {
        conn.query_row(
            "SELECT raw FROM folder_roles WHERE account_id = ?1 AND role = ?2",
            params![account, crate::FolderRole::Sent.key()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    };
    let Some(sent) = sent else {
        return Ok(Vec::new());
    };
    let mailbox = crate::imap::escape_mailbox(&sent);
    let mut confirmed: Vec<String> = Vec::new();
    let mut update = conn.prepare_cached(
        "UPDATE compose_send SET state = ?4, updated_unix = ?5
         WHERE account_id = ?1 AND state = ?2 AND message_id = ?3
         RETURNING draft_id",
    )?;
    let mut confirm = |message_id: &crate::MessageId| -> Result<()> {
        for id in update.query_map(
            params![
                account,
                SEND_MAYBE_SENT,
                message_id.as_str(),
                SEND_SENT,
                now_unix()
            ],
            |row| row.get(0),
        )? {
            confirmed.push(id?);
        }
        Ok(())
    };
    for message in &delta.messages {
        if crate::imap::parse_remote_id(&message.remote_id.0)
            .is_some_and(|copy| copy.mailbox == mailbox)
        {
            confirm(&message.headers.message_id)?;
        }
    }
    for removal in &delta.removals {
        if removal.mailbox == mailbox
            && let crate::backend::RemovalKind::Listing { found, .. } = &removal.kind
        {
            for (_, message_id) in found {
                confirm(message_id)?;
            }
        }
    }
    for id in &confirmed {
        crate::outbox::drafts::queue_delete(conn, account_id, id)?;
    }
    Ok(confirmed)
}

const UPSERT_COMPOSE: &str = "INSERT INTO compose_drafts (
        id, account_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
        subject, body, in_reply_to, identities, owner, updated_unix,
        references_header, quote_text, quote_html, reply_to_unverified, managed_signature
     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
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
        updated_unix = excluded.updated_unix,
        references_header = CASE WHEN excluded.references_header = ''
            THEN compose_drafts.references_header ELSE excluded.references_header END,
        quote_text = CASE WHEN excluded.quote_text = ''
            THEN compose_drafts.quote_text ELSE excluded.quote_text END,
        quote_html = CASE WHEN excluded.quote_html = ''
            THEN compose_drafts.quote_html ELSE excluded.quote_html END,
        reply_to_unverified = compose_drafts.reply_to_unverified OR excluded.reply_to_unverified,
        managed_signature = excluded.managed_signature
     WHERE compose_drafts.discarded_action_id IS NULL";

fn save_compose_in(
    conn: &rusqlite::Connection,
    draft: &StoredCompose,
    files: Option<&[ResolvedFile]>,
    review: Option<&StoredReview>,
) -> Result<()> {
    let written = conn.execute(
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
            join_list(&draft.reply.references),
            draft.reply.quote_text,
            draft.reply.quote_html,
            draft.reply.reply_to_unverified,
            draft.managed_signature,
        ],
    )?;
    if written == 0 {
        return Err(crate::Error::Action("Draft was discarded".into()));
    }
    if let Some(files) = files {
        conn.execute(
            "DELETE FROM compose_attachments WHERE draft_id = ?1",
            params![draft.id],
        )?;
        for file in files {
            conn.execute(
                "INSERT INTO compose_attachments (draft_id, path, name, bytes)
                     VALUES (?1, ?2, ?3, ?4)",
                params![draft.id, file.path, file.display_name, file.bytes],
            )?;
        }
    }
    if let Some(review) = review {
        conn.execute_batch(REVIEW_SCHEMA)?;
        conn.execute(
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
    Ok(())
}

impl Store {
    /// Which exact server copy a row in Drafts opens for editing.
    pub(crate) fn server_draft_source(
        &self,
        account: &AccountId,
        source: &crate::RemoteId,
    ) -> Result<Option<crate::outbox::drafts::Copy>> {
        let roles = self.folder_roles(account)?;
        let Some((folder, remote)) = roles
            .get(crate::FolderRole::Drafts)
            .zip(crate::imap::parse_remote_id(&source.0))
            .filter(|(folder, remote)| crate::imap::escape_mailbox(&folder.raw) == remote.mailbox)
        else {
            return Ok(None);
        };
        let withheld: Option<bool> = self
            .conn
            .query_row(
                "SELECT COALESCE(b.withheld, 0) FROM message_copies c
             LEFT JOIN message_bodies b USING (account_id, message_id)
             WHERE c.account_id = ?1 AND c.mailbox = ?2 AND c.uidvalidity = ?3 AND c.uid = ?4",
                params![
                    account.as_str(),
                    remote.mailbox,
                    remote.uidvalidity,
                    remote.uid
                ],
                |row| row.get(0),
            )
            .optional()?;
        if withheld == Some(true) {
            return Err(crate::Error::DraftMessageLimit);
        }
        Ok(withheld.map(|_| crate::outbox::drafts::Copy {
            mailbox: folder.raw.clone(),
            validity: remote.uidvalidity,
            uid: remote.uid,
        }))
    }

    pub(crate) fn import_server_draft(
        &self,
        copy: &crate::outbox::drafts::Copy,
        draft: &StoredCompose,
        message_id: Option<&str>,
    ) -> Result<String> {
        workspace::validate_id(&draft.id).map_err(crate::Error::Invalid)?;
        let tx = crate::store::begin_immediate(&self.conn)?;
        let account = &draft.account_id;
        if let Some(id) = crate::outbox::drafts::local_id(&tx, account, copy)? {
            return Ok(id);
        }
        save_compose_in(&tx, draft, Some(&draft.files), None)?;
        crate::outbox::drafts::import(
            &tx,
            account,
            &draft.id,
            copy,
            &super::server::digest(draft, &draft.files),
            message_id,
        )?;
        self.hide_server_draft_copy(&tx, account, copy)?;
        tx.commit()?;
        self.seal_private_files()?;
        Ok(draft.id.clone())
    }

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
        let tx = crate::store::begin_immediate(&self.conn)?;
        save_compose_in(&tx, draft, files, review)?;
        tx.commit()?;
        self.seal_private_files()?;
        Ok(())
    }

    /// Save a normal reply draft and consume the exact suggestion in one write.
    pub fn save_suggested_reply(
        &self,
        draft: &StoredCompose,
        thread: &crate::ThreadId,
        note: i64,
        position: i64,
    ) -> Result<i64> {
        let tx = crate::store::begin_immediate(&self.conn)?;
        let action = crate::notes::accept_reply(&tx, &draft.account_id, thread, note, position)?;
        save_compose_in(&tx, draft, Some(&draft.files), None)?;
        self.seal_private_files()?;
        tx.commit()?;
        Ok(action)
    }

    /// Remove a reply that failed to open and restore its exact suggestion.
    pub fn rollback_suggested_reply(
        &self,
        account: &AccountId,
        draft: &str,
        action: i64,
    ) -> Result<()> {
        let tx = crate::store::begin_immediate(&self.conn)?;
        let changed = tx.execute(
            "UPDATE local_actions SET undone = 1
             WHERE id = ?1 AND account_id = ?2 AND kind = 'accept_suggestion' AND undone = 0",
            params![action, account.as_str()],
        )?;
        if changed > 0 {
            crate::notes::undo_local(&tx, account, action)?;
        }
        let belongs_to_account = tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM compose_drafts WHERE id = ?1 AND account_id = ?2)",
            params![draft, account.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if belongs_to_account {
            delete_compose_in(&tx, draft)?;
        }
        tx.commit()?;
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

    /// A live SMTP transfer owns this lock even across app processes. The
    /// file stays in place so a crash releases the lock without changing
    /// the durable, possibly-sent record or letting callers lock two inodes.
    pub(crate) fn lock_compose_send(&self, id: &str) -> Result<Option<File>> {
        workspace::validate_id(id).map_err(crate::Error::Invalid)?;
        let guard = self.try_lock_mailbox_operation(id)?;
        if guard.is_none() && self.db_path().is_some() {
            return Err(crate::Error::Action(SEND_IN_PROGRESS.into()));
        }
        Ok(guard)
    }

    /// Cross-process lock. In-memory stores have no second connection.
    pub(crate) fn try_lock_mailbox_operation(&self, name: &str) -> Result<Option<File>> {
        let Some(db) = self.db_path() else {
            return Ok(None);
        };
        let mut directory = fs::canonicalize(db)?.into_os_string();
        directory.push(".compose-locks");
        let directory = PathBuf::from(directory);
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&directory)?;
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .open(directory.join(name))?;
        if !FileExt::try_lock_exclusive(&file)? {
            return Ok(None);
        }
        Ok(Some(file))
    }

    pub(crate) fn compose_is_discarded(&self, id: &str) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT EXISTS (
                 SELECT 1 FROM compose_drafts WHERE id = ?1 AND discarded_action_id IS NOT NULL
             )",
            params![id],
            |row| row.get(0),
        )?)
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
             SELECT ?1, ?2, ?3, ?4, ?5
             WHERE NOT EXISTS (
                 SELECT 1 FROM compose_drafts WHERE id = ?2 AND discarded_action_id IS NOT NULL
             )
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
        if self.compose_is_discarded(draft_id)? {
            return Err(crate::Error::Action("Draft was discarded".into()));
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
        let tx = crate::store::begin_immediate(&self.conn)?;
        match lock {
            None => {
                tx.execute(
                    "UPDATE compose_send SET state = ?3, updated_unix = ?4, message_id = NULL
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
                tx.execute(
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
        if lock == Some(SendLock::Sent) {
            crate::outbox::drafts::queue_delete(&tx, account_id, draft_id)?;
        }
        tx.commit()?;
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
                        subject, body, in_reply_to, identities, owner,
                        references_header, quote_text, quote_html, reply_to_unverified, managed_signature
                 FROM compose_drafts WHERE id = ?1 AND discarded_action_id IS NULL",
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
                        ReplyParts {
                            references: split_list(&row.get::<_, String>(10)?),
                            quote_text: row.get(11)?,
                            quote_html: row.get(12)?,
                            reply_to_unverified: row.get(13)?,
                        },
                        row.get::<_, Option<String>>(14)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            account,
            from,
            to,
            cc,
            bcc,
            subject,
            body,
            reply,
            identities,
            owner,
            parts,
            managed_signature,
        )) = row
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
            managed_signature,
            owner,
            reply: parts,
        }))
    }

    /// The account's compose drafts that have not gone out, newest first.
    ///
    /// A draft whose send went out leaves the list; one that may have gone
    /// out stays, marked, so the person checks Sent. An agent's draft is
    /// listed while its review draft is (an undone one is not).
    pub fn unsent_compose_drafts(&self, account_id: &AccountId) -> Result<Vec<DraftSummary>> {
        use super::DraftSaveStatus;
        let write_back = self.write_back(account_id)?;
        let mut stmt = self.conn.prepare(
            "SELECT d.id, d.from_addr, d.to_addrs, d.cc_addrs, d.subject, d.body, d.owner,
                    d.updated_unix, s.state, d.bcc_addrs, d.in_reply_to, d.quote_text,
                    ?4 AND (EXISTS (SELECT 1 FROM server_draft_copies c
                        WHERE c.account_id = d.account_id AND c.draft_id = d.id)
                      OR EXISTS (SELECT 1 FROM server_outbox o
                        WHERE o.account_id = d.account_id AND o.draft_id = d.id
                          AND o.kind = 'append_draft' AND o.placed_uid IS NOT NULL)),
                    ?4 AND EXISTS (SELECT 1 FROM compose_attachments a WHERE a.draft_id = d.id),
                    ?4 AND EXISTS (SELECT 1 FROM server_outbox o
                        WHERE o.account_id = d.account_id AND o.draft_id = d.id
                          AND o.kind = 'append_draft')
             FROM compose_drafts d
             LEFT JOIN compose_send s ON s.account_id = d.account_id AND s.draft_id = d.id
             WHERE d.account_id = ?1
               AND d.discarded_action_id IS NULL
               AND IFNULL(s.state, ?2) != ?3
               AND (d.owner = '' OR EXISTS (
                   SELECT 1 FROM review_drafts r
                   WHERE r.account_id = d.account_id AND 'r' || r.id = d.id
               ))
             ORDER BY d.updated_unix DESC, d.id DESC",
        )?;
        let rows = stmt.query_map(
            params![account_id.as_str(), SEND_CLEAR, SEND_SENT, write_back],
            |row| {
                let state: Option<String> = row.get(8)?;
                let in_reply_to: Option<String> = row.get(10)?;
                let quote_text: Option<String> = row.get(11)?;
                Ok((
                    in_reply_to.unwrap_or_default(),
                    quote_text.unwrap_or_default(),
                    row.get::<_, bool>(12)?,
                    row.get::<_, bool>(13)?,
                    row.get::<_, bool>(14)?,
                    DraftSummary {
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
                        not_on_server: false,
                        server_save_failed: false,
                        reply_context: None,
                        forwarded: None,
                        in_reply_to: None,
                        known_names: Default::default(),
                    },
                ))
            },
        )?;
        let rows = rows.collect::<std::result::Result<Vec<_>, _>>()?;
        // The drafts list is short; each reply reads its original as the
        // compose window does, so the two say the same thing.
        let mut drafts = Vec::with_capacity(rows.len());
        for (in_reply_to, quote_text, has_copy, has_files, attempted, mut draft) in rows {
            if write_back && !draft.maybe_sent {
                draft.not_on_server = if has_copy || attempted {
                    // The existing digest also identifies drafts saved by older versions.
                    // Compare it with the copy and outbox for this revision.
                    if let Some(current) = self.load_compose(&draft.id)? {
                        let digest = super::server::digest(&current, &current.files);
                        let status =
                            self.server_draft_save_status(account_id, &draft.id, &digest)?;
                        draft.server_save_failed = status == Some(DraftSaveStatus::ServerFailed);
                        status != Some(DraftSaveStatus::Server)
                    } else {
                        false
                    }
                } else {
                    draft.owner.is_empty()
                        && (!draft.subject.is_empty()
                            || !draft.body.trim().is_empty()
                            || !draft.to.is_empty()
                            || !draft.cc.is_empty()
                            || !draft.bcc.is_empty()
                            || !in_reply_to.is_empty()
                            || has_files)
                };
            }
            if !in_reply_to.trim().is_empty() {
                draft.reply_context = self
                    .message_summary(account_id, &crate::MessageId::new(in_reply_to.trim()))
                    .ok()
                    .flatten();
                draft.in_reply_to = Some(in_reply_to.trim().to_string());
            }
            draft.forwarded = super::reply::forward_summary(&quote_text);
            drafts.push(draft);
        }
        let emails: Vec<String> = drafts
            .iter()
            .flat_map(|draft| draft.to.iter().chain(&draft.cc).chain(&draft.bcc))
            .filter_map(|raw| super::email_of(raw))
            .collect();
        let names = self.sender_names(account_id, &emails).unwrap_or_default();
        for draft in &mut drafts {
            draft.known_names = draft
                .to
                .iter()
                .chain(&draft.cc)
                .chain(&draft.bcc)
                .filter_map(|raw| super::email_of(raw))
                .filter_map(|email| {
                    let email = email.to_lowercase();
                    names.get(&email).map(|name| (email, name.clone()))
                })
                .collect();
        }
        Ok(drafts)
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
                "SELECT m.from_name, m.from_email, m.subject, m.date_unix, m.reply_to_line,
                        m.arrived_via
                 FROM messages m
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
                    let reply_to: String = row.get(4)?;
                    Ok(super::ReplyContext {
                        sender: name
                            .filter(|name| !name.trim().is_empty())
                            .unwrap_or_else(|| email.clone()),
                        sender_email: email,
                        arrived_via: crate::store::split_field(&row.get::<_, String>(5)?),
                        reply_to: crate::message::decode_reply_to(&reply_to).map(|addresses| {
                            addresses
                                .into_iter()
                                .map(|address| address.email.trim().to_lowercase())
                                .filter(|email| !email.is_empty())
                                .collect()
                        }),
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
        // Sent by role: "Sent Items" or "[Gmail]/Sent Mail" count too.
        let roles = self.folder_roles(account_id)?;
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
            if !split_list(&folders)
                .iter()
                .any(|folder| roles.is(crate::FolderRole::Sent, folder))
            {
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
        let tx = crate::store::begin_immediate(&self.conn)?;
        delete_compose_in(&tx, id)?;
        tx.commit()?;
        Ok(())
    }
}

fn delete_compose_in(conn: &Connection, id: &str) -> Result<()> {
    conn.execute_batch(REVIEW_SCHEMA)?;
    conn.execute("DELETE FROM compose_review WHERE draft_id = ?1", [id])?;
    conn.execute("DELETE FROM compose_attachments WHERE draft_id = ?1", [id])?;
    conn.execute("DELETE FROM compose_send WHERE draft_id = ?1", [id])?;
    conn.execute("DELETE FROM compose_drafts WHERE id = ?1", [id])?;
    Ok(())
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
    let store = indexed_store(&compose_index_file(id)?).ok()?;
    store.is_file().then_some(store)
}

fn indexed_store(path: &Path) -> std::io::Result<PathBuf> {
    std::fs::read_to_string(path).map(|text| PathBuf::from(text.trim()))
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
    let store = open_store_for(id);
    if draft_file_is_regular(&folder) {
        if let Some(store) = &store
            && store
                .compose_is_discarded(id)
                .map_err(|err| err.to_string())?
        {
            return Err(format!("draft {id} is not open"));
        }
        return Ok(folder);
    }
    let Some(store) = store else {
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
    workspace::validate_id(id).ok()?;
    let root = if let Some(dir) = std::env::var_os("FELL_COMPOSE_INDEX") {
        PathBuf::from(dir)
    } else if cfg!(any(test, feature = "test-utils")) {
        panic!(
            "draft tests must use an explicit store or set FELL_COMPOSE_INDEX in an isolated child"
        );
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
    let open = |path: &Path| -> crate::Result<_> {
        match path.metadata() {
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            result => {
                result?;
            }
        }
        let store = Store::open_existing_without_index(path)?;
        Ok(store.load_compose(id)?.map(|draft| (store, draft)))
    };
    let index = compose_index_file(id)?;
    match indexed_store(&index) {
        Ok(path) => return open(&path).ok()?.map(|(store, _)| store),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    let base = mailbox_db_path()?;
    let registry = crate::accounts::Registry::resolve().ok()?;
    let accounts = registry.load();
    if accounts.is_empty() && !registry.directory().exists() {
        return open(&base).ok()?.map(|(store, _)| store);
    }
    let mut matched = None;
    for entry in accounts.entries {
        let Some(id) = entry.path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if entry.path.parent() != Some(registry.directory().as_path()) {
            continue;
        }
        let Ok(path) = registry.store_path(&base, id) else {
            continue;
        };
        if let Ok(Some((store, draft))) = open(&path)
            && draft.account_id.as_str() == id
            && matched.replace(store).is_some()
        {
            return None;
        }
    }
    matched
}

/// Include discarded drafts and workspaces whose index was written before
/// their SQLite row. Both may still have local text or attachments.
pub(crate) fn drafts_for_removal(
    account: &AccountId,
    mailbox: &Path,
    store: Option<&Store>,
) -> std::result::Result<Vec<String>, String> {
    let mut drafts = Vec::new();
    if let Some(store) = store {
        let read = || -> crate::Result<Vec<String>> {
            let mut stmt = store
                .conn
                .prepare("SELECT id FROM compose_drafts WHERE account_id = ?1")?;
            Ok(stmt
                .query_map([account.as_str()], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?)
        };
        drafts = read().map_err(|err| err.to_string())?;
    }
    let root = workspace::draft_root().map_err(|err| err.to_string())?;
    for dir in [
        root.as_path(),
        compose_index_file("draft")
            .as_deref()
            .and_then(Path::parent)
            .unwrap_or(&root),
    ] {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.to_string()),
        };
        for entry in entries {
            let entry = entry.map_err(|err| err.to_string())?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let id = name
                .strip_prefix('.')
                .and_then(|name| name.strip_suffix(".private"))
                .unwrap_or(name);
            if workspace::validate_id(id).is_err() {
                continue;
            }
            let folder = root.join(id);
            if workspace::read_account(&folder).as_deref() == Some(account.as_str())
                || draft_store_matches(id, mailbox, &folder)
            {
                drafts.push(id.to_owned());
            }
        }
    }
    Ok(drafts)
}

fn draft_store_matches(id: &str, mailbox: &Path, folder: &Path) -> bool {
    [
        compose_index_file(id),
        Some(workspace::control_dir(folder).join("store")),
    ]
    .into_iter()
    .flatten()
    .any(|path| {
        indexed_store(&path).is_ok_and(|path| {
            normalized_mailbox(&path)
                .is_ok_and(|path| normalized_mailbox(mailbox).is_ok_and(|mailbox| path == mailbox))
        })
    })
}

fn normalized_mailbox(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    if let (Some(parent), Some(name)) = (absolute.parent(), absolute.file_name()) {
        // During removal the database has already moved, but the directory
        // still resolves aliases and relative components in the saved path.
        match parent.canonicalize() {
            Ok(parent) => return Ok(parent.join(name)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    Ok(absolute)
}

pub(crate) fn remove_draft_workspaces(
    ids: &[String],
    account: &str,
    mailbox: &Path,
) -> std::result::Result<(), String> {
    let check = |path: &Path, result: std::io::Result<()>| match result {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("{}: {err}", path.display())),
    };
    let mailbox =
        normalized_mailbox(mailbox).map_err(|err| format!("{}: {err}", mailbox.display()))?;
    let mut workspaces = Vec::new();
    for id in ids {
        let folder = workspace::draft_folder(id).map_err(|err| format!("Draft {id:?}: {err}"))?;
        let control = workspace::control_dir(&folder);
        for path in [&folder, &control] {
            match path.symlink_metadata() {
                Ok(meta) if !meta.is_dir() => {
                    return Err(format!(
                        "{}: Expected a draft workspace directory",
                        path.display()
                    ));
                }
                Ok(_) => {}
                Err(err) => check(path, Err(err))?,
            }
        }
        let index = compose_index_file(id).ok_or("No compose index directory")?;
        match indexed_store(&index).and_then(|path| normalized_mailbox(&path)) {
            Ok(path) if path != mailbox => {
                return Err(format!(
                    "{} points to {}. Check which mailbox owns draft {id:?} before retrying.",
                    index.display(),
                    path.display(),
                ));
            }
            Ok(_) => {}
            Err(err) => check(&index, Err(err))?,
        }
        if let Some(owner) = workspace::read_account(&folder).filter(|owner| owner != account) {
            return Err(format!(
                "{} belongs to account {owner:?}. Check which account owns draft {id:?} before retrying.",
                folder.display(),
            ));
        }
        workspaces.push((folder, control, index));
    }
    for (folder, control, index) in workspaces {
        check(&folder, std::fs::remove_dir_all(&folder))?;
        check(&control, std::fs::remove_dir_all(&control))?;
        if let Some(root) = folder.parent() {
            check(
                root,
                std::fs::File::open(root).and_then(|dir| dir.sync_all()),
            )?;
        }
        check(&index, std::fs::remove_file(&index))?;
        if let Some(root) = index.parent() {
            check(
                root,
                std::fs::File::open(root).and_then(|dir| dir.sync_all()),
            )?;
        }
    }
    Ok(())
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
    fn an_unindexed_draft_resolves_its_account_or_fails_on_ambiguity() {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;

        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let registry = crate::accounts::Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        let base = root.join("mail.db");
        let mut stores = Vec::new();
        for id in ["mail", "work"] {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(registry.path(id).unwrap())
                .unwrap();
            writeln!(file, "email = '{id}@example.test'\npassword = 'fixture'").unwrap();
            let config = registry.get(&AccountId(id.into())).unwrap().unwrap();
            let path = registry.store_path(&base, id).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let store = Store::open(&path).unwrap();
            let account = config.account();
            store.upsert_account(&account).unwrap();
            stores.push((store, account));
        }
        let save = |(store, account): &(Store, crate::Account)| {
            store
                .save_compose(&StoredCompose {
                    id: "cfixture".into(),
                    account_id: account.id.clone(),
                    fields: DraftFields::default(),
                    body: "Synthetic draft".into(),
                    files: Vec::new(),
                    identities: Vec::new(),
                    managed_signature: None,
                    owner: String::new(),
                    reply: Default::default(),
                })
                .unwrap();
        };
        save(&stores[1]);
        assert_eq!(stored_compose_account("cfixture").as_deref(), Some("work"));
        save(&stores[0]);
        assert!(open_store_for("cfixture").is_none());
        drop(stores);
        std::fs::create_dir(crate::accounts::removal_trash(&base)).unwrap();
        assert_eq!(stored_compose_account("cfixture").as_deref(), Some("work"));
    }

    #[test]
    fn people_mailed_from_any_sent_folder_are_known() {
        // #140: only "Sent" and "Sent Mail" counted, so Fastmail's "Sent
        // Items" left everyone mailed from it marked as first-time.
        let store = Store::open_in_memory().unwrap();
        let account = crate::Account {
            id: AccountId("a".into()),
            display_name: "Me".into(),
            email: "me@x".into(),
            backend_kind: crate::BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        let put = |id: &str, folder: &str, to: &str| {
            let message_id = crate::MessageId::new(id);
            store
                .upsert_message(&crate::Message {
                    account_id: account.id.clone(),
                    headers: crate::MessageHeaders {
                        message_id: message_id.clone(),
                        proton_conversation_id: None,
                        in_reply_to: None,
                        references: Vec::new(),
                        from: crate::Address {
                            name: None,
                            email: "me@x".into(),
                        },
                        subject: "Hi".into(),
                        date_unix: 1,
                    },
                    remote_id: crate::RemoteId(id.into()),
                    thread_id: crate::ThreadId::from_message_id(&message_id),
                    snippet: "hi".into(),
                    body_text: Some("hi".into()),
                    unread: false,
                    folders: vec![folder.into()],
                    labels: Vec::new(),
                    arrived_via: Vec::new(),
                    to: vec![crate::Address {
                        name: None,
                        email: to.into(),
                    }],
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    starred: false,
                    has_attachment: false,
                    archived: false,
                    snooze_until: None,
                    list_id: None,
                    reply_to: Some(Vec::new()),
                    attachment_names: Vec::new(),
                    parts: None,
                })
                .unwrap();
        };
        put("sent@x", "Sent Items", "maya@x");
        put("inbox@x", "INBOX", "stranger@x");
        let known = store.mailed_addresses(&account.id).unwrap();
        assert!(known.contains("maya@x"), "{known:?}");
        assert!(!known.contains("stranger@x"), "{known:?}");
    }

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
                reply_to: Some(Vec::new()),
                attachment_names: Vec::new(),
                parts: None,
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
        // The drafts list says what a reply answers, as its window does.
        let save = |id: &str, in_reply_to: Option<&str>, quote_text: &str| {
            store
                .save_compose(&StoredCompose {
                    id: id.into(),
                    account_id: account.id.clone(),
                    fields: DraftFields {
                        from: "me@x".into(),
                        to: vec!["jon@x".into()],
                        subject: "Re: Plans".into(),
                        in_reply_to: in_reply_to.map(str::to_string),
                        ..DraftFields::default()
                    },
                    body: "sounds good\n".into(),
                    files: Vec::new(),
                    identities: vec!["me@x".into()],
                    managed_signature: None,
                    owner: String::new(),
                    reply: super::super::ReplyParts {
                        quote_text: quote_text.into(),
                        ..Default::default()
                    },
                })
                .unwrap();
        };
        save("reply", Some("echo@list"), "> hi\n");
        save(
            "forward",
            None,
            "---------- Forwarded message ----------\nFrom: Ada Lovelace <ada@x>\n\
             Subject: Q3\n\nhi\n",
        );
        save("gone", Some("missing@x"), "");
        let listed = store.unsent_compose_drafts(&account.id).unwrap();
        let find = |id: &str| listed.iter().find(|draft| draft.id == id).unwrap();
        let reply = find("reply");
        assert_eq!(reply.reply_context.as_ref().unwrap().subject, "Plans");
        assert_eq!(reply.reply_context.as_ref().unwrap().sender, "Jon");
        assert_eq!(reply.forwarded, None);
        let forward = find("forward");
        assert_eq!(forward.reply_context, None);
        let summary = forward.forwarded.as_ref().unwrap();
        assert_eq!(
            (summary.sender.as_str(), summary.subject.as_str()),
            ("Ada Lovelace", "Q3")
        );
        // The original is no longer in the mailbox: nothing to name, but
        // its id still says it is a reply.
        assert_eq!(find("gone").reply_context, None);
        assert_eq!(find("gone").in_reply_to.as_deref(), Some("missing@x"));
        assert_eq!(find("forward").in_reply_to, None);
        // Jon sent mail with his name, so the drafts list knows it.
        assert_eq!(
            reply.known_names.get("jon@x").map(String::as_str),
            Some("Jon")
        );
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
                    managed_signature: None,
                    owner: owner.into(),
                    reply: Default::default(),
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
                managed_signature: None,
                owner: String::new(),
                reply: Default::default(),
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
                managed_signature: None,
                owner: String::new(),
                reply: Default::default(),
            })
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path().join("draft");
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
    }

    #[test]
    fn a_flushed_edit_restores_when_the_runtime_folder_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
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
        let mut session = super::super::session::DraftSession::testing_create_local(
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
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
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
                managed_signature: None,
                owner: "agent:ada".into(),
                reply: Default::default(),
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
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
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
                managed_signature: None,
                owner: "agent:ada".into(),
                reply: Default::default(),
            })
            .unwrap();
        let err = store.restore_compose_folder("keep", &folder).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("symlink"), "{text}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep-me");
    }
}
