use std::collections::HashMap;

use crate::Result;
use crate::account::{Account, AccountId};
use crate::backend::MailBackend;
use crate::folders::FolderRole;
use crate::imap::decode_cursor;
use crate::message::{MessageId, MessageParts, RemoteId};
pub use crate::store::inbox_arrivals::InboxArrival;
use crate::store::inbox_arrivals::Observer;
use crate::store::{BodyRequest, ReplyToVerification, Store};

/// Messages per backfill batch. Each batch is one fetch and one commit.
const BODY_BATCH: usize = 200;
const REPLY_TO_BATCH: u32 = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// Distinct messages added by catch-up deltas in this pass. A staged
    /// folder walk can write provisional rows that later merge when their
    /// bodies arrive. If a walk ran, zero confirmed arrivals is reported as
    /// `None`, while positive catch-up counts are still reported.
    pub new_messages: Option<usize>,
    /// Mail may have changed since the beginning of the pass, including
    /// updates to messages that were already stored.
    pub changed: bool,
    /// Uncertain local drafts confirmed by exact Message-IDs in Sent.
    pub confirmed_drafts: Vec<String>,
    /// The most recent first-sync folder progress in this pass, if any.
    pub progress: Option<crate::backend::SyncProgress>,
}

impl Default for SyncReport {
    fn default() -> Self {
        Self {
            new_messages: Some(0),
            changed: false,
            confirmed_drafts: Vec::new(),
            progress: None,
        }
    }
}

/// Pull a backend delta into the local store. The UI should call this off the frame path.
pub fn sync_once(
    account: &Account,
    backend: &dyn MailBackend,
    store: &Store,
) -> Result<SyncReport> {
    sync_with_progress(account, backend, store, &mut |_| {})
}

/// [`sync_once`], reporting progress after a delta successfully changes
/// mail or advances a folder walk. A delta with a large flag sweep can
/// commit its messages and flags before its copy and cursor transaction.
/// A failure after the first commit returns the error without reporting
/// that delta. Its committed messages are already stored on retry.
pub fn sync_with_progress(
    account: &Account,
    backend: &dyn MailBackend,
    store: &Store,
    progress: &mut dyn FnMut(&SyncReport),
) -> Result<SyncReport> {
    sync_with_arrivals(account, backend, store, progress, None)
}

/// Like [`sync_with_progress`], with an optional observer of new unread
/// Inbox messages. The observer runs after each delta's final commit,
/// even when the pass later fails. A lookup failure only reaches the observer.
pub fn sync_with_arrivals(
    account: &Account,
    backend: &dyn MailBackend,
    store: &Store,
    progress: &mut dyn FnMut(&SyncReport),
    arrivals: Option<&mut dyn FnMut(Result<Vec<InboxArrival>>)>,
) -> Result<SyncReport> {
    sync_until(account, backend, store, progress, arrivals, &|| false)
}

/// Stop before applying the next delta when the mailbox worker is retiring.
pub fn sync_until(
    account: &Account,
    backend: &dyn MailBackend,
    store: &Store,
    progress: &mut dyn FnMut(&SyncReport),
    arrivals: Option<&mut dyn FnMut(Result<Vec<InboxArrival>>)>,
    stop: &dyn Fn() -> bool,
) -> Result<SyncReport> {
    sync_on(account, backend, store, progress, arrivals, stop, None)
}

/// Sync an account file using the same authenticated session that verified its server.
pub fn sync_connection_until(
    config: &crate::AccountConfig,
    store: &Store,
    progress: &mut dyn FnMut(&SyncReport),
    arrivals: Option<&mut dyn FnMut(Result<Vec<InboxArrival>>)>,
    stop: &dyn Fn() -> bool,
) -> Result<SyncReport> {
    if stop() {
        return Err(crate::Error::Backend("Sync stopped".into()));
    }
    let imap = config.imap_config()?;
    let session = crate::imap::BoundRead::open(config, imap.clone(), store)?;
    let backend = config.backend_with_imap(imap, crate::SmtpSenders::AccountFile);
    sync_on(
        &config.account(),
        backend.as_ref(),
        store,
        progress,
        arrivals,
        stop,
        Some(session),
    )
}

