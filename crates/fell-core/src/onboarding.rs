//! Shared connection check for account setup, with no mailbox writes.

use crate::credentials::{PasswordStore, SecretService};
use crate::{
    AccountConfig, BackendKind, Error, ImapConfig, Result, SmtpConfig, SyncFailureKind,
    classify_sync_error,
};

/// Both logins are attempted even when one fails. Invalid settings or an
/// unreadable password stop the check before either server is contacted.
#[derive(Debug)]
pub struct ConnectionReport {
    pub imap: Result<usize>,
    pub smtp: Result<()>,
}

impl ConnectionReport {
    pub fn errors(&self) -> impl Iterator<Item = &Error> {
        self.imap
            .as_ref()
            .err()
            .into_iter()
            .chain(self.smtp.as_ref().err())
    }
}

pub enum ConnectionProgress<'a> {
    ImapStarted,
    ImapFinished(&'a Result<usize>),
    SmtpStarted,
    SmtpFinished(&'a Result<()>),
}

/// Login-only checks. Tests can substitute results without opening sockets.
pub trait ConnectionProbe {
    fn check_imap(&self, config: &ImapConfig) -> Result<usize>;
    fn check_smtp(&self, config: &SmtpConfig, username: &str, password: &str) -> Result<()>;
}

struct MailServerProbe;

impl ConnectionProbe for MailServerProbe {
    fn check_imap(&self, config: &ImapConfig) -> Result<usize> {
        crate::imap::check_login(config)
    }

    fn check_smtp(&self, config: &SmtpConfig, username: &str, password: &str) -> Result<()> {
        crate::smtp::check_login(&crate::smtp::Endpoint::new(config, username, password))
    }
}

/// Check the settings and both logins without downloading messages or sending
/// mail. Use on a worker thread. An entered password can be checked as
/// [`crate::PasswordSource::Inline`] before it is saved to the keyring.
pub fn check_connection(
    config: &AccountConfig,
    progress: impl FnMut(ConnectionProgress<'_>),
) -> Result<ConnectionReport> {
    check_connection_with(config, &SecretService, &MailServerProbe, progress)
}

pub fn check_connection_with(
    config: &AccountConfig,
    store: &dyn PasswordStore,
    probe: &dyn ConnectionProbe,
    mut progress: impl FnMut(ConnectionProgress<'_>),
) -> Result<ConnectionReport> {
    config.validate()?;
    let imap_config = config.imap_config_with_password(config.resolve_password_with(store)?);
    progress(ConnectionProgress::ImapStarted);
    let imap = probe.check_imap(&imap_config);
    progress(ConnectionProgress::ImapFinished(&imap));
    progress(ConnectionProgress::SmtpStarted);
    let smtp = probe.check_smtp(
        &config.smtp_config(),
        &config.username,
        &imap_config.password,
    );
    progress(ConnectionProgress::SmtpFinished(&smtp));
    Ok(ConnectionReport { imap, smtp })
}

/// Recovery text for failed connection checks, shared by setup surfaces.
pub fn connection_recovery(config: &AccountConfig, err: &Error) -> Option<&'static str> {
    use crate::credentials::KeyringError;

    if let Error::Keyring(error) = err {
        return Some(match error {
            KeyringError::Unavailable => "Start your desktop keyring service and try again.",
            KeyringError::Locked => "Unlock your desktop keyring and try again.",
            KeyringError::Missing | KeyringError::Invalid => {
                "Run fell account setup to save the password again."
            }
            KeyringError::Ambiguous => {
                "Remove the duplicate Fell password entries from your desktop keyring, then run fell account setup."
            }
        });
    }

    let bridge = config.backend == BackendKind::Bridge;
    Some(match classify_sync_error(err) {
        SyncFailureKind::Unreachable if bridge => {
            "Nothing is listening there. Start Proton Mail Bridge and sign in, then try again."
        }
        SyncFailureKind::Unreachable => "The server did not answer on that host and port.",
        SyncFailureKind::LoginRejected if bridge => {
            "Bridge refused the login. Use the IMAP username and password shown in Bridge's mailbox details."
        }
        SyncFailureKind::LoginRejected => {
            "Check the username and password. Some providers require an app password."
        }
        SyncFailureKind::Tls => {
            "Check the server host, port, TLS mode, and certificate, then try again."
        }
        SyncFailureKind::Password => "The password command did not print a password.",
        SyncFailureKind::Other => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    use super::*;
    use crate::PasswordSource;
    use crate::credentials::KeyringError;

    #[derive(Default)]
    struct MemoryPasswords {
        passwords: RefCell<HashMap<String, String>>,
        reads: Cell<usize>,
    }

    impl PasswordStore for MemoryPasswords {
        fn read(&self, key: &str) -> std::result::Result<String, KeyringError> {
            self.reads.set(self.reads.get() + 1);
            self.passwords
                .borrow()
                .get(key)
                .cloned()
                .ok_or(KeyringError::Missing)
        }

        fn store(&self, key: &str, password: &str) -> std::result::Result<(), KeyringError> {
            self.passwords
                .borrow_mut()
                .insert(key.into(), password.into());
            Ok(())
        }
    }

    const EXAMPLE_PASSWORD: &str = "sample-password";

    #[derive(Default)]
    struct FakeProbe {
        imap_error: Cell<Option<Error>>,
        smtp_error: Cell<Option<Error>>,
    }

    impl ConnectionProbe for FakeProbe {
        fn check_imap(&self, config: &ImapConfig) -> Result<usize> {
            assert_eq!((config.host.as_str(), config.port), ("127.0.0.1", 1143));
            assert_eq!(config.username, "mailbox-user");
            assert_eq!(config.password, EXAMPLE_PASSWORD);
            self.imap_error.take().map_or(Ok(3), Err)
        }

        fn check_smtp(&self, config: &SmtpConfig, username: &str, password: &str) -> Result<()> {
            assert_eq!((config.host.as_str(), config.port), ("127.0.0.1", 1025));
            assert_eq!(username, "mailbox-user");
            assert_eq!(password, EXAMPLE_PASSWORD);
            self.smtp_error.take().map_or(Ok(()), Err)
        }
    }

    fn account(password: PasswordSource) -> AccountConfig {
        AccountConfig::bridge("me@example.test", "mailbox-user", password)
    }

    #[test]
    fn keyring_password_is_replaced_and_round_trips_without_entering_the_file() {
        let store = MemoryPasswords::default();
        let config = account(PasswordSource::Keyring);
        config
            .store_keyring_password(&store, "older-example")
            .unwrap();
        config
            .store_keyring_password(&store, EXAMPLE_PASSWORD)
            .unwrap();
        assert_eq!(store.passwords.borrow().len(), 1);

        let rendered = config.render();
        assert!(!rendered.contains(EXAMPLE_PASSWORD));
        let mut loaded = AccountConfig::parse(&rendered).unwrap();
        loaded.name = "New display name".into();
        assert_eq!(
            loaded.resolve_password_with(&store).unwrap(),
            EXAMPLE_PASSWORD
        );

        for other in [
            AccountConfig {
                email: "other@example.test".into(),
                ..loaded.clone()
            },
            AccountConfig {
                username: "other-user".into(),
                ..loaded.clone()
            },
            AccountConfig {
                port: 993,
                ..loaded.clone()
            },
            AccountConfig {
                smtp_host: Some("smtp.example.test".into()),
                ..loaded.clone()
            },
        ] {
            assert!(matches!(
                other.resolve_password_with(&store),
                Err(Error::Keyring(KeyringError::Missing))
            ));
        }
    }

    #[test]
    fn connection_check_resolves_the_password_once_and_reports_both_logins() {
        let store = MemoryPasswords::default();
        let config = account(PasswordSource::Keyring);
        config
            .store_keyring_password(&store, EXAMPLE_PASSWORD)
            .unwrap();
        let probe = FakeProbe {
            imap_error: Cell::new(Some(Error::Backend("imap: LOGIN failed: NO".into()))),
            smtp_error: Cell::new(Some(Error::Tls("SMTP rejected STARTTLS".into()))),
        };
        let mut events = Vec::new();
        let report = check_connection_with(&config, &store, &probe, |event| {
            events.push(match event {
                ConnectionProgress::ImapStarted => "IMAP started",
                ConnectionProgress::ImapFinished(_) => "IMAP finished",
                ConnectionProgress::SmtpStarted => "SMTP started",
                ConnectionProgress::SmtpFinished(_) => "SMTP finished",
            });
        })
        .unwrap();
        assert_eq!(store.reads.get(), 1);
        assert_eq!(
            events,
            [
                "IMAP started",
                "IMAP finished",
                "SMTP started",
                "SMTP finished"
            ]
        );
        assert_eq!(
            report.errors().map(classify_sync_error).collect::<Vec<_>>(),
            [SyncFailureKind::LoginRejected, SyncFailureKind::Tls]
        );
        assert!(
            connection_recovery(&config, report.imap.as_ref().unwrap_err())
                .unwrap()
                .contains("Bridge's mailbox details")
        );
        assert!(
            connection_recovery(&config, report.smtp.as_ref().unwrap_err())
                .unwrap()
                .contains("certificate")
        );
    }

    #[test]
    fn connection_check_accepts_unsaved_and_command_passwords_without_a_keyring() {
        let store = MemoryPasswords::default();
        let probe = FakeProbe::default();
        for source in [
            PasswordSource::Inline(EXAMPLE_PASSWORD.into()),
            PasswordSource::Command(format!("printf '{EXAMPLE_PASSWORD}\\nignored'")),
        ] {
            let config = account(source);
            let report = check_connection_with(&config, &store, &probe, |_| {}).unwrap();
            assert_eq!(report.imap.unwrap(), 3);
            report.smtp.unwrap();
        }
        assert_eq!(store.reads.get(), 0);
        assert!(store.passwords.borrow().is_empty());
    }

    #[test]
    fn validation_and_password_errors_stop_before_connecting() {
        let store = MemoryPasswords::default();
        let probe = FakeProbe::default();
        let mut config = account(PasswordSource::Keyring);
        config.port = 0;
        let no_progress = |_: ConnectionProgress<'_>| {
            panic!("invalid settings or missing password started a connection")
        };
        let error = check_connection_with(&config, &store, &probe, no_progress).unwrap_err();
        assert!(matches!(error, Error::Invalid(_)));
        assert_eq!(store.reads.get(), 0);

        config.port = 1143;
        let error = check_connection_with(&config, &store, &probe, no_progress).unwrap_err();
        assert!(matches!(error, Error::Keyring(KeyringError::Missing)));
        assert_eq!(classify_sync_error(&error), SyncFailureKind::Password);
        assert!(
            connection_recovery(&config, &error)
                .unwrap()
                .contains("account setup")
        );
        let unreachable = Error::Backend("imap: connect 127.0.0.1:1143 failed: refused".into());
        assert!(
            connection_recovery(&config, &unreachable)
                .unwrap()
                .contains("Start Proton Mail Bridge")
        );
    }
}
