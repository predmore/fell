//! Disconnect a registry account, with a recoverable local deletion.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::{
    Accounts, Entry, FileError, FileIdentity, INTERRUPTED_REMOVAL_RECOVERY, MailboxOwnership,
    OriginalMailbox, Registry, check_live_mailbox, file_identity, inspect_mailbox, mailbox_owner,
    path_exists, publish, read, read_account_contents, real_directory, removal_trash,
    sibling_mailbox, validate_mailbox,
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
        let mut catalog = self.load_at(Some(base));
        let _lock = self
            .directory()
            .is_dir()
            .then(|| self.lock_shared().ok())
            .flatten();
        let original = match self.original_mailbox(base) {
            Ok(original) => original,
            Err(error) => {
                for entry in &mut catalog.entries {
                    if entry.config.is_ok() {
                        entry.config = Err(error.clone());
                        entry.file_identity = None;
                    }
                }
                catalog.error = Some(error);
                return catalog;
            }
        };
        let original = match original {
            Some(OriginalMailbox::Known { id, backup }) => Some((id, backup)),
            Some(OriginalMailbox::Unavailable { id, path, error }) => {
                let sibling = sibling_mailbox(base, &id);
                if error.recovery != INTERRUPTED_REMOVAL_RECOVERY
                    || !has_removal_plan(&sibling, &id)
                {
                    self.mark_mailbox_unavailable(&mut catalog, &id, path, error);
                }
                None
            }
            None => None,
        };
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
        let base_trash = original
            .as_ref()
            .map(|(id, _)| (id.clone(), removal_trash(base)));
        for (id, trash) in base_trash.into_iter().chain(paths) {
            let Ok(mailbox) = self.removal_mailbox(base, &id) else {
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
        for entry in &mut catalog.entries {
            let Ok(config) = &entry.config else { continue };
            if catalog
                .interrupted
                .iter()
                .any(|pending| pending.id == config.id)
            {
                continue;
            }
            let sibling;
            let (mailbox, backup) = match &original {
                Some((id, backup)) if *id == config.id => (base, backup.as_deref()),
                _ => {
                    sibling = sibling_mailbox(base, &config.id);
                    (sibling.as_path(), None)
                }
            };
            if let Err(error) = check_live_mailbox(mailbox, &config.id, Some(&config.email), backup)
            {
                mark_entry(entry, error);
            }
        }
        catalog
    }

    fn mark_mailbox_unavailable(
        &self,
        catalog: &mut Accounts,
        id: &str,
        path: PathBuf,
        error: FileError,
    ) {
        let config = self.directory().join(format!("{id}.toml"));
        if let Some(entry) = catalog
            .entries
            .iter_mut()
            .find(|entry| entry.path == config)
        {
            mark_entry(entry, error);
        } else {
            let damaged_link_name = path
                .extension()
                .is_some_and(|ext| ext == "toml")
                .then(|| read(&path, None).ok().flatten())
                .flatten()
                .filter(|(config, _)| config.id == id)
                .map(|(config, _)| config.account().display_name);
            catalog.entries.push(Entry {
                path,
                config: Err(error),
                damaged_link_name,
                file_identity: None,
            });
        }
    }

    fn removal_mailbox(&self, base: &Path, id: &str) -> Result<PathBuf, FileError> {
        self.mailbox_path(base, id).or_else(|error| {
            let sibling = sibling_mailbox(base, id);
            if error.recovery == INTERRUPTED_REMOVAL_RECOVERY && has_removal_plan(&sibling, id) {
                Ok(sibling)
            } else {
                Err(error)
            }
        })
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
        self.remove_impl(base, id, delete, None, false)
    }

    /// Disconnect a mailbox while the caller holds the exclusive registry
    /// lock, preserving its ID so its mail and drafts can be reconnected.
    pub fn keep_for_replacement(
        &self,
        directory: &File,
        base: &Path,
        id: &str,
        snapshot: &str,
    ) -> Result<(), String> {
        let account = self.path(id).map_err(|err| err.to_string())?;
        let mailbox = self.store_path(base, id).map_err(|err| err.cli_message())?;
        if !read(&account, Some(id))
            .is_ok_and(|current| current.is_some_and(|(_, text)| text == snapshot))
        {
            return Err("The account file changed. Review it and try again.".into());
        }
        let store = Store::open_existing_without_index(&mailbox).map_err(|err| err.to_string())?;
        keep(&account, directory, &AccountId(id.into()), Some(&store))
    }

    pub fn remove_if_unchanged(
        &self,
        base: &Path,
        id: &str,
        delete: bool,
        expected: Option<FileIdentity>,
    ) -> Result<RemovalOutcome, String> {
        let expected =
            expected.ok_or("Couldn't verify the account file. Select the account again.")?;
        self.remove_impl(base, id, delete, Some(expected), false)
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
        if !recovery_only {
            self.migrate_at(Some(base))
                .map_err(|err| err.cli_message())?;
        }
        let directory = self.lock().map_err(|err| err.to_string())?;
        let account = self.path(id).map_err(|err| err.to_string())?;
        let mailbox = self
            .removal_mailbox(base, id)
            .map_err(|err| err.cli_message())?;
        let changed = match expected {
            Some(expected) => account.symlink_metadata().ok().map(file_identity) != Some(expected),
            None if recovery_only => !account
                .symlink_metadata()
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound),
            None => false,
        };
        if changed {
            return Err("The account file changed. Select the account again.".into());
        }
        let trash = removal_trash(&mailbox);
        if path_exists(&trash).map_err(|err| err.to_string())? {
            real_directory(&trash).map_err(|err| format!("{}: {err}", trash.display()))?;
            let plan = match read_plan(&trash) {
                Ok(plan) => plan,
                Err(err)
                    if err.kind() == io::ErrorKind::NotFound
                        && only_removal_sidecars(&trash, &mailbox)
                            .map_err(|err| err.to_string())? =>
                {
                    if !delete && trash.join("committed").exists() {
                        return Err("Permanently deleting local mail has already started.".into());
                    }
                    discard_staged_mailbox(&trash, &mailbox).map_err(|err| err.to_string())?;
                    clear_trash(&trash).map_err(|err| err.to_string())?;
                    return Ok(RemovalOutcome::Cleared);
                }
                Err(err) => return Err(err.to_string()),
            };
            if plan.account != id {
                return Err(format!(
                    "{} belongs to account {:?}. Select that account to recover it.",
                    trash.display(),
                    plan.account,
                ));
            }
            let _local = lock_mailbox(&mailbox)?;
            let staged = staged_file(&trash, &mailbox, "");
            let _staged = lock_mailbox(&staged)?;
            let committed = path_exists(&trash.join("committed")).map_err(|err| err.to_string())?;
            let snapshot = read_account_contents(&trash.join("account.toml"), Some(id))
                .map_err(|err| err.cli_message())?;
            let current = if committed && snapshot.is_some() {
                None
            } else {
                read(&account, Some(id)).map_err(|err| err.to_string())?
            };
            if let (Some((_, snapshot)), Some((_, current))) = (&snapshot, &current)
                && snapshot != current
            {
                return Err("The account file differs from the one saved before deletion. Move it aside before recovering local mail.".into());
            }
            let email = snapshot
                .as_ref()
                .or(current.as_ref())
                .map(|(config, _)| config.email.as_str());
            validate_mailbox(&staged, id, email, true).map_err(|err| err.cli_message())?;
            if !committed {
                validate_mailbox(&mailbox, id, email, true).map_err(|err| err.cli_message())?;
            }
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
        validate_mailbox(&mailbox, id, Some(&config.0.email), false)
            .map_err(|err| err.cli_message())?;
        let id = AccountId(id.to_owned());
        let mut store = match mailbox.symlink_metadata() {
            Ok(meta) if meta.is_file() && meta.len() != 0 => {
                Some(Store::open_existing_without_index(&mailbox).map_err(|err| err.to_string())?)
            }
            Ok(meta) if meta.is_file() => None,
            Ok(_) => return Err("Expected a regular mailbox file.".into()),
            Err(err) if err.kind() == io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.to_string()),
        };
        if !delete {
            keep(&account, &directory, &id, store.as_ref())?;
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
            lock_mailbox(&mailbox)?
        };
        validate_mailbox(&mailbox, id.as_str(), Some(&config.0.email), true)
            .map_err(|err| err.cli_message())?;
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

fn mark_entry(entry: &mut Entry, error: FileError) {
    entry.damaged_link_name = entry
        .config
        .as_ref()
        .ok()
        .map(|config| config.account().display_name);
    entry.config = Err(error);
    entry.file_identity = None;
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

pub(super) fn staged_file(trash: &Path, mailbox: &Path, suffix: &str) -> PathBuf {
    trash.join(
        mailbox_file(mailbox, suffix)
            .file_name()
            .expect("mailbox file name"),
    )
}

pub(super) fn mailbox_has_owner(base: &Path, id: &str) -> bool {
    mailbox_owner(base).is_ok_and(|owner| owner.is_some_and(|(owner, _)| owner == id))
        || staged_mailbox_id(base).is_some_and(|owner| owner == id)
}

pub(super) fn staged_mailbox_id(base: &Path) -> Option<String> {
    let trash = removal_trash(base);
    real_directory(&trash).ok()?;
    let plan = read_plan(&trash).ok()?;
    let staged = staged_file(&trash, base, "");
    let owner_matches = match inspect_mailbox(&staged, true).ok()? {
        MailboxOwnership::Owned((id, _)) => id == plan.account,
        MailboxOwnership::New => true,
        _ => false,
    };
    (owner_matches && validate_mailbox(base, &plan.account, None, false).is_ok())
        .then_some(plan.account)
}

fn has_removal_plan(mailbox: &Path, id: &str) -> bool {
    let trash = removal_trash(mailbox);
    real_directory(&trash).is_ok() && read_plan(&trash).is_ok_and(|plan| plan.account == id)
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
                return Err(format!(
                    "{}: Expected a regular mailbox file with one link.",
                    path.display()
                ));
            }
            Ok(Some(file))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(format!("{}: {err}", path.display())),
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
        Ok(_) => {
            // Older Fell versions committed before cleaning up drafts.
            compose::remove_draft_workspaces(&plan.drafts, &plan.account, mailbox)?;
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            move_mailbox(mailbox, trash, false).map_err(|err| err.to_string())?;
            disconnect(account, directory).map_err(|err| err.to_string())?;
            // If draft cleanup fails, leave SQLite intact so Restore can
            // recover the mailbox and its saved drafts.
            compose::remove_draft_workspaces(&plan.drafts, &plan.account, mailbox)?;
            File::create_new(&committed)
                .and_then(|file| file.sync_all())
                .and_then(|()| sync_dir(trash))
                .map_err(|err| err.to_string())?;
        }
        Err(err) => return Err(err.to_string()),
    }
    discard_staged_mailbox(trash, mailbox).map_err(|err| err.to_string())?;
    absent_ok(fs::remove_file(trash.join("account.toml"))).map_err(|err| err.to_string())?;
    clear_trash(trash).map_err(|err| err.to_string())
}