fn sync_on(
    account: &Account,
    backend: &dyn MailBackend,
    store: &Store,
    progress: &mut dyn FnMut(&SyncReport),
    arrivals: Option<&mut dyn FnMut(Result<Vec<InboxArrival>>)>,
    stop: &dyn Fn() -> bool,
    session: Option<crate::imap::BoundRead>,
) -> Result<SyncReport> {
    let check = || {
        if stop() {
            Err(crate::Error::Backend("Sync stopped".into()))
        } else {
            Ok(())
        }
    };
    check()?;
    store.upsert_account(account)?;
    let cursor = store.sync_cursor_for_pass(&account.id)?;
    let mut observer = arrivals.and_then(|send| {
        let inbox = FolderRole::Inbox.default_name();
        let (_, checkpoint) = decode_cursor(cursor.as_ref())
            .into_iter()
            .find(|(mailbox, _)| mailbox.eq_ignore_ascii_case(inbox))?;
        Some(Observer {
            account,
            checkpoint,
            send,
        })
    });
    let mut report = SyncReport::default();
    let known = |mailbox: &str, uidvalidity: u32, range: (u32, u32)| {
        check()?;
        store.stored_uids(&account.id, mailbox, uidvalidity, range)
    };
    let mut persist = |delta: crate::MailboxDelta| {
        check()?;
        let writes = store.apply_sync_delta_observed(&account.id, &delta, observer.as_mut())?;
        report.changed |= writes.changed;
        let advances_walk = delta.progress.is_some();
        if let Some(folder_progress) = delta.progress {
            report.progress = Some(folder_progress);
            if report.new_messages == Some(0) {
                report.new_messages = None;
            }
        } else if writes.new_messages > 0 {
            *report.new_messages.get_or_insert(0) += writes.new_messages;
        }
        let confirmed_send = !writes.confirmed_drafts.is_empty();
        report.confirmed_drafts.extend(writes.confirmed_drafts);
        if writes.changed || advances_walk || confirmed_send {
            progress(&report);
        }
        Ok(())
    };
    if let Some(session) = session {
        session.fetch(account, cursor.as_ref(), &known, &mut persist)?;
    } else {
        backend.sync_changes(cursor.as_ref(), &known, &mut persist)?;
    }
    check()?;
    // Repair a bounded page of undated mail after a successful sync pass.
    if let Ok(changed) = repair_internal_dates(&account.id, backend, store) {
        report.changed |= changed;
    }
    Ok(report)
}

fn repair_internal_dates(
    account: &AccountId,
    backend: &dyn MailBackend,
    store: &Store,
) -> Result<bool> {
    let latest = crate::latest_mail_date();
    let batch = store.missing_internal_dates(account)?;
    let copies: Vec<_> = batch
        .iter()
        .flat_map(|request| request.copies_to_fetch(latest))
        .cloned()
        .collect();
    let mut checked = HashMap::new();
    // Keep the results from finished mailboxes even when the fetch is
    // interrupted. Only a completed pass spends a retry.
    let fetch_complete = copies.is_empty()
        || backend
            .fetch_internal_dates(&copies, &mut |id, date| {
                checked.insert(id.clone(), date);
            })
            .is_ok();
    store.finish_internal_dates(account, &batch, &checked, fetch_complete, latest)
}

/// Cursor for walking unchecked Reply-To headers between sync passes.
/// Unavailable copies stay unchecked. Paging past them lets older mail
/// make progress before the next walk tries them again.
#[derive(Default)]
pub struct ReplyToBackfill {
    before: Option<i64>,
}

