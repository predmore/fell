//! The editor hands the writer a snapshot for Drafts after a quiet period
//! or on close. The regular compose saves and agent tools stay local.
use std::sync::Arc;

use super::{ResolvedFile, StoredCompose, frontmatter::write_draft, mime, workspace};
use crate::{Result, Store, diagnostic, outbox};

pub(super) type Wake = Arc<dyn Fn() + Send + Sync>;

pub const SERVER_DRAFT_SAVE_FAILED: &str =
    "Not saved to the server · kept in Fell · edit to retry · Space ? l for details";
pub const SERVER_DRAFT_SAVE_RETRYING: &str = "Not saved to the server · kept in Fell · retrying";

/// What the writer saved for one exact editor revision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveReceipt {
    Local,
    Pending,
    Server([u8; 32]),
    ServerFailed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DraftSaveStatus {
    Local,
    Server,
    ServerRetrying,
    ServerFailed,
}

impl DraftSaveStatus {
    pub fn warning(self) -> Option<&'static str> {
        match self {
            Self::ServerRetrying => Some(SERVER_DRAFT_SAVE_RETRYING),
            Self::ServerFailed => Some(SERVER_DRAFT_SAVE_FAILED),
            _ => None,
        }
    }
}

pub fn server_draft_open_notice(error: &crate::Error) -> String {
    use crate::{Error, SyncFailureKind, classify_sync_error};
    let detail = match error {
        Error::DraftBodyUnavailable
        | Error::DraftFormattingUnavailable
        | Error::DraftNoLongerInFolder
        | Error::DraftUnavailable
        | Error::DraftAttachmentLimit
        | Error::DraftMessageLimit => return error.to_string(),
        _ => match classify_sync_error(error) {
            SyncFailureKind::Unreachable => "Fell couldn't reach the mail server.".into(),
            SyncFailureKind::LoginRejected => "The mail server rejected the account login.".into(),
            SyncFailureKind::Password => "Fell couldn't read the account password.".into(),
            SyncFailureKind::Tls => {
                "Fell couldn't establish a secure connection to the mail server.".into()
            }
            SyncFailureKind::Other => crate::notes::display_safe(&error.to_string(), false),
        },
    };
    format!("Couldn't open the server draft. {detail}")
}

impl Store {
    /// The write-back worker owns the transition from pending to confirmed.
    pub fn server_draft_save_status(
        &self,
        account: &crate::AccountId,
        draft: &str,
        digest: &[u8; 32],
    ) -> Result<Option<DraftSaveStatus>> {
        outbox::drafts::save_status(self, account, draft, digest)
    }
}

const DEBOUNCE_MS: u64 = 5_000;

#[derive(Default)]
pub(super) struct Debounce {
    allowed: bool,
    observed: Option<(u64, u64)>,
    queued: Option<u64>,
}

impl Debounce {
    pub fn allow(&mut self) {
        self.allowed = true;
    }

    pub fn close(&mut self) {
        // Reopening reauthorizes future edits. A revision already queued
        // keeps its receipt and does not get another automatic attempt.
        self.allowed = false;
        self.observed = None;
    }

    pub fn due(&mut self, revision: u64, now_ms: Option<u64>) -> bool {
        if !self.allowed || self.queued == Some(revision) {
            return false;
        }
        if let Some(now_ms) = now_ms {
            let (previous, since) = *self.observed.get_or_insert((revision, now_ms));
            if previous != revision {
                self.observed = Some((revision, now_ms));
                return false;
            }
            if now_ms.saturating_sub(since) < DEBOUNCE_MS {
                return false;
            }
        }
        self.queued = Some(revision);
        true
    }
}

pub(super) fn digest(draft: &StoredCompose, files: &[ResolvedFile]) -> [u8; 32] {
    let mut fields = draft.fields.clone();
    // Materializing a draft reconstructs attachment refs from its files.
    fields.attachments.clear();
    let mut files: Vec<_> = files
        .iter()
        .map(|file| {
            (
                &file.path,
                &file.display_name,
                super::digest::sha256(&file.bytes),
            )
        })
        .collect();
    files.sort_unstable();
    let content = serde_json::to_vec(&(
        write_draft(&fields, &draft.body),
        &draft.reply.references,
        &draft.reply.quote_text,
        &draft.reply.quote_html,
        files,
    ))
    .expect("serializing draft content");
    super::digest::sha256(&content)
}

pub(super) fn queue(
    store: &Store,
    folder: &std::path::Path,
    draft: &StoredCompose,
    wake: &Wake,
) -> SaveReceipt {
    match queue_if_enabled(store, folder, draft, wake) {
        Ok(receipt) => receipt,
        Err(err) => {
            diagnostic!(
                WARN,
                "Could not queue server draft",
                error = &err as &dyn std::error::Error
            );
            SaveReceipt::ServerFailed
        }
    }
}

fn queue_if_enabled(
    store: &Store,
    folder: &std::path::Path,
    draft: &StoredCompose,
    wake: &Wake,
) -> Result<SaveReceipt> {
    if !store.write_back(&draft.account_id)? {
        return Ok(SaveReceipt::Local);
    }
    let fields = &draft.fields;
    if fields.subject.is_empty()
        && draft.body.trim().is_empty()
        && fields.to.is_empty()
        && fields.cc.is_empty()
        && fields.bcc.is_empty()
        && fields.attachments.is_empty()
        && fields.in_reply_to.is_none()
    {
        // Closing a fresh, untouched compose window does not create server mail.
        if !outbox::drafts::has_copy(store, &draft.account_id, &draft.id)? {
            return Ok(SaveReceipt::Pending);
        }
    }
    let closure = workspace::resolve_closure(folder, fields, &draft.body);
    if closure.issues.iter().any(|issue| issue.blocks_send) {
        diagnostic!(DEBUG, "Server draft has an unresolved attachment");
        return Ok(SaveReceipt::ServerFailed);
    }
    let (digest, outgoing) = server_draft_payload(draft, &closure.files);
    if outbox::drafts::queue(
        store,
        &draft.account_id,
        &draft.id,
        &digest,
        outgoing.message_id,
        &outgoing.mime,
    )? {
        wake();
    }
    Ok(SaveReceipt::Server(digest))
}

pub(crate) fn server_draft_payload(
    draft: &StoredCompose,
    files: &[ResolvedFile],
) -> ([u8; 32], super::Outgoing) {
    (digest(draft, files), mime::build_server_draft(draft, files))
}
