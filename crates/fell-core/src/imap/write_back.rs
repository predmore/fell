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
    }
    report.waiting = entries.len() - settled;
    Ok(report)
}

enum Stop {
    Connection(Error),
    Store(Error),
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
        let uids: Vec<u32> = batch.iter().map(|(_, uid)| *uid).collect();

        let copied = entry.state == OutboxState::Copied;
        let sent = if copied {
            session.uid_remove(&uids).map(|()| None)
        } else if session.has_move() {
            session.uid_move(&uids, &target).map(Some)
        } else {
            match session.uid_copy(&uids, &target) {
                Ok(placed) => {
                    for (row, uid) in &batch {
                        let outcome = Outcome::Copied {
                            placed: placed_for(&placed, *uid),
                        };
                        self.settle(*row, (&source, uidvalidity, *uid), &target, outcome)?;
                    }
                    session.uid_remove(&uids).map(|()| Some(placed))
                }
                Err(err) => Err(err),
            }
        };
        match sent {
            Ok(placed) => {
                for (row, uid) in &batch {
                    let placed = match &placed {
                        Some(placed) => placed_for(placed, *uid),
                        None => {
                            let entry = &entries[*row];
                            entry.placed_uidvalidity.zip(entry.placed_uid)
                        }
                    };
                    let mut entry = entries[*row].clone();
                    if !copied && !session.has_move() {
                        // Settled as Copied above; the row now says so.
                        entry.state = OutboxState::Copied;
                    }
                    outbox::settle(
                        self.store,
                        self.account_id,
                        &entry,
                        Some((&source, uidvalidity, *uid)),
                        Some(&target),
                        Outcome::Done { placed },
                    )?;
                    self.settled[*row] = true;
                    self.report.sent += 1;
                }
                Ok(())
            }
            Err(WriteError::Refused(message)) => {
                for (row, _) in &batch {
                    self.retry(*row, DropReason::Refused, &message)?;
                }
                Ok(())
            }
            Err(WriteError::Connection(err)) => Err(Stop::Connection(err)),
        }
    }

    fn settle(
        &mut self,
        index: usize,
        (source, uidvalidity, uid): (&str, u32, u32),
        target: &str,
        outcome: Outcome<'_>,
    ) -> Result<(), Stop> {
        outbox::settle(
            self.store,
            self.account_id,
            &self.entries[index],
            Some((source, uidvalidity, uid)),
            Some(target),
            outcome,
        )?;
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
        outbox::settle(
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
        self.held.insert(entry.message_id.clone());
        Ok(())
    }

    fn drop_row(&mut self, index: usize, reason: DropReason, message: &str) -> Result<(), Stop> {
        let entries = self.entries;
        let entry = &entries[index];
        outbox::settle(
            self.store,
            self.account_id,
            entry,
            None,
            None,
            Outcome::Dropped { error: message },
        )?;
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
