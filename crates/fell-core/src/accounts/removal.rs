//! Disconnect a registry account, with a recoverable local deletion.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::{
    Accounts, Entry, FileIdentity, Registry, file_identity, mailbox_exists, mailbox_owner,
    path_exists, publish, read, real_directory, removal_trash, verify_backup,
};
use crate::{AccountId, Store, compose};

struct Removal {
    account: String,
    drafts: Vec<String>,
    name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedRemoval {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub can_restore: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemovalOutcome {
    Kept,
    Deleted,
    Restored { connected: bool },
    Cleared,
}

impl Registry {
    pub fn load_with_removals(&self, base: &Path) -> Accounts {
        let mut catalog = self.load();
        let original = self
            .original_id(base)
            .ok()
            .flatten()
            .and_then(|(id, backup)| {
                if let Some(backup) = backup
                    && let Err(error) = verify_backup(base, &backup, &id)
                {
                    let config = self.directory().join(format!("{id}.toml"));
                    if let Some(entry) = catalog
                        .entries
                        .iter_mut()
                        .find(|entry| entry.path == config)
                    {
                        entry.config = Err(error);
                        entry.file_identity = None;
                    } else {
                        catalog.entries.push(Entry {
                            path: backup,
                            config: Err(error),
                            file_identity: None,
                        });
                    }
                    return None;
                }
                Some((id, removal_trash(base)))
            });
        let siblings = base.with_extension("accounts");
        let paths = real_directory(&siblings)
            .and_then(|()| fs::read_dir(&siblings))
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let id = entry
                    .file_name()
                    .to_str()?
                    .strip_suffix(".db.removing")?
                    .to_owned();
                Some((id, entry.path()))
            });
        for (id, trash) in original.into_iter().chain(paths) {
            let Ok(mailbox) = self.mailbox_path(base, &id) else {
                continue;
            };
            if !trash.symlink_metadata().is_ok_and(|meta| meta.is_dir())
                || removal_trash(&mailbox) != trash
            {
                continue;
            }
            let Ok(path) = self.path(&id) else {
                continue;
            };
            let plan = read_plan(&trash).ok();
            let config = catalog.configs().find(|config| config.id == id);
            let name = plan
                .as_ref()
                .and_then(|plan| plan.name.clone())
                .or_else(|| config.map(|config| config.account().display_name))
                .unwrap_or_else(|| id.clone());
            let can_restore = plan.is_some_and(|plan| plan.account == id)
                && trash
                    .join("committed")
                    .symlink_metadata()
                    .is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
                && (path.is_file()
                    || trash.join("account.toml").is_file()
                    || mailbox.is_file()
                    || staged_file(&trash, &mailbox, "").is_file());
            catalog.interrupted.push(InterruptedRemoval {
                id,
                name,
                path,
                can_restore,
            });
        }
        catalog.interrupted.sort_by(|a, b| a.id.cmp(&b.id));
        catalog
    }

    pub fn remove_broken_file(&self, path: &Path, expected: FileIdentity) -> Result<(), String> {
        let legacy = self.config.join("account.toml");
        let id = if path == legacy {
            None
        } else if path.parent() == Some(self.directory().as_path())
            && path.extension().is_some_and(|ext| ext == "toml")
        {
            path.file_stem().and_then(|stem| stem.to_str())
        } else {
            return Err("Expected an account file.".into());
        };
        let directory = self.lock().map_err(|err| err.to_string())?;
        if path.symlink_metadata().ok().map(file_identity) != Some(expected)
            || (path == legacy || id.is_some_and(|id| super::validate_id(id).is_ok()))
                && read(path, id).is_ok_and(|config| config.is_some())
        {
            return Err("The account file changed. Select the account again.".into());
        }
        let directory = if path == legacy {
            File::open(&self.config).map_err(|err| err.to_string())?
        } else {
            directory
        };
        disconnect(path, &directory).map_err(|err| err.to_string())
    }

    /// Revoke grants before disconnecting. A confirmed deletion first moves
    /// all SQLite files into a durable staging directory. If interrupted, the
    /// mailbox stays closed until removal resumes using that directory.
    pub fn remove(&self, base: &Path, id: &str, delete: bool) -> Result<RemovalOutcome, String> {
        self.remove_if_unchanged(base, id, delete, None)
    }

    pub fn remove_if_unchanged(
        &self,
        base: &Path,
        id: &str,
        delete: bool,
        expected: Option<FileIdentity>,
    ) -> Result<RemovalOutcome, String> {
        self.remove_impl(base, id, delete, expected, false)
    }

