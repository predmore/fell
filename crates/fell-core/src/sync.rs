use crate::Result;
use crate::account::{Account, AccountId};
use crate::backend::MailBackend;
use crate::message::{MessageId, MessageParts, RemoteId};
use crate::store::{BodyRequest, Store};

/// Messages per backfill batch. Each batch is one fetch and one commit.
const BODY_BATCH: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncReport {
    pub upserted: usize,
}

/// Pull a backend delta into the local store. The UI should call this off the frame path.
pub fn sync_once(
    account: &Account,
    backend: &dyn MailBackend,
    store: &Store,
) -> Result<SyncReport> {
    sync_with_progress(account, backend, store, &mut |_| {})
}

/// [`sync_once`], calling `progress` after each delta is committed with the
/// running count of messages written so far.
pub fn sync_with_progress(
    account: &Account,
    backend: &dyn MailBackend,
    store: &Store,
    progress: &mut dyn FnMut(&SyncReport),
) -> Result<SyncReport> {
    store.upsert_account(account)?;
    let cursor = store.sync_cursor(&account.id)?;
    let mut report = SyncReport::default();
    backend.sync_changes(cursor.as_ref(), &mut |delta| {
        let count = delta.messages.len();
        store.apply_delta(&account.id, &delta)?;
        report.upserted += count;
        if count > 0 || !delta.flags.is_empty() {
            progress(&report);
        }
        Ok(())
    })?;
    Ok(report)
}

/// Fill parts for mail stored before they were kept, newest first, in up to
/// `batches` batches of 200. Returns how many messages were recorded, with
/// or without a body; 0 means none are left to fetch. Run it
/// on the sync thread after a pass; a large old mailbox fills over several.
///
/// A copy the server no longer has, or whose mailbox it refuses, falls back
/// to the message's next copy; when none is left, the message is recorded as
/// fetched so later passes do not ask again ([`fetch_message_body`] still
/// does). A connection error stores nothing, so the batch is tried again.
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
/// For the conversation view: call it on the sync thread, never the UI
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

/// Fetch `wanted` and record every one of them. Returns how many were recorded.
fn fetch_bodies(
    account_id: &AccountId,
    backend: &dyn MailBackend,
    store: &Store,
    wanted: &[BodyRequest],
) -> Result<usize> {
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
        let answers = backend.fetch_bodies(&asks)?;
        let mut next = Vec::new();
        for request in waiting {
            let Some(asked) = request.copies.get(round) else {
                continue;
            };
            let raw = answers
                .iter()
                .find(|answer| &answer.remote_id == asked)
                .and_then(|answer| answer.raw.as_deref());
            match raw {
                Some(raw) => results.push((
                    request.message_id.clone(),
                    Some(crate::imap::parts_from_raw(raw)),
                )),
                None if round + 1 < request.copies.len() => next.push(request),
                None => results.push((request.message_id.clone(), None)),
            }
        }
        waiting = next;
        round += 1;
    }
    store.store_fetched_parts(account_id, &results)?;
    Ok(results.len())
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
            reply_to: Vec::new(),
            attachment_names: Vec::new(),
            parts: None,
        };
        let backend = FixtureBackend {
            messages: vec![msg],
        };
        let store = Store::open_in_memory().unwrap();
        let report = sync_once(&account, &backend, &store).unwrap();
        assert_eq!(report.upserted, 1);
        assert_eq!(store.list_threads(&account.id).unwrap().len(), 1);
    }
}
