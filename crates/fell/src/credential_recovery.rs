//! An interrupted account-file publish must be able to undo a keyring overwrite.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use fell_core::accounts::Registry;
use fell_core::credentials::{PasswordStore, SecretService};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::mailbox::{private_config_dir, restrict_permissions};

#[derive(Clone)]
pub(crate) struct Journal {
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Backup {
    published: bool,
    key: String,
    previous: String,
}

impl Journal {
    pub(crate) fn for_accounts(accounts: &Path) -> Option<Self> {
        let state = std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| {
                if cfg!(test) {
                    return None;
                }
                std::env::var_os("HOME")
                    .map(PathBuf::from)
                    .filter(|path| path.is_absolute())
                    .map(|home| home.join(".local/state"))
            })?;
        Some(Self::at(&state.join("fell"), accounts))
    }

    pub(crate) fn at(state: &Path, accounts: &Path) -> Self {
        let scope = Sha256::digest(accounts.as_os_str().as_encoded_bytes());
        Self {
            path: state.join(format!("credential-recovery-{scope:x}")),
        }
    }

    fn staging(&self) -> PathBuf {
        self.path.with_extension("staging")
    }

    pub(crate) fn record(&self, key: &str, previous: &str) -> io::Result<()> {
        if exists(&self.path)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an earlier credential update still needs recovery",
            ));
        }
        let backup = Backup {
            published: false,
            key: key.into(),
            previous: previous.into(),
        };
        self.install(&backup)?.sync_all()?;
        let state = self.path.parent().expect("journal has a parent directory");
        File::open(state.parent().expect("state has a parent directory"))?.sync_all()
    }

    /// Install the published flag after syncing the account directory. The
    /// caller must sync the returned directory to make the flag durable.
    pub(crate) fn mark_published(&self) -> io::Result<File> {
        let mut backup = self.read()?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "Missing credential recovery file")
        })?;
        backup.published = true;
        self.install(&backup)
    }

    fn install(&self, backup: &Backup) -> io::Result<File> {
        let directory =
            private_config_dir(self.path.parent().expect("journal has a parent directory"))?;
        let staging = self.staging();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging)?;
        let result = (|| {
            restrict_permissions(&file, 0o600)?;
            serde_json::to_writer(&file, backup).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("writing recovery file: {error}"),
                )
            })?;
            file.sync_all()?;
            fs::rename(&staging, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&staging);
        }
        result.map(|()| directory)
    }

    pub(crate) fn clear(&self) -> io::Result<()> {
        fs::remove_file(&self.path)?;
        File::open(self.path.parent().expect("journal has a parent directory"))?.sync_all()
    }

    pub(crate) fn recover_locked(&self, keyring: &dyn PasswordStore) -> Result<(), String> {
        let error_at = |error| format!("{}: {error}", self.path.display());
        if !self.pending().map_err(error_at)? {
            return Ok(());
        }
        private_config_dir(self.path.parent().expect("journal has a parent directory"))
            .map_err(error_at)?;
        // The staging file may be incomplete. The keyring is only touched
        // after the rename and directory sync have both completed.
        match fs::remove_file(self.staging()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error_at(error)),
        }
        if let Some(backup) = self.read().map_err(error_at)? {
            if !backup.published {
                keyring
                    .store(&backup.key, &backup.previous)
                    .map_err(|error| {
                        format!("{error}. Unlock the desktop keyring and reopen Fell.")
                    })?;
            }
            self.clear().map_err(error_at)?;
        }
        Ok(())
    }

    fn read(&self) -> io::Result<Option<Backup>> {
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let meta = file.metadata()?;
        // SAFETY: geteuid takes no arguments and returns the effective user.
        if !meta.is_file()
            || meta.mode() & 0o7777 != 0o600
            || meta.uid() != unsafe { libc::geteuid() }
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "The credential recovery file must be private and owned by you.",
            ));
        }
        serde_json::from_reader(file).map(Some).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Invalid credential recovery file.",
            )
        })
    }

    pub(crate) fn recover(
        &self,
        registry: &Registry,
        keyring: &dyn PasswordStore,
    ) -> Result<(), String> {
        if self
            .pending()
            .map_err(|error| format!("{}: {error}", self.path.display()))?
        {
            let _lock = registry.lock().map_err(|error| error.to_string())?;
            self.recover_locked(keyring)?;
        }
        Ok(())
    }

    fn pending(&self) -> io::Result<bool> {
        Ok(exists(&self.path)? || exists(&self.staging())?)
    }
}

pub(crate) fn recover(registry: &Registry) -> Result<(), String> {
    Journal::for_accounts(&registry.directory())
        .map(|journal| journal.recover(registry, &SecretService))
        .transpose()
        .map(|_| ())
        .map_err(|error| format!("Couldn't recover the account password. {error}"))
}

fn exists(path: &Path) -> io::Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn startup_names_an_invalid_journal_without_printing_its_contents() {
        let Some(_root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        fs::create_dir_all(registry.directory()).unwrap();
        let journal = Journal::for_accounts(&registry.directory()).unwrap();
        let secret = "fixture-secret";
        journal.record("fixture-key", secret).unwrap();

        let assert_error = |expected: &str| {
            let error = crate::mailbox::startup().error.unwrap();
            assert!(error.contains(journal.path.to_str().unwrap()));
            assert!(error.contains(expected));
            assert!(!error.contains(secret));
        };
        fs::set_permissions(&journal.path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_error("must be private and owned by you");

        fs::set_permissions(&journal.path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(&journal.path, format!("\"{secret}\"")).unwrap();
        assert_error("Invalid credential recovery file");
    }
}
