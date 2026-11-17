//! Compose drafts and their attachment bytes live in the mailbox store.
//!
//! The runtime folder can disappear. A save copies the resolved files into
//! SQLite so a restart loses nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rusqlite::{OptionalExtension, params};

use crate::store::Store;
use crate::{AccountId, Result};

use super::frontmatter::{AttachmentRef, DraftFields};
use super::mime::ResolvedFile;
use super::workspace;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredCompose {
    pub id: String,
    pub account_id: AccountId,
    pub fields: DraftFields,
    pub body: String,
    pub files: Vec<ResolvedFile>,
    pub identities: Vec<String>,
    pub owner: String,
}

impl Store {
    pub fn save_compose(&self, draft: &StoredCompose) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO compose_drafts (
                id, account_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
                subject, body, in_reply_to, identities, owner, updated_unix
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
                from_addr = excluded.from_addr,
                to_addrs = excluded.to_addrs,
                cc_addrs = excluded.cc_addrs,
                bcc_addrs = excluded.bcc_addrs,
                subject = excluded.subject,
                body = excluded.body,
                in_reply_to = excluded.in_reply_to,
                identities = excluded.identities,
                owner = CASE WHEN excluded.owner = '' THEN compose_drafts.owner ELSE excluded.owner END,
                updated_unix = excluded.updated_unix",
            params![
                draft.id,
                draft.account_id.as_str(),
                draft.fields.from,
                join_list(&draft.fields.to),
                join_list(&draft.fields.cc),
                join_list(&draft.fields.bcc),
                draft.fields.subject,
                draft.body,
                draft.fields.in_reply_to,
                join_list(&draft.identities),
                draft.owner,
                now_unix(),
            ],
        )?;
        tx.execute(
            "DELETE FROM compose_attachments WHERE draft_id = ?1",
            params![draft.id],
        )?;
        for file in &draft.files {
            tx.execute(
                "INSERT INTO compose_attachments (draft_id, path, name, bytes)
                 VALUES (?1, ?2, ?3, ?4)",
                params![draft.id, file.path, file.display_name, file.bytes],
            )?;
        }
        tx.commit()?;
        self.seal_private_files()?;
        Ok(())
    }

    /// Update the draft text and leave stored attachment bytes in place.
    pub fn save_compose_text(&self, draft: &StoredCompose) -> Result<()> {
        self.conn.execute(
            "INSERT INTO compose_drafts (
                id, account_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
                subject, body, in_reply_to, identities, owner, updated_unix
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
                from_addr = excluded.from_addr,
                to_addrs = excluded.to_addrs,
                cc_addrs = excluded.cc_addrs,
                bcc_addrs = excluded.bcc_addrs,
                subject = excluded.subject,
                body = excluded.body,
                in_reply_to = excluded.in_reply_to,
                identities = excluded.identities,
                owner = CASE WHEN excluded.owner = '' THEN compose_drafts.owner ELSE excluded.owner END,
                updated_unix = excluded.updated_unix",
            params![
                draft.id,
                draft.account_id.as_str(),
                draft.fields.from,
                join_list(&draft.fields.to),
                join_list(&draft.fields.cc),
                join_list(&draft.fields.bcc),
                draft.fields.subject,
                draft.body,
                draft.fields.in_reply_to,
                join_list(&draft.identities),
                draft.owner,
                now_unix(),
            ],
        )?;
        self.seal_private_files()?;
        Ok(())
    }

    pub fn load_compose(&self, id: &str) -> Result<Option<StoredCompose>> {
        let row = self
            .conn
            .query_row(
                "SELECT account_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
                        subject, body, in_reply_to, identities, owner
                 FROM compose_drafts WHERE id = ?1",
                params![id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, Option<String>>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                    ))
                },
            )
            .optional()?;
        let Some((account, from, to, cc, bcc, subject, body, reply, identities, owner)) = row
        else {
            return Ok(None);
        };
        let mut stmt = self.conn.prepare(
            "SELECT path, name, bytes FROM compose_attachments WHERE draft_id = ?1 ORDER BY path",
        )?;
        let files = stmt
            .query_map(params![id], |row| {
                Ok(ResolvedFile {
                    path: row.get(0)?,
                    display_name: row.get(1)?,
                    bytes: row.get(2)?,
                    inline: false,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Some(StoredCompose {
            id: id.to_string(),
            account_id: AccountId(account),
            fields: DraftFields {
                from,
                to: split_list(&to),
                cc: split_list(&cc),
                bcc: split_list(&bcc),
                subject,
                in_reply_to: reply.filter(|value| !value.is_empty()),
                attachments: files
                    .iter()
                    .map(|file| AttachmentRef {
                        path: file.path.clone(),
                        name: Some(file.display_name.clone()),
                        line: 0,
                    })
                    .collect(),
            },
            body,
            files,
            identities: split_list(&identities),
            owner,
        }))
    }

    pub fn compose_owner(&self, id: &str) -> Result<Option<String>> {
        Ok(self.load_compose(id)?.map(|draft| draft.owner))
    }

    /// Addresses this account has already mailed, plus its own address.
    pub fn mailed_addresses(&self, account_id: &AccountId) -> Result<HashSet<String>> {
        let mut known = HashSet::new();
        if let Some(account) = self.get_account(account_id)? {
            known.insert(account.email.to_ascii_lowercase());
        }
        let mut stmt = self.conn.prepare(
            "SELECT folders, to_line, cc_line, bcc_line FROM messages WHERE account_id = ?1",
        )?;
        let rows = stmt.query_map(params![account_id.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (folders, to, cc, bcc) = row?;
            if !split_list(&folders).iter().any(|folder| {
                folder.eq_ignore_ascii_case("sent") || folder.eq_ignore_ascii_case("sent mail")
            }) {
                continue;
            }
            for line in [&to, &cc, &bcc] {
                for address in decode_address_line(line) {
                    known.insert(address);
                }
            }
        }
        Ok(known)
    }

    pub fn restore_compose_folder(&self, id: &str, folder: &Path) -> Result<Option<StoredCompose>> {
        let Some(stored) = self.load_compose(id)? else {
            return Ok(None);
        };
        workspace::materialize_in(
            folder.parent().unwrap_or(folder),
            id,
            &stored.fields,
            &stored.body,
            &stored.account_id,
        )
        .map_err(crate::Error::Invalid)?;
        for file in &stored.files {
            workspace::write_inside(folder, &file.path, &file.bytes)
                .map_err(crate::Error::Invalid)?;
        }
        Ok(Some(stored))
    }
}

fn join_list(items: &[String]) -> String {
    items.join("\u{1e}")
}

fn split_list(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        Vec::new()
    } else {
        raw.split('\u{1e}')
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect()
    }
}

fn decode_address_line(raw: &str) -> Vec<String> {
    if raw.is_empty() {
        return Vec::new();
    }
    raw.split('\u{1e}')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let email = part.split('\u{1f}').nth(1).unwrap_or(part);
            let email = email.trim();
            if email.contains('@') {
                Some(email.to_ascii_lowercase())
            } else {
                None
            }
        })
        .collect()
}