    pub fn recover_removal(
        &self,
        base: &Path,
        id: &str,
        delete: bool,
        expected: Option<FileIdentity>,
    ) -> Result<RemovalOutcome, String> {
        self.remove_impl(base, id, delete, expected, true)
    }

    fn remove_impl(
        &self,
        base: &Path,
        id: &str,
        delete: bool,
        expected: Option<FileIdentity>,
        recovery_only: bool,
    ) -> Result<RemovalOutcome, String> {
        self.migrate().map_err(|err| err.to_string())?;
        let directory = self.lock().map_err(|err| err.to_string())?;
        let account = self.path(id).map_err(|err| err.to_string())?;
        let mailbox = self.mailbox_path(base, id).map_err(|err| err.to_string())?;
        if expected.is_some() && account.symlink_metadata().ok().map(file_identity) != expected {
            return Err("The account file changed. Select the account again.".into());
        }
        let trash = removal_trash(&mailbox);
        if trash.symlink_metadata().is_ok() {
            if !trash.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                return Err("The mailbox removal directory is unavailable.".into());
            }
            let plan = match read_plan(&trash) {
                Ok(plan) => plan,
                Err(err)
                    if err.kind() == io::ErrorKind::NotFound
                        && fs::read_dir(&trash).is_ok_and(|mut entries| {
                            entries.all(|entry| {
                                entry.is_ok_and(|entry| entry.file_name() == "committed")
                            })
                        }) =>
                {
                    if !delete && trash.join("committed").exists() {
                        return Err("Permanently deleting local mail has already started.".into());
                    }
                    clear_trash(&trash).map_err(|err| err.to_string())?;
                    return Ok(RemovalOutcome::Cleared);
                }
                Err(err) => return Err(err.to_string()),
            };
            if plan.account != id {
                return Err("The mailbox removal directory belongs to another account.".into());
            }
            if trash
                .join("committed")
                .symlink_metadata()
                .is_err_and(|err| err.kind() == io::ErrorKind::NotFound)
                && let Some((_, snapshot)) =
                    read(&trash.join("account.toml"), None).map_err(|err| err.to_string())?
                && let Some((_, current)) =
                    read(&account, Some(id)).map_err(|err| err.to_string())?
                && snapshot != current
            {
                return Err("The account file differs from the one saved before deletion. Move it aside before recovering local mail.".into());
            }
            let _local = lock_mailbox(&mailbox)?;
            let _staged = lock_mailbox(&staged_file(&trash, &mailbox, ""))?;
            return if delete {
                finish(&account, &directory, &mailbox, &trash, &plan)?;
                Ok(RemovalOutcome::Deleted)
            } else {
                restore(&account, &directory, &mailbox, &trash)
            };
        }
        if recovery_only {
            return Err("There is no interrupted deletion for this account.".into());
        }

        if !account
            .symlink_metadata()
            .is_ok_and(|meta| meta.is_file() || meta.is_symlink())
        {
            return Err(format!("No account file for {id:?}"));
        }
        let config = read(&account, Some(id))
            .map_err(|err| err.to_string())?
            .ok_or("The account file changed. Select the account again.")?;
        let id = AccountId(id.to_owned());
        let mut store = match mailbox.symlink_metadata() {
            Ok(meta) if meta.is_file() => {
                Some(Store::open_existing_without_index(&mailbox).map_err(|err| err.to_string())?)
            }
            Ok(_) => return Err("Expected a regular mailbox file.".into()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.to_string()),
        };
        if !delete {
            if let Some(store) = store {
                store
                    .disable_account_grants(&id)
                    .map_err(|err| err.to_string())?;
            }
            disconnect(&account, &directory).map_err(|err| err.to_string())?;
            return Ok(RemovalOutcome::Kept);
        }

        let locked = if let Some(store) = store.as_mut() {
            let canonical = mailbox.canonicalize().map_err(|err| err.to_string())?;
            if store.db_path() != Some(canonical.as_path()) {
                return Err("The mailbox is open through another path.".into());
            }
            Some(
                store
                    .take_exclusive_mailbox_lock()
                    .map_err(|err| err.to_string())?,
            )
        } else {
            None
        };
        let mut drafts = compose::drafts_for_removal(&id, &mailbox, store.as_ref())?;
        if let Some(store) = store {
            store
                .disable_account_grants(&id)
                .map_err(|err| err.to_string())?;
            // Close SQLite before moving its files, with the inode still locked.
        }
        drafts.sort();
        drafts.dedup();

