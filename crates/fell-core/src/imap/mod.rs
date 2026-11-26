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
pub(crate) mod part_fetch;
mod projections;
pub(crate) mod rfc822;
mod session;
mod stored_metadata;
mod structure;
mod utf7;
mod write_back;

pub(crate) use session::{TlsStream, is_loopback_host as host_is_loopback, wrap_tcp_as_tls};

pub use bodies::CancelBodyFetch;
pub(crate) use bodies::{BodyConnection, cancelled as body_fetch_cancelled, fetch_bodies};
pub(crate) use part_fetch::{
    CANCELLED as PART_FETCH_CANCELLED, fetch_part, fetch_provisional_part,
};
/// The parser sync stores fetched mail with, for tests that read back what
/// Fell sends.
#[cfg(test)]
pub(crate) use rfc822::to_message;
pub(crate) use rfc822::{
    CidReferences, SNIPPET_BYTES, attachment_names, cap_snippet, has_attachment,
    html_only_snippet_text, html_snippet_text, is_bidi_control, parts_from_raw,
    parts_from_trusted_raw, snippet as plain_snippet,
};
pub(crate) use stored_metadata::{fetch_internal_dates, fetch_reply_to};
pub use structure::BodyStructure;

#[cfg(test)]
mod bridge_sync_tests;
#[cfg(test)]
mod first_sync_tests;
#[cfg(test)]
mod keyword_sync_tests;
#[cfg(test)]
pub(crate) mod mime_fixtures;
#[cfg(test)]
mod sync_tests;
#[cfg(test)]
mod test_server;
#[cfg(test)]
mod write_back_tests;

use std::collections::{HashMap, HashSet};
use std::fmt;

use crate::Error;
use crate::account::AccountId;
use crate::backend::{
    CopyLink, FlagUpdate, FolderRemoval, KnownCopies, MailboxDelta, MailboxView, MailboxViewKind,
    RemovalKind, Settle, SyncCursor, SyncProgress,
};
use crate::folders::{FolderRole, FolderRoles, ListedMailbox};
use crate::message::RemoteId;

/// Messages held in memory before they are persisted. A larger mailbox is
/// written in several deltas so the cursor can move before the FETCH ends.
const BATCH_MESSAGES: usize = 32;
const BATCH_BYTES: usize = 8 * 1024 * 1024;
/// Flag updates held before they are persisted.
const BATCH_FLAGS: usize = 5_000;
/// UIDs named in one `UID FETCH` of bodies, so the command line stays short.
const UIDS_PER_FETCH: usize = 250;

/// How a first sync reads a mailbox newest first. See [`walk_down`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Windows {
    /// Messages in a mailbox's first window, so its newest rows show within
    /// moments of login.
    pub first: u32,
    /// Messages in each window after it.
    pub window: u32,
    /// How much of a folder one visit reads before the pass turns to the
    /// next: the newest Inbox mail, then the newest Sent, then the newest
    /// of each other folder. See [`visit_order`].
    pub inbox_head: u32,
    pub sent_head: u32,
    pub other_head: u32,
}

const WINDOWS: Windows = Windows {
    first: 100,
    window: 500,
    inbox_head: 2_000,
    sent_head: 1_000,
    other_head: 1_000,
};
/// Bodies one `UID FETCH` of a window asks for, newest first.
const BODIES_PER_FETCH: usize = 50;
/// Header-only rows per delta.
const HEADER_ROWS: usize = 100;

fn windows() -> Windows {
    #[cfg(test)]
    {
        TEST_WINDOWS.with(|sizes| sizes.get())
    }
    #[cfg(not(test))]
    {
        WINDOWS
    }
}

#[cfg(test)]
thread_local! {
    static TEST_WINDOWS: std::cell::Cell<Windows> = const { std::cell::Cell::new(WINDOWS) };
}

#[cfg(test)]
pub(super) struct WindowsGuard;

#[cfg(test)]
pub(super) fn set_windows(sizes: Windows) -> WindowsGuard {
    TEST_WINDOWS.with(|cell| cell.set(sizes));
    WindowsGuard
}

#[cfg(test)]
impl Drop for WindowsGuard {
    fn drop(&mut self) {
        TEST_WINDOWS.with(|cell| cell.set(WINDOWS));
    }
}

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

/// Send the account's queued writes ([`crate::outbox`]) on
/// a separate `SELECT` session.
#[cfg(test)]
pub(crate) fn replay_outbox(
    config: &ImapConfig,
    account_id: &AccountId,
    store: &crate::store::Store,
    write_back: bool,
    append_sent: bool,
) -> Result<crate::outbox::ReplayReport, Error> {
    write_back::replay_with(config, account_id, store, write_back, append_sent, &|| {})
}

pub(crate) fn replay_account_outbox(
    config: &crate::AccountConfig,
    account_id: &AccountId,
    store: &crate::Store,
) -> Result<crate::ReplayReport, Error> {
    write_back::replay_bound(
        &config.imap_config()?,
        account_id,
        store,
        config.write_back,
        config.append_sent(),
        &|| {},
        Some(config),
    )
}

pub(crate) use write_back::requeue_held_writes;

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
    keywords: bool,
    persist: impl FnMut(MailboxDelta) -> Result<(), Error>,
) -> Result<(), Error> {
    if config.username.is_empty() {
        return Err(Error::Backend("imap username is empty".into()));
    }
    let session = session::Session::open(config)?;
    fetch_on(
        session, account_id, config, cursor, known, keywords, persist,
    )
}

pub(crate) struct BoundRead {
    session: session::Session,
    config: ImapConfig,
}

impl BoundRead {
    pub(crate) fn open(
        config: &crate::AccountConfig,
        imap: ImapConfig,
        store: &crate::Store,
    ) -> crate::Result<Self> {
        let session = session::Session::open(&imap)?;
        store.observe_imap_server(config)?;
        Ok(Self {
            session,
            config: imap,
        })
    }

    pub(crate) fn fetch(
        self,
        account: &crate::Account,
        cursor: Option<&SyncCursor>,
        known: &KnownCopies<'_>,
        persist: impl FnMut(MailboxDelta) -> crate::Result<()>,
    ) -> crate::Result<()> {
        fetch_on(
            self.session,
            &account.id,
            &self.config,
            cursor,
            known,
            account.backend_kind == crate::BackendKind::Imap,
            persist,
        )
    }
}

