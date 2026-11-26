//! The on-disk registry shared by the inbox, SMTP and fell-mcp.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, ErrorCode, OpenFlags};

use crate::{AccountConfig, AccountId};

mod removal;
pub use removal::{InterruptedRemoval, RemovalOutcome};

pub const DEFAULT_ID: &str = "mail";
pub const MAILBOX_LINK_DAMAGED: &str = "Mailbox link damaged";
pub const MAILBOX_LINK_RECOVERY: &str = "Restore account.toml.bak or accounts/.original-id";
pub const MAILBOX_ADDRESS_RECOVERY: &str = "Use the same address or open Fell Settings → Accounts, choose Keep local mail, then Add account.";
const MAILBOX_OWNER_RECOVERY: &str =
    "Restore the account that owns this mailbox, or move the file aside before using this ID.";
const MAILBOX_READ_RECOVERY: &str =
    "Check the file and close other programs using it. Restore a mailbox backup if it is damaged.";
const INTERRUPTED_REMOVAL_RECOVERY: &str =
    "Open Settings → Accounts to restore local mail or finish deleting.";
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
            format!("{}: {error}", file_name(path)),
            "Restore read access to the account file.",
        )
    }

    pub fn reason(&self, path: &Path) -> &str {
        let path = file_name(path);
        self.detail
            .strip_prefix(path.as_ref())
            .and_then(|rest| rest.strip_prefix(": ").or_else(|| rest.strip_prefix(' ')))
            .unwrap_or(&self.detail)
    }

    fn mailbox_link(detail: impl Into<String>) -> Self {
        Self::new(detail, MAILBOX_LINK_RECOVERY)
    }

    pub fn is_mailbox_link(&self) -> bool {
        self.recovery == MAILBOX_LINK_RECOVERY
    }

    pub fn cli_message(&self) -> String {
        if self.is_mailbox_link() {
            format!(
                "{MAILBOX_LINK_DAMAGED} · {} · {}",
                self.recovery, self.detail
            )
        } else if matches!(
            self.recovery,
            MAILBOX_ADDRESS_RECOVERY
                | MAILBOX_OWNER_RECOVERY
                | MAILBOX_READ_RECOVERY
                | INTERRUPTED_REMOVAL_RECOVERY
        ) {
            format!("{} · {}", self.detail, self.recovery)
        } else {
            self.to_string()
        }
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
    pub(crate) damaged_link_name: Option<String>,
    /// A replacement with identical settings can still replace a removed account.
    pub file_identity: Option<FileIdentity>,
}

impl Entry {
    pub fn damaged_link_name(&self) -> Option<&str> {
        self.damaged_link_name.as_deref()
    }
}

enum OriginalMailbox {
    Known {
        id: String,
        backup: Option<PathBuf>,
    },
    Unavailable {
        id: String,
        path: PathBuf,
        error: FileError,
    },
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
    base: Option<PathBuf>,
}

impl Registry {
    pub fn resolve() -> Result<Self, FileError> {
        if let Some(xdg) =
            std::env::var_os("XDG_CONFIG_HOME").filter(|path| Path::new(path).is_absolute())
        {
            return Ok(Self::resolved(PathBuf::from(xdg).join("fell")));
        }
        // The draft/socket test helpers set XDG_CONFIG_HOME in an isolated
        // child. A test without an explicit directory must never use HOME.
        if !cfg!(any(test, feature = "test-utils"))
            && let Some(home) =
                std::env::var_os("HOME").filter(|path| Path::new(path).is_absolute())
        {
            return Ok(Self::resolved(PathBuf::from(home).join(".config/fell")));
        }
        Err(FileError::new(
            "No account configuration directory",
            "Set XDG_CONFIG_HOME.",
        ))
    }

    fn resolved(config: PathBuf) -> Self {
        Self {
            config,
            base: crate::compose::mailbox_db_path(),
        }
    }

    pub fn at(config: PathBuf, base: PathBuf) -> Self {
        Self {
            config,
            base: Some(base),
        }
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

    /// First open creates the schema before installing the owner. Hold the
    /// exclusive registry lock across both steps so readers see a complete mailbox.
    pub fn lock_for_mailbox_open(&self, base: &Path, id: &str) -> Result<File, FileError> {
        let lock = self.lock_shared()?;
        let path = self.mailbox_path(base, id)?;
        match path.symlink_metadata() {
            Ok(meta) if meta.len() != 0 => return Ok(lock),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(FileError::new(
                    format!("{}: {err}", path.display()),
                    MAILBOX_READ_RECOVERY,
                ));
            }
        }
        lock.unlock()
            .and_then(|()| lock.lock())
            .map_err(|err| FileError::access(&self.directory(), err))?;
        // The caller rechecks the account and route after acquiring this lock.
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
        self.load_at(self.base.as_deref())
    }

