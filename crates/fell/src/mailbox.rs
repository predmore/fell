//! Open the local store and read the account file.

use std::env;
use std::ffi::OsStr;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::{fs, io};

pub(crate) use fell_core::accounts::FileError as AccountConfigError;
use fell_core::accounts::{self, Registry};
use fell_core::{Account, AccountConfig, AccountId, Store};

use crate::fixtures::{fixture_account, seed_if_empty};
use crate::keymap::{self, Keymap};
use crate::model::SyncKind;

/// What the window reads before its first frame: small config files only.
pub struct Startup {
    pub keymap: Keymap,
    pub split: f32,
    /// Legacy `read_dwell_ms` from ui.toml: how long a thread stays open before it
    /// is marked read. 0, the default, marks it as it opens.
    pub read_dwell_ms: u64,
    pub error: Option<String>,
    pub account_missing: bool,
    pub config: Option<AccountConfig>,
    pub path: Result<PathBuf, String>,
}

/// The mailbox the store thread opened.
pub struct Opened {
    pub account: Account,
    pub sync: SyncKind,
    pub error: Option<String>,
}

pub fn startup() -> Startup {
    let registry = Registry::resolve();
    let base = store_path();
    let accounts = registry.as_ref().ok().map(|registry| match &base {
        Ok(base) => registry.load_with_removals(base),
        Err(_) => registry.load(),
    });
    let config = accounts
        .as_ref()
        .and_then(|accounts| accounts.default_config())
        .cloned();
    let account_missing = accounts.as_ref().is_some_and(accounts::Accounts::is_empty);
    let config_error = registry.as_ref().err().map(ToString::to_string);
    let path = base.and_then(|base| match (&registry, &config) {
        (Ok(registry), Some(config)) => registry
            .store_path(&base, &config.id)
            .map_err(|err| err.cli_message()),
        _ if accounts
            .as_ref()
            .is_some_and(|accounts| !accounts.is_empty()) =>
        {
            Err("No readable account configuration".into())
        }
        _ => Ok(base),
    });
    startup_for(config, account_missing, config_error, path)
}

pub fn startup_for(
    config: Option<AccountConfig>,
    account_missing: bool,
    config_error: Option<String>,
    path: Result<PathBuf, String>,
) -> Startup {
    let (keymap, keymap_error) = load_keymap();
    let split = load_split();
    let read_dwell_ms = load_read_dwell();
    Startup {
        keymap,
        split,
        read_dwell_ms,
        error: join_errors([keymap_error, config_error]),
        account_missing,
        config,
        path,
    }
}

pub fn open_registered_store(
    path: Result<PathBuf, String>,
    config: Option<&AccountConfig>,
    demo: bool,
) -> (Store, Opened) {
    let locked = (|| {
        let Some(config) = config else {
            return Ok(None);
        };
        let registry = Registry::resolve().map_err(|err| err.to_string())?;
        let base = store_path()?;
        let lock = registry
            .lock_for_mailbox_open(&base, &config.id)
            .map_err(|err| err.cli_message())?;
        let account = AccountId(config.id.clone());
        if registry
            .get(&account)
            .map_err(|err| err.to_string())?
            .as_ref()
            .is_none_or(|current| !config.same_except_identities(current))
        {
            return Err("The account file changed while opening the mailbox.".into());
        }
        let current = registry
            .configured_store_path(&base, config)
            .map_err(|err| err.cli_message())?;
        if path.as_ref().is_ok_and(|path| *path != current) {
            return Err("The mailbox link changed while opening the account.".into());
        }
        Ok(Some((lock, current)))
    })();
    match locked {
        Ok(Some((_lock, current))) => open_store(Ok(current), config, demo),
        Ok(None) => open_store(path, config, demo),
        Err(error) => fallback(error),
    }
}

/// Open the store and pick the account. Runs on the store thread: the first
/// open of a large mailbox can replay its log and take seconds.
pub fn open_store(
    path: Result<PathBuf, String>,
    config: Option<&AccountConfig>,
    demo: bool,
) -> (Store, Opened) {
    let opened = path
        .and_then(|path| open_at(&path).map_err(|err| err.to_string()))
        .and_then(|store| {
            let opened = prepare(&store, config, demo).map_err(|err| err.to_string())?;
            Ok((store, opened))
        });
    match opened {
        Ok(result) => result,
        Err(error) => fallback(error),
    }
}

fn join_errors<const N: usize>(errors: [Option<String>; N]) -> Option<String> {
    let lines: Vec<String> = errors.into_iter().flatten().collect();
    if lines.is_empty() {
        None
    } else {
        Some(lines.join("\n"))
    }
}

pub(crate) fn next_account_id() -> Result<String, String> {
    let registry = Registry::resolve().map_err(|err| err.to_string())?;
    registry.migrate().map_err(|err| err.to_string())?;
    let base = store_path()?;
    next_account_id_at(&registry, &base)
}

fn next_account_id_at(registry: &Registry, base: &Path) -> Result<String, String> {
    for number in 1.. {
        let id = if number == 1 {
            "mail".into()
        } else {
            format!("mail-{number}")
        };
        if account_id_available(registry, base, &id)? {
            return Ok(id);
        }
    }
    unreachable!()
}

fn account_id_available(registry: &Registry, base: &Path, id: &str) -> Result<bool, String> {
    let path = registry.path(id).map_err(|err| err.to_string())?;
    if accounts::path_exists(&path).map_err(|err| err.to_string())? {
        return Ok(false);
    }
    registry
        .mailbox_reserved(base, id)
        .map(|reserved| !reserved)
        .map_err(|err| err.to_string())
}