fn fetch_on(
    mut session: session::Session,
    account_id: &AccountId,
    config: &ImapConfig,
    cursor: Option<&SyncCursor>,
    known: &KnownCopies<'_>,
    keywords: bool,
    mut persist: impl FnMut(MailboxDelta) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut mailboxes = session.list_selectable()?;
    mailboxes.sort_by(|left, right| {
        left.decoded
            .cmp(&right.decoded)
            .then_with(|| left.raw.cmp(&right.raw))
    });
    let views = projections::listed(&mailboxes, !keywords);
    let roles = listed_roles(&mailboxes, &config.folder_overrides, !keywords, &views);
    let prior = decode_cursor(cursor);
    // Roles first, so `gs` and archive find the right folder while the
    // first pass is still reading mail.
    persist(MailboxDelta {
        roles: Some(roles.clone()),
        uidplus: Some(session.has_cap("UIDPLUS")),
        views: Some(views.values().cloned().collect()),
        removals: if keywords {
            Vec::new()
        } else {
            projections::missing_legacy(&mailboxes, &prior)
        },
        ..MailboxDelta::default()
    })?;
    let mut pass = Pass {
        account_id,
        listed: &mailboxes,
        known,
        keywords,
        prior: prior.into_iter().collect(),
        done: HashMap::new(),
        // Every mailbox was read to the end, so a copy moved on the server
        // has arrived wherever it went.
        complete: true,
        caught_up: HashMap::new(),
    };
    let persist: &mut dyn FnMut(MailboxDelta) -> Result<(), Error> = &mut persist;
    let mut visited: HashSet<usize> = HashSet::new();
    // Mailboxes whose first sync has older mail left to read.
    let mut walking: HashSet<usize> = HashSet::new();
    for (index, reach) in visit_order(&mailboxes, &roles, &views) {
        let mailbox = &mailboxes[index];
        let view = views.get(&mailbox.raw);
        let flags = FlagMode {
            keywords,
            view: view.map(|view| &view.kind),
        };
        let first = visited.insert(index);
        if !first && !walking.contains(&index) {
            continue;
        }
        let Some(examined) = session.examine(&mailbox.raw)? else {
            pass.complete = false;
            walking.remove(&index);
            if first && let Some(prev) = pass.prior.get(&mailbox.raw).cloned() {
                pass.done.insert(mailbox.raw.clone(), prev);
                emit_progress(
                    persist,
                    &pass.snapshot(),
                    None,
                    Vec::new(),
                    Vec::new(),
                    Vec::new(),
                )?;
            }
            continue;
        };
        // A mailbox whose UIDVALIDITY changed since an earlier visit of
        // this pass holds other mail under the same UIDs: it starts over,
        // as on a first visit, instead of walking on below the old floor.
        let renumbered = !first
            && pass
                .done
                .get(&mailbox.raw)
                .is_some_and(|checkpoint| checkpoint.uidvalidity != examined.uidvalidity);
        if (first || renumbered)
            && !catch_up(&mut pass, &mut session, mailbox, &examined, flags, persist)?
        {
            walking.remove(&index);
            continue;
        }
        let folder = match flags.view {
            Some(MailboxViewKind::Label(label)) => label.clone(),
            _ => roles.label(&mailbox.decoded),
        };
        let walked = walk_down(
            &mut pass,
            &mut session,
            Walking {
                mailbox,
                folder: &folder,
                examined: &examined,
                reach,
                flags,
            },
            persist,
        )?;
        if walked == Walked::Paused {
            walking.insert(index);
        } else {
            walking.remove(&index);
        }
    }
    if !walking.is_empty() {
        pass.complete = false;
    }
    // Folders whose last copy vanished come off rows only now, after every
    // mailbox had its chance to add the copy a server move made.
    persist(MailboxDelta {
        settle: if pass.complete {
            Settle::All
        } else {
            Settle::KeepRows
        },
        ..MailboxDelta::default()
    })?;
    session.logout();
    Ok(())
}

/// What one sync pass has learned so far.
struct Pass<'a> {
    account_id: &'a AccountId,
    listed: &'a [session::Listed],
    known: &'a KnownCopies<'a>,
    keywords: bool,
    /// The checkpoints the pass started from.
    prior: HashMap<String, MailboxCheckpoint>,
    /// Checkpoints this pass wrote.
    done: HashMap<String, MailboxCheckpoint>,
    /// Every mailbox was read to the end.
    complete: bool,
    /// UIDNEXT when this pass caught a mailbox up: all mail below it is
    /// read, or left to the walk of its first sync.
    caught_up: HashMap<String, u32>,
}

impl Pass<'_> {
    fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            account_id: self.account_id,
            listed: self.listed,
            prior: &self.prior,
            done: &self.done,
        }
    }
}

/// How much of a mailbox one visit of a first sync reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// About this many more messages, then on to the next mailbox.
    Head(u32),
    /// To the oldest message.
    All,
}

/// The order a pass visits mailboxes in. A mailbox caught up already is
/// synced on its first visit and skipped after; one still on its first
/// sync is read newest first a stretch at a time: the newest Inbox mail,
/// then the newest Sent (so replies thread), then the rest of the Inbox,
/// then the newest of every other folder, then everything older.
fn visit_order(
    listed: &[session::Listed],
    roles: &FolderRoles,
    views: &HashMap<String, MailboxView>,
) -> Vec<(usize, Reach)> {
    let sizes = windows();
    // A server that marks some folders but not Sent still has one: the
    // name table's, so replies thread as early as with a marked one.
    let sent_by_name = if roles.get(FolderRole::Sent).is_none() {
        crate::folders::named(&listed_mailboxes(listed), FolderRole::Sent)
            .map(|mailbox| mailbox.raw)
    } else {
        None
    };
    let rank = |mailbox: &session::Listed| {
        if mailbox.decoded.eq_ignore_ascii_case("INBOX") {
            return 0;
        }
        if sent_by_name.as_deref() == Some(mailbox.raw.as_str()) {
            return 1;
        }
        match roles
            .iter()
            .find(|folder| folder.raw == mailbox.raw)
            .map(|folder| folder.role)
        {
            Some(FolderRole::Inbox) => 0,
            Some(FolderRole::Sent) => 1,
            // Copies of mail other folders hold, or mail on its way out.
            Some(FolderRole::All | FolderRole::Trash | FolderRole::Junk) => 3,
            _ => 2,
        }
    };
    let mut order: Vec<(u8, usize)> = listed
        .iter()
        .enumerate()
        .filter(|(_, mailbox)| !views.contains_key(&mailbox.raw))
        .map(|(index, mailbox)| (rank(mailbox), index))
        .collect();
    order.sort();
    let pick = |keep: &dyn Fn(u8) -> bool, reach: Reach| {
        order
            .iter()
            .filter(|(rank, _)| keep(*rank))
            .map(|(_, index)| (*index, reach))
            .collect::<Vec<_>>()
    };
    let mut visits = pick(&|rank| rank == 0, Reach::Head(sizes.inbox_head));
    visits.extend(pick(&|rank| rank == 1, Reach::Head(sizes.sent_head)));
    visits.extend(pick(&|rank| rank == 0, Reach::All));
    visits.extend(pick(&|rank| rank >= 2, Reach::Head(sizes.other_head)));
    visits.extend(pick(&|rank| rank >= 1, Reach::All));
    // Ordinary folders have all had a chance to supply the body. All
    // Mail is last, including after Starred and the label mailboxes.
    let mut projected: Vec<_> = listed
        .iter()
        .enumerate()
        .filter_map(|(index, mailbox)| {
            let view = views.get(&mailbox.raw)?;
            Some((view.kind == MailboxViewKind::All, index))
        })
        .collect();
    projected.sort_unstable();
    visits.extend(projected.into_iter().map(|(_, index)| (index, Reach::All)));
    visits
}

