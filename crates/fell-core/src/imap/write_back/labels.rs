use super::{
    DropReason, OutboxEntry, OutboxKind, Pass, Stop, WriteError, WriteSession, outbox, placed_for,
};
use crate::Error;
use crate::imap::{folder_name, utf7::encode_modified_utf7};
use crate::outbox::labels::{self, Applied, LabelCopy};

impl Pass<'_> {
    pub(super) fn replay_label(
        &mut self,
        session: &mut WriteSession,
        index: usize,
    ) -> Result<(), Stop> {
        let Some(entry) = labels::pending(self.store, self.account_id, self.entries[index].id)?
        else {
            self.settled[index] = true;
            return Ok(());
        };
        if entry.retry_unix > self.now {
            return Ok(());
        }
        if let Some(id) = entry.reverses {
            match labels::settled_original(self.store, id)? {
                Some(true) => {}
                Some(false) => return self.finish_label(index, &entry, Applied::Unchanged),
                None => return Ok(()),
            }
        }
        let add = entry
            .flag_value
            .ok_or_else(|| Error::Action("label write needs a value".into()))?;
        if entry.kind == OutboxKind::Keyword {
            return self.replay_keyword(session, index, entry, add);
        }
        let Some(label) = entry.target_folder.strip_prefix("Labels/") else {
            return Err(Error::Action("invalid Bridge label mailbox".into()).into());
        };
        if label.is_empty() {
            return Err(Error::Action("empty Bridge label".into()).into());
        }
        let copies = labels::copies(self.store.connection(), self.account_id, &entry.message_id)?;
        if add {
            self.copy_into_label(session, index, entry, &copies)
        } else {
            self.remove_from_label(session, index, entry, &copies)
        }
    }

    fn replay_keyword(
        &mut self,
        session: &mut WriteSession,
        index: usize,
        mut entry: OutboxEntry,
        add: bool,
    ) -> Result<(), Stop> {
        if session.gmail_labels() {
            return self.drop_row(
                index,
                DropReason::Refused,
                "Gmail labels use a separate protocol",
            );
        }
        let (Some(raw), Some(validity), Some(uid)) =
            (entry.source_mailbox.clone(), entry.uidvalidity, entry.uid)
        else {
            return Err(Error::Action("keyword write needs its server copy".into()).into());
        };
        if self.store_unconfirmed(&raw, validity, uid) {
            return Ok(());
        }
        if !self.select_label_copy(session, index, &raw, validity)? {
            return Ok(());
        }
        match session.keyword(uid, &entry.target_folder) {
            Ok(Some(value)) if value == add => {
                let applied = if entry.sent {
                    Applied::Keyword
                } else {
                    return self.finish_label(index, &entry, Applied::Unchanged);
                };
                let copy = (entry.source_folder.clone(), raw, validity, uid);
                return self.finish_label(index, &entry, applied(&copy));
            }
            Ok(Some(_)) => {}
            Ok(None) => return self.drop_row(index, DropReason::NotFound, "keyword copy is gone"),
            Err(err) => return self.label_error(index, err, false),
        }
        if !self.mark_label_sent(&mut entry, (0, 0))? {
            self.settled[index] = true;
            return Ok(());
        }
        match session.set_keyword(uid, &entry.target_folder, add) {
            Ok(()) => {
                let copy = (entry.source_folder.clone(), raw, validity, uid);
                self.finish_label(index, &entry, Applied::Keyword(&copy))
            }
            Err(err) => self.label_error(index, err, true),
        }
    }

    fn copy_into_label(
        &mut self,
        session: &mut WriteSession,
        index: usize,
        mut entry: OutboxEntry,
        copies: &[LabelCopy],
    ) -> Result<(), Stop> {
        // Use the case-preserving server name when it already exists.
        let target = match self.mailbox(session, None, &entry.target_folder)? {
            Some(raw) => raw,
            None if entry.sent => encode_modified_utf7(&entry.target_folder),
            None => {
                let raw = encode_modified_utf7(&entry.target_folder);
                if let Err(err) = session.create_label_mailbox(&raw) {
                    self.listing = None;
                    return self.label_error(index, err, false);
                }
                if let Some(listing) = &mut self.listing {
                    listing.insert(entry.target_folder.clone(), raw.clone());
                }
                raw
            }
        };
        if entry.sent {
            // A lost COPY reply must never cause a second copy.
            self.mark_label_sent(&mut entry, (0, 0))?;
            return self.recover_label_copy(session, index, &entry, &target);
        }
        if copies
            .iter()
            .any(|(folder, _, _, _)| folder == &entry.target_folder)
        {
            return self.finish_label(index, &entry, Applied::Unchanged);
        }
        let Some((_, source, validity, uid)) = copies
            .iter()
            .find(|(folder, _, _, _)| !folder.starts_with("Labels/"))
        else {
            return self.retry(index, DropReason::NotFound, "no source copy for label");
        };
        let before = match session.status(&target) {
            Ok(before) => before,
            Err(err) => return self.label_error(index, err, false),
        };
        if !self.select_label_copy(session, index, source, *validity)? {
            return Ok(());
        }
        if !self.mark_label_sent(&mut entry, before)? {
            self.settled[index] = true;
            return Ok(());
        }
        let placed = match session.uid_copy(*uid, &target) {
            Ok(placed) => placed,
            Err(err) => return self.label_error(index, err, true),
        };
        match placed_for(&placed, *uid) {
            Some((validity, uid)) if validity == before.0 => {
                self.finish_label(index, &entry, Applied::Copied(&target, validity, uid))
            }
            _ => self.recover_label_copy(session, index, &entry, &target),
        }
    }

    fn recover_label_copy(
        &mut self,
        session: &mut WriteSession,
        index: usize,
        entry: &OutboxEntry,
        target: &str,
    ) -> Result<(), Stop> {
        match self.locate(session, index, target, &[])? {
            Some((validity, uid)) => {
                self.finish_label(index, entry, Applied::Copied(target, validity, uid))
            }
            None => self.retry(
                index,
                DropReason::NotFound,
                "cannot identify the label copy",
            ),
        }
    }

    fn remove_from_label(
        &mut self,
        session: &mut WriteSession,
        index: usize,
        mut entry: OutboxEntry,
        copies: &[LabelCopy],
    ) -> Result<(), Stop> {
        for copy @ (folder, raw, validity, uid) in copies {
            if folder != &entry.target_folder || folder_name(raw) != entry.target_folder {
                continue;
            }
            if !session.has_uid_expunge() {
                return self.drop_row(
                    index,
                    DropReason::Refused,
                    "server cannot expunge one label copy",
                );
            }
            if !self.select_label_copy(session, index, raw, *validity)? {
                return Ok(());
            }
            if !self.mark_label_sent(&mut entry, (0, 0))? {
                self.settled[index] = true;
                return Ok(());
            }
            if let Err(err) = session.set_keyword(*uid, "\\Deleted", true) {
                return self.label_error(index, err, false);
            }
            if let Err(err) = session.expunge_uid(*uid) {
                return self.label_error(index, err, false);
            }
            labels::finish(self.store, self.account_id, &entry, Applied::Removed(copy))?;
        }
        self.finish_label(index, &entry, Applied::Unchanged)
    }

    fn select_label_copy(
        &mut self,
        session: &mut WriteSession,
        index: usize,
        raw: &str,
        validity: u32,
    ) -> Result<bool, Stop> {
        match self.select(session, raw)? {
            Ok(current) if current == validity => return Ok(true),
            Ok(_) => self.drop_row(index, DropReason::FolderReset, "UIDVALIDITY changed")?,
            Err(message) => self.retry(index, DropReason::Refused, &message)?,
        }
        Ok(false)
    }

    fn mark_label_sent(
        &mut self,
        entry: &mut OutboxEntry,
        before: (u32, u32),
    ) -> Result<bool, Stop> {
        (self.before_send)();
        let marked = outbox::mark_sent(self.store, self.account_id, &[entry.id], before)?;
        entry.sent |= !marked.is_empty();
        self.bounds.extend(marked.iter().copied());
        Ok(!marked.is_empty())
    }

    fn finish_label(
        &mut self,
        index: usize,
        entry: &OutboxEntry,
        applied: Applied<'_>,
    ) -> Result<(), Stop> {
        labels::finish(self.store, self.account_id, entry, applied)?;
        self.settled[index] = true;
        Ok(())
    }

    fn label_error(&mut self, index: usize, error: WriteError, refused: bool) -> Result<(), Stop> {
        match error {
            WriteError::Refused(message) => {
                self.retry_as(index, DropReason::Refused, &message, refused)
            }
            WriteError::Connection(err) => Err(Stop::Connection(err)),
        }
    }
}
