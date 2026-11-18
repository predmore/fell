//! Read-only generic IMAP. Proton Mail Bridge is one account of this kind.
//!
//! The session lists mailboxes, examines them, and fetches with `BODY.PEEK`
//! so the server is not asked to mark mail read. There is no SMTP, no OAuth,
//! and no send command. INBOX IDLE is read-only too.
//!
//! Each pass also reports server copies that left a mailbox: `VANISHED`
//! when QRESYNC is on, otherwise a paged `UID SEARCH` of the mailbox that
//! the store compares with the copies it recorded.

mod bodies;
mod part_fetch;
mod rfc822;
mod session;
mod utf7;

pub(crate) use session::{TlsStream, is_loopback_host as host_is_loopback, wrap_tcp_as_tls};

pub(crate) use bodies::fetch_bodies;
pub(crate) use part_fetch::fetch_part;
/// The parser sync stores fetched mail with, for tests that read back what
/// Fell sends.
#[cfg(test)]
pub(crate) use rfc822::to_message;
pub(crate) use rfc822::{attachment_names, has_attachment, is_bidi_control, parts_from_raw};

#[cfg(test)]
pub(crate) mod mime_fixtures;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod test_server;

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::Error;
use crate::account::AccountId;
use crate::backend::{
    FlagUpdate, FolderRemoval, KnownCopies, MailboxDelta, RemovalKind, Settle, SyncCursor,
};
use crate::folders::{FolderRole, FolderRoles, ListedMailbox};
use crate::message::{Message, RemoteId};

/// Messages held in memory before they are persisted. A larger mailbox is
/// written in several deltas so the cursor can move before the FETCH ends.
const BATCH_MESSAGES: usize = 32;
const BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Flag updates held before they are persisted.
const BATCH_FLAGS: usize = 5_000;
/// UIDs named in one `UID FETCH` of bodies, so the command line stays short.
const UIDS_PER_FETCH: usize = 250;

/// Messages per `UID SEARCH` page of a removal listing.
const LIST_PAGE: u32 = 5_000;
/// Messages per header page when a listing also learns Message-IDs.
const FOUND_PAGE: u32 = 500;

fn batch_message_limit() -> usize {
    #[cfg(test)]
    {
        TEST_BATCH_MESSAGES.with(|limit| limit.get())
    }
    #[cfg(not(test))]
    {
        BATCH_MESSAGES
    }
}

#[cfg(test)]
thread_local! {
    static TEST_BATCH_MESSAGES: std::cell::Cell<usize> = const { std::cell::Cell::new(BATCH_MESSAGES) };
}

#[cfg(test)]
pub(super) struct BatchLimitGuard;

#[cfg(test)]
pub(super) fn limit_fetch_batch(messages: usize) -> BatchLimitGuard {
    TEST_BATCH_MESSAGES.with(|cell| cell.set(messages));
    BatchLimitGuard
}

#[cfg(test)]
impl Drop for BatchLimitGuard {
    fn drop(&mut self) {
        TEST_BATCH_MESSAGES.with(|cell| cell.set(BATCH_MESSAGES));
    }
}

/// Log in, count selectable mailboxes, and log out. Reads no mail.
pub(crate) fn check_login(config: &ImapConfig) -> Result<usize, Error> {
    if config.username.is_empty() {
        return Err(Error::Backend("imap username is empty".into()));
    }
    let mut session = session::Session::open(config)?;
    let count = session.list_selectable()?.len();
    session.logout();
    Ok(count)
}

/// How [`wait_for_inbox`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InboxWait {
    /// INBOX changed since the cursor, or while waiting.
    Changed,
    /// `max` passed with no news.
    Quiet,
    /// The caller asked to stop.
    Stopped,
    /// The server has no IDLE.
    Unsupported,
}

/// Wait on INBOX with IDLE until it changes, `max` passes, or `stop` is true.
///
/// Read-only: `EXAMINE`, `IDLE`, `DONE`, `LOGOUT`. Mail that arrived after
/// the last sync (`cursor`) but before IDLE started counts as a change, so
/// nothing waits a whole interval. IDLE is renewed every 20 minutes,
/// under the 29 minutes RFC 2177 allows.
pub(crate) fn wait_for_inbox(
    config: &ImapConfig,
    cursor: Option<&SyncCursor>,
    max: std::time::Duration,
    stop: &mut dyn FnMut() -> bool,
) -> Result<InboxWait, Error> {
    const RENEW: std::time::Duration = std::time::Duration::from_secs(20 * 60);
    let mut session = session::Session::open(config)?;
    if !session.has_idle() {
        session.logout();
        return Ok(InboxWait::Unsupported);
    }
    let Some(examined) = session.examine("INBOX")? else {
        session.logout();
        // A refusal can be passing (a busy server). Only a missing IDLE
        // capability turns idling off for good.
        return Err(Error::Backend("imap: EXAMINE INBOX refused".into()));
    };
    let checkpoints = decode_cursor(cursor);
    let inbox = checkpoints
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("INBOX"))
        .map(|(_, checkpoint)| checkpoint);
    if inbox_moved_since(inbox, &examined) {
        session.logout();
        return Ok(InboxWait::Changed);
    }
    let started = std::time::Instant::now();
    let outcome = loop {
        let left = max.saturating_sub(started.elapsed());
        if left.is_zero() {
            break InboxWait::Quiet;
        }
        match session.idle(left.min(RENEW), stop)? {
            session::IdleEnd::Changed => break InboxWait::Changed,
            session::IdleEnd::Stopped => break InboxWait::Stopped,
            session::IdleEnd::Quiet => continue,
        }
    };
    session.logout();
    Ok(outcome)
}

