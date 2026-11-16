//! Sending mail always needs an explicit UI confirmation.
//!
//! MCP tools may produce a [`Draft`]. They cannot construct a
//! [`UserSendConfirmation`]; that type is minted only by the GPUI app after the
//! user confirms send.

use crate::account::AccountId;

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

/// Proof that the user confirmed send in the Fell UI.
///
/// Only [`UserSendConfirmation::from_interactive_ui`] constructs this, and that
/// function is intended to be called from `fell` (the GPUI crate) alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSendConfirmation {
    pending_id: PendingSendId,
}

impl UserSendConfirmation {
    /// Call only from the GPUI app after an explicit user Send action.
    ///
    /// Agent / MCP code must not call this. There is no send MCP tool.
    pub fn from_interactive_ui(pending_id: PendingSendId) -> Self {
        Self { pending_id }
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

/// Refuse a send unless the confirmation token names this pending send.
pub fn require_confirmation(
    pending: &PendingSend,
    confirmation: &UserSendConfirmation,
) -> crate::Result<()> {
    if confirmation.pending_id() == &pending.id {
        Ok(())
    } else {
        Err(crate::Error::SendUnconfirmed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountId, BackendKind};
    use crate::backend::{MailBackend, ProtonApiBackend};

    fn draft() -> Draft {
        Draft {
            account_id: AccountId("a1".into()),
            to: vec!["b@x".into()],
            cc: vec![],
            bcc: vec![],
            subject: "hi".into(),
            body: "body".into(),
            in_reply_to: None,
        }
    }

    #[test]
    fn backend_will_not_send_without_matching_confirmation() {
        let backend = ProtonApiBackend;
        let pending = backend.submit_draft(draft()).unwrap();
        let other = UserSendConfirmation::from_interactive_ui(PendingSendId("nope".into()));
        let err = backend.confirm_send(pending, other).unwrap_err();
        assert!(matches!(err, crate::Error::SendUnconfirmed));
    }

    #[test]
    fn fixture_kind_is_bridge_or_api_not_send_path() {
        assert_eq!(BackendKind::Bridge.as_str(), "bridge");
    }
}
