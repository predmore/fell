use rusqlite::{OptionalExtension, params};

use crate::folders::{FolderRole, FolderRoles};
use crate::imap::{MailboxCheckpoint, parse_remote_id};
use crate::{Account, AccountId, Message, MessageId, Result, Store};

use super::split_field;

const ELIGIBLE_MESSAGE: &str =
    "SELECT m.folders, COALESCE(NULLIF(trim(m.from_name), ''), m.from_email),
            m.subject, m.snippet, m.date_unix
     FROM messages m
     WHERE m.account_id = ?1 AND m.message_id = ?2 AND m.unread = 1 AND m.archived = 0
       AND NOT EXISTS (
           SELECT 1 FROM thread_folders tf
           WHERE tf.account_id = m.account_id AND tf.thread_id = m.thread_id
             AND tf.folder_key IN (char(31) || 'snoozed', char(31) || 'muted')
       )
       AND NOT EXISTS (
           SELECT 1 FROM messages t JOIN message_copies c USING(account_id, message_id)
           WHERE t.account_id = m.account_id AND t.thread_id = m.thread_id
             AND instr(char(30) || lower(c.keywords) || char(30), char(30) || '$muted' || char(30)) > 0
       )
       AND NOT EXISTS (
           SELECT 1 FROM message_copies c
           WHERE c.account_id = m.account_id AND c.message_id = m.message_id
             AND instr(char(30) || lower(c.keywords) || char(30), char(30) || '$junk' || char(30)) > 0
       )";

/// A distinct unread message just committed to the Inbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxArrival {
    pub id: MessageId,
    pub sender: String,
    pub subject: String,
    pub snippet: String,
    pub date_unix: i64,
}

pub(crate) struct Observer<'a> {
    pub account: &'a Account,
    pub checkpoint: MailboxCheckpoint,
    pub send: &'a mut dyn FnMut(Result<Vec<InboxArrival>>),
}

impl Observer<'_> {
    pub fn is_candidate(&self, message: &Message) -> bool {
        // A snooze wake can mark a reply unread before the commit.
        !message.archived
            && !message
                .headers
                .from
                .email
                .eq_ignore_ascii_case(&self.account.email)
            && parse_remote_id(&message.remote_id.0).is_some_and(|copy| {
                copy.folder
                    .eq_ignore_ascii_case(FolderRole::Inbox.default_name())
                    && copy.uidvalidity == self.checkpoint.uidvalidity
                    && copy.uid > self.checkpoint.uid
            })
    }

    pub fn committed(&mut self, store: &Store, ids: &[MessageId]) {
        if !ids.is_empty() {
            let arrivals = self.read_arrivals(store, ids);
            (self.send)(arrivals);
        }
    }

    fn read_arrivals(&self, store: &Store, ids: &[MessageId]) -> Result<Vec<InboxArrival>> {
        let roles = store.folder_roles(&self.account.id)?;
        let mut query = store.conn.prepare_cached(ELIGIBLE_MESSAGE)?;
        let mut arrivals = Vec::new();
        for id in ids {
            let row = query
                .query_row(params![self.account.id.as_str(), id.as_str()], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                })
                .optional()?;
            if let Some((folders, sender, subject, snippet, date_unix)) = row
                && in_inbox(&roles, &folders)
            {
                arrivals.push(InboxArrival {
                    id: id.clone(),
                    sender,
                    subject,
                    snippet,
                    date_unix,
                });
            }
        }
        Ok(arrivals)
    }
}

impl Store {
    /// Recheck the exact arrivals an alert represents, even if threading
    /// changed or a message ID collapsed into another copy since delivery.
    pub fn unread_inbox_messages(
        &self,
        account: &AccountId,
        ids: &[MessageId],
    ) -> Result<Vec<MessageId>> {
        let roles = self.folder_roles(account)?;
        let mut query = self.conn.prepare_cached(ELIGIBLE_MESSAGE)?;
        let mut unread = Vec::new();
        for id in ids {
            if let Some(canonical) = self.canonical_id(account, id)?
                && let Some(folders) = query
                    .query_row(params![account.as_str(), canonical], |row| {
                        row.get::<_, String>(0)
                    })
                    .optional()?
                && in_inbox(&roles, &folders)
            {
                unread.push(id.clone());
            }
        }
        Ok(unread)
    }
}

pub(super) fn in_inbox(roles: &FolderRoles, encoded: &str) -> bool {
    let mut inbox = false;
    for folder in split_field(encoded) {
        match roles.role_of(&folder) {
            Some(FolderRole::Inbox) => inbox = true,
            Some(FolderRole::Junk | FolderRole::Trash | FolderRole::Sent) => return false,
            _ => {}
        }
    }
    inbox
}