    fn load_at(&self, base: Option<&Path>) -> Accounts {
        let mut accounts = Accounts {
            error: self.migrate_at(base).err(),
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
                        damaged_link_name: None,
                        file_identity,
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => accounts.error = Some(FileError::access(&dir, error)),
        }
        let legacy = self.config.join("account.toml");
        if let Err(error) = read(&legacy, None) {
            accounts.entries.push(Entry {
                file_identity: legacy.symlink_metadata().ok().map(file_identity),
                path: legacy,
                config: Err(error),
                damaged_link_name: None,
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
        let account = self.path(id)?;
        let config = read(&account, Some(id)).ok().flatten();
        let (path, backup) = self.mailbox_location(base, id)?;
        let legacy = self.config.join("account.toml");
        let config = config.or_else(|| {
            (path == base)
                .then(|| read(&legacy, None).ok().flatten())
                .flatten()
                .filter(|(config, _)| config.id == id)
        });
        check_live_mailbox(
            &path,
            id,
            config.as_ref().map(|(c, _)| c.email.as_str()),
            backup.as_deref(),
        )?;
        Ok(path)
    }

    pub fn configured_store_path(
        &self,
        base: &Path,
        config: &AccountConfig,
    ) -> Result<PathBuf, FileError> {
        let (path, backup) = self.mailbox_location(base, &config.id)?;
        check_live_mailbox(&path, &config.id, Some(&config.email), backup.as_deref())?;
        Ok(path)
    }

    /// Locate a candidate for reservation and recovery, without authorizing an open.
    pub fn mailbox_path(&self, base: &Path, id: &str) -> Result<PathBuf, FileError> {
        self.mailbox_location(base, id).map(|(path, _)| path)
    }

    pub fn mailbox_reserved(&self, base: &Path, id: &str) -> Result<bool, FileError> {
        match self.mailbox_path(base, id) {
            Ok(path) => mailbox_exists(&path).map_err(|err| FileError::access(&path, err)),
            Err(error)
                if error.is_mailbox_link() || error.recovery == INTERRUPTED_REMOVAL_RECOVERY =>
            {
                Ok(true)
            }
            Err(error) => Err(error),
        }
    }

    fn mailbox_location(
        &self,
        base: &Path,
        id: &str,
    ) -> Result<(PathBuf, Option<PathBuf>), FileError> {
        self.path(id)?;
        match self.original_mailbox(base)? {
            Some(OriginalMailbox::Known {
                id: original,
                backup,
            }) if id == original => {
                return Ok((base.to_owned(), backup));
            }
            Some(OriginalMailbox::Unavailable {
                id: original,
                error,
                ..
            }) if id == original => {
                return Err(error);
            }
            _ => {}
        }
        Ok((sibling_mailbox(base, id), None))
    }

    fn original_mailbox(&self, base: &Path) -> Result<Option<OriginalMailbox>, FileError> {
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
        let marker = self.directory().join(".original-id");
        let marker_error = match self.original_marker() {
            Ok(Some(id)) => return Ok(Some(check_original_collision(base, id, marker, None))),
            marker => marker.err(),
        };
        let backup = self.config.join("account.toml.bak");
        let metadata_error = match read(&backup, None) {
            Ok(Some((config, _))) => {
                return Ok(Some(check_original_collision(
                    base,
                    config.id,
                    backup.clone(),
                    Some(backup),
                )));
            }
            Err(error) => Some((backup, error)),
            Ok(None) => {
                if let Ok(Some((config, text))) = legacy {
                    let sibling = sibling_mailbox(base, &config.id);
                    let target = self.path(&config.id)?;
                    let target_exists =
                        path_exists(&target).map_err(|err| FileError::access(&target, err))?;
                    let current = read(&target, Some(&config.id)).ok().flatten();
                    let target_conflicts = target_exists
                        && current.as_ref().is_none_or(|(_, current)| *current != text);
                    let sibling_owned = has_classified_owner(&sibling);
                    // A repaired legacy file cannot take over an established
                    // per-account route. Ownerless leftovers cannot establish one.
                    let different_address = current.as_ref().is_some_and(|(current, _)| {
                        !current.email.eq_ignore_ascii_case(&config.email)
                    });
                    if sibling_owned
                        && (!removal::mailbox_has_owner(base, &config.id)
                            || (different_address
                                && original_collision(base, &config.id).is_some()))
                    {
                        return Ok(None);
                    }
                    if target_conflicts {
                        return Ok(Some(OriginalMailbox::Unavailable {
                            id: config.id,
                            path: self.config.join("account.toml"),
                            error: FileError::mailbox_link(
                                "account.toml: Conflicts with the account file in accounts/",
                            ),
                        }));
                    }
                    return Ok(Some(check_original_collision(
                        base,
                        config.id,
                        self.config.join("account.toml"),
                        None,
                    )));
                }
                marker_error.map(|error| (marker.clone(), error))
            }
        };
        if let Some((path, error)) = metadata_error {
            let id = mailbox_owner(base)
                .ok()
                .flatten()
                .map(|(id, _)| id)
                .or_else(|| removal::staged_mailbox_id(base))
                .or_else(|| {
                    legacy
                        .as_ref()
                        .ok()?
                        .as_ref()
                        .map(|(config, _)| config.id.clone())
                })
                .unwrap_or_else(|| DEFAULT_ID.into());
            return Ok(Some(OriginalMailbox::Unavailable {
                id,
                path,
                error: FileError::mailbox_link(error.detail),
            }));
        }
        Ok(self
            .infer_original_mailbox(base, legacy.is_err())?
            .map(|id| check_original_collision(base, id, marker, None)))
    }

    fn infer_original_mailbox(
        &self,
        base: &Path,
        broken_legacy: bool,
    ) -> Result<Option<String>, FileError> {
        let owner = mailbox_owner(base)
            .ok()
            .flatten()
            .filter(|(id, _)| validate_id(id).is_ok());
        let dir = self.directory();
        if broken_legacy && has_account_file(&dir).map_err(|err| FileError::access(&dir, err))? {
            // A readable account file for the same address can corroborate
            // the base owner when the legacy file is damaged.
            let Some((id, email)) = owner else {
                return Ok(None);
            };
            let path = self.path(&id)?;
            if !read(&path, Some(&id)).is_ok_and(|config| {
                config.is_some_and(|(config, _)| email.eq_ignore_ascii_case(&config.email))
            }) {
                return Ok(None);
            }
            return Ok(Some(id));
        }
        if let Some(id) = owner
            .map(|(id, _)| id)
            .or_else(|| removal::staged_mailbox_id(base))
        {
            return Ok(Some(id));
        }
        let sibling = sibling_mailbox(base, DEFAULT_ID);
        let use_base = !broken_legacy && matches!(mailbox_exists(&sibling), Ok(false));
        Ok(use_base.then(|| DEFAULT_ID.into()))
    }

    /// Record the first account's route immediately before publishing its
    /// config. The caller holds the exclusive directory lock and rolls back
    /// this new marker if account publication fails.
    pub fn initialize_original(
        &self,
        directory: &File,
        base: &Path,
        id: &str,
    ) -> Result<Option<PathBuf>, FileError> {
        self.path(id)?;
        let dir = self.directory();
        let marker = dir.join(".original-id");
        let initialize = || -> io::Result<bool> {
            if path_exists(&marker)?
                || path_exists(&self.config.join("account.toml.bak"))?
                || mailbox_exists(base)?
                || mailbox_exists(&sibling_mailbox(base, id))?
                || has_account_file(&dir)?
            {
                return Ok(false);
            }
            publish(&dir, &marker, id.as_bytes())?;
            if let Err(error) = directory
                .sync_all()
                .and_then(|()| File::open(&self.config)?.sync_all())
            {
                fs::remove_file(&marker).and_then(|()| directory.sync_all())?;
                return Err(error);
            }
            Ok(true)
        };
        let initialized = initialize().map_err(|err| FileError::access(&marker, err))?;
        Ok(initialized.then_some(marker))
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
        file.take(129)
            .read_to_string(&mut value)
            .map_err(|err| FileError::access(&marker, err))?;
        let id = value.trim();
        if value.len() > 128 {
            return Err(FileError::new(
                ".original-id: File is too large",
                "Restore the original account ID.",
            ));
        }
        validate_id(id).map_err(|err| {
            FileError::new(
                format!("{}: {err}", file_name(&marker)),
                "Restore the original account ID.",
            )
        })?;
        Ok(Some(id.to_owned()))
    }

    /// Copy the original bytes into the new layout, then rename the source
    /// to its backup. Both names survive a crash between those operations.
    /// The account directory lock also serializes setup and removal.
    pub fn migrate(&self) -> Result<(), FileError> {
        self.migrate_at(self.base.as_deref())
    }

    fn migrate_at(&self, base: Option<&Path>) -> Result<(), FileError> {
        self.migrate_with(base, || Ok(()), || {})
    }

    fn migrate_with(
        &self,
        base: Option<&Path>,
        before_publish: impl FnOnce() -> io::Result<()>,
        published: impl FnOnce(),
    ) -> Result<(), FileError> {
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
        if read(&legacy, None).is_err() {
            return Ok(());
        }
        let lock = open_account_directory(&dir).map_err(|err| FileError::access(&dir, err))?;
        lock.try_lock()
            .map_err(|err| FileError::access(&dir, err.into()))?;
        let Ok(Some((config, text))) = read(&legacy, None) else {
            return Ok(());
        };
        let target = self.path(&config.id)?;
        let backup = self.config.join("account.toml.bak");
        let marker = dir.join(".original-id");
        let mut created_marker = false;
        let mut created_target = false;
        let mut backed_up = false;
        let write = || -> io::Result<()> {
            if path_exists(&backup)? {
                return Err(io::Error::other("account.toml.bak already exists"));
            }
            let base = base.ok_or_else(|| io::Error::other("No local mailbox path"))?;
            let sibling = sibling_mailbox(base, &config.id);
            if (has_classified_owner(&sibling)
                && (!removal::mailbox_has_owner(base, &config.id)
                    || original_collision(base, &config.id).is_some()))
                || (path_exists(&target)? && !path_exists(&marker)?)
            {
                return Err(io::Error::other(
                    "An account file or local mailbox already uses this ID",
                ));
            }
            created_marker = publish(&dir, &marker, config.id.as_bytes())?;
            lock.sync_all()?;
            before_publish()?;
            created_target = publish(&dir, &target, text.as_bytes())?;
            lock.sync_all()?;
            File::open(&self.config)?.sync_all()?;
            published();
            fs::rename(&legacy, &backup)?;
            backed_up = true;
            File::open(&self.config)?.sync_all()
        };
        if let Err(error) = write() {
            // Keep an installed backup and config if the final fsync failed.
            // Roll back only files this attempt published.
            if !backed_up && (created_target || created_marker) {
                let target_cleanup = created_target.then(|| fs::remove_file(&target));
                let marker_cleanup = created_marker.then(|| fs::remove_file(&marker));
                let synced = lock.sync_all();
                if let Some(cleanup_error) = [target_cleanup, marker_cleanup, Some(synced)]
                    .into_iter()
                    .flatten()
                    .find_map(Result::err)
                {
                    return Err(FileError::access(&legacy, cleanup_error));
                }
            }
            return Err(FileError::access(&legacy, error));
        }
        Ok(())
    }
}

fn sibling_mailbox(base: &Path, id: &str) -> PathBuf {
    base.with_extension("accounts").join(format!("{id}.db"))
}

fn check_original_collision(
    base: &Path,
    id: String,
    path: PathBuf,
    backup: Option<PathBuf>,
) -> OriginalMailbox {
    match original_collision(base, &id) {
        Some(error) => OriginalMailbox::Unavailable { id, path, error },
        None => OriginalMailbox::Known { id, backup },
    }
}

fn original_collision(base: &Path, id: &str) -> Option<FileError> {
    // Recovery of the actual base mailbox takes precedence over leftovers.
    if path_exists(&removal_trash(base)).unwrap_or(false) {
        return None;
    }
    let sibling = sibling_mailbox(base, id);
    let trash = removal_trash(&sibling);
    let staged = removal::staged_file(&trash, &sibling, "");
    let conflict = |path: &Path, staged: bool| {
        let owner = match inspect_mailbox(path, staged) {
            Ok(MailboxOwnership::Owned(owner)) => owner,
            Ok(MailboxOwnership::Shared(first, second)) => {
                return Some(format!(
                    "{} has multiple owners: {} and {}",
                    path.display(),
                    first.0,
                    second.0,
                ));
            }
            Err(error) if staged => return Some(error.detail),
            _ => return None,
        };
        if mailbox_owner(base).is_ok_and(|base_owner| {
            base_owner.is_some_and(|(base_id, base_email)| {
                base_id == id && base_id == owner.0 && base_email.eq_ignore_ascii_case(&owner.1)
            })
        }) {
            return None;
        }
        Some(format!(
            "{} belongs to account {:?} ({}) and conflicts with {}",
            path.display(),
            owner.0,
            owner.1,
            base.display()
        ))
    };
    if let Some(detail) = conflict(&staged, true) {
        return Some(FileError::new(
            format!("Interrupted removal for {id}: {detail}"),
            INTERRUPTED_REMOVAL_RECOVERY,
        ));
    }
    conflict(&sibling, false).map(FileError::mailbox_link)
}

fn has_classified_owner(path: &Path) -> bool {
    matches!(
        inspect_mailbox(path, false),
        Ok(MailboxOwnership::Owned(_) | MailboxOwnership::Shared(..))
    )
}

fn has_account_file(dir: &Path) -> io::Result<bool> {
    let files = match fs::read_dir(dir) {
        Ok(files) => files,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err),
    };
    for entry in files {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "toml") {
            return Ok(true);
        }
    }
    Ok(false)
}

fn file_name(path: &Path) -> std::borrow::Cow<'_, str> {
    path.file_name().unwrap_or_default().to_string_lossy()
}

