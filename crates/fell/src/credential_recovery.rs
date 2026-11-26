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
    accounts: PathBuf,
    path: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Backup {
    account_file: String,
    device: u64,
    inode: u64,
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
            accounts: accounts.to_owned(),
            path: state.join(format!("credential-recovery-{scope:x}")),
        }
    }

    fn staging(&self) -> PathBuf {
        self.path.with_extension("staging")
    }

    pub(crate) fn record(
        &self,
        account_file: &Path,
        staged: &File,
        key: &str,
        previous: &str,
    ) -> io::Result<()> {
        let account_file = account_file
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid account path"))?;
        let state = self.path.parent().expect("journal has a parent directory");
        let directory = private_config_dir(state)?;
        if exists(&self.path)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "an earlier credential update still needs recovery",
            ));
        }
        let identity = staged.metadata()?;
        let backup = Backup {
            account_file: account_file.into(),
            device: identity.dev(),
            inode: identity.ino(),
            key: key.into(),
            previous: previous.into(),
        };
        let staging = self.staging();
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staging)?;
        let result = (|| {
            restrict_permissions(&file, 0o600)?;
            serde_json::to_writer(&file, &backup).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("writing recovery file: {error}"),
                )
            })?;
            file.sync_all()?;
            fs::rename(&staging, &self.path)?;
            directory.sync_all()?;
            File::open(state.parent().expect("state has a parent directory"))?.sync_all()
        })();
        if result.is_err() {
            let _ = fs::remove_file(&staging);
        }
        result
    }

    pub(crate) fn clear(&self) -> io::Result<()> {
        fs::remove_file(&self.path)?;
        File::open(self.path.parent().expect("journal has a parent directory"))?.sync_all()
    }

    pub(crate) fn recover_locked(&self, keyring: &dyn PasswordStore) -> Result<(), String> {
        if !self.pending().map_err(|error| error.to_string())? {
            return Ok(());
        }
        private_config_dir(self.path.parent().expect("journal has a parent directory"))
            .map_err(|error| error.to_string())?;
        // The staging file may be incomplete. The keyring is only touched
        // after the rename and directory sync have both completed.
        match fs::remove_file(self.staging()) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.to_string()),
        }
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.to_string()),
        };
        let meta = file.metadata().map_err(|error| error.to_string())?;
        // SAFETY: geteuid takes no arguments and returns the effective user.
        if !meta.is_file()
            || meta.mode() & 0o7777 != 0o600
            || meta.uid() != unsafe { libc::geteuid() }
        {
            return Err("The credential recovery file must be private and owned by you.".into());
        }
        let backup: Backup =
            serde_json::from_reader(file).map_err(|_| "Invalid credential recovery file.")?;
        let name = Path::new(&backup.account_file);
        if name.file_name() != Some(name.as_os_str()) {
            return Err("Invalid account path in the credential recovery file.".into());
        }
        let published = match self.accounts.join(name).symlink_metadata() {
            Ok(meta) => meta.is_file() && meta.dev() == backup.device && meta.ino() == backup.inode,
            Err(error) if error.kind() == io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.to_string()),
        };
        if published {
            File::open(&self.accounts)
                .and_then(|dir| dir.sync_all())
                .map_err(|error| error.to_string())?;
        } else {
            keyring
                .store(&backup.key, &backup.previous)
                .map_err(|error| format!("{error}. Unlock the desktop keyring and reopen Fell."))?;
        }
        self.clear().map_err(|error| error.to_string())
    }

    pub(crate) fn recover(
        &self,
        registry: &Registry,
        keyring: &dyn PasswordStore,
    ) -> Result<(), String> {
        if self.pending().map_err(|error| error.to_string())? {
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
