//! The on-disk registry shared by the inbox, SMTP and fell-mcp.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::{AccountConfig, AccountId};

pub const DEFAULT_ID: &str = "mail";

pub fn validate_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty()
        || id.len() > 64
        || !id.as_bytes()[0].is_ascii_alphanumeric()
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(
            "Use 1–64 letters, digits, hyphens or underscores, starting with a letter or digit.",
        );
    }
    if crate::is_demo_account(&AccountId(id.into())) {
        return Err("This account ID is reserved for the sample inbox.");
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileError {
    pub detail: String,
    pub recovery: &'static str,
}

impl FileError {
    pub fn new(detail: impl Into<String>, recovery: &'static str) -> Self {
        Self {
            detail: detail.into(),
            recovery,
        }
    }

    fn access(path: &Path, error: io::Error) -> Self {
        Self::new(
            format!("{}: {error}", path.display()),
            "Restore read access to the account file.",
        )
    }
}

impl std::fmt::Display for FileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub config: Result<AccountConfig, FileError>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Accounts {
    pub entries: Vec<Entry>,
    pub error: Option<FileError>,
}

impl Accounts {
    pub fn default_config(&self) -> Option<&AccountConfig> {
        let mut valid = self
            .entries
            .iter()
            .filter_map(|entry| entry.config.as_ref().ok());
        let first = valid.next()?;
        if first.id == DEFAULT_ID {
            return Some(first);
        }
        Some(
            valid
                .find(|config| config.id == DEFAULT_ID)
                .unwrap_or(first),
        )
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.error.is_none()
    }
}

#[derive(Clone, Debug)]
pub struct Registry {
    config: PathBuf,
}

impl Registry {
    pub fn resolve() -> Result<Self, FileError> {
        if let Some(xdg) =
            std::env::var_os("XDG_CONFIG_HOME").filter(|path| Path::new(path).is_absolute())
        {
            return Ok(Self::at(PathBuf::from(xdg).join("fell")));
        }
        // The draft/socket test helpers set XDG_CONFIG_HOME in an isolated
        // child. A test without an explicit directory must never use HOME.
        if !cfg!(any(test, feature = "test-utils"))
            && let Some(home) =
                std::env::var_os("HOME").filter(|path| Path::new(path).is_absolute())
        {
            return Ok(Self::at(PathBuf::from(home).join(".config/fell")));
        }
        Err(FileError::new(
            "No account configuration directory",
            "Set XDG_CONFIG_HOME.",
        ))
    }

    pub fn at(config: PathBuf) -> Self {
        Self { config }
    }

    pub fn directory(&self) -> PathBuf {
        self.config.join("accounts")
    }

    pub fn path(&self, id: &str) -> Result<PathBuf, FileError> {
        validate_id(id)
            .map_err(|recovery| FileError::new(format!("Invalid account ID {id:?}"), recovery))?;
        match real_directory(&self.config) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(FileError::access(&self.config, error)),
        }
        Ok(self.directory().join(format!("{id}.toml")))
    }

    pub fn load(&self) -> Accounts {
        let mut accounts = Accounts {
            error: self.migrate().err(),
            ..Accounts::default()
        };
        let dir = self.directory();
        match real_directory(&self.config)
            .and_then(|_| real_directory(&dir))
            .and_then(|_| fs::read_dir(&dir))
        {
            Ok(files) => {
                for file in files {
                    let file = match file {
                        Ok(file) => file,
                        Err(error) => {
                            accounts.error = Some(FileError::access(&dir, error));
                            continue;
                        }
                    };
                    let path = file.path();
                    if path.extension().is_none_or(|ext| ext != "toml") {
                        continue;
                    }
                    let id = path.file_stem().and_then(|id| id.to_str()).unwrap_or("");
                    let config = self.path(id).and_then(|_| {
                        read(&path, Some(id))?
                            .map(|(config, _)| config)
                            .ok_or_else(|| missing(&path))
                    });
                    accounts.entries.push(Entry { path, config });
                }
                accounts.entries.sort_by(|a, b| a.path.cmp(&b.path));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => accounts.error = Some(FileError::access(&dir, error)),
        }
        accounts
    }

    /// Read only the requested account so a broken sibling cannot block it.
    pub fn get(&self, id: &AccountId) -> Result<Option<AccountConfig>, FileError> {
        let path = self.path(id.as_str())?;
        let migration = self.migrate();
        match real_directory(&self.config) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return migration.map(|()| None);
            }
            Err(error) => return Err(FileError::access(&self.config, error)),
        }
        match read(&path, Some(id.as_str()))? {
            Some((config, _)) => Ok(Some(config)),
            None => migration.map(|()| None),
        }
    }

    /// Use the existing mailbox for the original account, including old
    /// configs that set a custom ID. Every other account has its own DB.
    pub fn store_path(&self, base: &Path, id: &str) -> PathBuf {
        let original = self.directory().join(".original-id");
        let original = fs::read_to_string(original).ok();
        if id == original.as_deref().unwrap_or(DEFAULT_ID) {
            base.to_owned()
        } else {
            base.with_extension("accounts").join(format!("{id}.db"))
        }
    }

    /// Copy the original bytes into the new layout, then rename the source
    /// to its backup. Both names survive a crash between those operations.
    /// The account directory lock also serializes setup and removal.
    pub fn migrate(&self) -> Result<(), FileError> {
        match real_directory(&self.config) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(FileError::access(&self.config, error)),
        }
        let legacy = self.config.join("account.toml");
        match legacy.symlink_metadata() {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(FileError::access(&legacy, error)),
        }
        let dir = self.directory();
        let lock = open_account_directory(&dir).map_err(|err| FileError::access(&dir, err))?;
        lock.try_lock()
            .map_err(|err| FileError::access(&dir, err.into()))?;
        let Some((config, text)) = read(&legacy, None)? else {
            return Ok(());
        };
        let target = self.path(&config.id)?;
        let backup = self.config.join("account.toml.bak");
        let write = || -> io::Result<()> {
            if backup.symlink_metadata().is_ok() {
                return Err(io::Error::other("account.toml.bak already exists"));
            }
            publish(&dir, &target, text.as_bytes())?;
            if config.id != DEFAULT_ID {
                publish(&dir, &dir.join(".original-id"), config.id.as_bytes())?;
            }
            lock.sync_all()?;
            fs::rename(&legacy, &backup)?;
            File::open(&self.config)?.sync_all()
        };
        write().map_err(|err| FileError::access(&legacy, err))
    }
}

