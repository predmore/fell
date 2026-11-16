//! MCP server for Fell.
//!
//! Tools: search, read threads, list labels, triage (label, archive, mark
//! read, snooze), and drafts saved for review. Scopes (read / triage / draft)
//! stay off until a person enables them. Triage and drafts are logged and
//! undoable.
//!
//! There is **no send tool**. This process speaks stdio only. It must not
//! depend on the UI confirmation crate, and it cannot mint a send token.
//! The core crate's compile-fail test shows a confirmation cannot be minted
//! without the UI type.

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
            include_str!("lib.rs"),
            include_str!("cli.rs"),
            include_str!("tools.rs"),
            include_str!("protocol.rs"),
            include_str!("main.rs"),
        ];
        let banned = [
            format!("{}{}", "fell_", "confirm"),
            format!("{}{}", "fell-", "confirm"),
            format!("{}{}", "for_", "fell_app"),
            format!("{}{}", "Interactive", "Ui"),
            format!("{}{}", "from_interactive", "_ui"),
            format!("{}{}", "UserSend", "Confirmation"),
        ];
        for file in FILES {
            for word in &banned {
                assert!(!file.contains(word), "{word} appeared in an MCP source");
            }
        }
        let manifest = include_str!("../Cargo.toml");
        let confirm_dep = format!("{}{}", "fell-", "confirm");
        let confirm_mod = format!("{}{}", "fell_", "confirm");
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with('#') {
                continue;
            }
            assert!(
                !trimmed.contains(&confirm_dep) && !trimmed.contains(&confirm_mod),
                "fell-mcp must not depend on the UI confirmation crate"
            );
        }
    }
}
