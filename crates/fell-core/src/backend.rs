use crate::account::{AccountId, BackendKind};
use crate::account_config::AccountConfig;
use crate::imap::ImapConfig;
use crate::message::Message;
use crate::send::{
    Draft, PendingSend, SentMessage, SubmittedFile, UserSendConfirmation, require_confirmation,
};
use crate::smtp::SmtpConfig;
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

/// Server copies that one mailbox no longer holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRemoval {
    /// The mailbox as remote ids spell it (`imap:{uidvalidity}:{uid}:{mailbox}`).
    pub mailbox: String,
    /// The folder name copies from this mailbox add to a row.
    pub folder: String,
    pub kind: RemovalKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemovalKind {
    /// Every UID the mailbox held under `uidvalidity` when the pass began.
    /// A stored copy at or below `through` that is not listed is gone, and so
    /// is every copy stored under another UIDVALIDITY.
    Listing {
        uidvalidity: u32,
        /// Sorted.
        present: Vec<u32>,
        through: u32,
        /// Message-IDs of present copies the store may not know yet (mail
        /// synced before copies were recorded). Empty on later passes.
        found: Vec<(u32, crate::MessageId)>,
        /// Every present copy is now recorded, so a row that lists this
        /// folder with no copy here (and no local action) lost it earlier.
        sweep: bool,
    },
    /// UID ranges (inclusive) the server reported expunged (QRESYNC `VANISHED`).
    Vanished {
        uidvalidity: u32,
        ranges: Vec<(u32, u32)>,
    },
}

/// What the end of a sync pass does with folders whose last copy vanished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Settle {
    /// Not the end of a pass: leave them for later.
    #[default]
    Later,
    /// Take the folder off rows that keep another folder. Rows with nothing
    /// left wait, because a mailbox this pass could not read may hold the
    /// copy a server move made.
    KeepRows,
    /// Every mailbox was read: also delete rows with no folder and no copy.
    All,
}

/// The bytes of one server copy, fetched again to fill its stored parts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedBody {
    pub remote_id: crate::RemoteId,
    /// The RFC 822 message. `None` when the server no longer has this copy
    /// (expunged, too large, a refused mailbox, or a new UIDVALIDITY).
    pub raw: Option<Vec<u8>>,
}

/// Which server copies the store holds already, so sync never downloads a
/// body it has. Called with a mailbox as remote ids spell it
/// (`imap:{uidvalidity}:{uid}:{mailbox}`), a UIDVALIDITY, and an inclusive
/// UID range; answers the stored UIDs in that range, ascending.
pub type KnownCopies<'a> = dyn Fn(&str, u32, (u32, u32)) -> Result<Vec<u32>> + 'a;

/// For a one-shot fetch with no store: every copy found gets its body fetched.
fn no_known_copies(_mailbox: &str, _uidvalidity: u32, _range: (u32, u32)) -> Result<Vec<u32>> {
    Ok(Vec::new())
}

/// Messages, flag updates, and vanished copies to apply to the local store.
///
/// Removals drop server copies only. The folders they backed come off the
/// rows at [`Settle`], so mail moved on the server keeps its row once the
/// copy in the new folder has arrived, whichever mailbox syncs first.
#[derive(Debug, Clone, Default)]
pub struct MailboxDelta {
    pub messages: Vec<Message>,
    pub flags: Vec<FlagUpdate>,
    pub cursor: Option<SyncCursor>,
    pub removals: Vec<FolderRemoval>,
    pub settle: Settle,
    /// Each role's folder, when this pass listed the server's mailboxes.
    /// Replaces the account's stored roles.
    pub roles: Option<crate::folders::FolderRoles>,
}

/// Mail transport. IMAP/Proton live behind this trait; the UI never calls them directly.
pub trait MailBackend {
    fn kind(&self) -> BackendKind;

    fn fetch_changes(&self, cursor: Option<&SyncCursor>) -> Result<MailboxDelta>;

