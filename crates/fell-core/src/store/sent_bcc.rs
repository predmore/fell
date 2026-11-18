//! Bcc recipients of mail sent from Fell.
//!
//! A message's Bcc is on the SMTP envelope, not in its header, so the copy
//! the server files in Sent has none. After a send, the editor records the
//! Bcc here by Message-ID. When that Sent copy syncs, or if it synced first,
//! the recorded addresses join the row's `bcc_line`, so search and the
//! "already mailed" check see them. The record never leaves the machine.

use rusqlite::{Connection, OptionalExtension, params};

use super::{Store, find_stored, immediate_transaction, union_addresses};
use crate::Result;
use crate::account::AccountId;
use crate::message::{Address, Message, MessageId, decode_addresses};
use crate::search::{IndexedText, sync_terms, terms_for_indexed};

pub(super) const SENT_BCC_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS sent_bcc (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    bcc_addrs TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
);
"#;

impl Store {
    /// Remember `bcc` for the message Fell sent as `message_id`, and add it
    /// to that message's row if the Sent copy is already stored. Recording
    /// the same addresses again changes nothing.
    pub fn record_sent_bcc(
        &self,
        account_id: &AccountId,
        message_id: &MessageId,
        bcc: &[Address],
    ) -> Result<()> {
        let bcc: Vec<Address> = bcc
            .iter()
            .filter(|addr| !addr.email.trim().is_empty())
            .cloned()
            .collect();
        if bcc.is_empty() || message_id.as_str().is_empty() {
            return Ok(());
        }
        let tx = immediate_transaction(&self.conn)?;
        let existing: Option<String> = tx
            .query_row(
                "SELECT bcc_addrs FROM sent_bcc WHERE account_id = ?1 AND message_id = ?2",
                params![account_id.as_str(), message_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let recorded = union_addresses(existing.as_deref().unwrap_or(""), &bcc);
        tx.execute(
            "INSERT INTO sent_bcc (account_id, message_id, bcc_addrs) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id, message_id) DO UPDATE SET bcc_addrs = excluded.bcc_addrs",
            params![account_id.as_str(), message_id.as_str(), recorded],
        )?;
        add_to_stored_row(&tx, account_id, message_id, &bcc)?;
        tx.commit()
    }
}

/// `message` with any Bcc recorded for it, or `None` when there is none.
pub(super) fn with_recorded_bcc(conn: &Connection, message: &Message) -> Result<Option<Message>> {
    let recorded: Option<String> = conn
        .query_row(
            "SELECT bcc_addrs FROM sent_bcc WHERE account_id = ?1 AND message_id = ?2",
            params![
                message.account_id.as_str(),
                message.headers.message_id.as_str()
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(recorded) = recorded else {
        return Ok(None);
    };
    let mut merged = message.clone();
    for addr in decode_addresses(&recorded) {
        if !merged
            .bcc
            .iter()
            .any(|have| have.email.eq_ignore_ascii_case(&addr.email))
        {
            merged.bcc.push(addr);
        }
    }
    Ok(Some(merged))
}

/// Union `bcc` into the stored row for `message_id`, with its search terms.
fn add_to_stored_row(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &MessageId,
    bcc: &[Address],
) -> Result<()> {
    let Some((stored, _)) = find_stored(conn, account_id, message_id)? else {
        return Ok(());
    };
    let bcc_line = union_addresses(&stored.bcc_line, bcc);
    if bcc_line == stored.bcc_line {
        return Ok(());
    }
    let indexed = |bcc_line: &str| {
        terms_for_indexed(IndexedText {
            subject: &stored.subject,
            from_name: stored.from_name.as_deref(),
            from_email: &stored.from_email,
            body: &stored.body,
            to_line: &stored.to_line,
            cc_line: &stored.cc_line,
            bcc_line,
            folders: &stored.folders,
            labels: &stored.labels,
        })
    };
    sync_terms(
        conn,
        account_id.as_str(),
        &indexed(&stored.bcc_line),
        &indexed(&bcc_line),
    )?;
    conn.execute(
        "UPDATE messages SET bcc_line = ?1 WHERE account_id = ?2 AND message_id = ?3",
        params![bcc_line, account_id.as_str(), stored.canonical_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, BackendKind};
    use crate::message::{MessageHeaders, RemoteId};
    use crate::threading::ThreadId;

    fn store() -> Store {
        let store = Store::open_in_memory().unwrap();
        store
            .upsert_account(&Account {
                id: AccountId("a1".into()),
                display_name: "Me".into(),
                email: "me@x".into(),
                backend_kind: BackendKind::Bridge,
            })
            .unwrap();
        store
    }

    fn account() -> AccountId {
        AccountId("a1".into())
    }

    fn hidden() -> Vec<Address> {
        vec![Address {
            name: Some("Hidden Person".into()),
            email: "hidden@example.com".into(),
        }]
    }

    /// The copy Bridge files in Sent: a To, no Bcc header.
    fn sent_copy(id: &str) -> Message {
        Message {
            account_id: account(),
            headers: MessageHeaders {
                message_id: MessageId::new(id),
                proton_conversation_id: None,
                in_reply_to: None,
                references: Vec::new(),
                from: Address {
                    name: None,
                    email: "me@x".into(),
                },
                subject: "Plans".into(),
                date_unix: 10,
            },
            remote_id: RemoteId(format!("sent:{id}")),
            thread_id: ThreadId::from_message_id(&MessageId::new(id)),
            snippet: "See you Friday".into(),
            body_text: Some("See you Friday".into()),
            unread: false,
            folders: vec!["Sent".into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: vec![Address {
                name: None,
                email: "ada@example.com".into(),
            }],
            cc: Vec::new(),
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
        }
    }

    fn stored_bcc(store: &Store, id: &str) -> Vec<String> {
        let line: String = store
            .conn
            .query_row(
                "SELECT bcc_line FROM messages WHERE account_id = 'a1' AND message_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        decode_addresses(&line)
            .into_iter()
            .map(|addr| addr.email)
            .collect()
    }

    fn found_by(store: &Store, query: &str) -> Vec<String> {
        store
            .search(&account(), query)
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.message_id.as_str().to_string())
            .collect()
    }

    #[test]
    fn a_sent_copy_that_syncs_later_gets_the_recorded_bcc() {
        let store = store();
        store
            .record_sent_bcc(&account(), &MessageId::new("<m1@compose.fell>"), &hidden())
            .unwrap();
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        assert_eq!(
            stored_bcc(&store, "m1@compose.fell"),
            ["hidden@example.com"]
        );
        let thread = store
            .thread_messages(
                &account(),
                &ThreadId::from_message_id(&MessageId::new("m1@compose.fell")),
            )
            .unwrap();
        assert_eq!(thread[0].bcc, hidden());
        assert_eq!(found_by(&store, "hidden"), ["m1@compose.fell"]);
    }

    #[test]
    fn a_sent_copy_that_synced_first_is_updated_and_searchable() {
        let store = store();
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        assert!(found_by(&store, "hidden").is_empty());
        store
            .record_sent_bcc(&account(), &MessageId::new("m1@compose.fell"), &hidden())
            .unwrap();
        assert_eq!(
            stored_bcc(&store, "m1@compose.fell"),
            ["hidden@example.com"]
        );
        assert_eq!(found_by(&store, "hidden"), ["m1@compose.fell"]);
        // A resync of the same Sent copy keeps the Bcc.
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        assert_eq!(
            stored_bcc(&store, "m1@compose.fell"),
            ["hidden@example.com"]
        );
    }

    #[test]
    fn recording_twice_keeps_one_address_and_one_search_posting() {
        let store = store();
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        let id = MessageId::new("m1@compose.fell");
        store.record_sent_bcc(&account(), &id, &hidden()).unwrap();
        store.record_sent_bcc(&account(), &id, &hidden()).unwrap();
        assert_eq!(
            stored_bcc(&store, "m1@compose.fell"),
            ["hidden@example.com"]
        );
        let df: i64 = store
            .conn
            .query_row(
                "SELECT df FROM search_terms WHERE account_id = 'a1' AND term = 'hidden'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(df, 1);
    }

    #[test]
    fn a_bcc_recipient_counts_as_already_mailed() {
        let store = store();
        assert!(
            !store
                .mailed_addresses(&account())
                .unwrap()
                .contains("hidden@example.com")
        );
        store
            .record_sent_bcc(&account(), &MessageId::new("m1@compose.fell"), &hidden())
            .unwrap();
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        assert!(
            store
                .mailed_addresses(&account())
                .unwrap()
                .contains("hidden@example.com")
        );
    }

    #[test]
    fn a_record_does_not_touch_other_messages_or_accounts() {
        let store = store();
        store.upsert_message(&sent_copy("other@x")).unwrap();
        store
            .record_sent_bcc(
                &AccountId("a2".into()),
                &MessageId::new("other@x"),
                &hidden(),
            )
            .unwrap();
        store
            .record_sent_bcc(&account(), &MessageId::new("m1@compose.fell"), &hidden())
            .unwrap();
        assert!(stored_bcc(&store, "other@x").is_empty());
        assert!(found_by(&store, "hidden").is_empty());
    }

    #[test]
    fn an_empty_bcc_records_nothing() {
        let store = store();
        store
            .record_sent_bcc(&account(), &MessageId::new("m1@compose.fell"), &[])
            .unwrap();
        let rows: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM sent_bcc", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }
}