pub(crate) fn retained_mailbox(config: &AccountConfig) -> Option<String> {
    let registry = Registry::resolve().ok()?;
    let base = store_path().ok()?;
    let original = accounts::mailbox_owner(&base)
        .ok()
        .flatten()
        .map(|(id, _)| id);
    let siblings = fs::read_dir(base.with_extension("accounts"))
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let name = entry.ok()?.file_name();
            Some(name.to_str()?.strip_suffix(".db")?.to_owned())
        });
    // Offer an unambiguous match only. The user can name a specific ID in
    // the CLI when they have kept multiple mailboxes with the same address.
    let mut candidates: Vec<_> = original
        .into_iter()
        .chain(siblings)
        .filter(|id| can_reconnect_at(&registry, &base, id, &config.email, &config.username))
        .collect();
    candidates.sort();
    candidates.dedup();
    if candidates.len() == 1 {
        candidates.pop()
    } else {
        None
    }
}

pub(crate) fn can_reconnect(config: &AccountConfig) -> bool {
    Registry::resolve()
        .ok()
        .zip(store_path().ok())
        .is_some_and(|(registry, base)| {
            can_reconnect_at(
                &registry,
                &base,
                &config.id,
                &config.email,
                &config.username,
            )
        })
}

pub(crate) fn queued_connection_changes(config: &AccountConfig) -> (usize, usize) {
    let read = || -> Option<_> {
        let path = Registry::resolve()
            .ok()?
            .configured_store_path(&store_path().ok()?, config)
            .ok()?;
        let store = Store::open_existing(&path).ok()?;
        let account = AccountId(config.id.clone());
        let roles = store.folder_roles(&account).ok()?;
        let count = store.queued_write_count(&account).ok()?;
        let archives = store
            .outbox(&account)
            .ok()?
            .into_iter()
            .filter(|row| {
                row.state == fell_core::OutboxState::Pending
                    && row.kind == fell_core::OutboxKind::Move
                    && roles.is(fell_core::FolderRole::Archive, &row.target_folder)
            })
            .count();
        Some((count, archives))
    };
    read().unwrap_or_default()
}

fn can_reconnect_at(
    registry: &Registry,
    base: &Path,
    id: &str,
    email: &str,
    username: &str,
) -> bool {
    registry
        .path(id)
        .is_ok_and(|path| accounts::path_exists(&path).is_ok_and(|exists| !exists))
        && registry.store_path(base, id).is_ok_and(|path| {
            accounts::mailbox_owner(&path).is_ok_and(|owner| {
                owner.is_some_and(|(stored_id, stored_email)| {
                    stored_id == id && stored_email.eq_ignore_ascii_case(email)
                })
            }) && Store::matches_imap_login(&path, id, username).unwrap_or(false)
        })
}

#[cfg(test)]
fn read_account_config(path: &Path) -> Result<Option<AccountConfig>, AccountConfigError> {
    Ok(accounts::read(path, None)?.map(|(config, _)| config))
}

/// The account file a send from `account` takes its login and SMTP settings
/// from, read now with the same mode check as at startup. Never the draft
/// folder or anything an agent can write. The demo mailbox needs none.
pub(crate) fn send_account_config(
    account: &AccountId,
) -> Result<Option<AccountConfig>, AccountConfigError> {
    if fell_core::is_demo_account(account) {
        return Ok(None);
    }
    let registry = Registry::resolve()?;
    let config = registry.get(account)?.ok_or_else(|| {
        registry
            .path(account.as_str())
            .map_or_else(|err| err, |path| accounts::missing(&path))
    })?;
    if let Ok(base) = store_path() {
        registry.configured_store_path(&base, &config)?;
    }
    Ok(Some(config))
}

#[cfg(test)]
fn send_config_at(path: &Path, account: &AccountId) -> Result<AccountConfig, AccountConfigError> {
    accounts::read(path, Some(account.as_str()))?
        .map(|(config, _)| config)
        .ok_or_else(|| accounts::missing(path))
}

pub fn load_account_file(id: Option<&str>) -> Result<(PathBuf, AccountConfig), String> {
    let registry = Registry::resolve().map_err(|err| err.to_string())?;
    if let Some(id) = id {
        let path = registry.path(id).map_err(|err| err.to_string())?;
        return registry
            .get(&AccountId(id.to_owned()))
            .map_err(|err| err.to_string())?
            .map(|config| (path.clone(), config))
            .ok_or_else(|| accounts::missing(&path).to_string());
    }
    let accounts = match store_path() {
        Ok(base) => registry.load_with_removals(&base),
        Err(_) => registry.load(),
    };
    if let Some(config) = accounts.default_config() {
        let path = registry.path(&config.id).map_err(|err| err.to_string())?;
        return Ok((path, config.clone()));
    }
    Err(accounts
        .error
        .as_ref()
        .or_else(|| {
            accounts
                .entries
                .iter()
                .find_map(|entry| entry.config.as_ref().err())
        })
        .map_or_else(
            || "No account file. Run fell account setup.".into(),
            ToString::to_string,
        ))
}

#[derive(Debug)]
pub(crate) enum AccountFileError {
    AlreadyConfigured,
    MailboxAddressConflict { id: String, email: String },
    Write(String),
}

impl From<String> for AccountFileError {
    fn from(detail: String) -> Self {
        Self::Write(detail)
    }
}

impl From<&str> for AccountFileError {
    fn from(detail: &str) -> Self {
        detail.to_owned().into()
    }
}