    /// Block until the server reports new mail, `max` passes, or `stop`
    /// returns true (checked several times a second). Backends without push
    /// return [`crate::imap::InboxWait::Unsupported`] at once.
    fn wait_for_change(
        &self,
        _cursor: Option<&SyncCursor>,
        _max: std::time::Duration,
        _stop: &mut dyn FnMut() -> bool,
    ) -> Result<crate::imap::InboxWait> {
        Ok(crate::imap::InboxWait::Unsupported)
    }

    /// Persist each bounded delta as it is produced. The default fetches once.
    ///
    /// `known` names the copies the store holds; their bodies are not fetched
    /// again, only their flags.
    fn sync_changes(
        &self,
        cursor: Option<&SyncCursor>,
        _known: &KnownCopies<'_>,
        persist: &mut dyn FnMut(MailboxDelta) -> Result<()>,
    ) -> Result<()> {
        persist(self.fetch_changes(cursor)?)
    }

    /// Fetch stored copies again by remote id, for body parts the store does
    /// not have yet. Read-only: nothing is marked read. A copy the server
    /// answered without comes back with `raw: None`; one not fetched this
    /// time (a byte cap) is left out and asked for again later. A refused or
    /// cut-short fetch is an error. Backends with nothing to fetch return none.
    fn fetch_bodies(&self, _copies: &[crate::RemoteId]) -> Result<Vec<FetchedBody>> {
        Ok(Vec::new())
    }

