//! Disconnect a registry account, with a recoverable local deletion.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use super::{FileIdentity, Registry, file_identity, removal_trash};
use crate::{AccountId, Store, compose};

struct Removal {
    account: String,
    drafts: Vec<String>,
}

impl Registry {
    /// Revoke grants before disconnecting. A confirmed deletion first moves
    /// all SQLite files into a durable staging directory. If interrupted, the
    /// mailbox stays closed until removal resumes using that directory.
    pub fn remove(&self, base: &Path, id: &str, delete: bool) -> Result<(), String> {
        self.remove_if_unchanged(base, id, delete, None)
    }

    pub fn remove_if_unchanged(
        &self,
        base: &Path,
        id: &str,
        delete: bool,
        expected: Option<FileIdentity>,
    ) -> Result<(), String> {
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
            let plan = match fs::read(trash.join("plan")) {
                Ok(plan) => plan,
                Err(err)
                    if err.kind() == io::ErrorKind::NotFound
                        && !account.exists()
                        && fs::read_dir(&trash)
                            .is_ok_and(|mut entries| entries.next().is_none()) =>
                {
                    fs::remove_dir(&trash).map_err(|err| err.to_string())?;
                    return sync_dir(trash.parent().expect("mailbox directory"))
                        .map_err(|err| err.to_string());
                }
                Err(err) => return Err(err.to_string()),
            };
            let (account_id, drafts) =
                serde_json::from_slice(&plan).map_err(|err| err.to_string())?;
            let plan = Removal {
                account: account_id,
                drafts,
            };
            if plan.account != id {
                return Err("The mailbox removal directory belongs to another account.".into());
            }
            let _locked = lock_mailbox(&mailbox)?;
            return finish(&account, &directory, &mailbox, &trash, &plan);
        }

        if !account
            .symlink_metadata()
            .is_ok_and(|meta| meta.is_file() || meta.is_symlink())
        {
            return Err(format!("No account file for {id:?}"));
        }
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
            return disconnect(&account, &directory).map_err(|err| err.to_string());
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
        };
        let mut journal =
            File::create(staged.path().join("plan")).map_err(|err| err.to_string())?;
        journal
            .write_all(
                &serde_json::to_vec(&(&plan.account, &plan.drafts))
                    .map_err(|err| err.to_string())?,
            )
            .and_then(|_| journal.sync_all())
            .map_err(|err| err.to_string())?;
        sync_dir(staged.path()).map_err(|err| err.to_string())?;
        fs::rename(staged.path(), &trash).map_err(|err| err.to_string())?;
        sync_dir(parent).map_err(|err| err.to_string())?;
        let result = finish(&account, &directory, &mailbox, &trash, &plan);
        drop(locked);
        result
    }
}

fn lock_mailbox(path: &Path) -> Result<Option<File>, String> {
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => {
            file.try_lock().map_err(|err| format!(
                "Close Fell and any fell-mcp sessions using this account before deleting local mail: {err}"
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
    // Once the staged DB exists, all of its sidecars have already moved.
    // This also lets a retry skip staging after the config is gone.
    for suffix in ["-wal", "-shm", "-journal", ".embed.lock", ""] {
        let mut source = mailbox.as_os_str().to_os_string();
        source.push(suffix);
        let source = PathBuf::from(source);
        let target = trash.join(source.file_name().ok_or("Missing mailbox file name")?);
        if target.symlink_metadata().is_ok() {
            continue;
        }
        absent_ok(fs::rename(&source, &target)).map_err(|err| err.to_string())?;
    }
    sync_dir(trash).map_err(|err| err.to_string())?;
    let parent = trash.parent().expect("trash is beside the mailbox");
    sync_dir(parent).map_err(|err| err.to_string())?;
    disconnect(account, directory).map_err(|err| err.to_string())?;
    compose::remove_draft_workspaces(&plan.drafts, &plan.account, mailbox)?;
    for entry in fs::read_dir(trash).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        if entry.file_name() != "plan" {
            fs::remove_file(entry.path()).map_err(|err| err.to_string())?;
        }
    }
    sync_dir(trash).map_err(|err| err.to_string())?;
    fs::remove_file(trash.join("plan")).map_err(|err| err.to_string())?;
    fs::remove_dir(trash).map_err(|err| err.to_string())?;
    sync_dir(parent).map_err(|err| err.to_string())
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
        fs::write(trash.join("work.db-wal"), "staged sidecar").unwrap();
        assert!(file.is_file());
        assert!(registry.store_path(&base, id.as_str()).is_err());
        assert!(Store::open_existing(&db).is_err());
        registry.remove(&base, id.as_str(), false).unwrap();
        assert!(!file.exists());
        assert!(!db.exists());
        assert!(!trash.exists());
        assert!(!folder.exists());
        assert!(!compose::control_dir(&folder).exists());
        assert!(!root.join("data/compose-stores/cwork").exists());
        assert!(sibling.is_dir());
        assert!(root.join("data/compose-stores/cmail").is_file());

        // A fresh confirmed delete takes the same durable path.
        configure();
        drop(Store::open(&db).unwrap());
        registry.remove(&base, id.as_str(), true).unwrap();
        assert!(!file.exists());
        assert!(!db.exists());
        assert!(!trash.exists());
    }
}
