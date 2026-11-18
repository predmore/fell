//! Replay of the write-back outbox ([`crate::outbox`]) on its own session.
//!
//! One pass loads the rows not yet applied and, only if there are any,
//! logs in on a [`WriteSession`] (never the read-only sync session). For
//! each source mailbox it sends `SELECT`, checks UIDVALIDITY, and moves the
//! queued UIDs with one `UID MOVE` per target. A server without MOVE but
//! with UIDPLUS gets `UID COPY`, then `UID STORE +FLAGS.SILENT (\Deleted)`
//! and `UID EXPUNGE` of the same UIDs; the copy is recorded first, so a
//! retry does not copy twice. A server with neither keeps the change local.
//!
//! A row is marked sent before its `UID MOVE` or `UID COPY` goes out. When
//! the reply is lost the server may have acted, so a retry never trusts a
//! missing `COPYUID`: after a move it looks for the message in the target
//! by Message-ID (`UID SEARCH HEADER Message-ID`) and records that UID, or
//! leaves the row to retry; before copying again it looks there first, so
//! the message is not copied twice. Mail with no Message-ID cannot be
//! looked for: a move of it is recorded without its new UID, and a copy
//! whose reply was lost is sent again.
//!
//! A refusal (NO or BAD) is retried with backoff and the row is dropped
//! after [`MAX_ATTEMPTS`]. A dropped connection ends the pass and costs the
//! rows nothing; the next pass tries again. Rows for one message go in the
//! order they were queued: one that waits holds back the rest.

use std::collections::{HashMap, HashSet};

use crate::Error;
use crate::account::AccountId;
use crate::outbox::{
    self, DropReason, DroppedMove, OutboxEntry, OutboxKind, OutboxState, Outcome, ReplayReport,
};
use crate::store::Store;

use super::ImapConfig;
use super::rfc822::is_uid_message_id;
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
    report: &'a mut ReplayReport,
}

