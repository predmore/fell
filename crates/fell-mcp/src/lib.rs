//! MCP server for Fell.
//!
//! Tools: search, read threads, list labels, triage (label, archive, mark
//! read, snooze), and drafts saved for review. Scopes (read / triage / draft)
//! stay off until a person enables them. Triage and drafts are logged and
//! undoable.
//!
//! There is **no send tool**. Sending mail requires
//! [`fell_core::UserSendConfirmation`] minted by the GPUI app via
//! `fell_confirm::InteractiveUi`. This crate must not depend on
//! `fell-confirm`.
//!
//! ```compile_fail
//! use fell_core::{AccountId, Draft, PendingSend, UserSendConfirmation};
//! let pending = PendingSend::new(
//!     "x",
//!     Draft {
//!         account_id: AccountId("a".into()),
//!         to: vec![],
//!         cc: vec![],
//!         bcc: vec![],
//!         subject: String::new(),
//!         body: String::new(),
//!         in_reply_to: None,
//!     },
//! );
//! let _ = UserSendConfirmation::from_interactive_ui(&Default::default(), &pending);
//! ```

#![forbid(unsafe_code)]

mod cli;
mod protocol;
mod tools;

pub use cli::cli_main;
pub use protocol::Session;

#[cfg(test)]
mod tests {
    #[test]
    fn mcp_sources_do_not_mint_send_tokens() {
        const FILES: &[&str] = &[
            include_str!("../Cargo.toml"),
            include_str!("cli.rs"),
            include_str!("tools.rs"),
            include_str!("protocol.rs"),
            include_str!("main.rs"),
        ];
        const BANNED: &[&str] = &[
            "fell_confirm",
            "fell-confirm",
            "for_fell_app",
            "InteractiveUi",
            "from_interactive_ui",
            "UserSendConfirmation",
        ];
        for file in FILES {
            for word in BANNED {
                assert!(!file.contains(word), "{word} appeared in an MCP source");
            }
        }
    }
}
