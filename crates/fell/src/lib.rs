//! GPUI app crate. Send confirmation is minted here, not in `fell-mcp`.
//!
//! [`render`] paints one message. The inbox shell embeds that trait; this
//! crate does not own the thread list, keymap, or command palette.

pub mod render;

mod compose_ui;
mod fixtures;
mod keymap;
mod mailbox;
mod model;
mod png;
mod present;
mod setup;
mod store_actor;
mod sync_worker;
mod text_render;
mod theme;
mod view;

use fell_confirm::InteractiveUi;
use fell_core::{PendingSend, UserSendConfirmation};

/// Open the inbox window.
pub fn run() {
    view::run();
}

pub use compose_ui::{ComposeView, SendFailure, ThemeKind, open_demo, send_from_editor};
pub use setup::account_command;

/// Proof of an explicit Send click in the Fell window.
///
/// Bound to this pending send's id, account, recipients, subject, body,
/// reply parent, attachments, and a hash of the message bytes.
/// `fell-mcp` does not depend on `fell-confirm` and cannot call this. There
/// is no send MCP tool.
pub fn confirmation_from_ui(pending: &PendingSend) -> UserSendConfirmation {
    UserSendConfirmation::from_interactive_ui(&InteractiveUi::for_fell_app(), pending)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::{AccountId, Draft, FixtureBackend, MailBackend};

    #[test]
    fn app_crate_mints_send_confirmation() {
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
        let confirm = confirmation_from_ui(&pending);
        let sent = backend.confirm_send(pending, confirm).unwrap();
        assert_eq!(sent.remote_id.0, "fixture-sent");
    }

    #[test]
    fn only_the_editor_send_action_transmits() {
        let root = std::env::temp_dir().join(format!(
            "fell-editor-send-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let (hub, id, _) = open_demo(root, ThemeKind::Dark).unwrap();
        hub.with_session(&id, |session| {
            assert!(!session.request_send());
            let backend = fell_core::backend_for(session.account_id(), None).unwrap();
            let blocked = send_from_editor(session, backend.as_ref()).unwrap_err();
            assert!(blocked.to_string().contains("on screen"), "{blocked}");
            session.mark_seen();
            // The chart on screen is hashed off the editor's thread first;
            // until then send asks to try again, as a person would.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let sent = loop {
                match send_from_editor(session, backend.as_ref()) {
                    Err(err) if err.to_string().contains("still checking") => {
                        assert!(std::time::Instant::now() < deadline, "{err}");
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                    other => break other.unwrap(),
                }
            };
            assert_eq!(sent.remote_id.0, "fixture-sent");
        })
        .unwrap();
    }

    fn real_session(name: &str) -> fell_core::compose::DraftSession {
        let root = std::env::temp_dir().join(format!(
            "fell-editor-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let fields = fell_core::compose::DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hello".into(),
            ..fell_core::compose::DraftFields::default()
        };
        let folder = fell_core::compose::materialize_in(
            &root,
            name,
            &fields,
            "body\n",
            &AccountId("mailbox".into()),
        )
        .unwrap();
        let mut session = fell_core::compose::DraftSession::create(
            folder,
            name,
            AccountId("mailbox".into()),
            fields,
            "body\n",
        )
        .unwrap();
        // Sends are recorded in the mailbox before they start.
        let db = root.join("mail.db");
        fell_core::Store::open(&db)
            .unwrap()
            .upsert_account(&fell_core::Account {
                id: AccountId("mailbox".into()),
                display_name: "Jon".into(),
                email: "jon@example.com".into(),
                backend_kind: fell_core::BackendKind::Bridge,
            })
            .unwrap();
        session.bind_store(&db);
        session.flush().unwrap();
        session.mark_seen();
        session
    }

    fn mailbox_of(session: &fell_core::compose::DraftSession) -> std::path::PathBuf {
        session.folder().parent().unwrap().join("mail.db")
    }

    #[test]
    fn a_real_account_without_an_account_file_has_no_backend() {
        let session = real_session("real");
        let err = fell_core::backend_for(session.account_id(), None)
            .err()
            .expect("no credentials, no backend");
        assert!(err.to_string().contains("account setup"), "{err}");
    }

    /// Counts transmissions and answers each with the next scripted result.
    #[derive(Clone)]
    struct Counting {
        sends: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        answer: fn() -> fell_core::Result<()>,
    }

    impl Counting {
        fn new(answer: fn() -> fell_core::Result<()>) -> Self {
            Self {
                sends: Default::default(),
                answer,
            }
        }

        fn count(&self) -> usize {
            self.sends.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl MailBackend for Counting {
        fn kind(&self) -> fell_core::BackendKind {
            fell_core::BackendKind::Bridge
        }

        fn fetch_changes(
            &self,
            _cursor: Option<&fell_core::SyncCursor>,
        ) -> fell_core::Result<fell_core::MailboxDelta> {
            Ok(fell_core::MailboxDelta::default())
        }

        fn submit_draft(&self, draft: Draft) -> fell_core::Result<PendingSend> {
            Ok(PendingSend::new("count", draft))
        }

        fn confirm_send(
            &self,
            pending: PendingSend,
            confirmation: UserSendConfirmation,
        ) -> fell_core::Result<fell_core::SentMessage> {
            fell_core::send::require_confirmation(&pending, &confirmation)?;
            self.sends.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            (self.answer)()?;
            Ok(fell_core::SentMessage {
                pending_id: pending.id,
                remote_id: fell_core::RemoteId("counted".into()),
            })
        }
    }

    #[test]
    fn a_second_send_after_success_does_not_transmit() {
        let mut session = real_session("twice");
        let backend = Counting::new(|| Ok(()));
        send_from_editor(&mut session, &backend).unwrap();
        let again = send_from_editor(&mut session, &backend).unwrap_err();
        assert_eq!(
            again,
            SendFailure::Locked(fell_core::compose::SendLock::Sent)
        );
        assert_eq!(backend.count(), 1);
        assert!(session.snapshot().send_block.is_some());
    }

    #[test]
    fn a_failure_after_data_may_have_been_sent_and_locks_the_draft() {
        let mut session = real_session("uncertain");
        let backend = Counting::new(|| {
            Err(fell_core::Error::SendUncertain(
                "smtp: read timed out".into(),
            ))
        });
        let err = send_from_editor(&mut session, &backend).unwrap_err();
        assert!(matches!(err, SendFailure::MaybeSent(_)), "{err:?}");
        let shown = err.to_string();
        assert!(shown.starts_with("May have been sent"), "{shown}");
        assert!(!shown.contains("Not sent"), "{shown}");
        let again = send_from_editor(&mut session, &backend).unwrap_err();
        assert_eq!(
            again,
            SendFailure::Locked(fell_core::compose::SendLock::MaybeSent)
        );
        assert_eq!(backend.count(), 1);
    }

    #[test]
    fn a_failure_before_data_is_not_sent_and_can_be_retried() {
        let mut session = real_session("retry");
        let backend =
            Counting::new(|| Err(fell_core::Error::Backend("smtp: RCPT rejected".into())));
        let err = send_from_editor(&mut session, &backend).unwrap_err();
        assert!(err.to_string().starts_with("Not sent"), "{err}");
        assert!(session.send_lock().is_none());
        let _ = send_from_editor(&mut session, &backend).unwrap_err();
        assert_eq!(backend.count(), 2);
    }

    #[test]
    fn a_claim_the_mailbox_cannot_record_blocks_the_send() {
        let mut session = real_session("unrecorded");
        // The mailbox refuses the write, as a full or read-only disk would.
        rusqlite::Connection::open(mailbox_of(&session))
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER refuse_claim BEFORE INSERT ON compose_send
                 BEGIN SELECT RAISE(FAIL, 'database or disk is full'); END;",
            )
            .unwrap();
        let backend = Counting::new(|| Ok(()));
        let err = send_from_editor(&mut session, &backend).unwrap_err();
        assert!(matches!(err, SendFailure::NotSent(_)), "{err:?}");
        let shown = err.to_string();
        assert!(shown.starts_with("Not sent"), "{shown}");
        assert!(shown.contains("could not record this send"), "{shown}");
        assert!(shown.contains("disk is full"), "{shown}");
        assert_eq!(backend.count(), 0);
        assert!(session.send_lock().is_none());
    }

    #[test]
    fn a_press_while_a_send_is_in_flight_does_not_transmit() {
        let mut session = real_session("inflight");
        let backend = Counting::new(|| Ok(()));
        let first = crate::compose_ui::prepare_from_editor(&mut session).unwrap();
        let second = send_from_editor(&mut session, &backend).unwrap_err();
        assert_eq!(
            second,
            SendFailure::Locked(fell_core::compose::SendLock::InFlight)
        );
        assert_eq!(backend.count(), 0);
        drop(first);
    }

    #[test]
    fn the_window_path_locks_the_draft_in_the_hub_after_a_send() {
        let hub = fell_core::compose::DraftHub::new();
        hub.insert(real_session("hub"));
        let backend = Counting::new(|| Ok(()));
        for press in 0..2 {
            let prepared = hub
                .with_session("hub", |session| {
                    session.mark_seen();
                    crate::compose_ui::prepare_from_editor(session)
                })
                .unwrap();
            match prepared {
                Ok(prepared) => {
                    assert_eq!(press, 0);
                    let counting = backend.clone();
                    crate::compose_ui::transmit_from_hub(&hub, "hub", prepared, move |_| {
                        Ok(Box::new(counting))
                    })
                    .unwrap();
                }
                Err(err) => {
                    assert_eq!(press, 1);
                    assert_eq!(err, SendFailure::Locked(fell_core::compose::SendLock::Sent));
                }
            }
        }
        assert_eq!(backend.count(), 1);
    }
}
