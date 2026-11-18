//! Fetch the bodies of copies already stored, to fill parts kept by later code.
//!
//! Mail synced before body parts were stored has only `body_text`. This reads
//! those copies again by UID with `BODY.PEEK[]`, so nothing is marked read.

use std::collections::BTreeMap;

use crate::Error;
use crate::backend::FetchedBody;
use crate::message::RemoteId;

use super::{ImapConfig, parse_remote_id, session, unescape_mailbox};

/// UIDs per `UID FETCH`. Bytes past the cap in one reply are read and
/// dropped, so this also bounds what a full batch fetches for nothing.
const UIDS_PER_FETCH: usize = 32;

/// Bytes of message source one call holds, like sync's batch. Once reached,
/// no further `UID FETCH` is sent and the copies not fetched are left out.
fn byte_cap() -> usize {
    #[cfg(test)]
    {
        TEST_BYTE_CAP.with(|cap| cap.get())
    }
    #[cfg(not(test))]
    {
        super::BATCH_BYTES
    }
}

#[cfg(test)]
thread_local! {
    static TEST_BYTE_CAP: std::cell::Cell<usize> = const { std::cell::Cell::new(super::BATCH_BYTES) };
}

#[cfg(test)]
pub(super) struct ByteCapGuard;

#[cfg(test)]
pub(super) fn limit_body_bytes(bytes: usize) -> ByteCapGuard {
    TEST_BYTE_CAP.with(|cell| cell.set(bytes));
    ByteCapGuard
}

#[cfg(test)]
impl Drop for ByteCapGuard {
    fn drop(&mut self) {
        TEST_BYTE_CAP.with(|cell| cell.set(super::BATCH_BYTES));
    }
}

/// Fetch each copy in `wanted`. A copy the server answered for without it
/// (expunged, too large to fetch, a refused mailbox, or a new UIDVALIDITY)
/// comes back with `raw: None`. A copy not fetched because the call reached
/// its byte cap is left out, to be asked again.
///
/// A `UID FETCH` the server refused or cut short is an error, and nothing is
/// returned: a UID missing from such a reply may still be on the server.
pub(crate) fn fetch_bodies(
    config: &ImapConfig,
    wanted: &[RemoteId],
) -> Result<Vec<FetchedBody>, Error> {
    let mut by_mailbox: BTreeMap<(String, u32), Vec<(u32, RemoteId)>> = BTreeMap::new();
    let mut out = Vec::new();
    for remote_id in wanted {
        let named = parse_remote_id(&remote_id.0)
            .and_then(|copy| Some((unescape_mailbox(&copy.mailbox)?, copy.uidvalidity, copy.uid)));
        let Some((raw_name, uidvalidity, uid)) = named else {
            // No mailbox to ask: as good as gone.
            out.push(FetchedBody {
                remote_id: remote_id.clone(),
                raw: None,
            });
            continue;
        };
        by_mailbox
            .entry((raw_name, uidvalidity))
            .or_default()
            .push((uid, remote_id.clone()));
    }
    if by_mailbox.is_empty() {
        return Ok(out);
    }
    let cap = byte_cap();
    let mut session = session::Session::open(config)?;
    let mut held = 0usize;
    'mailboxes: for ((mailbox, uidvalidity), mut copies) in by_mailbox {
        if held >= cap {
            break;
        }
        match session.examine(&mailbox)? {
            Some(examined) if examined.uidvalidity == uidvalidity => {}
            _ => {
                out.extend(copies.into_iter().map(|(_, remote_id)| FetchedBody {
                    remote_id,
                    raw: None,
                }));
                continue;
            }
        }
        copies.sort_by_key(|(uid, _)| *uid);
        copies.dedup_by_key(|(uid, _)| *uid);
        for chunk in copies.chunks(UIDS_PER_FETCH) {
            if held >= cap {
                break 'mailboxes;
            }
            let set = chunk
                .iter()
                .map(|(uid, _)| uid.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut answered: BTreeMap<u32, Option<Vec<u8>>> = BTreeMap::new();
            // Answered past the cap: read off the wire, dropped, asked again.
            let mut deferred: Vec<u32> = Vec::new();
            let stop = session.uid_fetch_each(&set, None, |item| {
                if item.body_missing {
                    return Ok(());
                }
                let raw = (!item.skipped).then_some(item.body);
                let size = raw.as_ref().map_or(0, Vec::len);
                if size > 0 && held > 0 && held + size > cap {
                    deferred.push(item.uid);
                    return Ok(());
                }
                held += size;
                answered.insert(item.uid, raw);
                Ok(())
            })?;
            if !matches!(stop, session::FetchStop::Done) {
                let missing = chunk
                    .iter()
                    .any(|(uid, _)| !answered.contains_key(uid) && !deferred.contains(uid));
                if missing {
                    session.logout();
                    let why = match stop {
                        session::FetchStop::Refused => "refused",
                        _ => "cut short",
                    };
                    return Err(Error::Backend(format!(
                        "imap: UID FETCH of stored copies in {mailbox} {why}"
                    )));
                }
            }
            for (uid, remote_id) in chunk {
                if deferred.contains(uid) {
                    continue;
                }
                out.push(FetchedBody {
                    remote_id: remote_id.clone(),
                    // Not in a finished reply: the server no longer has it.
                    raw: answered.remove(uid).flatten(),
                });
            }
        }
    }
    session.logout();
    Ok(out)
}