/// INBOX has mail the last finished pass did not see.
///
/// Compares with the `UIDNEXT` that pass recorded, never with the highest
/// stored UID: UIDs have gaps (expunged mail, skipped oversized messages),
/// so that comparison would claim news forever and sync would never pause.
/// Without a recorded `UIDNEXT` this says no, and the interval catches up.
fn inbox_moved_since(checkpoint: Option<&MailboxCheckpoint>, examined: &session::Examined) -> bool {
    let Some(checkpoint) = checkpoint else {
        return false;
    };
    let Some(seen) = checkpoint.uidnext else {
        return false;
    };
    checkpoint.uidvalidity != examined.uidvalidity
        || examined.uidnext.is_some_and(|now| now != seen)
}

pub(crate) fn fetch_imap(
    account_id: &AccountId,
    config: &ImapConfig,
    cursor: Option<&SyncCursor>,
    known: &KnownCopies<'_>,
    mut persist: impl FnMut(MailboxDelta) -> Result<(), Error>,
) -> Result<(), Error> {
    if config.username.is_empty() {
        return Err(Error::Backend("imap username is empty".into()));
    }
    let mut session = session::Session::open(config)?;
    let mut mailboxes = session.list_selectable()?;
    mailboxes.sort_by(|left, right| {
        inbox_rank(&left.decoded)
            .cmp(&inbox_rank(&right.decoded))
            .then_with(|| left.decoded.cmp(&right.decoded))
            .then_with(|| left.raw.cmp(&right.raw))
    });
    // Roles first, so `gs` and archive find the right folder while the
    // first pass is still reading mail.
    persist(MailboxDelta {
        roles: Some(listed_roles(&mailboxes, &config.folder_overrides)),
        ..MailboxDelta::default()
    })?;
    let prior: HashMap<String, MailboxCheckpoint> = decode_cursor(cursor).into_iter().collect();
    let mut done: HashMap<String, MailboxCheckpoint> = HashMap::new();
    // Every mailbox was read to the end, so a copy moved on the server has
    // arrived wherever it went.
    let mut complete = true;
    for mailbox in &mailboxes {
        let previous = prior.get(&mailbox.raw);
        let Some(examined) = session.examine(&mailbox.raw)? else {
            complete = false;
            if let Some(prev) = previous {
                done.insert(mailbox.raw.clone(), prev.clone());
                emit_progress(
                    &mut persist,
                    &Snapshot {
                        account_id,
                        listed: &mailboxes,
                        prior: &prior,
                        done: &done,
                    },
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )?;
            }
            continue;
        };
        let plan = plan_fetch(previous, &examined);
        let previous_owned = previous.cloned();
        let same_validity = previous_owned
            .as_ref()
            .is_some_and(|prev| prev.uidvalidity == examined.uidvalidity);
        // High-water body fetches ignore UIDs already at the checkpoint.
        // CHANGEDSINCE results are kept so an older flag update is applied.
        let carried_pruned = previous_owned
            .as_ref()
            .filter(|_| same_validity)
            .and_then(|prev| prev.pruned);
        let seen_through = match (previous_owned.as_ref(), &plan) {
            (Some(prev), FetchPlan::HighWater { .. } | FetchPlan::Discover { .. })
                if same_validity =>
            {
                Some(prev.uid)
            }
            _ => None,
        };
        let uidvalidity = examined.uidvalidity;
        let uidnext = examined.uidnext;
        let highest_modseq = examined.highest_modseq;
        let exists = examined.exists;
        let escaped = escape_mailbox(&mailbox.raw);
        let mut batch = Vec::new();
        let mut batch_bytes = 0usize;
        // Flags of copies already stored, whose bodies were not fetched.
        let mut pending_flags = Vec::new();
        let mut highest: Option<u32> = None;
        // Stored copies, and `\Deleted` ones whose bodies are not fetched,
        // count toward the checkpoint only once the fetch is done: a body
        // still to come may have a lower UID.
        let mut stored_highest: Option<u32> = None;
        let mut min_unparsed: Option<u32> = None;
        let flush_batch = |batch: &mut Vec<Message>,
                           batch_bytes: &mut usize,
                           flags: &mut Vec<FlagUpdate>,
                           highest: Option<u32>,
                           persist: &mut dyn FnMut(MailboxDelta) -> Result<(), Error>,
                           done: &HashMap<String, MailboxCheckpoint>|
         -> Result<(), Error> {
            if batch.is_empty() && flags.is_empty() {
                return Ok(());
            }
            let uid = checkpoint_uid(previous_owned.as_ref(), same_validity, highest);
            let modseq = if same_validity {
                previous_owned.as_ref().and_then(|prev| prev.modseq)
            } else {
                None
            };
            let current = MailboxCheckpoint {
                uidvalidity,
                uid,
                modseq,
                // Mid-pass: the mailbox is not caught up yet.
                uidnext: None,
                pruned: carried_pruned,
            };
            emit_progress(
                persist,
                &Snapshot {
                    account_id,
                    listed: &mailboxes,
                    prior: &prior,
                    done,
                },
                Some((&mailbox.raw, &current)),
                std::mem::take(batch),
                std::mem::take(flags),
                Vec::new(),
            )?;
            *batch_bytes = 0;
            Ok(())
        };
        let copies = Copies {
            mailbox: &escaped,
            uidvalidity,
            known,
        };
        let stop = stream_bodies(&mut session, &plan, &copies, |streamed| {
            let item = match streamed {
                Streamed::Stored { uid, flags } => {
                    if seen_through.is_none_or(|floor| uid > floor) {
                        stored_highest = Some(stored_highest.map_or(uid, |top| top.max(uid)));
                    }
                    pending_flags.push(flag_update(uidvalidity, uid, &escaped, &flags));
                    if pending_flags.len() >= BATCH_FLAGS {
                        flush_batch(
                            &mut batch,
                            &mut batch_bytes,
                            &mut pending_flags,
                            capped_highest(highest, min_unparsed),
                            &mut persist,
                            &done,
                        )?;
                    }
                    return Ok(());
                }
                Streamed::Deleted { uid } => {
                    if seen_through.is_none_or(|floor| uid > floor) {
                        stored_highest = Some(stored_highest.map_or(uid, |top| top.max(uid)));
                    }
                    return Ok(());
                }
                Streamed::Fetched(item) => item,
            };
            let counts = seen_through.is_none_or(|floor| item.uid > floor);
            if counts && item.body_missing {
                min_unparsed = Some(min_unparsed.map_or(item.uid, |uid| uid.min(item.uid)));
            } else if counts {
                highest = Some(highest.map_or(item.uid, |current| current.max(item.uid)));
            }
            if !counts || item.skipped || item.body_missing || rfc822::is_deleted(&item.flags) {
                return Ok(());
            }
            let raw_len = item.body.len();
            batch.push(rfc822::to_message(
                account_id,
                &mailbox.raw,
                &mailbox.decoded,
                uidvalidity,
                item.uid,
                &item.flags,
                &item.body,
            ));
            batch_bytes += raw_len;
            if batch.len() >= batch_message_limit() || batch_bytes >= BATCH_BYTES {
                flush_batch(
                    &mut batch,
                    &mut batch_bytes,
                    &mut pending_flags,
                    capped_highest(highest, min_unparsed),
                    &mut persist,
                    &done,
                )?;
            }
            Ok(())
        })?;
        let (finished, min_skipped) = match stop {
            session::FetchStop::Done => {
                if let Some(top) = stored_highest {
                    highest = Some(highest.map_or(top, |current| current.max(top)));
                }
                (true, None)
            }
            session::FetchStop::Refused => {
                complete = false;
                if batch.is_empty() && highest.is_none() && pending_flags.is_empty() {
                    if let Some(prev) = previous_owned.clone() {
                        done.insert(mailbox.raw.clone(), prev);
                        emit_progress(
                            &mut persist,
                            &Snapshot {
                                account_id,
                                listed: &mailboxes,
                                prior: &prior,
                                done: &done,
                            },
                            None,
                            Vec::new(),
                            Vec::new(),
                            Vec::new(),
                        )?;
                    }
                    continue;
                }
                (false, None)
            }
            session::FetchStop::Truncated { min_skipped_uid } => (false, min_skipped_uid),
        };
        let mut flags = std::mem::take(&mut pending_flags);
        let mut flags_complete = true;
        let needs_baseline = finished
            && same_validity
            && previous_owned
                .as_ref()
                .is_some_and(|prev| prev.modseq.is_none() && prev.uid > 0)
            && highest_modseq.is_some()
            && !matches!(plan, FetchPlan::Full | FetchPlan::ChangedSince(_));
        if needs_baseline {
            match session.uid_flags("1:*") {
                Ok(rows) => {
                    for (uid, flag_names) in rows {
                        flags.push(flag_update(uidvalidity, uid, &escaped, &flag_names));
                    }
                }
                Err(err) if uid_fetch_refused(&err) || untagged_overflow(&err) => {
                    flags_complete = false;
                }
                Err(err) => return Err(err),
            }
        }
        let mut uid = if finished {
            match &plan {
                FetchPlan::Full | FetchPlan::HighWater { .. } => match uidnext {
                    Some(next) => highest.unwrap_or(0).max(next.saturating_sub(1)),
                    None => checkpoint_uid(previous_owned.as_ref(), same_validity, highest),
                },
                FetchPlan::Discover { .. } | FetchPlan::ChangedSince(_) | FetchPlan::Idle => {
                    checkpoint_uid(previous_owned.as_ref(), same_validity, highest)
                }
            }
        } else {
            let partial = match min_skipped {
                Some(skipped) => Some(skipped.saturating_sub(1)),
                None => highest,
            };
            checkpoint_uid(previous_owned.as_ref(), same_validity, partial)
        };
        if let Some(missing) = min_unparsed {
            uid = uid.min(missing.saturating_sub(1));
        }
        // Record HIGHESTMODSEQ when this pass saw every message's flags: a full
        // or CHANGEDSINCE fetch, an empty mailbox, a high-water pass from uid 0,
        // or a UID FLAGS baseline for older mail that had no modseq yet.
        let saw_every_flag = finished
            && flags_complete
            && min_unparsed.is_none()
            && (matches!(plan, FetchPlan::Full | FetchPlan::ChangedSince(_))
                || exists == 0
                || needs_baseline
                || (matches!(plan, FetchPlan::HighWater { start } if start <= 1)
                    && previous_owned.as_ref().map(|prev| prev.uid).unwrap_or(0) == 0));
        let modseq = if saw_every_flag {
            highest_modseq.or_else(|| previous_owned.as_ref().and_then(|prev| prev.modseq))
        } else if same_validity {
            previous_owned.as_ref().and_then(|prev| prev.modseq)
        } else {
            None
        };
        let clean = finished && min_unparsed.is_none();
        complete &= clean;
        let tracked = if clean {
            track_removals(
                &mut session,
                mailbox,
                Seen {
                    uidvalidity,
                    uidnext,
                    exists,
                },
                previous_owned.as_ref(),
                &plan,
            )?
        } else {
            Tracked {
                removals: Vec::new(),
                pruned: carried_pruned,
            }
        };
        let current = MailboxCheckpoint {
            uidvalidity,
            uid,
            modseq,
            // What a finished pass saw, so IDLE can tell later mail apart.
            uidnext: if finished { uidnext } else { None },
            pruned: tracked.pruned,
        };
        emit_progress(
            &mut persist,
            &Snapshot {
                account_id,
                listed: &mailboxes,
                prior: &prior,
                done: &done,
            },
            Some((&mailbox.raw, &current)),
            std::mem::take(&mut batch),
            flags,
            tracked.removals,
        )?;
        done.insert(mailbox.raw.clone(), current);
    }
    // Folders whose last copy vanished come off rows only now, after every
    // mailbox had its chance to add the copy a server move made.
    persist(MailboxDelta {
        settle: if complete {
            Settle::All
        } else {
            Settle::KeepRows
        },
        ..MailboxDelta::default()
    })?;
    session.logout();
    Ok(())
}

