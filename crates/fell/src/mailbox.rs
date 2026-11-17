//! Open the local store and, when it is empty, stub-sync fixture mail into it.

use std::env;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use fell_core::{Account, Store};

use crate::fixtures::{fixture_account, seed_if_empty};
use crate::keymap::{self, Keymap};
use crate::model::SyncKind;

pub struct Mailbox {
    pub store: Store,
    pub account: Account,
    pub sync: SyncKind,
    pub error: Option<String>,
    pub keymap: Keymap,
    pub split: f32,
}

pub fn open() -> Mailbox {
    let (keymap, keymap_error) = load_keymap();
    let split = load_split();
    match open_store() {
        Ok(store) => match prepare(&store) {
            Ok((account, sync)) => Mailbox {
                store,
                account,
                sync,
                error: keymap_error,
                keymap,
                split,
            },
            Err(err) => fallback(keymap, split, err.to_string()),
        },
        Err(err) => fallback(keymap, split, err.to_string()),
    }
}

fn fallback(keymap: Keymap, split: f32, error: String) -> Mailbox {
    let store = Store::open_in_memory().unwrap_or_else(|_| {
        panic!("in-memory store");
    });
    let account = fixture_account();
    Mailbox {
        store,
        account,
        sync: SyncKind::StoreError,
        error: Some(error),
        keymap,
        split,
    }
}

fn prepare(store: &Store) -> fell_core::Result<(Account, SyncKind)> {
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

fn open_store() -> fell_core::Result<Store> {
    let path = store_path().map_err(fell_core::Error::Backend)?;
    open_at(&path)
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

fn load_split() -> f32 {
    let Some(path) = config_dir().map(|dir| dir.join("ui.toml")) else {
        return 0.38;
    };
    let Ok(text) = fs::read_to_string(path) else {
        return 0.38;
    };
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix("split") else {
            continue;
        };
        let value = value.trim().trim_start_matches('=').trim();
        if let Ok(split) = value.parse::<f32>() {
            return split.clamp(0.22, 0.52);
        }
    }
    0.38
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
    if fs::write(&path, format!("split = {split:.3}\n")).is_ok() {
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