pub fn path_exists(path: &Path) -> io::Result<bool> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Include sidecars and interrupted removal directories when reserving IDs.
pub fn mailbox_exists(path: &Path) -> io::Result<bool> {
    if path_exists(&removal_trash(path))? {
        return Ok(true);
    }
    for suffix in removal::MAILBOX_SUFFIXES {
        if path_exists(&removal::mailbox_file(path, suffix))? {
            return Ok(true);
        }
    }
    Ok(false)
}

type Owner = (String, String);

enum MailboxOwnership {
    New,
    Unowned(&'static str),
    Owned(Owner),
    Shared(Owner, Owner),
}

/// Inspect ownership without opening a store or modifying its schema.
pub fn mailbox_owner(path: &Path) -> Result<Option<(String, String)>, FileError> {
    Ok(match inspect_mailbox(path, false)? {
        MailboxOwnership::Owned(owner) => Some(owner),
        _ => None,
    })
}

fn inspect_mailbox(path: &Path, offline: bool) -> Result<MailboxOwnership, FileError> {
    let unavailable = |detail: &str| {
        FileError::new(
            format!("{}: {detail}", path.display()),
            MAILBOX_READ_RECOVERY,
        )
    };
    match path.symlink_metadata() {
        Ok(meta) if !meta.is_file() => {
            return Err(unavailable("Expected a regular mailbox file"));
        }
        Ok(meta) if meta.len() == 0 => return Ok(MailboxOwnership::New),
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(MailboxOwnership::New),
        Err(err) => return Err(unavailable(&err.to_string())),
    }

    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let query = || -> rusqlite::Result<MailboxOwnership> {
        let conn = if offline {
            // Removal excludes store writers. SQLite's exclusive locking mode
            // keeps the WAL index in memory, and unix-none lets the read-only
            // connection use that mode without modifying the staged files.
            let conn = Connection::open_with_flags_and_vfs(path, flags, "unix-none")?;
            conn.execute_batch("PRAGMA locking_mode = EXCLUSIVE")?;
            conn
        } else {
            Connection::open_with_flags(path, flags)?
        };
        conn.busy_timeout(std::time::Duration::from_millis(50))?;
        let has_accounts: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = 'accounts')",
            [],
            |row| row.get(0),
        )?;
        if !has_accounts {
            return Ok(MailboxOwnership::Unowned("No accounts table"));
        }
        read_mailbox_owners(&conn, |_| true)
    };
    match query() {
        Err(error) if error.sqlite_error_code() == Some(ErrorCode::NotADatabase) => Ok(
            MailboxOwnership::Unowned("The file is not a SQLite database"),
        ),
        result => result.map_err(|err| unavailable(&err.to_string())),
    }
}

