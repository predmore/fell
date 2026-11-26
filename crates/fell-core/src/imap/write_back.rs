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
//! checks the bounded range of UIDs allocated since the recorded UIDNEXT
//! and verifies the exact Message-ID. Replay rearms a MOVE only after
//! confirming the destination did not get it and the original UID remains
//! in the source. Other misses back off. At the retry limit a confirmed
//! source copy is restored to its folder in Fell.
//!
//! A refusal (NO or BAD) is retried with backoff and the row is dropped
//! after [`MAX_ATTEMPTS`]. A dropped connection ends the pass. An oversized
//! line during recovery counts as an inconclusive check. Moves keep their queue order
//! because a move can change a later write's UID. A refused STORE can
//! retry on its own. A STORE with an unknown outcome holds its copy until
//! replay can check what reached the server.
//!
//! Undo's reverse of a row that was sent but not settled waits behind it,
//! then moves the copy that row placed. If replay gave up finding that copy,
//! the reverse can use a destination UID recorded by the read-only sync
//! within the persisted recovery window.

use std::collections::{HashMap, HashSet};
use std::ops::RangeInclusive;

use crate::Error;
use crate::account::AccountId;
use crate::folders::{FolderRole, FolderRoles};
use crate::outbox::{
    self, Capability, DropReason, DroppedWrite, MoveSource, OutboxEntry, OutboxKind, Outcome,
    ReplayReport, Reversed, Settled, drafts,
    flags::{self, FetchReason, Observation, Progress},
};
use crate::store::{Store, copies::CopyFlags};

use super::rfc822::{copy_message_id, is_synthetic_message_id};
use super::session::{FlagReply, Placed, WriteError, WriteSession, quote_imap};
use super::{ImapConfig, listed_mailboxes};

mod labels;
mod trash;

/// Refusals (or "not on the server yet") before a row is dropped.
pub(crate) const MAX_ATTEMPTS: u32 = 5;
/// Wait before the next try, by attempts so far. The last one repeats.
const BACKOFF_SECS: [i64; 5] = [10, 30, 120, 600, 1800];
/// Inconclusive checks are independent of explicit APPEND refusals.
pub(super) const MAX_APPEND_CHECKS: u32 = 5;
pub(crate) const RECOVERY_UID_LIMIT: usize = 500;
const RECOVERY_HEADER_BATCH: usize = 100;
const UNCONFIRMED_MOVE: &str = "Could not confirm the move";

/// The closed range of UIDs allocated between two UIDNEXT observations.
fn recovery_uid_range(first: u32, next: u32) -> Option<RangeInclusive<u32>> {
    (first != 0 && next > first && next - first <= RECOVERY_UID_LIMIT as u32)
        .then(|| first..=next - 1)
}

/// Send every outbox row that is due. Returns what was sent and dropped.
/// A connection that fails part way is in [`ReplayReport::interrupted`];
/// what was settled before it stays settled.
#[cfg(test)]
pub(super) fn replay_with(
    config: &ImapConfig,
    account_id: &AccountId,
    store: &Store,
    write_back: bool,
    append_sent: bool,
    before_send: &dyn Fn(),
) -> crate::Result<ReplayReport> {
    replay_bound(
        config,
        account_id,
        store,
        write_back,
        append_sent,
        before_send,
        None,
    )
}

