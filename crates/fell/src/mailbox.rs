//! Open the local store and, when it is empty, stub-sync fixture mail into it.

use std::env;
use std::fs;
use std::path::PathBuf;

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
    let path = store_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|err| fell_core::Error::Backend(err.to_string()))?;
    }
    Store::open(&path)
}

pub fn store_path() -> PathBuf {
    if let Some(path) = env::var_os("FELL_STORE") {
        return PathBuf::from(path);
    }
    data_dir().join("mail.db")
}

pub fn config_dir() -> PathBuf {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("fell")
}

fn data_dir() -> PathBuf {
    env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/share"))
        .join("fell")
}

fn home() -> PathBuf {
    env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn load_keymap() -> (Keymap, Option<String>) {
    let path = config_dir().join("keymap.toml");
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
    let path = config_dir().join("ui.toml");
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
    let dir = config_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    let _ = fs::write(dir.join("ui.toml"), format!("split = {split:.3}\n"));
}
