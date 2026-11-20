//! GPUI app crate. Send confirmation is minted here, not in `fell-mcp`.
//!
//! [`render`] paints one message. The inbox shell embeds that trait; this
//! crate does not own the thread list, keymap, or command palette.

pub mod render;

mod attachments;
mod compose_ui;
pub mod draft_host;
mod embeddings;
mod fixtures;
mod frame_log;
mod inbox_motion;
mod keymap;
mod mail_body;
mod mailbox;
mod model;
mod motion;
mod png;
mod present;
mod reply_check;
mod row_widths;
mod scroll_anchor;
mod setup;
mod store_actor;
mod sync_worker;
mod text_cut;
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
pub use embeddings::embeddings_command;
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
        /// The message bytes of the latest transmission.
        mime: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl Counting {
        fn new(answer: fn() -> fell_core::Result<()>) -> Self {
            Self {
                sends: Default::default(),
                answer,
                mime: Default::default(),
            }
        }

        /// The `Message-ID` of the latest transmission.
        fn message_id(&self) -> fell_core::MessageId {
            let mime = self.mime.lock().unwrap();
            let text = String::from_utf8_lossy(&mime);
            let line = text
                .lines()
                .find_map(|line| line.strip_prefix("Message-ID:"))
                .expect("Message-ID header");
            fell_core::MessageId::new(line)
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
            *self.mime.lock().unwrap() = pending.mime.clone();
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

    /// The account file's account, and a draft `c` started for it in the
    /// inbox, addressed and written as the person would in the editor.
    fn inbox_draft(
        tag: &str,
    ) -> (
        fell_core::AccountConfig,
        crate::draft_host::DraftHome,
        std::sync::Arc<fell_core::compose::DraftHub>,
        String,
    ) {
        let config = fell_core::AccountConfig::bridge(
            "jon@example.com",
            "jon",
            fell_core::PasswordSource::Inline("bridge-password".into()),
        );
        let home = crate::draft_host::tests::temp_home(tag, &config.account());
        let hub = std::sync::Arc::new(fell_core::compose::DraftHub::new());
        let id = addressed_draft(&home, &hub, &config, "Lunch");
        (config, home, hub, id)
    }

    /// Another draft `c` started in `home`, shown in the window, addressed
    /// and titled `subject`.
    fn addressed_draft(
        home: &crate::draft_host::DraftHome,
        hub: &fell_core::compose::DraftHub,
        config: &fell_core::AccountConfig,
        subject: &str,
    ) -> String {
        let id = home
            .new_draft(hub, &AccountId(config.id.clone()), &config.email)
            .unwrap();
        assert!(crate::draft_host::show_draft(hub, &id));
        hub.with_session(&id, |session| {
            session
                .set_addresses_from_user("to", "Ada <ada@example.com>")
                .unwrap();
            session.set_subject_from_user(subject.into());
            session.set_body_from_user("Thursday?\n".into());
            session.flush().unwrap();
            session.mark_seen();
        });
        id
    }

    #[test]
    fn a_send_that_lands_after_the_window_moved_on_leaves_the_new_draft_open() {
        let (config, home, hub, sent) = inbox_draft("moved-on");
        let backend = Counting::new(|| Ok(()));
        let asked = std::sync::Mutex::new(Vec::new());
        let prepared = hub
            .with_session(&sent, crate::compose_ui::prepare_from_editor)
            .unwrap()
            .unwrap();
        // `c` while the send is still talking to the server: the one window
        // moves to the new draft.
        hub.note_shown(&sent);
        let other = addressed_draft(&home, &hub, &config, "Dinner");
        hub.note_hidden(&sent);
        hub.note_shown(&other);
        let _ = hub.take_closed();
        let counting = backend.clone();
        let outcome = crate::compose_ui::transmit_from_hub(&hub, &sent, prepared, |account| {
            asked.lock().unwrap().push(account.clone());
            Ok(Box::new(counting))
        });
        assert!(outcome.is_ok());
        let end = crate::compose_ui::finish_send(&hub, &sent, &other, true, &outcome);
        assert_eq!(end.status, None, "the draft on screen keeps its status");
        assert!(!end.close, "the draft on screen is not closed");
        let open = hub
            .with_session(&other, |session| session.snapshot().open)
            .unwrap();
        assert!(open, "the new draft is still open for editing");
        // The inbox still hears that the first one went out.
        let lines = hub.take_closed();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Sent"), "{lines:?}");
        assert!(lines[0].contains("Lunch"), "{lines:?}");
        // On the draft that was sent, the window closes as before.
        let backend = Counting::new(|| Ok(()));
        let outcome = send_as_window(&hub, &other, &backend, &asked);
        let end = crate::compose_ui::finish_send(&hub, &other, &other, true, &outcome);
        assert!(end.close);
        assert_eq!(end.status.as_deref(), Some("Sent from the editor"));
        assert!(hub.take_closed()[0].contains("Dinner"));
    }

    #[test]
    fn a_maybe_sent_result_for_a_draft_no_longer_on_screen_goes_to_the_inbox() {
        let (config, home, hub, sent) = inbox_draft("moved-on-uncertain");
        let backend = Counting::new(|| {
            Err(fell_core::Error::SendUncertain(
                "smtp: read timed out".into(),
            ))
        });
        let asked = std::sync::Mutex::new(Vec::new());
        let other = addressed_draft(&home, &hub, &config, "Dinner");
        let _ = hub.take_closed();
        let outcome = send_as_window(&hub, &sent, &backend, &asked);
        let end = crate::compose_ui::finish_send(&hub, &sent, &other, true, &outcome);
        assert_eq!(end.status, None, "no error is painted on the other draft");
        assert!(!end.sent);
        let notices = hub.take_notices();
        let (line, warn) = &notices[0];
        assert!(line.starts_with("May have been sent"), "{notices:?}");
        assert!(line.contains("check Sent"), "{line}");
        assert!(line.contains("Lunch"), "{line}");
        assert!(*warn, "the inbox shows it in the error colour");
    }

    /// Send draft `id` as the window does, through `backend`, recording
    /// which account the window asked a backend for.
    fn send_as_window(
        hub: &fell_core::compose::DraftHub,
        id: &str,
        backend: &Counting,
        asked: &std::sync::Mutex<Vec<AccountId>>,
    ) -> Result<fell_core::SentMessage, SendFailure> {
        let prepared = hub
            .with_session(id, crate::compose_ui::prepare_from_editor)
            .unwrap()?;
        let counting = backend.clone();
        crate::compose_ui::transmit_from_hub(hub, id, prepared, move |account| {
            asked.lock().unwrap().push(account.clone());
            Ok(Box::new(counting))
        })
    }

    #[test]
    fn a_draft_from_the_inbox_sends_once_through_the_account_file() {
        let (config, home, hub, id) = inbox_draft("inbox-send");
        let account = AccountId(config.id.clone());
        // The window resolves its backend from the account file for this
        // account; there is no fixture to fall back on.
        assert!(!fell_core::is_demo_account(&account));
        assert!(fell_core::backend_for(&account, None).is_err());
        let mut imap = config.clone();
        imap.backend = fell_core::BackendKind::Imap;
        assert_eq!(
            fell_core::backend_for(&account, Some(&imap))
                .unwrap()
                .kind(),
            fell_core::BackendKind::Imap
        );
        let backend = Counting::new(|| Ok(()));
        let asked = std::sync::Mutex::new(Vec::new());
        let sent = send_as_window(&hub, &id, &backend, &asked).unwrap();
        assert_eq!(sent.remote_id.0, "counted");
        assert_eq!(backend.count(), 1);
        assert_eq!(*asked.lock().unwrap(), vec![account.clone()]);
        let again = send_as_window(&hub, &id, &backend, &asked).unwrap_err();
        assert_eq!(
            again,
            SendFailure::Locked(fell_core::compose::SendLock::Sent)
        );
        assert_eq!(backend.count(), 1);
        // The sent draft leaves the drafts list.
        let listed = fell_core::Store::open(&home.store)
            .unwrap()
            .unsent_compose_drafts(&account)
            .unwrap();
        assert!(listed.iter().all(|draft| draft.id != id), "{listed:?}");
        // Closing its window tells the inbox it was sent.
        hub.note_shown(&id);
        hub.note_hidden(&id);
        let lines = hub.take_closed();
        assert!(lines[0].starts_with("Sent"), "{lines:?}");
        assert!(lines[0].contains("Lunch"), "{lines:?}");
        assert!(
            !hub.reopen_last(),
            "mail that went out is not offered again"
        );
    }

    #[test]
    fn a_draft_that_may_have_been_sent_stays_listed_with_check_sent() {
        let (config, home, hub, id) = inbox_draft("inbox-uncertain");
        let account = AccountId(config.id.clone());
        let backend = Counting::new(|| {
            Err(fell_core::Error::SendUncertain(
                "smtp: read timed out".into(),
            ))
        });
        let asked = std::sync::Mutex::new(Vec::new());
        let err = send_as_window(&hub, &id, &backend, &asked).unwrap_err();
        assert!(err.to_string().contains("Check Sent"), "{err}");
        let listed = fell_core::Store::open(&home.store)
            .unwrap()
            .unsent_compose_drafts(&account)
            .unwrap();
        let kept = listed.iter().find(|draft| draft.id == id).expect("kept");
        assert!(kept.maybe_sent);
        let row = &crate::present::draft_rows(&listed)[0];
        assert!(row.labels[0].contains("check Sent"), "{:?}", row.labels);
        hub.note_shown(&id);
        hub.note_hidden(&id);
        assert!(hub.take_closed()[0].contains("check Sent"));
    }

    /// A draft with a Bcc, bound to a mailbox file the test can read.
    fn bcc_session(name: &str) -> (fell_core::compose::DraftSession, fell_core::Store) {
        let root = std::env::temp_dir().join(format!(
            "fell-editor-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("mail.db");
        let store = fell_core::Store::open(&db).unwrap();
        store
            .upsert_account(&fell_core::Account {
                id: AccountId("mailbox".into()),
                display_name: "Jon".into(),
                email: "jon@example.com".into(),
                backend_kind: fell_core::BackendKind::Bridge,
            })
            .unwrap();
        let fields = fell_core::compose::DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            bcc: vec!["Hidden Person <hidden@example.com>".into()],
            subject: "Hello".into(),
            ..fell_core::compose::DraftFields::default()
        };
        let folder = fell_core::compose::materialize_in(
            &root.join("drafts"),
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
        session.bind_store(&db);
        session.mark_seen();
        (session, store)
    }

    /// The copy the server files in Sent for `id`: no Bcc header.
    fn sent_copy(id: fell_core::MessageId) -> fell_core::Message {
        fell_core::Message {
            account_id: AccountId("mailbox".into()),
            headers: fell_core::MessageHeaders {
                message_id: id.clone(),
                proton_conversation_id: None,
                in_reply_to: None,
                references: Vec::new(),
                from: fell_core::Address {
                    name: None,
                    email: "jon@example.com".into(),
                },
                subject: "Hello".into(),
                date_unix: 1,
            },
            remote_id: fell_core::RemoteId("sent-1".into()),
            thread_id: fell_core::ThreadId::from_message_id(&id),
            snippet: "body".into(),
            body_text: Some("body".into()),
            unread: false,
            folders: vec!["Sent".into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: vec![fell_core::Address {
                name: None,
                email: "ada@example.com".into(),
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
        }
    }

    #[test]
    fn a_sent_message_keeps_its_bcc_for_the_sent_copy() {
        let (mut session, store) = bcc_session("bcc-sent");
        let backend = Counting::new(|| Ok(()));
        send_from_editor(&mut session, &backend).unwrap();
        let mime = String::from_utf8(backend.mime.lock().unwrap().clone()).unwrap();
        assert!(!mime.to_ascii_lowercase().contains("hidden@"), "{mime}");
        let id = backend.message_id();
        let thread = store.upsert_message(&sent_copy(id.clone())).unwrap();
        let messages = shown_once_recorded(&store, &thread);
        assert_eq!(messages[0].bcc[0].email, "hidden@example.com");
        assert_eq!(messages[0].bcc[0].name.as_deref(), Some("Hidden Person"));
    }

    #[test]
    fn a_message_that_may_have_been_sent_keeps_its_bcc() {
        let (mut session, store) = bcc_session("bcc-uncertain");
        let backend = Counting::new(|| {
            Err(fell_core::Error::SendUncertain(
                "smtp: read timed out".into(),
            ))
        });
        send_from_editor(&mut session, &backend).unwrap_err();
        let thread = store
            .upsert_message(&sent_copy(backend.message_id()))
            .unwrap();
        let messages = shown_once_recorded(&store, &thread);
        assert_eq!(messages[0].bcc.len(), 1);
    }

    #[test]
    fn a_message_that_was_not_sent_leaves_no_bcc_record() {
        let (mut session, store) = bcc_session("bcc-refused");
        let backend =
            Counting::new(|| Err(fell_core::Error::Backend("smtp: RCPT rejected".into())));
        send_from_editor(&mut session, &backend).unwrap_err();
        let thread = store
            .upsert_message(&sent_copy(backend.message_id()))
            .unwrap();
        // The record is written off the send path; give a stray one time.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let messages = store
            .thread_messages(&AccountId("mailbox".into()), &thread)
            .unwrap();
        assert!(messages[0].bcc.is_empty());
    }

    /// The thread once the background Bcc record has landed, or after 5 s.
    fn shown_once_recorded(
        store: &fell_core::Store,
        thread: &fell_core::ThreadId,
    ) -> Vec<fell_core::Message> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let messages = store
                .thread_messages(&AccountId("mailbox".into()), thread)
                .unwrap();
            if !messages[0].bcc.is_empty() || std::time::Instant::now() > deadline {
                return messages;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
