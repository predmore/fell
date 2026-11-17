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

pub use compose_ui::{ComposeView, ThemeKind, open_demo, send_from_editor};
pub use setup::account_command;

/// Proof of an explicit Send click in the Fell window.
///
/// Bound to this pending send's id, account, recipients, subject, and body.
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
            let backend = fell_core::backend_for(session.account_id());
            let blocked = send_from_editor(session, backend.as_ref()).unwrap_err();
            assert!(blocked.contains("on screen"), "{blocked}");
            session.mark_seen();
            // The chart on screen is hashed off the editor's thread first;
            // until then send asks to try again, as a person would.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let sent = loop {
                match send_from_editor(session, backend.as_ref()) {
                    Err(err) if err.contains("still checking") => {
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

    #[test]
    fn a_real_account_is_not_reported_as_sent() {
        let root = std::env::temp_dir().join(format!(
            "fell-editor-real-{}-{}",
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
            "real",
            &fields,
            "body\n",
            &AccountId("mailbox".into()),
        )
        .unwrap();
        let mut session = fell_core::compose::DraftSession::create(
            folder,
            "real",
            AccountId("mailbox".into()),
            fields,
            "body\n",
        )
        .unwrap();
        session.mark_seen();
        let backend = fell_core::backend_for(session.account_id());
        let err = send_from_editor(&mut session, backend.as_ref()).unwrap_err();
        assert!(
            err.contains("SMTP") || err.contains("not implemented"),
            "{err}"
        );
        assert!(!err.contains("fixture-sent"), "{err}");
    }
}