/// What EXAMINE reported for the mailbox being synced.
#[derive(Clone, Copy)]
struct Seen {
    uidvalidity: u32,
    uidnext: Option<u32>,
    exists: u32,
}

struct Tracked {
    removals: Vec<FolderRemoval>,
    /// For the checkpoint: `(EXISTS, UIDNEXT)` when removals were last fully
    /// reported, or the previous pair when this pass could not.
    pruned: Option<(u32, u32)>,
}

/// Report the copies that left this mailbox since the last pass.
///
/// With QRESYNC on, the CHANGEDSINCE fetch already carried `VANISHED
/// (EARLIER)`. That is enough when the UIDs it names account for the drop
/// in EXISTS; otherwise, or when a set used `*`, the mailbox is listed too.
/// A listing goes by sequence number in pages, and counts only when no
/// expunge arrived while it ran and it holds one ascending UID per message
/// EXAMINE counted; anything else waits for the next pass. When EXISTS and
/// UIDNEXT match the last full listing, nothing was added or removed, so no
/// listing is sent.
///
/// The first listing of a mailbox synced before (copies not yet recorded,
/// or a new UIDVALIDITY) also fetches Message-ID headers (`BODY.PEEK`). The
/// body fetch skips some mail (flagged `\Deleted`, too large), so only the
/// headers record every copy still there before old ones are judged gone.
fn track_removals(
    session: &mut session::Session,
    mailbox: &session::Listed,
    seen: Seen,
    checkpoint: Option<&MailboxCheckpoint>,
    plan: &FetchPlan,
) -> Result<Tracked, Error> {
    let escaped = escape_mailbox(&mailbox.raw);
    let removal = |kind| FolderRemoval {
        mailbox: escaped.clone(),
        folder: mailbox.decoded.clone(),
        kind,
    };
    let previous = checkpoint.filter(|prev| prev.uidvalidity == seen.uidvalidity);
    let mut removals = Vec::new();
    // `None`: a set used `*`. It is not applied, and cannot stand in for a
    // listing.
    let vanished = session.take_vanished();
    let reported = vanished.as_deref().map_or(0, |ranges| {
        vanished_below(ranges, seen.uidnext.unwrap_or(u32::MAX))
    });
    if let Some(ranges) = vanished
        && !ranges.is_empty()
    {
        removals.push(removal(RemovalKind::Vanished {
            uidvalidity: seen.uidvalidity,
            ranges,
        }));
    }
    let carried = previous.and_then(|prev| prev.pruned);
    let snapshot = (seen.exists, seen.uidnext.unwrap_or(0));
    if let Some((then_exists, then_next)) = carried
        && session.qresync()
        && matches!(plan, FetchPlan::ChangedSince(_))
        && vanished_accounts_for(then_exists, then_next, seen, reported)
    {
        return Ok(Tracked {
            removals,
            pruned: Some(snapshot),
        });
    }
    if session.saw_expunge() || (seen.uidnext.is_some() && carried == Some(snapshot)) {
        return Ok(Tracked {
            removals,
            pruned: carried,
        });
    }
    let discover = carried.is_none() && checkpoint.is_some();
    let Some((present, found)) = list_mailbox(session, mailbox, seen, discover)? else {
        return Ok(Tracked {
            removals,
            pruned: carried,
        });
    };
    // Every UID below UIDNEXT was assigned before EXAMINE, so a stored copy
    // at or below it that is not listed was expunged. Mail that arrived
    // later is not judged.
    let through = match seen.uidnext {
        Some(next) => next.saturating_sub(1),
        None => present
            .last()
            .copied()
            .unwrap_or(0)
            .max(previous.map_or(0, |prev| prev.uid)),
    };
    removals.push(removal(RemovalKind::Listing {
        uidvalidity: seen.uidvalidity,
        present,
        through,
        found,
        sweep: carried.is_none(),
    }));
    Ok(Tracked {
        removals,
        pruned: Some(snapshot),
    })
}

