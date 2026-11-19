//! Open the local store and read the account file. With no account file, an
//! empty store is filled with fixture mail so the window has something to show.

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
    let (keymap, keymap_error) = load_keymap();
    let split = load_split();
    let read_dwell_ms = load_read_dwell();
    let (config, config_error) = match load_account_config() {
        Ok(config) => (config, None),
        Err(err) => (None, Some(err)),
    };
    Startup {
        keymap,
        split,
        read_dwell_ms,
        error: join_errors([keymap_error, config_error]),
        config,
        path: store_path(),
    }
}

/// Open the store and pick the account. Runs on the store thread: the first
/// open of a large mailbox can replay its log and take seconds.
pub fn open_store(
    path: Result<PathBuf, String>,
    config: Option<&AccountConfig>,
) -> (Store, Opened) {
    let opened = path
        .and_then(|path| open_at(&path).map_err(|err| err.to_string()))
        .and_then(|store| match prepare(&store, config) {
            Ok((account, sync)) => Ok((store, account, sync)),
            Err(err) => Err(err.to_string()),
        });
    match opened {
        Ok((store, account, sync)) => (
            store,
            Opened {
                account,
                sync,
                error: None,
            },
        ),
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

/// Write the account file mode 0600, creating `~/.config/fell` mode 0700 if needed.
pub fn write_account_config(config: &AccountConfig) -> Result<PathBuf, String> {
    let path = account_config_path()
        .ok_or_else(|| "no config directory; set HOME or XDG_CONFIG_HOME".to_string())?;
    write_account_config_at(&path, config)?;
    Ok(path)
}

fn write_account_config_at(path: &Path, config: &AccountConfig) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    if let Some(dir) = path.parent() {
        create_mailbox_dirs(dir).map_err(|err| err.to_string())?;
    }
    let tmp = path.with_extension("toml.tmp");
    let _ = fs::remove_file(&tmp);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|err| format!("{}: {err}", tmp.display()))?;
    file.write_all(config.render().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|err| format!("{}: {err}", tmp.display()))?;
    fs::rename(&tmp, path).map_err(|err| format!("{}: {err}", path.display()))?;
    Ok(())
}

fn fallback(error: String) -> (Store, Opened) {
    let store = Store::open_in_memory().unwrap_or_else(|_| {
        panic!("in-memory store");
    });
    (
        store,
        Opened {
            account: fixture_account(),
            sync: SyncKind::StoreError,
            error: Some(error),
        },
    )
}

fn prepare(
    store: &Store,
    config: Option<&AccountConfig>,
) -> fell_core::Result<(Account, SyncKind)> {
    if let Some(config) = config {
        let account = config.account();
        store.upsert_account(&account)?;
        // Before the window can archive anything, so the first action is
        // queued for the server or not as the file says.
        store.set_write_back(&account.id, config.write_back)?;
        return Ok((account, SyncKind::Connecting));
    }
    let accounts = store.list_accounts()?;
    if accounts.is_empty() {
        let account = fixture_account();
        seed_if_empty(store, &account)?;
        return Ok((account, SyncKind::FixtureOffline));
    }
    let account = accounts.into_iter().next().unwrap();
    if store.list_threads(&account.id)?.is_empty() {
        Ok((account, SyncKind::FirstSync))
    } else {
        Ok((account, SyncKind::LocalOffline))
    }
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
        let dir = std::env::temp_dir().join(format!("fell-write-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("fell/account.toml");
        let config = AccountConfig::bridge(
            "me@proton.me",
            "me@proton.me",
            fell_core::PasswordSource::Inline("p\"w #1".into()),
        );
        write_account_config_at(&path, &config).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(read_account_config(&path).unwrap(), Some(config.clone()));
        // A second write replaces the first through a fresh 0600 temp file.
        write_account_config_at(&path, &config).unwrap();
        assert!(!path.with_extension("toml.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn configured_account_wins_over_fixture_mail() {
        let store = Store::open_in_memory().unwrap();
        let (fixture, kind) = prepare(&store, None).unwrap();
        assert_eq!(kind, SyncKind::FixtureOffline);
        let config = AccountConfig::bridge(
            "me@proton.me",
            "me@proton.me",
            fell_core::PasswordSource::Inline("x".into()),
        );
        let (account, kind) = prepare(&store, Some(&config)).unwrap();
        assert_eq!(kind, SyncKind::Connecting);
        assert_eq!(account.email, "me@proton.me");
        assert_ne!(account.id, fixture.id);
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