pub fn remember_compose_store(id: &str, store: &Path) -> std::result::Result<(), String> {
    let Some(path) = compose_index_file(id) else {
        return Err("no durable directory for the compose index".into());
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    workspace::atomic_write(&path, store.display().to_string().as_bytes())
}

pub fn remembered_store_path(id: &str) -> Option<PathBuf> {
    let path = compose_index_file(id)?;
    let text = std::fs::read_to_string(path).ok()?;
    let store = PathBuf::from(text.trim());
    store.is_file().then_some(store)
}

pub fn stored_compose_account(id: &str) -> Option<String> {
    let store = open_store_for(id)?;
    store
        .load_compose(id)
        .ok()
        .flatten()
        .map(|draft| draft.account_id.as_str().to_string())
}

pub fn stored_compose_owner(id: &str) -> Option<String> {
    let store = open_store_for(id)?;
    let owner = store.compose_owner(id).ok().flatten()?;
    if owner.is_empty() { None } else { Some(owner) }
}

pub fn open_owned_draft(
    id: &str,
    agent: &str,
    account: &str,
) -> std::result::Result<PathBuf, String> {
    let folder = workspace::draft_folder(id)?;
    if draft_file_is_regular(&folder) {
        return Ok(folder);
    }
    let Some(store) = open_store_for(id) else {
        return Err(format!("draft {id} is not open"));
    };
    let Some(stored) = store.load_compose(id).map_err(|err| err.to_string())? else {
        return Err(format!("draft {id} is not open"));
    };
    if stored.account_id.as_str() != account {
        return Err("this draft belongs to another account".into());
    }
    if stored.owner != agent {
        if stored.owner.is_empty() {
            return Err(format!("draft {id} is not open"));
        }
        return Err("this draft belongs to another agent".into());
    }
    store
        .restore_compose_folder(id, &folder)
        .map_err(|err| err.to_string())?;
    Ok(folder)
}

pub fn mailbox_db_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("FELL_STORE") {
        return Some(PathBuf::from(path));
    }
    if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        return Some(PathBuf::from(data).join("fell/mail.db"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share/fell/mail.db"))
}

pub(crate) fn compose_index_file(id: &str) -> Option<PathBuf> {
    let root = if let Some(dir) = std::env::var_os("FELL_COMPOSE_INDEX") {
        PathBuf::from(dir)
    } else if let Some(data) = std::env::var_os("XDG_DATA_HOME") {
        PathBuf::from(data).join("fell/compose-stores")
    } else {
        PathBuf::from(std::env::var_os("HOME")?).join(".local/share/fell/compose-stores")
    };
    Some(root.join(id))
}

pub(crate) fn open_store_for_draft(id: &str) -> Option<Store> {
    open_store_for(id)
}

fn open_store_for(id: &str) -> Option<Store> {
    let path = remembered_store_path(id).or_else(mailbox_db_path)?;
    if !path.is_file() {
        return None;
    }
    Store::open(path).ok()
}

/// Recreate the runtime folder from the mailbox when `draft.md` is gone.
pub fn ensure_draft_folder(id: &str) -> std::result::Result<PathBuf, String> {
    let folder = workspace::draft_folder(id)?;
    if draft_file_is_regular(&folder) {
        return Ok(folder);
    }
    let Some(store) = open_store_for(id) else {
        return Err(format!("draft {id} is not open"));
    };
    match store
        .restore_compose_folder(id, &folder)
        .map_err(|err| err.to_string())?
    {
        Some(_) => Ok(folder),
        None => Err(format!("draft {id} is not open")),
    }
}

fn draft_file_is_regular(folder: &Path) -> bool {
    std::fs::symlink_metadata(folder.join("draft.md"))
        .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Account, BackendKind};

    #[test]
    fn attachments_survive_a_reload() {
        let store = Store::open_in_memory().unwrap();
        let account = Account {
            id: AccountId("a".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        store
            .save_compose(&StoredCompose {
                id: "d1".into(),
                account_id: account.id.clone(),
                fields: DraftFields {
                    from: "jon@example.com".into(),
                    to: vec!["ada@example.com".into()],
                    subject: "Q3".into(),
                    ..DraftFields::default()
                },
                body: "![chart](chart.png)\n".into(),
                files: vec![ResolvedFile {
                    path: "chart.png".into(),
                    display_name: "chart.png".into(),
                    bytes: b"png".to_vec(),
                    inline: true,
                }],
                identities: vec!["jon@example.com".into()],
                owner: String::new(),
            })
            .unwrap();
        let loaded = store.load_compose("d1").unwrap().unwrap();
        assert_eq!(loaded.body, "![chart](chart.png)\n");
        assert_eq!(loaded.files[0].bytes, b"png");
        assert_eq!(loaded.fields.to, vec!["ada@example.com".to_string()]);
    }

    #[test]
    fn a_flushed_edit_restores_when_the_runtime_folder_is_gone() {
        let dir = std::env::temp_dir().join(format!(
            "fell-persist-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let drafts = dir.join("drafts");
        let db = dir.join("mail.db");
        let store = Store::open(&db).unwrap();
        let account = Account {
            id: AccountId("a".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hello".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&drafts, "keep", &fields, "original\n", &account.id).unwrap();
        let mut session = super::super::session::DraftSession::create(
            folder.clone(),
            "keep",
            account.id.clone(),
            fields,
            "original\n",
        )
        .unwrap();
        session.bind_store(&db);
        session.set_body_from_user("edited line\n![chart](chart.png)\n".into());
        std::fs::write(folder.join("chart.png"), b"png-bytes").unwrap();
        session.flush().unwrap();
        drop(store);
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(drafts.join(".keep.private"));
        let store = Store::open(&db).unwrap();
        store
            .restore_compose_folder("keep", &drafts.join("keep"))
            .unwrap()
            .unwrap();
        let text = std::fs::read_to_string(drafts.join("keep/draft.md")).unwrap();
        assert!(text.contains("edited line"), "{text}");
        assert_eq!(
            std::fs::read(drafts.join("keep/chart.png")).unwrap(),
            b"png-bytes"
        );
    }

    #[test]
    fn another_account_or_agent_cannot_open_the_folder() {
        let dir = std::env::temp_dir().join(format!(
            "fell-owned-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            bcc: vec!["secret@example.com".into()],
            subject: "Hello".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&dir, "r1", &fields, "body\n", &AccountId("acct-a".into()))
                .unwrap();
        let control = workspace::control_dir(&folder);
        std::fs::create_dir_all(&control).unwrap();
        std::fs::write(control.join("owner"), "agent:bob\n").unwrap();
        std::fs::write(control.join("account"), "acct-b\n").unwrap();
        let db = dir.join("mail.db");
        let store = Store::open(&db).unwrap();
        store
            .upsert_account(&Account {
                id: AccountId("acct-a".into()),
                display_name: "Ada".into(),
                email: "jon@example.com".into(),
                backend_kind: BackendKind::Bridge,
            })
            .unwrap();
        store
            .save_compose(&StoredCompose {
                id: "r1".into(),
                account_id: AccountId("acct-a".into()),
                fields: fields.clone(),
                body: "body\n".into(),
                files: Vec::new(),
                identities: vec!["jon@example.com".into()],
                owner: "agent:ada".into(),
            })
            .unwrap();
        let opened = super::super::session::DraftSession::open_with_store(
            &folder,
            AccountId("acct-b".into()),
            &store,
        );
        match opened {
            Ok(_) => panic!("opened with the wrong account"),
            Err(err) => assert!(err.contains("account"), "{err}"),
        }
        let opened = super::super::session::DraftSession::open_with_store(
            &folder,
            AccountId("acct-a".into()),
            &store,
        )
        .unwrap();
        assert_eq!(opened.owner(), "agent:ada");
        assert_eq!(opened.account_id().as_str(), "acct-a");
    }

    #[test]
    fn restore_does_not_follow_an_attachment_symlink() {
        let dir = std::env::temp_dir().join(format!(
            "fell-restore-link-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("mail.db");
        let store = Store::open(&db).unwrap();
        let account = Account {
            id: AccountId("a".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: BackendKind::Bridge,
        };
        store.upsert_account(&account).unwrap();
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hello".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&dir, "keep", &fields, "body\n", &account.id).unwrap();
        let outside = dir.join("outside.txt");
        std::fs::write(&outside, b"keep-me").unwrap();
        std::os::unix::fs::symlink(&outside, folder.join("chart.png")).unwrap();
        store
            .save_compose(&StoredCompose {
                id: "keep".into(),
                account_id: account.id.clone(),
                fields,
                body: "body\n".into(),
                files: vec![crate::compose::ResolvedFile {
                    path: "chart.png".into(),
                    display_name: "chart.png".into(),
                    bytes: b"local".to_vec(),
                    inline: false,
                }],
                identities: vec!["jon@example.com".into()],
                owner: "agent:ada".into(),
            })
            .unwrap();
        let err = store.restore_compose_folder("keep", &folder).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("symlink"), "{text}");
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep-me");
    }
}