fn read_mailbox_owners(
    conn: &Connection,
    include: impl Fn(&str) -> bool,
) -> rusqlite::Result<MailboxOwnership> {
    let mut statement = conn.prepare("SELECT id, email FROM accounts")?;
    let mut rows = statement.query([])?;
    let mut owner = None;
    while let Some(row) = rows.next()? {
        let id: String = row.get(0)?;
        if include(&id) {
            let next = (id, row.get(1)?);
            if let Some(first) = owner {
                return Ok(MailboxOwnership::Shared(first, next));
            }
            owner = Some(next);
        }
    }
    Ok(owner.map_or(
        MailboxOwnership::Unowned("The accounts table is empty"),
        MailboxOwnership::Owned,
    ))
}

/// Recheck ownership in the account write transaction. The store has already
/// initialized its schema, and its demo cleanup handles the reserved rows.
pub(crate) fn validate_configure_account(
    conn: &Connection,
    path: Option<&Path>,
    account: &crate::Account,
) -> crate::Result<()> {
    let owners = read_mailbox_owners(conn, |id| !crate::backend::DEMO_ACCOUNT_IDS.contains(&id))?;
    if matches!(owners, MailboxOwnership::Unowned(_)) {
        return Ok(());
    }
    validate_ownership(
        owners,
        path.unwrap_or_else(|| Path::new("Local mailbox")),
        account.id.as_str(),
        Some(&account.email),
    )
    .map_err(|err| crate::Error::Invalid(err.cli_message()))
}