impl std::fmt::Display for AccountFileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyConfigured => {
                f.write_str("An account is already configured. Reopen Fell to use it.")
            }
            Self::MailboxAddressConflict { id, email } => write!(
                f,
                "{id} already holds mail for {email} · run with --replace to start an empty mailbox, or use the same address"
            ),
            Self::Write(detail) => f.write_str(detail),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AccountSave {
    New,
    Update,
    Reconnect,
    ReplaceMailbox,
}

/// Prepare the file so setup can save its keyring password before publishing
/// the new config with mode 0600. Dropping the staged file keeps the current
/// config in place.
#[cfg(test)]
pub(crate) fn stage_account_config(
    config: &mut AccountConfig,
    save: AccountSave,
) -> Result<StagedAccountConfig, AccountFileError> {
    let expected = Registry::resolve()
        .and_then(|registry| registry.get_with_identity(&AccountId(config.id.clone())))
        .map_err(|err| err.to_string())?
        .map(|(_, identity)| identity);
    stage_connection_config(config, save, expected)
}

pub(crate) fn stage_connection_config(
    config: &mut AccountConfig,
    save: AccountSave,
    expected: Option<accounts::FileIdentity>,
) -> Result<StagedAccountConfig, AccountFileError> {
    let registry = Registry::resolve().map_err(|err| err.to_string())?;
    registry.migrate().map_err(|err| err.to_string())?;
    let mut path = registry.path(&config.id).map_err(|err| err.to_string())?;
    let lock = lock_account_dir(&path)?;
    let base = store_path()?;
    let mailbox = registry
        .store_path(&base, &config.id)
        .map_err(|err| err.cli_message())?;
    let mut configured = accounts::path_exists(&path).map_err(|err| err.to_string())?;
    if configured && save == AccountSave::New {
        return Err(AccountFileError::AlreadyConfigured);
    }
    let current =
        accounts::read_with_identity(&path, Some(&config.id)).map_err(|err| err.to_string())?;
    if current.as_ref().map(|(_, _, identity)| *identity) != expected {
        return Err("The account file changed. Reopen Edit connection and try again.".into());
    }
    let mut keep = None;
    let different_owner = accounts::mailbox_owner(&mailbox)
        .map_err(|err| err.to_string())?
        .map(|(_, email)| email)
        .filter(|email| !email.eq_ignore_ascii_case(&config.email))
        .or_else(|| {
            current.as_ref().and_then(|(current, _, _)| {
                (!current.same_mailbox(config)).then(|| current.email.clone())
            })
        });
    if save == AccountSave::Reconnect {
        if !can_reconnect_at(
            &registry,
            &base,
            &config.id,
            &config.email,
            &config.username,
        ) {
            return Err("The kept mailbox changed. Review the account and try again.".into());
        }
        config.write_back =
            Store::stored_write_back(&mailbox, &config.id).map_err(|err| err.to_string())?;
    } else if let Some(email) = different_owner {
        if save != AccountSave::ReplaceMailbox {
            return Err(AccountFileError::MailboxAddressConflict {
                id: config.id.clone(),
                email,
            });
        }
        let next = next_account_id_at(&registry, &base)?;
        let previous = std::mem::replace(&mut config.id, next);
        config.identities.clear();
        config.folders.clear();
        config.save_sent = Default::default();
        config.write_back = false;
        if let Some((_, snapshot, _)) = &current {
            keep = Some((previous, snapshot.clone()));
        }
        path = registry.path(&config.id).map_err(|err| err.to_string())?;
        configured = false;
    } else if !configured && accounts::mailbox_exists(&mailbox).map_err(|err| err.to_string())? {
        return Err("This ID has a local mailbox. Choose a new ID or confirm reconnecting the same account.".into());
    }
    if let Some((previous, _, _)) = &current
        && accounts::mailbox_exists(&mailbox).map_err(|err| err.to_string())?
    {
        fell_core::Store::remember_imap_connection(&mailbox, previous)
            .map_err(|err| err.to_string())?;
    }
    let mut staged = stage_account_text_at(path, &config.render(), !configured, lock)?;
    if configured && let Some((_, snapshot, _)) = &current {
        staged.backup = Some(stage_account_file(&staged.path, snapshot)?);
    }
    if let Some((id, snapshot)) = keep {
        staged.keep = Some((registry, base, id, snapshot));
    } else if !configured && save != AccountSave::Reconnect {
        staged.original = Some((registry, base, config.id.clone()));
    }
    Ok(staged)
}

pub(crate) struct StagedAccountConfig {
    path: PathBuf,
    file: tempfile::NamedTempFile,
    first_run: bool,
    backup: Option<tempfile::NamedTempFile>,
    lock: fs::File,
    original: Option<(Registry, PathBuf, String)>,
    keep: Option<(Registry, PathBuf, String, String)>,
}

impl StagedAccountConfig {
    pub(crate) fn publish(self) -> Result<PathBuf, AccountFileError> {
        self.publish_then(|result, _| result)
    }

