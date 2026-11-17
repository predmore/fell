//! Compose drafts and their attachment bytes live in the mailbox store.
//!
//! The runtime folder can disappear. A save copies the resolved files into
//! SQLite so a restart loses nothing.

use std::collections::HashSet;
use std::path::Path;

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
}

impl Store {
    pub fn save_compose(&self, draft: &StoredCompose) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO compose_drafts (
                id, account_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
                subject, body, in_reply_to, updated_unix
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
                from_addr = excluded.from_addr,
                to_addrs = excluded.to_addrs,
                cc_addrs = excluded.cc_addrs,
                bcc_addrs = excluded.bcc_addrs,
                subject = excluded.subject,
                body = excluded.body,
                in_reply_to = excluded.in_reply_to,
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

    pub fn load_compose(&self, id: &str) -> Result<Option<StoredCompose>> {
        let row = self
            .conn
            .query_row(
                "SELECT account_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
                        subject, body, in_reply_to
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
                    ))
                },
            )
            .optional()?;
        let Some((account, from, to, cc, bcc, subject, body, reply)) = row else {
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
        }))
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
            let path = folder.join(&file.path);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, &file.bytes)?;
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
            })
            .unwrap();
        let loaded = store.load_compose("d1").unwrap().unwrap();
        assert_eq!(loaded.body, "![chart](chart.png)\n");
        assert_eq!(loaded.files[0].bytes, b"png");
        assert_eq!(loaded.fields.to, vec!["ada@example.com".to_string()]);
    }
}
