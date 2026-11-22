//! Replay of the write-back outbox ([`crate::outbox`]) on its own session.
//!
//! One pass loads the rows not yet applied and, only if there are any,
//! logs in on a [`WriteSession`] (never the read-only sync session). For
//! each source mailbox it sends `SELECT` and checks UIDVALIDITY. It sends
//! `UID MOVE` for moves and `UID STORE` for read and star changes.
//!
//! Before a row's first `UID MOVE`, replay records the target's UIDVALIDITY
//! and UIDNEXT (`STATUS`), then marks the row sent. When the reply is lost
//! or has no `COPYUID`, the server may have moved the message without
//! saying where, so a missing `COPYUID` is never taken as done. Replay
//! looks in the target with `UID SEARCH HEADER Message-ID`, keeps only UIDs
//! at or above the recorded UIDNEXT, and takes one only when its fetched
//! Message-ID header is exactly the row's. Anything less (no match, a
//! refused search, mail with no Message-ID) leaves the row to retry, and
//! after [`MAX_ATTEMPTS`] it is dropped with a notice. The source copy stays
//! on record until a new one is known.
//!
//! A refusal (NO or BAD) is retried with backoff and the row is dropped
//! after [`MAX_ATTEMPTS`]. A dropped connection ends the pass and costs the
//! rows nothing; the next pass tries again. Moves keep their queue order
//! because a move can change a later write's UID. A refused STORE can
//! retry on its own. A STORE with an unknown outcome holds its copy until
//! replay can check what reached the server.
//!
//! Undo's reverse of a row that was sent but not settled waits behind it,
//! then moves the copy that row placed. If that row never reached the
//! server (refused, or given up), the reverse is dropped without a notice.

use std::collections::{HashMap, HashSet};

use crate::Error;
use crate::account::AccountId;
use crate::folders::{FolderRole, FolderRoles};
use crate::outbox::{
    self, DropReason, DroppedWrite, OutboxEntry, OutboxKind, Outcome, ReplayReport, Reversed,
    Settled,
    flags::{self, FetchReason, Observation, Progress},
};
use crate::store::{Store, copies::CopyFlags};

use super::rfc822::{copy_message_id, is_synthetic_message_id};
use super::session::{FlagReply, Placed, WriteError, WriteSession, quote_imap};
use super::{ImapConfig, listed_mailboxes};

mod labels;

/// Refusals (or "not on the server yet") before a row is dropped.
pub(crate) const MAX_ATTEMPTS: u32 = 5;
/// Wait before the next try, by attempts so far. The last one repeats.
const BACKOFF_SECS: [i64; 5] = [10, 30, 120, 600, 1800];
/// Inconclusive checks are independent of explicit APPEND refusals.
pub(super) const MAX_APPEND_CHECKS: u32 = 5;
pub(super) const APPEND_UID_LIMIT: usize = 500;
const APPEND_HEADER_BATCH: usize = 100;

/// Send every outbox row that is due. Returns what was sent and dropped.
/// A connection that fails part way is in [`ReplayReport::interrupted`];
/// what was settled before it stays settled.
pub(super) fn replay_with(
    config: &ImapConfig,
    account_id: &AccountId,
    store: &Store,
    append_sent: bool,
    before_send: &dyn Fn(),
) -> crate::Result<ReplayReport> {
    let mut entries = outbox::unfinished(store, account_id)?;
    if !append_sent {
        entries.retain(|entry| entry.kind != OutboxKind::AppendSent);
    }
    let mut report = ReplayReport::default();
    if entries.is_empty() {
        return Ok(report);
    }
    let now = outbox::now_unix();
    if entries.iter().all(|entry| entry.retry_unix > now) {
        report.waiting = entries.len();
        return Ok(report);
    }
    let mut pass = Pass {
        store,
        account_id,
        now,
        entries: &entries,
        settled: vec![false; entries.len()],
        held_moves: HashMap::new(),
        unconfirmed_stores: HashMap::new(),
        listing: None,
        sent_mailbox: None,
        overrides: &config.folder_overrides,
        selected: None,
        bounds: HashMap::new(),
        before_send,
        report: &mut report,
    };
    let result = match WriteSession::open(config) {
        Ok(mut session) => {
            let result = pass.run(&mut session);
            if !matches!(&result, Err(Stop::Connection(_))) {
                session.logout();
            }
            result
        }
        Err(err)
            if entries
                .iter()
                .any(|entry| entry.kind == OutboxKind::AppendSent && entry.retry_unix <= now) =>
        {
            let note = err.to_string();
            for (index, entry) in entries.iter().enumerate() {
                if entry.kind == OutboxKind::AppendSent && entry.retry_unix <= now {
                    let guard = store.try_lock_mailbox_operation(".append-sent")?;
                    if (guard.is_none() && store.db_path().is_some())
                        || outbox::pending_append(store, entry)?.is_none()
                    {
                        continue;
                    }
                    match pass.retry_append(index, &note, AppendRetry::Deferred) {
                        Ok(()) => {}
                        Err(Stop::Store(error)) => return Err(error),
                        Err(_) => break,
                    }
                }
            }
            Err(Stop::Connection(err))
        }
        Err(err) => return Err(err),
    };
    let settled = pass.settled.iter().filter(|done| **done).count();
    match result {
        Ok(()) => {}
        Err(Stop::Store(err)) => return Err(err),
        Err(
            Stop::AppendRetry(message) | Stop::AppendRefused(message) | Stop::AppendCheck(message),
        ) => {
            return Err(Error::Backend(message));
        }
        Err(Stop::Connection(err) | Stop::AppendCheckDisconnected(err)) => {
            report.interrupted = Some(err.to_string());
        }
        // Write-back was turned off by another process mid-pass.
        Err(Stop::Stale) => {}
    }
    report.waiting = entries.len() - settled;
    Ok(report)
}

