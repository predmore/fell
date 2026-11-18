//! Fixture mailbox used until read-only IMAP is on main.
//!
//! Messages are written through the store. The window only reads them back.

use fell_core::message::{Address, Message, MessageHeaders, MessageId, RemoteId};
use fell_core::{Account, AccountId, BackendKind, FixtureBackend, Store, ThreadId};
use fell_core::{SyncReport, sync_once};

use std::time::{SystemTime, UNIX_EPOCH};

pub fn fixture_account() -> Account {
    Account {
        id: AccountId("local".into()),
        display_name: "Caleb".into(),
        email: "caleb@fell.test".into(),
        backend_kind: BackendKind::Bridge,
    }
}

pub fn seed_if_empty(store: &Store, account: &Account) -> fell_core::Result<SyncReport> {
    if !store.list_threads(&account.id)?.is_empty() {
        return Ok(SyncReport::default());
    }
    let backend = FixtureBackend {
        messages: fixture_messages(account),
    };
    sync_once(account, &backend, store)
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

struct DraftMail {
    id: &'static str,
    name: &'static str,
    email: &'static str,
    subject: &'static str,
    body: &'static str,
    ago: i64,
    unread: bool,
    folders: &'static [&'static str],
    labels: &'static [&'static str],
    reply_to: Option<&'static str>,
    proton: Option<&'static str>,
    starred: bool,
    attachment: bool,
}

