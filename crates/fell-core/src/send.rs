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
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PendingSendId(pub String);

/// Draft accepted by a backend, waiting on UI confirmation before SMTP/API send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingSend {
    pub id: PendingSendId,
    pub draft: Draft,
}

impl PendingSend {
    pub fn new(kind: &'static str, draft: Draft) -> Self {
        let n = NEXT_PENDING.fetch_add(1, Ordering::Relaxed);
        Self {
            id: PendingSendId(format!("{kind}-{n}")),
            draft,
        }
    }
}

/// Proof that the user confirmed send in the Fell UI.
///
/// Bound to a specific pending id plus the account, recipients, subject, and
/// body that were shown. Constructed only via
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
}

impl UserSendConfirmation {
    /// Call from the GPUI app after an explicit user Send action.
    ///
    /// Snapshots the pending id, account, recipients, subject, and body.
    /// `fell-mcp` cannot name [`fell_confirm::InteractiveUi`] without adding a
    /// Cargo dependency on `fell-confirm`. There is no send MCP tool.
    pub fn from_interactive_ui(_ui: &impl fell_confirm::UiProof, pending: &PendingSend) -> Self {
        Self {
            pending_id: pending.id.clone(),
            account_id: pending.draft.account_id.clone(),
            to: pending.draft.to.clone(),
            cc: pending.draft.cc.clone(),
            bcc: pending.draft.bcc.clone(),
            subject: pending.draft.subject.clone(),
            body: pending.draft.body.clone(),
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
/// and still matches its account, recipients, subject, and body.
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
}