enum Stop {
    Connection(Error),
    Store(Error),
    AppendRetry(String),
    AppendRefused(String),
    AppendCheck(String),
    AppendCheckDisconnected(Error),
    /// The row is no longer in this write-back generation: send nothing more.
    Stale,
}

fn append_check_error(error: WriteError) -> Stop {
    match error {
        WriteError::Refused(message) => Stop::AppendCheck(message),
        WriteError::Connection(error @ Error::ImapLineLimit { .. }) => {
            Stop::AppendCheckDisconnected(error)
        }
        WriteError::Connection(error) => Stop::Connection(error),
    }
}

enum AppendRetry {
    Deferred,
    Refused,
    Inconclusive { checks: u32 },
}

enum MoveHold {
    /// An earlier MOVE can change which copy a later write should use.
    EarlierMove,
    /// A MOVE needs the copy with an unconfirmed STORE. Other copies can write.
    StoreRecovery,
}

impl From<Error> for Stop {
    fn from(err: Error) -> Self {
        Self::Store(err)
    }
}

struct Pass<'a> {
    store: &'a Store,
    account_id: &'a AccountId,
    now: i64,
    entries: &'a [OutboxEntry],
    /// Rows that reached Done or Dropped in this pass.
    settled: Vec<bool>,
    held_moves: HashMap<String, MoveHold>,
    /// Mailbox, UIDVALIDITY, and UID of each STORE still being confirmed.
    unconfirmed_stores: HashMap<String, HashSet<(u32, u32)>>,
    /// Folder name → raw mailbox name, from LIST when first needed.
    listing: Option<HashMap<String, String>>,
    sent_mailbox: Option<String>,
    overrides: &'a [(FolderRole, String)],
    /// The mailbox selected now and its UIDVALIDITY.
    selected: Option<(String, u32)>,
    /// Row id → the target's `(UIDVALIDITY, UIDNEXT)` before its first
    /// MOVE, for rows marked sent in this pass.
    bounds: HashMap<i64, Option<(u32, u32)>>,
    before_send: &'a dyn Fn(),
    report: &'a mut ReplayReport,
}