    pub(crate) fn publish_then<T>(
        self,
        finish: impl FnOnce(Result<PathBuf, AccountFileError>, bool) -> T,
    ) -> T {
        let mut published = false;
        let result = (|| {
            let marker = match self.original {
                Some((registry, base, id)) => registry
                    .initialize_original(&self.lock, &base, &id)
                    .map_err(|err| err.to_string())?,
                None => None,
            };
            let installed = if self.first_run {
                self.file.persist_noclobber(&self.path)
            } else {
                self.file.persist(&self.path)
            };
            if let Err(error) = installed {
                if let Some(marker) = marker {
                    fs::remove_file(&marker)
                        .and_then(|()| self.lock.sync_all())
                        .map_err(|err| format!("{}: {err}", marker.display()))?;
                }
                return Err(
                    if self.first_run && error.error.kind() == std::io::ErrorKind::AlreadyExists {
                        AccountFileError::AlreadyConfigured
                    } else {
                        format!("{}: {}", self.path.display(), error.error).into()
                    },
                );
            }
            published = true;
            if let Err(error) = self.lock.sync_all() {
                let rollback = match self.backup {
                    Some(backup) => backup
                        .persist(&self.path)
                        .map(|_| ())
                        .map_err(|err| err.error),
                    None if self.first_run => fs::remove_file(&self.path),
                    None => return Err(error.to_string().into()),
                };
                rollback
                    .map_err(|err| format!("{error}; couldn't restore the account file: {err}"))?;
                published = false;
                if let Some(marker) = marker {
                    fs::remove_file(marker).map_err(|err| err.to_string())?;
                }
                self.lock.sync_all().map_err(|err| err.to_string())?;
                return Err(error.to_string().into());
            }
            if let Some((registry, base, id, snapshot)) = self.keep
                && let Err(error) = registry.keep_for_replacement(&self.lock, &base, &id, &snapshot)
            {
                // If disconnect never happened, roll back the new account file.
                // A completed disconnect leaves the old mailbox available to reconnect.
                let old = registry.path(&id).map_err(|err| err.to_string())?;
                if accounts::path_exists(&old).map_err(|err| err.to_string())? {
                    fs::remove_file(&self.path).map_err(|err| {
                        format!("{error}; couldn't roll back the new account: {err}")
                    })?;
                    published = false;
                    self.lock.sync_all().map_err(|err| err.to_string())?;
                }
                return Err(error.into());
            }
            Ok(self.path)
        })();
        // The registry lock also covers retiring or rolling back the credential.
        let output = finish(result, published);
        drop(self.lock);
        output
    }
}

#[cfg(test)]
pub(crate) fn stage_account_config_at(
    path: PathBuf,
    config: &AccountConfig,
    first_run: bool,
) -> Result<StagedAccountConfig, AccountFileError> {
    let lock = lock_account_dir(&path)?;
    stage_account_text_at(path, &config.render(), first_run, lock)
}

/// Import only the addresses the user checked, preserving comments and
/// settings in the current file. Serializes with other Fell account writes.
pub(crate) fn import_identities(
    account: &AccountId,
    addresses: Vec<String>,
) -> Result<AccountConfig, String> {
    let registry = Registry::resolve().map_err(|err| err.to_string())?;
    registry.get(account).map_err(|err| err.to_string())?;
    let path = registry
        .path(account.as_str())
        .map_err(|err| err.to_string())?;
    import_identities_at(path, account, addresses)
}

fn import_identities_at(
    path: PathBuf,
    account: &AccountId,
    addresses: Vec<String>,
) -> Result<AccountConfig, String> {
    let lock = lock_account_dir(&path).map_err(|err| err.to_string())?;
    let (config, mut text) = accounts::read(&path, Some(account.as_str()))
        .map_err(|err| err.to_string())?
        .ok_or("No account file")?;
    if config.id != account.as_str() {
        return Err("The account changed. Open Accounts and review the addresses again.".into());
    }
    let known = config.compose_identities();
    let identities: Vec<_> = addresses
        .into_iter()
        .filter(|address| {
            !known
                .iter()
                .any(|identity| identity.address.eq_ignore_ascii_case(address))
        })
        .map(|address| fell_core::Identity {
            address,
            name: String::new(),
            signature: None,
        })
        .collect();
    if identities.is_empty() {
        return Ok(config);
    }
    if !text.ends_with('\n') {
        text.push('\n');
    }
    AccountConfig::render_identities(&mut text, &identities);
    let updated =
        accounts::parse(&path, &text, Some(account.as_str())).map_err(|err| err.to_string())?;
    stage_account_text_at(path, &text, false, lock)
        .and_then(StagedAccountConfig::publish)
        .map_err(|err| err.to_string())?;
    Ok(updated)
}

fn lock_account_dir(path: &Path) -> Result<fs::File, AccountFileError> {
    let dir = path.parent().expect("account file has a parent directory");
    // Serialize setup through publication, since a concurrent attempt could
    // otherwise replace the credential while this config is being written.
    let lock = private_config_dir(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    lock.try_lock()
        .map_err(|err| format!("Couldn't lock the account directory: {err}"))?;
    Ok(lock)
}

fn stage_account_text_at(
    path: PathBuf,
    config: &str,
    first_run: bool,
    lock: fs::File,
) -> Result<StagedAccountConfig, AccountFileError> {
    if first_run {
        match path.symlink_metadata() {
            Ok(_) => return Err(AccountFileError::AlreadyConfigured),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("{}: {err}", path.display()).into()),
        }
    }
    let file = stage_account_file(&path, config)?;
    Ok(StagedAccountConfig {
        path,
        file,
        first_run,
        backup: None,
        lock,
        original: None,
        keep: None,
    })
}