/// A new mailbox can be initialized. An existing mailbox can only be opened
/// or removed on behalf of its single recorded owner.
fn validate_mailbox(
    path: &Path,
    id: &str,
    email: Option<&str>,
    offline: bool,
) -> Result<(), FileError> {
    validate_ownership(inspect_mailbox(path, offline)?, path, id, email)
}

fn validate_ownership(
    owners: MailboxOwnership,
    path: &Path,
    id: &str,
    email: Option<&str>,
) -> Result<(), FileError> {
    let detail = match owners {
        MailboxOwnership::New => return Ok(()),
        MailboxOwnership::Owned((owner, address)) => {
            if owner != id {
                format!("Account {owner:?} ({address}) owns this mailbox. Requested ID: {id:?}")
            } else if email.is_some_and(|email| !address.eq_ignore_ascii_case(email)) {
                return Err(FileError::new(
                    format!(
                        "{}: Account {id:?} holds mail for {address}",
                        path.display()
                    ),
                    MAILBOX_ADDRESS_RECOVERY,
                ));
            } else {
                return Ok(());
            }
        }
        MailboxOwnership::Unowned(reason) => reason.to_owned(),
        MailboxOwnership::Shared((first, first_email), (second, second_email)) => {
            format!("Multiple owners: {first:?} ({first_email}) and {second:?} ({second_email})")
        }
    };
    Err(FileError::new(
        format!("{}: {detail}", path.display()),
        MAILBOX_OWNER_RECOVERY,
    ))
}

fn check_live_mailbox(
    path: &Path,
    id: &str,
    email: Option<&str>,
    backup: Option<&Path>,
) -> Result<(), FileError> {
    let trash = removal_trash(path);
    if path_exists(&trash).map_err(|err| FileError::access(&trash, err))? {
        return Err(FileError::new(
            format!("Removal of {id} is in progress at {}", trash.display()),
            INTERRUPTED_REMOVAL_RECOVERY,
        ));
    }
    validate_mailbox(path, id, email, false).map_err(|mut error| {
        if let Some(backup) = backup {
            error.detail = format!("{}: {}", file_name(backup), error.detail);
        }
        error
    })
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

fn publish(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<bool> {
    let mut staged = tempfile::NamedTempFile::new_in(dir)?;
    staged.write_all(bytes)?;
    staged.as_file().sync_all()?;
    match staged.persist_noclobber(path) {
        Ok(_) => Ok(true),
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
                    return Ok(false);
                }
            }
            Err(error.error)
        }
        Err(error) => Err(error.error),
    }
}

pub fn missing(path: &Path) -> FileError {
    FileError::new(
        format!("{} does not exist", file_name(path)),
        "Run fell account setup.",
    )
}

