//! Fetch metadata for copies already recorded in the local mailbox.

use std::collections::BTreeMap;

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
        persist,
    )
}

pub(crate) fn fetch_internal_dates(
    config: &ImapConfig,
    wanted: &[RemoteId],
) -> Result<Vec<(RemoteId, i64)>> {
    let mut dates = Vec::new();
    let latest = crate::latest_mail_date();
    fetch_copy_metadata(
        config,
        wanted,
        session::FetchItems::InternalDate,
        |remote_id, item| {
            let date = item
                .internal_date
                .filter(|date| (1..=latest).contains(date))?;
            Some((remote_id.clone(), date))
        },
        |fetched| {
            dates.extend_from_slice(fetched);
            Ok(())
        },
    )?;
    Ok(dates)
}

fn fetch_copy_metadata<T>(
    config: &ImapConfig,
    wanted: &[RemoteId],
    items: session::FetchItems,
    parse: impl Fn(&RemoteId, session::Fetched) -> Option<T>,
    mut persist: impl FnMut(&[T]) -> Result<()>,
) -> Result<()> {
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
        return Ok(());
    }

    let mut session = session::Session::open(config)?;
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
        if matches!(stop, session::FetchStop::Done) && !fetched.is_empty() {
            persist(&fetched)?;
        } else if items == session::FetchItems::InternalDate
            && !matches!(stop, session::FetchStop::Done)
        {
            return Err(crate::Error::Backend(
                "imap: incomplete INTERNALDATE fetch".into(),
            ));
        }
    }
    session.logout();
    Ok(())
}
