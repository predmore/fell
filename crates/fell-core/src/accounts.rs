//! The on-disk registry shared by the inbox, SMTP and fell-mcp.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::{AccountConfig, AccountId};

mod removal;
pub use removal::{InterruptedRemoval, RemovalOutcome};

pub const DEFAULT_ID: &str = "mail";
pub type FileIdentity = (u64, u64, i64, i64);

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

    pub fn reason(&self, path: &Path) -> &str {
        let path = path.to_string_lossy();
        self.detail
            .strip_prefix(path.as_ref())
            .and_then(|rest| rest.strip_prefix(": ").or_else(|| rest.strip_prefix(' ')))
            .unwrap_or(&self.detail)
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
    /// A replacement with identical settings can still replace a removed account.
    pub file_identity: Option<FileIdentity>,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Accounts {
    pub entries: Vec<Entry>,
    pub error: Option<FileError>,
    pub interrupted: Vec<InterruptedRemoval>,
}

impl Accounts {
    pub fn configs(&self) -> impl Iterator<Item = &AccountConfig> {
        self.entries
            .iter()
            .filter_map(|entry| entry.config.as_ref().ok())
            .filter(|config| {
                self.interrupted
                    .iter()
                    .all(|pending| pending.id != config.id)
            })
    }

    pub fn default_config(&self) -> Option<&AccountConfig> {
        let mut valid = self.configs();
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
        self.entries.is_empty() && self.interrupted.is_empty() && self.error.is_none()
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

    /// Serialize registry-backed grant changes with account removal.
    pub fn lock(&self) -> Result<File, FileError> {
        let dir = self.directory();
        real_directory(&self.config).map_err(|err| FileError::access(&self.config, err))?;
        let lock = open_account_directory(&dir).map_err(|err| FileError::access(&dir, err))?;
        lock.try_lock()
            .map_err(|err| FileError::access(&dir, err.into()))?;
        Ok(lock)
    }

    /// Hold the account file stable while opening its mailbox for the UI.
    pub fn lock_shared(&self) -> Result<File, FileError> {
        let dir = self.directory();
        real_directory(&self.config).map_err(|err| FileError::access(&self.config, err))?;
        let lock = open_account_directory(&dir).map_err(|err| FileError::access(&dir, err))?;
        lock.lock_shared()
            .map_err(|err| FileError::access(&dir, err))?;
        Ok(lock)
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
                    let file_identity = path.symlink_metadata().ok().map(file_identity);
                    accounts.entries.push(Entry {
                        path,
                        config,
                        file_identity,
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => accounts.error = Some(FileError::access(&dir, error)),
        }
        let legacy = self.config.join("account.toml");
        if accounts.error.is_none()
            && let Err(error) = read(&legacy, None)
        {
            accounts.entries.push(Entry {
                file_identity: legacy.symlink_metadata().ok().map(file_identity),
                path: legacy,
                config: Err(error),
            });
        }
        accounts.entries.sort_by(|a, b| a.path.cmp(&b.path));
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
    pub fn store_path(&self, base: &Path, id: &str) -> Result<PathBuf, FileError> {
        let path = self.mailbox_path(base, id)?;
        if removal_trash(&path).symlink_metadata().is_ok() {
            return Err(FileError::new(
                format!("Removal of {id} is in progress"),
                "Open Settings → Accounts to restore local mail or finish deleting.",
            ));
        }
        Ok(path)
    }

    pub fn mailbox_path(&self, base: &Path, id: &str) -> Result<PathBuf, FileError> {
        self.path(id)?;
        Ok(if id == self.original_id()? {
            base.to_owned()
        } else {
            base.with_extension("accounts").join(format!("{id}.db"))
        })
    }

    fn original_id(&self) -> Result<String, FileError> {
        if let Err(err) = real_directory(&self.config)
            && err.kind() != io::ErrorKind::NotFound
        {
            return Err(FileError::access(&self.config, err));
        }
        // Read the source first. If migration has already renamed it, the
        // durable marker must be visible before any migrated file is visible.
        let legacy = read(&self.config.join("account.toml"), None);
        match real_directory(&self.directory()) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(FileError::access(&self.directory(), err)),
        }
        let original = match self.original_marker() {
            Ok(Some(id)) => id,
            marker => {
                let backup = self.config.join("account.toml.bak");
                match backup.symlink_metadata() {
                    Ok(_) => read(&backup, None)?.ok_or_else(|| missing(&backup))?.0.id,
                    Err(err) if err.kind() == io::ErrorKind::NotFound => match legacy? {
                        Some((config, _)) => config.id,
                        None => marker?.unwrap_or_else(|| DEFAULT_ID.into()),
                    },
                    Err(err) => return Err(FileError::access(&backup, err)),
                }
            }
        };
        Ok(original)
    }

    fn original_marker(&self) -> Result<Option<String>, FileError> {
        let marker = self.directory().join(".original-id");
        let file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&marker)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(FileError::access(&marker, err)),
        };
        let mut value = String::new();
        file.take(65)
            .read_to_string(&mut value)
            .map_err(|err| FileError::access(&marker, err))?;
        validate_id(&value).map_err(|err| {
            FileError::new(
                format!("{}: {err}", marker.display()),
                "Restore the original account ID.",
            )
        })?;
        Ok(Some(value))
    }

    /// Copy the original bytes into the new layout, then rename the source
    /// to its backup. Both names survive a crash between those operations.
    /// The account directory lock also serializes setup and removal.
    pub fn migrate(&self) -> Result<(), FileError> {
        self.migrate_with(|| {})
    }

    fn migrate_with(&self, published: impl FnOnce()) -> Result<(), FileError> {
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
        if real_directory(&dir).is_ok()
            && matches!(self.original_marker(), Ok(Some(_)))
            && read(&legacy, None).is_err()
        {
            return Ok(());
        }
        let lock = open_account_directory(&dir).map_err(|err| FileError::access(&dir, err))?;
        lock.try_lock()
            .map_err(|err| FileError::access(&dir, err.into()))?;
        let legacy_config = match read(&legacy, None) {
            Err(_) if matches!(self.original_marker(), Ok(Some(_))) => return Ok(()),
            result => result?,
        };
        let Some((config, text)) = legacy_config else {
            return Ok(());
        };
        let target = self.path(&config.id)?;
        let backup = self.config.join("account.toml.bak");
        let write = || -> io::Result<()> {
            if backup.symlink_metadata().is_ok() {
                return Err(io::Error::other("account.toml.bak already exists"));
            }
            publish(&dir, &dir.join(".original-id"), config.id.as_bytes())?;
            lock.sync_all()?;
            publish(&dir, &target, text.as_bytes())?;
            lock.sync_all()?;
            File::open(&self.config)?.sync_all()?;
            published();
            fs::rename(&legacy, &backup)?;
            File::open(&self.config)?.sync_all()
        };
        write().map_err(|err| FileError::access(&legacy, err))
    }
}

fn file_identity(meta: fs::Metadata) -> FileIdentity {
    (meta.dev(), meta.ino(), meta.ctime(), meta.ctime_nsec())
}

/// Durable staging directory beside the mailbox being removed.
pub(crate) fn removal_trash(store: &Path) -> PathBuf {
    let mut name = store.file_name().unwrap_or_default().to_os_string();
    name.push(".removing");
    store.with_file_name(name)
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
            assert_eq!(registry.store_path(&base, id).unwrap(), base);
            assert_ne!(registry.store_path(&base, "work").unwrap(), base);
            fs::rename(account, dir.join("disconnected.toml")).unwrap();
            assert!(registry.load().is_empty());
        }
    }

    #[test]
    fn a_failed_marker_never_publishes_a_new_mailbox_path() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let registry = Registry::at(config.clone());
        let legacy = config.join("account.toml");
        let text = "id = 'personal'\nemail = 'personal@example.test'\npassword = 'fixture'\n";
        write_private(&legacy, text);
        fs::create_dir_all(registry.directory().join(".original-id")).unwrap();
        assert!(registry.load().error.is_some());
        assert!(!registry.path("personal").unwrap().exists());
        assert_eq!(fs::read_to_string(&legacy).unwrap(), text);

        // An account file left by an older Fell stays on the legacy DB too.
        write_private(&registry.path("personal").unwrap(), text);
        let base = root.path().join("mail.db");
        assert_eq!(registry.store_path(&base, "personal").unwrap(), base);
        assert!(!base.with_extension("accounts").exists());
    }

    #[test]
    fn a_lost_or_invalid_marker_uses_the_migration_backup() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let registry = Registry::at(config.clone());
        let base = root.path().join("mail.db");
        let backup = config.join("account.toml.bak");
        write_private(
            &backup,
            "id = 'personal'\nemail = 'personal@example.test'\npassword = 'fixture'\n",
        );
        for marker in [None, Some("bad/id")] {
            if let Some(marker) = marker {
                write_private(&registry.directory().join(".original-id"), marker);
            }
            assert_eq!(registry.store_path(&base, "personal").unwrap(), base);
            assert_ne!(registry.store_path(&base, "mail").unwrap(), base);
        }
        fs::write(backup, "broken").unwrap();
        assert!(registry.store_path(&base, "mail").is_err());
    }

    #[test]
    fn a_broken_legacy_leftover_is_listed_without_blocking_mailboxes() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let registry = Registry::at(config.clone());
        let legacy = config.join("account.toml");
        let base = root.path().join("mail.db");
        write_private(
            &legacy,
            "id = 'personal'\nemail = 'personal@example.test'\npassword = 'fixture'\n",
        );
        registry.migrate().unwrap();
        write_private(
            &registry.path("work").unwrap(),
            "email = 'work@example.test'\npassword = 'fixture'\n",
        );
        write_private(&legacy, "invalid = [");
        registry.migrate().unwrap();
        let catalog = registry.load();
        assert!(catalog.error.is_none());
        let broken = catalog
            .entries
            .iter()
            .find(|entry| entry.path == legacy)
            .unwrap();
        assert!(broken.config.is_err());
        assert_eq!(registry.store_path(&base, "personal").unwrap(), base);
        assert_ne!(registry.store_path(&base, "work").unwrap(), base);
        assert_eq!(fs::read_to_string(legacy).unwrap(), "invalid = [");
    }

    #[test]
    fn a_reader_during_migration_sees_the_legacy_mailbox() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let registry = Registry::at(config.clone());
        let legacy = config.join("account.toml");
        write_private(
            &legacy,
            "id = 'personal'\nemail = 'p@example.test'\npassword = 'fixture'\n",
        );
        let base = root.path().join("mail.db");
        std::thread::scope(|scope| {
            let (published, observed) = std::sync::mpsc::channel();
            let (read, done) = std::sync::mpsc::channel();
            let registry = &registry;
            let migrating = scope.spawn(move || {
                registry.migrate_with(|| {
                    published.send(()).unwrap();
                    done.recv().unwrap();
                })
            });
            observed.recv().unwrap();
            let during = registry.store_path(&base, "personal");
            let account = registry.get(&AccountId("personal".into()));
            let legacy_visible = legacy.is_file();
            let marker = fs::read_to_string(registry.directory().join(".original-id"));
            read.send(()).unwrap();
            migrating.join().unwrap().unwrap();
            assert!(legacy_visible);
            assert_eq!(marker.unwrap(), "personal");
            assert_eq!(during.unwrap(), base);
            assert!(account.unwrap().is_some());
        });
        assert_eq!(registry.store_path(&base, "personal").unwrap(), base);
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