    /// Fetch one MIME part of a stored copy by its IMAP section, still
    /// transfer-encoded. `encoded_size` is its listed size on the wire
    /// ([`crate::MimePart::encoded_size`]), so a fetch knows when it has it
    /// all. Read-only: nothing is marked read. `Ok(None)` when the server
    /// no longer has the copy. `progress` hears the bytes read so far; when
    /// it answers `Break`, the fetch closes its session and fails. Backends
    /// with nothing to fetch return `None`.
    fn fetch_part(
        &self,
        _copy: &crate::RemoteId,
        _section: &str,
        _encoded_size: u64,
        _progress: &mut dyn FnMut(u64) -> std::ops::ControlFlow<()>,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
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
/// `confirm_send` submits [`PendingSend::mime`] to [`BridgeBackend::smtp`]
/// with this account's IMAP username and password. Bridge's usual SMTP port
/// is 1025 and offers STARTTLS. Bcc is an envelope recipient only.
#[derive(Debug, Clone)]
pub struct BridgeBackend {
    pub account_id: AccountId,
    pub imap: ImapConfig,
    pub smtp: SmtpConfig,
}

impl Default for BridgeBackend {
    fn default() -> Self {
        Self {
            account_id: AccountId("bridge".into()),
            imap: ImapConfig::bridge_local("", ""),
            smtp: SmtpConfig::bridge_local(),
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

    fn wait_for_change(
        &self,
        cursor: Option<&SyncCursor>,
        max: std::time::Duration,
        stop: &mut dyn FnMut() -> bool,
    ) -> Result<crate::imap::InboxWait> {
        crate::imap::wait_for_inbox(&self.imap, cursor, max, stop)
    }

    fn sync_changes(
        &self,
        cursor: Option<&SyncCursor>,
        known: &KnownCopies<'_>,
        persist: &mut dyn FnMut(MailboxDelta) -> Result<()>,
    ) -> Result<()> {
        crate::imap::fetch_imap(&self.account_id, &self.imap, cursor, known, persist)
    }

    fn fetch_bodies(&self, copies: &[crate::RemoteId]) -> Result<Vec<FetchedBody>> {
        crate::imap::fetch_bodies(&self.imap, copies)
    }

    fn fetch_part(
        &self,
        copy: &crate::RemoteId,
        section: &str,
        encoded_size: u64,
        progress: &mut dyn FnMut(u64) -> std::ops::ControlFlow<()>,
    ) -> Result<Option<Vec<u8>>> {
        crate::imap::fetch_part(&self.imap, copy, section, encoded_size, progress)
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
        let remote = crate::smtp::deliver(
            &crate::smtp::Endpoint::new(&self.smtp, &self.imap.username, &self.imap.password),
            &pending,
        )?;
        Ok(SentMessage {
            pending_id: pending.id,
            remote_id: crate::RemoteId(remote),
        })
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

    fn wait_for_change(
        &self,
        cursor: Option<&SyncCursor>,
        max: std::time::Duration,
        stop: &mut dyn FnMut() -> bool,
    ) -> Result<crate::imap::InboxWait> {
        crate::imap::wait_for_inbox(&self.config, cursor, max, stop)
    }

    fn sync_changes(
        &self,
        cursor: Option<&SyncCursor>,
        known: &KnownCopies<'_>,
        persist: &mut dyn FnMut(MailboxDelta) -> Result<()>,
    ) -> Result<()> {
        crate::imap::fetch_imap(&self.account_id, &self.config, cursor, known, persist)
    }

    fn fetch_bodies(&self, copies: &[crate::RemoteId]) -> Result<Vec<FetchedBody>> {
        crate::imap::fetch_bodies(&self.config, copies)
    }

    fn fetch_part(
        &self,
        copy: &crate::RemoteId,
        section: &str,
        encoded_size: u64,
        progress: &mut dyn FnMut(u64) -> std::ops::ControlFlow<()>,
    ) -> Result<Option<Vec<u8>>> {
        crate::imap::fetch_part(&self.config, copy, section, encoded_size, progress)
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

/// Ids served by the in-process demo backend. An account file may not use them.
pub(crate) const DEMO_ACCOUNT_IDS: [&str; 3] = ["local", "fixture", "demo"];

/// The demo mailbox, served in process with no network and no credentials.
pub fn is_demo_account(account_id: &AccountId) -> bool {
    DEMO_ACCOUNT_IDS.contains(&account_id.as_str())
}

/// Backend for this account. The in-process fixture is only the local demo
/// account. Any other id needs the user's account file (`config`) with the
/// same id: its credentials and SMTP settings are the only ones used, so a
/// draft can never supply them.
///
/// Resolves `password_command`, so call it off the UI thread.
pub fn backend_for(
    account_id: &AccountId,
    config: Option<&AccountConfig>,
) -> Result<Box<dyn MailBackend + Send>> {
    if is_demo_account(account_id) {
        return Ok(Box::new(FixtureBackend::default()));
    }
    match config {
        Some(config) if config.id == account_id.as_str() => config.backend(),
        _ => Err(Error::UnknownAccount(format!(
            "{}: no account file entry for it; run fell account setup",
            account_id.as_str()
        ))),
    }
}

fn collect_imap(
    account_id: &AccountId,
    config: &ImapConfig,
    cursor: Option<&SyncCursor>,
) -> Result<MailboxDelta> {
    let mut combined = MailboxDelta::default();
    crate::imap::fetch_imap(account_id, config, cursor, &no_known_copies, |mut delta| {
        combined.messages.append(&mut delta.messages);
        combined.flags.append(&mut delta.flags);
        combined.removals.append(&mut delta.removals);
        if delta.settle != Settle::Later {
            combined.settle = delta.settle;
        }
        if delta.cursor.is_some() {
            combined.cursor = delta.cursor;
        }
        if delta.roles.is_some() {
            combined.roles = delta.roles;
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
            ..MailboxDelta::default()
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
    fn backend_for_takes_credentials_only_from_the_matching_account_file() {
        use crate::account_config::PasswordSource;

        assert_eq!(
            backend_for(&AccountId("local".into()), None)
                .unwrap()
                .kind(),
            BackendKind::Bridge
        );
        for (id, config) in [("mail", None), ("other", Some("mail"))] {
            let file = config
                .map(|_| AccountConfig::bridge("a@b", "a@b", PasswordSource::Inline("pw".into())));
            let err = backend_for(&AccountId(id.into()), file.as_ref())
                .err()
                .expect("no backend without a matching account file");
            assert!(matches!(err, Error::UnknownAccount(_)), "{err}");
        }
        let file = AccountConfig::bridge(
            "a@b",
            "a@b",
            PasswordSource::Command("printf 'from-command'".into()),
        );
        assert!(backend_for(&AccountId("mail".into()), Some(&file)).is_ok());
        let bridge = file.bridge_backend().unwrap();
        assert_eq!(bridge.imap.username, "a@b");
        assert_eq!(bridge.imap.password, "from-command");
        assert_eq!(bridge.smtp, SmtpConfig::bridge_local());
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
