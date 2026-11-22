use rusqlite::{OptionalExtension, params};

use crate::folders::{FolderRole, FolderRoles};
use crate::imap::{MailboxCheckpoint, cap_snippet, parse_remote_id};
use crate::{Account, Message, MessageId, Result, Store};

use super::split_field;

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
    pub fn is_arrival(&self, message: &Message) -> bool {
        message.unread
            && !message.archived
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
        let mut query = store.conn.prepare_cached(
            "SELECT COALESCE(NULLIF(trim(m.from_name), ''), m.from_email), m.subject,
                    m.snippet, substr(m.body_text, 1, 160), m.date_unix, m.folders
             FROM messages m
             WHERE m.account_id = ?1 AND m.message_id = ?2 AND m.unread = 1
               AND NOT EXISTS (
                   SELECT 1 FROM messages t JOIN message_copies c USING(account_id, message_id)
                   WHERE t.account_id = m.account_id AND t.thread_id = m.thread_id
                     AND instr(char(30) || lower(c.keywords) || char(30), char(30) || '$muted' || char(30)) > 0
               )
               AND NOT EXISTS (
                   SELECT 1 FROM message_copies c
                   WHERE c.account_id = m.account_id AND c.message_id = m.message_id
                     AND instr(char(30) || lower(c.keywords) || char(30), char(30) || '$junk' || char(30)) > 0
               )",
        )?;
        let mut arrivals = Vec::new();
        for id in ids {
            let row = query
                .query_row(params![self.account.id.as_str(), id.as_str()], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })
                .optional()?;
            if let Some((sender, subject, snippet, body, date_unix, folders)) = row
                && in_inbox(&roles, &folders)
            {
                let first_line = body
                    .as_deref()
                    .and_then(|body| body.lines().map(str::trim).find(|line| !line.is_empty()))
                    .unwrap_or(&snippet);
                arrivals.push(InboxArrival {
                    id: id.clone(),
                    sender,
                    subject,
                    snippet: cap_snippet(first_line).to_owned(),
                    date_unix,
                });
            }
        }
        Ok(arrivals)
    }
}

fn in_inbox(roles: &FolderRoles, encoded: &str) -> bool {
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