/// How many UIDs below `uidnext` the ranges name.
fn vanished_below(ranges: &[(u32, u32)], uidnext: u32) -> u64 {
    ranges
        .iter()
        .filter(|(low, _)| *low < uidnext)
        .map(|(low, high)| u64::from((*high).min(uidnext.saturating_sub(1)) - low) + 1)
        .sum()
}

/// The VANISHED UIDs explain every message that left since the last full
/// listing saw `then_exists` messages and UIDNEXT `then_next`.
///
/// Each UID assigned since then is either still here or expunged, so
/// `then_exists + (UIDNEXT - then_next) - EXISTS` messages left. A server
/// may name more (UIDs expunged earlier), never fewer. Without both UIDNEXT
/// values nothing can be checked, so the mailbox is listed.
fn vanished_accounts_for(then_exists: u32, then_next: u32, seen: Seen, reported: u64) -> bool {
    let Some(next) = seen.uidnext.filter(|_| then_next != 0) else {
        return false;
    };
    let assigned = u64::from(next.saturating_sub(then_next));
    let left = (u64::from(then_exists) + assigned).saturating_sub(u64::from(seen.exists));
    reported >= left
}

type Listing = (Vec<u32>, Vec<(u32, crate::MessageId)>);

/// Every UID in the mailbox, and with `discover` each copy's Message-ID.
/// `None` when the server refused, or the listing cannot be trusted.
fn list_mailbox(
    session: &mut session::Session,
    mailbox: &session::Listed,
    seen: Seen,
    discover: bool,
) -> Result<Option<Listing>, Error> {
    let page = if discover { FOUND_PAGE } else { LIST_PAGE };
    let mut present = Vec::new();
    let mut found = Vec::new();
    let mut first = 1u32;
    while first <= seen.exists {
        let last = first.saturating_add(page - 1).min(seen.exists);
        let listed = if discover {
            session.fetch_message_ids(first, last).map(|rows| {
                rows.map(|rows| {
                    let mut uids = Vec::with_capacity(rows.len());
                    for (uid, headers) in rows {
                        let id =
                            rfc822::copy_message_id(&mailbox.raw, seen.uidvalidity, uid, &headers);
                        found.push((uid, id));
                        uids.push(uid);
                    }
                    uids
                })
            })
        } else {
            session.uid_search_seq(first, last)
        };
        match listed {
            Ok(Some(mut uids)) => present.append(&mut uids),
            Ok(None) => return Ok(None),
            Err(err) if untagged_overflow(&err) => return Ok(None),
            Err(err) => return Err(err),
        }
        if last == seen.exists {
            break;
        }
        first = last + 1;
    }
    // Sequence numbers shift under an expunge, so a page could skip a UID
    // that is still there.
    let ordered = present.windows(2).all(|pair| pair[0] < pair[1]);
    if session.saw_expunge() || present.len() != seen.exists as usize || !ordered {
        return Ok(None);
    }
    Ok(Some((present, found)))
}

