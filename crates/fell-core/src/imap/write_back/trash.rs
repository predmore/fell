use super::{BACKOFF_SECS, DropReason, MAX_ATTEMPTS, OutboxEntry, Pass, Stop};
use super::{Error, WriteError, WriteSession, outbox};

impl Pass<'_> {
    pub(super) fn replay_expunge(
        &mut self,
        session: &mut WriteSession,
        index: usize,
    ) -> Result<(), Stop> {
        let entry = &self.entries[index];
        let (mailbox, validity, uid) = trash_copy(entry)?;
        let confirmed = outbox::confirmed_trash(self.store.connection(), self.account_id)?
            .is_some_and(|(folder, raw)| {
                raw == mailbox && entry.source_folder.eq_ignore_ascii_case(&folder)
            });
        if !confirmed || !session.has_uidplus() {
            let reason = if confirmed {
                DropReason::NoUidPlus
            } else {
                DropReason::ServerCopyKept
            };
            if entry.sent {
                return self.replay_trash_cleanup(session, index, entry, Some(reason));
            }
            return self.drop_row(index, reason, "Trash expunge is unavailable");
        }
        if self.store_unconfirmed(mailbox, validity, uid) {
            return Ok(());
        }
        match self.select(session, mailbox) {
            Ok(Ok(current)) if current != validity => {
                self.uncleared_trash.remove(&entry.id);
                return self.drop_row(index, DropReason::FolderReset, "UIDVALIDITY changed");
            }
            Ok(Err(message)) if entry.owns_trash_mark() => {
                return self.retry_trash(index, DropReason::TrashMarkNotCleared, &message, true);
            }
            Ok(Err(message)) => return self.retry(index, DropReason::Refused, &message),
            Ok(Ok(_)) => {}
            Err(Stop::Connection(error)) if entry.owns_trash_mark() => {
                return self.trash_cleanup_disconnected(index, entry, error);
            }
            Err(stop) => return Err(stop),
        }
        let already_deleted = if entry.owns_trash_mark() {
            false
        } else {
            match session.keyword(uid, "\\Deleted") {
                Ok(Some(value)) => value,
                Ok(None) => {
                    if entry.sent {
                        match session.search_uid_range(uid, uid, 1) {
                            Ok(uids) if uids.is_empty() => return self.finish_expunge(index),
                            Err(WriteError::Connection(error)) => {
                                return Err(Stop::TrashDisconnected(error));
                            }
                            _ => {}
                        }
                    }
                    return self.retry(
                        index,
                        DropReason::ServerCopyKept,
                        "UID FETCH did not return the Trash copy's FLAGS",
                    );
                }
                Err(WriteError::Refused(message)) => {
                    return self.retry(index, DropReason::ServerCopyKept, &message);
                }
                Err(WriteError::Connection(error)) => return Err(Stop::TrashDisconnected(error)),
            }
        };
        let authorize = || {
            (self.before_send)();
            outbox::mark_expunge_sent(self.store, self.account_id, entry, already_deleted)
        };
        if !authorize()? {
            if entry.sent {
                self.replay_trash_cleanup(session, index, entry, None)?;
            }
            return Err(Stop::Stale);
        }
        let mut sent = entry.clone();
        sent.sent = true;
        sent.flag_base = Some(already_deleted);
        let mut may_be_marked = entry.owns_trash_mark();
        let mut expunge = session
            .trash_expunge(entry)
            .ok_or_else(|| Error::Action("invalid Trash expunge in write session".into()))?;
        let result = if already_deleted {
            expunge.expunge()
        } else {
            self.uncleared_trash.insert(entry.id);
            match expunge.mark_deleted() {
                Ok(()) => {
                    may_be_marked = true;
                    if !authorize()? {
                        self.replay_trash_cleanup(session, index, &sent, None)?;
                        return Err(Stop::Stale);
                    }
                    expunge.expunge()
                }
                Err(error) => Err(error),
            }
        };
        match result {
            Ok(()) => {}
            Err(WriteError::Connection(error)) => return Err(Stop::TrashDisconnected(error)),
            Err(WriteError::Refused(message)) => {
                // Some servers refuse writes against a UID that's already gone.
                match session.search_uid_range(uid, uid, 1) {
                    Ok(uids) if uids.is_empty() => return self.finish_expunge(index),
                    Err(WriteError::Connection(error)) => {
                        return Err(Stop::TrashDisconnected(error));
                    }
                    _ => {}
                }
                if may_be_marked {
                    match self.clear_trash_mark(session, &sent, false) {
                        Ok(false) => return self.finish_expunge(index),
                        Ok(true) => {}
                        Err(error) => return self.trash_cleanup_error(index, entry, error, true),
                    }
                }
                self.uncleared_trash.remove(&entry.id);
                return self.retry_trash(index, DropReason::ServerCopyKept, &message, false);
            }
        }
        self.finish_expunge(index)
    }

    fn finish_expunge(&mut self, index: usize) -> Result<(), Stop> {
        let entry = &self.entries[index];
        self.uncleared_trash.remove(&entry.id);
        if outbox::finish_expunge(self.store, self.account_id, entry)? {
            self.report.sent += 1;
        }
        self.settled[index] = true;
        Ok(())
    }

    pub(super) fn replay_trash_cleanup(
        &mut self,
        session: &mut WriteSession,
        index: usize,
        entry: &OutboxEntry,
        unavailable: Option<DropReason>,
    ) -> Result<(), Stop> {
        let (mailbox, validity, _) = trash_copy(entry)?;
        let owned = entry.owns_trash_mark();
        let kept = match self.select(session, mailbox) {
            Ok(Ok(current)) if current == validity => {
                match self.clear_trash_mark(session, entry, true) {
                    Ok(kept) => kept,
                    Err(error) => return self.trash_cleanup_error(index, entry, error, owned),
                }
            }
            Ok(Ok(_)) => {
                self.note_drop(index, DropReason::FolderReset);
                false
            }
            Ok(Err(message)) => {
                return self.trash_cleanup_error(index, entry, WriteError::Refused(message), owned);
            }
            Err(Stop::Connection(error)) => {
                return self.trash_cleanup_disconnected(index, entry, error);
            }
            Err(stop) => return Err(stop),
        };
        self.uncleared_trash.remove(&entry.id);
        let active = outbox::finish_trash_cleanup(self.store, entry.id, kept)?;
        if active
            && kept
            && let Some(reason) = unavailable
        {
            self.note_drop(index, reason);
        }
        self.settled[index] = true;
        Ok(())
    }

    /// Returns whether the copy is still present when cleanup is the last write.
    fn clear_trash_mark(
        &self,
        session: &mut WriteSession,
        entry: &OutboxEntry,
        check_presence: bool,
    ) -> Result<bool, WriteError> {
        let uid = entry.uid.expect("validated Trash UID");
        if check_presence && !self.trash_uid_present(session, uid)? {
            return Ok(false);
        }
        if entry.owns_trash_mark() {
            match session.clear_trash_deleted(entry) {
                Err(error @ WriteError::Refused(_)) if !check_presence => {
                    if !self.trash_uid_present(session, uid)? {
                        return Ok(false);
                    }
                    return Err(error);
                }
                result => result?,
            }
        }
        Ok(true)
    }

    fn trash_uid_present(&self, session: &mut WriteSession, uid: u32) -> Result<bool, WriteError> {
        match session.search_uid_range(uid, uid, 1) {
            Ok(uids) => Ok(!uids.is_empty()),
            Err(WriteError::Refused(_)) => Ok(true),
            Err(error) => Err(error),
        }
    }

    fn trash_cleanup_error(
        &mut self,
        index: usize,
        entry: &OutboxEntry,
        error: WriteError,
        owned: bool,
    ) -> Result<(), Stop> {
        match error {
            WriteError::Refused(message) => {
                self.retry_trash(index, cleanup_reason(owned), &message, true)
            }
            WriteError::Connection(error) => self.trash_cleanup_disconnected(index, entry, error),
        }
    }

    fn trash_cleanup_disconnected(
        &mut self,
        index: usize,
        entry: &OutboxEntry,
        error: Error,
    ) -> Result<(), Stop> {
        let owned = self.uncleared_trash.contains(&entry.id);
        self.retry_trash(index, cleanup_reason(owned), &error.to_string(), true)?;
        Err(Stop::TrashDisconnected(error))
    }

    fn retry_trash(
        &mut self,
        index: usize,
        reason: DropReason,
        message: &str,
        cleanup_pending: bool,
    ) -> Result<(), Stop> {
        let entry = &self.entries[index];
        let last = entry.attempts + 1 >= MAX_ATTEMPTS;
        let delay = BACKOFF_SECS[(entry.attempts as usize).min(BACKOFF_SECS.len() - 1)];
        let state = outbox::retry_trash(
            self.store,
            entry.id,
            self.now + delay,
            message,
            last,
            cleanup_pending,
        )?;
        if state == Some(outbox::OutboxState::Dropped) {
            self.note_drop(index, reason);
        }
        self.settled[index] = state != Some(outbox::OutboxState::Pending);
        Ok(())
    }
}

fn cleanup_reason(owned: bool) -> DropReason {
    if owned {
        DropReason::TrashMarkNotCleared
    } else {
        DropReason::ServerCopyKept
    }
}

fn trash_copy(entry: &OutboxEntry) -> crate::Result<(&str, u32, u32)> {
    match (
        entry.source_mailbox.as_deref(),
        entry.uidvalidity,
        entry.uid,
    ) {
        (Some(mailbox), Some(validity), Some(uid)) if validity != 0 && uid != 0 => {
            Ok((mailbox, validity, uid))
        }
        _ => Err(Error::Action("invalid Trash copy in outbox".into())),
    }
}
