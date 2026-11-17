//! GPUI app crate. Send confirmation is minted here, not in `fell-mcp`.

mod fixtures;
mod keymap;
mod mailbox;
mod model;
mod present;
mod text_render;
mod theme;
mod view;

use fell_confirm::InteractiveUi;
use fell_core::{PendingSend, UserSendConfirmation};

/// Open the inbox window.
pub fn run() {
    view::run();
}

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
            })
            .unwrap();
        let confirm = confirmation_from_ui(&pending);
        let sent = backend.confirm_send(pending, confirm).unwrap();
        assert_eq!(sent.remote_id.0, "fixture-sent");
    }
}