fn capped_highest(highest: Option<u32>, min_unparsed: Option<u32>) -> Option<u32> {
    match min_unparsed {
        Some(missing) => Some(
            highest
                .map(|uid| uid.min(missing.saturating_sub(1)))
                .unwrap_or(missing.saturating_sub(1)),
        ),
        None => highest,
    }
}

/// The copies of one mailbox the store already holds.
struct Copies<'a> {
    /// Escaped, as remote ids spell it.
    mailbox: &'a str,
    uidvalidity: u32,
    known: &'a KnownCopies<'a>,
}

impl Copies<'_> {
    fn stored(&self, low: u32, high: u32) -> Result<Vec<u32>, Error> {
        (self.known)(self.mailbox, self.uidvalidity, (low, high))
    }
}

/// One message a sync fetch reports.
enum Streamed {
    /// Not stored yet: fetched with its body.
    Fetched(session::Fetched),
    /// Stored already: only its flags were fetched.
    Stored { uid: u32, flags: Vec<String> },
    /// Not stored and flagged `\Deleted`: dropped either way, so its body
    /// was not fetched.
    Deleted { uid: u32 },
}

/// Report every message `plan` covers, fetching a body only for a UID the
/// store has no copy of.
///
/// A CONDSTORE pass asks for flags alone (`UID FLAGS`, `CHANGEDSINCE`), then
/// for the bodies of the UIDs it named that are not stored: new mail, or a
/// copy skipped before (flagged `\Deleted` then). A high-water or full pass
/// streams bodies in one command when nothing at or above its start is
/// stored, as on a first sync; otherwise it goes the same way, so a
/// checkpoint left low by an unreadable body never downloads known mail
/// again. A pass without UIDNEXT always does.
fn stream_bodies(
    session: &mut session::Session,
    plan: &FetchPlan,
    copies: &Copies<'_>,
    mut on_fetched: impl FnMut(Streamed) -> Result<(), Error>,
) -> Result<session::FetchStop, Error> {
    let start = match plan {
        FetchPlan::Idle => return Ok(session::FetchStop::Done),
        FetchPlan::ChangedSince(modseq) => {
            return flags_then_bodies(session, 1, Some(*modseq), copies, on_fetched);
        }
        FetchPlan::Discover { start } => {
            return flags_then_bodies(session, *start, None, copies, on_fetched);
        }
        FetchPlan::Full => 1,
        FetchPlan::HighWater { start } => *start,
    };
    if !copies.stored(start, u32::MAX)?.is_empty() {
        return flags_then_bodies(session, start, None, copies, on_fetched);
    }
    session.uid_fetch_each(
        &format!("{start}:*"),
        None,
        session::FetchItems::Bodies,
        |item| on_fetched(Streamed::Fetched(item)),
    )
}