fn only_removal_sidecars(trash: &Path, mailbox: &Path) -> io::Result<bool> {
    for entry in fs::read_dir(trash)? {
        let entry = entry?;
        if entry.file_name() != "committed"
            && !MAILBOX_SUFFIXES
                .iter()
                .filter(|suffix| !suffix.is_empty())
                .any(|suffix| entry.path() == staged_file(trash, mailbox, suffix))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn discard_staged_mailbox(trash: &Path, mailbox: &Path) -> io::Result<()> {
    for suffix in MAILBOX_SUFFIXES {
        absent_ok(fs::remove_file(staged_file(trash, mailbox, suffix)))?;
    }
    Ok(())
}

fn clear_trash(trash: &Path) -> io::Result<()> {
    for entry in fs::read_dir(trash)? {
        let entry = entry?;
        if entry.file_name() != "plan" && entry.file_name() != "committed" {
            return Err(io::Error::other(format!(
                "Move {} out of the removal directory, then retry.",
                entry.path().display(),
            )));
        }
    }
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

fn keep(
    account: &Path,
    directory: &File,
    id: &AccountId,
    store: Option<&Store>,
) -> Result<(), String> {
    if let Some(store) = store {
        store
            .disable_account_grants(id)
            .map_err(|err| err.to_string())?;
    }
    disconnect(account, directory).map_err(|err| err.to_string())
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
            let base = root.path().join("mail.db");
            let registry = Registry::at(root.path().join("fell"), base.clone());
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
        let account =
            crate::AccountConfig::parse("email = 'mail@example.test'\npassword = 'fixture'")
                .unwrap()
                .account();
        let store = Store::open(&base).unwrap();
        store.upsert_account(&account).unwrap();
        drop(store);
        let trash = removal_trash(&base);
        fs::create_dir_all(&trash).unwrap();
        fs::write(trash.join("plan"), br#"["mail",[]]"#).unwrap();
        fs::write(trash.join("committed"), "").unwrap();
        fs::rename(&base, trash.join("mail.db")).unwrap();
        assert!(!registry.load_with_removals(&base).interrupted[0].can_restore);
        assert!(registry.remove(&base, "mail", false).is_err());
        assert!(trash.join("mail.db").is_file());
        assert_eq!(
            registry.remove(&base, "mail", true).unwrap(),
            RemovalOutcome::Deleted
        );
        assert!(!trash.exists());
    }

    #[test]
    fn sibling_trash_only_blocks_the_base_with_a_conflicting_staged_mailbox() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("mail.db");
        let registry = Registry::at(root.path().join("fell"), base.clone());
        fs::create_dir_all(registry.directory()).unwrap();
        let text = "email = 'mail@example.test'\npassword = 'fixture'";
        let config = crate::AccountConfig::parse(text).unwrap();
        let account = registry.path("mail").unwrap();
        publish(&registry.directory(), &account, text.as_bytes()).unwrap();
        Store::open(&base)
            .unwrap()
            .upsert_account(&config.account())
            .unwrap();
        let sibling = sibling_mailbox(&base, "mail");
        let trash = removal_trash(&sibling);
        let staged = staged_file(&trash, &sibling, "");
        fs::create_dir_all(&trash).unwrap();
        assert_eq!(registry.store_path(&base, "mail").unwrap(), base);
        fs::write(trash.join("plan"), br#"["mail",[]]"#).unwrap();
        assert_eq!(registry.store_path(&base, "mail").unwrap(), base);
        fs::write(&staged, "not a SQLite database".repeat(10)).unwrap();
        assert_eq!(registry.store_path(&base, "mail").unwrap(), base);
        fs::rename(&staged, root.path().join("not-a-db")).unwrap();
        Store::open(&staged)
            .unwrap()
            .upsert_account(&config.account())
            .unwrap();
        assert_eq!(registry.store_path(&base, "mail").unwrap(), base);
        assert!(registry.load_with_removals(&base).interrupted.is_empty());

        let mut other = config.account();
        other.email = "other@example.test".into();
        Store::open(&staged)
            .unwrap()
            .upsert_account(&other)
            .unwrap();
        publish(
            &trash,
            &trash.join("account.toml"),
            text.replace(&config.email, &other.email).as_bytes(),
        )
        .unwrap();
        fs::write(trash.join("committed"), "").unwrap();
        let error = registry.store_path(&base, "mail").unwrap_err();
        assert_eq!(error.recovery, INTERRUPTED_REMOVAL_RECOVERY);
        assert!(error.detail.contains(&*staged.to_string_lossy()));
        assert!(error.detail.contains(&other.email));
        let catalog = registry.load_with_removals(&base);
        assert_eq!(catalog.interrupted.len(), 1);
        assert!(!catalog.interrupted[0].can_restore);
        assert_eq!(
            registry
                .recover_removal(&base, "mail", true, catalog.entries[0].file_identity)
                .unwrap(),
            RemovalOutcome::Deleted
        );
        assert!(account.is_file());
        assert_eq!(registry.store_path(&base, "mail").unwrap(), base);
        assert_eq!(mailbox_owner(&base).unwrap().unwrap().1, config.email);
    }

    #[test]
    fn recovery_cannot_restore_or_delete_a_staged_mailbox_with_a_different_id() {
        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("mail.db");
        let registry = Registry::at(root.path().join("fell"), base.clone());
        fs::create_dir_all(registry.directory()).unwrap();
        let mailbox = sibling_mailbox(&base, "work");
        let trash = removal_trash(&mailbox);
        fs::create_dir_all(&trash).unwrap();
        let staged = staged_file(&trash, &mailbox, "");
        let text = "email = 'same@example.test'\npassword = 'fixture'";
        let owner = crate::AccountConfig::parse(text).unwrap().account();
        Store::open(&staged)
            .unwrap()
            .upsert_account(&owner)
            .unwrap();
        publish(&trash, &trash.join("account.toml"), text.as_bytes()).unwrap();
        fs::write(trash.join("plan"), br#"["work",[]]"#).unwrap();
        for delete in [false, true] {
            let failure = registry
                .recover_removal(&base, "work", delete, None)
                .unwrap_err();
            assert!(
                failure.contains("Account \"mail\" (same@example.test)"),
                "{failure}"
            );
            assert!(failure.contains(&*staged.to_string_lossy()), "{failure}");
        }
        assert!(staged.is_file());
        assert!(!mailbox.exists());
        assert!(!trash.join("committed").exists());
    }

    #[test]
    fn staged_ownership_reads_the_wal_without_creating_shared_memory() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("mail.db");
        let conn = rusqlite::Connection::open(&live).unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts(id TEXT, email TEXT);
             PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             INSERT INTO accounts VALUES ('mail', 'mail@example.test');",
        )
        .unwrap();
        let trash = removal_trash(&live);
        fs::create_dir(&trash).unwrap();
        let staged = staged_file(&trash, &live, "");
        fs::copy(&live, &staged).unwrap();
        fs::copy(
            mailbox_file(&live, "-wal"),
            staged_file(&trash, &live, "-wal"),
        )
        .unwrap();
        validate_mailbox(&staged, "mail", Some("mail@example.test"), true).unwrap();
        assert!(!staged_file(&trash, &live, "-shm").exists());
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 2);
    }

    #[test]
    fn leftover_files_keep_the_journal_and_sidecar_only_trash_can_be_cleared() {
        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let base = root.join("mail.db");
        let registry = Registry::resolve().unwrap();
        fs::create_dir_all(registry.directory()).unwrap();
        let trash = removal_trash(&base);
        fs::create_dir(&trash).unwrap();
        fs::write(trash.join("plan"), br#"["mail",[]]"#).unwrap();
        fs::write(trash.join("committed"), "").unwrap();
        let sidecar = staged_file(&trash, &base, "-shm");
        fs::write(&sidecar, "").unwrap();
        let unexpected = trash.join("unexpected");
        fs::write(&unexpected, "").unwrap();
        let failure = registry
            .recover_removal(&base, "mail", true, None)
            .unwrap_err();
        assert!(
            failure.contains(&*unexpected.to_string_lossy()),
            "{failure}"
        );
        assert!(trash.join("plan").is_file());
        assert!(trash.join("committed").is_file());
        assert!(!sidecar.exists());
        fs::rename(&unexpected, root.join("unexpected")).unwrap();
        assert_eq!(
            registry.recover_removal(&base, "mail", true, None).unwrap(),
            RemovalOutcome::Deleted
        );

        fs::create_dir(&trash).unwrap();
        fs::write(&sidecar, "").unwrap();
        fs::write(trash.join("committed"), "").unwrap();
        assert!(registry.mailbox_reserved(&base, "mail").unwrap());
        assert_eq!(
            registry.recover_removal(&base, "mail", true, None).unwrap(),
            RemovalOutcome::Cleared
        );
        assert!(!trash.exists());
        assert!(!registry.mailbox_reserved(&base, "mail").unwrap());
    }

    #[test]
    fn failed_draft_cleanup_leaves_the_staged_mailbox_restorable() {
        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        let base = root.join("mail.db");
        let account = registry.path("work").unwrap();
        let mailbox = registry.mailbox_path(&base, "work").unwrap();
        let trash = removal_trash(&mailbox);
        fs::create_dir_all(registry.directory()).unwrap();
        fs::create_dir_all(&trash).unwrap();
        let config = "email = 'work@example.test'\npassword = 'fixture'\n";
        publish(&trash, &trash.join("account.toml"), config.as_bytes()).unwrap();
        let owner = crate::AccountConfig::parse_for(config, "work")
            .unwrap()
            .account();
        Store::open(staged_file(&trash, &mailbox, ""))
            .unwrap()
            .upsert_account(&owner)
            .unwrap();
        fs::write(trash.join("plan"), br#"["work",["cblocked"]]"#).unwrap();
        // A directory in place of an index file makes cleanup fail regardless
        // of the effective user's privileges.
        fs::create_dir_all(root.join("data/compose-stores/cblocked")).unwrap();
        let failure = registry
            .recover_removal(&base, "work", true, None)
            .unwrap_err();
        assert!(failure.contains("Is a directory"), "{failure}");
        assert!(!trash.join("committed").exists());
        assert!(registry.load_with_removals(&base).interrupted[0].can_restore);
        assert_eq!(
            registry
                .recover_removal(&base, "work", false, None)
                .unwrap(),
            RemovalOutcome::Restored { connected: true }
        );
        assert_eq!(fs::read_to_string(account).unwrap(), config);
        assert_eq!(
            mailbox_owner(&mailbox).unwrap(),
            Some((owner.id.0, owner.email))
        );
    }

    #[test]
    fn a_non_directory_draft_workspace_keeps_the_mailbox_restorable() {
        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let base = root.join("mail.db");
        let registry = Registry::resolve().unwrap();
        fs::create_dir_all(registry.directory()).unwrap();
        let text = "email = 'mail@example.test'\npassword = 'fixture'";
        let account = registry.path("mail").unwrap();
        publish(&registry.directory(), &account, text.as_bytes()).unwrap();
        let config = crate::AccountConfig::parse(text).unwrap();
        Store::open(&base)
            .unwrap()
            .upsert_account(&config.account())
            .unwrap();
        let folder = root.join("drafts/kepttext");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("draft.md"), "secret draft body").unwrap();
        let control = compose::control_dir(&folder);
        fs::write(&control, "not a directory").unwrap();
        compose::remember_compose_store("kepttext", &base).unwrap();
        let failure = registry.remove(&base, "mail", true).unwrap_err();
        assert!(failure.contains(&*control.to_string_lossy()), "{failure}");
        assert!(!removal_trash(&base).join("committed").exists());
        assert!(registry.load_with_removals(&base).interrupted[0].can_restore);
        assert_eq!(
            registry
                .recover_removal(&base, "mail", false, None)
                .unwrap(),
            RemovalOutcome::Restored { connected: true }
        );
        assert_eq!(
            fs::read_to_string(folder.join("draft.md")).unwrap(),
            "secret draft body"
        );
    }

    #[test]
    fn a_relative_mailbox_route_cleans_drafts_indexed_by_absolute_path() {
        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let base = root.join("mail.db");
        let registry = Registry::resolve().unwrap();
        fs::create_dir_all(registry.directory()).unwrap();
        let text = "email = 'mail@example.test'\npassword = 'fixture'";
        let account = registry.path("mail").unwrap();
        publish(&registry.directory(), &account, text.as_bytes()).unwrap();
        let config = crate::AccountConfig::parse(text).unwrap();
        Store::open(&base)
            .unwrap()
            .upsert_account(&config.account())
            .unwrap();
        let folder = root.join("drafts/kepttext");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join("draft.md"), "secret draft body").unwrap();
        compose::remember_compose_store("kepttext", &base).unwrap();
        std::env::set_current_dir(&root).unwrap();
        assert_eq!(
            registry
                .remove(Path::new("data/../mail.db"), "mail", true)
                .unwrap(),
            RemovalOutcome::Deleted
        );
        assert!(!folder.exists());
        assert!(!root.join("data/compose-stores/kepttext").exists());
    }
}