/// Progress for one page of legacy Reply-To checks. A batch can confirm
/// some messages before a later fetch is interrupted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyToProgress {
    Checking(Vec<MessageId>),
    Verified(Vec<MessageId>),
    Finished,
}

struct ReplyToCandidate {
    id: MessageId,
    copies: Vec<RemoteId>,
    empty: usize,
    found: bool,
}

impl ReplyToBackfill {
    /// Check a page of up to 32 messages off the UI thread. Returns whether
    /// there was a page to check.
    pub fn run_batch(
        &mut self,
        account: &AccountId,
        backend: &dyn MailBackend,
        store: &Store,
    ) -> Result<bool> {
        self.run_batch_with_progress(account, backend, store, &mut |_| {})
    }

    /// Report the messages being checked and their committed results.
    pub fn run_batch_with_progress(
        &mut self,
        account: &AccountId,
        backend: &dyn MailBackend,
        store: &Store,
        progress: &mut dyn FnMut(ReplyToProgress),
    ) -> Result<bool> {
        let batch = store.missing_reply_to(account, self.before, REPLY_TO_BATCH)?;
        let Some(&(last, _)) = batch.last() else {
            self.before = None;
            return Ok(false);
        };
        let mut candidates = batch
            .into_iter()
            .map(|(_, id)| {
                Ok(ReplyToCandidate {
                    copies: store.reply_to_copies(account, &id)?,
                    id,
                    empty: 0,
                    found: false,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        progress(ReplyToProgress::Checking(
            candidates
                .iter()
                .map(|candidate| candidate.id.clone())
                .collect(),
        ));
        let result = check_reply_to(account, backend, store, &mut candidates, progress);
        progress(ReplyToProgress::Finished);
        result?;
        self.before = Some(last);
        Ok(true)
    }
}

fn check_reply_to(
    account: &AccountId,
    backend: &dyn MailBackend,
    store: &Store,
    candidates: &mut [ReplyToCandidate],
    progress: &mut dyn FnMut(ReplyToProgress),
) -> Result<()> {
    // Read copies in Inbox-first order. The first actual list of addresses
    // wins. A missing copy cannot establish absence, but still leaves the
    // later copies a chance to supply addresses.
    for round in 0.. {
        let mut requested: HashMap<_, _> = candidates
            .iter()
            .enumerate()
            .filter(|(_, candidate)| !candidate.found)
            .filter_map(|(index, candidate)| Some((candidate.copies.get(round)?.clone(), index)))
            .collect();
        if requested.is_empty() {
            break;
        }
        let copies: Vec<_> = requested.keys().cloned().collect();
        backend.fetch_reply_to(&copies, &mut |fetched| {
            let answered: Vec<_> = fetched
                .iter()
                .filter_map(|answer| Some((requested.remove(&answer.remote_id)?, answer)))
                .collect();
            for &(index, answer) in &answered {
                if answer.addresses.is_empty() {
                    candidates[index].empty += 1;
                }
            }
            let answers: Vec<_> = answered
                .into_iter()
                .filter_map(|(index, answer)| {
                    let candidate = &candidates[index];
                    let result = if !answer.addresses.is_empty() {
                        ReplyToVerification::Present(answer)
                    } else if candidate.empty == candidate.copies.len() {
                        ReplyToVerification::Absent(&candidate.copies)
                    } else {
                        return None;
                    };
                    Some((&candidate.id, result))
                })
                .collect();
            let changed = store.store_verified_reply_to(account, &answers)?;
            if !changed.is_empty() {
                for candidate in candidates.iter_mut() {
                    candidate.found |= changed.contains(&candidate.id);
                }
                progress(ReplyToProgress::Verified(changed));
            }
            Ok(())
        })?;
    }
    Ok(())
}

/// Fill parts for mail stored before they were kept, newest first, in up to
/// `batches` batches of 200. Returns how many messages were recorded, with
/// or without a body; 0 means none are left to fetch. Run it
/// on the sync thread after a pass; a large old mailbox fills over several.
///
/// A copy the server no longer has, or whose mailbox it refuses, falls back
/// to the message's next copy; when none is left, the message is recorded as
/// fetched so later passes do not ask again ([`fetch_message_body`] still
/// does). A connection error, or a fetch the server refused or cut short,
/// stores nothing for that batch, so it is tried again. A batch holds about
/// 8 MB of message source at most; the rest waits for the next batch.
pub fn fetch_missing_bodies(
    account_id: &AccountId,
    backend: &dyn MailBackend,
    store: &Store,
    batches: usize,
) -> Result<usize> {
    let mut recorded = 0;
    for _ in 0..batches {
        let wanted = store.missing_bodies(account_id, BODY_BATCH)?;
        if wanted.is_empty() {
            break;
        }
        recorded += fetch_bodies(account_id, backend, store, &wanted)?;
    }
    Ok(recorded)
}

/// Fetch and store the parts of one message that has no stored body part.
///
/// For stored mail: call it on a background thread, never the UI
/// thread, when [`Store::message_body`] has no `html` or `plain`. Returns what
/// [`Store::message_body`] returns afterwards.
pub fn fetch_message_body(
    account_id: &AccountId,
    backend: &dyn MailBackend,
    store: &Store,
    message_id: &MessageId,
) -> Result<Option<MessageParts>> {
    if let Some(request) = store.body_request(account_id, message_id)? {
        fetch_bodies(account_id, backend, store, &[request])?;
    }
    store.message_body(account_id, message_id)
}

/// [`fetch_message_body`] for several messages in one backend call, so a
/// conversation of old mail costs one login, not one per message.
///
/// One answer per id, in order. `Some` parts are what is stored now; they
/// are empty when no server copy had a body (or the message has no server
/// copy). `None` means nothing is stored yet: the fetch stopped at its byte
/// cap before this message, so ask again later.
pub fn fetch_message_bodies(
    account_id: &AccountId,
    backend: &dyn MailBackend,
    store: &Store,
    message_ids: &[MessageId],
) -> Result<Vec<Option<MessageParts>>> {
    let mut wanted = Vec::new();
    let mut asked = Vec::with_capacity(message_ids.len());
    for id in message_ids {
        let request = store.body_request(account_id, id)?;
        asked.push(request.is_some());
        wanted.extend(request);
    }
    if !wanted.is_empty() {
        fetch_bodies(account_id, backend, store, &wanted)?;
    }
    message_ids
        .iter()
        .zip(asked)
        .map(|(id, asked)| {
            Ok(match store.message_body(account_id, id)? {
                Some(parts) => Some(parts),
                // Asked and still nothing stored: cut off, not answered.
                None if asked => None,
                // No server copy to ask: that is the answer.
                None => Some(MessageParts::default()),
            })
        })
        .collect()
}

/// A read-only connection owned by the foreground body worker. It keeps
/// both the login and the examined mailbox across requests.
#[derive(Default)]
pub struct ReaderBodyFetcher {
    connection: crate::imap::BodyConnection,
}

impl ReaderBodyFetcher {
    /// Open a connection without starting another embeddings indexer.
    pub fn open_store(path: &std::path::Path) -> Result<Store> {
        Store::open_for_part_fetch(path)
    }

    /// Check structure first, then fetch any missing bodies. `stored` runs
    /// after each commit so the reader can show the new information while
    /// the next command is still on the wire. Returns unresolved ids.
    pub fn fetch(
        &mut self,
        config: &crate::AccountConfig,
        store: &Store,
        ids: &[MessageId],
        cancel: &crate::CancelBodyFetch,
        mut stored: impl FnMut(),
    ) -> Result<Vec<MessageId>> {
        use crate::{BodyState, ReaderParts};

        let account = AccountId(config.id.clone());
        let mut unknown = Vec::new();
        let mut wanted = Vec::new();
        for id in ids {
            match store.reader_parts(&account, id.as_str())? {
                ReaderParts::Unknown => unknown.extend(store.body_request(&account, id)?),
                ReaderParts::Listed { .. } => wanted.extend(store.body_request(&account, id)?),
                ReaderParts::Unavailable { .. } | ReaderParts::Ready => {}
            }
        }

        let mut failed = Vec::new();
        if !unknown.is_empty() {
            let (structures, missing) = self.structures(config, cancel, unknown)?;
            if cancel.is_cancelled() {
                return Err(crate::imap::body_fetch_cancelled());
            }
            if !structures.is_empty() {
                store.store_structures(&account, &structures, true)?;
                stored();
            }
            for (id, _, _) in structures {
                if matches!(
                    store.reader_parts(&account, id.as_str())?,
                    ReaderParts::Listed { .. }
                ) {
                    wanted.extend(store.body_request(&account, &id)?);
                }
            }
            for request in missing {
                // A malformed structure does not prevent the full source
                // from supplying both the body and a verified part list.
                if store.reader_parts(&account, request.message_id.as_str())?
                    == ReaderParts::Unknown
                {
                    wanted.push(request);
                }
            }
        }

        let states = store.body_states(
            &account,
            &wanted
                .iter()
                .map(|request| request.message_id.clone())
                .collect::<Vec<_>>(),
        )?;
        let mut wanted: Vec<_> = wanted
            .into_iter()
            .zip(states)
            .filter_map(|(request, state)| (state != BodyState::TooLarge).then_some(request))
            .collect();
        while !wanted.is_empty() {
            let mut fetched =
                collect_bodies(&wanted, config.trust_verified_sender_header, |copies| {
                    self.connection
                        .bodies(|| config.imap_config(), cancel, copies)
                })?;
            if cancel.is_cancelled() {
                return Err(crate::imap::body_fetch_cancelled());
            }
            if fetched.is_empty() {
                failed.extend(wanted.into_iter().map(|request| request.message_id));
                break;
            }
            wanted.retain(|request| !fetched.iter().any(|(id, _)| *id == request.message_id));
            fetched.retain(|(id, parts)| {
                if parts.is_none() {
                    failed.push(id.clone());
                }
                parts.is_some()
            });
            if !fetched.is_empty() {
                store.store_fetched_parts_with(&account, &fetched, true)?;
                stored();
            }
        }
        // A size cap can leave the full source withheld. That is only a
        // settled result once the message has a part list to show.
        for id in ids {
            if !failed.contains(id)
                && store.reader_parts(&account, id.as_str())? == ReaderParts::Unknown
            {
                failed.push(id.clone());
            }
        }
        Ok(failed)
    }

    /// List the attachments for a batch of old mail in newest-first order.
    /// Leaves the body queue in place for the full source, which can
    /// confirm inline roles and decoded sizes.
    pub fn backfill_structures(config: &crate::AccountConfig, store: &Store) -> Result<usize> {
        let account = AccountId(config.id.clone());
        let wanted = store.missing_structures(&account, BODY_BATCH)?;
        let mut fetcher = Self::default();
        let result = fetcher.structures(config, &crate::CancelBodyFetch::default(), wanted);
        // The next background fetch opens its own connection. Finish this
        // session first so it never competes for another server login.
        fetcher.connection.logout();
        let (fetched, missing) = result?;
        let stored = store.store_structures(&account, &fetched, false)?;
        store.skip_missing_structures(&account, &missing)?;
        Ok(stored + missing.len())
    }

    fn structures(
        &mut self,
        config: &crate::AccountConfig,
        cancel: &crate::CancelBodyFetch,
        mut waiting: Vec<BodyRequest>,
    ) -> Result<(Vec<StoredStructure>, Vec<BodyRequest>)> {
        let mut found = Vec::new();
        let mut missing = Vec::new();
        let mut round = 0;
        while !waiting.is_empty() {
            let copies: Vec<_> = waiting
                .iter()
                .map(|request| request.copies[round].clone())
                .collect();
            let mut answers: HashMap<_, _> = self
                .connection
                .structures(|| config.imap_config(), cancel, &copies)?
                .into_iter()
                .collect();
            let mut next = Vec::new();
            for request in waiting {
                let copy = &request.copies[round];
                if let Some(structure) = answers.remove(copy) {
                    found.push((request.message_id, copy.clone(), structure));
                } else if request.copies.len() > round + 1 {
                    next.push(request);
                } else {
                    missing.push(request);
                }
            }
            waiting = next;
            round += 1;
        }
        Ok((found, missing))
    }
}

type StoredStructure = (MessageId, RemoteId, crate::imap::BodyStructure);

/// Fetch `wanted` and record what the server answered. Returns how many
/// were recorded.
///
/// A copy the server no longer has falls back to the message's next copy,
/// and a message with none left is recorded without parts. A copy the
/// backend left out (its byte cap was reached) keeps the message queued, and
/// no further copies are asked for in this call.
fn fetch_bodies(
    account_id: &AccountId,
    backend: &dyn MailBackend,
    store: &Store,
    wanted: &[BodyRequest],
) -> Result<usize> {
    let results = collect_bodies(wanted, backend.trust_verified_sender_header(), |copies| {
        backend.fetch_bodies(copies)
    })?;
    store.store_fetched_parts(account_id, &results)?;
    Ok(results.len())
}

fn collect_bodies(
    wanted: &[BodyRequest],
    trusted: bool,
    mut fetch: impl FnMut(&[RemoteId]) -> Result<Vec<crate::backend::FetchedBody>>,
) -> Result<Vec<(MessageId, Option<MessageParts>)>> {
    let mut results: Vec<(MessageId, Option<MessageParts>)> = Vec::new();
    // Copy `round` of each message still waiting. Most are found in round 0.
    let mut waiting: Vec<&BodyRequest> = wanted.iter().collect();
    let mut round = 0;
    while !waiting.is_empty() {
        let asks: Vec<RemoteId> = waiting
            .iter()
            .filter_map(|request| request.copies.get(round).cloned())
            .collect();
        if asks.is_empty() {
            break;
        }
        let mut answers: HashMap<RemoteId, Option<Vec<u8>>> = fetch(&asks)?
            .into_iter()
            .map(|answer| (answer.remote_id, answer.raw))
            .collect();
        let mut next = Vec::new();
        let mut capped = false;
        for request in waiting {
            let Some(asked) = request.copies.get(round) else {
                continue;
            };
            match answers.remove(asked) {
                // Parsed one at a time, so each source is dropped once read.
                Some(Some(raw)) => results.push((
                    request.message_id.clone(),
                    Some(if trusted {
                        crate::imap::parts_from_trusted_raw(&raw)
                    } else {
                        crate::imap::parts_from_raw(&raw)
                    }),
                )),
                Some(None) if round + 1 < request.copies.len() => next.push(request),
                Some(None) => results.push((request.message_id.clone(), None)),
                None => capped = true,
            }
        }
        if capped {
            break;
        }
        waiting = next;
        round += 1;
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AccountId, BackendKind};
    use crate::backend::FixtureBackend;
    use crate::message::{Address, Message, MessageHeaders, MessageId, RemoteId};
    use crate::threading::ThreadId;

    #[test]
    fn fixture_sync_populates_store() {
        let account = Account {
            id: AccountId("a1".into()),
            display_name: "Caleb".into(),
            email: "c@x".into(),
            backend_kind: BackendKind::Bridge,
        };
        let msg = Message {
            account_id: account.id.clone(),
            headers: MessageHeaders {
                message_id: MessageId::new("m1@x"),
                proton_conversation_id: None,
                in_reply_to: None,
                references: vec![],
                from: Address {
                    name: None,
                    email: "from@x".into(),
                },
                subject: "Hello".into(),
                date_unix: 1,
            },
            remote_id: RemoteId("r1".into()),
            thread_id: ThreadId::from_message_id(&MessageId::new("m1@x")),
            snippet: "Hello".into(),
            body_text: Some("Hello body".into()),
            unread: true,
            folders: Vec::new(),
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
            reply_to: Some(Vec::new()),
            attachment_names: Vec::new(),
            parts: None,
        };
        let backend = FixtureBackend {
            messages: vec![msg],
        };
        let store = Store::open_in_memory().unwrap();
        let report = sync_once(&account, &backend, &store).unwrap();
        assert_eq!(report.new_messages, Some(1));
        assert_eq!(store.list_threads(&account.id).unwrap().len(), 1);
    }

    #[test]
    fn body_fetches_keep_attestation_until_a_trusted_refresh_clears_it() {
        struct Source<'a> {
            raw: &'a [u8],
            trusted: bool,
        }

        impl MailBackend for Source<'_> {
            fn kind(&self) -> BackendKind {
                BackendKind::Imap
            }

            fn trust_verified_sender_header(&self) -> bool {
                self.trusted
            }

            fn fetch_changes(
                &self,
                _: Option<&crate::backend::SyncCursor>,
            ) -> Result<crate::backend::MailboxDelta> {
                Ok(Default::default())
            }

            fn fetch_bodies(
                &self,
                copies: &[RemoteId],
            ) -> Result<Vec<crate::backend::FetchedBody>> {
                Ok(copies
                    .iter()
                    .map(|remote_id| crate::backend::FetchedBody {
                        remote_id: remote_id.clone(),
                        raw: Some(self.raw.to_vec()),
                    })
                    .collect())
            }

            fn submit_draft(&self, _: crate::send::Draft) -> Result<crate::send::PendingSend> {
                unreachable!()
            }

            fn confirm_send(
                &self,
                _: crate::send::PendingSend,
                _: crate::send::UserSendConfirmation,
            ) -> Result<crate::send::SentMessage> {
                unreachable!()
            }
        }

        let store = Store::open_in_memory().unwrap();
        let account = Account {
            id: AccountId("a1".into()),
            display_name: "Reader".into(),
            email: "reader@example.test".into(),
            backend_kind: BackendKind::Imap,
        };
        store.upsert_account(&account).unwrap();
        let plain = b"From: sender@example.test\r\nMessage-ID: <m@example.test>\r\n\r\nBody";
        let verified = b"From: sender@example.test\r\nMessage-ID: <m@example.test>\r\nX-Fell-Verified-Sender: sender@example.test\r\n\r\nBody";
        let mut message = crate::imap::to_message(&account.id, "INBOX", "INBOX", 1, 1, &[], plain);
        let request = BodyRequest {
            message_id: message.headers.message_id.clone(),
            copies: vec![message.remote_id.clone()],
        };
        let attestation = || {
            store
                .message_body(&account.id, &request.message_id)
                .unwrap()
                .and_then(|parts| {
                    parts
                        .bulk_headers
                        .into_iter()
                        .find(|(name, _)| name == crate::message::VERIFIED_SENDER_HEADER)
                        .map(|(_, address)| address)
                })
        };
        message.parts = None;
        store.upsert_message(&message).unwrap();
        let fetch = |raw, trusted| {
            fetch_bodies(
                &account.id,
                &Source { raw, trusted },
                &store,
                std::slice::from_ref(&request),
            )
            .unwrap();
        };
        fetch(verified, false);
        assert_eq!(attestation(), None);
        fetch(verified, true);
        assert_eq!(attestation().as_deref(), Some("sender@example.test"));
        fetch(plain, false);
        message.parts = Some(crate::imap::parts_from_raw(plain));
        store.upsert_message(&message).unwrap();
        assert_eq!(attestation().as_deref(), Some("sender@example.test"));
        fetch(plain, true);
        assert_eq!(attestation(), None);
    }
}