/// `UID FETCH start:* (UID FLAGS)`, then `BODY.PEEK[]` by UID for what the
/// store does not hold. Stored copies are reported before any body.
///
/// A body fetch that stops early ends as `Truncated` at the lowest UID it
/// did not deliver, so the checkpoint stays below it.
fn flags_then_bodies(
    session: &mut session::Session,
    start: u32,
    changed_since: Option<u64>,
    copies: &Copies<'_>,
    mut on_fetched: impl FnMut(Streamed) -> Result<(), Error>,
) -> Result<session::FetchStop, Error> {
    let mut rows: Vec<(u32, Vec<String>)> = Vec::new();
    let listed = session.uid_fetch_each(
        &format!("{start}:*"),
        changed_since,
        session::FetchItems::Flags,
        |item| {
            // `n:*` names the highest UID even when it is below `n`.
            if item.uid >= start {
                rows.push((item.uid, item.flags));
            }
            Ok(())
        },
    )?;
    let cut = match listed {
        session::FetchStop::Done => None,
        session::FetchStop::Refused => return Ok(session::FetchStop::Refused),
        session::FetchStop::Truncated { min_skipped_uid } => Some(min_skipped_uid),
    };
    rows.sort_by_key(|(uid, _)| *uid);
    rows.dedup_by_key(|(uid, _)| *uid);
    let stored = match (rows.first(), rows.last()) {
        (Some((low, _)), Some((high, _))) => copies.stored(*low, *high)?,
        _ => Vec::new(),
    };
    let mut wanted = Vec::new();
    for (uid, flags) in rows {
        if stored.binary_search(&uid).is_ok() {
            on_fetched(Streamed::Stored { uid, flags })?;
        } else if rfc822::is_deleted(&flags) {
            // Not stored either way, so its body is not worth the bytes.
            on_fetched(Streamed::Deleted { uid })?;
        } else {
            wanted.push(uid);
        }
    }
    // A fetch can finish without a UID it was asked for (expunged since the
    // flags came, or a server that drops it): the checkpoint stays below the
    // lowest such UID, as when a fetch stops early.
    let mut first_missing: Option<u32> = None;
    for (index, chunk) in wanted.chunks(UIDS_PER_FETCH).enumerate() {
        let mut delivered = HashSet::with_capacity(chunk.len());
        let stop =
            session.uid_fetch_each(&uid_set(chunk), None, session::FetchItems::Bodies, |item| {
                delivered.insert(item.uid);
                on_fetched(Streamed::Fetched(item))
            })?;
        let missing = chunk.iter().find(|uid| !delivered.contains(uid)).copied();
        if !matches!(stop, session::FetchStop::Done) {
            let first_left = missing.or_else(|| wanted.get((index + 1) * UIDS_PER_FETCH).copied());
            return Ok(session::FetchStop::Truncated {
                min_skipped_uid: lowest([first_missing, first_left, cut.flatten()]),
            });
        }
        // Chunks ascend, so the first chunk missing a UID has the lowest.
        first_missing = first_missing.or(missing);
    }
    Ok(match (cut, first_missing) {
        (None, None) => session::FetchStop::Done,
        (cut, missing) => session::FetchStop::Truncated {
            min_skipped_uid: lowest([missing, cut.flatten()]),
        },
    })
}

fn lowest<const N: usize>(uids: [Option<u32>; N]) -> Option<u32> {
    uids.into_iter().flatten().min()
}

/// Ascending UIDs as an IMAP set, runs written `low:high`.
fn uid_set(uids: &[u32]) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < uids.len() {
        let low = uids[index];
        let mut high = low;
        while uids.get(index + 1) == Some(&high.saturating_add(1)) {
            index += 1;
            high = uids[index];
        }
        if !out.is_empty() {
            out.push(',');
        }
        if low == high {
            out.push_str(&low.to_string());
        } else {
            out.push_str(&format!("{low}:{high}"));
        }
        index += 1;
    }
    out
}

/// The unread state of one stored copy, from its flags.
fn flag_update(uidvalidity: u32, uid: u32, escaped: &str, flags: &[String]) -> FlagUpdate {
    FlagUpdate {
        remote_id: RemoteId(format!("imap:{uidvalidity}:{uid}:{escaped}")),
        unread: !flags.iter().any(|flag| flag.eq_ignore_ascii_case("\\Seen")),
    }
}

struct Snapshot<'a> {
    account_id: &'a AccountId,
    listed: &'a [session::Listed],
    prior: &'a HashMap<String, MailboxCheckpoint>,
    done: &'a HashMap<String, MailboxCheckpoint>,
}

fn emit_progress(
    persist: &mut dyn FnMut(MailboxDelta) -> Result<(), Error>,
    snapshot: &Snapshot<'_>,
    current: Option<(&str, &MailboxCheckpoint)>,
    messages: Vec<Message>,
    flags: Vec<FlagUpdate>,
    removals: Vec<FolderRemoval>,
) -> Result<(), Error> {
    let mut pairs = Vec::new();
    for mailbox in snapshot.listed {
        let checkpoint = if let Some((name, cp)) = current
            && name == mailbox.raw
        {
            Some(cp.clone())
        } else {
            snapshot
                .done
                .get(&mailbox.raw)
                .cloned()
                .or_else(|| snapshot.prior.get(&mailbox.raw).cloned())
        };
        if let Some(checkpoint) = checkpoint {
            pairs.push((mailbox.raw.clone(), checkpoint));
        }
    }
    pairs.sort_by(|left, right| left.0.cmp(&right.0));
    persist(MailboxDelta {
        messages,
        flags,
        cursor: Some(encode_cursor(snapshot.account_id.as_str(), &pairs)),
        removals,
        settle: Settle::Later,
        roles: None,
    })
}

/// Tagged NO/BAD on UID FETCH. The socket is still a valid IMAP session.
/// A dropped connection is not this error and must abort the sync.
fn uid_fetch_refused(err: &Error) -> bool {
    matches!(err, Error::Backend(message) if message.contains("UID FETCH failed:"))
}

/// The untagged-reply cap. The command was drained, so later mailboxes can run.
fn untagged_overflow(err: &Error) -> bool {
    matches!(err, Error::Backend(message) if message.contains("exceeded") && message.contains("untagged"))
}