impl Pass<'_> {
    fn run(&mut self, session: &mut WriteSession) -> Result<(), Stop> {
        if !session.has_move() && !session.has_uidplus() {
            for index in 0..self.entries.len() {
                if self.entries[index].retry_unix <= self.now {
                    self.drop_row(index, DropReason::NoMove, "server has no MOVE or UIDPLUS")?;
                }
            }
            return Ok(());
        }
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
        let uid = match (entry.uidvalidity, entry.uid) {
            (Some(queued), Some(uid)) if queued == uidvalidity => uid,
            (Some(_), Some(_)) => {
                return self.drop_row(index, DropReason::FolderReset, "UIDVALIDITY changed");
            }
            _ => {
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
                && other.state == entry.state
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
        if entry.state == OutboxState::Copied {
            return self.finish_copied(session, &batch, (&source, uidvalidity), &target);
        }
        let ids: Vec<i64> = batch.iter().map(|(row, _)| entries[*row].id).collect();
        outbox::mark_sent(self.store, &ids)?;
        if session.has_move() {
            self.send_move(session, &batch, (&source, uidvalidity), &target)
        } else {
            self.send_copy(session, &batch, (&source, uidvalidity), &target)
        }
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
            Err(WriteError::Refused(message)) => return self.retry_all(batch, &message),
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
            let placed = placements[at];
            if placed.is_none() && !is_uid_message_id(&self.entries[*row].message_id) {
                self.retry(
                    *row,
                    DropReason::NotFound,
                    "message is not in the target on the server",
                )?;
                continue;
            }
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

    /// `UID COPY` the batch, record each copy, then remove the source
    /// copies. A row sent before whose reply was lost is looked for in the
    /// target first, so it is not copied twice.
    fn send_copy(
        &mut self,
        session: &mut WriteSession,
        batch: &[(usize, u32)],
        (source, uidvalidity): (&str, u32),
        target: &str,
    ) -> Result<(), Stop> {
        let entries = self.entries;
        let mut placements: Vec<Option<(u32, u32)>> = vec![None; batch.len()];
        let mut looked = false;
        for at in 0..batch.len() {
            if entries[batch[at].0].sent {
                looked = true;
                placements[at] = self.locate(session, batch[at].0, target, &placements)?;
            }
        }
        if looked {
            match self.reselect(session, source, uidvalidity)? {
                Ok(()) => {}
                Err(message) => return self.retry_all(batch, &message),
            }
        }
        let to_copy: Vec<u32> = batch
            .iter()
            .zip(&placements)
            .filter(|(_, placed)| placed.is_none())
            .map(|((_, uid), _)| *uid)
            .collect();
        if !to_copy.is_empty() {
            let placed = match session.uid_copy(&to_copy, target) {
                Ok(placed) => placed,
                Err(WriteError::Refused(message)) => return self.retry_all(batch, &message),
                Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
            };
            for (at, (_, uid)) in batch.iter().enumerate() {
                if placements[at].is_none() {
                    placements[at] = placed_for(&placed, *uid);
                }
            }
        }
        // A UID the copy did not place was not in the source any more: look
        // for it in the target, and leave it out of the removal.
        let mut unplaced = false;
        for at in 0..batch.len() {
            if placements[at].is_none() {
                placements[at] = self.locate(session, batch[at].0, target, &placements)?;
                unplaced = true;
            }
        }
        let mut copied = Vec::new();
        for (at, (row, uid)) in batch.iter().enumerate() {
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
                Outcome::Copied {
                    placed: Some(placed),
                },
            )?;
            let mut entry = entries[*row].clone();
            entry.state = OutboxState::Copied;
            entry.placed_uidvalidity = Some(placed.0);
            entry.placed_uid = Some(placed.1);
            copied.push((*row, *uid, entry));
        }
        if copied.is_empty() {
            return Ok(());
        }
        if unplaced {
            match self.reselect(session, source, uidvalidity)? {
                Ok(()) => {}
                // Recorded as copied: the next pass removes the source copy.
                Err(_) => return Ok(()),
            }
        }
        let uids: Vec<u32> = copied.iter().map(|(_, uid, _)| *uid).collect();
        match session.uid_remove(&uids) {
            Ok(()) => {
                for (row, uid, entry) in &copied {
                    self.settle_entry(
                        *row,
                        entry,
                        (source, uidvalidity, *uid),
                        target,
                        Outcome::Done {
                            placed: entry.placed_uidvalidity.zip(entry.placed_uid),
                        },
                    )?;
                }
                Ok(())
            }
            Err(WriteError::Refused(message)) => {
                for (row, _, _) in &copied {
                    self.retry(*row, DropReason::Refused, &message)?;
                }
                Ok(())
            }
            Err(WriteError::Connection(err)) => Err(Stop::Connection(err)),
        }
    }

    /// Rows already copied on a server without MOVE: remove the source copy.
    fn finish_copied(
        &mut self,
        session: &mut WriteSession,
        batch: &[(usize, u32)],
        (source, uidvalidity): (&str, u32),
        target: &str,
    ) -> Result<(), Stop> {
        let entries = self.entries;
        let uids: Vec<u32> = batch.iter().map(|(_, uid)| *uid).collect();
        match session.uid_remove(&uids) {
            Ok(()) => {
                for (row, uid) in batch {
                    let entry = &entries[*row];
                    self.settle_entry(
                        *row,
                        entry,
                        (source, uidvalidity, *uid),
                        target,
                        Outcome::Done {
                            placed: entry.placed_uidvalidity.zip(entry.placed_uid),
                        },
                    )?;
                }
                Ok(())
            }
            Err(WriteError::Refused(message)) => self.retry_all(batch, &message),
            Err(WriteError::Connection(err)) => Err(Stop::Connection(err)),
        }
    }

    /// The copy of the row's message a move or copy put in `target`, when
    /// the server did not say: the newest one there with its Message-ID
    /// that no other row moves or was just placed. `None` when it is not
    /// there, or the message has no Message-ID to search for.
    fn locate(
        &mut self,
        session: &mut WriteSession,
        row: usize,
        target: &str,
        taken: &[Option<(u32, u32)>],
    ) -> Result<Option<(u32, u32)>, Stop> {
        let entries = self.entries;
        let message_id = &entries[row].message_id;
        if is_uid_message_id(message_id) {
            return Ok(None);
        }
        let uidvalidity = match self.select(session, target)? {
            Ok(uidvalidity) => uidvalidity,
            Err(_) => return Ok(None),
        };
        let found = match session.search_message_id(message_id) {
            Ok(found) => found,
            Err(WriteError::Refused(_)) => return Ok(None),
            Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
        };
        for uid in found.into_iter().rev() {
            if taken.contains(&Some((uidvalidity, uid)))
                || outbox::is_claimed(self.store, self.account_id, target, uidvalidity, uid)?
            {
                continue;
            }
            return Ok(Some((uidvalidity, uid)));
        }
        Ok(None)
    }

    /// `SELECT` the source again after looking in the target. `Err` when it
    /// is refused or its UIDVALIDITY changed in between.
    fn reselect(
        &mut self,
        session: &mut WriteSession,
        source: &str,
        uidvalidity: u32,
    ) -> Result<Result<(), String>, Stop> {
        match self.select(session, source)? {
            Ok(now) if now == uidvalidity => Ok(Ok(())),
            Ok(_) => Ok(Err("UIDVALIDITY changed".to_string())),
            Err(message) => Ok(Err(message)),
        }
    }

    fn retry_all(&mut self, batch: &[(usize, u32)], message: &str) -> Result<(), Stop> {
        for (row, _) in batch {
            self.retry(*row, DropReason::Refused, message)?;
        }
        Ok(())
    }

    /// Settle `entry` (the row at `index`, as it stands now) as sent.
    fn settle_entry(
        &mut self,
        index: usize,
        entry: &OutboxEntry,
        (source, uidvalidity, uid): (&str, u32, u32),
        target: &str,
        outcome: Outcome<'_>,
    ) -> Result<(), Stop> {
        if !outbox::settle(
            self.store,
            self.account_id,
            entry,
            Some((source, uidvalidity, uid)),
            Some(target),
            outcome,
        )? {
            return Err(Stop::Stale);
        }
        self.settled[index] = true;
        self.report.sent += 1;
        Ok(())
    }

    fn settle(
        &mut self,
        index: usize,
        (source, uidvalidity, uid): (&str, u32, u32),
        target: &str,
        outcome: Outcome<'_>,
    ) -> Result<(), Stop> {
        if !outbox::settle(
            self.store,
            self.account_id,
            &self.entries[index],
            Some((source, uidvalidity, uid)),
            Some(target),
            outcome,
        )? {
            return Err(Stop::Stale);
        }
        Ok(())
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
        let entries = self.entries;
        let entry = &entries[index];
        if entry.attempts + 1 >= MAX_ATTEMPTS {
            return self.drop_row(index, reason, message);
        }
        let wait = BACKOFF_SECS[(entry.attempts as usize).min(BACKOFF_SECS.len() - 1)];
        let live = outbox::settle(
            self.store,
            self.account_id,
            entry,
            None,
            None,
            Outcome::Retry {
                error: message,
                retry_unix: self.now + wait,
                counts: true,
            },
        )?;
        if !live {
            return Err(Stop::Stale);
        }
        self.held.insert(entry.message_id.clone());
        Ok(())
    }

    fn drop_row(&mut self, index: usize, reason: DropReason, message: &str) -> Result<(), Stop> {
        let entries = self.entries;
        let entry = &entries[index];
        if !outbox::settle(
            self.store,
            self.account_id,
            entry,
            None,
            None,
            Outcome::Dropped { error: message },
        )? {
            return Err(Stop::Stale);
        }
        self.settled[index] = true;
        self.held.insert(entry.message_id.clone());
        self.report.dropped.push(DroppedMove {
            source_folder: entry.source_folder.clone(),
            target_folder: entry.target_folder.clone(),
            reason,
        });
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
