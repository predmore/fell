//! Sending mail always needs an explicit UI confirmation.
//!
//! MCP tools may produce a [`Draft`]. They cannot construct a
//! [`UserSendConfirmation`]: minting requires [`fell_confirm::UiProof`],
//! which is sealed to [`fell_confirm::InteractiveUi`], and `fell-mcp` does
//! not depend on `fell-confirm`.

use std::sync::atomic::{AtomicU64, Ordering};

use crate::account::AccountId;

static NEXT_PENDING: AtomicU64 = AtomicU64::new(1);

/// A composed message that has not been transmitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub account_id: AccountId,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    /// When set, this draft is a reply in an existing thread.
    pub in_reply_to: Option<crate::MessageId>,
    /// Hash of the attachment bytes the editor resolved from `draft.md`.
    /// Empty when the draft has no attachments. Part of the send confirmation.
    pub attachments_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PendingSendId(pub String);

/// One file the editor resolved from `draft.md` and included in the MIME body.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SubmittedFile {
    pub name: String,
    pub bytes: Vec<u8>,
    pub inline: bool,
}

/// Draft accepted by a backend, waiting on UI confirmation before SMTP/API send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSend {
    pub id: PendingSendId,
    pub draft: Draft,
    /// Full MIME message, including the HTML alternative and inline images.
    pub mime: Vec<u8>,
    pub files: Vec<SubmittedFile>,
}

impl PendingSend {
    pub fn new(kind: &'static str, draft: Draft) -> Self {
        let n = NEXT_PENDING.fetch_add(1, Ordering::Relaxed);
        Self {
            id: PendingSendId(format!("{kind}-{n}")),
            draft,
            mime: Vec::new(),
            files: Vec::new(),
        }
    }

    pub fn with_message(mut self, mime: Vec<u8>, files: Vec<SubmittedFile>) -> Self {
        self.mime = mime;
        self.files = files;
        self
    }
}

/// Proof that the user confirmed send in the Fell UI.
///
/// Bound to a specific pending id plus the account, recipients, subject,
/// body, reply parent, attachments, and a SHA-256 of the exact message bytes
/// that were shown. Constructed only via
/// [`UserSendConfirmation::from_interactive_ui`].
///
/// ```compile_fail
/// use fell_core::{AccountId, Draft, PendingSend, UserSendConfirmation};
/// let pending = PendingSend::new(
///     "x",
///     Draft {
///         account_id: AccountId("a".into()),
///         to: vec![],
///         cc: vec![],
///         bcc: vec![],
///         subject: String::new(),
///         body: String::new(),
///         in_reply_to: None,
///         attachments_digest: String::new(),
///     },
/// );
/// let _ = UserSendConfirmation::from_interactive_ui(&Default::default(), &pending);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSendConfirmation {
    pending_id: PendingSendId,
    account_id: AccountId,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body: String,
    in_reply_to: Option<crate::MessageId>,
    attachments_digest: String,
    /// SHA-256 of [`PendingSend::mime`], the bytes SMTP transmits.
    mime_sha256: [u8; 32],
}

/// SHA-256 of the message bytes a confirmation covers.
pub fn message_digest(mime: &[u8]) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(mime).into()
}

impl UserSendConfirmation {
    /// Call from the GPUI app after an explicit user Send action.
    ///
    /// Snapshots the pending id, account, recipients, subject, body, reply
    /// parent, attachments, and a hash of the message bytes.
    /// `fell-mcp` cannot name [`fell_confirm::InteractiveUi`] without adding a
    /// Cargo dependency on `fell-confirm`, and builds `fell-core` without the
    /// `ui-confirm` feature, so this function does not exist there. There is
    /// no send MCP tool.
    #[cfg(any(test, feature = "ui-confirm"))]
    pub fn from_interactive_ui(_ui: &impl fell_confirm::UiProof, pending: &PendingSend) -> Self {
        Self {
            pending_id: pending.id.clone(),
            account_id: pending.draft.account_id.clone(),
            to: pending.draft.to.clone(),
            cc: pending.draft.cc.clone(),
            bcc: pending.draft.bcc.clone(),
            subject: pending.draft.subject.clone(),
            body: pending.draft.body.clone(),
            in_reply_to: pending.draft.in_reply_to.clone(),
            attachments_digest: pending.draft.attachments_digest.clone(),
            mime_sha256: message_digest(&pending.mime),
        }
    }

