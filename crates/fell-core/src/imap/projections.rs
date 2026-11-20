use std::collections::HashMap;

use crate::Result;
use crate::backend::{CopyLink, FolderRemoval, MailboxView, MailboxViewKind, RemovalKind};
use crate::folders::FolderRole;

use super::{
    Copies, FlagMode, MailboxCheckpoint, escape_mailbox, flag_update, folder_name, rfc822, session,
};

pub(super) fn listed(mailboxes: &[session::Listed], bridge: bool) -> HashMap<String, MailboxView> {
    mailboxes
        .iter()
        .filter_map(|mailbox| {
            let kind = if marked(mailbox, FolderRole::All)
                || (bridge && named(mailbox, FolderRole::All))
            {
                MailboxViewKind::All
            } else if bridge
                && (marked(mailbox, FolderRole::Flagged) || named(mailbox, FolderRole::Flagged))
            {
                MailboxViewKind::Starred
            } else if bridge {
                let name = mailbox.decoded.strip_prefix("Labels/")?;
                if name.trim().is_empty() || name.chars().any(char::is_control) {
                    return None;
                }
                MailboxViewKind::Label(name.to_string())
            } else {
                return None;
            };
            Some((
                mailbox.raw.clone(),
                MailboxView {
                    mailbox: escape_mailbox(&mailbox.raw),
                    folder: mailbox.decoded.clone(),
                    kind,
                },
            ))
        })
        .collect()
}

fn named(mailbox: &session::Listed, role: FolderRole) -> bool {
    mailbox.decoded.eq_ignore_ascii_case(role.default_name())
}

fn marked(mailbox: &session::Listed, role: FolderRole) -> bool {
    mailbox
        .attributes
        .iter()
        .any(|attr| FolderRole::from_attribute(attr) == Some(role))
}

/// A Bridge mailbox may have disappeared before the first upgraded pass had
/// a chance to record it as a view. Its old cursor still names the folder.
pub(super) fn missing_legacy(
    mailboxes: &[session::Listed],
    prior: &[(String, MailboxCheckpoint)],
) -> Vec<FolderRemoval> {
    prior
        .iter()
        .filter_map(|(raw, checkpoint)| {
            if checkpoint.projected || mailboxes.iter().any(|mailbox| &mailbox.raw == raw) {
                return None;
            }
            let folder = folder_name(raw);
            if !folder.starts_with("Labels/")
                && ![FolderRole::All, FolderRole::Flagged]
                    .iter()
                    .any(|role| folder.eq_ignore_ascii_case(role.default_name()))
            {
                return None;
            }
            Some(FolderRemoval {
                mailbox: escape_mailbox(raw),
                folder,
                kind: RemovalKind::Listing {
                    uidvalidity: checkpoint.uidvalidity,
                    present: Vec::new(),
                    through: u32::MAX,
                    found: Vec::new(),
                    sweep: true,
                },
            })
        })
        .collect()
}

/// Read just enough of each virtual copy to associate it with a stored
/// message. The caller then asks which of those UIDs still need bodies.
pub(super) fn link(
    session: &mut session::Session,
    mailbox: &session::Listed,
    copies: &Copies<'_>,
    range: &str,
    mode: FlagMode<'_>,
    includes: impl Fn(u32) -> bool,
    persist: impl FnOnce(Vec<CopyLink>) -> Result<()>,
) -> Result<Option<Vec<u32>>> {
    let mut uids = Vec::new();
    let mut links = Vec::new();
    let stop = session.uid_fetch_each(range, None, session::FetchItems::MessageId, |item| {
        if !includes(item.uid) {
            return Ok(());
        }
        uids.push(item.uid);
        if !item.skipped && !item.body_missing && !rfc822::is_deleted(&item.flags) {
            links.push(CopyLink {
                message_id: rfc822::copy_message_id(
                    &mailbox.raw,
                    copies.uidvalidity,
                    item.uid,
                    &item.body,
                ),
                flags: flag_update(copies.uidvalidity, copies.mailbox, &item, mode),
            });
        }
        Ok(())
    })?;
    if !matches!(stop, session::FetchStop::Done) {
        return Ok(None);
    }
    if !links.is_empty() {
        persist(links)?;
    }
    uids.sort_unstable();
    uids.dedup();
    Ok(Some(uids))
}