/// Bring one mailbox up to date on the pass's first visit of it: new mail,
/// flag changes, and copies that left. A mailbox never synced (or under a
/// new UIDVALIDITY) is not read here: its checkpoint is set to read it
/// newest first, and this returns true, as it does for a mailbox whose
/// first sync has older mail left. Then [`walk_down`] reads it.
fn catch_up(
    pass: &mut Pass<'_>,
    session: &mut session::Session,
    mailbox: &session::Listed,
    examined: &session::Examined,
    mode: FlagMode<'_>,
    persist: &mut dyn FnMut(MailboxDelta) -> Result<(), Error>,
) -> Result<bool, Error> {
    let account_id = pass.account_id;
    let mailboxes = pass.listed;
    let prior_checkpoint = pass.prior.get(&mailbox.raw);
    let projected = mode.view.is_some();
    // Old checkpoints treated virtual mailboxes as folders. Revisit their
    // headers and flags once to establish membership from the server.
    let previous_owned = prior_checkpoint
        .filter(|checkpoint| checkpoint.projected == projected)
        .cloned();
    let same_validity = previous_owned
        .as_ref()
        .is_some_and(|prev| prev.uidvalidity == examined.uidvalidity);
    if !same_validity
        && examined.exists > 0
        && let Some(next) = examined.uidnext.filter(|next| *next > 1)
    {
        pass.caught_up.insert(mailbox.raw.clone(), next);
        // Nothing below UIDNEXT is read yet. HIGHESTMODSEQ now is where
        // flag changes are asked for once the walk is done: every flag it
        // reads is at least this new.
        pass.done.insert(
            mailbox.raw.clone(),
            MailboxCheckpoint {
                uidvalidity: examined.uidvalidity,
                uid: next - 1,
                modseq: examined.highest_modseq,
                uidnext: None,
                pruned: None,
                floor: Some(next),
                keyword_baseline: pass.keywords,
                projected,
                star_baseline: !pass.keywords,
            },
        );
        return Ok(true);
    }
    // A first sync with older mail left: new mail is fetched here, the rest
    // by `walk_down`. Flags and removals wait until it reaches the oldest.
    let floor = previous_owned
        .as_ref()
        .filter(|_| same_validity)
        .and_then(|prev| prev.floor);
    let walking = floor.is_some();
    let keyword_baseline = pass.keywords
        && (!same_validity
            || previous_owned
                .as_ref()
                .is_some_and(|prev| prev.keyword_baseline));
    let star_baseline = !pass.keywords
        && (!same_validity
            || previous_owned
                .as_ref()
                .is_some_and(|prev| prev.star_baseline));
    let plan = plan_fetch(previous_owned.as_ref(), examined);
    // High-water body fetches ignore UIDs already at the checkpoint.
    // CHANGEDSINCE results are kept so an older flag update is applied.
    let carried_pruned = previous_owned
        .as_ref()
        .filter(|_| same_validity)
        .and_then(|prev| prev.pruned);
    let seen_through = match (previous_owned.as_ref(), &plan) {
        (Some(prev), FetchPlan::HighWater { .. } | FetchPlan::Discover { .. }) if same_validity => {
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
    let prior = &pass.prior;
    let flush_batch = |batch: &mut Vec<rfc822::FetchedMessage>,
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
            floor,
            keyword_baseline,
            projected,
            star_baseline,
        };
        emit_progress(
            persist,
            &Snapshot {
                account_id,
                listed: mailboxes,
                prior,
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
        known: pass.known,
    };
    let done = &pass.done;
    let trusted = session.trust_verified_sender_header;
    let stop = stream_bodies(session, &plan, &copies, mailbox, mode, |streamed| {
        let item = match streamed {
            Streamed::Linked(links) => {
                return persist(MailboxDelta {
                    links,
                    ..MailboxDelta::default()
                });
            }
            Streamed::Stored(item) => {
                let uid = item.uid;
                if seen_through.is_none_or(|floor| uid > floor) {
                    stored_highest = Some(stored_highest.map_or(uid, |top| top.max(uid)));
                }
                pending_flags.push(flag_update(uidvalidity, &escaped, &item, mode));
                if pending_flags.len() >= BATCH_FLAGS {
                    flush_batch(
                        &mut batch,
                        &mut batch_bytes,
                        &mut pending_flags,
                        capped_highest(highest, min_unparsed),
                        persist,
                        done,
                    )?;
                }
                return Ok(());
            }
            Streamed::BodyUnneeded { uid } => {
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
        pending_flags.extend(arrival_flags(uidvalidity, &escaped, &item, mode));
        batch.push(rfc822::fetched_message(
            account_id,
            mailbox,
            uidvalidity,
            &item,
            !mode.keywords,
            trusted,
        ));
        batch_bytes += raw_len;
        if batch.len() >= batch_message_limit() || batch_bytes >= BATCH_BYTES {
            flush_batch(
                &mut batch,
                &mut batch_bytes,
                &mut pending_flags,
                capped_highest(highest, min_unparsed),
                persist,
                done,
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
            pass.complete = false;
            if batch.is_empty() && highest.is_none() && pending_flags.is_empty() {
                if let Some(prev) = previous_owned.clone() {
                    pass.done.insert(mailbox.raw.clone(), prev);
                    emit_progress(
                        persist,
                        &pass.snapshot(),
                        None,
                        Vec::new(),
                        Vec::new(),
                        Vec::new(),
                    )?;
                }
                // Older mail waits for the next pass too.
                return Ok(false);
            }
            (false, None)
        }
        session::FetchStop::Truncated { min_skipped_uid } => (false, min_skipped_uid),
    };
    let mut flags = std::mem::take(&mut pending_flags);
    let mut flags_complete = true;
    let needs_baseline = finished
        && !walking
        && same_validity
        && previous_owned
            .as_ref()
            .is_some_and(|prev| prev.modseq.is_none() && prev.uid > 0)
        && highest_modseq.is_some()
        && !matches!(plan, FetchPlan::Full | FetchPlan::ChangedSince(_));
    if needs_baseline {
        match session.uid_flags("1:*") {
            Ok(rows) => {
                flags.extend(
                    rows.iter()
                        .map(|item| flag_update(uidvalidity, &escaped, item, mode)),
                );
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
    // or a UID FLAGS baseline for older mail that had no modseq yet. Not
    // while a first sync has older mail left: its flags were read over
    // several windows, maybe several passes.
    let saw_every_flag = finished
        && !walking
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
    if !walking {
        pass.complete &= clean;
    }
    let seen = Seen {
        uidvalidity,
        uidnext,
        exists,
    };
    // Bridge can report changes to older stars without a MODSEQ. Keep this
    // sweep to star evidence so it does not touch the read-state logic.
    if !pass.keywords
        && !walking
        && same_validity
        && highest_modseq.is_none()
        && previous_owned.as_ref().is_some_and(|prev| prev.uid > 0)
        && !matches!(plan, FetchPlan::Full)
        && let Some(mut refreshed) = walk_flags(session, mailbox, seen, &escaped, mode)?
    {
        for flag in &mut refreshed {
            flag.unread = None;
        }
        flags.extend(refreshed);
    }
    let tracked = if clean && !walking {
        track_removals(
            session,
            mailbox,
            seen,
            prior_checkpoint,
            &plan,
            !mode.keywords,
        )?
    } else {
        Tracked {
            removals: Vec::new(),
            pruned: carried_pruned,
        }
    };
    if finished && let Some(next) = uidnext {
        pass.caught_up.insert(mailbox.raw.clone(), next);
    }
    let mut current = MailboxCheckpoint {
        uidvalidity,
        uid,
        modseq,
        // What a finished pass saw, so IDLE can tell later mail apart.
        uidnext: if finished && !walking { uidnext } else { None },
        pruned: tracked.pruned,
        floor,
        keyword_baseline,
        projected,
        star_baseline,
    };
    emit_progress(
        persist,
        &pass.snapshot(),
        Some((&mailbox.raw, &current)),
        std::mem::take(&mut batch),
        flags,
        tracked.removals,
    )?;
    if clean && !walking {
        upgrade_flag_baseline(pass, session, mailbox, seen, &mut current, persist)?;
    }
    pass.done.insert(mailbox.raw.clone(), current);
    Ok(walking)
}

/// One visit of a mailbox whose first sync has older mail left.
struct Walking<'a> {
    mailbox: &'a session::Listed,
    /// What the UI calls it, for progress.
    folder: &'a str,
    examined: &'a session::Examined,
    reach: Reach,
    flags: FlagMode<'a>,
}

/// How [`walk_down`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Walked {
    /// It read as far as the visit allowed; older mail is left.
    Paused,
    /// It reached the oldest message and finalized the mailbox.
    Finished,
    /// The server did not finish a window or the final flag sweep.
    Stopped,
}

/// Read a mailbox on its first sync from its newest unread message down, a
/// window at a time.
///
/// Each window is a UID range just below the checkpoint's `floor`. Its
/// headers come first (`BODY.PEEK[HEADER]`), stored as rows with no body so
/// the list shows them; then its bodies, newest first, a few dozen per
/// `UID FETCH`. The floor moves down, in the same write, as each fetch of
/// bodies is stored, so a restart resumes below what was read. The lowest
/// UID of a window is the UID at the sequence number a window below the
/// floor, so windows hold about as many messages however sparse UIDs are;
/// a guess that lands above the floor is lowered and tried again.
///
/// A body too large to keep leaves its header row, marked so in the write
/// that moves the floor below it ([`crate::BodyState::TooLarge`]), and is
/// not asked for again. A body the server does not send lowers the
/// checkpoint below it, so the catch-up of a later pass fetches it.
fn walk_down(
    pass: &mut Pass<'_>,
    session: &mut session::Session,
    walking: Walking<'_>,
    persist: &mut dyn FnMut(MailboxDelta) -> Result<(), Error>,
) -> Result<Walked, Error> {
    let Walking {
        mailbox,
        folder,
        examined,
        reach,
        flags: mode,
    } = walking;
    let Some(mut checkpoint) = pass.done.get(&mailbox.raw).cloned() else {
        return Ok(Walked::Stopped);
    };
    let sizes = windows();
    let escaped = escape_mailbox(&mailbox.raw);
    let copies = Copies {
        mailbox: &escaped,
        uidvalidity: checkpoint.uidvalidity,
        known: pass.known,
    };
    let total = examined.exists;
    let mut floor = checkpoint.floor.unwrap_or(1);
    // About how many messages sit below the floor: what EXAMINE counts,
    // less the copies stored at or above it.
    let stored_above = copies.stored(floor, u32::MAX)?.len() as u32;
    let mut below = total.saturating_sub(stored_above);
    let mut read = 0u32;
    // A window that only has its headers is visible, but its bodies are
    // still pending. Keep missing bodies pending for the rest of the visit.
    let mut pending_bodies = 0u32;
    let progress = |below: u32, pending: u32| SyncProgress {
        folder: folder.to_string(),
        done: total.saturating_sub(below.saturating_add(pending)),
        total,
    };
    while floor > 1 {
        if let Reach::Head(limit) = reach
            && read >= limit
        {
            return Ok(Walked::Paused);
        }
        let size = if checkpoint.floor == Some(checkpoint.uid.saturating_add(1)) {
            sizes.first
        } else {
            sizes.window
        };
        let first_seq = below.saturating_sub(size).saturating_add(1);
        let low = if first_seq <= 1 {
            1
        } else {
            match session.uid_search_seq(first_seq, first_seq)? {
                Some(uids) => match uids.first() {
                    Some(uid) => *uid,
                    // Fewer messages than the guess: try lower.
                    None => {
                        below = first_seq - 1;
                        continue;
                    }
                },
                None => floor.saturating_sub(size).max(1),
            }
        };
        if low >= floor {
            // That message is read already: the guess was high.
            below = first_seq - 1;
            continue;
        }
        let high = floor - 1;
        let mut resolved = Vec::new();
        let range = format!("{low}:{high}");
        let header_set = if mode.view.is_some() {
            let found = projections::link(
                session,
                mailbox,
                &copies,
                &range,
                mode,
                |uid| (low..=high).contains(&uid),
                |links| {
                    persist(MailboxDelta {
                        links,
                        ..MailboxDelta::default()
                    })
                },
            )?;
            let Some(found) = found else {
                pass.complete = false;
                return Ok(Walked::Stopped);
            };
            resolved = copies.stored(low, high)?;
            let wanted: Vec<_> = found
                .into_iter()
                .filter(|uid| resolved.binary_search(uid).is_err())
                .collect();
            (!wanted.is_empty()).then(|| uid_set(&wanted))
        } else {
            Some(range)
        };
        let mut by_uid = std::collections::BTreeMap::new();
        if let Some(set) = header_set {
            let mut fetch = |session: &mut session::Session, items| {
                by_uid.clear();
                session.uid_fetch_each(&set, None, items, |item| {
                    if (low..=high).contains(&item.uid) {
                        use std::collections::btree_map::Entry;
                        match by_uid.entry(item.uid) {
                            Entry::Vacant(entry) => {
                                entry.insert(item);
                            }
                            Entry::Occupied(mut entry) => entry.get_mut().merge_header_fields(item),
                        }
                    }
                    Ok(())
                })
            };
            let mut stop = fetch(session, session::FetchItems::Headers)?;
            if matches!(stop, session::FetchStop::Refused) {
                // A provider that rejects a combined request can still
                // deliver its headers. Body parsing supplies the list later.
                stop = fetch(session, session::FetchItems::HeadersOnly)?;
            }
            if !matches!(stop, session::FetchStop::Done) {
                pass.complete = false;
                return Ok(Walked::Stopped);
            }
        }
        let rows: Vec<_> = by_uid
            .into_values()
            .filter(|row| !row.body_missing || row.flags_present)
            .collect();
        // A tagged OK can end a short FETCH. SEARCH lists the UIDs that
        // actually exist in the range, including across gaps in the UID space.
        // Search after the fetch so an expunged UID does not hold up the walk.
        let listed = match session.uid_search_range(low, high) {
            Ok(listed) => listed,
            Err(err) if untagged_overflow(&err) => None,
            Err(err) => return Err(err),
        };
        let Some(listed) = listed.filter(|uids| {
            uids.iter().all(|uid| {
                resolved.binary_search(uid).is_ok()
                    || rows.binary_search_by_key(uid, |row| row.uid).is_ok()
            })
        }) else {
            pass.complete = false;
            return Ok(Walked::Stopped);
        };
        let in_window = listed.len() as u32;
        let stored = copies.stored(low, high)?;
        let mut flags = Vec::new();
        let mut headers = Vec::new();
        // The Message-ID each header row was stored under, for a body too
        // large to keep.
        let mut header_ids: HashMap<u32, crate::MessageId> = HashMap::new();
        // Newest first, with RFC822.SIZE.
        let mut wanted: Vec<(u32, u32)> = Vec::new();
        for mut row in rows.into_iter().rev() {
            if stored.binary_search(&row.uid).is_ok() {
                flags.push(flag_update(checkpoint.uidvalidity, &escaped, &row, mode));
                continue;
            }
            if rfc822::is_deleted(&row.flags) {
                continue;
            }
            if !row.skipped && !row.body_missing {
                let mut header = rfc822::headers_message(
                    pass.account_id,
                    mailbox,
                    checkpoint.uidvalidity,
                    &row,
                    !mode.keywords,
                );
                header.structure = row.structure.take();
                header_ids.insert(row.uid, header.message.headers.message_id.clone());
                let learned = arrival_flags(checkpoint.uidvalidity, &escaped, &row, mode);
                headers.push((header, learned));
            }
            wanted.push((row.uid, row.size.unwrap_or(0)));
        }
        below = below.saturating_sub(in_window);
        read = read.saturating_add(in_window);
        let requested = wanted.len() as u32;
        pending_bodies = pending_bodies.saturating_add(requested);
        // Rows first, so the list shows the window before its bodies.
        let mut rows_left = headers.into_iter();
        loop {
            let (chunk, learned): (Vec<_>, Vec<_>) = rows_left.by_ref().take(HEADER_ROWS).unzip();
            flags.extend(learned.into_iter().flatten());
            if chunk.is_empty() && flags.is_empty() {
                break;
            }
            emit_walk(
                persist,
                pass,
                (&mailbox.raw, &checkpoint),
                chunk,
                std::mem::take(&mut flags),
                Vec::new(),
                Some(progress(below, pending_bodies)),
            )?;
        }
        // A copy whose message another folder already gave a body to
        // needs no second download.
        let filled = copies.stored(low, high)?;
        wanted.retain(|(uid, _)| filled.binary_search(uid).is_err());
        pending_bodies -= requested - wanted.len() as u32;
        let mut chunks: Vec<&[(u32, u32)]> = Vec::new();
        let mut start = 0;
        while start < wanted.len() {
            let mut end = start;
            let mut bytes = 0usize;
            while end < wanted.len()
                && end - start < BODIES_PER_FETCH
                && (end == start || bytes + wanted[end].1 as usize <= BATCH_BYTES)
            {
                bytes += wanted[end].1 as usize;
                end += 1;
            }
            chunks.push(&wanted[start..end]);
            start = end;
        }
        let count = chunks.len();
        for (index, chunk) in chunks.into_iter().enumerate() {
            let mut uids: Vec<u32> = chunk.iter().map(|(uid, _)| *uid).collect();
            uids.sort_unstable();
            let mut batch = Vec::new();
            let mut body_flags = Vec::new();
            let mut batch_bytes = 0usize;
            // Answered with a body, or too large to keep.
            let mut answered: HashSet<u32> = HashSet::with_capacity(uids.len());
            // Header rows whose body is too large to keep: marked so, in
            // the write that moves the floor below them.
            let mut too_large: Vec<crate::MessageId> = Vec::new();
            let trusted = session.trust_verified_sender_header;
            let stop = session.uid_fetch_each(
                &uid_set(&uids),
                None,
                session::FetchItems::Bodies,
                |item| {
                    if uids.binary_search(&item.uid).is_err() || item.body_missing {
                        return Ok(());
                    }
                    if !answered.insert(item.uid) {
                        return Ok(());
                    }
                    if item.skipped
                        && let Some(id) = header_ids.get(&item.uid)
                    {
                        too_large.push(id.clone());
                    }
                    if item.skipped || rfc822::is_deleted(&item.flags) {
                        return Ok(());
                    }
                    batch_bytes += item.body.len();
                    body_flags.extend(arrival_flags(checkpoint.uidvalidity, &escaped, &item, mode));
                    batch.push(rfc822::fetched_message(
                        pass.account_id,
                        mailbox,
                        checkpoint.uidvalidity,
                        &item,
                        !mode.keywords,
                        trusted,
                    ));
                    // Bodies show as they come, whether or not the fetch ends.
                    if batch.len() >= batch_message_limit() || batch_bytes >= BATCH_BYTES {
                        batch_bytes = 0;
                        emit_walk(
                            persist,
                            pass,
                            (&mailbox.raw, &checkpoint),
                            std::mem::take(&mut batch),
                            std::mem::take(&mut body_flags),
                            Vec::new(),
                            Some(progress(
                                below,
                                pending_bodies - answered.len() as u32 + too_large.len() as u32,
                            )),
                        )?;
                    }
                    Ok(())
                },
            )?;
            let finished = matches!(stop, session::FetchStop::Done);
            if finished {
                // A body the server did not send, or sent unreadably, is
                // asked for again: the catch-up of a later pass starts
                // below it and fetches what is not stored. HIGHESTMODSEQ
                // goes too, so that pass is not a CHANGEDSINCE one.
                if let Some(unread) = uids.iter().find(|uid| !answered.contains(uid)) {
                    checkpoint.uid = checkpoint.uid.min(unread.saturating_sub(1));
                    checkpoint.modseq = None;
                }
                // Everything from here up to the old floor is read. The last
                // fetch of a window takes the floor to the window's bottom.
                floor = if index + 1 == count { low } else { uids[0] };
                checkpoint.floor = Some(floor);
            }
            pending_bodies -= answered.len() as u32;
            emit_walk(
                persist,
                pass,
                (&mailbox.raw, &checkpoint),
                batch,
                body_flags,
                too_large,
                Some(progress(below, pending_bodies)),
            )?;
            if !finished {
                pass.complete = false;
                pass.done.insert(mailbox.raw.clone(), checkpoint);
                return Ok(Walked::Stopped);
            }
        }
        if count == 0 {
            floor = low;
            checkpoint.floor = Some(floor);
            emit_walk(
                persist,
                pass,
                (&mailbox.raw, &checkpoint),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Some(progress(below, pending_bodies)),
            )?;
        }
        pass.done.insert(mailbox.raw.clone(), checkpoint.clone());
    }
    let seen = Seen {
        uidvalidity: examined.uidvalidity,
        uidnext: examined.uidnext,
        exists: examined.exists,
    };
    // CONDSTORE can use the modseq from the start of the walk. Otherwise
    // read every flag once more. Keep floor=1 on failure so the next pass
    // retries this sweep without downloading the windows again.
    let flags = if examined.highest_modseq.is_none() {
        let Some(flags) = walk_flags(session, mailbox, seen, &escaped, mode)? else {
            pass.complete = false;
            return Ok(Walked::Stopped);
        };
        flags
    } else {
        Vec::new()
    };
    // The oldest message is read. Report the copies that left while the
    // walk ran, as the end of a full first pass would (with the checkpoint
    // the pass began from, so a UIDVALIDITY change or a walk resumed from
    // an earlier pass also learns Message-IDs). The modseq kept from
    // where the walk began has the next pass ask for every flag changed
    // since.
    let tracked = track_removals(
        session,
        mailbox,
        seen,
        pass.prior.get(&mailbox.raw),
        &FetchPlan::Full,
        !mode.keywords,
    )?;
    checkpoint.floor = None;
    checkpoint.pruned = tracked.pruned;
    // Mail that came while the walk ran is above what this pass caught up
    // to, so IDLE reports it.
    checkpoint.uidnext = pass.caught_up.get(&mailbox.raw).copied();
    emit_progress(
        persist,
        &pass.snapshot(),
        Some((&mailbox.raw, &checkpoint)),
        Vec::new(),
        flags,
        tracked.removals,
    )?;
    upgrade_flag_baseline(pass, session, mailbox, seen, &mut checkpoint, persist)?;
    pass.done.insert(mailbox.raw.clone(), checkpoint);
    Ok(Walked::Finished)
}

/// A legacy cursor can already be at HIGHESTMODSEQ without keyword or
/// Bridge star membership on its rows. Read FLAGS for every UID once.
fn upgrade_flag_baseline(
    pass: &Pass<'_>,
    session: &mut session::Session,
    mailbox: &session::Listed,
    seen: Seen,
    checkpoint: &mut MailboxCheckpoint,
    persist: &mut dyn FnMut(MailboxDelta) -> Result<(), Error>,
) -> Result<(), Error> {
    let keywords = pass.keywords && !checkpoint.keyword_baseline;
    let stars = !pass.keywords && !checkpoint.star_baseline && checkpoint.modseq.is_some();
    if (keywords || stars)
        && let Some(mut flags) = walk_flags(
            session,
            mailbox,
            seen,
            &escape_mailbox(&mailbox.raw),
            FlagMode {
                keywords,
                view: None,
            },
        )?
    {
        // Leave read state to the ordinary delta for this mailbox.
        for flag in &mut flags {
            flag.unread = None;
        }
        checkpoint.keyword_baseline |= keywords;
        checkpoint.star_baseline |= stars;
        emit_progress(
            persist,
            &pass.snapshot(),
            Some((&mailbox.raw, checkpoint)),
            Vec::new(),
            flags,
            Vec::new(),
        )?;
    }
    Ok(())
}

/// Refresh flags in bounded UID sets. An incomplete reply leaves the sweep
/// pending, and an expunge can change which messages the listing covers.
fn walk_flags(
    session: &mut session::Session,
    mailbox: &session::Listed,
    seen: Seen,
    escaped: &str,
    mode: FlagMode<'_>,
) -> Result<Option<Vec<FlagUpdate>>, Error> {
    let Some((uids, _)) = list_mailbox(session, mailbox, seen, false, !mode.keywords)? else {
        return Ok(None);
    };
    let mut flags = Vec::with_capacity(uids.len());
    for chunk in uids.chunks(UIDS_PER_FETCH) {
        let mut rows = match session.uid_flags(&uid_set(chunk)) {
            Ok(rows) => rows,
            Err(err) if uid_fetch_refused(&err) || untagged_overflow(&err) => return Ok(None),
            Err(err) => return Err(err),
        };
        rows.retain(|row| chunk.binary_search(&row.uid).is_ok());
        rows.sort_by_key(|row| (row.uid, std::cmp::Reverse(row.modseq)));
        rows.dedup_by_key(|row| row.uid);
        if rows.len() != chunk.len() {
            return Ok(None);
        }
        flags.extend(
            rows.iter()
                .map(|row| flag_update(seen.uidvalidity, escaped, row, mode)),
        );
    }
    Ok(Some(flags))
}

/// Persist rows, flags and the checkpoint of a mailbox being walked.
fn emit_walk(
    persist: &mut dyn FnMut(MailboxDelta) -> Result<(), Error>,
    pass: &Pass<'_>,
    current: (&str, &MailboxCheckpoint),
    messages: Vec<rfc822::FetchedMessage>,
    flags: Vec<FlagUpdate>,
    too_large: Vec<crate::MessageId>,
    progress: Option<SyncProgress>,
) -> Result<(), Error> {
    let mut delta = progress_delta(&pass.snapshot(), Some(current), messages, flags, Vec::new());
    delta.progress = progress;
    delta.too_large = too_large;
    persist(delta)
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
    bridge: bool,
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
    let Some((present, found)) = list_mailbox(session, mailbox, seen, discover, bridge)? else {
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
    bridge: bool,
) -> Result<Option<Listing>, Error> {
    let page = if discover { FOUND_PAGE } else { LIST_PAGE };
    let mut present = Vec::new();
    let mut found = Vec::new();
    let mut first = 1u32;
    while first <= seen.exists {
        let last = first.saturating_add(page - 1).min(seen.exists);
        let listed = if discover {
            session.fetch_message_ids(first, last, bridge).map(|rows| {
                rows.map(|rows| {
                    let mut uids = Vec::with_capacity(rows.len());
                    for (uid, headers) in rows {
                        let id = rfc822::copy_message_id(
                            &mailbox.raw,
                            seen.uidvalidity,
                            uid,
                            &headers,
                            bridge,
                        );
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
    /// Message-ID headers that can link virtual copies to an existing row.
    Linked(Vec<CopyLink>),
    /// Not stored yet: fetched with its body.
    Fetched(session::Fetched),
    /// Stored already: only its flags were fetched.
    Stored(session::Fetched),
    /// A deleted copy or a virtual copy that can reuse a stored body.
    BodyUnneeded { uid: u32 },
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
    mailbox: &session::Listed,
    mode: FlagMode<'_>,
    mut on_fetched: impl FnMut(Streamed) -> Result<(), Error>,
) -> Result<session::FetchStop, Error> {
    let (start, changed_since, discover) = match plan {
        FetchPlan::Idle => return Ok(session::FetchStop::Done),
        FetchPlan::ChangedSince(modseq) => (1, Some(*modseq), true),
        FetchPlan::Discover { start } => (*start, None, true),
        FetchPlan::Full => (1, None, false),
        FetchPlan::HighWater { start } => (*start, None, false),
    };
    if discover
        || !mode.keywords
        || mode.view.is_some()
        || !copies.stored(start, u32::MAX)?.is_empty()
    {
        return flags_then_bodies(
            session,
            (start, changed_since),
            copies,
            mailbox,
            mode,
            on_fetched,
        );
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
    range: (u32, Option<u64>),
    copies: &Copies<'_>,
    mailbox: &session::Listed,
    mode: FlagMode<'_>,
    mut on_fetched: impl FnMut(Streamed) -> Result<(), Error>,
) -> Result<session::FetchStop, Error> {
    let (start, changed_since) = range;
    let mut rows = Vec::new();
    let listed = session.uid_fetch_each(
        &format!("{start}:*"),
        changed_since,
        session::FetchItems::Flags,
        |item| {
            // `n:*` names the highest UID even when it is below `n`.
            if item.uid >= start {
                rows.push(item);
            }
            Ok(())
        },
    )?;
    let cut = match listed {
        session::FetchStop::Done => None,
        session::FetchStop::Refused => return Ok(session::FetchStop::Refused),
        session::FetchStop::Truncated { min_skipped_uid } => Some(min_skipped_uid),
    };
    rows.sort_by_key(|item| (item.uid, std::cmp::Reverse(item.modseq)));
    rows.dedup_by_key(|item| item.uid);
    let stored = match (rows.first(), rows.last()) {
        (Some(low), Some(high)) => copies.stored(low.uid, high.uid)?,
        _ => Vec::new(),
    };
    let mut wanted = Vec::new();
    for item in rows {
        let uid = item.uid;
        if stored.binary_search(&uid).is_ok() {
            on_fetched(Streamed::Stored(item))?;
        } else if rfc822::is_deleted(&item.flags) {
            // Not stored either way, so its body is not worth the bytes.
            on_fetched(Streamed::BodyUnneeded { uid })?;
        } else {
            wanted.push(uid);
        }
    }
    // A fetch can finish without a UID it was asked for (expunged since the
    // flags came, or a server that drops it): the checkpoint stays below the
    // lowest such UID, as when a fetch stops early.
    let mut first_missing: Option<u32> = None;
    for (index, chunk) in wanted.chunks(UIDS_PER_FETCH).enumerate() {
        let unresolved;
        let chunk = if mode.view.is_some() || !mode.keywords {
            let Some(_) = projections::link(
                session,
                mailbox,
                copies,
                &uid_set(chunk),
                mode,
                |uid| chunk.binary_search(&uid).is_ok(),
                |links| on_fetched(Streamed::Linked(links)),
            )?
            else {
                return Ok(session::FetchStop::Truncated {
                    min_skipped_uid: lowest([first_missing, chunk.first().copied(), cut.flatten()]),
                });
            };
            let stored = copies.stored(chunk[0], chunk[chunk.len() - 1])?;
            for &uid in chunk.iter().filter(|uid| stored.binary_search(uid).is_ok()) {
                on_fetched(Streamed::BodyUnneeded { uid })?;
            }
            unresolved = chunk
                .iter()
                .copied()
                .filter(|uid| stored.binary_search(uid).is_err())
                .collect::<Vec<_>>();
            if unresolved.is_empty() {
                continue;
            }
            unresolved.as_slice()
        } else {
            chunk
        };
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

/// Flag evidence from one server copy.
#[derive(Clone, Copy)]
struct FlagMode<'a> {
    keywords: bool,
    view: Option<&'a MailboxViewKind>,
}

fn flag_update(
    uidvalidity: u32,
    escaped: &str,
    item: &session::Fetched,
    mode: FlagMode<'_>,
) -> FlagUpdate {
    let uid = item.uid;
    FlagUpdate {
        remote_id: RemoteId(format!("imap:{uidvalidity}:{uid}:{escaped}")),
        unread: Some(
            !item
                .flags
                .iter()
                .any(|flag| flag.eq_ignore_ascii_case("\\Seen")),
        ),
        flagged: item.flags_present.then(|| {
            item.flags
                .iter()
                .any(|flag| flag.eq_ignore_ascii_case("\\Flagged"))
        }),
        keywords: match mode.view {
            Some(MailboxViewKind::Label(label)) => Some(
                (!rfc822::is_deleted(&item.flags))
                    .then(|| label.clone())
                    .into_iter()
                    .collect(),
            ),
            _ if mode.keywords => item.flags_present.then(|| rfc822::keywords(&item.flags)),
            _ => None,
        },
        delivery_keywords: (!mode.keywords && mode.view.is_none() && item.flags_present).then(
            || {
                item.flags
                    .iter()
                    .filter(|flag| is_delivery_keyword(flag))
                    .cloned()
                    .collect()
            },
        ),
        modseq: item.modseq,
    }
}

pub(crate) fn is_delivery_keyword(flag: &str) -> bool {
    flag.eq_ignore_ascii_case("$Muted") || flag.eq_ignore_ascii_case("$Junk")
}

/// Reserved IMAP flags and keywords, matched as whole, case-insensitive atoms.
pub(crate) fn is_reserved_keyword(keyword: &str) -> bool {
    // IANA IMAP and JMAP Keywords registry, including its JMAP-only and
    // reserved entries. Thunderbird's $label1–$label5 are user tags, and
    // FLAGS alone cannot distinguish them from other uses of those names.
    const RESERVED: &[&str] = &[
        "$MDNSent",
        "$Forwarded",
        "$SubmitPending",
        "$Submitted",
        "$Junk",
        "$NotJunk",
        "$Phishing",
        "$Important",
        "$draft",
        "$seen",
        "$flagged",
        "$answered",
        "$recent",
        "$autosent",
        "$canunsubscribe",
        "$followed",
        "$hasattachment",
        "$hasmemo",
        "$hasnoattachment",
        "$imported",
        "$istrusted",
        "$MailFlagBit0",
        "$MailFlagBit1",
        "$MailFlagBit2",
        "$maskedemail",
        "$memo",
        "$muted",
        "$new",
        "$notify",
        "$unsubscribed",
        // Bare flags emitted by some IMAP clients.
        "Forwarded",
        "Redirected",
        "Junk",
        "NonJunk",
        "NotJunk",
    ];
    keyword.starts_with('\\')
        || RESERVED
            .iter()
            .any(|reserved| keyword.eq_ignore_ascii_case(reserved))
}

/// With no revision, a new row already applied unread as part of its arrival.
/// Remember its keywords without applying a second read observation.
fn arrival_flags(
    uidvalidity: u32,
    escaped: &str,
    item: &session::Fetched,
    mode: FlagMode<'_>,
) -> Option<FlagUpdate> {
    let mut update = flag_update(uidvalidity, escaped, item, mode);
    if item.modseq.is_none()
        && update.flagged.is_none()
        && update.keywords.is_none()
        && update.delivery_keywords.is_none()
        && !matches!(mode.view, Some(MailboxViewKind::Starred))
    {
        return None;
    }
    if item.modseq.is_none() {
        update.unread = None;
    }
    Some(update)
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
    messages: Vec<rfc822::FetchedMessage>,
    flags: Vec<FlagUpdate>,
    removals: Vec<FolderRemoval>,
) -> Result<(), Error> {
    persist(progress_delta(snapshot, current, messages, flags, removals))
}

/// A delta carrying the cursor every mailbox has reached, `current` in
/// place of what the snapshot holds for it.
fn progress_delta(
    snapshot: &Snapshot<'_>,
    current: Option<(&str, &MailboxCheckpoint)>,
    fetched: Vec<rfc822::FetchedMessage>,
    flags: Vec<FlagUpdate>,
    removals: Vec<FolderRemoval>,
) -> MailboxDelta {
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
    let mut messages = Vec::with_capacity(fetched.len());
    let mut deferred_dates = HashMap::new();
    let mut body_structures = HashMap::new();
    for item in fetched {
        if let Some(date) = item.deferred_date {
            deferred_dates.insert(item.message.remote_id.clone(), date);
        }
        if let Some(structure) = item.structure {
            body_structures.insert(item.message.remote_id.clone(), structure);
        }
        messages.push(item.message);
    }
    MailboxDelta {
        messages,
        deferred_dates,
        body_structures,
        flags,
        cursor: Some(encode_cursor(snapshot.account_id.as_str(), &pairs)),
        removals,
        settle: Settle::Later,
        roles: None,
        uidplus: None,
        progress: None,
        too_large: Vec::new(),
        links: Vec::new(),
        views: None,
    }
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

fn listed_roles(
    listed: &[session::Listed],
    overrides: &[(FolderRole, String)],
    bridge: bool,
    views: &HashMap<String, MailboxView>,
) -> FolderRoles {
    let detected = FolderRoles::detect(&listed_mailboxes(listed), overrides);
    if !bridge {
        return detected;
    }
    // Bridge's named views are still available when LIST marks a different
    // special-use mailbox. Keep server and configured roles when present.
    let mut roles: Vec<_> = detected.iter().cloned().collect();
    for mailbox in listed {
        let role = match views.get(&mailbox.raw).map(|view| &view.kind) {
            Some(MailboxViewKind::All) => FolderRole::All,
            Some(MailboxViewKind::Starred) => FolderRole::Flagged,
            _ => continue,
        };
        if !roles.iter().any(|folder| folder.role == role) {
            roles.push(crate::folders::RoleFolder {
                role,
                name: mailbox.decoded.clone(),
                raw: mailbox.raw.clone(),
                source: crate::folders::RoleSource::Name,
            });
        }
    }
    FolderRoles::new(roles).with_bridge_names(true)
}

fn listed_mailboxes(listed: &[session::Listed]) -> Vec<ListedMailbox> {
    listed
        .iter()
        .map(|mailbox| ListedMailbox {
            raw: mailbox.raw.clone(),
            name: mailbox.decoded.clone(),
            delimiter: mailbox.delimiter,
            attributes: mailbox.attributes.clone(),
        })
        .collect()
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
            // While a first sync has older mail left, its modseq is where it
            // began; only new mail is fetched until the walk is done.
            if prev.floor.is_none()
                && let Some(modseq) = prev.modseq
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
    pub trust_verified_sender_header: bool,
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
            trust_verified_sender_header: false,
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
    /// While a first sync reads the mailbox newest first: the lowest UID read
    /// so far. Every UID from here to `uid` is read; those below are not yet.
    /// `None` once it reaches the oldest message, and for cursors from
    /// before first syncs read newest first.
    pub floor: Option<u32>,
    /// This UIDVALIDITY started with keyword tracking or finished its
    /// upgrade sweep. Older cursors and Bridge cursors omit the marker.
    pub keyword_baseline: bool,
    /// This mailbox has been synced as a view, with per-copy membership.
    pub projected: bool,
    /// Bridge's older `\Flagged` values have been applied to the rows.
    pub star_baseline: bool,
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
        let floor = match parts.next() {
            None | Some("-") => None,
            Some(raw) => match raw.parse().ok() {
                Some(value) => Some(value),
                None => return Vec::new(),
            },
        };
        let mut keyword_baseline = false;
        let mut projected = false;
        let mut star_baseline = false;
        if let Some(markers) = parts.next() {
            for marker in markers.split('+') {
                match marker {
                    "keywords" if !keyword_baseline => keyword_baseline = true,
                    "view" if !projected => projected = true,
                    "stars" if !star_baseline => star_baseline = true,
                    _ => return Vec::new(),
                }
            }
        }
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
                floor,
                keyword_baseline,
                projected,
                star_baseline,
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
        // Written only while a first sync is under way, so a cursor for a
        // mailbox caught up reads the same as before the field.
        if let Some(floor) = checkpoint.floor {
            token.push('\t');
            token.push_str(&floor.to_string());
        } else if checkpoint.keyword_baseline || checkpoint.projected || checkpoint.star_baseline {
            token.push_str("\t-");
        }
        let mut separator = '\t';
        for (enabled, marker) in [
            (checkpoint.keyword_baseline, "keywords"),
            (checkpoint.projected, "view"),
            (checkpoint.star_baseline, "stars"),
        ] {
            if enabled {
                token.push(separator);
                token.push_str(marker);
                separator = '+';
            }
        }
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

/// The folder name a raw (modified UTF-7) mailbox name stands for.
pub(crate) fn folder_name(raw: &str) -> String {
    utf7::decode_modified_utf7(raw)
}

pub(crate) fn escape_mailbox(name: &str) -> String {
    name.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

pub(crate) fn unescape_mailbox(raw: &str) -> Option<String> {
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
                    floor: None,
                    keyword_baseline: true,
                    projected: true,
                    star_baseline: true,
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
                    floor: None,
                    keyword_baseline: false,
                    projected: false,
                    star_baseline: false,
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
                floor: None,
                keyword_baseline: false,
                projected: false,
                star_baseline: false,
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
        assert!(!decoded[0].1.keyword_baseline);
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