        let parent = mailbox.parent().ok_or("No mailbox directory")?;
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        let staged = tempfile::Builder::new()
            .prefix(".remove-")
            .tempdir_in(parent)
            .map_err(|err| err.to_string())?;
        let plan = Removal {
            account: id.0,
            drafts,
            name: Some(config.0.account().display_name),
        };
        publish(
            staged.path(),
            &staged.path().join("account.toml"),
            config.1.as_bytes(),
        )
        .map_err(|err| err.to_string())?;
        let mut journal =
            File::create(staged.path().join("plan")).map_err(|err| err.to_string())?;
        journal
            .write_all(
                &serde_json::to_vec(&(&plan.account, &plan.drafts, &plan.name))
                    .map_err(|err| err.to_string())?,
            )
            .and_then(|_| journal.sync_all())
            .map_err(|err| err.to_string())?;
        sync_dir(staged.path()).map_err(|err| err.to_string())?;
        fs::rename(staged.path(), &trash).map_err(|err| err.to_string())?;
        sync_dir(parent).map_err(|err| err.to_string())?;
        let result = finish(&account, &directory, &mailbox, &trash, &plan);
        drop(locked);
        result.map(|()| RemovalOutcome::Deleted)
    }
}

fn read_plan(trash: &Path) -> io::Result<Removal> {
    let bytes = fs::read(trash.join("plan"))?;
    let (account, drafts, name) = serde_json::from_slice(&bytes)
        .or_else(|_| serde_json::from_slice(&bytes).map(|(id, drafts)| (id, drafts, None)))
        .map_err(io::Error::other)?;
    Ok(Removal {
        account,
        drafts,
        name,
    })
}

pub(super) const MAILBOX_SUFFIXES: [&str; 5] = ["-wal", "-shm", "-journal", ".embed.lock", ""];

pub(super) fn mailbox_file(mailbox: &Path, suffix: &str) -> PathBuf {
    let mut path = mailbox.as_os_str().to_os_string();
    path.push(suffix);
    PathBuf::from(path)
}

fn staged_file(trash: &Path, mailbox: &Path, suffix: &str) -> PathBuf {
    trash.join(
        mailbox_file(mailbox, suffix)
            .file_name()
            .expect("mailbox file name"),
    )
}

pub(super) fn mailbox_has_owner(base: &Path, id: &str) -> bool {
    let trash = removal_trash(base);
    mailbox_owner(base).is_ok_and(|owner| owner.is_some_and(|(owner, _)| owner == id))
        || real_directory(&trash).is_ok()
            && read_plan(&trash).is_ok_and(|plan| plan.account == id)
            && verify_mailbox_owner(base, id)
}

pub(super) fn verify_mailbox_owner(base: &Path, id: &str) -> bool {
    let verify = || -> io::Result<bool> {
        let trash = removal_trash(base);
        let staged = if path_exists(&trash)? {
            real_directory(&trash)?;
            let plan = read_plan(&trash)?;
            if plan.account != id {
                return Ok(false);
            }
            Some(staged_file(&trash, base, ""))
        } else {
            None
        };
        if staged.is_none() && !path_exists(base)? && mailbox_exists(base)? {
            return Ok(false);
        }
        for path in std::iter::once(base).chain(staged.as_deref()) {
            if path_exists(path)?
                && !mailbox_owner(path)
                    .is_ok_and(|owner| owner.is_some_and(|(owner, _)| owner == id))
            {
                return Ok(false);
            }
        }
        Ok(true)
    };
    verify().unwrap_or(false)
}

fn move_mailbox(mailbox: &Path, trash: &Path, restoring: bool) -> io::Result<()> {
    for suffix in MAILBOX_SUFFIXES {
        let live = mailbox_file(mailbox, suffix);
        let staged = staged_file(trash, mailbox, suffix);
        let (from, to) = if restoring {
            (staged, live)
        } else {
            (live, staged)
        };
        match from.symlink_metadata() {
            Ok(_) => match to.symlink_metadata() {
                Ok(_) => return Err(io::Error::other("There are two copies of a mailbox file.")),
                Err(err) if err.kind() == io::ErrorKind::NotFound => fs::rename(from, to)?,
                Err(err) => return Err(err),
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => {}
            Err(err) => return Err(err),
        }
    }
    sync_dir(trash)?;
    sync_dir(trash.parent().expect("trash is beside the mailbox"))
}

fn restore(
    account: &Path,
    directory: &File,
    mailbox: &Path,
    trash: &Path,
) -> Result<RemovalOutcome, String> {
    match trash.join("committed").symlink_metadata() {
        Ok(_) => return Err("Permanently deleting local mail has already started.".into()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.to_string()),
    }
    if !account.is_file()
        && !trash.join("account.toml").is_file()
        && !mailbox.is_file()
        && !staged_file(trash, mailbox, "").is_file()
    {
        return Err("There is no local mailbox left to restore.".into());
    }
    move_mailbox(mailbox, trash, true).map_err(|err| err.to_string())?;
    let snapshot = trash.join("account.toml");
    if !account.exists()
        && let Some((_, text)) = read(&snapshot, None).map_err(|err| err.to_string())?
    {
        publish(
            account.parent().expect("account directory"),
            account,
            text.as_bytes(),
        )
        .map_err(|err| err.to_string())?;
    }
    directory.sync_all().map_err(|err| err.to_string())?;
    absent_ok(fs::remove_file(snapshot)).map_err(|err| err.to_string())?;
    clear_trash(trash).map_err(|err| err.to_string())?;
    Ok(RemovalOutcome::Restored {
        connected: account.is_file(),
    })
}

fn lock_mailbox(path: &Path) -> Result<Option<File>, String> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => {
            file.try_lock().map_err(|err| format!(
                "Close Fell and any fell-mcp sessions using this account before continuing: {err}"
            ))?;
            let metadata = file.metadata().map_err(|err| err.to_string())?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err("Expected a mailbox file with one link.".into());
            }
            Ok(Some(file))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.to_string()),
    }
}