fn stage_account_file(
    path: &Path,
    config: &str,
) -> Result<tempfile::NamedTempFile, AccountFileError> {
    use std::io::Write;

    let dir = path.parent().expect("account file has a parent directory");
    let mut file =
        tempfile::NamedTempFile::new_in(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    file.write_all(config.as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|err| format!("{}: {err}", file.path().display()))?;
    Ok(file)
}

fn fallback(error: String) -> (Store, Opened) {
    let store = Store::open_in_memory().unwrap_or_else(|_| {
        panic!("in-memory store");
    });
    (
        store,
        Opened {
            account: Account {
                id: AccountId(String::new()),
                display_name: String::new(),
                email: String::new(),
                backend_kind: fell_core::BackendKind::Imap,
            },
            sync: SyncKind::StoreError,
            error: Some(error),
        },
    )
}

fn prepare(store: &Store, config: Option<&AccountConfig>, demo: bool) -> fell_core::Result<Opened> {
    if let Some(config) = config {
        let account = config.account();
        store.configure_connection(config)?;
        return Ok(Opened {
            account,
            sync: SyncKind::Connecting,
            error: None,
        });
    }
    let accounts = store.list_accounts()?;
    if demo && accounts.is_empty() {
        let account = fixture_account();
        seed_if_empty(store, &account)?;
        return Ok(Opened {
            account,
            sync: SyncKind::FixtureOffline,
            error: None,
        });
    }
    let account = accounts
        .into_iter()
        .find(|account| demo || !fell_core::is_demo_account(&account.id))
        .ok_or_else(|| fell_core::Error::Invalid("No readable account configuration.".into()))?;
    let sync = if fell_core::is_demo_account(&account.id) {
        SyncKind::FixtureOffline
    } else if store.list_threads(&account.id)?.is_empty() {
        SyncKind::FirstSync
    } else {
        SyncKind::LocalOffline
    };
    Ok(Opened {
        account,
        sync,
        error: None,
    })
}

fn open_at(path: &Path) -> fell_core::Result<Store> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        create_mailbox_dirs(parent)?;
    }
    let store = Store::open(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|err| fell_core::Error::Backend(err.to_string()))?;
    Ok(store)
}

