//! Bcc recipients of mail sent from Fell.
//!
//! A message's Bcc is on the SMTP envelope, not in its header, so the copy
//! the server files in Sent has none. After a send, the editor records the
//! Bcc here by Message-ID. It is joined in only for the person's own reads:
//! the conversation view and the "already mailed" check. It is never copied
//! into the message row or the search index, so a fell-mcp agent that was
//! not shown a draft's Bcc cannot find it by searching. The record never
//! leaves the machine, and goes with the last stored copy of its message.

use rusqlite::{Connection, OptionalExtension, params};

use super::{Store, immediate_transaction, union_addresses};
use crate::Result;
use crate::account::AccountId;
use crate::message::{Address, Message, MessageId, decode_addresses};

pub(super) const SENT_BCC_TABLE: &str = r#"
CREATE TABLE IF NOT EXISTS sent_bcc (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    bcc_addrs TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
);
"#;

impl Store {
    /// Remember `bcc` for the message Fell sent as `message_id`. Recording
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
        tx.commit()
    }
}

/// Add each message's recorded Bcc, found by its own id or any id that
/// collapsed into it, to `messages`. For the person's own view only.
pub(super) fn add_recorded_bcc(conn: &Connection, messages: &mut [Message]) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT bcc_addrs FROM sent_bcc
         WHERE account_id = ?1 AND (
             message_id = ?2
             OR message_id IN (
                 SELECT message_id FROM message_aliases
                 WHERE account_id = ?1 AND canonical_message_id = ?2
             )
         )",
    )?;
    for message in messages {
        let rows = stmt.query_map(
            params![
                message.account_id.as_str(),
                message.headers.message_id.as_str()
            ],
            |row| row.get::<_, String>(0),
        )?;
        for recorded in rows {
            for addr in decode_addresses(&recorded?) {
                if !message
                    .bcc
                    .iter()
                    .any(|have| have.email.eq_ignore_ascii_case(&addr.email))
                {
                    message.bcc.push(addr);
                }
            }
        }
    }
    Ok(())
}

/// Every address the person Bcc'd from Fell on this account.
pub(crate) fn recorded_bcc_addresses(
    conn: &Connection,
    account_id: &AccountId,
) -> Result<Vec<Address>> {
    let mut stmt = conn.prepare("SELECT bcc_addrs FROM sent_bcc WHERE account_id = ?1")?;
    let rows = stmt.query_map(params![account_id.as_str()], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for recorded in rows {
        out.extend(decode_addresses(&recorded?));
    }
    Ok(out)
}

/// Drop the record once no stored copy of `message_id` is left: neither a
/// row with that id nor an alias of one.
pub(super) fn forget_if_gone(
    conn: &Connection,
    account_id: &AccountId,
    message_id: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM sent_bcc
         WHERE account_id = ?1 AND message_id = ?2
           AND NOT EXISTS (
               SELECT 1 FROM messages WHERE account_id = ?1 AND message_id = ?2
           )
           AND NOT EXISTS (
               SELECT 1 FROM message_aliases WHERE account_id = ?1 AND message_id = ?2
           )",
        params![account_id.as_str(), message_id],
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
            reply_to: Vec::new(),
            attachment_names: Vec::new(),
            parts: None,
        }
    }

    fn row_bcc_line(store: &Store, id: &str) -> String {
        store
            .conn
            .query_row(
                "SELECT bcc_line FROM messages WHERE account_id = 'a1' AND message_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// Bcc as the person's conversation view reads it.
    fn shown_bcc(store: &Store, id: &str) -> Vec<String> {
        let thread: String = store
            .conn
            .query_row(
                "SELECT thread_id FROM messages WHERE account_id = 'a1' AND message_id = ?1",
                [id],
                |row| row.get(0),
            )
            .unwrap();
        store
            .thread_messages(&account(), &ThreadId(thread))
            .unwrap()
            .into_iter()
            .find(|message| message.headers.message_id.as_str() == id)
            .unwrap()
            .bcc
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
    fn a_sent_copy_that_syncs_later_shows_the_recorded_bcc() {
        let store = store();
        store
            .record_sent_bcc(&account(), &MessageId::new("<m1@compose.fell>"), &hidden())
            .unwrap();
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        assert_eq!(shown_bcc(&store, "m1@compose.fell"), ["hidden@example.com"]);
        let id = MessageId::new("m1@compose.fell");
        let thread = ThreadId::from_message_id(&id);
        let thread = store.thread_messages(&account(), &thread).unwrap();
        assert_eq!(thread[0].bcc, hidden());
    }

    #[test]
    fn a_recorded_bcc_never_reaches_the_row_or_search() {
        let store = store();
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        store
            .record_sent_bcc(&account(), &MessageId::new("m1@compose.fell"), &hidden())
            .unwrap();
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        assert_eq!(shown_bcc(&store, "m1@compose.fell"), ["hidden@example.com"]);
        // An agent that was not shown the draft's Bcc cannot find it.
        assert_eq!(row_bcc_line(&store, "m1@compose.fell"), "");
        assert!(found_by(&store, "hidden").is_empty());
        assert!(found_by(&store, "to:hidden@example.com").is_empty());
    }

    #[test]
    fn recording_twice_keeps_one_address() {
        let store = store();
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        let id = MessageId::new("m1@compose.fell");
        store.record_sent_bcc(&account(), &id, &hidden()).unwrap();
        store.record_sent_bcc(&account(), &id, &hidden()).unwrap();
        assert_eq!(shown_bcc(&store, "m1@compose.fell"), ["hidden@example.com"]);
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
        assert!(
            store
                .mailed_addresses(&account())
                .unwrap()
                .contains("hidden@example.com")
        );
    }

    #[test]
    fn a_sent_copy_that_collapsed_into_its_list_echo_still_shows_its_bcc() {
        let store = store();
        let body = "Here is the long version of the plan for Friday, with the details.";
        let mut list = sent_copy("list-1@lists.example");
        list.headers.subject = "[team] Plans".into();
        list.list_id = Some("team.lists.example".into());
        list.folders = vec!["INBOX".into()];
        list.body_text = Some(body.into());
        list.snippet = body.into();
        list.headers.date_unix = 20;
        store.upsert_message(&list).unwrap();
        let mut sent = sent_copy("m1@compose.fell");
        sent.body_text = Some(body.into());
        sent.snippet = body.into();
        store.upsert_message(&sent).unwrap();
        // The Sent copy is now an alias of the list row.
        let canonical: String = store
            .conn
            .query_row(
                "SELECT canonical_message_id FROM message_aliases WHERE message_id = 'm1@compose.fell'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canonical, "list-1@lists.example");
        store
            .record_sent_bcc(&account(), &MessageId::new("m1@compose.fell"), &hidden())
            .unwrap();
        assert_eq!(
            shown_bcc(&store, "list-1@lists.example"),
            ["hidden@example.com"]
        );
        assert!(found_by(&store, "hidden").is_empty());
    }

    #[test]
    fn the_record_goes_with_the_last_copy() {
        let store = store();
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        let id = MessageId::new("m1@compose.fell");
        store.record_sent_bcc(&account(), &id, &hidden()).unwrap();
        assert!(store.delete_message(&account(), &id).unwrap());
        // A later message reusing that id does not inherit the Bcc.
        store.upsert_message(&sent_copy("m1@compose.fell")).unwrap();
        assert!(shown_bcc(&store, "m1@compose.fell").is_empty());
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
        assert!(shown_bcc(&store, "other@x").is_empty());
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
