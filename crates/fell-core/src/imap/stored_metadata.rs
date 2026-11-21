//! Fetch metadata for copies already recorded in the local mailbox.

use std::collections::{BTreeMap, HashMap};

use crate::backend::FetchedReplyTo;
use crate::{RemoteId, Result};

use super::{ImapConfig, parse_remote_id, rfc822, session, unescape_mailbox};

pub(crate) fn fetch_reply_to(
    config: &ImapConfig,
    wanted: &[RemoteId],
    persist: &mut dyn FnMut(&[FetchedReplyTo]) -> Result<()>,
) -> Result<()> {
    fetch_copy_metadata(
        config,
        wanted,
        session::FetchItems::ReplyTo,
        |remote_id, item| {
            Some(FetchedReplyTo {
                remote_id: remote_id.clone(),
                addresses: rfc822::reply_to_from_header(&item.body)?,
            })
        },
        |_, fetched| {
            if !fetched.is_empty() {
                persist(&fetched)?;
            }
            Ok(())
        },
    )
    .map(|_| ())
}

pub(crate) fn fetch_internal_dates(
    config: &ImapConfig,
    wanted: &[RemoteId],
    completed: &mut dyn FnMut(&RemoteId, Option<i64>),
) -> Result<()> {
    let complete = fetch_copy_metadata(
        config,
        wanted,
        session::FetchItems::InternalDate,
        |_, item| Some((item.uid, item.internal_date?)),
        |copies, fetched| {
            let dates: HashMap<_, _> = fetched.into_iter().collect();
            for (uid, id) in copies {
                completed(id, dates.get(uid).copied());
            }
            Ok(())
        },
    )?;
    if !complete {
        return Err(crate::Error::Backend(
            "IMAP INTERNALDATE fetch was truncated".into(),
        ));
    }
    Ok(())
}

fn fetch_copy_metadata<T>(
    config: &ImapConfig,
    wanted: &[RemoteId],
    items: session::FetchItems,
    parse: impl Fn(&RemoteId, session::Fetched) -> Option<T>,
    mut persist: impl FnMut(&BTreeMap<u32, &RemoteId>, Vec<T>) -> Result<()>,
) -> Result<bool> {
    let mut mailboxes = BTreeMap::<_, BTreeMap<_, _>>::new();
    for id in wanted {
        if let Some(copy) = parse_remote_id(&id.0)
            && let Some(mailbox) = unescape_mailbox(&copy.mailbox)
        {
            mailboxes
                .entry((mailbox, copy.uidvalidity))
                .or_default()
                .insert(copy.uid, id);
        }
    }
    if mailboxes.is_empty() {
        return Ok(true);
    }

    let mut session = session::Session::open(config)?;
    let mut complete = true;
    for ((mailbox, uidvalidity), copies) in mailboxes {
        let Some(examined) = session.examine(&mailbox)? else {
            continue;
        };
        if examined.uidvalidity != uidvalidity {
            continue;
        }
        let set = copies
            .keys()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let mut fetched = Vec::new();
        let stop = session.uid_fetch_each(&set, None, items, |item| {
            if let Some(&remote_id) = copies.get(&item.uid)
                && let Some(value) = parse(remote_id, item)
            {
                fetched.push(value);
            }
            Ok(())
        })?;
        match stop {
            session::FetchStop::Done => persist(&copies, fetched)?,
            session::FetchStop::Refused => {}
            session::FetchStop::Truncated { .. } => complete = false,
        }
    }
    session.logout();
    Ok(complete)
}