fn fixture_messages(account: &Account) -> Vec<Message> {
    let stamp = now();
    let rows = [
        DraftMail {
            id: "offsite@fell.test",
            name: "Caleb",
            email: "caleb@fell.test",
            subject: "Offsite plan for March",
            body: "Booked the lodge for the 12th to the 14th. Agenda to follow; send me anything you want on it by Friday.",
            ago: 5 * 60,
            unread: false,
            // Sent to the team list, which delivers a copy back to the inbox.
            folders: &["Sent", "INBOX"],
            labels: &["work"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "thread-a-1@fell.test",
            name: "Jon",
            email: "jon@fell.test",
            subject: "Sent copies in the thread",
            body: "Can we keep Sent in the thread?\n\nI don't want a second sidebar for it.",
            ago: 2 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["work"],
            reply_to: None,
            proton: Some("sent-copies"),
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "thread-a-2@fell.test",
            name: "Caleb",
            email: "caleb@fell.test",
            subject: "Re: Sent copies in the thread",
            body: "The list stays one folder wide. Your reply in Sent still shows up here.\n\n-- \nCaleb\n\n> Can we keep Sent in the thread?",
            ago: 86_400,
            unread: false,
            folders: &["Sent"],
            labels: &["work"],
            reply_to: Some("thread-a-1@fell.test"),
            proton: Some("sent-copies"),
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "thread-a-3@fell.test",
            name: "Jon",
            email: "jon@fell.test",
            subject: "Re: Sent copies in the thread",
            body: "Yes. m moves the copies in this folder. The Sent copy stays in Sent.\n\n-- \nJon\n\n> Can we keep Sent in the thread?\n> I don't want a second sidebar for it.\n>> The list stays one folder wide.\n> > Your reply in Sent still shows up here.",
            ago: 12 * 60,
            unread: true,
            folders: &["INBOX"],
            labels: &["work"],
            reply_to: Some("thread-a-2@fell.test"),
            proton: Some("sent-copies"),
            starred: true,
            attachment: false,
        },
        DraftMail {
            id: "rows@fell.test",
            name: "Ada",
            email: "ada@fell.test",
            subject: "Row height for the inbox",
            body: "Two lines is enough: who, then the subject and a snippet. The paperclip is the diagram.",
            ago: 40 * 60,
            unread: true,
            folders: &["INBOX"],
            labels: &["design"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: true,
        },
        DraftMail {
            id: "narrow@fell.test",
            name: "Maya",
            email: "maya@fell.test",
            subject: "Narrow window check",
            body: "When the tile gets narrow, show one pane. Enter opens, Esc comes back.",
            ago: 2 * 3600,
            unread: true,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "bridge@fell.test",
            name: "Jon",
            email: "jon@fell.test",
            subject: "Bridge on port 1143",
            body: "STARTTLS before LOGIN. The UI should not wait on it.",
            ago: 3 * 3600,
            unread: false,
            folders: &["INBOX"],
            labels: &["work"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "dinner@fell.test",
            name: "Sam",
            email: "sam@fell.test",
            subject: "Thursday dinner",
            body: "Same place as last time, 7pm. I'll grab the corner table.",
            ago: 26 * 3600,
            unread: false,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "tiles@fell.test",
            name: "Maya",
            email: "maya@fell.test",
            subject: "Hyprland tiles",
            body: "A half-width tile should not crush the split. One pane, same keys.",
            ago: 3 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["design"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "tickets@fell.test",
            name: "Maya",
            email: "maya@fell.test",
            subject: "Tickets for Friday",
            body: "Two seats, aisle. I'll forward the PDF when it lands.",
            ago: 4 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["travel"],
            reply_to: None,
            proton: None,
            starred: true,
            attachment: true,
        },
        DraftMail {
            id: "qresync@fell.test",
            name: "Jon",
            email: "jon@fell.test",
            subject: "QRESYNC notes",
            body: "CONDSTORE when the server has it. Otherwise just the new UIDs.",
            ago: 5 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "font@fell.test",
            name: "Ada",
            email: "ada@fell.test",
            subject: "Font on Omarchy",
            body: "Use the UI face and the size Omarchy already set. Don't invent a third.",
            ago: 6 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["design"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "weekly@fell.test",
            name: "Weekly",
            email: "notes@fell.test",
            subject: "Weekly notes",
            body: "A short note on native text.\n\nQuoted history stays folded.\n\n> older weekly\n> still folded",
            ago: 9 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "labels@fell.test",
            name: "Ada",
            email: "ada@fell.test",
            subject: "Labels are not folders",
            body: "A chip on the row, not a sidebar. l will edit them later.",
            ago: 8 * 86_400,
            unread: false,
            folders: &["INBOX"],
            labels: &["work", "design"],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "intro@fell.test",
            name: "Caleb",
            email: "caleb@fell.test",
            subject: "Introducing Fell",
            body: "Threads first, local store, no send from the agent.",
            ago: 5 * 86_400,
            unread: false,
            folders: &["Sent"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "invoice@fell.test",
            name: "Caleb",
            email: "caleb@fell.test",
            subject: "Re: Invoice",
            body: "Draft only. Not sent.",
            ago: 6 * 3600,
            unread: true,
            folders: &["Drafts"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "hosting@fell.test",
            name: "Sam",
            email: "sam@fell.test",
            subject: "Old hosting bill",
            body: "Already paid. Sitting in Archive.",
            ago: 20 * 86_400,
            unread: false,
            folders: &["Archive"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
        DraftMail {
            id: "spam@fell.test",
            name: "Prize",
            email: "prize@example.test",
            subject: "You have won",
            body: "This stays in Spam.",
            ago: 12 * 86_400,
            unread: true,
            folders: &["Spam"],
            labels: &[],
            reply_to: None,
            proton: None,
            starred: false,
            attachment: false,
        },
    ];
    rows.into_iter()
        .map(|row| message(account, stamp, row))
        .collect()
}

/// Who each fixture message went to. Mail to the person by default, and a
/// longer list on the first thread so the conversation shows cc, "+N more",
/// and the Bcc Fell keeps on the person's own sent copy.
type People = &'static [(&'static str, &'static str)];

fn recipients(row: &DraftMail, me: &str) -> (People, People, People) {
    const TEAM: People = &[("Ada", "ada@fell.test"), ("Maya", "maya@fell.test")];
    const EVERYONE: People = &[
        ("Ada", "ada@fell.test"),
        ("Maya", "maya@fell.test"),
        ("Sam", "sam@fell.test"),
        ("Priya Natarajan", "priya@fell.test"),
    ];
    match row.id {
        "thread-a-1@fell.test" => (&[("Caleb", "caleb@fell.test")], TEAM, &[]),
        "thread-a-2@fell.test" => (
            &[("Jon", "jon@fell.test")],
            TEAM,
            &[("Sam", "sam@fell.test")],
        ),
        "thread-a-3@fell.test" => (&[("Caleb", "caleb@fell.test")], EVERYONE, &[]),
        "offsite@fell.test" => (
            &[("Ada", "ada@fell.test")],
            &[
                ("Maya", "maya@fell.test"),
                ("Sam", "sam@fell.test"),
                ("Priya Natarajan", "priya@fell.test"),
                ("Jon", "jon@fell.test"),
                ("Bob Stone", "bob@fell.test"),
                ("Lena Ortiz", "lena@fell.test"),
            ],
            &[("Riya", "riya@fell.test")],
        ),
        _ if row.email == me => (&[("Jon", "jon@fell.test")], &[], &[]),
        _ => (&[("Caleb", "caleb@fell.test")], &[], &[]),
    }
}

fn people(list: People) -> Vec<Address> {
    list.iter()
        .map(|(name, email)| Address {
            name: Some((*name).into()),
            email: (*email).into(),
        })
        .collect()
}

fn message(account: &Account, now: i64, row: DraftMail) -> Message {
    let (to, cc, bcc) = recipients(&row, &account.email);
    let references = row.reply_to.map(MessageId::new).into_iter().collect();
    Message {
        account_id: account.id.clone(),
        headers: MessageHeaders {
            message_id: MessageId::new(row.id),
            proton_conversation_id: row.proton.map(str::to_string),
            in_reply_to: row.reply_to.map(MessageId::new),
            references,
            from: Address {
                name: Some(row.name.into()),
                email: row.email.into(),
            },
            subject: row.subject.into(),
            date_unix: now.saturating_sub(row.ago),
        },
        remote_id: RemoteId(format!("fixture:{}", row.id)),
        thread_id: ThreadId::from_message_id(&MessageId::new(row.id)),
        snippet: first_line(row.body),
        body_text: Some(row.body.into()),
        unread: row.unread,
        folders: row
            .folders
            .iter()
            .map(|folder| (*folder).to_string())
            .collect(),
        labels: row
            .labels
            .iter()
            .map(|label| (*label).to_string())
            .collect(),
        arrived_via: vec![account.email.clone()],
        to: people(to),
        cc: people(cc),
        bcc: people(bcc),
        starred: row.starred,
        has_attachment: row.attachment,
        archived: false,
        snooze_until: None,
        list_id: None,
        reply_to: reply_address(row.id),
        attachment_names: attachment_names(&row),
        parts: None,
    }
}

/// The newsletter answers at its own address, so `r` on it shows Reply-To.
fn reply_address(id: &str) -> Vec<Address> {
    match id {
        "weekly@fell.test" => people(&[("Weekly replies", "weekly-replies@fell.test")]),
        _ => Vec::new(),
    }
}

/// Names for the fixture attachments, as a forward lists them.
fn attachment_names(row: &DraftMail) -> Vec<String> {
    if !row.attachment {
        return Vec::new();
    }
    let names: &[&str] = match row.id {
        "rows@fell.test" => &["inbox-rows.png"],
        "tickets@fell.test" => &["tickets-friday.pdf", "seat-map.png"],
        _ => &["attachment"],
    };
    names.iter().map(|name| (*name).to_string()).collect()
}

fn first_line(body: &str) -> String {
    body.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixtures_land_in_the_store_and_span_folders() {
        let store = Store::open_in_memory().unwrap();
        let account = fixture_account();
        let report = seed_if_empty(&store, &account).unwrap();
        assert!(report.upserted > 10);
        let inbox = store.folder_threads(&account.id, "INBOX").unwrap();
        assert!(inbox.len() >= 10);
        let sent_copy = inbox
            .iter()
            .find(|thread| thread.message_count > 1)
            .unwrap();
        let messages = store
            .thread_messages(&account.id, &sent_copy.thread_id)
            .unwrap();
        assert!(
            messages
                .iter()
                .any(|message| message.folders.iter().any(|folder| folder == "Sent"))
        );
        assert!(
            messages
                .iter()
                .any(|message| message.folders.iter().any(|folder| folder == "INBOX"))
        );
        assert!(inbox.iter().any(|thread| thread.starred));
        assert!(inbox.iter().any(|thread| thread.has_attachment));
        assert_eq!(
            store.folder_threads(&account.id, "Drafts").unwrap().len(),
            1
        );
        let again = seed_if_empty(&store, &account).unwrap();
        assert_eq!(again.upserted, 0);
    }
}
