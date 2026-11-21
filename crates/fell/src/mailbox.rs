//! Open the local store and read the account file.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fell_core::{Account, AccountConfig, AccountId, Store};

use crate::fixtures::{fixture_account, seed_if_empty};
use crate::keymap::{self, Keymap};
use crate::model::SyncKind;

/// What the window reads before its first frame: small config files only.
pub struct Startup {
    pub keymap: Keymap,
    pub split: f32,
    /// `read_dwell_ms` in ui.toml: how long a thread stays open before it
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
    pub warning: Option<String>,
}

pub fn startup() -> Startup {
    let (keymap, keymap_error) = load_keymap();
    let split = load_split();
    let read_dwell_ms = load_read_dwell();
    let (config, account_missing, config_error) = match load_account_config() {
        Ok(config) => {
            let missing = config.is_none();
            (config, missing, None)
        }
        Err(err) => (None, false, Some(err)),
    };
    Startup {
        keymap,
        split,
        read_dwell_ms,
        error: join_errors([keymap_error, config_error]),
        account_missing,
        config,
        path: store_path(),
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

pub fn account_config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("account.toml"))
}

/// `Ok(None)` when there is no account file. An unreadable, malformed, or
/// group/world-readable file is an error so the window can say so.
fn load_account_config() -> Result<Option<AccountConfig>, String> {
    let Some(path) = account_config_path() else {
        return Ok(None);
    };
    read_account_config(&path).map_err(|err| err.to_string())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AccountConfigError {
    pub(crate) detail: String,
    pub(crate) recovery: &'static str,
}

impl AccountConfigError {
    fn new(detail: impl Into<String>, recovery: &'static str) -> Self {
        Self {
            detail: detail.into(),
            recovery,
        }
    }
}

impl std::fmt::Display for AccountConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

fn read_account_config(path: &Path) -> Result<Option<AccountConfig>, AccountConfigError> {
    let shown = path.display();
    let unreadable = |err: std::io::Error| {
        AccountConfigError::new(
            format!("{shown}: {err}"),
            "Restore read access to account.toml.",
        )
    };
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(unreadable(err)),
    };
    let mode = meta.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(AccountConfigError::new(
            format!(
                "{shown} is open to other users (mode {:o}); run chmod 600 on it",
                mode & 0o777
            ),
            "Set account.toml permissions to 600.",
        ));
    }
    let text = fs::read_to_string(path).map_err(unreadable)?;
    AccountConfig::parse(&text).map(Some).map_err(|err| {
        AccountConfigError::new(
            format!("{shown}: {err}"),
            "Fix the settings in account.toml.",
        )
    })
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
    let path = account_config_path().ok_or_else(|| {
        AccountConfigError::new("no config directory", "Set HOME or XDG_CONFIG_HOME.")
    })?;
    send_config_at(&path, account).map(Some)
}

fn send_config_at(path: &Path, account: &AccountId) -> Result<AccountConfig, AccountConfigError> {
    let config = read_account_config(path)?.ok_or_else(|| {
        AccountConfigError::new(
            format!("{} does not exist", path.display()),
            "Run fell account setup.",
        )
    })?;
    if config.id != account.as_str() {
        return Err(AccountConfigError::new(
            format!(
                "{} has no entry for account {}",
                path.display(),
                account.as_str()
            ),
            "Restore this draft’s account ID in account.toml.",
        ));
    }
    Ok(config)
}

/// The account file for `fell account show|check`, with the same mode check
/// the window applies. Nothing in a file other users can change is run.
pub fn load_account_file() -> Result<(PathBuf, AccountConfig), String> {
    let path = account_config_path()
        .ok_or_else(|| "no config directory; set HOME or XDG_CONFIG_HOME".to_string())?;
    match read_account_config(&path).map_err(|err| err.to_string())? {
        Some(config) => Ok((path, config)),
        None => Err(format!(
            "{} does not exist; run fell account setup",
            path.display()
        )),
    }
}