/// Create missing directories for the mailbox and restrict only those new
/// directories to 0700. An existing parent, including a home directory or
/// `/tmp`, keeps the mode it already has.
pub(crate) fn create_mailbox_dirs(dir: &Path) -> std::io::Result<()> {
    let mut missing = Vec::new();
    let mut current = dir;
    loop {
        if current.as_os_str().is_empty() || current.exists() {
            break;
        }
        missing.push(current.to_path_buf());
        match current.parent() {
            Some(parent) if parent != current => current = parent,
            _ => break,
        }
    }
    for path in missing.iter().rev() {
        match fs::DirBuilder::new().mode(0o700).create(path) {
            Ok(()) => restrict_permissions(&open_directory(path)?, 0o700)?,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn open_directory(dir: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(dir)
}

pub(crate) fn private_config_dir(dir: &Path) -> io::Result<fs::File> {
    create_mailbox_dirs(dir)?;
    let file = open_directory(dir)?;
    restrict_permissions(&file, 0o700)?;
    Ok(file)
}

pub(crate) fn restrict_permissions(file: &fs::File, mode: u32) -> io::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no arguments and returns the current effective user.
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Fell configuration must be owned by you",
        ));
    }
    if metadata.mode() & 0o7777 != mode {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

pub fn store_path() -> Result<PathBuf, String> {
    store_path_from(
        env::var_os("FELL_STORE").as_deref(),
        env::var_os("XDG_DATA_HOME").as_deref(),
        env::var_os("HOME").as_deref(),
    )
}

fn store_path_from(
    fell_store: Option<&OsStr>,
    xdg_data: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf, String> {
    if let Some(path) = fell_store {
        return Ok(PathBuf::from(path));
    }
    if let Some(data) = xdg_data {
        return Ok(PathBuf::from(data).join("fell/mail.db"));
    }
    if let Some(home) = home {
        return Ok(PathBuf::from(home).join(".local/share/fell/mail.db"));
    }
    Err("no private directory for the mailbox; set HOME, XDG_DATA_HOME, or FELL_STORE".into())
}

pub fn config_dir() -> Option<PathBuf> {
    if let Some(config) = env::var_os("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(config).join("fell"));
    }
    env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/fell"))
}

fn load_keymap() -> (Keymap, Option<String>) {
    let Some(path) = config_dir().map(|dir| dir.join("keymap.toml")) else {
        return (Keymap::default(), None);
    };
    match fs::read_to_string(&path) {
        Ok(text) => match keymap::keymap_from_text(&text) {
            Ok(keymap) => (keymap, None),
            Err(err) => (Keymap::default(), Some(format!("keymap: {err}"))),
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => (Keymap::default(), None),
        Err(err) => (Keymap::default(), Some(format!("keymap: {err}"))),
    }
}

fn ui_settings() -> Option<String> {
    let path = config_dir()?.join("ui.toml");
    fs::read_to_string(path).ok()
}

/// The value of `key = value` in ui.toml text.
fn ui_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().find_map(|line| {
        let (name, value) = line.trim().split_once('=')?;
        (name.trim() == key).then(|| value.trim())
    })
}

fn load_split() -> f32 {
    ui_settings()
        .as_deref()
        .and_then(|text| ui_value(text, "split")?.parse::<f32>().ok())
        .map_or(0.38, |split| split.clamp(0.22, 0.52))
}

/// At most ten seconds, so a typo cannot leave mail unread for good.
fn load_read_dwell() -> u64 {
    ui_settings()
        .as_deref()
        .and_then(|text| ui_value(text, "read_dwell_ms")?.parse::<u64>().ok())
        .map_or(0, |ms| ms.min(10_000))
}

pub fn save_split(split: f32) {
    let Some(dir) = config_dir() else {
        return;
    };
    if private_config_dir(&dir).is_err() {
        return;
    }
    let path = dir.join("ui.toml");
    // Keep the other settings in the file.
    let kept: String = fs::read_to_string(&path)
        .unwrap_or_default()
        .lines()
        .filter(|line| {
            line.trim()
                .split_once('=')
                .is_none_or(|(name, _)| name.trim() != "split")
        })
        .map(|line| format!("{line}\n"))
        .collect();
    if fs::write(&path, format!("split = {split:.3}\n{kept}")).is_ok() {
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_setup_records_the_original_id_with_a_broken_legacy_file() {
        let Some(_root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        use std::io::Write;

        let registry = Registry::resolve().unwrap();
        fs::create_dir_all(registry.directory()).unwrap();
        let legacy = registry.directory().parent().unwrap().join("account.toml");
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&legacy)
            .unwrap()
            .write_all(b"invalid = [")
            .unwrap();
        let mut config = AccountConfig::bridge(
            "first@example.test",
            "first@example.test",
            fell_core::PasswordSource::Inline("fixture".into()),
        );
        config.id = "first".into();
        let marker = registry.directory().join(".original-id");
        let staged = stage_account_config(&mut config, AccountSave::New).unwrap();
        assert!(!marker.exists());
        drop(staged);
        assert!(!marker.exists());
        assert!(!registry.path("first").unwrap().exists());
        stage_account_config(&mut config, AccountSave::New)
            .unwrap()
            .publish()
            .unwrap();
        assert_eq!(fs::read_to_string(marker).unwrap(), "first");
        let base = store_path().unwrap();
        assert_eq!(registry.store_path(&base, "first").unwrap(), base);
        assert_eq!(fs::read_to_string(legacy).unwrap(), "invalid = [");
    }

    #[test]
    fn kept_mailboxes_require_explicit_reconnect_and_matching_email() {
        let Some(_root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        let base = store_path().unwrap();
        let mut config = AccountConfig::bridge(
            "kept@example.test",
            "kept@example.test",
            fell_core::PasswordSource::Inline("fixture".into()),
        );
        open_at(&base)
            .unwrap()
            .upsert_account(&config.account())
            .unwrap();
        let other = registry.mailbox_path(&base, "mail-2").unwrap();
        open_at(&other).unwrap();
        assert_eq!(next_account_id().unwrap(), "mail-3");
        assert_eq!(retained_mailbox(&config).as_deref(), Some("mail"));
        assert!(stage_account_config(&mut config, AccountSave::Update).is_err());
        config.email = "different@example.test".into();
        assert!(stage_account_config(&mut config, AccountSave::Reconnect).is_err());
        assert!(!registry.path("mail").unwrap().exists());
        config.email = "kept@example.test".into();
        stage_account_config(&mut config, AccountSave::Reconnect)
            .unwrap()
            .publish()
            .unwrap();
        assert!(registry.path("mail").unwrap().is_file());
        assert_eq!(registry.store_path(&base, "mail").unwrap(), base);
    }

    #[test]
    fn replacing_a_configured_address_keeps_the_old_mailbox_under_its_id() {
        let Some(_root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        let base = store_path().unwrap();
        let mut original = AccountConfig::bridge(
            "old@example.test",
            "test-user",
            fell_core::PasswordSource::Inline("fixture".into()),
        );
        original.id = "work".into();
        let old_path = stage_account_config(&mut original, AccountSave::New)
            .unwrap()
            .publish()
            .unwrap();
        let old_mailbox = registry.store_path(&base, "work").unwrap();
        let old_store = open_at(&old_mailbox).unwrap();
        let old_id = AccountId("work".into());
        old_store.upsert_account(&original.account()).unwrap();
        old_store
            .set_agent_grant(
                &old_id,
                "atlas",
                fell_core::AgentGrant::enabled(true, false, false),
            )
            .unwrap();
        let mut update = original.clone();
        update.port = 1234;
        update.email = "OLD@example.test".into();
        stage_account_config(&mut update, AccountSave::Update)
            .unwrap()
            .publish()
            .unwrap();
        assert_eq!(registry.get(&old_id).unwrap().unwrap().port, 1234);
        assert!(old_store.agent_grant(&old_id, "atlas").unwrap().enabled);
        let mut replacement = original.clone();
        replacement.email = "new@example.test".into();
        assert!(matches!(
            stage_account_config(&mut replacement, AccountSave::Update),
            Err(AccountFileError::MailboxAddressConflict { id, email })
                if id == "work" && email == "old@example.test"
        ));
        assert!(
            old_store
                .configure_account(&replacement.account(), false)
                .is_err()
        );
        assert!(old_path.is_file());
        let mut cancelled = replacement.clone();
        drop(stage_account_config(&mut cancelled, AccountSave::ReplaceMailbox).unwrap());
        assert!(old_path.is_file());
        let new_path = stage_account_config(&mut replacement, AccountSave::ReplaceMailbox)
            .unwrap()
            .publish()
            .unwrap();
        assert_ne!(replacement.id, "work");
        assert!(!old_path.exists());
        assert!(new_path.is_file());
        let new_mailbox = registry.store_path(&base, &replacement.id).unwrap();
        assert_ne!(old_mailbox, new_mailbox);
        assert!(!new_mailbox.exists());
        assert_eq!(
            old_store.get_account(&old_id).unwrap().unwrap(),
            original.account()
        );
        assert!(!old_store.agent_grant(&old_id, "atlas").unwrap().enabled);
        assert!(
            open_at(&new_mailbox)
                .unwrap()
                .list_accounts()
                .unwrap()
                .is_empty()
        );
        assert!(can_reconnect(&original));
        let mut wrong_login = original.clone();
        wrong_login.username = "another-user".into();
        assert!(!can_reconnect(&wrong_login));
        assert!(stage_account_config(&mut wrong_login, AccountSave::Reconnect).is_err());
    }

    #[test]
    fn changing_only_the_imap_login_keeps_the_original_mailbox_reconnectable() {
        let Some(_root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        let base = store_path().unwrap();
        let mut old = AccountConfig::bridge(
            "mail@example.test",
            "before",
            fell_core::PasswordSource::Inline("fixture".into()),
        );
        old.write_back = true;
        stage_account_config(&mut old, AccountSave::New)
            .unwrap()
            .publish()
            .unwrap();
        let previous_path = registry.configured_store_path(&base, &old).unwrap();
        let store = open_at(&previous_path).unwrap();
        store.configure_connection(&old).unwrap();
        rusqlite::Connection::open(&previous_path).unwrap().execute(
            "INSERT INTO server_outbox (account_id, kind, state, message_id, source_folder, source_mailbox,
                uidvalidity, uid, target_folder, created_unix)
             VALUES (?1, 'move', 'pending', 'kept@example.test', 'INBOX', 'INBOX', 1, 2, 'Archive', 0)",
             [&old.id],
        ).unwrap();

        let mut edited = old.clone();
        edited.username = "after".into();
        let (_, identity) = registry
            .get_with_identity(&old.account().id)
            .unwrap()
            .unwrap();
        assert!(stage_connection_config(&mut edited, AccountSave::Update, Some(identity)).is_err());
        let staged =
            stage_connection_config(&mut edited, AccountSave::ReplaceMailbox, Some(identity))
                .unwrap();
        assert_ne!(&edited.id, &old.id);
        assert_eq!(registry.get(&old.account().id).unwrap(), Some(old.clone()));
        staged.publish().unwrap();
        assert_eq!(
            accounts::mailbox_owner(&previous_path).unwrap(),
            Some((old.id.clone(), old.email.clone()))
        );
        assert_ne!(
            registry.configured_store_path(&base, &edited).unwrap(),
            previous_path
        );
        assert!(
            store
                .configure_connection(&AccountConfig {
                    id: old.id.clone(),
                    ..edited.clone()
                })
                .is_err()
        );
        assert!(can_reconnect(&old));
        edited.id.clone_from(&old.id);
        assert!(!can_reconnect(&edited));
        let kept = registry.load_with_removals(&base).disconnected;
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].email, old.email);
        let mut reconnect = registry.reconnect_config(&base, &old.id).unwrap();
        reconnect.password = fell_core::PasswordSource::Inline("reconnected".into());
        reconnect.write_back = false;
        stage_account_config(&mut reconnect, AccountSave::Reconnect)
            .unwrap()
            .publish()
            .unwrap();
        let saved = registry.get(&old.account().id).unwrap().unwrap();
        assert!(saved.write_back);
        store.configure_connection(&saved).unwrap();
        assert_eq!(store.queued_write_count(&old.account().id).unwrap(), 1);
    }

    #[test]
    fn config_permissions_are_repaired_without_chmodding_parents_or_symlinks() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("config");
        let dir = parent.join("fell");
        fs::create_dir_all(&dir).unwrap();
        fs::set_permissions(&parent, fs::Permissions::from_mode(0o755)).unwrap();

        for mode in [0o755, 0o777] {
            fs::set_permissions(&dir, fs::Permissions::from_mode(mode)).unwrap();
            private_config_dir(&dir).unwrap();
            assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        }
        assert_eq!(fs::metadata(&parent).unwrap().mode() & 0o777, 0o755);

        let link = temp.path().join("fell");
        symlink(&parent, &link).unwrap();
        assert!(private_config_dir(&link).is_err());
        assert_eq!(fs::metadata(&parent).unwrap().mode() & 0o777, 0o755);
    }

    #[test]
    fn ui_settings_read_by_name() {
        let text = "split = 0.400\nread_dwell_ms = 750\n# split = 9\n";
        assert_eq!(ui_value(text, "split"), Some("0.400"));
        assert_eq!(ui_value(text, "read_dwell_ms"), Some("750"));
        assert_eq!(ui_value(text, "split_ms"), None);
    }

    #[test]
    fn missing_home_does_not_put_mail_in_tmp() {
        let err = store_path_from(None, None, None).unwrap_err();
        assert!(!err.contains("/tmp"));
        let path = store_path_from(None, None, Some(OsStr::new("/home/caleb"))).unwrap();
        assert_eq!(path, PathBuf::from("/home/caleb/.local/share/fell/mail.db"));
    }

    #[test]
    fn created_mailbox_is_not_world_readable() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("mailbox");
        let path = dir.join("mail.db");
        open_at(&path).unwrap();
        let file_mode = fs::metadata(&path).unwrap().permissions().mode();
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o077, 0, "mailbox mode {file_mode:o}");
        assert_eq!(dir_mode & 0o077, 0, "directory mode {dir_mode:o}");
    }

    #[test]
    fn account_file_must_be_private_and_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.toml");
        let account = AccountId("mail".into());
        assert_eq!(read_account_config(&path).unwrap(), None);
        let missing = send_config_at(&path, &account).unwrap_err();
        assert_eq!(missing.recovery, "Run fell account setup.");

        fs::write(&path, "email = \"a@b\"\npassword = \"x\"\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let err = send_config_at(&path, &account).unwrap_err();
        assert!(err.detail.contains("mode 600"), "{err}");
        assert_eq!(err.recovery, "Set account file permissions to 600.");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let config = send_config_at(&path, &account).unwrap();
        assert_eq!(config.email, "a@b");
        let mismatch = send_config_at(&path, &AccountId("other".into())).unwrap_err();
        assert_eq!(mismatch.recovery, "Fix the settings in the account file.");

        fs::write(&path, "email = \"a@b\"\n").unwrap();
        let err = send_config_at(&path, &account).unwrap_err();
        assert!(err.detail.contains("password"), "{err}");
        assert_eq!(err.recovery, "Fix the settings in the account file.");
    }

    #[test]
    fn import_appends_only_confirmed_new_aliases_and_keeps_the_account_text() {
        let Some(root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let path = root.join("fell/accounts/mail.toml");
        let original = "# Keep my notes\nemail = 'primary@example.com'\npassword = 'demo'";
        let lock = lock_account_dir(&path).unwrap();
        stage_account_text_at(path.clone(), original, true, lock)
            .unwrap()
            .publish()
            .unwrap();
        let account = AccountId("mail".into());
        let updated = import_identities_at(
            path.clone(),
            &account,
            vec!["primary@example.com".into(), "confirmed@example.com".into()],
        )
        .unwrap();
        assert_eq!(updated.identities.len(), 1);
        assert_eq!(updated.identities[0].address, "confirmed@example.com");
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.starts_with(original));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            import_identities_at(
                path.clone(),
                &AccountId("other".into()),
                vec!["unconfirmed@example.com".into(),]
            )
            .is_err()
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
    }

    #[test]
    fn written_account_file_is_private_and_reads_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fell/account.toml");
        let config = AccountConfig::bridge(
            "me@proton.me",
            "me@proton.me",
            fell_core::PasswordSource::Inline("p\"w #1".into()),
        );
        stage_account_config_at(path.clone(), &config, true)
            .unwrap()
            .publish()
            .unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(read_account_config(&path).unwrap(), Some(config.clone()));
        let keyring = AccountConfig {
            password: fell_core::PasswordSource::Keyring,
            ..config.clone()
        };
        let staged = stage_account_config_at(path.clone(), &keyring, false).unwrap();
        assert!(stage_account_config_at(path.clone(), &config, false).is_err());
        let staged_path = staged.file.path().to_owned();
        assert_eq!(
            read_account_config(&staged_path).unwrap(),
            Some(keyring.clone())
        );
        // A failed keyring write drops the staged file and keeps the installed account.
        drop(staged);
        assert!(!staged_path.exists());
        assert!(stage_account_config_at(path.clone(), &config, true).is_err());
        assert_eq!(read_account_config(&path).unwrap(), Some(config));
        stage_account_config_at(path.clone(), &keyring, false)
            .unwrap()
            .publish()
            .unwrap();
        assert_eq!(read_account_config(&path).unwrap(), Some(keyring));
    }

    #[test]
    fn configured_account_wins_over_fixture_mail() {
        let store = Store::open_in_memory().unwrap();
        assert!(prepare(&store, None, false).is_err());
        assert!(store.list_accounts().unwrap().is_empty());
        let opened = prepare(&store, None, true).unwrap();
        assert_eq!(opened.sync, SyncKind::FixtureOffline);
        let fixture = opened.account;
        assert!(prepare(&store, None, false).is_err());
        let config = AccountConfig::bridge(
            "me@example.test",
            "me@example.test",
            fell_core::PasswordSource::Inline("x".into()),
        );
        for id in ["local", "fixture", "demo"].map(|id| AccountId(id.into())) {
            let store = Store::open_in_memory().unwrap();
            let demo = Account {
                id,
                ..fixture.clone()
            };
            seed_if_empty(&store, &demo).unwrap();
            assert!(!store.list_threads(&demo.id).unwrap().is_empty());
            store
                .set_agent_grant(
                    &demo.id,
                    "bot",
                    fell_core::AgentGrant::enabled(true, true, true),
                )
                .unwrap();
            let opened = prepare(&store, Some(&config), true).unwrap();
            assert_eq!(opened.sync, SyncKind::Connecting);
            let account = opened.account;
            assert_eq!(account.email, config.email);
            assert_eq!(store.list_accounts().unwrap(), [account]);
            assert!(store.list_agent_grants().unwrap().is_empty());
            assert!(store.list_threads(&demo.id).unwrap().is_empty());
            assert!(
                store
                    .agent_grant(&demo.id, "bot")
                    .unwrap_err()
                    .to_string()
                    .contains("unknown account")
            );
        }
    }

    #[test]
    fn a_reserved_real_account_stays_the_only_owner_on_each_launch() {
        let config = AccountConfig::bridge(
            "configured@example.test",
            "configured@example.test",
            fell_core::PasswordSource::Inline("test".into()),
        );
        let fixture = fixture_account();
        for legacy in [
            Account {
                email: "legacy@example.test".into(),
                ..fixture.clone()
            },
            Account {
                backend_kind: fell_core::BackendKind::Imap,
                ..fixture
            },
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("mail.db");
            Store::open(&path).unwrap().upsert_account(&legacy).unwrap();
            let (store, opened) = open_store(Ok(path.clone()), Some(&config), false);
            assert_eq!(opened.sync, SyncKind::StoreError);
            let error = opened.error.unwrap();
            assert!(error.contains(legacy.id.as_str()), "{error}");
            assert!(error.contains(&legacy.email), "{error}");
            assert!(error.contains(&*path.to_string_lossy()), "{error}");
            assert!(store.list_accounts().unwrap().is_empty());
            assert_eq!(
                Store::open(&path).unwrap().list_accounts().unwrap(),
                [legacy]
            );
        }
    }

    #[test]
    fn existing_parent_keeps_its_mode() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        let before = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;

        let path = dir.join("mail.db");
        open_at(&path).unwrap();
        let after = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(after, before, "existing parent mode changed");
        assert_eq!(file_mode, 0o600, "mailbox mode {file_mode:o}");

        let nested = dir.join("created");
        open_at(&nested.join("mail.db")).unwrap();
        let nested_mode = fs::metadata(&nested).unwrap().permissions().mode() & 0o777;
        let parent_again = fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            nested_mode, 0o700,
            "new mailbox directory mode {nested_mode:o}"
        );
        assert_eq!(parent_again, before);
    }
}