fn listed_roles(listed: &[session::Listed], overrides: &[(FolderRole, String)]) -> FolderRoles {
    let mailboxes: Vec<ListedMailbox> = listed
        .iter()
        .map(|mailbox| ListedMailbox {
            raw: mailbox.raw.clone(),
            name: mailbox.decoded.clone(),
            delimiter: mailbox.delimiter,
            attributes: mailbox.attributes.clone(),
        })
        .collect();
    FolderRoles::detect(&mailboxes, overrides)
}

fn inbox_rank(name: &str) -> u8 {
    if name.eq_ignore_ascii_case("INBOX") {
        0
    } else {
        1
    }
}

#[derive(Clone, Copy)]
enum FetchPlan {
    /// Nothing new, or the mailbox is empty.
    Idle,
    /// Every message, including flags.
    Full,
    /// CONDSTORE flag and new-message pass.
    ChangedSince(u64),
    /// Body fetch from `start`. UIDNEXT is known, so the checkpoint can move
    /// to `UIDNEXT - 1` after this succeeds.
    HighWater { start: u32 },
    /// UIDNEXT was omitted. Learn UIDs without bodies, then fetch only new ones.
    Discover { start: u32 },
}

fn checkpoint_uid(
    previous: Option<&MailboxCheckpoint>,
    same_validity: bool,
    highest: Option<u32>,
) -> u32 {
    let prior = if same_validity {
        previous.map(|prev| prev.uid).unwrap_or(0)
    } else {
        0
    };
    prior.max(highest.unwrap_or(0))
}

fn plan_fetch(previous: Option<&MailboxCheckpoint>, examined: &session::Examined) -> FetchPlan {
    if examined.exists == 0 {
        return FetchPlan::Idle;
    }
    match previous {
        Some(prev) if prev.uidvalidity == examined.uidvalidity => {
            if let Some(modseq) = prev.modseq
                && examined.highest_modseq.is_some()
            {
                return FetchPlan::ChangedSince(modseq);
            }
            if prev.uid == u32::MAX
                || examined
                    .uidnext
                    .is_some_and(|next| next <= prev.uid.saturating_add(1))
            {
                return FetchPlan::Idle;
            }
            let start = prev.uid.saturating_add(1);
            if examined.uidnext.is_some() {
                FetchPlan::HighWater { start }
            } else {
                FetchPlan::Discover { start }
            }
        }
        _ => FetchPlan::Full,
    }
}

/// How the TCP session is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImapSecurity {
    /// Cleartext. Local tests, and a Bridge install set to plain IMAP.
    Plain,
    /// Upgrade with STARTTLS after the greeting. Bridge's usual local port.
    StartTls,
    /// Implicit TLS, typically port 993.
    Tls,
}

/// Connection settings for one IMAP account.
///
/// The password is redacted in [`Debug`] output.
#[derive(Clone, PartialEq, Eq)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub security: ImapSecurity,
    /// Bridge's local certificate is not a public CA. Leave this false for
    /// any host that is not the user's own machine.
    pub accept_invalid_certs: bool,
    /// Folders `account.toml` names for a role. They win over what the
    /// server marks.
    pub folder_overrides: Vec<(FolderRole, String)>,
}

impl fmt::Debug for ImapConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("security", &self.security)
            .field("accept_invalid_certs", &self.accept_invalid_certs)
            .field("folder_overrides", &self.folder_overrides)
            .finish()
    }
}

impl ImapConfig {
    /// Proton Mail Bridge defaults: local STARTTLS, certificate not public.
    pub fn bridge_local(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1143,
            username: username.into(),
            password: password.into(),
            security: ImapSecurity::StartTls,
            accept_invalid_certs: true,
            folder_overrides: Vec::new(),
        }
    }
}

/// Per-mailbox checkpoint. `uid` is the highest UID already stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailboxCheckpoint {
    pub uidvalidity: u32,
    pub uid: u32,
    pub modseq: Option<u64>,
    /// `UIDNEXT` when a pass last finished this mailbox. UIDs can have gaps,
    /// so `uid + 1` is not a stand-in. `None` for cursors from before this
    /// field and for passes that stopped early.
    pub uidnext: Option<u32>,
    /// `(EXISTS, UIDNEXT)` when a pass last reported every copy that left
    /// this mailbox. `None` until the first such pass. UIDNEXT is 0 when the
    /// server omitted it.
    pub pruned: Option<(u32, u32)>,
}

/// Decode a sync cursor written by [`encode_cursor`].
///
/// An empty token and any token this crate did not write are "no checkpoint",
/// so the next sync fetches every mailbox from UID 1.
pub fn decode_cursor(cursor: Option<&SyncCursor>) -> Vec<(String, MailboxCheckpoint)> {
    let Some(cursor) = cursor else {
        return Vec::new();
    };
    let mut lines = cursor.token.lines();
    if lines.next() != Some("fell-imap/1") {
        return Vec::new();
    }
    let mut out = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split('\t');
        let Some(name) = parts.next().and_then(unescape_mailbox) else {
            return Vec::new();
        };
        let Some(uidvalidity) = parts.next().and_then(|raw| raw.parse().ok()) else {
            return Vec::new();
        };
        let Some(uid) = parts.next().and_then(|raw| raw.parse().ok()) else {
            return Vec::new();
        };
        let modseq = match parts.next() {
            Some("-") => None,
            Some(raw) => match raw.parse().ok() {
                Some(value) => Some(value),
                None => return Vec::new(),
            },
            None => return Vec::new(),
        };
        // Optional, so cursors written before the field still decode.
        let uidnext = match parts.next() {
            None | Some("-") => None,
            Some(raw) => match raw.parse().ok() {
                Some(value) => Some(value),
                None => return Vec::new(),
            },
        };
        let pruned = match parts.next() {
            None | Some("-") => None,
            Some(raw) => match raw
                .split_once('/')
                .and_then(|(exists, next)| Some((exists.parse().ok()?, next.parse().ok()?)))
            {
                Some(pair) => Some(pair),
                None => return Vec::new(),
            },
        };
        if parts.next().is_some() {
            return Vec::new();
        }
        out.push((
            name,
            MailboxCheckpoint {
                uidvalidity,
                uid,
                modseq,
                uidnext,
                pruned,
            },
        ));
    }
    out
}

