//! Skipping tests that need something only some machines have.
//!
//! A few tests need a capability a developer sandbox may lack, such as the
//! TEST-NET-1 address on loopback, or a home directory with no real Fell
//! mailbox in it. Such a test asks [`skip_unless_available`] first. On a
//! machine without the capability it prints why and is skipped. On CI
//! (`CI=true`, which GitHub Actions sets) or with `FELL_REQUIRE_ALL_TESTS=1`,
//! a missing capability fails the test instead, so nothing is silently
//! skipped where it must run.

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

/// `None` when no real Fell mailbox is at the default path. Tests whose draft
/// lookups fall back to that path would read the person's own mail.
#[doc(hidden)]
pub fn real_mailbox_present() -> Option<String> {
    // A draft's mailbox is found through the compose index first, then the
    // default path; either can lead to the person's mail.
    let index = crate::compose::compose_index_file("probe")
        .and_then(|file| file.parent().map(std::path::Path::to_path_buf))
        .filter(|dir| dir.is_dir());
    let mailbox = crate::compose::mailbox_db_path().filter(|path| path.exists());
    let found = mailbox.or(index)?;
    Some(format!(
        "real Fell data is at {}; run with XDG_DATA_HOME and XDG_RUNTIME_DIR pointing at empty directories",
        found.display()
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_available_capability_never_skips() {
        assert!(!super::skip_unless_available("t", None));
    }
}