    pub fn pending_id(&self) -> &PendingSendId {
        &self.pending_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentMessage {
    pub pending_id: PendingSendId,
    pub remote_id: crate::RemoteId,
}

/// Refuse a send unless the confirmation token names this pending send
/// and still matches its account, recipients, subject, body, reply parent,
/// attachments, and message bytes.
pub fn require_confirmation(
    pending: &PendingSend,
    confirmation: &UserSendConfirmation,
) -> crate::Result<()> {
    if confirmation.pending_id == pending.id
        && confirmation.account_id == pending.draft.account_id
        && confirmation.to == pending.draft.to
        && confirmation.cc == pending.draft.cc
        && confirmation.bcc == pending.draft.bcc
        && confirmation.subject == pending.draft.subject
        && confirmation.body == pending.draft.body
        && confirmation.in_reply_to == pending.draft.in_reply_to
        && confirmation.attachments_digest == pending.draft.attachments_digest
        && confirmation.mime_sha256 == message_digest(&pending.mime)
    {
        Ok(())
    } else {
        Err(crate::Error::SendUnconfirmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountId, BackendKind};
    use crate::backend::{FixtureBackend, MailBackend, ProtonApiBackend};

    fn draft(body: &str) -> Draft {
        Draft {
            account_id: AccountId("a1".into()),
            to: vec!["b@x".into()],
            cc: vec![],
            bcc: vec![],
            subject: "hi".into(),
            body: body.into(),
            in_reply_to: None,
            attachments_digest: String::new(),
        }
    }

    fn mint(pending: &PendingSend) -> UserSendConfirmation {
        UserSendConfirmation::from_interactive_ui(
            &fell_confirm::InteractiveUi::for_fell_app(),
            pending,
        )
    }

    #[test]
    fn backend_will_not_send_without_matching_confirmation() {
        let backend = ProtonApiBackend;
        let pending = backend.submit_draft(draft("body")).unwrap();
        let other = mint(&PendingSend {
            id: PendingSendId("nope".into()),
            draft: draft("body"),
            mime: Vec::new(),
            files: Vec::new(),
        });
        let err = backend.confirm_send(pending, other).unwrap_err();
        assert!(matches!(err, crate::Error::SendUnconfirmed));
    }

    #[test]
    fn confirmation_for_one_draft_does_not_send_another() {
        let backend = FixtureBackend::default();
        let first = backend.submit_draft(draft("one")).unwrap();
        let second = backend.submit_draft(draft("two")).unwrap();
        assert_ne!(first.id, second.id);
        let err = backend
            .confirm_send(second.clone(), mint(&first))
            .unwrap_err();
        assert!(matches!(err, crate::Error::SendUnconfirmed));
        backend.confirm_send(second.clone(), mint(&second)).unwrap();
    }

    #[test]
    fn confirmation_does_not_cover_mutated_body() {
        let backend = FixtureBackend::default();
        let pending = backend.submit_draft(draft("shown")).unwrap();
        let confirm = mint(&pending);
        let mut swapped = pending;
        swapped.draft.body = "not what the user saw".into();
        let err = backend.confirm_send(swapped, confirm).unwrap_err();
        assert!(matches!(err, crate::Error::SendUnconfirmed));
    }

    #[test]
    fn confirmation_does_not_cover_mutated_subject() {
        let backend = FixtureBackend::default();
        let pending = backend.submit_draft(draft("shown")).unwrap();
        let confirm = mint(&pending);
        let mut swapped = pending;
        swapped.draft.subject = "not what the user saw".into();
        let err = backend.confirm_send(swapped, confirm).unwrap_err();
        assert!(matches!(err, crate::Error::SendUnconfirmed));
    }

    #[test]
    fn confirmation_does_not_cover_mutated_to_cc_or_bcc() {
        let backend = FixtureBackend::default();
        let pending = backend
            .submit_draft(Draft {
                account_id: AccountId("a1".into()),
                to: vec!["b@x".into()],
                cc: vec!["c@x".into()],
                bcc: vec!["d@x".into()],
                subject: "hi".into(),
                body: "shown".into(),
                in_reply_to: None,
                attachments_digest: String::new(),
            })
            .unwrap();
        let confirm = mint(&pending);
        for swapped in [
            {
                let mut draft = pending.clone();
                draft.draft.to = vec!["other@x".into()];
                draft
            },
            {
                let mut draft = pending.clone();
                draft.draft.cc = vec!["other@x".into()];
                draft
            },
            {
                let mut draft = pending.clone();
                draft.draft.bcc = vec!["other@x".into()];
                draft
            },
        ] {
            let err = backend.confirm_send(swapped, confirm.clone()).unwrap_err();
            assert!(matches!(err, crate::Error::SendUnconfirmed));
        }
    }

    #[test]
    fn confirmation_does_not_cover_mutated_account_id() {
        let backend = FixtureBackend::default();
        let pending = backend.submit_draft(draft("shown")).unwrap();
        let confirm = mint(&pending);
        let mut swapped = pending;
        swapped.draft.account_id = AccountId("other".into());
        let err = backend.confirm_send(swapped, confirm).unwrap_err();
        assert!(matches!(err, crate::Error::SendUnconfirmed));
    }

    #[test]
    fn fixture_kind_is_bridge_or_api_not_send_path() {
        assert_eq!(BackendKind::Bridge.as_str(), "bridge");
    }

    #[test]
    fn confirmation_does_not_cover_swapped_attachments() {
        let backend = FixtureBackend::default();
        let pending = backend.submit_draft(draft("shown")).unwrap();
        let confirm = mint(&pending);
        let mut swapped = pending;
        swapped.draft.attachments_digest = "other-bytes".into();
        let err = backend.confirm_send(swapped, confirm).unwrap_err();
        assert!(matches!(err, crate::Error::SendUnconfirmed));
    }

    #[test]
    fn confirmation_does_not_cover_a_retargeted_reply_parent() {
        let backend = FixtureBackend::default();
        let mut reply = draft("shown");
        reply.in_reply_to = Some(crate::MessageId::new("<parent@x>"));
        let pending = backend.submit_draft(reply).unwrap();
        let confirm = mint(&pending);
        for parent in [Some(crate::MessageId::new("<other@x>")), None] {
            let mut swapped = pending.clone();
            swapped.draft.in_reply_to = parent;
            let err = backend.confirm_send(swapped, confirm.clone()).unwrap_err();
            assert!(matches!(err, crate::Error::SendUnconfirmed));
        }
        let mut new_reply = backend.submit_draft(draft("shown")).unwrap();
        let confirm = mint(&new_reply);
        new_reply.draft.in_reply_to = Some(crate::MessageId::new("<parent@x>"));
        let err = backend.confirm_send(new_reply, confirm).unwrap_err();
        assert!(matches!(err, crate::Error::SendUnconfirmed));
        backend
            .confirm_send(pending.clone(), mint(&pending))
            .unwrap();
    }

    #[test]
    fn confirmation_does_not_cover_changed_message_bytes() {
        let backend = FixtureBackend::default();
        let pending = backend
            .submit_composed(
                draft("shown"),
                b"From: a@x\r\nTo: b@x\r\n\r\nshown\r\n".to_vec(),
                Vec::new(),
            )
            .unwrap();
        let confirm = mint(&pending);
        for mime in [
            b"From: a@x\r\nTo: b@x\r\nBcc: c@x\r\n\r\nshown\r\n".to_vec(),
            b"From: a@x\r\nTo: b@x\r\n\r\nshown\r\n ".to_vec(),
            Vec::new(),
        ] {
            let mut swapped = pending.clone();
            swapped.mime = mime;
            let err = backend.confirm_send(swapped, confirm.clone()).unwrap_err();
            assert!(matches!(err, crate::Error::SendUnconfirmed));
        }
        backend.confirm_send(pending, confirm).unwrap();
    }

    #[test]
    fn message_digest_is_sha256() {
        let hex: String = message_digest(b"abc")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(
            hex,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
