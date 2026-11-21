//! Sent membership until a real copy backs it. An empty server folder can
//! never sweep a just-sent message or one that lives only in Fell.

use rusqlite::{Connection, params};

use super::{MergeRoles, Store, canonical_id_on, copies, find_stored, immediate_transaction};
use super::{insert_arrival, sent_bcc, split_field, thread_id_for_message_id};
use crate::folders::{FolderRole, FolderRoles};
use crate::{AccountId, Address, Error, Message, MessageId, Result, ThreadId};

pub(super) const LOCAL_SENT_TABLE: &str = "
CREATE TABLE IF NOT EXISTS local_sent (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    folder TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id),
    FOREIGN KEY (account_id, message_id) REFERENCES messages(account_id, message_id) ON DELETE CASCADE
) WITHOUT ROWID;
";

impl Store {
    pub(crate) fn record_sent_message(
        &self,
        account: &AccountId,
        draft_id: &str,
        id: &MessageId,
        bcc: &[Address],
        mime: &[u8],
        append: bool,
    ) -> Result<(ThreadId, Message)> {
        let mut message =
            crate::imap::rfc822::local_sent_message(account, FolderRole::Sent.default_name(), mime);
        if &message.headers.message_id != id || !message.bcc.is_empty() {
            return Err(Error::Action("unexpected headers in sent MIME".into()));
        }
        if append {
            // Apply the SMTP check to the raw headers too. A Bcc field
            // with no parseable addresses still cannot go into the outbox.
            crate::smtp::sender(mime)?;
        }
        match self.insert_sent_message(draft_id, bcc, &mut message, append.then_some(mime)) {
            Ok(thread) => Ok((thread, message)),
            Err(local) => {
                if append && let Err(queue) = self.queue_sent_after_failed_insert(account, id, mime)
                {
                    return Err(Error::Action(format!(
                        "{local}. Could not queue Sent copy: {queue}"
                    )));
                }
                Err(local)
            }
        }
    }

    fn insert_sent_message(
        &self,
        draft_id: &str,
        bcc: &[Address],
        message: &mut Message,
        append: Option<&[u8]>,
    ) -> Result<ThreadId> {
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        let tx = immediate_transaction(&self.conn)?;
        let account = &message.account_id;
        let id = &message.headers.message_id;
        let roles = self.folder_roles(account)?;
        let folder = roles.name(FolderRole::Sent);
        message.folders = vec![folder.to_string()];
        if !has_copy(&tx, account, id, folder)? {
            insert_arrival(&tx, message, &mut MergeRoles::new(Some(&roles)))?;
            let canonical = canonical_id_on(&tx, account, id)?
                .ok_or_else(|| Error::Action("sent message missing after insert".into()))?;
            tx.execute(
                "INSERT OR IGNORE INTO local_sent (account_id, message_id, folder) VALUES (?1, ?2, ?3)",
                params![account.as_str(), canonical, folder],
            )?;
            if let Some(mime) = append {
                crate::outbox::queue_append(&tx, account, id, folder, mime)?;
            }
        }
        sent_bcc::record(&tx, account, id, bcc)?;
        crate::compose::record_confirmed_send(&tx, account, draft_id, id)?;
        self.rethread_written(&tx, account)?;
        let thread = thread_id_for_message_id(&tx, account, id)?
            .ok_or_else(|| Error::Action("sent message missing after threading".into()))?;
        message.headers.message_id = MessageId(
            canonical_id_on(&tx, account, id)?
                .ok_or_else(|| Error::Action("sent message missing after threading".into()))?,
        );
        tx.commit()?;
        // Only the person's conversation gets the envelope's Bcc.
        message.bcc = bcc.to_vec();
        Ok(thread)
    }

    fn queue_sent_after_failed_insert(
        &self,
        account: &AccountId,
        id: &MessageId,
        mime: &[u8],
    ) -> Result<()> {
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        let tx = immediate_transaction(&self.conn)?;
        let roles = self.folder_roles(account)?;
        let folder = roles.name(FolderRole::Sent);
        if !has_copy(&tx, account, id, folder)? {
            crate::outbox::queue_append(&tx, account, id, folder, mime)?;
        }
        tx.commit()
    }
}

fn has_copy(conn: &Connection, account: &AccountId, id: &MessageId, folder: &str) -> Result<bool> {
    let Some(canonical) = canonical_id_on(conn, account, id)? else {
        return Ok(false);
    };
    Ok(conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM message_copies
         WHERE account_id = ?1 AND message_id = ?2 AND folder = ?3 COLLATE NOCASE)",
        params![account.as_str(), canonical, folder],
        |row| row.get(0),
    )?)
}

pub(super) fn repoint(conn: &Connection, account: &AccountId, from: &str, to: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO local_sent (account_id, message_id, folder)
         SELECT account_id, ?3, folder FROM local_sent WHERE account_id = ?1 AND message_id = ?2",
        params![account.as_str(), from, to],
    )?;
    Ok(())
}

pub(super) fn copy_arrived(
    conn: &Connection,
    account: &AccountId,
    canonical: &str,
    folder: &str,
) -> Result<()> {
    conn.prepare_cached(
        "DELETE FROM local_sent WHERE account_id = ?1 AND message_id = ?2 AND folder = ?3 COLLATE NOCASE",
    )?
    .execute(params![account.as_str(), canonical, folder])?;
    Ok(())
}

/// The first LIST may discover a localized Sent folder after the send.
pub(super) fn on_listing(
    conn: &Connection,
    account: &AccountId,
    roles: &FolderRoles,
) -> Result<bool> {
    let Some(target) = roles.get(FolderRole::Sent) else {
        return Ok(false);
    };
    let mut stmt = conn.prepare_cached(
        "SELECT message_id, folder FROM local_sent WHERE account_id = ?1 AND folder != ?2",
    )?;
    let rows = stmt
        .query_map(params![account.as_str(), target.name], |row| {
            Ok((MessageId(row.get(0)?), row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut changed = false;
    for (id, old) in rows {
        let Some((stored, _)) = find_stored(conn, account, &id)? else {
            continue;
        };
        let mut folders = split_field(&stored.folders);
        if folders.contains(&old) {
            folders.retain(|f| *f != old);
            if !folders.contains(&target.name) {
                folders.push(target.name.clone());
            }
            changed |= copies::write_folders(conn, account, &stored, &folders)?;
        }
        conn.execute(
            "UPDATE local_sent SET folder = ?3 WHERE account_id = ?1 AND message_id = ?2",
            params![account.as_str(), id.as_str(), target.name],
        )?;
        if has_copy(conn, account, &id, &target.name)? {
            copy_arrived(conn, account, id.as_str(), &target.name)?;
        }
    }
    Ok(changed)
}
