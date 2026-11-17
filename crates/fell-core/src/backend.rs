use crate::account::{AccountId, BackendKind};
use crate::imap::ImapConfig;
use crate::message::Message;
use crate::send::{
    Draft, PendingSend, SentMessage, SubmittedFile, UserSendConfirmation, require_confirmation,
};
use crate::{Error, Result};

/// Opaque sync checkpoint from a backend (IMAP UIDVALIDITY/HIGHESTMODSEQ, Proton event id, …).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCursor {
    pub account_key: String,
    pub token: String,
}

/// One already-stored message whose flags changed. The body is left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagUpdate {
    pub remote_id: crate::RemoteId,
    pub unread: bool,
}

/// Messages and flag updates to apply to the local store.
#[derive(Debug, Clone, Default)]
pub struct MailboxDelta {
    pub messages: Vec<Message>,
    pub flags: Vec<FlagUpdate>,
    pub cursor: Option<SyncCursor>,
}

/// Mail transport. IMAP/Proton live behind this trait; the UI never calls them directly.
pub trait MailBackend {
    fn kind(&self) -> BackendKind;

    fn fetch_changes(&self, cursor: Option<&SyncCursor>) -> Result<MailboxDelta>;

    /// Persist each bounded delta as it is produced. The default fetches once.
    fn sync_changes(
        &self,
        cursor: Option<&SyncCursor>,
        persist: &mut dyn FnMut(MailboxDelta) -> Result<()>,
    ) -> Result<()> {
        persist(self.fetch_changes(cursor)?)
    }

    /// Accept a draft for later send. Must not transmit.
    fn submit_draft(&self, draft: Draft) -> Result<PendingSend>;

    /// Accept the MIME body and the files the editor resolved. Must not transmit.
    ///
    /// The default keeps [`submit_draft`] and attaches the message the editor built.
    fn submit_composed(
        &self,
        draft: Draft,
        mime: Vec<u8>,
        files: Vec<SubmittedFile>,
    ) -> Result<PendingSend> {
        Ok(self.submit_draft(draft)?.with_message(mime, files))
    }

    /// Transmit only after [`UserSendConfirmation`] from the GPUI app.
    fn confirm_send(
        &self,
        pending: PendingSend,
        confirmation: UserSendConfirmation,
    ) -> Result<SentMessage>;
}

/// Proton Mail Bridge as a local IMAP account. Bridge stays a separate process.
///
/// SMTP fields are recorded for a later send path. This backend does not connect
/// to them and does not transmit mail.
#[derive(Debug, Clone)]
pub struct BridgeBackend {
    pub account_id: AccountId,
    pub imap: ImapConfig,
    pub smtp_host: String,
    pub smtp_port: u16,
}

impl Default for BridgeBackend {
    fn default() -> Self {
        Self {
            account_id: AccountId("bridge".into()),
            imap: ImapConfig::bridge_local("", ""),
            smtp_host: "127.0.0.1".into(),
            smtp_port: 1025,
        }
    }
}

impl MailBackend for BridgeBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Bridge
    }

    fn fetch_changes(&self, cursor: Option<&SyncCursor>) -> Result<MailboxDelta> {
        collect_imap(&self.account_id, &self.imap, cursor)
    }

    fn sync_changes(
        &self,
        cursor: Option<&SyncCursor>,
        persist: &mut dyn FnMut(MailboxDelta) -> Result<()>,
    ) -> Result<()> {
        crate::imap::fetch_imap(&self.account_id, &self.imap, cursor, persist)
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

/// Any IMAP account, including one that is not Proton Mail Bridge.
///
/// Read-only. `confirm_send` does not transmit.
#[derive(Debug, Clone)]
pub struct ImapBackend {
    pub account_id: AccountId,
    pub config: ImapConfig,
}

impl MailBackend for ImapBackend {
    fn kind(&self) -> BackendKind {
        BackendKind::Imap
    }

    fn fetch_changes(&self, cursor: Option<&SyncCursor>) -> Result<MailboxDelta> {
        collect_imap(&self.account_id, &self.config, cursor)
    }

    fn sync_changes(
        &self,
        cursor: Option<&SyncCursor>,
        persist: &mut dyn FnMut(MailboxDelta) -> Result<()>,
    ) -> Result<()> {
        crate::imap::fetch_imap(&self.account_id, &self.config, cursor, persist)
    }

    fn submit_draft(&self, draft: Draft) -> Result<PendingSend> {
        Ok(PendingSend::new("imap", draft))
    }

    fn confirm_send(
        &self,
        pending: PendingSend,
        confirmation: UserSendConfirmation,
    ) -> Result<SentMessage> {
        require_confirmation(&pending, &confirmation)?;
        Err(Error::NotImplemented("IMAP send"))
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

/// Backend for this account. The in-process fixture is only the local demo account.
pub fn backend_for(account_id: &AccountId) -> Box<dyn MailBackend + Send> {
    match account_id.as_str() {
        "local" | "fixture" | "demo" => Box::new(FixtureBackend::default()),
        _ => Box::new(BridgeBackend {
            account_id: account_id.clone(),
            ..BridgeBackend::default()
        }),
    }
}

fn collect_imap(
    account_id: &AccountId,
    config: &ImapConfig,
    cursor: Option<&SyncCursor>,
) -> Result<MailboxDelta> {
    let mut combined = MailboxDelta::default();
    crate::imap::fetch_imap(account_id, config, cursor, |mut delta| {
        combined.messages.append(&mut delta.messages);
        combined.flags.append(&mut delta.flags);
        if delta.cursor.is_some() {
            combined.cursor = delta.cursor;
        }
        Ok(())
    })?;
    Ok(combined)
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
            flags: Vec::new(),
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
    use crate::imap::ImapSecurity;

    #[test]
    fn bridge_fetch_needs_credentials() {
        let backend = BridgeBackend::default();
        assert_eq!(backend.kind(), BackendKind::Bridge);
        assert_eq!(backend.imap.security, ImapSecurity::StartTls);
        assert_eq!(backend.imap.port, 1143);
        let err = backend.fetch_changes(None).unwrap_err();
        assert!(matches!(err, Error::Backend(message) if message.contains("username")));
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
                attachments_digest: String::new(),
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
