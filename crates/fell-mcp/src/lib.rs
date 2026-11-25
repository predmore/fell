//! MCP server for Fell.
//!
//! Tools: search, read threads, list folders, threads and labels, triage
//! (label, move, archive, star, mark read or unread, snooze, unsnooze), and
//! drafts saved for review. Scopes (read / triage / draft)
//! stay off until a person enables them. Triage and drafts are logged and
//! undoable.
//!
//! There is **no send tool**. This process speaks stdio only. It must not
//! depend on the UI confirmation crate, and it cannot mint a send token.
//! The core crate's compile-fail test shows a confirmation cannot be minted
//! without the UI type.

#![forbid(unsafe_code)]

mod cli;
#[cfg(test)]
mod corpus;
mod protocol;
mod setup;
mod tools;
mod untrusted;

pub use cli::{FELL_MCP_ARG0, cli_main};
pub use protocol::Session;

/// Tests that spawn a child process take this for writing while they spawn;
/// tests that lock one file twice in a row hold it for reading.
///
/// A fork copies every open descriptor until the child execs, and a flock
/// belongs to the open file, so a test spawning a process at the wrong
/// moment can keep another test's config locked after Fell let go of it.
/// The second lock then fails with "another program is writing" (#266).
#[cfg(test)]
pub(crate) static SPAWN: std::sync::RwLock<()> = std::sync::RwLock::new(());

/// Spawn `command` while no test holds a file lock it could inherit.
#[cfg(test)]
pub(crate) fn spawn_alone(command: &mut std::process::Command) -> std::process::Child {
    let _alone = SPAWN
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    command.spawn().unwrap()
}

#[cfg(test)]
fn isolated_draft_paths() -> Option<std::path::PathBuf> {
    fell_core::test_support::in_isolated_process_with(spawn_alone)
}

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
            include_str!("setup.rs"),
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

    /// `test-utils` lets a caller rewrite the vector index and swap the
    /// search ranker. Only the tests may turn it on, never the binary.
    #[test]
    fn only_the_tests_enable_fell_core_test_utils() {
        let manifest: toml_edit::DocumentMut = include_str!("../Cargo.toml").parse().unwrap();
        let features = |table: &str| -> Vec<String> {
            manifest
                .get(table)
                .and_then(|deps| deps.get("fell-core"))
                .and_then(|dep| dep.get("features"))
                .and_then(|features| features.as_array())
                .map(|features| {
                    features
                        .iter()
                        .filter_map(|feature| feature.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        assert!(features("dependencies").is_empty());
        assert!(features("build-dependencies").is_empty());
        assert!(manifest.get("features").is_none());
        assert!(manifest.get("target").is_none());
        assert_eq!(features("dev-dependencies"), ["test-utils"]);
    }
}
