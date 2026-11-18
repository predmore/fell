//! Fetch the bodies of copies already stored, to fill parts kept by later code.
//!
//! Mail synced before body parts were stored has only `body_text`. This reads
//! those copies again by UID with `BODY.PEEK[]`, so nothing is marked read.

use std::collections::BTreeMap;

use crate::Error;
use crate::backend::FetchedBody;
use crate::message::RemoteId;

use super::{ImapConfig, parse_remote_id, session, unescape_mailbox};

/// UIDs per `UID FETCH`, so one command line stays short.
const UIDS_PER_FETCH: usize = 100;

/// Fetch each copy in `wanted`. A copy the server answered for without it
/// (expunged, too large to fetch, or a new UIDVALIDITY) comes back with
/// `raw: None`. A copy whose mailbox or fetch the server refused is left out.
pub(crate) fn fetch_bodies(
    config: &ImapConfig,
    wanted: &[RemoteId],
) -> Result<Vec<FetchedBody>, Error> {
    let mut by_mailbox: BTreeMap<(String, u32), Vec<(u32, RemoteId)>> = BTreeMap::new();
    for remote_id in wanted {
        let Some(copy) = parse_remote_id(&remote_id.0) else {
            continue;
        };
        let Some(raw_name) = unescape_mailbox(&copy.mailbox) else {
            continue;
        };
        by_mailbox
            .entry((raw_name, copy.uidvalidity))
            .or_default()
            .push((copy.uid, remote_id.clone()));
    }
    if by_mailbox.is_empty() {
        return Ok(Vec::new());
    }
    let mut session = session::Session::open(config)?;
    let mut out = Vec::new();
    for ((mailbox, uidvalidity), mut copies) in by_mailbox {
        let Some(examined) = session.examine(&mailbox)? else {
            continue;
        };
        if examined.uidvalidity != uidvalidity {
            out.extend(copies.into_iter().map(|(_, remote_id)| FetchedBody {
                remote_id,
                raw: None,
            }));
            continue;
        }
        copies.sort_by_key(|(uid, _)| *uid);
        copies.dedup_by_key(|(uid, _)| *uid);
        for chunk in copies.chunks(UIDS_PER_FETCH) {
            let set = chunk
                .iter()
                .map(|(uid, _)| uid.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut answered: BTreeMap<u32, Option<Vec<u8>>> = BTreeMap::new();
            let stop = session.uid_fetch_each(&set, None, |item| {
                if item.body_missing {
                    return Ok(());
                }
                let raw = (!item.skipped).then_some(item.body);
                answered.insert(item.uid, raw);
                Ok(())
            })?;
            // Only a complete reply says a UID that did not come back is gone.
            let complete = matches!(stop, session::FetchStop::Done);
            for (uid, remote_id) in chunk {
                match answered.remove(uid) {
                    Some(raw) => out.push(FetchedBody {
                        remote_id: remote_id.clone(),
                        raw,
                    }),
                    None if complete => out.push(FetchedBody {
                        remote_id: remote_id.clone(),
                        raw: None,
                    }),
                    None => {}
                }
            }
        }
    }
    session.logout();
    Ok(out)
}