#[derive(Debug)]
pub(crate) enum AccountFileError {
    AlreadyConfigured,
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
            Self::Write(detail) => f.write_str(detail),
        }
    }
}

/// Prepare the file so setup can save its keyring password before publishing
/// the new config with mode 0600. Dropping the staged file keeps the current
/// config in place.
pub(crate) fn stage_account_config(
    config: &AccountConfig,
) -> Result<StagedAccountConfig, AccountFileError> {
    stage_config(config, false)
}

pub(crate) fn stage_new_account_config(
    config: &AccountConfig,
) -> Result<StagedAccountConfig, AccountFileError> {
    stage_config(config, true)
}

fn stage_config(
    config: &AccountConfig,
    first_run: bool,
) -> Result<StagedAccountConfig, AccountFileError> {
    let path = account_config_path()
        .ok_or_else(|| "no config directory; set HOME or XDG_CONFIG_HOME".to_string())?;
    stage_account_config_at(path, config, first_run)
}

pub(crate) struct StagedAccountConfig {
    path: PathBuf,
    file: tempfile::NamedTempFile,
    first_run: bool,
    _lock: fs::File,
}

impl StagedAccountConfig {
    pub(crate) fn publish(self) -> Result<PathBuf, AccountFileError> {
        let installed = if self.first_run {
            self.file.persist_noclobber(&self.path)
        } else {
            self.file.persist(&self.path)
        };
        installed.map_err(|err| {
            if self.first_run && err.error.kind() == std::io::ErrorKind::AlreadyExists {
                AccountFileError::AlreadyConfigured
            } else {
                format!("{}: {}", self.path.display(), err.error).into()
            }
        })?;
        Ok(self.path)
    }
}

pub(crate) fn stage_account_config_at(
    path: PathBuf,
    config: &AccountConfig,
    first_run: bool,
) -> Result<StagedAccountConfig, AccountFileError> {
    use std::io::Write;

    let dir = path.parent().expect("account file has a parent directory");
    create_mailbox_dirs(dir).map_err(|err| err.to_string())?;
    // Serialize setup through publication, since a concurrent attempt could
    // otherwise replace the credential while this config is being written.
    let lock = fs::File::open(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    lock.try_lock()
        .map_err(|err| format!("Couldn't lock the account directory: {err}"))?;
    if first_run {
        match path.symlink_metadata() {
            Ok(_) => return Err(AccountFileError::AlreadyConfigured),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(format!("{}: {err}", path.display()).into()),
        }
    }
    let mut file =
        tempfile::NamedTempFile::new_in(dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    file.write_all(config.render().as_bytes())
        .and_then(|()| file.as_file().sync_all())
        .map_err(|err| format!("{}: {err}", file.path().display()))?;
    Ok(StagedAccountConfig {
        path,
        file,
        first_run,
        _lock: lock,
    })
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
            warning: None,
        },
    )
}