pub fn encode_cursor(account_key: &str, mailboxes: &[(String, MailboxCheckpoint)]) -> SyncCursor {
    let mut token = String::from("fell-imap/1");
    for (name, checkpoint) in mailboxes {
        let modseq = checkpoint
            .modseq
            .map(|value| value.to_string())
            .unwrap_or_else(|| "-".into());
        token.push('\n');
        token.push_str(&escape_mailbox(name));
        token.push('\t');
        token.push_str(&checkpoint.uidvalidity.to_string());
        token.push('\t');
        token.push_str(&checkpoint.uid.to_string());
        token.push('\t');
        token.push_str(&modseq);
        token.push('\t');
        token.push_str(
            &checkpoint
                .uidnext
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".into()),
        );
        token.push('\t');
        token.push_str(
            &checkpoint
                .pruned
                .map(|(exists, next)| format!("{exists}/{next}"))
                .unwrap_or_else(|| "-".into()),
        );
    }
    SyncCursor {
        account_key: account_key.to_string(),
        token,
    }
}

/// One server copy, as an IMAP remote id names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteCopy {
    /// Escaped like the remote id, so it compares with [`FolderRemoval::mailbox`].
    ///
    /// [`FolderRemoval::mailbox`]: crate::backend::FolderRemoval::mailbox
    pub mailbox: String,
    pub uidvalidity: u32,
    pub uid: u32,
    /// The folder name this copy adds to a row.
    pub folder: String,
}

/// Parse `imap:{uidvalidity}:{uid}:{mailbox}`. Any other id is not a copy.
pub(crate) fn parse_remote_id(raw: &str) -> Option<RemoteCopy> {
    let rest = raw.strip_prefix("imap:")?;
    let (uidvalidity, rest) = rest.split_once(':')?;
    let (uid, mailbox) = rest.split_once(':')?;
    let name = unescape_mailbox(mailbox)?;
    Some(RemoteCopy {
        mailbox: mailbox.to_string(),
        uidvalidity: uidvalidity.parse().ok()?,
        uid: uid.parse().ok()?,
        folder: utf7::decode_modified_utf7(&name),
    })
}

pub(super) fn escape_mailbox(name: &str) -> String {
    name.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

fn unescape_mailbox(raw: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = raw.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next()? {
            '\\' => out.push('\\'),
            't' => out.push('\t'),
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_is_redacted_in_debug() {
        let config = ImapConfig::bridge_local("caleb", "secret-bridge");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("secret-bridge"));
        assert!(rendered.contains("<redacted>"));
        assert!(rendered.contains("1143"));
    }

    #[test]
    fn cursor_round_trips_mailbox_names() {
        let mailboxes = vec![
            (
                "INBOX".into(),
                MailboxCheckpoint {
                    uidvalidity: 9,
                    uid: 40,
                    modseq: Some(12),
                    uidnext: None,
                    pruned: None,
                },
            ),
            (
                "Sent\tItems".into(),
                MailboxCheckpoint {
                    uidvalidity: 3,
                    uid: 0,
                    modseq: None,
                    uidnext: None,
                    pruned: None,
                },
            ),
        ];
        let cursor = encode_cursor("acct", &mailboxes);
        assert_eq!(cursor.account_key, "acct");
        assert_eq!(decode_cursor(Some(&cursor)), mailboxes);
    }

    #[test]
    fn uidnext_round_trips_and_older_cursors_still_decode() {
        let mailboxes = vec![(
            "INBOX".to_string(),
            MailboxCheckpoint {
                uidvalidity: 9,
                uid: 40,
                modseq: None,
                uidnext: Some(77),
                pruned: Some((12, 77)),
            },
        )];
        let cursor = encode_cursor("acct", &mailboxes);
        assert_eq!(decode_cursor(Some(&cursor)), mailboxes);
        let older = SyncCursor {
            account_key: "acct".into(),
            token: "fell-imap/1\nINBOX\t9\t40\t12".into(),
        };
        let decoded = decode_cursor(Some(&older));
        assert_eq!(decoded.len(), 1);
        assert_eq!(decoded[0].1.uid, 40);
        assert_eq!(decoded[0].1.uidnext, None);
    }

    #[test]
    fn uid_sets_join_runs() {
        assert_eq!(uid_set(&[3]), "3");
        assert_eq!(uid_set(&[1, 2, 3, 7, 9, 10]), "1:3,7,9:10");
        assert_eq!(
            uid_set(&[u32::MAX - 1, u32::MAX]),
            format!("{}:{}", u32::MAX - 1, u32::MAX)
        );
        assert_eq!(uid_set(&[]), "");
    }

    #[test]
    fn unknown_cursor_starts_over() {
        let cursor = SyncCursor {
            account_key: "acct".into(),
            token: "not-ours".into(),
        };
        assert!(decode_cursor(Some(&cursor)).is_empty());
        assert!(decode_cursor(None).is_empty());
    }
}