fn finish(
    account: &Path,
    directory: &File,
    mailbox: &Path,
    trash: &Path,
    plan: &Removal,
) -> Result<(), String> {
    let committed = trash.join("committed");
    match committed.symlink_metadata() {
        Ok(_) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            move_mailbox(mailbox, trash, false).map_err(|err| err.to_string())?;
            disconnect(account, directory).map_err(|err| err.to_string())?;
            // The next step can unlink drafts or SQLite files. Persist the
            // point of no return before it touches either.
            File::create_new(&committed)
                .and_then(|file| file.sync_all())
                .and_then(|()| sync_dir(trash))
                .map_err(|err| err.to_string())?;
        }
        Err(err) => return Err(err.to_string()),
    }
    compose::remove_draft_workspaces(&plan.drafts, &plan.account, mailbox)?;
    for suffix in MAILBOX_SUFFIXES {
        absent_ok(fs::remove_file(staged_file(trash, mailbox, suffix)))
            .map_err(|err| err.to_string())?;
    }
    absent_ok(fs::remove_file(trash.join("account.toml"))).map_err(|err| err.to_string())?;
    clear_trash(trash).map_err(|err| err.to_string())
}

fn clear_trash(trash: &Path) -> io::Result<()> {
    absent_ok(fs::remove_file(trash.join("plan")))?;
    sync_dir(trash)?;
    absent_ok(fs::remove_file(trash.join("committed")))?;
    fs::remove_dir(trash)?;
    sync_dir(trash.parent().expect("trash is beside the mailbox"))
}

fn disconnect(account: &Path, directory: &File) -> io::Result<()> {
    absent_ok(fs::remove_file(account))?;
    directory.sync_all()
}

fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

