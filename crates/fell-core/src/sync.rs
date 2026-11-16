use crate::Result;
use crate::account::Account;
use crate::backend::MailBackend;
use crate::store::Store;

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
    store.upsert_account(account)?;
    let cursor = store.sync_cursor(&account.id)?;
    let delta = backend.fetch_changes(cursor.as_ref())?;
    let upserted = delta.messages.len();
    store.apply_delta(&account.id, &delta)?;
    Ok(SyncReport { upserted })
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