impl Pass<'_> {
    fn run(&mut self, session: &mut WriteSession) -> Result<(), Stop> {
        // Whether the user hears of it is up to the status line's report
        // (`outbox::report_for_status`): that notice is said once.
        let mut visited = vec![false; self.entries.len()];
        let has_move = session.has_move();
        if has_move {
            outbox::note_move_missing(self.store, self.account_id, false)?;
        } else {
            for (index, entry) in self.entries.iter().enumerate() {
                if entry.kind == OutboxKind::Move && entry.retry_unix <= self.now {
                    self.drop_row(index, DropReason::NoMove, "server has no MOVE")?;
                    visited[index] = true;
                }
            }
            self.held_moves.clear();
        }
        for index in 0..self.entries.len() {
            if visited[index] || (!has_move && self.entries[index].kind == OutboxKind::Move) {
                continue;
            }
            visited[index] = true;
            let entry = &self.entries[index];
            let held_move = self.held_moves.get(&entry.message_id);
            match entry.kind {
                OutboxKind::Move if held_move.is_some() => {}
                OutboxKind::Move if entry.retry_unix > self.now => {
                    self.held_moves
                        .insert(entry.message_id.clone(), MoveHold::EarlierMove);
                }
                OutboxKind::Move => self.replay_move(session, index, &mut visited)?,
                OutboxKind::AppendSent if entry.retry_unix <= self.now => {
                    self.replay_append(session, index)?;
                }
                OutboxKind::AppendSent => {}
                _ if matches!(held_move, Some(MoveHold::EarlierMove)) => {}
                OutboxKind::Seen | OutboxKind::Flagged => self.replay_flag(session, index)?,
                OutboxKind::Keyword | OutboxKind::BridgeLabel => {
                    self.replay_label(session, index)?;
                    if !self.settled[index] {
                        self.held_moves
                            .insert(entry.message_id.clone(), MoveHold::EarlierMove);
                    }
                }
            }
        }
        Ok(())
    }

    fn replay_append(&mut self, session: &mut WriteSession, index: usize) -> Result<(), Stop> {
        // Serialize recovery and transfer across app processes. Recheck the
        // row once we have the lock: another pass may already have sent it.
        let guard = self.store.try_lock_mailbox_operation(".append-sent")?;
        if guard.is_none() && self.store.db_path().is_some() {
            return Ok(());
        }
        let entry = &self.entries[index];
        let Some(pending) = outbox::pending_append(self.store, entry)? else {
            return Ok(());
        };
        match self.append(session, entry, &pending) {
            Ok(()) => {
                if outbox::complete_append(self.store, entry)? {
                    self.report.sent += 1;
                    self.settled[index] = true;
                }
                Ok(())
            }
            Err(Stop::Connection(err)) => {
                self.retry_append(index, &err.to_string(), AppendRetry::Deferred)?;
                Err(Stop::Connection(err))
            }
            Err(Stop::AppendRetry(message)) => {
                self.retry_append(index, &message, AppendRetry::Deferred)
            }
            Err(Stop::AppendRefused(message)) => {
                self.retry_append(index, &message, AppendRetry::Refused)
            }
            Err(Stop::AppendCheck(message)) => self.retry_append(
                index,
                &message,
                AppendRetry::Inconclusive {
                    checks: pending.checks,
                },
            ),
            Err(Stop::AppendCheckDisconnected(err)) => {
                self.retry_append(
                    index,
                    &err.to_string(),
                    AppendRetry::Inconclusive {
                        checks: pending.checks,
                    },
                )?;
                Err(Stop::Connection(err))
            }
            Err(stop) => Err(stop),
        }
    }

    fn append(
        &mut self,
        session: &mut WriteSession,
        entry: &OutboxEntry,
        pending: &outbox::PendingAppend,
    ) -> Result<(), Stop> {
        let (mailbox, bound) = if let Some(mailbox) = &pending.sent_to {
            // Refresh the selected UID state before checking for the copy.
            self.selected = None;
            let (validity, next) = session
                .select_with_next(mailbox)
                .map_err(append_check_error)?;
            self.selected = Some((mailbox.clone(), validity));
            let next = next.unwrap_or_default();
            if Self::check_append(session, entry, pending, mailbox, validity, next)? {
                return Ok(());
            }
            (mailbox.clone(), (validity, next))
        } else {
            self.load_listing(session)?;
            let mailbox = self
                .sent_mailbox
                .clone()
                .ok_or_else(|| Stop::AppendRetry("no Sent mailbox".into()))?;
            let bound = if self
                .selected
                .as_ref()
                .is_some_and(|(name, _)| name == &mailbox)
            {
                self.selected = None;
                match session.select_with_next(&mailbox) {
                    Ok((validity, next)) => {
                        self.selected = Some((mailbox.clone(), validity));
                        (validity, next.unwrap_or_default())
                    }
                    Err(WriteError::Refused(_)) => (0, 0),
                    Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
                }
            } else {
                match session.status(&mailbox) {
                    Ok(bound) => bound,
                    Err(WriteError::Refused(_)) => (0, 0),
                    Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
                }
            };
            (mailbox, bound)
        };
        let quoted_mailbox = quote_imap(&mailbox).map_err(Stop::AppendRetry)?;
        if !outbox::mark_append_sent(self.store, entry, &mailbox, bound)? {
            return Err(Stop::Stale);
        }
        match session.append_seen(&quoted_mailbox, &pending.mime) {
            Ok(()) => Ok(()),
            Err(WriteError::Refused(message)) => Err(Stop::AppendRefused(message)),
            Err(WriteError::Connection(error)) => Err(Stop::Connection(error)),
        }
    }

    /// True means the copy is present, false means it is safe to APPEND.
    fn check_append(
        session: &mut WriteSession,
        entry: &OutboxEntry,
        pending: &outbox::PendingAppend,
        mailbox: &str,
        validity: u32,
        next: u32,
    ) -> Result<bool, Stop> {
        let candidates = session
            .search_message_id(&entry.message_id, APPEND_UID_LIMIT)
            .map_err(append_check_error)?;
        if !candidates.is_empty() {
            let (found, _) =
                Self::check_append_headers(session, entry, mailbox, validity, &candidates)?;
            return if found {
                Ok(true)
            } else {
                Err(Stop::AppendCheck(
                    "Sent Message-ID check found no exact match".into(),
                ))
            };
        }
        if pending.validity != 0 && validity != pending.validity {
            return Ok(false);
        }
        if pending.next == 0 || next < pending.next {
            return Err(Stop::AppendCheck(
                "Sent UIDNEXT could not be verified".into(),
            ));
        }
        if next == pending.next {
            // A successful APPEND allocates a UID even if it is expunged later.
            return Ok(false);
        }
        if next - pending.next > APPEND_UID_LIMIT as u32 {
            return Err(Stop::AppendCheck(
                "Sent UID range is too large to verify".into(),
            ));
        }
        // UIDNEXT may also advance for unrelated arrivals. List the existing
        // UIDs in the closed range so gaps are distinguished from missing
        // FETCH responses, and check every header before sending again.
        let uids = session
            .search_uid_range(pending.next, next - 1, APPEND_UID_LIMIT)
            .map_err(append_check_error)?;
        let (found, complete) =
            Self::check_append_headers(session, entry, mailbox, validity, &uids)?;
        if found || complete {
            Ok(found)
        } else {
            Err(Stop::AppendCheck(
                "Sent UID range had missing headers".into(),
            ))
        }
    }

    fn check_append_headers(
        session: &mut WriteSession,
        entry: &OutboxEntry,
        mailbox: &str,
        validity: u32,
        uids: &[u32],
    ) -> Result<(bool, bool), Stop> {
        let mut complete = true;
        for batch in uids.chunks(APPEND_HEADER_BATCH) {
            let headers = session
                .uid_fetch_message_ids(batch)
                .map_err(append_check_error)?;
            let returned: HashSet<_> = headers.iter().map(|(uid, _)| *uid).collect();
            if headers.iter().any(|(uid, bytes)| {
                batch.binary_search(uid).is_ok()
                    && copy_message_id(mailbox, validity, *uid, bytes, false).as_str()
                        == entry.message_id
            }) {
                return Ok((true, true));
            }
            complete &= batch.iter().all(|uid| returned.contains(uid));
        }
        Ok((false, complete))
    }

    fn retry_append(
        &mut self,
        index: usize,
        message: &str,
        retry: AppendRetry,
    ) -> Result<(), Stop> {
        let entry = &self.entries[index];
        let attempts = match retry {
            AppendRetry::Deferred | AppendRetry::Refused => entry.attempts,
            AppendRetry::Inconclusive { checks } => checks,
        };
        match retry {
            AppendRetry::Refused if attempts + 1 >= MAX_ATTEMPTS => {
                return self.drop_row(index, DropReason::Refused, message);
            }
            AppendRetry::Inconclusive { .. } if attempts + 1 >= MAX_APPEND_CHECKS => {
                return self.drop_row(index, DropReason::UnconfirmedAppend, message);
            }
            _ => {}
        }
        let wait = BACKOFF_SECS[(attempts as usize).min(BACKOFF_SECS.len() - 1)];
        let refused = matches!(retry, AppendRetry::Refused);
        match outbox::settle(
            self.store,
            self.account_id,
            entry,
            None,
            None,
            Outcome::Retry {
                error: message,
                retry_unix: self.now + wait,
                counts: refused,
                refused,
                append_check: matches!(retry, AppendRetry::Inconclusive { .. }),
            },
        )? {
            Settled::Recorded => self.report.sent_append_delayed |= entry.error.is_empty(),
            Settled::Stale => return Err(Stop::Stale),
            Settled::Retired | Settled::Recovering => {}
        }
        Ok(())
    }

    fn replay_flag(&mut self, session: &mut WriteSession, index: usize) -> Result<(), Stop> {
        // Retry a safely rebased STORE once in this pass. A server that
        // keeps changing the copy uses the ordinary retry schedule.
        for _ in 0..2 {
            let Some(entry) = self.pending_flag(index)? else {
                return Ok(());
            };
            if !self.try_flag(session, index, entry)? {
                return Ok(());
            }
        }
        self.retry(
            index,
            DropReason::Refused,
            "flag revision kept changing during STORE",
        )
    }

    fn pending_flag(&mut self, index: usize) -> Result<Option<OutboxEntry>, Stop> {
        let entry = flags::pending(self.store, self.account_id, self.entries[index].id)?;
        if entry.is_none() {
            self.settled[index] = true;
        }
        Ok(entry)
    }

    fn store_unconfirmed(&self, mailbox: &str, validity: u32, uid: u32) -> bool {
        self.unconfirmed_stores
            .get(mailbox)
            .is_some_and(|copies| copies.contains(&(validity, uid)))
    }

    fn hold_unconfirmed_store(&mut self, entry: &OutboxEntry) {
        if entry.sent
            && let (Some(mailbox), Some(validity), Some(uid)) =
                (&entry.source_mailbox, entry.uidvalidity, entry.uid)
        {
            self.unconfirmed_stores
                .entry(mailbox.clone())
                .or_default()
                .insert((validity, uid));
        }
    }

    /// Send one conditional STORE attempt. Only a safe rebase asks for a
    /// second attempt; all other observations settle or use normal backoff.
    fn try_flag(
        &mut self,
        session: &mut WriteSession,
        index: usize,
        mut entry: OutboxEntry,
    ) -> Result<bool, Stop> {
        let (Some(mailbox), Some(validity), Some(uid), Some(value)) = (
            &entry.source_mailbox,
            entry.uidvalidity,
            entry.uid,
            entry.flag_value,
        ) else {
            return Err(Error::Action("invalid flag write in outbox".into()).into());
        };
        if self.store_unconfirmed(mailbox, validity, uid) {
            return Ok(false);
        }
        if entry.retry_unix > self.now {
            self.hold_unconfirmed_store(&entry);
            return Ok(false);
        }
        match self.select(session, mailbox)? {
            Ok(current) if current != validity => {
                self.drop_row(index, DropReason::FolderReset, "UIDVALIDITY changed")?;
                return Ok(false);
            }
            Err(message) => {
                self.retry(index, DropReason::Refused, &message)?;
                return Ok(false);
            }
            Ok(_) => {}
        }
        if entry.sent || (session.conditional() && entry.flag_modseq.is_none()) {
            let Some(current) = self.read_flags(session, index, uid)? else {
                return Ok(false);
            };
            if !self.observe_flag(
                index,
                &entry,
                Observation::Fetched(current, FetchReason::BeforeStore),
            )? {
                return Ok(false);
            }
            let Some(rebased) = self.pending_flag(index)? else {
                return Ok(false);
            };
            entry = rebased;
        }

        if !session.conditional() {
            entry.flag_modseq = None;
            entry.flag_base = None;
        }
        (self.before_send)();
        match flags::mark_sent(self.store, self.account_id, &entry)? {
            Settled::Stale => return Err(Stop::Stale),
            Settled::Retired => {
                self.settled[index] = true;
                return Ok(false);
            }
            Settled::Recorded | Settled::Recovering => {}
        }
        let reason = match session.uid_store_flag(uid, entry.kind, value, entry.flag_modseq) {
            Ok(FlagReply::Stored(None)) if session.conditional() => FetchReason::AfterStore,
            Ok(FlagReply::Stored(modseq)) => {
                return self.observe_flag(index, &entry, Observation::Stored(modseq));
            }
            Ok(FlagReply::Modified) => FetchReason::Modified,
            Err(WriteError::Refused(message)) => {
                self.retry_as(index, DropReason::Refused, &message, true)?;
                return Ok(false);
            }
            Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
        };
        let Some(current) = self.read_flags(session, index, uid)? else {
            return Ok(false);
        };
        self.observe_flag(index, &entry, Observation::Fetched(current, reason))
    }

    fn read_flags(
        &mut self,
        session: &mut WriteSession,
        index: usize,
        uid: u32,
    ) -> Result<Option<CopyFlags>, Stop> {
        match session.uid_copy_flags(uid) {
            Ok(Some(current)) if current.modseq.is_none() && session.conditional() => {
                self.retry(
                    index,
                    DropReason::Refused,
                    "server did not report the copy's MODSEQ",
                )?;
            }
            Ok(Some(current)) => return Ok(Some(current)),
            Ok(None) => {
                self.drop_row(index, DropReason::NotFound, "message is not on the server")?;
            }
            Err(WriteError::Refused(message)) => {
                self.retry(index, DropReason::Refused, &message)?;
            }
            Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
        }
        Ok(None)
    }

    /// True when the observation authorizes a STORE at the rebased MODSEQ.
    fn observe_flag(
        &mut self,
        index: usize,
        entry: &OutboxEntry,
        observation: Observation,
    ) -> Result<bool, Stop> {
        match flags::observe(self.store, self.account_id, entry, observation)? {
            Progress::Stale => return Err(Stop::Stale),
            Progress::Ready => return Ok(true),
            Progress::Unconfirmed => {
                self.retry(
                    index,
                    DropReason::Refused,
                    "FETCH did not report current flags and revision",
                )?;
                return Ok(false);
            }
            Progress::Retired => {}
            Progress::Applied => self.report.sent += 1,
            Progress::Conflict { changed } => self.report.flags_changed |= changed,
        }
        self.settled[index] = true;
        Ok(false)
    }

    /// Send the move at `index`, with every later row that moves another
    /// message's copy between the same two mailboxes.
    fn replay_move(
        &mut self,
        session: &mut WriteSession,
        index: usize,
        visited: &mut [bool],
    ) -> Result<(), Stop> {
        let entries = self.entries;
        let entry = &entries[index];
        // Undo's reverse of a row that had not settled: the copy to move
        // back is the one that row placed.
        let reversing = match entry
            .reverses
            .map(|id| outbox::reversed(self.store, id))
            .transpose()?
        {
            None => None,
            Some(Reversed::Placed(uidvalidity, uid)) => Some((uidvalidity, uid)),
            Some(Reversed::Waiting) => {
                self.held_moves
                    .insert(entry.message_id.clone(), MoveHold::EarlierMove);
                return Ok(());
            }
            Some(Reversed::Gone) => {
                self.drop_quietly(index, "the move it reverses never reached the server")?;
                return Ok(());
            }
        };
        let Some(source) = self.mailbox(
            session,
            entry.source_mailbox.as_deref(),
            &entry.source_folder,
        )?
        else {
            return self.retry(index, DropReason::NotFound, "folder is not on the server");
        };
        let Some(target) = self.mailbox(
            session,
            entry.target_mailbox.as_deref(),
            &entry.target_folder,
        )?
        else {
            return self.retry(index, DropReason::NotFound, "folder is not on the server");
        };
        let roles = self.store.folder_roles(self.account_id)?;
        for (folder, raw) in [
            (&entry.source_folder, &source),
            (&entry.target_folder, &target),
        ] {
            if roles.is_view(folder)
                || crate::store::views::has_view(
                    self.store.connection(),
                    self.account_id,
                    &super::escape_mailbox(raw),
                )?
            {
                return self.drop_row(
                    index,
                    DropReason::Refused,
                    "virtual mailbox cannot be moved",
                );
            }
        }
        let uidvalidity = match self.select(session, &source)? {
            Ok(uidvalidity) => uidvalidity,
            Err(message) => return self.retry(index, DropReason::Refused, &message),
        };
        let uid = match reversing.or(entry.uidvalidity.zip(entry.uid)) {
            Some((queued, uid)) if queued == uidvalidity => uid,
            Some(_) => {
                return self.drop_row(index, DropReason::FolderReset, "UIDVALIDITY changed");
            }
            None => {
                let found = outbox::find_copy(
                    self.store,
                    self.account_id,
                    &entry.message_id,
                    &source,
                    uidvalidity,
                )?;
                match found {
                    Some(uid) => uid,
                    None => {
                        return self.retry(
                            index,
                            DropReason::NotFound,
                            "message is not on the server there yet",
                        );
                    }
                }
            }
        };
        if self.store_unconfirmed(&source, uidvalidity, uid) {
            // Moving this copy would replace the UID and revision needed
            // to confirm the earlier STORE.
            self.held_moves
                .insert(entry.message_id.clone(), MoveHold::StoreRecovery);
            return Ok(());
        }

        // Later rows that can go in the same command.
        let mut batch = vec![(index, uid)];
        let mut waiting: HashSet<&str> = HashSet::new();
        for later in index + 1..self.entries.len() {
            if visited[later] {
                continue;
            }
            let other = &entries[later];
            let fits = !self.held_moves.contains_key(&other.message_id)
                && !waiting.contains(other.message_id.as_str())
                && !batch
                    .iter()
                    .any(|(taken, _)| entries[*taken].message_id == other.message_id)
                && other.retry_unix <= self.now
                && other.kind == entry.kind
                && other.source_mailbox.as_deref() == Some(source.as_str())
                && other.uidvalidity == Some(uidvalidity)
                && other
                    .uid
                    .is_some_and(|uid| !self.store_unconfirmed(&source, uidvalidity, uid))
                && self.known_target(other).as_deref() == Some(target.as_str());
            if fits {
                visited[later] = true;
                batch.push((later, other.uid.unwrap_or_default()));
            } else {
                waiting.insert(other.message_id.as_str());
            }
        }
        // What the target holds now, so a later look for the moved copy
        // there trusts only mail that arrived after this.
        let before = match session.status(&target) {
            Ok(before) => before,
            Err(WriteError::Refused(message)) => return self.retry_all(&batch, &message, false),
            Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
        };
        (self.before_send)();
        let ids: Vec<i64> = batch.iter().map(|(row, _)| entries[*row].id).collect();
        // Undo, or turning write-back off, may have retired rows since the
        // pass loaded them: only the rows still pending now are sent.
        let marked = outbox::mark_sent(self.store, self.account_id, &ids, before)?;
        batch.retain(|(row, _)| marked.iter().any(|(id, _)| *id == entries[*row].id));
        self.bounds.extend(marked);
        if batch.is_empty() {
            return Ok(());
        }
        self.send_move(session, &batch, (&source, uidvalidity), &target)
    }

    /// `UID MOVE` the batch. A row the reply does not place is looked for
    /// in the target; one not found there waits for another try.
    fn send_move(
        &mut self,
        session: &mut WriteSession,
        batch: &[(usize, u32)],
        (source, uidvalidity): (&str, u32),
        target: &str,
    ) -> Result<(), Stop> {
        let uids: Vec<u32> = batch.iter().map(|(_, uid)| *uid).collect();
        let placed = match session.uid_move(&uids, target) {
            Ok(placed) => placed,
            // Not moved: the next try is a first send again.
            Err(WriteError::Refused(message)) => return self.retry_all(batch, &message, true),
            Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
        };
        let mut placements: Vec<Option<(u32, u32)>> = batch
            .iter()
            .map(|(_, uid)| placed_for(&placed, *uid))
            .collect();
        for at in 0..batch.len() {
            if placements[at].is_none() {
                placements[at] = self.locate(session, batch[at].0, target, &placements)?;
            }
        }
        for (at, (row, uid)) in batch.iter().enumerate() {
            // Moved, maybe, but to no copy anyone can name: retry, then drop.
            let Some(placed) = placements[at] else {
                self.retry(
                    *row,
                    DropReason::NotFound,
                    "message is not in the target on the server",
                )?;
                continue;
            };
            self.settle(
                *row,
                (source, uidvalidity, *uid),
                target,
                Outcome::Done { placed },
            )?;
            self.settled[*row] = true;
            self.report.sent += 1;
        }
        Ok(())
    }

    /// The copy of the row's message the move put in `target`, when the
    /// server did not say. Only a UID at or above the target's UIDNEXT from
    /// before the first MOVE counts, under the same UIDVALIDITY, and only
    /// when its fetched Message-ID header is exactly the row's: `HEADER`
    /// search is a substring match and ignores case. The lowest such UID no
    /// other row moves is taken. `None` when nothing qualifies, the search
    /// or fetch is refused, the bound is unknown, or the message has no
    /// Message-ID to search for.
    fn locate(
        &mut self,
        session: &mut WriteSession,
        row: usize,
        target: &str,
        taken: &[Option<(u32, u32)>],
    ) -> Result<Option<(u32, u32)>, Stop> {
        let entries = self.entries;
        let entry = &entries[row];
        if is_synthetic_message_id(&entry.message_id) {
            return Ok(None);
        }
        let Some(&Some((bound_uidvalidity, uidnext))) = self.bounds.get(&entry.id) else {
            return Ok(None);
        };
        let uidvalidity = match self.select(session, target)? {
            Ok(uidvalidity) if uidvalidity == bound_uidvalidity => uidvalidity,
            Ok(_) | Err(_) => return Ok(None),
        };
        let found = match session.search_message_id(&entry.message_id, usize::MAX) {
            Ok(found) => found,
            Err(WriteError::Refused(_)) => return Ok(None),
            Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
        };
        let mut candidates = Vec::new();
        for uid in found {
            if uid < uidnext
                || taken.contains(&Some((uidvalidity, uid)))
                || outbox::is_claimed(self.store, self.account_id, target, uidvalidity, uid)?
            {
                continue;
            }
            candidates.push(uid);
        }
        if candidates.is_empty() {
            return Ok(None);
        }
        let headers = match session.uid_fetch_message_ids(&candidates) {
            Ok(headers) => headers,
            Err(WriteError::Refused(_)) => return Ok(None),
            Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
        };
        let exact = headers
            .iter()
            .filter(|(uid, header)| {
                candidates.contains(uid)
                    && copy_message_id(target, uidvalidity, *uid, header, false).as_str()
                        == entry.message_id
            })
            .map(|(uid, _)| *uid)
            .min();
        Ok(exact.map(|uid| (uidvalidity, uid)))
    }

    /// `refused`: the server answered the MOVE itself with NO or BAD.
    fn retry_all(
        &mut self,
        batch: &[(usize, u32)],
        message: &str,
        refused: bool,
    ) -> Result<(), Stop> {
        for (row, _) in batch {
            self.retry_as(*row, DropReason::Refused, message, refused)?;
        }
        Ok(())
    }

    fn settle(
        &mut self,
        index: usize,
        (source, uidvalidity, uid): (&str, u32, u32),
        target: &str,
        outcome: Outcome<'_>,
    ) -> Result<(), Stop> {
        match outbox::settle(
            self.store,
            self.account_id,
            &self.entries[index],
            Some((source, uidvalidity, uid)),
            Some(target),
            outcome,
        )? {
            Settled::Stale => Err(Stop::Stale),
            Settled::Recorded | Settled::Recovering | Settled::Retired => Ok(()),
        }
    }

    /// The raw name: as queued, else from the server's LIST.
    fn mailbox(
        &mut self,
        session: &mut WriteSession,
        queued: Option<&str>,
        folder: &str,
    ) -> Result<Option<String>, Stop> {
        if let Some(raw) = queued {
            return Ok(Some(raw.to_string()));
        }
        self.load_listing(session)?;
        Ok(self.listed(folder))
    }

    fn load_listing(&mut self, session: &mut WriteSession) -> Result<(), Stop> {
        if self.listing.is_none() {
            let listed = session.list().map_err(Stop::Connection)?;
            self.sent_mailbox = FolderRoles::detect(&listed_mailboxes(&listed), self.overrides)
                .get(FolderRole::Sent)
                .map(|folder| folder.raw.clone());
            self.listing = Some(
                listed
                    .into_iter()
                    .map(|mailbox| (mailbox.decoded, mailbox.raw))
                    .collect(),
            );
        }
        Ok(())
    }

    fn listed(&self, folder: &str) -> Option<String> {
        let listing = self.listing.as_ref()?;
        listing.get(folder).cloned().or_else(|| {
            listing
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(folder))
                .map(|(_, raw)| raw.clone())
        })
    }

    fn known_target(&self, entry: &OutboxEntry) -> Option<String> {
        entry
            .target_mailbox
            .clone()
            .or_else(|| self.listed(&entry.target_folder))
    }

    /// `Ok(Err(message))` when the server refused the mailbox.
    fn select(
        &mut self,
        session: &mut WriteSession,
        mailbox: &str,
    ) -> Result<Result<u32, String>, Stop> {
        if let Some((selected, uidvalidity)) = &self.selected
            && selected == mailbox
        {
            return Ok(Ok(*uidvalidity));
        }
        self.selected = None;
        match session.select(mailbox) {
            Ok(uidvalidity) => {
                self.selected = Some((mailbox.to_string(), uidvalidity));
                Ok(Ok(uidvalidity))
            }
            Err(WriteError::Refused(message)) => Ok(Err(message)),
            Err(WriteError::Connection(err)) => Err(Stop::Connection(err)),
        }
    }

    /// Try the row again later, or drop it once it has used its attempts.
    fn retry(&mut self, index: usize, reason: DropReason, message: &str) -> Result<(), Stop> {
        self.retry_as(index, reason, message, false)
    }

    fn retry_as(
        &mut self,
        index: usize,
        reason: DropReason,
        message: &str,
        refused: bool,
    ) -> Result<(), Stop> {
        let entries = self.entries;
        let entry = &entries[index];
        let last = entry.attempts + 1 >= MAX_ATTEMPTS;
        // A MOVE refusal can need to retire a queued undo. A flag retry can
        // still be recovering an earlier STORE for a newer edit.
        if last && !refused && entry.kind == OutboxKind::Move {
            return self.drop_row(index, reason, message);
        }
        let wait = BACKOFF_SECS[(entry.attempts as usize).min(BACKOFF_SECS.len() - 1)];
        let settled = outbox::settle(
            self.store,
            self.account_id,
            entry,
            None,
            None,
            Outcome::Retry {
                error: message,
                retry_unix: self.now + wait,
                counts: true,
                refused,
                append_check: false,
            },
        )?;
        match settled {
            Settled::Stale => return Err(Stop::Stale),
            // Undone, and the server never moved it: it and its reverse are
            // settled, with nothing to say.
            Settled::Retired => {
                for (at, other) in entries.iter().enumerate() {
                    if at == index || other.reverses == Some(entry.id) {
                        self.settled[at] = true;
                    }
                }
                if entry.kind != OutboxKind::Move {
                    return Ok(());
                }
            }
            Settled::Recorded if last => return self.drop_row(index, reason, message),
            Settled::Recovering if entry.attempts + 1 == MAX_ATTEMPTS => {
                self.report.flag_recovery_delayed = true;
            }
            Settled::Recorded | Settled::Recovering => {}
        }
        if entry.kind == OutboxKind::Move {
            self.held_moves
                .insert(entry.message_id.clone(), MoveHold::EarlierMove);
        } else if matches!(entry.kind, OutboxKind::Seen | OutboxKind::Flagged)
            && !refused
            && let Some(current) = self.pending_flag(index)?
        {
            self.hold_unconfirmed_store(&current);
        }
        Ok(())
    }

    fn drop_row(&mut self, index: usize, reason: DropReason, message: &str) -> Result<(), Stop> {
        if self.drop_quietly(index, message)? == Settled::Retired {
            return Ok(());
        }
        let entry = &self.entries[index];
        self.report.dropped.push(DroppedWrite {
            kind: entry.kind,
            source_folder: entry.source_folder.clone(),
            target_folder: entry.target_folder.clone(),
            reason,
        });
        Ok(())
    }

    /// Drop the row without a notice: the user has nothing to learn from it.
    fn drop_quietly(&mut self, index: usize, message: &str) -> Result<Settled, Stop> {
        let entries = self.entries;
        let entry = &entries[index];
        let settled = outbox::settle(
            self.store,
            self.account_id,
            entry,
            None,
            None,
            Outcome::Dropped { error: message },
        )?;
        if settled == Settled::Stale {
            return Err(Stop::Stale);
        }
        self.settled[index] = true;
        if entry.kind == OutboxKind::Move {
            self.held_moves
                .insert(entry.message_id.clone(), MoveHold::EarlierMove);
        }
        Ok(settled)
    }
}

fn placed_for(placed: &Placed, uid: u32) -> Option<(u32, u32)> {
    let uidvalidity = placed.uidvalidity?;
    placed
        .uids
        .iter()
        .find(|(from, _)| *from == uid)
        .map(|(_, to)| (uidvalidity, *to))
}