fn real_directory(path: &Path) -> io::Result<()> {
    if path.symlink_metadata()?.is_dir() {
        Ok(())
    } else {
        Err(io::Error::other("Expected a real account directory"))
    }
}

fn open_account_directory(path: &Path) -> io::Result<File> {
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(path)
}

fn publish(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut staged = tempfile::NamedTempFile::new_in(dir)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(path) {
        Ok(_) => Ok(()),
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            let mut existing = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(path)?;
            let meta = existing.metadata()?;
            if meta.is_file()
                && meta.permissions().mode() & 0o077 == 0
                && meta.len() == bytes.len() as u64
            {
                let mut contents = Vec::new();
                existing.read_to_end(&mut contents)?;
                if contents == bytes {
                    return Ok(());
                }
            }
            Err(error.error)
        }
        Err(error) => Err(error.error),
    }
}

pub fn missing(path: &Path) -> FileError {
    FileError::new(
        format!("{} does not exist", path.display()),
        "Run fell account setup.",
    )
}

/// Read from one descriptor and reject special files and exposed passwords.
pub fn read(path: &Path, id: Option<&str>) -> Result<Option<(AccountConfig, String)>, FileError> {
    if id.is_some_and(|id| path.file_stem().and_then(|stem| stem.to_str()) != Some(id)) {
        return Err(FileError::new(
            format!("{} is not this account’s file", path.display()),
            "Fix the settings in the account file.",
        ));
    }
    if id.is_some()
        && let Some(dir) = path.parent()
    {
        match real_directory(dir) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(FileError::access(dir, error)),
        }
    }
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(FileError::access(path, error)),
    };
    let meta = file
        .metadata()
        .map_err(|err| FileError::access(path, err))?;
    if !meta.is_file() || meta.permissions().mode() & 0o077 != 0 {
        return Err(FileError::new(
            format!(
                "{} must be a private regular file (mode 600)",
                path.display()
            ),
            "Set account file permissions to 600.",
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|err| FileError::access(path, err))?;
    Ok(Some((parse(path, &text, id)?, text)))
}

pub fn parse(path: &Path, text: &str, id: Option<&str>) -> Result<AccountConfig, FileError> {
    let invalid = |detail| {
        FileError::new(
            format!("{}: {detail}", path.display()),
            "Fix the settings in the account file.",
        )
    };
    let config = AccountConfig::parse_for(text, id.unwrap_or(DEFAULT_ID))
        .map_err(|err| invalid(err.to_string()))?;
    validate_id(&config.id).map_err(|err| invalid(err.into()))?;
    if id.is_some_and(|id| id != config.id) {
        return Err(invalid(format!(
            "ID {:?} must match the file name",
            config.id
        )));
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_private(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    #[derive(Default)]
    struct Keyring(std::sync::Mutex<std::collections::HashMap<String, String>>);

    impl crate::credentials::PasswordStore for Keyring {
        fn read(&self, key: &str) -> Result<String, crate::credentials::KeyringError> {
            self.0
                .lock()
                .unwrap()
                .get(key)
                .cloned()
                .ok_or(crate::credentials::KeyringError::Missing)
        }

        fn store(&self, key: &str, password: &str) -> Result<(), crate::credentials::KeyringError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_owned(), password.to_owned());
            Ok(())
        }
    }

    #[test]
    fn migration_preserves_exact_bytes_id_and_keyring_source() {
        for id in ["mail", "personal"] {
            let root = tempfile::tempdir().unwrap();
            let dir = root.path().join("config/fell");
            let registry = Registry::at(dir.clone());
            let legacy = dir.join("account.toml");
            let text = format!(
                "# A note\nid = \"{id}\"\nemail = \"mailbox@example.test\"\npassword_source = \"keyring\"\n\n[[identities]]\naddress = \"alias@example.test\"\nsignature = \"Regards\"\n"
            );
            write_private(&legacy, &text);
            let accounts = registry.load();
            assert!(accounts.error.is_none());
            let config = accounts.default_config().unwrap();
            assert_eq!(config.id, id);
            assert_eq!(config.password, crate::PasswordSource::Keyring);
            let keyring = Keyring::default();
            AccountConfig::parse(&text)
                .unwrap()
                .store_keyring_password(&keyring, "test-secret")
                .unwrap();
            assert_eq!(
                config.resolve_password_with(&keyring).unwrap(),
                "test-secret"
            );
            assert_eq!(config.identities[0].signature.as_deref(), Some("Regards"));
            let account = registry.path(id).unwrap();
            assert_eq!(fs::read_to_string(&account).unwrap(), text);
            assert_eq!(
                fs::read_to_string(dir.join("account.toml.bak")).unwrap(),
                text
            );
            assert!(!legacy.exists());
            let base = root.path().join("data/fell/mail.db");
            assert_eq!(registry.store_path(&base, id), base);
            assert_ne!(registry.store_path(&base, "work"), base);
            fs::rename(account, dir.join("disconnected.toml")).unwrap();
            assert!(registry.load().is_empty());
        }
    }

    #[test]
    fn one_invalid_account_does_not_hide_its_neighbors() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::at(root.path().join("fell"));
        write_private(
            &registry.path("work").unwrap(),
            "email = 'work@example.test'\npassword = 'fixture'\n",
        );
        write_private(
            &registry.path("mail").unwrap(),
            "email = 'invalid@example.test'\n",
        );
        let accounts = registry.load();
        assert_eq!(accounts.entries.len(), 2);
        assert_eq!(accounts.default_config().unwrap().id, "work");
        assert!(
            accounts.entries[0]
                .config
                .as_ref()
                .unwrap_err()
                .detail
                .contains("mail.toml")
        );
        assert!(registry.get(&AccountId("mail".into())).is_err());
        assert!(registry.path("../outside").is_err());
    }

    #[test]
    fn migration_stays_inside_the_resolved_config_directory() {
        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let home_config = root.join(".config/fell/account.toml");
        let control = "email = 'control@example.test'\npassword = 'fixture'\n";
        write_private(&home_config, control);
        let xdg_config = root.join("config/fell");
        write_private(
            &xdg_config.join("account.toml"),
            "email = 'xdg@example.test'\npassword = 'fixture'\n",
        );
        let registry = Registry::resolve().unwrap();
        assert_eq!(
            registry.load().default_config().unwrap().email,
            "xdg@example.test"
        );
        assert_eq!(fs::read_to_string(&home_config).unwrap(), control);
        assert!(!home_config.with_extension("toml.bak").exists());
    }
}
