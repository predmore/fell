use crate::account::BackendKind;
use crate::message::Message;
use crate::send::{Draft, PendingSend, SentMessage, UserSendConfirmation, require_confirmation};
use crate::{Error, Result};

/// Opaque sync checkpoint from a backend (IMAP UIDVALIDITY/HIGHESTMODSEQ, Proton event id, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCursor {
    pub account_key: String,
    pub token: String,
}

/// Messages (and later flag/label changes) to apply to the local store.
#[derive(Debug, Clone, Default)]
pub struct MailboxDelta {
    pub messages: Vec<Message>,
    pub cursor: Option<SyncCursor>,
}

/// Mail transport. IMAP/Proton live behind this trait; the UI never calls them directly.
pub trait MailBackend {
    fn kind(&self) -> BackendKind;

    fn fetch_changes(&self, cursor: Option<&SyncCursor>) -> Result<MailboxDelta>;

    /// Accept a draft for later send. Must not transmit.
    fn submit_draft(&self, draft: Draft) -> Result<PendingSend>;

    /// Transmit only after [`UserSendConfirmation`] from the GPUI app.
    fn confirm_send(
        &self,
        pending: PendingSend,
        confirmation: UserSendConfirmation,
    ) -> Result<SentMessage>;
}

/// Proton Mail Bridge over local IMAP/SMTP. Protocol work is not wired yet.
#[derive(Debug, Clone)]
pub struct BridgeBackend {
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
}

impl Default for BridgeBackend {
    fn default() -> Self {
        Self {
            imap_host: "127.0.0.1".into(),
            imap_port: 1143,
            smtp_host: "127.0.0.1".into(),
            smtp_port: 1025,
        }
    }
}

impl MailBackend for BridgeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Bridge
    }

    fn fetch_changes(&self, _cursor: Option<&SyncCursor>) -> Result<MailboxDelta> {
        Err(Error::NotImplemented("Proton Mail Bridge IMAP"))
    }

    fn submit_draft(&self, draft: Draft) -> Result<PendingSend> {
        Ok(PendingSend::new("bridge", draft))
    }

    fn confirm_send(
        &self,
        pending: PendingSend,
        confirmation: UserSendConfirmation,
    ) -> Result<SentMessage> {
        require_confirmation(&pending, &confirmation)?;
        Err(Error::NotImplemented("Proton Mail Bridge SMTP"))
    }
}

/// Direct Proton API. Same trait as Bridge; not implemented yet.
#[derive(Debug, Clone, Default)]
pub struct ProtonApiBackend;

impl MailBackend for ProtonApiBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::ProtonApi
    }

    fn fetch_changes(&self, _cursor: Option<&SyncCursor>) -> Result<MailboxDelta> {
        Err(Error::NotImplemented("Proton API"))
    }

    fn submit_draft(&self, draft: Draft) -> Result<PendingSend> {
        Ok(PendingSend::new("api", draft))
    }

    fn confirm_send(
        &self,
        pending: PendingSend,
        confirmation: UserSendConfirmation,
    ) -> Result<SentMessage> {
        require_confirmation(&pending, &confirmation)?;
        Err(Error::NotImplemented("Proton API send"))
    }
}

/// In-process backend for tests and the empty-window demo. No network.
#[derive(Debug, Clone, Default)]
pub struct FixtureBackend {
    pub messages: Vec<Message>,
}

impl MailBackend for FixtureBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Bridge
    }

    fn fetch_changes(&self, _cursor: Option<&SyncCursor>) -> Result<MailboxDelta> {
        Ok(MailboxDelta {
            messages: self.messages.clone(),
            cursor: Some(SyncCursor {
                account_key: "fixture".into(),
                token: "1".into(),
            }),
        })
    }

    fn submit_draft(&self, draft: Draft) -> Result<PendingSend> {
        Ok(PendingSend::new("fixture", draft))
    }

    fn confirm_send(
        &self,
        pending: PendingSend,
        confirmation: UserSendConfirmation,
    ) -> Result<SentMessage> {
        require_confirmation(&pending, &confirmation)?;
        Ok(SentMessage {
            pending_id: pending.id,
            remote_id: crate::RemoteId("fixture-sent".into()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::AccountId;

    #[test]
    fn bridge_is_typed_stub() {
        let backend = BridgeBackend::default();
        assert_eq!(backend.kind(), BackendKind::Bridge);
        assert!(matches!(
            backend.fetch_changes(None),
            Err(Error::NotImplemented(_))
        ));
    }

    #[test]
    fn fixture_send_after_ui_confirmation() {
        let backend = FixtureBackend::default();
        let pending = backend
            .submit_draft(Draft {
                account_id: AccountId("a".into()),
                to: vec!["b@x".into()],
                cc: vec![],
                bcc: vec![],
                subject: "s".into(),
                body: "b".into(),
                in_reply_to: None,
            })
            .unwrap();
        let confirm = UserSendConfirmation::from_interactive_ui(
            &fell_confirm::InteractiveUi::for_fell_app(),
            &pending,
        );
        let sent = backend.confirm_send(pending, confirm).unwrap();
        assert_eq!(sent.remote_id.0, "fixture-sent");
    }
}