fn absent_ok(result: io::Result<()>) -> io::Result<()> {
    result.or_else(|err| {
        if err.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            Err(err)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentGrant;

    #[test]
    fn keep_revokes_every_grant_and_delete_cleans_drafts_and_resumes() {
        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        fs::create_dir_all(registry.directory()).unwrap();
        let base = root.join("mail.db");
        let id = AccountId("work".into());
        let file = registry.path(id.as_str()).unwrap();
        let configure = || {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file)
                .unwrap()
                .write_all(b"email = 'work@example.test'\npassword = 'fixture'\n")
                .unwrap();
            registry.get(&id).unwrap().unwrap().account()
        };
        let account = configure();
        let db = registry.store_path(&base, id.as_str()).unwrap();
        fs::create_dir_all(db.parent().unwrap()).unwrap();
        let store = Store::open(&db).unwrap();
        store.upsert_account(&account).unwrap();
        for agent in ["atlas", "scout"] {
            store
                .set_agent_grant(
                    &id,
                    agent,
                    AgentGrant::enabled(true, true, true).with_annotation(true),
                )
                .unwrap();
        }
        assert!(registry.remove(&base, id.as_str(), true).is_err());
        assert!(file.is_file());
        assert!(db.is_file());
        assert!(store.agent_grant(&id, "atlas").unwrap().enabled);
        registry.remove(&base, id.as_str(), false).unwrap();
        assert!(!file.exists());
        assert!(db.is_file());
        assert!(
            store
                .list_agent_grants()
                .unwrap()
                .iter()
                .all(|row| row.grant == AgentGrant::DISABLED)
        );

        configure();
        let fields = compose::DraftFields::default();
        let folder = compose::materialize_workspace("cwork", &fields, "work draft", &id).unwrap();
        compose::write_store_link(&folder, &db).unwrap();
        compose::remember_compose_store("cwork", &db).unwrap();
        let sibling = compose::materialize_workspace(
            "cmail",
            &fields,
            "other draft",
            &AccountId("mail".into()),
        )
        .unwrap();
        compose::remember_compose_store("cmail", &base).unwrap();
        drop(store);
        // An interrupted move already staged the journal. The old config
        // is still on disk, but the registry and DB open both refuse it.
        let trash = removal_trash(&db);
        fs::create_dir(&trash).unwrap();
        fs::write(trash.join("plan"), br#"["work",["cwork"]]"#).unwrap();
        let sidecar = mailbox_file(&db, ".embed.lock");
        fs::write(&sidecar, "staged sidecar").unwrap();
        fs::rename(sidecar, trash.join("work.db.embed.lock")).unwrap();
        assert!(file.is_file());
        assert!(registry.store_path(&base, id.as_str()).is_err());
        assert!(Store::open_existing(&db).is_err());
        assert_eq!(
            registry.remove(&base, id.as_str(), false).unwrap(),
            RemovalOutcome::Restored { connected: true },
        );
        assert!(file.is_file());
        assert!(db.is_file());
        assert!(!trash.exists());
        assert!(folder.exists());
        assert!(root.join("data/compose-stores/cwork").is_file());

        // A fresh confirmed delete takes the same durable path.
        registry.remove(&base, id.as_str(), true).unwrap();
        assert!(!file.exists());
        assert!(!db.exists());
        assert!(!trash.exists());
        assert!(!folder.exists());
        assert!(!compose::control_dir(&folder).exists());
        assert!(!root.join("data/compose-stores/cwork").exists());
        assert!(sibling.is_dir());
        assert!(root.join("data/compose-stores/cmail").is_file());
    }

    #[test]
    fn restore_retries_at_each_rename_and_after_disconnect() {
        let config = "email = 'work@example.test'\npassword = 'fixture'\n";
        for moved in 0..=MAILBOX_SUFFIXES.len() + 1 {
            let root = tempfile::tempdir().unwrap();
            let registry = Registry::at(root.path().join("fell"));
            let base = root.path().join("mail.db");
            let account = registry.path("work").unwrap();
            let mailbox = registry.mailbox_path(&base, "work").unwrap();
            let trash = removal_trash(&mailbox);
            fs::create_dir_all(registry.directory()).unwrap();
            fs::create_dir_all(&trash).unwrap();
            publish(&trash, &trash.join("account.toml"), config.as_bytes()).unwrap();
            fs::write(trash.join("plan"), br#"["work",[],"Work"]"#).unwrap();
            if moved <= MAILBOX_SUFFIXES.len() {
                publish(&registry.directory(), &account, config.as_bytes()).unwrap();
            }
            for (index, suffix) in MAILBOX_SUFFIXES.iter().enumerate() {
                let path = if index < moved {
                    staged_file(&trash, &mailbox, suffix)
                } else {
                    mailbox_file(&mailbox, suffix)
                };
                fs::write(path, suffix).unwrap();
            }
            let catalog = registry.load_with_removals(&base);
            assert_eq!(catalog.interrupted.len(), 1);
            assert!(catalog.interrupted[0].can_restore);
            assert!(catalog.default_config().is_none());
            assert_eq!(
                registry.remove(&base, "work", false).unwrap(),
                RemovalOutcome::Restored { connected: true },
            );
            assert_eq!(fs::read_to_string(account).unwrap(), config);
            for suffix in MAILBOX_SUFFIXES {
                assert_eq!(
                    fs::read_to_string(mailbox_file(&mailbox, suffix)).unwrap(),
                    suffix
                );
            }
            assert!(!trash.exists());
        }
    }

    #[test]
    fn committed_deletion_requires_explicit_finish() {
        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        let base = root.join("mail.db");
        fs::create_dir_all(registry.directory()).unwrap();
        let trash = removal_trash(&base);
        fs::create_dir_all(&trash).unwrap();
        fs::write(trash.join("plan"), br#"["mail",[]]"#).unwrap();
        fs::write(trash.join("committed"), "").unwrap();
        fs::write(trash.join("mail.db"), "mail").unwrap();
        assert!(!registry.load_with_removals(&base).interrupted[0].can_restore);
        assert!(registry.remove(&base, "mail", false).is_err());
        assert!(trash.join("mail.db").is_file());
        assert_eq!(
            registry.remove(&base, "mail", true).unwrap(),
            RemovalOutcome::Deleted
        );
        assert!(!trash.exists());
    }
}