fn prepare(store: &Store, config: Option<&AccountConfig>, demo: bool) -> fell_core::Result<Opened> {
    if let Some(config) = config {
        let account = config.account();
        let retained = store.configure_account(&account, config.write_back)?;
        let warning =
            (!retained.is_empty()).then(|| "Kept older local mail, drafts, and outbox".to_owned());
        return Ok(Opened {
            account,
            sync: SyncKind::Connecting,
            error: None,
            warning,
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
            warning: None,
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
        warning: None,
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
fn create_mailbox_dirs(dir: &Path) -> fell_core::Result<()> {
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
        match fs::create_dir(path) {
            Ok(()) => fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .map_err(|err| fell_core::Error::Backend(err.to_string()))?,
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(fell_core::Error::Backend(err.to_string())),
        }
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
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
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
        let dir = std::env::temp_dir().join(format!("fell-perm-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("mail.db");
        open_at(&path).unwrap();
        let file_mode = fs::metadata(&path).unwrap().permissions().mode();
        let dir_mode = fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(file_mode & 0o077, 0, "mailbox mode {file_mode:o}");
        assert_eq!(dir_mode & 0o077, 0, "directory mode {dir_mode:o}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn account_file_must_be_private_and_valid() {
        let dir = std::env::temp_dir().join(format!("fell-acct-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("account.toml");
        let account = AccountId("mail".into());
        assert_eq!(read_account_config(&path).unwrap(), None);
        let missing = send_config_at(&path, &account).unwrap_err();
        assert_eq!(missing.recovery, "Run fell account setup.");

        fs::write(&path, "email = \"a@b\"\npassword = \"x\"\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let err = send_config_at(&path, &account).unwrap_err();
        assert!(err.detail.contains("chmod 600"), "{err}");
        assert_eq!(err.recovery, "Set account.toml permissions to 600.");

        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let config = send_config_at(&path, &account).unwrap();
        assert_eq!(config.email, "a@b");
        let mismatch = send_config_at(&path, &AccountId("other".into())).unwrap_err();
        assert_eq!(
            mismatch.recovery,
            "Restore this draft’s account ID in account.toml."
        );

        fs::write(&path, "email = \"a@b\"\n").unwrap();
        let err = send_config_at(&path, &account).unwrap_err();
        assert!(err.detail.contains("password"), "{err}");
        assert_eq!(err.recovery, "Fix the settings in account.toml.");
        let _ = fs::remove_dir_all(&dir);
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
        for id in ["local", "fixture", "demo"] {
            let mut demo = fixture.clone();
            demo.id = AccountId(id.into());
            seed_if_empty(&store, &demo).unwrap();
            assert!(!store.list_threads(&demo.id).unwrap().is_empty());
            store
                .set_agent_grant(
                    &demo.id,
                    "bot",
                    fell_core::AgentGrant::enabled(true, true, true),
                )
                .unwrap();
        }
        let config = AccountConfig::bridge(
            "me@proton.me",
            "me@proton.me",
            fell_core::PasswordSource::Inline("x".into()),
        );
        let opened = prepare(&store, Some(&config), true).unwrap();
        assert_eq!(opened.sync, SyncKind::Connecting);
        assert!(opened.warning.is_none());
        let account = opened.account;
        assert_eq!(account.email, "me@proton.me");
        assert_eq!(store.list_accounts().unwrap(), [account]);
        assert!(store.list_agent_grants().unwrap().is_empty());
        for id in ["local", "fixture", "demo"].map(|id| AccountId(id.into())) {
            assert!(store.list_threads(&id).unwrap().is_empty());
            assert!(
                store
                    .agent_grant(&id, "bot")
                    .unwrap_err()
                    .to_string()
                    .contains("unknown account")
            );
        }
    }

    #[test]
    fn a_reserved_real_account_keeps_the_disk_mailbox_open_on_each_launch() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mail.db");
        let legacy = Account {
            email: "legacy@example.test".into(),
            ..fixture_account()
        };
        let store = Store::open(&path).unwrap();
        store.upsert_account(&legacy).unwrap();
        drop(store);

        let config = AccountConfig::bridge(
            "configured@example.test",
            "configured@example.test",
            fell_core::PasswordSource::Inline("test".into()),
        );
        for _ in 0..2 {
            let (store, opened) = open_store(Ok(path.clone()), Some(&config), false);
            assert_eq!(store.db_path(), Some(path.as_path()));
            assert_eq!(opened.sync, SyncKind::Connecting);
            assert!(opened.error.is_none());
            assert_eq!(
                opened.warning.as_deref(),
                Some("Kept older local mail, drafts, and outbox")
            );
            assert_eq!(opened.account.id, config.account().id);
            assert!(store.get_account(&opened.account.id).unwrap().is_some());
            assert_eq!(
                store.get_account(&legacy.id).unwrap().unwrap().email,
                legacy.email
            );
        }
    }

    #[test]
    fn existing_parent_keeps_its_mode() {
        let dir = std::env::temp_dir().join(format!("fell-keep-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
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
        let _ = fs::remove_dir_all(&dir);
    }
}
