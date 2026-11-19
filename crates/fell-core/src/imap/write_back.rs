//! Replay of the write-back outbox ([`crate::outbox`]) on its own session.
//!
//! One pass loads the rows not yet applied and, only if there are any,
//! logs in on a [`WriteSession`] (never the read-only sync session). For
//! each source mailbox it sends `SELECT`, checks UIDVALIDITY, and moves the
//! queued UIDs with one `UID MOVE` per target. A server without MOVE keeps
//! the change in Fell: nothing is copied, flagged, or expunged, and the
//! user is told once.
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
//! rows nothing; the next pass tries again. Rows for one message go in the
//! order they were queued: one that waits holds back the rest.
//!
//! Undo's reverse of a row that was sent but not settled waits behind it,
//! then moves the copy that row placed. If that row never reached the
//! server (refused, or given up), the reverse is dropped without a notice.

use std::collections::{HashMap, HashSet};

use crate::Error;
use crate::account::AccountId;
use crate::outbox::{
    self, DropReason, DroppedMove, OutboxEntry, OutboxKind, Outcome, ReplayReport, Reversed,
    Settled,
};
use crate::store::Store;

use super::ImapConfig;
use super::rfc822::{copy_message_id, is_uid_message_id};
use super::session::{Placed, WriteError, WriteSession};

/// Refusals (or "not on the server yet") before a row is dropped.
pub(crate) const MAX_ATTEMPTS: u32 = 5;
/// Wait before the next try, by attempts so far. The last one repeats.
const BACKOFF_SECS: [i64; 5] = [10, 30, 120, 600, 1800];

/// Send every outbox row that is due. Returns what was sent and dropped.
/// A connection that fails part way is in [`ReplayReport::interrupted`];
/// what was settled before it stays settled.
pub(crate) fn replay(
    config: &ImapConfig,
    account_id: &AccountId,
    store: &Store,
) -> crate::Result<ReplayReport> {
    replay_with(config, account_id, store, &|| {})
}

/// [`replay`], running `before_send` after the pass loaded its rows and
/// just before it marks a batch sent. Tests change the store there.
pub(super) fn replay_with(
    config: &ImapConfig,
    account_id: &AccountId,
    store: &Store,
    before_send: &dyn Fn(),
) -> crate::Result<ReplayReport> {
    let entries = outbox::unfinished(store, account_id)?;
    let mut report = ReplayReport::default();
    if entries.is_empty() {
        return Ok(report);
    }
    let now = outbox::now_unix();
    if entries.iter().all(|entry| entry.retry_unix > now) {
        report.waiting = entries.len();
        return Ok(report);
    }
    let mut session = WriteSession::open(config)?;
    let mut pass = Pass {
        store,
        account_id,
        now,
        entries: &entries,
        settled: vec![false; entries.len()],
        held: HashSet::new(),
        listing: None,
        selected: None,
        bounds: HashMap::new(),
        before_send,
        report: &mut report,
    };
    let result = pass.run(&mut session);
    let settled = pass.settled.iter().filter(|done| **done).count();
    session.logout();
    match result {
        Ok(()) => {}
        Err(Stop::Store(err)) => return Err(err),
        Err(Stop::Connection(err)) => report.interrupted = Some(err.to_string()),
        // Write-back was turned off by another process mid-pass.
        Err(Stop::Stale) => {}
    }
    report.waiting = entries.len() - settled;
    Ok(report)
}

enum Stop {
    Connection(Error),
    Store(Error),
    /// The row is no longer in this write-back generation: send nothing more.
    Stale,
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
    /// Messages with a row that did not finish: their later rows wait.
    held: HashSet<String>,
    /// Folder name → raw mailbox name, from LIST when first needed.
    listing: Option<HashMap<String, String>>,
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
        if !session.has_move() {
            for index in 0..self.entries.len() {
                if self.entries[index].retry_unix <= self.now {
                    self.drop_row(index, DropReason::NoMove, "server has no MOVE")?;
                }
            }
            return Ok(());
        }
        outbox::note_move_missing(self.store, self.account_id, false)?;
        let mut visited = vec![false; self.entries.len()];
        for index in 0..self.entries.len() {
            if visited[index] {
                continue;
            }
            visited[index] = true;
            let entry = &self.entries[index];
            if self.held.contains(&entry.message_id) || entry.retry_unix > self.now {
                self.held.insert(entry.message_id.clone());
                continue;
            }
            match entry.kind {
                OutboxKind::Move => self.replay_move(session, index, &mut visited)?,
            }
        }
        Ok(())
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
                self.held.insert(entry.message_id.clone());
                return Ok(());
            }
            Some(Reversed::Gone) => {
                return self.drop_quietly(index, "the move it reverses never reached the server");
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

        // Later rows that can go in the same command.
        let mut batch = vec![(index, uid)];
        let mut waiting: HashSet<&str> = HashSet::new();
        for later in index + 1..self.entries.len() {
            if visited[later] {
                continue;
            }
            let other = &entries[later];
            let fits = !self.held.contains(&other.message_id)
                && !waiting.contains(other.message_id.as_str())
                && !batch
                    .iter()
                    .any(|(taken, _)| entries[*taken].message_id == other.message_id)
                && other.retry_unix <= self.now
                && other.kind == entry.kind
                && other.source_mailbox.as_deref() == Some(source.as_str())
                && other.uidvalidity == Some(uidvalidity)
                && other.uid.is_some()
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
        if is_uid_message_id(&entry.message_id) {
            return Ok(None);
        }
        let Some(&Some((bound_uidvalidity, uidnext))) = self.bounds.get(&entry.id) else {
            return Ok(None);
        };
        let uidvalidity = match self.select(session, target)? {
            Ok(uidvalidity) if uidvalidity == bound_uidvalidity => uidvalidity,
            Ok(_) | Err(_) => return Ok(None),
        };
        let found = match session.search_message_id(&entry.message_id) {
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
                    && copy_message_id(target, uidvalidity, *uid, header).as_str()
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
            Settled::Recorded | Settled::Retired => Ok(()),
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
        if self.listing.is_none() {
            let listed = session.list().map_err(Stop::Connection)?;
            self.listing = Some(
                listed
                    .into_iter()
                    .map(|mailbox| (mailbox.decoded, mailbox.raw))
                    .collect(),
            );
        }
        Ok(self.listed(folder))
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
        // A refusal is recorded even on the last try: if undo reversed the
        // row, it is retired there without a notice.
        if last && !refused {
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
            }
            Settled::Recorded if last => return self.drop_row(index, reason, message),
            Settled::Recorded => {}
        }
        self.held.insert(entry.message_id.clone());
        Ok(())
    }

    fn drop_row(&mut self, index: usize, reason: DropReason, message: &str) -> Result<(), Stop> {
        self.drop_quietly(index, message)?;
        let entry = &self.entries[index];
        self.report.dropped.push(DroppedMove {
            source_folder: entry.source_folder.clone(),
            target_folder: entry.target_folder.clone(),
            reason,
        });
        Ok(())
    }

    /// Drop the row without a notice: the user has nothing to learn from it.
    fn drop_quietly(&mut self, index: usize, message: &str) -> Result<(), Stop> {
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
        self.held.insert(entry.message_id.clone());
        Ok(())
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