/// Read from one descriptor and reject special files and exposed passwords.
pub fn read(path: &Path, id: Option<&str>) -> Result<Option<(AccountConfig, String)>, FileError> {
    if id.is_some_and(|id| path.file_stem().and_then(|stem| stem.to_str()) != Some(id)) {
        return Err(FileError::new(
            format!("{} is not this account’s file", file_name(path)),
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
    read_account_contents(path, id)
}

fn read_account_contents(
    path: &Path,
    id: Option<&str>,
) -> Result<Option<(AccountConfig, String)>, FileError> {
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
                file_name(path)
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
            format!("{}: {detail}", file_name(path)),
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
            let registry = Registry::at(dir.clone(), root.path().join("data/fell/mail.db"));
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
        let registry = Registry::at(config.clone(), root.path().join("mail.db"));
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
    fn a_failed_account_publish_rolls_back_only_its_new_marker() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let base = root.path().join("mail.db");
        let registry = Registry::at(config.clone(), base.clone());
        let legacy = config.join("account.toml");
        let marker = registry.directory().join(".original-id");
        let target = registry.path("mail").unwrap();
        write_private(
            &legacy,
            "email = 'original@example.test'\npassword = 'fixture'\n",
        );
        assert!(
            registry
                .migrate_with(
                    Some(&base),
                    || {
                        assert_eq!(fs::read_to_string(&marker).unwrap(), "mail");
                        write_private(
                            &target,
                            "email = 'new@example.test'\npassword = 'fixture'\n",
                        );
                        Ok(())
                    },
                    || panic!("the conflicting account file must not be replaced"),
                )
                .is_err()
        );
        assert!(!marker.exists());
        assert!(legacy.exists());
        assert!(
            fs::read_to_string(&target)
                .unwrap()
                .contains("new@example.test")
        );
    }

    #[test]
    fn a_failed_target_cleanup_still_rolls_back_the_marker() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let base = root.path().join("mail.db");
        let registry = Registry::at(config.clone(), base.clone());
        let target = registry.path("mail").unwrap();
        let marker = registry.directory().join(".original-id");
        write_private(
            &config.join("account.toml"),
            "email = 'original@example.test'\npassword = 'fixture'\n",
        );
        let failure = registry
            .migrate_with(
                Some(&base),
                || Ok(()),
                || {
                    fs::rename(&target, config.join("published.toml")).unwrap();
                    fs::create_dir(&target).unwrap();
                    fs::create_dir(config.join("account.toml.bak")).unwrap();
                },
            )
            .unwrap_err();
        assert!(failure.detail.contains("Is a directory"));
        assert!(!marker.exists());
    }

    #[test]
    fn an_owned_base_mailbox_ignores_ownerless_siblings_and_rejects_other_owners() {
        let text = "email = 'original@example.test'\npassword = 'fixture'\n";
        let original = AccountConfig::parse(text).unwrap().account();
        for metadata in [
            None,
            Some("accounts/.original-id"),
            Some("account.toml"),
            Some("account.toml.bak"),
        ] {
            for leftover in [
                "zero bytes",
                "schema",
                "sidecar",
                "not sqlite",
                "same owner",
                "other email",
                "other id",
                "multiple owners",
            ] {
                let root = tempfile::tempdir().unwrap();
                let config = root.path().join("fell");
                let base = root.path().join("mail.db");
                let registry = Registry::at(config.clone(), base.clone());
                let sibling = sibling_mailbox(&base, "mail");
                fs::create_dir_all(sibling.parent().unwrap()).unwrap();
                crate::Store::open(&base)
                    .unwrap()
                    .upsert_account(&original)
                    .unwrap();
                match leftover {
                    "zero bytes" => fs::write(&sibling, []).unwrap(),
                    "sidecar" => fs::write(removal::mailbox_file(&sibling, "-wal"), []).unwrap(),
                    "not sqlite" => {
                        fs::write(&sibling, "not a SQLite database".repeat(10)).unwrap()
                    }
                    "schema" => drop(crate::Store::open(&sibling).unwrap()),
                    _ => {
                        let store = crate::Store::open(&sibling).unwrap();
                        let mut owner = original.clone();
                        if leftover == "same owner" {
                            owner.email = owner.email.to_ascii_uppercase();
                        }
                        if leftover == "other email" {
                            owner.email = "other@example.test".into();
                        }
                        if leftover == "multiple owners" {
                            store.upsert_account(&owner).unwrap();
                        }
                        if matches!(leftover, "other id" | "multiple owners") {
                            owner.id = AccountId("other".into());
                        }
                        store.upsert_account(&owner).unwrap();
                    }
                }
                if let Some(metadata) = metadata {
                    write_private(
                        &config.join(metadata),
                        if metadata.ends_with(".original-id") {
                            "mail"
                        } else {
                            text
                        },
                    );
                }
                if metadata != Some("account.toml") {
                    write_private(&registry.path("mail").unwrap(), text);
                }
                let route = registry.store_path(&base, "mail");
                if matches!(leftover, "other email" | "other id" | "multiple owners") {
                    assert!(
                        route.unwrap_err().is_mailbox_link(),
                        "{metadata:?} / {leftover}"
                    );
                    assert!(
                        registry
                            .load_with_removals(&base)
                            .default_config()
                            .is_none()
                    );
                } else {
                    assert_eq!(route.unwrap(), base, "{metadata:?} / {leftover}");
                    let catalog = registry.load_with_removals(&base);
                    assert!(catalog.error.is_none(), "{metadata:?} / {leftover}");
                    assert_eq!(catalog.default_config().unwrap().id, "mail");
                    assert_eq!(registry.store_path(&base, "mail").unwrap(), base);
                }
                assert_eq!(
                    mailbox_owner(&base).unwrap(),
                    Some((original.id.0.clone(), original.email.clone()))
                );
            }
        }
    }

    #[test]
    fn matching_email_cannot_rebind_the_base_mailbox_to_another_id() {
        for metadata in ["accounts/.original-id", "account.toml", "account.toml.bak"] {
            let root = tempfile::tempdir().unwrap();
            let config_dir = root.path().join("fell");
            let base = root.path().join("mail.db");
            let registry = Registry::at(config_dir.clone(), base.clone());
            let text = "id = 'work'\nemail = 'same@example.test'\npassword = 'fixture'";
            let config = AccountConfig::parse(text).unwrap();
            let mut owner = config.account();
            owner.id = AccountId("mail".into());
            crate::Store::open(&base)
                .unwrap()
                .upsert_account(&owner)
                .unwrap();
            let sibling = sibling_mailbox(&base, "work");
            fs::create_dir_all(sibling.parent().unwrap()).unwrap();
            fs::write(&sibling, []).unwrap();
            write_private(
                &config_dir.join(metadata),
                if metadata.ends_with(".original-id") {
                    "work"
                } else {
                    text
                },
            );
            if metadata != "account.toml" {
                write_private(&registry.path("work").unwrap(), text);
            }
            for error in [
                registry
                    .store_path(&base, "work")
                    .unwrap_err()
                    .cli_message(),
                registry
                    .configured_store_path(&base, &config)
                    .unwrap_err()
                    .cli_message(),
                registry.remove(&base, "work", true).unwrap_err(),
            ] {
                assert!(
                    error.contains(&*base.to_string_lossy()),
                    "{metadata}: {error}"
                );
                assert!(
                    error.contains("Account \"mail\" (same@example.test)"),
                    "{error}"
                );
            }
            let store = crate::Store::open(&base).unwrap();
            assert!(store.configure_account(&config.account(), false).is_err());
            assert_eq!(
                mailbox_owner(&base).unwrap(),
                Some((owner.id.0, owner.email))
            );
            assert_eq!(sibling.metadata().unwrap().len(), 0);
        }
    }

    #[test]
    fn a_configured_mailbox_requires_one_owner_with_the_saved_address() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("mail.db");
        let registry = Registry::at(root.path().join("fell"), base.clone());
        let text = "email = 'work@example.test'\npassword = 'fixture'";
        let config = AccountConfig::parse_for(text, "work").unwrap();
        write_private(&registry.path("work").unwrap(), text);
        let path = sibling_mailbox(&base, "work");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE other(id TEXT)").unwrap();
        let error = registry.store_path(&base, "work").unwrap_err();
        assert!(error.detail.contains("No accounts table"));
        conn.execute_batch("CREATE TABLE accounts(id TEXT, email TEXT)")
            .unwrap();
        let error = registry.configured_store_path(&base, &config).unwrap_err();
        assert!(error.detail.contains("accounts table is empty"));
        conn.execute(
            "INSERT INTO accounts VALUES ('work', 'old@example.test')",
            [],
        )
        .unwrap();
        let error = registry.store_path(&base, "work").unwrap_err();
        assert!(error.detail.contains("old@example.test"));
        assert_eq!(error.recovery, MAILBOX_ADDRESS_RECOVERY);
        let removal = registry.remove(&base, "work", false).unwrap_err();
        assert!(removal.contains("old@example.test"), "{removal}");
        conn.execute("UPDATE accounts SET email = 'WORK@example.test'", [])
            .unwrap();
        assert_eq!(
            registry.configured_store_path(&base, &config).unwrap(),
            path
        );
        conn.execute(
            "INSERT INTO accounts VALUES ('other', 'work@example.test')",
            [],
        )
        .unwrap();
        let error = registry.configured_store_path(&base, &config).unwrap_err();
        assert!(error.detail.contains("Multiple owners"), "{error}");
        assert!(error.detail.contains("other"), "{error}");
    }

    #[test]
    fn a_sibling_unreadable_for_ownership_does_not_block_the_base() {
        for unreadable in ["symlink", "corrupt", "busy"] {
            let root = tempfile::tempdir().unwrap();
            let base = root.path().join("mail.db");
            let registry = Registry::at(root.path().join("fell"), base.clone());
            let text = "email = 'mail@example.test'\npassword = 'fixture'";
            let account = AccountConfig::parse(text).unwrap();
            write_private(&registry.path("mail").unwrap(), text);
            crate::Store::open(&base)
                .unwrap()
                .upsert_account(&account.account())
                .unwrap();
            let sibling = sibling_mailbox(&base, "mail");
            fs::create_dir_all(sibling.parent().unwrap()).unwrap();
            let mut conn = None;
            if unreadable == "symlink" {
                std::os::unix::fs::symlink(&base, &sibling).unwrap();
            } else {
                let db = Connection::open(&sibling).unwrap();
                db.execute_batch("CREATE TABLE accounts(id TEXT, email TEXT)")
                    .unwrap();
                if unreadable == "corrupt" {
                    drop(db);
                    let mut bytes = fs::read(&sibling).unwrap();
                    bytes[100] = 0;
                    fs::write(&sibling, bytes).unwrap();
                } else {
                    db.execute_batch("BEGIN EXCLUSIVE").unwrap();
                    conn = Some(db);
                }
            }
            let error =
                validate_mailbox(&sibling, "mail", Some(&account.email), false).unwrap_err();
            assert_eq!(
                error.recovery, MAILBOX_READ_RECOVERY,
                "{unreadable}: {error}"
            );
            assert!(error.detail.contains(&*sibling.to_string_lossy()));
            assert_eq!(
                registry.configured_store_path(&base, &account).unwrap(),
                base
            );
            assert_eq!(
                registry.load_with_removals(&base).default_config(),
                Some(&account)
            );
            drop(conn);
        }
    }

    #[test]
    fn a_lost_or_invalid_marker_uses_the_migration_backup() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let registry = Registry::at(config.clone(), root.path().join("mail.db"));
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
    fn invalid_original_metadata_blocks_only_the_owner_of_the_base_db() {
        for metadata in ["account.toml.bak", "accounts/.original-id"] {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("fell");
            let registry = Registry::at(config.clone(), root.path().join("mail.db"));
            let base = root.path().join("mail.db");
            let personal =
                "id = 'personal'\nemail = 'personal@example.test'\npassword = 'fixture'\n";
            write_private(&registry.path("personal").unwrap(), personal);
            write_private(
                &registry.path("work").unwrap(),
                "email = 'work@example.test'\npassword = 'fixture'\n",
            );
            let db = crate::Store::open(&base).unwrap();
            db.upsert_account(&AccountConfig::parse(personal).unwrap().account())
                .unwrap();
            drop(db);
            write_private(&config.join(metadata), "invalid = [");
            let work = registry.store_path(&base, "work").unwrap();
            assert_eq!(work, base.with_extension("accounts").join("work.db"));
            assert!(registry.store_path(&base, "personal").is_err());
            assert!(registry.remove(&base, "personal", true).is_err());
            assert!(base.is_file());

            fs::write(config.join("accounts/.original-id"), "personal\n").unwrap();
            assert_eq!(registry.store_path(&base, "personal").unwrap(), base);
            assert_eq!(registry.store_path(&base, "work").unwrap(), work);
        }
    }

    #[test]
    fn damaged_original_metadata_is_listed_with_or_without_an_account_file() {
        for metadata in ["account.toml.bak", "accounts/.original-id"] {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("fell");
            let base = root.path().join("mail.db");
            let registry = Registry::at(config.clone(), base.clone());
            fs::write(&base, "unreadable SQLite").unwrap();
            let damaged = config.join(metadata);
            write_private(&damaged, "invalid = [");
            assert!(registry.mailbox_path(&base, "mail").is_err());
            assert!(registry.mailbox_reserved(&base, "mail").unwrap());
            assert!(!registry.mailbox_reserved(&base, "work").unwrap());
            let catalog = registry.load_with_removals(&base);
            assert!(!catalog.is_empty());
            let [entry] = catalog.entries.as_slice() else {
                panic!("expected a damaged-link entry");
            };
            assert_eq!(entry.path, damaged);
            assert!(entry.config.as_ref().unwrap_err().is_mailbox_link());
            assert!(entry.file_identity.is_none());

            for id in ["mail", "work"] {
                write_private(
                    &registry.path(id).unwrap(),
                    "name = 'Harbor Desk'\nemail = 'synthetic@example.test'\npassword = 'fixture'\n",
                );
            }
            let catalog = registry.load_with_removals(&base);
            assert_eq!(catalog.default_config().unwrap().id, "work");
            let entry = &catalog.entries[0];
            assert_eq!(entry.path, registry.path("mail").unwrap());
            assert_eq!(entry.damaged_link_name(), Some("Harbor Desk"));
            assert!(entry.config.as_ref().unwrap_err().is_mailbox_link());
            assert!(entry.file_identity.is_none());
        }
    }

    #[test]
    fn a_broken_legacy_leftover_is_listed_without_blocking_mailboxes() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let registry = Registry::at(config.clone(), root.path().join("mail.db"));
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
    fn a_markerless_registry_routes_around_a_broken_legacy_file() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let registry = Registry::at(config.clone(), root.path().join("mail.db"));
        let legacy = config.join("account.toml");
        let base = root.path().join("mail.db");
        let live = base.with_extension("accounts").join("mail.db");
        for (path, email) in [(&base, "old@example.test"), (&live, "live@example.test")] {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            let db = crate::Store::open(path).unwrap();
            let config = AccountConfig::bridge(
                email,
                email,
                crate::PasswordSource::Inline("fixture".into()),
            );
            db.upsert_account(&config.account()).unwrap();
        }
        write_private(&legacy, "invalid = [");
        for id in ["mail", "work"] {
            write_private(
                &registry.path(id).unwrap(),
                "email = 'live@example.test'\npassword = 'fixture'\n",
            );
            assert_eq!(
                registry.store_path(&base, id).unwrap(),
                base.with_extension("accounts").join(format!("{id}.db"))
            );
        }
        let catalog = registry.load_with_removals(&base);
        assert!(catalog.error.is_none());
        assert_eq!(catalog.configs().count(), 2);
        let broken = catalog
            .entries
            .iter()
            .find(|entry| entry.path == legacy)
            .unwrap();
        let error = broken.config.as_ref().unwrap_err();
        assert!(error.detail.starts_with("account.toml:"));
        assert!(!error.detail.contains(&*config.to_string_lossy()));
        assert_eq!(fs::read_to_string(legacy).unwrap(), "invalid = [");
        assert!(
            registry
                .remove_if_unchanged(&base, "mail", true, None)
                .is_err()
        );
        assert!(registry.recover_removal(&base, "mail", true, None).is_err());
        assert!(live.is_file());
        assert_eq!(mailbox_owner(&base).unwrap().unwrap().1, "old@example.test");

        // Fixing the legacy file must neither redirect the live account nor
        // leave a marker for a failed migration.
        fs::write(
            config.join("account.toml"),
            "email = 'old@example.test'\npassword = 'fixture'\n",
        )
        .unwrap();
        assert!(registry.migrate().is_err());
        let marker = registry.directory().join(".original-id");
        assert!(!marker.exists());
        assert_eq!(registry.store_path(&base, "mail").unwrap(), live);
        assert_eq!(registry.mailbox_path(&base, "mail").unwrap(), live);

        // An installation affected by the old migration bug fails closed.
        write_private(&marker, "mail");
        assert!(registry.store_path(&base, "mail").is_err());
        assert!(registry.remove(&base, "mail", true).is_err());
        let catalog = registry.load_with_removals(&base);
        assert_eq!(catalog.default_config().unwrap().id, "work");
        assert!(
            catalog.entries[0]
                .config
                .as_ref()
                .unwrap_err()
                .is_mailbox_link()
        );
        assert_eq!(
            mailbox_owner(&live).unwrap().unwrap().1,
            "live@example.test"
        );
        assert_eq!(mailbox_owner(&base).unwrap().unwrap().1, "old@example.test");
    }

    #[test]
    fn a_missing_legacy_config_does_not_rebind_a_base_db_with_another_owner() {
        let root = tempfile::tempdir().unwrap();
        let registry = Registry::at(root.path().join("fell"), root.path().join("mail.db"));
        let base = root.path().join("mail.db");
        let mut original = AccountConfig::bridge(
            "other@example.test",
            "other@example.test",
            crate::PasswordSource::Inline("fixture".into()),
        );
        original.id = "other".into();
        let db = crate::Store::open(&base).unwrap();
        db.upsert_account(&original.account()).unwrap();
        drop(db);
        write_private(
            &registry.path("mail").unwrap(),
            "email = 'mail@example.test'\npassword = 'fixture'\n",
        );
        assert_eq!(
            registry.store_path(&base, "mail").unwrap(),
            base.with_extension("accounts").join("mail.db")
        );
    }

    #[test]
    fn a_backup_id_must_match_the_mailbox_before_binding_or_removal() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let registry = Registry::at(config.clone(), root.path().join("mail.db"));
        let base = root.path().join("mail.db");
        let verified = "id = 'personal'\nemail = 'personal@example.test'\npassword = 'fixture'\n";
        write_private(&config.join("account.toml.bak"), verified);
        write_private(&registry.path("personal").unwrap(), verified);
        write_private(
            &registry.path("work").unwrap(),
            "email = 'work@example.test'\npassword = 'fixture'\n",
        );
        let db = crate::Store::open(&base).unwrap();
        let mut owner = AccountConfig::parse(verified).unwrap().account();
        owner.id = AccountId("other".into());
        db.upsert_account(&owner).unwrap();
        drop(db);
        assert!(registry.store_path(&base, "personal").is_err());
        assert!(registry.remove(&base, "personal", true).is_err());
        assert!(base.is_file());
        assert_ne!(registry.store_path(&base, "work").unwrap(), base);
        let catalog = registry.load_with_removals(&base);
        assert_eq!(
            catalog.configs().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["work"]
        );
        let error = catalog.entries[0].config.as_ref().unwrap_err();
        assert!(error.detail.starts_with("account.toml.bak:"));
    }

    #[test]
    fn a_reader_during_migration_sees_the_legacy_mailbox() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("fell");
        let registry = Registry::at(config.clone(), root.path().join("mail.db"));
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
            let mailbox = &base;
            let migrating = scope.spawn(move || {
                registry.migrate_with(
                    Some(mailbox),
                    || Ok(()),
                    || {
                        published.send(()).unwrap();
                        done.recv().unwrap();
                    },
                )
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
        let registry = Registry::at(root.path().join("fell"), root.path().join("mail.db"));
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