pub(super) fn replay_bound(
    config: &ImapConfig,
    account_id: &AccountId,
    store: &Store,
    write_back: bool,
    append_sent: bool,
    before_send: &dyn Fn(),
    expected: Option<&crate::AccountConfig>,
) -> crate::Result<ReplayReport> {
    // A second pass must not check for absence while a MOVE is on the wire.
    let guard = store.try_lock_mailbox_operation(".write-back")?;
    if guard.is_none() && store.db_path().is_some() {
        return Ok(ReplayReport::default());
    }
    let verified = if let Some(config_file) = expected
        && outbox::endpoint::can_resume(store, config_file)?
    {
        let mut connection = ReplayConnection::open(config)?;
        let server = store.observe_imap_server(config_file)?;
        resume_held(&mut connection.session, store, config_file, &server)?;
        Some(connection)
    } else {
        None
    };
    let mut entries = outbox::unfinished(store, account_id, write_back, expected)?;
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
        append_roles: FolderRoles::default(),
        overrides: &config.folder_overrides,
        selected: None,
        uncleared_trash: entries
            .iter()
            .filter(|entry| entry.owns_trash_mark())
            .map(|entry| entry.id)
            .collect(),
        before_send,
        report: &mut report,
    };
    let result = match verified.map_or_else(|| ReplayConnection::open(config), Ok) {
        Ok(mut connection) => {
            if let Some(config) = expected
                && !store.imap_server_matches(config)?
            {
                // Bind the authenticated endpoint, then reload the queue
                // before sending any writes.
                store.observe_imap_server(config)?;
                return Ok(ReplayReport::default());
            }
            // Replay can disconnect or leave a Trash delete mark behind.
            // Enter it with LOGOUT disabled until the outcome is known.
            connection.logout = false;
            let result = pass.run(config, &mut connection.session);
            connection.logout = !matches!(
                &result,
                Err(Stop::Connection(_) | Stop::Store(_) | Stop::AppendCheckDisconnected(_))
            ) && pass.safe_to_logout();
            result
        }
        Err(err)
            if entries
                .iter()
                .any(|entry| entry.kind.is_append() && entry.retry_unix <= now) =>
        {
            let note = err.to_string();
            for (index, entry) in entries.iter().enumerate() {
                if entry.kind.is_append() && entry.retry_unix <= now {
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
            Stop::AppendRetry(message)
            | Stop::AppendRefused(message)
            | Stop::DraftCleanupRefused(message)
            | Stop::AppendCheck(message),
        ) => {
            return Err(Error::Backend(message));
        }
        Err(
            Stop::Connection(err)
            | Stop::AppendCheckDisconnected(err)
            | Stop::DraftCleanupDisconnected(err)
            | Stop::TrashDisconnected(err),
        ) => {
            report.interrupted = Some(err.to_string());
        }
        // Write-back was turned off by another process mid-pass.
        Err(Stop::Stale) => {}
    }
    report.waiting = entries.len() - settled;
    Ok(report)
}

struct ReplayConnection {
    session: WriteSession,
    logout: bool,
}

impl ReplayConnection {
    fn open(config: &ImapConfig) -> crate::Result<Self> {
        Ok(Self {
            session: WriteSession::open(config)?,
            logout: true,
        })
    }
}

impl Drop for ReplayConnection {
    fn drop(&mut self) {
        if self.logout {
            self.session.logout();
        }
    }
}

fn resume_held(
    session: &mut WriteSession,
    store: &Store,
    config: &crate::AccountConfig,
    server: &str,
) -> crate::Result<()> {
    let mut observed = HashMap::new();
    for row in outbox::endpoint::resumable(store, config, server)? {
        let (source_epoch, target_epoch) = outbox::endpoint::resume_epochs(store, &row)?;
        let source = row.source_mailbox.as_deref().zip(source_epoch);
        let target = row.target_mailbox.as_deref().zip(target_epoch);
        if source.is_none() && (row.uid.is_some() || target.is_none()) {
            continue;
        }
        let mut matches = true;
        let mut checked = true;
        for (mailbox, expected) in source.into_iter().chain(target) {
            let validity = match observed.entry(mailbox.to_owned()) {
                std::collections::hash_map::Entry::Occupied(entry) => *entry.get(),
                std::collections::hash_map::Entry::Vacant(entry) => {
                    let actual = match session.select(mailbox) {
                        Ok(validity) => Some(validity),
                        Err(WriteError::Refused(_)) => None,
                        Err(WriteError::Connection(error)) => return Err(error),
                    };
                    *entry.insert(actual)
                }
            };
            checked &= validity.is_some();
            matches &= validity == Some(expected);
        }
        if checked {
            outbox::endpoint::resume_verified(store, config, server, &row, matches)?;
        }
    }
    Ok(())
}

pub(crate) fn requeue_held_writes(
    config: &crate::AccountConfig,
    store: &Store,
) -> crate::Result<Vec<i64>> {
    if !config.write_back {
        return Ok(Vec::new());
    }
    let guard = store.try_lock_mailbox_operation(".write-back")?;
    if guard.is_none() && store.db_path().is_some() {
        return Err(Error::Backend(
            "Mail changes are already syncing. Try again shortly.".into(),
        ));
    }
    let account = AccountId(config.id.clone());
    let rows = outbox::endpoint::held(store, &account)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let imap = config.imap_config()?;
    let mut session = WriteSession::open(&imap)?;
    store.observe_imap_server(config)?;
    let listed = session.list()?;
    let roles = FolderRoles::detect(&listed_mailboxes(&listed), &imap.folder_overrides);
    let exact = |folder: &str| {
        listed
            .iter()
            .find(|item| item.decoded == folder)
            .map(|item| item.raw.as_str())
    };
    let by_role = |role| match role {
        FolderRole::Archive => exact(roles.archive_name()),
        _ => roles.get(role).and_then(|folder| exact(&folder.name)),
    };
    let mailbox = |folder: &str, original_role: Option<FolderRole>| {
        exact(folder)
            .or_else(|| {
                roles
                    .iter()
                    .find(|role| role.name.eq_ignore_ascii_case(folder))
                    .and_then(|role| exact(&role.name))
            })
            .or_else(|| {
                listed
                    .iter()
                    .find(|item| item.decoded.eq_ignore_ascii_case(folder))
                    .map(|item| item.raw.as_str())
            })
            .or_else(|| original_role.and_then(by_role))
            .or_else(|| {
                FolderRole::ALL
                    .into_iter()
                    .find(|role| role.default_name().eq_ignore_ascii_case(folder))
                    .and_then(by_role)
            })
    };
    let mut queued = Vec::new();
    for row in rows {
        if is_synthetic_message_id(&row.message_id)
            || row.reverses.is_some()
            || !matches!(
                row.kind,
                OutboxKind::Move | OutboxKind::Seen | OutboxKind::Flagged | OutboxKind::Keyword
            )
        {
            continue;
        }
        let Some(source) = mailbox(&row.source_folder, None) else {
            continue;
        };
        let target = (row.kind == OutboxKind::Move)
            .then(|| mailbox(&row.target_folder, row.target_role))
            .flatten();
        if row.kind == OutboxKind::Move && (target.is_none() || target == Some(source)) {
            continue;
        }
        match locate_for_resend(&mut session, source, &row.message_id) {
            Ok(Some((validity, uid))) => {
                if outbox::endpoint::requeue(store, config, &row, (source, validity, uid), target)?
                {
                    queued.push(row.id);
                }
            }
            Ok(None) | Err(WriteError::Refused(_)) => {}
            Err(WriteError::Connection(error)) => return Err(error),
        }
    }
    session.logout();
    Ok(queued)
}

fn locate_for_resend(
    session: &mut WriteSession,
    mailbox: &str,
    id: &str,
) -> Result<Option<(u32, u32)>, WriteError> {
    let validity = session.select(mailbox)?;
    let candidates = session.search_message_id(id, RECOVERY_UID_LIMIT, None)?;
    let mut found = None;
    for batch in candidates.chunks(RECOVERY_HEADER_BATCH) {
        for (uid, header) in session.uid_fetch_message_ids(batch)? {
            if batch.binary_search(&uid).is_ok()
                && copy_message_id(mailbox, validity, uid, &header, false).as_str() == id
                && found.replace(uid).is_some()
            {
                return Ok(None);
            }
        }
    }
    Ok(found.map(|uid| (validity, uid)))
}

enum Stop {
    Connection(Error),
    Store(Error),
    AppendRetry(String),
    AppendRefused(String),
    DraftCleanupRefused(String),
    DraftCleanupDisconnected(Error),
    /// Retry unrelated rows on a fresh session.
    TrashDisconnected(Error),
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
    CleanupRefused,
    Inconclusive { checks: u32 },
}

enum MoveHold {
    /// An earlier MOVE can change which copy a later write should use.
    EarlierMove,
    /// A MOVE needs the copy with an unconfirmed STORE. Other copies can write.
    StoreRecovery,
}

enum Transfer {
    Placed((u32, u32)),
    Absent,
    Unknown,
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
    append_roles: FolderRoles,
    overrides: &'a [(FolderRole, String)],
    /// The mailbox selected now and its UIDVALIDITY.
    selected: Option<(String, u32)>,
    /// Rows whose UID Fell may have marked, including cleanup in backoff.
    uncleared_trash: HashSet<i64>,
    before_send: &'a dyn Fn(),
    report: &'a mut ReplayReport,
}

impl Pass<'_> {
    fn run(&mut self, config: &ImapConfig, session: &mut WriteSession) -> Result<(), Stop> {
        // Whether the user hears of it is up to the status line's report
        // (`outbox::report_for_status`): that notice is said once.
        let mut visited = vec![false; self.entries.len()];
        let has_move = session.has_move();
        outbox::observe_uidplus(
            self.store.connection(),
            self.account_id,
            session.has_uidplus(),
        )?;
        if has_move {
            outbox::note_capability_missing(self.store, self.account_id, Capability::Move, false)?;
        } else {
            for (index, entry) in self.entries.iter().enumerate() {
                if entry.kind == OutboxKind::Move && !entry.sent && entry.retry_unix <= self.now {
                    self.drop_row(index, DropReason::NoMove, "server has no MOVE")?;
                    visited[index] = true;
                }
            }
            self.held_moves.clear();
        }
        let mut disconnected = None;
        for index in 0..self.entries.len() {
            let entry = &self.entries[index];
            if visited[index] || (!has_move && entry.kind == OutboxKind::Move && !entry.sent) {
                continue;
            }
            visited[index] = true;
            if disconnected.is_some() && entry.retry_unix <= self.now {
                *session = WriteSession::open(config).map_err(Stop::Connection)?;
                self.selected = None;
                disconnected = None;
            }
            let held_move = self.held_moves.get(&entry.message_id);
            match entry.kind {
                OutboxKind::Move if held_move.is_some() => {}
                OutboxKind::Move if entry.retry_unix > self.now => {
                    self.held_moves
                        .insert(entry.message_id.clone(), MoveHold::EarlierMove);
                }
                OutboxKind::Move => self.replay_move(session, index, &mut visited)?,
                OutboxKind::AppendSent | OutboxKind::AppendDraft | OutboxKind::DeleteDraft => {
                    if entry.retry_unix <= self.now {
                        self.replay_append(session, index)?;
                    }
                }
                OutboxKind::UnflagTrash => {
                    if entry.retry_unix <= self.now {
                        match self.replay_trash_cleanup(session, index, entry, None) {
                            Err(Stop::TrashDisconnected(error)) => disconnected = Some(error),
                            result => result?,
                        }
                    }
                }
                _ if matches!(held_move, Some(MoveHold::EarlierMove)) => {}
                OutboxKind::Expunge => {
                    if entry.retry_unix <= self.now {
                        match self.replay_expunge(session, index) {
                            Err(Stop::TrashDisconnected(error)) => disconnected = Some(error),
                            result => result?,
                        }
                    }
                    if !self.settled[index] {
                        self.held_moves
                            .insert(entry.message_id.clone(), MoveHold::EarlierMove);
                    }
                }
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
        disconnected.map_or(Ok(()), |error| Err(Stop::Connection(error)))
    }

    fn safe_to_logout(&self) -> bool {
        !self.entries.iter().any(|entry| {
            self.uncleared_trash.contains(&entry.id)
                && self.selected.as_ref().is_none_or(|(mailbox, _)| {
                    entry.source_mailbox.as_deref() == Some(mailbox.as_str())
                })
        })
    }

    fn replay_append(&mut self, session: &mut WriteSession, index: usize) -> Result<(), Stop> {
        // Serialize recovery and transfer across app processes. Recheck the
        // row once we have the lock: another pass may already have sent it.
        let guard = self.store.try_lock_mailbox_operation(".append-sent")?;
        if guard.is_none() && self.store.db_path().is_some() {
            return Ok(());
        }
        let entry = &self.entries[index];
        let draft = if entry.kind == OutboxKind::AppendSent {
            None
        } else {
            let Some(draft) = drafts::pending(self.store, entry)? else {
                return Ok(());
            };
            Some(draft)
        };
        let pending = if entry.kind.is_append() {
            let Some(pending) = outbox::pending_append(self.store, entry)? else {
                return Ok(());
            };
            Some(pending)
        } else {
            None
        };
        let checks = pending.as_ref().map_or(0, |pending| pending.checks);
        let result = self.write_compose_copy(session, entry, pending.as_ref(), draft.as_ref());
        match result {
            Ok(changed) => {
                if changed {
                    self.report.sent += 1;
                    self.report.draft_append_recovered |= entry.kind == OutboxKind::AppendDraft
                        && (!entry.error.is_empty()
                            || draft
                                .as_ref()
                                .is_some_and(|draft| draft.supersedes_uncertain));
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
            Err(Stop::DraftCleanupDisconnected(error)) => {
                self.retry_append(index, &error.to_string(), AppendRetry::CleanupRefused)?;
                Err(Stop::Connection(error))
            }
            Err(Stop::DraftCleanupRefused(message)) => {
                self.retry_append(index, &message, AppendRetry::CleanupRefused)
            }
            Err(Stop::AppendCheck(message)) => {
                self.retry_append(index, &message, AppendRetry::Inconclusive { checks })
            }
            Err(Stop::AppendCheckDisconnected(err)) => {
                self.retry_append(
                    index,
                    &err.to_string(),
                    AppendRetry::Inconclusive { checks },
                )?;
                Err(Stop::Connection(err))
            }
            Err(stop) => Err(stop),
        }
    }

    fn write_compose_copy(
        &mut self,
        session: &mut WriteSession,
        entry: &OutboxEntry,
        pending: Option<&outbox::PendingAppend>,
        draft: Option<&drafts::Pending>,
    ) -> Result<bool, Stop> {
        let Some(draft) = draft else {
            self.append(session, entry, pending.expect("Sent APPEND payload"), true)?;
            return Ok(outbox::complete_append(self.store, entry)?);
        };
        if entry.kind == OutboxKind::DeleteDraft {
            if let Some(copy) = &draft.current {
                self.delete_draft_copy(session, entry, copy, draft.current_message_id.as_deref())?;
            }
        } else {
            let placed = if let Some(placed) = &draft.placed {
                Some(placed.clone())
            } else {
                let placed = self.append(
                    session,
                    entry,
                    pending.expect("Drafts APPEND payload"),
                    draft.can_append,
                )?;
                if let Some(copy) = &placed
                    && !drafts::placed(self.store, entry, copy)?
                {
                    return Err(Stop::Stale);
                }
                placed
            };
            if let Some(copy) = &draft.current
                && placed.as_ref().is_some_and(|new| new != copy)
            {
                self.delete_draft_copy(session, entry, copy, draft.current_message_id.as_deref())?;
            }
        }
        let changed = drafts::finish(self.store, entry)?;
        if changed {
            crate::diagnostic!(DEBUG, "Server draft write completed");
        }
        Ok(changed)
    }

    fn delete_draft_copy(
        &mut self,
        session: &mut WriteSession,
        entry: &OutboxEntry,
        copy: &drafts::Copy,
        message_id: Option<&str>,
    ) -> Result<(), Stop> {
        let write_error = |error| match error {
            WriteError::Refused(message) => Stop::DraftCleanupRefused(message),
            WriteError::Connection(error) => Stop::DraftCleanupDisconnected(error),
        };
        self.selected = None;
        let validity = session.select(&copy.mailbox).map_err(write_error)?;
        self.selected = Some((copy.mailbox.clone(), validity));
        let uid = if validity == copy.validity {
            copy.uid
        } else {
            let unconfirmed = || {
                Stop::DraftCleanupRefused(
                    "Drafts mailbox changed and the old copy could not be verified".into(),
                )
            };
            let message_id = message_id.ok_or_else(unconfirmed)?;
            let candidates = session
                .search_message_id(message_id, RECOVERY_UID_LIMIT, None)
                .map_err(|error| match error {
                    WriteError::Refused(_) => unconfirmed(),
                    WriteError::Connection(error) => Stop::DraftCleanupDisconnected(error),
                })?;
            if candidates.is_empty() {
                if entry.kind == OutboxKind::DeleteDraft
                    && !drafts::begin_delete(self.store, entry)?
                {
                    return Err(Stop::Stale);
                }
                return Ok(());
            }
            let (found, complete) = Self::check_append_headers(
                session,
                message_id,
                &copy.mailbox,
                validity,
                &candidates,
            )
            .map_err(|stop| match stop {
                Stop::AppendCheck(_) => unconfirmed(),
                Stop::AppendCheckDisconnected(error) | Stop::Connection(error) => {
                    Stop::DraftCleanupDisconnected(error)
                }
                stop => stop,
            })?;
            found.filter(|_| complete).ok_or_else(unconfirmed)?
        };
        let cleanup = |session: &mut WriteSession| {
            if drafts::pending(self.store, entry)?.is_none() {
                return Err(Stop::Stale);
            }
            if entry.kind == OutboxKind::DeleteDraft && !drafts::begin_delete(self.store, entry)? {
                return Err(Stop::Stale);
            }
            session.mark_deleted(uid).map_err(write_error)?;
            if session.has_uid_expunge() {
                if drafts::pending(self.store, entry)?.is_none() {
                    return Err(Stop::Stale);
                }
                session.expunge_uid(uid).map_err(write_error)?;
            }
            Ok(())
        };
        match cleanup(session) {
            // A server may refuse STORE or EXPUNGE after someone already
            // removed the UID. Only an explicit empty listing settles it.
            Err(Stop::DraftCleanupRefused(_))
                if session
                    .search_uid_range(uid, uid, 1)
                    .is_ok_and(|uids| uids.is_empty()) =>
            {
                Ok(())
            }
            result => result,
        }
    }

    fn append(
        &mut self,
        session: &mut WriteSession,
        entry: &OutboxEntry,
        pending: &outbox::PendingAppend,
        may_append: bool,
    ) -> Result<Option<drafts::Copy>, Stop> {
        let draft = entry.kind == OutboxKind::AppendDraft;
        let mime = pending.mime.as_deref().filter(|_| may_append);
        let role = if draft {
            FolderRole::Drafts
        } else {
            FolderRole::Sent
        };
        let (mailbox, bound) = if let Some(mailbox) = &pending.sent_to {
            // Refresh the selected UID state before checking for the copy.
            self.selected = None;
            let (validity, next) = session
                .select_with_next(mailbox)
                .map_err(append_check_error)?;
            self.selected = Some((mailbox.clone(), validity));
            let next = next.unwrap_or_default();
            if let Some(uid) = Self::check_append(session, entry, pending, mailbox, validity, next)?
            {
                return Ok(Some(drafts::Copy {
                    mailbox: mailbox.clone(),
                    validity,
                    uid,
                }));
            }
            (mailbox.clone(), (validity, next))
        } else {
            self.load_listing(session)?;
            if mime.is_none() {
                return Ok(None);
            }
            let mailbox = self
                .append_roles
                .get(role)
                .map(|folder| folder.raw.clone())
                .ok_or_else(|| Stop::AppendRetry(format!("no {} mailbox", role.default_name())))?;
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
                    Err(WriteError::Refused(message)) => return Err(Stop::AppendRetry(message)),
                    Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
                }
            } else {
                match session.status(&mailbox) {
                    Ok(bound) => bound,
                    Err(WriteError::Refused(message)) => return Err(Stop::AppendRetry(message)),
                    Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
                }
            };
            (mailbox, bound)
        };
        let Some(mime) = mime else {
            return Ok(None);
        };
        // Never send without a bound that recovery can use if the reply is lost.
        if bound.1 == 0 {
            return Err(Stop::AppendRetry(
                "Sent UIDNEXT could not be verified".into(),
            ));
        }
        let quoted_mailbox = quote_imap(&mailbox).map_err(Stop::AppendRetry)?;
        if !outbox::mark_append_sent(self.store, entry, &mailbox, bound)? {
            return Err(Stop::Stale);
        }
        match session.append(&quoted_mailbox, mime, draft) {
            Ok(_) if !draft => Ok(None),
            Ok(Some((validity, uid))) if validity != bound.0 || uid >= bound.1 => {
                Ok(Some(drafts::Copy {
                    mailbox,
                    validity,
                    uid,
                }))
            }
            Ok(_) => {
                // Without APPENDUID use the same bounded lookup as a lost
                // reply. Do not send again from this acknowledged write.
                let Some(marked) = outbox::pending_append(self.store, entry)? else {
                    return Err(Stop::Stale);
                };
                self.selected = None;
                let (validity, next) = session
                    .select_with_next(&mailbox)
                    .map_err(append_check_error)?;
                self.selected = Some((mailbox.clone(), validity));
                let uid = Self::check_append(
                    session,
                    entry,
                    &marked,
                    &mailbox,
                    validity,
                    next.unwrap_or_default(),
                )?
                .ok_or_else(|| {
                    Stop::AppendCheck("Drafts APPENDUID could not be verified".into())
                })?;
                Ok(Some(drafts::Copy {
                    mailbox,
                    validity,
                    uid,
                }))
            }
            Err(WriteError::Refused(message)) => Err(Stop::AppendRefused(message)),
            Err(WriteError::Connection(error)) => Err(Stop::Connection(error)),
        }
    }

    /// The matching UID, or None when it is safe to APPEND again.
    fn check_append(
        session: &mut WriteSession,
        entry: &OutboxEntry,
        pending: &outbox::PendingAppend,
        mailbox: &str,
        validity: u32,
        next: u32,
    ) -> Result<Option<u32>, Stop> {
        let candidates = session
            .search_message_id(&entry.message_id, RECOVERY_UID_LIMIT, None)
            .map_err(append_check_error)?;
        if !candidates.is_empty() {
            let (found, _) = Self::check_append_headers(
                session,
                entry.message_id.as_str(),
                mailbox,
                validity,
                &candidates,
            )?;
            return if found.is_some() {
                Ok(found)
            } else {
                Err(Stop::AppendCheck(
                    "APPEND Message-ID check found no exact match".into(),
                ))
            };
        }
        if pending.validity != 0 && validity != pending.validity {
            return Ok(None);
        }
        if pending.next == 0 || next < pending.next {
            return Err(Stop::AppendCheck(
                "APPEND UIDNEXT could not be verified".into(),
            ));
        }
        if next == pending.next {
            // A successful APPEND allocates a UID even if it is expunged later.
            return Ok(None);
        }
        let Some(range) = recovery_uid_range(pending.next, next) else {
            return Err(Stop::AppendCheck(
                "APPEND UID range is too large to verify".into(),
            ));
        };
        // UIDNEXT may also advance for unrelated arrivals. List the existing
        // UIDs in the closed range so gaps are distinguished from missing
        // FETCH responses, and check every header before sending again.
        let uids = session
            .search_uid_range(*range.start(), *range.end(), RECOVERY_UID_LIMIT)
            .map_err(append_check_error)?;
        let (found, complete) = Self::check_append_headers(
            session,
            entry.message_id.as_str(),
            mailbox,
            validity,
            &uids,
        )?;
        if found.is_some() || complete {
            Ok(found)
        } else {
            Err(Stop::AppendCheck(
                "APPEND UID range had missing headers".into(),
            ))
        }
    }

    fn check_append_headers(
        session: &mut WriteSession,
        message_id: &str,
        mailbox: &str,
        validity: u32,
        uids: &[u32],
    ) -> Result<(Option<u32>, bool), Stop> {
        let mut complete = true;
        let mut found = None;
        // search_uid_range returns UIDs in ascending order from search_uids.
        for batch in uids.chunks(RECOVERY_HEADER_BATCH) {
            let headers = session
                .uid_fetch_message_ids(batch)
                .map_err(append_check_error)?;
            let returned: HashSet<_> = headers.iter().map(|(uid, _)| *uid).collect();
            for (uid, bytes) in headers {
                if batch.binary_search(&uid).is_ok()
                    && copy_message_id(mailbox, validity, uid, &bytes, false).as_str() == message_id
                    && found.replace(uid).is_some_and(|prev| prev != uid)
                {
                    return Ok((None, false));
                }
            }
            complete &= batch.iter().all(|uid| returned.contains(uid));
        }
        Ok((found, complete))
    }

    fn retry_append(
        &mut self,
        index: usize,
        message: &str,
        retry: AppendRetry,
    ) -> Result<(), Stop> {
        let entry = &self.entries[index];
        let message = if entry.kind == OutboxKind::AppendSent {
            message
        } else {
            match retry {
                AppendRetry::Deferred => {
                    crate::diagnostic!(WARN, "Server draft upload was interrupted");
                }
                AppendRetry::Refused => {
                    crate::diagnostic!(WARN, "The server refused the draft upload");
                }
                AppendRetry::CleanupRefused => {
                    crate::diagnostic!(WARN, "Could not remove the previous server draft copy");
                }
                AppendRetry::Inconclusive { .. } => {
                    crate::diagnostic!(WARN, "Could not verify the previous server draft upload");
                }
            }
            "Drafts write could not be confirmed"
        };
        let attempts = match retry {
            AppendRetry::Deferred | AppendRetry::Refused | AppendRetry::CleanupRefused => {
                entry.attempts
            }
            AppendRetry::Inconclusive { checks } => checks,
        };
        match retry {
            AppendRetry::Refused if attempts + 1 >= MAX_ATTEMPTS => {
                return self.drop_row(index, DropReason::Refused, message);
            }
            AppendRetry::CleanupRefused if attempts + 1 >= MAX_ATTEMPTS => {
                let settled = outbox::settle(
                    self.store,
                    self.account_id,
                    entry,
                    None,
                    None,
                    Outcome::Dropped {
                        error: message,
                        draft_cleanup: true,
                        draft_lookup: None,
                    },
                )?;
                match settled {
                    Settled::Stale => return Err(Stop::Stale),
                    Settled::Recorded => self.note_drop(index, DropReason::DraftCleanup),
                    _ => {}
                }
                self.settled[index] = true;
                return Ok(());
            }
            AppendRetry::Inconclusive { checks }
                if attempts + 1 >= MAX_APPEND_CHECKS && entry.kind == OutboxKind::AppendDraft =>
            {
                match outbox::settle(
                    self.store,
                    self.account_id,
                    entry,
                    None,
                    None,
                    Outcome::Dropped {
                        error: message,
                        draft_cleanup: false,
                        draft_lookup: Some((checks, MAX_APPEND_CHECKS)),
                    },
                )? {
                    Settled::Stale => return Err(Stop::Stale),
                    Settled::Recorded => {
                        self.note_drop(index, DropReason::UnconfirmedAppend);
                        self.settled[index] = true;
                    }
                    Settled::Retired => self.settled[index] = true,
                    _ => {}
                }
                return Ok(());
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
                // Cleanup follows a confirmed APPEND. Keep its sent
                // marker so queueing a later revision cannot cancel it.
                counts: refused || matches!(retry, AppendRetry::CleanupRefused),
                refused,
                append_check: matches!(retry, AppendRetry::Inconclusive { .. }),
            },
        )? {
            Settled::Recorded => {
                self.report.sent_append_delayed |=
                    entry.kind == OutboxKind::AppendSent && entry.error.is_empty();
                self.report.draft_append_delayed |= entry.kind == OutboxKind::AppendDraft
                    && !matches!(retry, AppendRetry::CleanupRefused)
                    && entry.error.is_empty()
                    && entry.placed_uid.is_none();
            }
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
        let recorded_copy = entry.uidvalidity.zip(entry.uid);
        let reversal = entry
            .reverses
            .filter(|_| recorded_copy.is_none())
            .map(|id| outbox::reversed(self.store, self.account_id, id))
            .transpose()?;
        match reversal {
            Some(Reversed::Waiting) => {
                self.held_moves
                    .insert(entry.message_id.clone(), MoveHold::EarlierMove);
                return Ok(());
            }
            Some(Reversed::Gone) => {
                self.drop_quietly(index, "no confirmed destination for the move it reverses")?;
                return Ok(());
            }
            _ => {}
        }
        let source = self.mailbox(
            session,
            entry.source_mailbox.as_deref(),
            &entry.source_folder,
        )?;
        let target = self.mailbox(
            session,
            entry.target_mailbox.as_deref(),
            &entry.target_folder,
        )?;
        if entry.sent {
            let source = source.as_deref().zip(recorded_copy);
            let target = target.as_deref().zip(entry.target_bound);
            return self
                .recover_move(session, index, source, target, &[])
                .map(|_| ());
        }
        let queued_copy = match reversal {
            Some(Reversed::Placed(validity, uid)) => Some((validity, uid)),
            Some(Reversed::Unconfirmed { source, placed }) => {
                let original = target.as_deref().zip(source);
                let message = "Sync did not identify a unique copy to move back";
                match self.move_source(session, original) {
                    Ok(MoveSource::Present) => {
                        self.drop_quietly(index, "the original copy is still on the server")?;
                        return Ok(());
                    }
                    Ok(_) => {}
                    Err(Stop::Connection(error @ Error::ImapLineLimit { .. })) => {
                        self.retry(index, DropReason::UnconfirmedReverse, message)?;
                        return Err(Stop::Connection(error));
                    }
                    Err(stop) => return Err(stop),
                }
                if placed.is_none() {
                    return self.retry(index, DropReason::UnconfirmedReverse, message);
                }
                placed
            }
            _ => recorded_copy,
        };
        let Some((source, target)) = source.zip(target) else {
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
        let uid = match queued_copy {
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
                && !other.sent
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
        let moves: Vec<_> = batch
            .iter()
            .map(|(row, uid)| (entries[*row].id, *uid))
            .collect();
        // Undo, or turning write-back off, may have retired rows since the
        // pass loaded them: only the rows still pending now are sent.
        let marked = outbox::mark_moves_sent(
            self.store,
            self.account_id,
            &moves,
            (&source, uidvalidity),
            &target,
            before,
        )?;
        batch.retain(|(row, _)| {
            let entry = &entries[*row];
            let claimed = marked.contains(&entry.id);
            if !claimed {
                self.held_moves
                    .insert(entry.message_id.clone(), MoveHold::EarlierMove);
            }
            claimed
        });
        if batch.is_empty() {
            return Ok(());
        }
        self.send_move(session, &batch, (&source, uidvalidity), &target, before)
    }

    /// `UID MOVE` the batch. A row the reply does not place is looked for
    /// in the target; one not found there waits for another try.
    fn send_move(
        &mut self,
        session: &mut WriteSession,
        batch: &[(usize, u32)],
        (source, uidvalidity): (&str, u32),
        target: &str,
        bound: outbox::Bound,
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
        // Keep confirmed COPYUIDs even if a lookup for another row disconnects.
        for (&(row, uid), &placed) in batch.iter().zip(&placements) {
            if let Some(placed) = placed {
                self.finish_move(
                    row,
                    (source, uidvalidity, uid),
                    target,
                    placed,
                    session.has_uidplus(),
                )?;
            }
        }
        for (at, &(row, uid)) in batch.iter().enumerate() {
            if placements[at].is_none() {
                placements[at] = self.recover_move(
                    session,
                    row,
                    Some((source, (uidvalidity, uid))),
                    Some((target, bound)),
                    &placements,
                )?;
            }
        }
        Ok(())
    }

    fn recover_move(
        &mut self,
        session: &mut WriteSession,
        row: usize,
        source: Option<(&str, (u32, u32))>,
        target: Option<(&str, outbox::Bound)>,
        taken: &[Option<(u32, u32)>],
    ) -> Result<Option<(u32, u32)>, Stop> {
        let entry = &self.entries[row];
        let last = entry.attempts + 1 >= MAX_ATTEMPTS;
        // On the last check, read the source first. An oversized target
        // reply closes the session, but must still use the observation.
        let observed = if last {
            match self.move_source(session, source) {
                Ok(observed) => observed,
                Err(Stop::Connection(error @ Error::ImapLineLimit { .. })) => {
                    self.give_up_move(row, MoveSource::Unknown, source)?;
                    return Err(Stop::Connection(error));
                }
                Err(stop) => return Err(stop),
            }
        } else {
            MoveSource::Unknown
        };
        let outcome = match (source, target) {
            (Some(_), Some((target, bound))) => self.locate(
                session,
                &entry.message_id,
                target,
                Some(bound),
                taken,
                Some(entry.id),
            ),
            _ => Ok(Transfer::Unknown),
        };
        match outcome {
            Ok(Transfer::Placed(placed)) => {
                if let (Some((source, (validity, uid))), Some((target, _))) = (source, target) {
                    self.finish_move(
                        row,
                        (source, validity, uid),
                        target,
                        placed,
                        session.has_uidplus(),
                    )?;
                }
                return Ok(Some(placed));
            }
            Ok(Transfer::Absent) if !last => match self.move_source(session, source) {
                Ok(MoveSource::Present) => {
                    // Consume a normal retry and clear sent. If undo has
                    // already queued a reverse, neither command is needed.
                    self.retry_as(
                        row,
                        DropReason::Refused,
                        "MOVE did not reach the target",
                        true,
                    )?;
                    return Ok(None);
                }
                Ok(_) => {}
                Err(Stop::Connection(error @ Error::ImapLineLimit { .. })) => {
                    self.retry(row, DropReason::UnconfirmedMove, UNCONFIRMED_MOVE)?;
                    return Err(Stop::Connection(error));
                }
                Err(stop) => return Err(stop),
            },
            Ok(_) => {}
            Err(Stop::Connection(error @ Error::ImapLineLimit { .. })) => {
                if last {
                    self.give_up_move(row, observed, source)?;
                } else {
                    self.retry(row, DropReason::UnconfirmedMove, UNCONFIRMED_MOVE)?;
                }
                return Err(Stop::Connection(error));
            }
            Err(stop) => return Err(stop),
        }
        if last {
            self.give_up_move(row, observed, source)?;
        } else {
            self.retry(row, DropReason::UnconfirmedMove, UNCONFIRMED_MOVE)?;
        }
        Ok(None)
    }

    fn move_source(
        &mut self,
        session: &mut WriteSession,
        source: Option<(&str, (u32, u32))>,
    ) -> Result<MoveSource, Stop> {
        let Some((mailbox, (validity, uid))) = source else {
            return Ok(MoveSource::Unknown);
        };
        if self.select(session, mailbox)? != Ok(validity) {
            return Ok(MoveSource::Unknown);
        }
        match session.uid_copy_flags(uid) {
            Ok(Some(_)) => Ok(MoveSource::Present),
            Ok(None) => Ok(MoveSource::Gone),
            Err(WriteError::Refused(_)) => Ok(MoveSource::Unknown),
            Err(WriteError::Connection(error)) => Err(Stop::Connection(error)),
        }
    }

    fn give_up_move(
        &mut self,
        index: usize,
        observed: MoveSource,
        source: Option<(&str, (u32, u32))>,
    ) -> Result<(), Stop> {
        let entry = &self.entries[index];
        if let Some(reason) =
            outbox::give_up_move(self.store, self.account_id, entry, observed, source)?
        {
            self.note_drop(index, reason);
        }
        self.settled[index] = true;
        self.held_moves
            .insert(entry.message_id.clone(), MoveHold::EarlierMove);
        Ok(())
    }

    fn finish_move(
        &mut self,
        row: usize,
        source: (&str, u32, u32),
        target: &str,
        placed: (u32, u32),
        uidplus: bool,
    ) -> Result<(), Stop> {
        match outbox::settle(
            self.store,
            self.account_id,
            &self.entries[row],
            Some(source),
            Some(target),
            Outcome::Done { placed, uidplus },
        )? {
            Settled::Recorded => self.report.sent += 1,
            Settled::Stale => return Err(Stop::Stale),
            Settled::Recovering | Settled::Retired => {}
        }
        self.settled[row] = true;
        Ok(())
    }

    fn check_transfer(
        &mut self,
        session: &mut WriteSession,
        row: usize,
        target: &str,
        bound: Option<outbox::Bound>,
        taken: &[Option<(u32, u32)>],
    ) -> Result<Option<(u32, u32)>, Stop> {
        let entry = &self.entries[row];
        let message = "cannot identify the label copy";
        match self.locate(session, &entry.message_id, target, bound, taken, None) {
            Ok(Transfer::Placed(placed)) => Ok(Some(placed)),
            Ok(_) => {
                self.retry(row, DropReason::NotFound, message)?;
                Ok(None)
            }
            Err(Stop::Connection(error @ Error::ImapLineLimit { .. })) => {
                // The reader has closed the session. Spend the check before
                // stopping the pass, so repeated oversized lines cannot stall it.
                self.retry(row, DropReason::NotFound, message)?;
                Err(Stop::Connection(error))
            }
            Err(stop) => Err(stop),
        }
    }

    /// The copy of the row's message the move put in `target`, when the
    /// server did not say. Only the bounded UID range allocated since
    /// the first write counts, under the same UIDVALIDITY, and only
    /// when its fetched Message-ID header is exactly the row's: `HEADER`
    /// search is a substring match and ignores case. The lowest such UID no
    /// other row moves is taken. For MOVE, proving absence also requires
    /// checking every existing UID in the span, even if SEARCH missed it.
    fn locate(
        &mut self,
        session: &mut WriteSession,
        message_id: &str,
        target: &str,
        bound: Option<outbox::Bound>,
        taken: &[Option<(u32, u32)>],
        move_id: Option<i64>,
    ) -> Result<Transfer, Stop> {
        let check_absence = move_id.is_some();
        let synthetic = is_synthetic_message_id(message_id);
        if synthetic && !check_absence {
            return Ok(Transfer::Unknown);
        }
        let Some((bound_validity, first)) = bound else {
            return Ok(Transfer::Unknown);
        };
        // Refresh UIDNEXT before searching, and use a closed upper bound so
        // new arrivals cannot extend the range during the check.
        self.selected = None;
        let selected = session.select_with_next(target);
        let next = match &selected {
            Ok((validity, next)) if *validity == bound_validity => *next,
            _ => None,
        };
        let range = next
            .filter(|_| !synthetic)
            .and_then(|next| recovery_uid_range(first, next));
        if let Some(id) = move_id {
            // Undo can use sync only for a span recovery could search in full.
            // A failed SELECT keeps whatever an earlier check recorded.
            match &selected {
                Ok((validity, _)) if *validity != bound_validity => {
                    outbox::clear_move_recovery_end(self.store, id, first)?;
                }
                Ok(_) => {
                    let observed = range.as_ref().map(|range| range.end() + 1);
                    outbox::record_move_recovery_end(self.store, id, first, observed)?;
                }
                Err(_) => {}
            }
        }
        let uidvalidity = match selected {
            Ok((validity, _)) => validity,
            Err(WriteError::Refused(_)) => return Ok(Transfer::Unknown),
            Err(WriteError::Connection(error)) => return Err(Stop::Connection(error)),
        };
        self.selected = Some((target.to_string(), uidvalidity));
        if check_absence && first != 0 && next == Some(first) {
            return Ok(Transfer::Absent);
        }
        let Some(range) = range else {
            return Ok(Transfer::Unknown);
        };
        let found =
            match session.search_message_id(message_id, RECOVERY_UID_LIMIT, Some(range.clone())) {
                // search_uids refuses a reply past the limit, so a list is whole.
                Ok(found) => found,
                Err(WriteError::Refused(_)) => return Ok(Transfer::Unknown),
                Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
            };
        let mut candidates = Vec::new();
        for uid in found {
            if taken.contains(&Some((uidvalidity, uid)))
                || outbox::is_claimed(self.store, self.account_id, target, uidvalidity, uid)?
            {
                continue;
            }
            candidates.push(uid);
        }
        // Filtering the sorted search_uids result preserves ascending order.
        for batch in candidates.chunks(RECOVERY_HEADER_BATCH) {
            let headers = match session.uid_fetch_message_ids(batch) {
                Ok(headers) => headers,
                Err(WriteError::Refused(_)) => return Ok(Transfer::Unknown),
                Err(WriteError::Connection(err)) => return Err(Stop::Connection(err)),
            };
            let exact = headers
                .into_iter()
                .filter(|(uid, header)| {
                    batch.binary_search(uid).is_ok()
                        && copy_message_id(target, uidvalidity, *uid, header, false).as_str()
                            == message_id
                })
                .map(|(uid, _)| uid)
                .min();
            if let Some(uid) = exact {
                return Ok(Transfer::Placed((uidvalidity, uid)));
            }
        }
        if !check_absence {
            return Ok(Transfer::Unknown);
        }
        let uids = match session.search_uid_range(*range.start(), *range.end(), RECOVERY_UID_LIMIT)
        {
            // The span holds at most RECOVERY_UID_LIMIT UIDs, so a full list is whole.
            Ok(uids) => uids,
            Err(WriteError::Refused(_)) => return Ok(Transfer::Unknown),
            Err(WriteError::Connection(error)) => return Err(Stop::Connection(error)),
        };
        // search_uids already sorted these UIDs, so sort the headers to match.
        for batch in uids.chunks(RECOVERY_HEADER_BATCH) {
            let mut headers = match session.uid_fetch_message_ids(batch) {
                Ok(headers) => headers,
                Err(WriteError::Refused(_)) => return Ok(Transfer::Unknown),
                Err(WriteError::Connection(error)) => return Err(Stop::Connection(error)),
            };
            headers.sort_unstable_by_key(|(uid, _)| *uid);
            if headers.len() != batch.len()
                || headers.iter().zip(batch).any(|((uid, header), expected)| {
                    uid != expected
                        || copy_message_id(target, uidvalidity, *uid, header, false).as_str()
                            == message_id
                })
            {
                return Ok(Transfer::Unknown);
            }
        }
        Ok(Transfer::Absent)
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
            self.append_roles = FolderRoles::detect(&listed_mailboxes(&listed), self.overrides);
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
            // A retired write needs no notice. For a refused move, undo's
            // queued reverse is retired as well.
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
        self.note_drop(index, reason);
        Ok(())
    }

    fn note_drop(&mut self, index: usize, reason: DropReason) {
        let entry = &self.entries[index];
        self.report.dropped.push(DroppedWrite {
            kind: if entry.kind == OutboxKind::UnflagTrash
                || reason == DropReason::TrashCopyUnconfirmed
            {
                OutboxKind::Expunge
            } else {
                entry.kind
            },
            source_folder: entry.source_folder.clone(),
            target_folder: entry.target_folder.clone(),
            reason,
        });
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
            Outcome::Dropped {
                error: message,
                draft_cleanup: false,
                draft_lookup: None,
            },
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
