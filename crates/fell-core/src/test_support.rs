//! Shared test capabilities and performance fixtures.
//!
//! A few tests need a capability a developer sandbox may lack, such as the
//! TEST-NET-1 address on loopback. Such a test asks [`skip_unless_available`]
//! first. On a machine without the capability it prints why and is skipped. On CI
//! (`CI=true`, which GitHub Actions sets) or with `FELL_REQUIRE_ALL_TESTS=1`,
//! a missing capability fails the test instead, so nothing is silently
//! skipped where it must run.

#[cfg(any(test, feature = "test-utils"))]
pub mod mailbox;

#[cfg(any(test, feature = "test-utils"))]
use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
};

/// Run a test that uses the default draft APIs in its own process. Call from
/// the test's libtest thread. The parent waits for the exact test and returns
/// `None`. The child gets its private root and continues the test.
#[cfg(any(test, feature = "test-utils"))]
pub fn in_isolated_process() -> Option<PathBuf> {
    in_isolated_process_with(|command| command.spawn().unwrap())
}

/// Like [`in_isolated_process`], with a custom way to spawn the child.
/// `spawn` lets callers coordinate forks with other tests holding file locks.
#[cfg(any(test, feature = "test-utils"))]
pub fn in_isolated_process_with(spawn: impl FnOnce(&mut Command) -> Child) -> Option<PathBuf> {
    const ISOLATED_ROOT: &str = "FELL_TEST_ISOLATED_ROOT";
    if let Some(root) = std::env::var_os(ISOLATED_ROOT) {
        return Some(root.into());
    }

    let thread = std::thread::current();
    let test = thread.name().expect("expected a named libtest thread");
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let output = spawn(
        Command::new(std::env::current_exe().unwrap())
            .args([test, "--exact", "--test-threads=1"])
            .env(ISOLATED_ROOT, root)
            .env("HOME", root)
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("FELL_STORE", root.join("mail.db"))
            .env("FELL_DRAFT_ROOT", root.join("drafts"))
            .env("FELL_DRAFT_SOCK", root.join("run/live.sock"))
            .env("FELL_COMPOSE_INDEX", root.join("data/compose-stores"))
            .env_remove("FELL_MODEL_DIR")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .wait_with_output()
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{stdout}");
    None
}

/// `true` when the test should return now. `missing` is `None` when the
/// capability is there, or the reason it is not.
///
/// # Panics
///
/// On CI or with `FELL_REQUIRE_ALL_TESTS=1`, when `missing` is set.
#[doc(hidden)]
pub fn skip_unless_available(test: &str, missing: Option<String>) -> bool {
    let Some(reason) = missing else {
        return false;
    };
    if all_tests_required() {
        panic!("{test}: {reason}; CI or FELL_REQUIRE_ALL_TESTS=1 does not allow a skip");
    }
    eprintln!("skipped: {test}: {reason}");
    true
}

fn all_tests_required() -> bool {
    let set = |name: &str, value: &str| std::env::var_os(name).is_some_and(|got| got == value);
    set("FELL_REQUIRE_ALL_TESTS", "1") || set("CI", "true")
}

/// `None` when this machine can bind TEST-NET-1 (`192.0.2.1`), which the
/// remote-fetch tests use as a stand-in for a public host. On CI it first
/// adds the address to loopback with `sudo -n`, which never prompts.
#[doc(hidden)]
pub fn test_net_missing() -> Option<String> {
    if std::net::TcpListener::bind("192.0.2.1:0").is_err() && all_tests_required() {
        let _ = std::process::Command::new("sudo")
            .args(["-n", "ip", "addr", "add", "192.0.2.1/32", "dev", "lo"])
            .status();
    }
    match std::net::TcpListener::bind("192.0.2.1:0") {
        Ok(_) => None,
        Err(err) => Some(format!(
            "cannot bind 192.0.2.1 ({err}); add it with `sudo ip addr add 192.0.2.1/32 dev lo`"
        )),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_available_capability_never_skips() {
        assert!(!super::skip_unless_available("t", None));
    }
}
