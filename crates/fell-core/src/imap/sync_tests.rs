use crate::LabelChange;
use crate::account::{Account, AccountId, BackendKind};
use crate::backend::{BridgeBackend, ImapBackend, MailBackend};
use crate::folders::FolderRole;
use crate::imap::{ImapConfig, ImapSecurity, InboxWait};
use crate::store::Store;
use crate::sync::sync_once;
use crate::threading::ThreadId;

use super::test_server::{
    BodyForm, FixtureMailbox, FixtureMessage, TestServer, TestState, raw_message,
};

fn account(id: &str, kind: BackendKind) -> Account {
    Account {
        id: AccountId(id.into()),
        display_name: "Caleb".into(),
        email: "c@x".into(),
        backend_kind: kind,
    }
}

fn config(port: u16) -> ImapConfig {
    ImapConfig {
        host: "127.0.0.1".into(),
        port,
        username: "caleb".into(),
        password: "secret".into(),
        security: ImapSecurity::Plain,
        accept_invalid_certs: false,
        folder_overrides: Vec::new(),
    }
}

fn sample_state() -> TestState {
    TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![
            FixtureMailbox {
                name: "Parent".into(),
                list_flags: "\\Noselect".into(),
                uidvalidity: 1,
                uidnext: None,
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: Vec::new(),
            },
            FixtureMailbox {
                name: "Sent".into(),
                list_flags: String::new(),
                uidvalidity: 10,
                uidnext: None,
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message(
                        "root@x",
                        None,
                        "Hello",
                        "Ann <ann@x>",
                        "hello body",
                        "X-Pm-Conversation-Id: conv-9\r\n",
                    ),
                }],
            },
            FixtureMailbox {
                name: "INBOX".into(),
                list_flags: String::new(),
                uidvalidity: 20,
                uidnext: None,
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![
                    FixtureMessage {
                        uid: 1,
                        flags: vec!["\\Seen".into(), "work".into()],
                        modseq: 1,
                        body_form: BodyForm::Literal,
                        raw: raw_message(
                            "root@x",
                            None,
                            "Hello",
                            "Ann <ann@x>",
                            "hello body",
                            "X-Pm-Conversation-Id: conv-9\r\nDelivered-To: alias@x.com\r\n",
                        ),
                    },
                    FixtureMessage {
                        uid: 2,
                        flags: vec![],
                        modseq: 1,
                        body_form: BodyForm::Literal,
                        raw: raw_message(
                            "reply@x",
                            Some("root@x"),
                            "Re: Hello",
                            "Bob <bob@x>",
                            "reply body",
                            "X-Pm-Conversation-Id: conv-9\r\n",
                        ),
                    },
                    FixtureMessage {
                        uid: 3,
                        flags: vec!["\\Deleted".into()],
                        modseq: 1,
                        body_form: BodyForm::Literal,
                        raw: raw_message("gone@x", None, "Gone", "Ann <ann@x>", "gone", ""),
                    },
                    FixtureMessage {
                        uid: 4,
                        flags: vec!["\\Seen".into()],
                        modseq: 1,
                        body_form: BodyForm::Literal,
                        raw: b"From: Ann <ann@x>\r\nSubject: Html\r\nMessage-ID: <html@x>\r\nDate: Mon, 16 Nov 2026 12:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><p>Hello from html</p></body></html>".to_vec(),
                    },
                    FixtureMessage {
                        uid: 5,
                        flags: vec!["\\Seen".into()],
                        modseq: 1,
                        body_form: BodyForm::Literal,
                        raw: b"From: Ann <ann@x>\r\nSubject: No id\r\nDate: Mon, 16 Nov 2026 12:00:00 +0000\r\n\r\nmissing id\r\n".to_vec(),
                    },
                ],
            },
            FixtureMailbox {
                name: "&Jjo-".into(),
                list_flags: String::new(),
                uidvalidity: 7,
                uidnext: None,
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message(
                        "smile@x",
                        None,
                        "Smile",
                        "Ann <ann@x>",
                        "smile body",
                        "",
                    ),
                }],
            },
        ],
    }
}

fn commands(server: &TestServer) -> Vec<String> {
    server.state.lock().expect("state").commands.clone()
}

#[test]
fn local_server_syncs_into_the_store() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert!(report.upserted >= 4);

    let threads = store.list_threads(&acct.id).unwrap();
    let proton = threads
        .iter()
        .find(|thread| thread.thread_id == ThreadId::proton("conv-9"))
        .expect("proton thread");
    assert_eq!(proton.message_count, 2);
    assert!(proton.unread);
    let msgs = store.thread_messages(&acct.id, &proton.thread_id).unwrap();
    let root = msgs
        .iter()
        .find(|message| message.headers.message_id.as_str() == "root@x")
        .unwrap();
    assert_eq!(root.folders, vec!["INBOX", "Sent"]);
    assert_eq!(root.labels, vec!["work"]);
    assert_eq!(root.arrived_via, vec!["alias@x.com"]);
    assert!(!root.unread);
    assert_eq!(root.body_text.as_deref(), Some("hello body"));
    assert!(root.headers.date_unix > 0);

    let all = store.list_threads(&acct.id).unwrap();
    let subjects: Vec<_> = all.iter().map(|thread| thread.subject.clone()).collect();
    assert!(!subjects.iter().any(|subject| subject == "Gone"));
    assert!(subjects.iter().any(|subject| subject == "Html"));
    let html = store
        .thread_messages(
            &acct.id,
            &all.iter()
                .find(|thread| thread.subject == "Html")
                .unwrap()
                .thread_id,
        )
        .unwrap();
    let html_body = html[0].body_text.as_deref().unwrap();
    assert!(html_body.contains("Hello from html"));
    assert!(!html_body.contains("<p>"));

    let smile = all.iter().find(|thread| thread.subject == "Smile").unwrap();
    let smile_msg = store.thread_messages(&acct.id, &smile.thread_id).unwrap();
    assert_eq!(smile_msg[0].folders, vec!["☺"]);

    let no_id = all.iter().find(|thread| thread.subject == "No id").unwrap();
    let no_id_msg = store.thread_messages(&acct.id, &no_id.thread_id).unwrap();
    assert!(
        no_id_msg[0]
            .headers
            .message_id
            .as_str()
            .contains("uid.20.5.")
    );

    let logged = commands(&server);
    let joined = logged.join("\n");
    assert!(joined.contains("BODY.PEEK[]"));
    assert!(joined.contains("EXAMINE"));
    assert!(!joined.contains("STORE"));
    assert!(!joined.contains("APPEND"));
    assert!(!joined.contains("EXPUNGE"));
    assert!(!joined.contains(" SELECT "));
    assert!(!joined.contains("secret"));
    assert!(
        !logged
            .iter()
            .any(|line| line.contains("EXAMINE") && line.contains("Parent"))
    );

    let again = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(again.upserted, 0);
    assert_eq!(store.list_threads(&acct.id).unwrap().len(), threads.len());
}

#[test]
fn second_sync_fetches_only_new_uids() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    {
        let mut state = server.state.lock().expect("state");
        state.commands.clear();
        state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "INBOX")
            .unwrap()
            .messages
            .push(FixtureMessage {
                uid: 6,
                flags: vec![],
                modseq: 2,
                body_form: BodyForm::Literal,
                raw: raw_message("new@x", None, "New", "Ann <ann@x>", "new body", ""),
            });
    }
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 1);
    let logged = commands(&server);
    let inbox_fetch = logged
        .iter()
        .find(|line| line.contains("UID FETCH") && line.contains("6:*"))
        .expect("inbox incremental fetch");
    assert!(inbox_fetch.contains("BODY.PEEK[]"));
    assert!(
        store
            .list_threads(&acct.id)
            .unwrap()
            .iter()
            .any(|thread| thread.subject == "New")
    );
}

#[test]
fn uidvalidity_change_refetches_without_duplicating_message_ids() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let before = store.list_threads(&acct.id).unwrap().len();
    {
        let mut state = server.state.lock().expect("state");
        let inbox = state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "INBOX")
            .unwrap();
        inbox.uidvalidity = 99;
    }
    let root_thread = thread_of(&store, &acct, "root@x");
    store
        .label_thread(&acct.id, &root_thread, LabelChange::Add("kept".into()))
        .unwrap();
    let no_id_thread = thread_of(&store, &acct, "uid.20.5.INBOX@imap.fell");
    store
        .label_thread(&acct.id, &no_id_thread, LabelChange::Add("noted".into()))
        .unwrap();
    store
        .snooze_thread(&acct.id, &no_id_thread, 1_900_000_000, "test")
        .unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    // The message with no Message-ID is keyed by UIDVALIDITY: its new row
    // replaces the old one and takes over its local state. Messages that
    // have a Message-ID stay one row and lose nothing.
    assert_eq!(threads.len(), before);
    let no_id = stored(&store, &acct, "uid.99.5.INBOX@imap.fell").unwrap();
    assert!(no_id.labels.iter().any(|label| label == "noted"));
    assert_eq!(no_id.snooze_until, Some(1_900_000_000));
    assert_eq!(no_id.folders, vec!["INBOX"]);
    assert!(stored(&store, &acct, "uid.20.5.INBOX@imap.fell").is_none());
    let proton = threads
        .iter()
        .find(|thread| thread.thread_id == ThreadId::proton("conv-9"))
        .unwrap();
    assert_eq!(proton.message_count, 2);
    let root = stored(&store, &acct, "root@x").unwrap();
    assert_eq!(root.folders, vec!["INBOX", "Sent"]);
    assert!(root.labels.iter().any(|label| label == "kept"));
    for id in ["reply@x", "html@x"] {
        assert_eq!(stored(&store, &acct, id).unwrap().folders, vec!["INBOX"]);
    }
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap();
    assert!(cursor.token.contains("\t99\t"));
}

#[test]
fn condstore_updates_unread_without_a_second_copy() {
    let mut state = sample_state();
    state.condstore = true;
    state.mailboxes.retain(|mailbox| mailbox.name == "INBOX");
    state.mailboxes[0]
        .messages
        .retain(|message| message.uid == 1);
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let thread = store.list_threads(&acct.id).unwrap();
    assert!(!thread[0].unread);
    {
        let mut guard = server.state.lock().expect("state");
        let message = &mut guard.mailboxes[0].messages[0];
        message.flags.clear();
        message.modseq = 4;
        guard.commands.clear();
    }
    let report = sync_once(&acct, &backend, &store).unwrap();
    // A flag change on stored mail is a flag update, not a new message.
    assert_eq!(report.upserted, 0);
    assert_eq!(store.list_threads(&acct.id).unwrap().len(), 1);
    assert!(store.list_threads(&acct.id).unwrap()[0].unread);
    let logged = commands(&server);
    assert!(
        logged
            .iter()
            .any(|line| line.contains("UID FETCH 1:* (UID FLAGS) (CHANGEDSINCE 1)")),
        "{logged:?}"
    );
    assert!(
        !logged.iter().any(|line| line.contains("BODY.PEEK")),
        "{logged:?}"
    );
    let login_at = logged
        .iter()
        .position(|line| line.starts_with("LOGIN"))
        .unwrap();
    let enable_at = logged
        .iter()
        .position(|line| line.contains("ENABLE"))
        .unwrap();
    assert!(login_at < enable_at);
    assert!(
        logged
            .iter()
            .any(|line| line.contains("EXAMINE") && line.contains("CONDSTORE"))
    );
}

/// Commands that download a message body (`BODY.PEEK[]`, not headers).
fn body_fetches(server: &TestServer) -> Vec<String> {
    commands(server)
        .into_iter()
        .filter(|line| line.contains("BODY.PEEK[]"))
        .collect()
}

/// INBOX holding `count` unread messages of about 2 KB each, UIDs from 1.
fn large_inbox(count: u32, qresync: bool) -> TestState {
    let paragraph = "A line of a message body long enough to count for something.\r\n";
    let mut state = sample_state();
    state.condstore = true;
    state.qresync = qresync;
    state.mailboxes = vec![FixtureMailbox {
        uidnext: Some(count + 1),
        messages: (1..=count)
            .map(|uid| FixtureMessage {
                uid,
                flags: Vec::new(),
                modseq: 1,
                body_form: BodyForm::Literal,
                raw: raw_message(
                    &format!("m{uid}@x"),
                    None,
                    &format!("Message {uid}"),
                    "Ann <ann@x>",
                    &paragraph.repeat(32),
                    "",
                ),
            })
            .collect(),
        ..empty_mailbox("INBOX", 7)
    }];
    state
}

#[test]
fn a_seen_change_on_stored_mail_fetches_flags_not_bodies() {
    for qresync in [false, true] {
        let server = TestServer::spawn(large_inbox(500, qresync));
        let acct = account("a1", BackendKind::Imap);
        let backend = ImapBackend {
            account_id: acct.id.clone(),
            config: config(server.addr.port()),
        };
        let store = Store::open_in_memory().unwrap();
        assert_eq!(sync_once(&acct, &backend, &store).unwrap().upserted, 500);
        let threads = store.list_threads(&acct.id).unwrap();
        assert_eq!(threads.len(), 500);
        assert!(threads.iter().all(|thread| thread.unread));
        let body_bytes: usize = {
            let mut state = server.state.lock().expect("state");
            let inbox = mailbox(&mut state, "INBOX");
            for message in &mut inbox.messages {
                message.flags = vec!["\\Seen".into()];
                message.modseq = 2;
            }
            let bytes = inbox.messages.iter().map(|message| message.raw.len()).sum();
            state.commands.clear();
            bytes
        };
        server.sent.store(0, std::sync::atomic::Ordering::Relaxed);
        let report = sync_once(&acct, &backend, &store).unwrap();
        let sent = server.sent.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(report.upserted, 0, "qresync {qresync}");
        let logged = commands(&server);
        assert!(
            logged
                .iter()
                .any(|line| line.contains("UID FETCH 1:* (UID FLAGS) (CHANGEDSINCE 1")),
            "{logged:?}"
        );
        assert!(
            !logged.iter().any(|line| line.contains("BODY.PEEK")),
            "a flag change must not download a body: {logged:?}"
        );
        let threads = store.list_threads(&acct.id).unwrap();
        assert_eq!(threads.len(), 500);
        assert!(
            threads.iter().all(|thread| !thread.unread),
            "every message is read now (qresync {qresync})"
        );
        assert!(
            sent * 20 <= body_bytes,
            "the pass sent {sent} bytes for {body_bytes} bytes of bodies (qresync {qresync})"
        );
    }
}

#[test]
fn condstore_fetches_the_body_of_a_message_no_longer_deleted() {
    let mut state = sample_state();
    state.condstore = true;
    state.mailboxes.retain(|mailbox| mailbox.name == "INBOX");
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "gone@x").is_none());
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        for message in &mut inbox.messages {
            if message.uid == 3 || message.uid == 2 {
                message.flags = vec!["\\Seen".into()];
                message.modseq = 5;
            }
        }
        state.commands.clear();
    }
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 1);
    assert!(stored(&store, &acct, "gone@x").is_some());
    assert!(!stored(&store, &acct, "reply@x").unwrap().unread);
    let bodies = body_fetches(&server);
    assert!(
        bodies.len() == 1 && bodies[0].ends_with("UID FETCH 3 (UID FLAGS BODY.PEEK[])"),
        "only the copy the store does not hold is downloaded: {bodies:?}"
    );
}

#[test]
fn without_uidnext_only_new_uids_are_downloaded() {
    let mut state = sample_state();
    state.mailboxes = vec![FixtureMailbox {
        omit_uidnext: true,
        messages: (1..=2)
            .map(|uid| FixtureMessage {
                uid,
                flags: vec!["\\Seen".into()],
                modseq: 1,
                body_form: BodyForm::Literal,
                raw: raw_message(&format!("d{uid}@x"), None, "Old", "Ann <ann@x>", "old", ""),
            })
            .collect(),
        ..empty_mailbox("INBOX", 6)
    }];
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    {
        let mut state = server.state.lock().expect("state");
        mailbox(&mut state, "INBOX").messages.push(FixtureMessage {
            uid: 3,
            flags: Vec::new(),
            modseq: 2,
            body_form: BodyForm::Literal,
            raw: raw_message("d3@x", None, "New", "Ann <ann@x>", "new", ""),
        });
        state.commands.clear();
    }
    assert_eq!(sync_once(&acct, &backend, &store).unwrap().upserted, 1);
    assert!(stored(&store, &acct, "d3@x").is_some());
    let bodies = body_fetches(&server);
    assert!(
        bodies.len() == 1 && bodies[0].ends_with("UID FETCH 3 (UID FLAGS BODY.PEEK[])"),
        "{bodies:?}"
    );
    server.state.lock().expect("state").commands.clear();
    // `4:*` still names UID 3, the highest; it is stored, so nothing is fetched.
    assert_eq!(sync_once(&acct, &backend, &store).unwrap().upserted, 0);
    assert!(body_fetches(&server).is_empty(), "{:?}", commands(&server));
}

fn literal(uid: u32, flags: &[&str], modseq: u64, raw: Vec<u8>) -> FixtureMessage {
    FixtureMessage {
        uid,
        flags: flags.iter().map(|flag| (*flag).to_string()).collect(),
        modseq,
        body_form: BodyForm::Literal,
        raw,
    }
}

#[test]
fn an_unread_change_on_the_other_copy_of_one_message_id_reaches_the_row() {
    // INBOX and Sent (or All Mail) hold the same Message-ID. The row names
    // the copy written last; the flag changes on the other one.
    let raw = raw_message("both@x", None, "Both", "Ann <ann@x>", "kept twice", "");
    let mut state = sample_state();
    state.condstore = true;
    state.mailboxes = vec![
        FixtureMailbox {
            messages: vec![literal(1, &["\\Seen"], 1, raw.clone())],
            ..empty_mailbox("INBOX", 3)
        },
        FixtureMailbox {
            messages: vec![literal(1, &["\\Seen"], 1, raw)],
            ..empty_mailbox("Sent", 4)
        },
    ];
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let row = stored(&store, &acct, "both@x").unwrap();
    assert_eq!(row.remote_id.0, "imap:4:1:Sent");
    assert!(!row.unread);
    {
        let mut state = server.state.lock().expect("state");
        let message = &mut mailbox(&mut state, "INBOX").messages[0];
        message.flags.clear();
        message.modseq = 2;
        state.commands.clear();
    }
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 0);
    assert!(body_fetches(&server).is_empty(), "{:?}", commands(&server));
    assert!(
        stored(&store, &acct, "both@x").unwrap().unread,
        "the INBOX copy is unread now"
    );
}

#[test]
fn a_flag_change_after_trash_is_emptied_comes_back_with_undo() {
    let mut state = sample_state();
    state.condstore = true;
    state.mailboxes = vec![
        empty_mailbox("INBOX", 3),
        FixtureMailbox {
            messages: vec![literal(
                1,
                &["\\Seen"],
                1,
                raw_message("binned@x", None, "Binned", "Ann <ann@x>", "old", ""),
            )],
            ..empty_mailbox("Trash", 5)
        },
    ];
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let thread = thread_of(&store, &acct, "binned@x");
    // Emptying Trash in Fell deletes the row; the server copy stays.
    store.delete_thread(&acct.id, &thread, "Trash").unwrap();
    assert!(stored(&store, &acct, "binned@x").is_none());
    {
        let mut state = server.state.lock().expect("state");
        let message = &mut mailbox(&mut state, "Trash").messages[0];
        message.flags.clear();
        message.modseq = 2;
        state.commands.clear();
    }
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 0);
    assert!(body_fetches(&server).is_empty(), "{:?}", commands(&server));
    assert!(stored(&store, &acct, "binned@x").is_none());
    store.undo(&acct.id).unwrap();
    assert!(
        stored(&store, &acct, "binned@x").unwrap().unread,
        "undo restores the read state the server reported since"
    );
}

#[test]
fn a_body_fetch_that_leaves_out_a_uid_keeps_the_checkpoint_below_it() {
    let mut state = sample_state();
    state.condstore = true;
    state.mailboxes = vec![FixtureMailbox {
        messages: vec![literal(
            1,
            &["\\Seen"],
            1,
            raw_message("first@x", None, "First", "Ann <ann@x>", "one", ""),
        )],
        ..empty_mailbox("INBOX", 8)
    }];
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        inbox.messages.push(FixtureMessage {
            body_form: BodyForm::Omitted,
            ..literal(
                2,
                &[],
                3,
                raw_message("second@x", None, "Second", "Ann <ann@x>", "two", ""),
            )
        });
        inbox.messages.push(literal(
            3,
            &[],
            3,
            raw_message("third@x", None, "Third", "Ann <ann@x>", "three", ""),
        ));
        state.commands.clear();
    }
    sync_once(&acct, &backend, &store).unwrap();
    assert!(
        body_fetches(&server)
            .iter()
            .any(|line| line.ends_with("UID FETCH 2:3 (UID FLAGS BODY.PEEK[])")),
        "{:?}",
        commands(&server)
    );
    assert!(stored(&store, &acct, "third@x").is_some());
    assert!(stored(&store, &acct, "second@x").is_none());
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        cursor.contains("INBOX\t8\t1\t"),
        "the fetch finished without UID 2, so the checkpoint stays below it: {cursor}"
    );
    mailbox(&mut server.state.lock().expect("state"), "INBOX").messages[1].body_form =
        BodyForm::Literal;
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "second@x").is_some());
}

#[test]
fn a_deleted_uid_does_not_move_the_checkpoint_before_lower_bodies_arrive() {
    let _batch = super::limit_fetch_batch(1);
    let mut state = sample_state();
    state.mailboxes = vec![FixtureMailbox {
        omit_uidnext: true,
        messages: (1..=2)
            .map(|uid| {
                literal(
                    uid,
                    &["\\Seen"],
                    1,
                    raw_message(
                        &format!("old{uid}@x"),
                        None,
                        "Old",
                        "Ann <ann@x>",
                        "old",
                        "",
                    ),
                )
            })
            .collect(),
        ..empty_mailbox("INBOX", 6)
    }];
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        for (uid, id) in [(3, "new3@x"), (4, "new4@x")] {
            inbox.messages.push(literal(
                uid,
                &[],
                2,
                raw_message(id, None, "New", "Ann <ann@x>", "new", ""),
            ));
        }
        inbox.messages.push(literal(
            5,
            &["\\Deleted"],
            2,
            raw_message("deleted@x", None, "Deleted", "Ann <ann@x>", "gone", ""),
        ));
        // The connection drops after UID 3's body, before UID 4's.
        state.close_after_body_fetches = Some(1);
    }
    assert!(sync_once(&acct, &backend, &store).is_err());
    assert!(stored(&store, &acct, "new3@x").is_some());
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        cursor.contains("INBOX\t6\t3\t"),
        "UID 4's body never came, so the checkpoint stays below it: {cursor}"
    );
    server.state.lock().expect("state").close_after_body_fetches = None;
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "new4@x").is_some());
    assert!(stored(&store, &acct, "deleted@x").is_none());
}

#[test]
fn bad_password_does_not_advance_the_cursor() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let mut imap = config(server.addr.port());
    imap.password = "wrong".into();
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: imap,
    };
    let store = Store::open_in_memory().unwrap();
    let err = sync_once(&acct, &backend, &store).unwrap_err();
    assert!(matches!(err, crate::Error::Backend(_)));
    assert!(store.list_threads(&acct.id).unwrap().is_empty());
    assert!(store.sync_cursor(&acct.id).unwrap().is_none());
    let logged = commands(&server);
    assert!(!logged.iter().any(|line| line.contains("wrong")));
}

#[test]
fn two_accounts_keep_separate_rows() {
    let server = TestServer::spawn(sample_state());
    let store = Store::open_in_memory().unwrap();
    for id in ["a1", "a2"] {
        let acct = account(id, BackendKind::Imap);
        let backend = ImapBackend {
            account_id: acct.id.clone(),
            config: config(server.addr.port()),
        };
        sync_once(&acct, &backend, &store).unwrap();
    }
    let a = store.list_threads(&AccountId("a1".into())).unwrap();
    let b = store.list_threads(&AccountId("a2".into())).unwrap();
    assert_eq!(a.len(), b.len());
    assert!(!a.is_empty());
    let a_msgs = store
        .thread_messages(&AccountId("a1".into()), &a[0].thread_id)
        .unwrap();
    let b_msgs = store
        .thread_messages(&AccountId("a2".into()), &b[0].thread_id)
        .unwrap();
    assert_eq!(a_msgs[0].account_id.as_str(), "a1");
    assert_eq!(b_msgs[0].account_id.as_str(), "a2");
}

#[test]
fn mismatched_account_on_the_backend_rolls_back() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: AccountId("other".into()),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    let err = sync_once(&acct, &backend, &store).unwrap_err();
    assert!(matches!(err, crate::Error::Backend(_)));
    assert!(store.list_threads(&acct.id).unwrap().is_empty());
    assert!(
        store
            .list_threads(&AccountId("other".into()))
            .unwrap()
            .is_empty()
    );
    assert!(store.sync_cursor(&acct.id).unwrap().is_none());
}

#[test]
fn bridge_backend_is_the_same_imap_fetch() {
    let server = TestServer::spawn(sample_state());
    let acct = account("bridge-1", BackendKind::Bridge);
    let backend = BridgeBackend {
        account_id: acct.id.clone(),
        imap: config(server.addr.port()),
        ..BridgeBackend::default()
    };
    let store = Store::open_in_memory().unwrap();
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert!(report.upserted >= 4);
    assert_eq!(backend.kind(), BackendKind::Bridge);
    assert!(!store.list_threads(&acct.id).unwrap().is_empty());
}

#[test]
fn starttls_does_not_send_the_password_in_clear() {
    let mut state = sample_state();
    state.close_after_starttls = true;
    let server = TestServer::spawn(state);
    let mut imap = config(server.addr.port());
    imap.security = ImapSecurity::StartTls;
    imap.password = "bridge-secret".into();
    let backend = ImapBackend {
        account_id: AccountId("a1".into()),
        config: imap,
    };
    let err = backend.fetch_changes(None).unwrap_err();
    assert!(matches!(err, crate::Error::Backend(_)));
    let logged = commands(&server);
    let joined = logged.join("\n");
    assert!(joined.to_ascii_uppercase().contains("STARTTLS"));
    assert!(!joined.contains("LOGIN"));
    assert!(!joined.contains("bridge-secret"));
}

#[test]
fn same_body_different_message_ids_stay_distinct() {
    let mut state = sample_state();
    state.mailboxes.retain(|mailbox| mailbox.name == "INBOX");
    state.mailboxes[0].messages = vec![
        FixtureMessage {
            uid: 1,
            flags: vec!["\\Seen".into()],
            modseq: 1,
            body_form: BodyForm::Literal,
            raw: raw_message("one@x", None, "Same", "Ann <ann@x>", "same body", ""),
        },
        FixtureMessage {
            uid: 2,
            flags: vec!["\\Seen".into()],
            modseq: 1,
            body_form: BodyForm::Literal,
            raw: raw_message("two@x", None, "Same", "Ann <ann@x>", "same body", ""),
        },
    ];
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert_eq!(threads.len(), 2);
}

/// UID 5 with UIDNEXT 10: the first sync stores the body and moves the
/// checkpoint to UIDNEXT-1. Later syncs must not download that body again.
#[test]
fn star_range_does_not_reapply_the_highest_uid() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![FixtureMailbox {
            name: "INBOX".into(),
            list_flags: String::new(),
            uidvalidity: 3,
            uidnext: Some(10),
            omit_uidnext: false,
            reject_examine: false,
            reject_fetch: false,
            vanished: Vec::new(),
            messages: vec![FixtureMessage {
                uid: 5,
                flags: vec!["\\Seen".into()],
                modseq: 1,
                body_form: BodyForm::Literal,
                raw: raw_message("only@x", None, "Only", "Ann <ann@x>", "only body", ""),
            }],
        }],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap();
    assert!(
        cursor.token.contains("\t3\t9\t"),
        "checkpoint should sit at UIDNEXT-1: {}",
        cursor.token
    );
    for _ in 0..2 {
        {
            let mut state = server.state.lock().expect("state");
            state.commands.clear();
        }
        let report = sync_once(&acct, &backend, &store).unwrap();
        assert_eq!(report.upserted, 0);
        let logged = commands(&server);
        assert!(
            !logged.iter().any(|line| line.contains("BODY.PEEK")),
            "unchanged gap must not download the body again: {logged:?}"
        );
    }
    assert_eq!(store.list_threads(&acct.id).unwrap().len(), 1);
}

#[test]
fn empty_mailbox_records_modseq_before_mail_arrives() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: true,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![FixtureMailbox {
            name: "INBOX".into(),
            list_flags: String::new(),
            uidvalidity: 4,
            uidnext: Some(1),
            omit_uidnext: false,
            reject_examine: false,
            reject_fetch: false,
            vanished: Vec::new(),
            messages: vec![],
        }],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap();
    assert!(
        cursor.token.contains("\t4\t0\t1"),
        "empty mailbox should store HIGHESTMODSEQ: {}",
        cursor.token
    );
    {
        let mut state = server.state.lock().expect("state");
        state.mailboxes[0].messages.push(FixtureMessage {
            uid: 1,
            flags: vec![],
            modseq: 2,
            body_form: BodyForm::Literal,
            raw: raw_message("later@x", None, "Later", "Ann <ann@x>", "later body", ""),
        });
        state.mailboxes[0].uidnext = Some(2);
    }
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert_eq!(threads.len(), 1);
    assert!(threads[0].unread);
    {
        let mut state = server.state.lock().expect("state");
        state.mailboxes[0].messages[0].flags = vec!["\\Seen".into()];
        state.mailboxes[0].messages[0].modseq = 3;
    }
    sync_once(&acct, &backend, &store).unwrap();
    assert!(!store.list_threads(&acct.id).unwrap()[0].unread);
}

#[test]
fn examine_refusal_does_not_drop_the_rest_of_the_account() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![
            FixtureMailbox {
                name: "Sent".into(),
                list_flags: String::new(),
                uidvalidity: 8,
                uidnext: Some(3),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 2,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("sent@x", None, "Sent copy", "Ann <ann@x>", "sent", ""),
                }],
            },
            FixtureMailbox {
                name: "INBOX".into(),
                list_flags: String::new(),
                uidvalidity: 9,
                uidnext: Some(2),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("in@x", None, "Inbox", "Ann <ann@x>", "inbox", ""),
                }],
            },
        ],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let before = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(before.contains("Sent"));
    {
        let mut state = server.state.lock().expect("state");
        state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "Sent")
            .unwrap()
            .reject_examine = true;
        state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "INBOX")
            .unwrap()
            .messages
            .push(FixtureMessage {
                uid: 2,
                flags: vec![],
                modseq: 2,
                body_form: BodyForm::Literal,
                raw: raw_message("new@x", None, "Still here", "Ann <ann@x>", "new", ""),
            });
        state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "INBOX")
            .unwrap()
            .uidnext = Some(3);
    }
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert!(threads.iter().any(|thread| thread.subject == "Still here"));
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        cursor.contains("Sent\t8\t"),
        "refused mailbox keeps its checkpoint: {cursor}"
    );
}

/// One message bigger than the cap must not stall the account. The body is
/// discarded, the UID is skipped, and the checkpoint moves past it so the
/// following message is stored once.
#[test]
fn oversized_message_is_skipped_and_later_sync_does_not_refetch() {
    let small = raw_message(
        "small@x",
        None,
        "After the large one",
        "Ann <ann@x>",
        "small body",
        "",
    );
    let _limit = super::session::limit_literals_to(small.len());
    let mut oversized = b"oversizedmarker ".to_vec();
    oversized.resize(small.len() + 80_000, b'Q');
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![
            FixtureMailbox {
                name: "Sent".into(),
                list_flags: String::new(),
                uidvalidity: 4,
                uidnext: Some(2),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("sent@x", None, "Already there", "Ann <ann@x>", "sent", ""),
                }],
            },
            FixtureMailbox {
                name: "INBOX".into(),
                list_flags: String::new(),
                uidvalidity: 9,
                uidnext: Some(3),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![
                    FixtureMessage {
                        uid: 1,
                        flags: vec!["\\Seen".into()],
                        modseq: 1,
                        body_form: BodyForm::Literal,
                        raw: oversized,
                    },
                    FixtureMessage {
                        uid: 2,
                        flags: vec![],
                        modseq: 1,
                        body_form: BodyForm::Literal,
                        raw: small,
                    },
                ],
            },
        ],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 2);
    let threads = store.list_threads(&acct.id).unwrap();
    assert!(
        threads
            .iter()
            .any(|thread| thread.subject == "Already there")
    );
    assert!(
        threads
            .iter()
            .any(|thread| thread.subject == "After the large one")
    );
    assert!(
        store
            .search(&acct.id, "oversizedmarker")
            .unwrap()
            .hits
            .is_empty(),
        "the oversized body must not be stored"
    );
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        cursor.contains("INBOX\t9\t2\t"),
        "checkpoint must move past the skipped UID: {cursor}"
    );
    {
        let mut state = server.state.lock().expect("state");
        state.commands.clear();
    }
    let again = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(again.upserted, 0);
    let logged = commands(&server);
    assert!(
        !logged.iter().any(|line| line.contains("BODY.PEEK")),
        "the message after the oversized one must not be fetched again: {logged:?}"
    );
}

/// UID FETCH NO/BAD skips that mailbox and keeps its checkpoint, including a
/// modseq from an earlier CONDSTORE sync. Later syncs still finish, and a
/// later successful FETCH can see new mail in the skipped mailbox.
#[test]
fn fetch_refusal_does_not_block_the_rest_of_the_account() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: true,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![
            FixtureMailbox {
                name: "Sent".into(),
                list_flags: String::new(),
                uidvalidity: 8,
                uidnext: Some(2),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("sent@x", None, "Sent copy", "Ann <ann@x>", "sent", ""),
                }],
            },
            FixtureMailbox {
                name: "INBOX".into(),
                list_flags: String::new(),
                uidvalidity: 9,
                uidnext: Some(2),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("in@x", None, "Inbox", "Ann <ann@x>", "inbox", ""),
                }],
            },
        ],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let saved = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        saved.contains("Sent\t8\t1\t1"),
        "first sync stores the Sent modseq: {saved}"
    );
    {
        let mut state = server.state.lock().expect("state");
        state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "Sent")
            .unwrap()
            .reject_fetch = true;
        let inbox = state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "INBOX")
            .unwrap();
        inbox.messages.push(FixtureMessage {
            uid: 2,
            flags: vec![],
            modseq: 2,
            body_form: BodyForm::Literal,
            raw: raw_message("new@x", None, "Still here", "Ann <ann@x>", "new", ""),
        });
        inbox.uidnext = Some(3);
        state.commands.clear();
    }
    sync_once(&acct, &backend, &store).unwrap();
    let logged = commands(&server);
    assert!(
        logged.iter().any(|line| line.contains("CHANGEDSINCE")),
        "the saved modseq must be retried, not discarded: {logged:?}"
    );
    assert!(
        store
            .list_threads(&acct.id)
            .unwrap()
            .iter()
            .any(|thread| thread.subject == "Still here")
    );
    let during = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        during.contains("Sent\t8\t1\t1"),
        "refused FETCH keeps the previous checkpoint: {during}"
    );
    sync_once(&acct, &backend, &store).unwrap();
    let still = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        still.contains("Sent\t8\t1\t1"),
        "a permanent CHANGEDSINCE failure must not fail the account: {still}"
    );
    {
        let mut state = server.state.lock().expect("state");
        let sent = state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "Sent")
            .unwrap();
        sent.reject_fetch = false;
        sent.messages.push(FixtureMessage {
            uid: 2,
            flags: vec!["\\Seen".into()],
            modseq: 4,
            body_form: BodyForm::Literal,
            raw: raw_message("later@x", None, "Sent later", "Ann <ann@x>", "later", ""),
        });
        sent.uidnext = Some(3);
    }
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert!(threads.iter().any(|thread| thread.subject == "Sent later"));
    assert_eq!(
        threads
            .iter()
            .filter(|thread| thread.subject == "Sent copy")
            .count(),
        1
    );
}

#[test]
fn fetch_io_error_aborts_before_the_cursor_moves() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![
            FixtureMailbox {
                name: "Sent".into(),
                list_flags: String::new(),
                uidvalidity: 8,
                uidnext: Some(2),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("sent@x", None, "Sent copy", "Ann <ann@x>", "sent", ""),
                }],
            },
            FixtureMailbox {
                name: "INBOX".into(),
                list_flags: String::new(),
                uidvalidity: 9,
                uidnext: Some(2),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("in@x", None, "Inbox", "Ann <ann@x>", "inbox", ""),
                }],
            },
        ],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let before = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    {
        let mut state = server.state.lock().expect("state");
        state.close_during_uid_fetch = true;
        let inbox = state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "INBOX")
            .unwrap();
        inbox.messages.push(FixtureMessage {
            uid: 2,
            flags: vec![],
            modseq: 2,
            body_form: BodyForm::Literal,
            raw: raw_message("new@x", None, "Not stored", "Ann <ann@x>", "new", ""),
        });
        inbox.uidnext = Some(3);
    }
    let err = sync_once(&acct, &backend, &store).unwrap_err();
    let text = err.to_string();
    assert!(
        !text.contains("UID FETCH failed:"),
        "a dropped connection is not a mailbox refusal: {text}"
    );
    assert_eq!(store.sync_cursor(&acct.id).unwrap().unwrap().token, before);
    assert!(
        !store
            .list_threads(&acct.id)
            .unwrap()
            .iter()
            .any(|thread| thread.subject == "Not stored")
    );
}

#[test]
fn non_ascii_password_logs_in_as_a_literal() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "pâssword".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: true,
        require_utf8_for_login: true,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![FixtureMailbox {
            name: "INBOX".into(),
            list_flags: String::new(),
            uidvalidity: 3,
            uidnext: Some(2),
            omit_uidnext: false,
            reject_examine: false,
            reject_fetch: false,
            vanished: Vec::new(),
            messages: vec![FixtureMessage {
                uid: 1,
                flags: vec!["\\Seen".into()],
                modseq: 1,
                body_form: BodyForm::Literal,
                raw: raw_message(
                    "in@x",
                    None,
                    "From literal login",
                    "Ann <ann@x>",
                    "body",
                    "",
                ),
            }],
        }],
    });
    let mut imap = config(server.addr.port());
    imap.password = "pâssword".into();
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: imap,
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let logged = commands(&server);
    let enable_at = logged
        .iter()
        .position(|line| line.to_ascii_uppercase().contains("ENABLE UTF8=ACCEPT"))
        .expect("UTF8=ACCEPT should be enabled before a non-ASCII password");
    let login_at = logged
        .iter()
        .position(|line| line == "LOGIN literal")
        .expect("credentials should be sent as literals");
    assert!(enable_at < login_at, "{logged:?}");
    assert!(
        store
            .list_threads(&acct.id)
            .unwrap()
            .iter()
            .any(|thread| thread.subject == "From literal login")
    );
}

#[test]
fn fetch_batches_survive_a_dropped_connection() {
    let _batch = super::limit_fetch_batch(1);
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: Some(2),
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![FixtureMailbox {
            name: "INBOX".into(),
            list_flags: String::new(),
            uidvalidity: 4,
            uidnext: Some(4),
            omit_uidnext: false,
            reject_examine: false,
            reject_fetch: false,
            vanished: Vec::new(),
            messages: vec![
                FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("one@x", None, "One", "Ann <ann@x>", "one", ""),
                },
                FixtureMessage {
                    uid: 2,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("two@x", None, "Two", "Ann <ann@x>", "two", ""),
                },
                FixtureMessage {
                    uid: 3,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("three@x", None, "Three", "Ann <ann@x>", "three", ""),
                },
            ],
        }],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    assert!(sync_once(&acct, &backend, &store).is_err());
    let subjects: Vec<_> = store
        .list_threads(&acct.id)
        .unwrap()
        .into_iter()
        .map(|thread| thread.subject)
        .collect();
    assert!(subjects.contains(&"One".into()), "{subjects:?}");
    assert!(subjects.contains(&"Two".into()), "{subjects:?}");
    assert!(!subjects.contains(&"Three".into()), "{subjects:?}");
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        cursor.contains("INBOX\t4\t2\t"),
        "the cursor advances through the persisted prefix: {cursor}"
    );
    {
        let mut state = server.state.lock().expect("state");
        state.close_after_body_fetches = None;
        state.commands.clear();
    }
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 1);
    let logged = commands(&server);
    assert!(
        logged.iter().any(|line| line.contains("3:*")),
        "the third message is fetched without repeating the stored prefix: {logged:?}"
    );
    assert!(
        store
            .list_threads(&acct.id)
            .unwrap()
            .iter()
            .any(|thread| thread.subject == "Three")
    );
}

#[test]
fn untagged_flood_does_not_block_the_rest_of_the_account() {
    let _limit = super::session::limit_untagged_to(4);
    let messages = (1..=6)
        .map(|uid| FixtureMessage {
            uid,
            flags: vec!["\\Seen".into()],
            modseq: 1,
            body_form: BodyForm::Literal,
            raw: raw_message(
                &format!("m{uid}@x"),
                None,
                &format!("Sent {uid}"),
                "Ann <ann@x>",
                "body",
                "",
            ),
        })
        .collect();
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![
            FixtureMailbox {
                name: "Sent".into(),
                list_flags: String::new(),
                uidvalidity: 8,
                uidnext: Some(7),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages,
            },
            FixtureMailbox {
                name: "INBOX".into(),
                list_flags: String::new(),
                uidvalidity: 9,
                uidnext: Some(2),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("in@x", None, "Inbox kept", "Ann <ann@x>", "inbox", ""),
                }],
            },
        ],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert!(threads.iter().any(|thread| thread.subject == "Sent 4"));
    assert!(!threads.iter().any(|thread| thread.subject == "Sent 5"));
    assert!(threads.iter().any(|thread| thread.subject == "Inbox kept"));
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        cursor.contains("Sent\t8\t4\t"),
        "discarded replies stay below the checkpoint: {cursor}"
    );
    drop(_limit);
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert!(threads.iter().any(|thread| thread.subject == "Sent 6"));
    assert_eq!(
        threads
            .iter()
            .filter(|thread| thread.subject == "Sent 1")
            .count(),
        1
    );
}

#[test]
fn quoted_body_is_stored_and_nil_is_not_missing() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![FixtureMailbox {
            name: "INBOX".into(),
            list_flags: String::new(),
            uidvalidity: 6,
            uidnext: Some(3),
            omit_uidnext: false,
            reject_examine: false,
            reject_fetch: false,
            vanished: Vec::new(),
            messages: vec![
                FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Quoted,
                    raw: b"Subject: Quoted hello".to_vec(),
                },
                FixtureMessage {
                    uid: 2,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Nil,
                    raw: Vec::new(),
                },
            ],
        }],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(store.list_threads(&acct.id).unwrap().len(), 2);
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        cursor.contains("INBOX\t6\t2\t"),
        "quoted and NIL bodies were read, so the checkpoint can move: {cursor}"
    );
    {
        let mut state = server.state.lock().expect("state");
        state.commands.clear();
    }
    let again = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(again.upserted, 0);
    let logged = commands(&server);
    assert!(
        !logged.iter().any(|line| line.contains("BODY.PEEK")),
        "{logged:?}"
    );
}

#[test]
fn missing_body_does_not_advance_the_checkpoint_past_that_uid() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![FixtureMailbox {
            name: "INBOX".into(),
            list_flags: String::new(),
            uidvalidity: 5,
            uidnext: Some(3),
            omit_uidnext: false,
            reject_examine: false,
            reject_fetch: false,
            vanished: Vec::new(),
            messages: vec![
                FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Absent,
                    raw: Vec::new(),
                },
                FixtureMessage {
                    uid: 2,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("two@x", None, "Second", "Ann <ann@x>", "two", ""),
                },
            ],
        }],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    assert!(
        store
            .list_threads(&acct.id)
            .unwrap()
            .iter()
            .any(|thread| thread.subject == "Second")
    );
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(
        cursor.contains("INBOX\t5\t0\t"),
        "UID 1 had no body, so the checkpoint stays below it: {cursor}"
    );
    {
        let mut state = server.state.lock().expect("state");
        state.mailboxes[0].messages[0].body_form = BodyForm::Literal;
        state.mailboxes[0].messages[0].raw =
            raw_message("one@x", None, "First", "Ann <ann@x>", "one", "");
        state.mailboxes[0].messages[1].flags.clear();
        state.commands.clear();
    }
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert!(threads.iter().any(|thread| thread.subject == "First"));
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(cursor.contains("INBOX\t5\t2\t"), "{cursor}");
    // The low checkpoint must not download UID 2 again: its flags come
    // from a flags-only fetch.
    let bodies = body_fetches(&server);
    assert!(
        bodies.len() == 1 && bodies[0].ends_with("UID FETCH 1 (UID FLAGS BODY.PEEK[])"),
        "{bodies:?}"
    );
    assert!(
        commands(&server)
            .iter()
            .any(|line| line.ends_with("UID FETCH 1:* (UID FLAGS)"))
    );
    assert!(stored(&store, &acct, "two@x").unwrap().unread);
}

#[test]
fn mailbox_name_with_a_newline_does_not_abort_the_account() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        close_during_uid_fetch: false,
        utf8_accept: false,
        require_utf8_for_login: false,
        close_after_body_fetches: None,
        commands: Vec::new(),
        idle: false,
        idle_push: false,
        idle_news_first: false,
        idle_bad: false,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        special_use: false,
        reject_list_return: false,
        part_write_delay: None,
        part_ignores_window: false,
        mailboxes: vec![
            FixtureMailbox {
                name: "Bad\nName".into(),
                list_flags: String::new(),
                uidvalidity: 2,
                uidnext: Some(2),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("bad@x", None, "Should skip", "Ann <ann@x>", "bad", ""),
                }],
            },
            FixtureMailbox {
                name: "INBOX".into(),
                list_flags: String::new(),
                uidvalidity: 9,
                uidnext: Some(2),
                omit_uidnext: false,
                reject_examine: false,
                reject_fetch: false,
                vanished: Vec::new(),
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
                    body_form: BodyForm::Literal,
                    raw: raw_message("in@x", None, "Inbox ok", "Ann <ann@x>", "inbox", ""),
                }],
            },
        ],
    });
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert!(threads.iter().any(|thread| thread.subject == "Inbox ok"));
    assert!(!threads.iter().any(|thread| thread.subject == "Should skip"));
    let cursor = store.sync_cursor(&acct.id).unwrap().unwrap().token;
    assert!(cursor.contains("INBOX"), "{cursor}");
}

#[test]
fn account_file_logs_in_syncs_and_reports_bad_logins() {
    use crate::account_config::{
        AccountConfig, PasswordSource, SyncFailureKind, classify_sync_error,
    };
    use crate::sync::sync_with_progress;

    let server = TestServer::spawn(sample_state());
    let text = format!(
        "backend = \"imap\"\nemail = \"c@x\"\nusername = \"caleb\"\n\
         password_command = \"printf secret\"\nhost = \"127.0.0.1\"\n\
         port = {}\nsecurity = \"plain\"\n",
        server.addr.port()
    );
    let config = AccountConfig::parse(&text).unwrap();
    assert!(config.check_login().unwrap() >= 1);

    let store = Store::open_in_memory().unwrap();
    let backend = config.backend().unwrap();
    let mut seen = Vec::new();
    let report = sync_with_progress(
        &config.account(),
        backend.as_ref(),
        &store,
        &mut |progress| seen.push(progress.upserted),
    )
    .unwrap();
    assert!(report.upserted >= 4);
    assert_eq!(seen.last(), Some(&report.upserted));
    assert!(!store.list_threads(&config.account().id).unwrap().is_empty());

    let mut wrong = config.clone();
    wrong.password = PasswordSource::Inline("nope".into());
    let err = wrong.check_login().unwrap_err();
    assert_eq!(
        classify_sync_error(&err),
        SyncFailureKind::LoginRejected,
        "{err}"
    );

    let mut closed = config.clone();
    closed.port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    };
    let err = closed.check_login().unwrap_err();
    assert_eq!(
        classify_sync_error(&err),
        SyncFailureKind::Unreachable,
        "{err}"
    );
}

fn idle_backend(server: &TestServer) -> (Account, ImapBackend) {
    let acct = account("idle", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    (acct, backend)
}

fn idle_state() -> TestState {
    TestState {
        idle: true,
        ..sample_state()
    }
}

/// IDLE never asks the server to change anything.
fn assert_read_only(server: &TestServer) {
    for command in commands(server) {
        let upper = command.to_ascii_uppercase();
        for verb in [" SELECT", " STORE", " EXPUNGE", " APPEND", " COPY", " MOVE"] {
            assert!(!upper.contains(verb), "{command}");
        }
    }
}

#[test]
fn idle_wakes_when_new_mail_arrives() {
    let server = TestServer::spawn(idle_state());
    let (acct, backend) = idle_backend(&server);
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let cursor = store.sync_cursor(&acct.id).unwrap();
    let state = std::sync::Arc::clone(&server.state);
    let pusher = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        state.lock().expect("state").idle_push = true;
    });
    let started = std::time::Instant::now();
    let outcome = backend
        .wait_for_change(
            cursor.as_ref(),
            std::time::Duration::from_secs(20),
            &mut || false,
        )
        .unwrap();
    pusher.join().unwrap();
    assert_eq!(outcome, InboxWait::Changed);
    assert!(started.elapsed() < std::time::Duration::from_secs(10));
    let sent = commands(&server);
    assert!(
        sent.iter().any(|command| command.ends_with(" IDLE")),
        "{sent:?}"
    );
    assert!(sent.iter().any(|command| command == "DONE"), "{sent:?}");
    assert_read_only(&server);
}

#[test]
fn idle_ends_quietly_or_when_asked() {
    let server = TestServer::spawn(idle_state());
    let (acct, backend) = idle_backend(&server);
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let cursor = store.sync_cursor(&acct.id).unwrap();
    let quiet = backend
        .wait_for_change(
            cursor.as_ref(),
            std::time::Duration::from_millis(1500),
            &mut || false,
        )
        .unwrap();
    assert_eq!(quiet, InboxWait::Quiet);
    let started = std::time::Instant::now();
    let asked = backend
        .wait_for_change(
            cursor.as_ref(),
            std::time::Duration::from_secs(20),
            &mut || started.elapsed() > std::time::Duration::from_millis(300),
        )
        .unwrap();
    assert_eq!(asked, InboxWait::Stopped);
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    let done = commands(&server)
        .iter()
        .filter(|command| command.as_str() == "DONE")
        .count();
    assert_eq!(done, 2, "every IDLE ends with DONE");
    assert_read_only(&server);
}

#[test]
fn mail_since_the_last_sync_counts_without_idling() {
    let server = TestServer::spawn(idle_state());
    let (acct, backend) = idle_backend(&server);
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let cursor = store.sync_cursor(&acct.id).unwrap();
    {
        let mut state = server.state.lock().expect("state");
        let inbox = state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "INBOX")
            .unwrap();
        let next = inbox
            .messages
            .iter()
            .map(|message| message.uid)
            .max()
            .unwrap()
            + 1;
        let raw = inbox.messages[0].raw.clone();
        inbox.messages.push(FixtureMessage {
            uid: next,
            flags: Vec::new(),
            modseq: 1,
            raw,
            body_form: BodyForm::Literal,
        });
        state.commands.clear();
    }
    let outcome = backend
        .wait_for_change(
            cursor.as_ref(),
            std::time::Duration::from_secs(20),
            &mut || false,
        )
        .unwrap();
    assert_eq!(outcome, InboxWait::Changed);
    assert!(
        !commands(&server)
            .iter()
            .any(|command| command.ends_with(" IDLE")),
        "no need to idle: {:?}",
        commands(&server)
    );
}

#[test]
fn a_server_without_idle_says_so() {
    let server = TestServer::spawn(sample_state());
    let (_, backend) = idle_backend(&server);
    let outcome = backend
        .wait_for_change(None, std::time::Duration::from_secs(1), &mut || false)
        .unwrap();
    assert_eq!(outcome, InboxWait::Unsupported);
}

#[test]
fn a_uid_gap_does_not_keep_waking_sync() {
    // Expunged or skipped mail leaves UIDNEXT above the highest stored UID.
    let mut state = idle_state();
    for mailbox in &mut state.mailboxes {
        if mailbox.name == "INBOX" {
            let highest = mailbox
                .messages
                .iter()
                .map(|message| message.uid)
                .max()
                .unwrap();
            mailbox.uidnext = Some(highest + 50);
        }
    }
    let server = TestServer::spawn(state);
    let (acct, backend) = idle_backend(&server);
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let cursor = store.sync_cursor(&acct.id).unwrap();
    let outcome = backend
        .wait_for_change(
            cursor.as_ref(),
            std::time::Duration::from_millis(500),
            &mut || false,
        )
        .unwrap();
    assert_eq!(outcome, InboxWait::Quiet, "no new mail, so no new pass");
}

type Setup = Box<dyn Fn(&mut TestState)>;

/// Each setup leaves the stored cursor behind UIDNEXT with no new mail.
/// The wait must still idle instead of reporting news at once.
#[test]
fn a_synced_mailbox_waits_even_when_the_cursor_trails_uidnext() {
    let setups: Vec<(&str, Setup)> = vec![
        (
            "a body the server never sent",
            Box::new(|state: &mut TestState| {
                let inbox = state
                    .mailboxes
                    .iter_mut()
                    .find(|m| m.name == "INBOX")
                    .unwrap();
                inbox.messages[0].body_form = BodyForm::Absent;
            }),
        ),
        (
            "an empty INBOX with burned UIDs",
            Box::new(|state: &mut TestState| {
                let inbox = state
                    .mailboxes
                    .iter_mut()
                    .find(|m| m.name == "INBOX")
                    .unwrap();
                inbox.messages.clear();
                inbox.uidnext = Some(3);
            }),
        ),
        (
            "CONDSTORE with a UID gap",
            Box::new(|state: &mut TestState| {
                state.condstore = true;
                let inbox = state
                    .mailboxes
                    .iter_mut()
                    .find(|m| m.name == "INBOX")
                    .unwrap();
                let highest = inbox.messages.iter().map(|m| m.uid).max().unwrap();
                inbox.uidnext = Some(highest + 2);
            }),
        ),
    ];
    for (name, setup) in setups {
        let mut state = idle_state();
        setup(&mut state);
        let server = TestServer::spawn(state);
        let (acct, backend) = idle_backend(&server);
        let store = Store::open_in_memory().unwrap();
        sync_once(&acct, &backend, &store).unwrap();
        let cursor = store.sync_cursor(&acct.id).unwrap();
        let outcome = backend
            .wait_for_change(
                cursor.as_ref(),
                std::time::Duration::from_millis(800),
                &mut || false,
            )
            .unwrap();
        assert_eq!(outcome, InboxWait::Quiet, "{name}");
    }
}

#[test]
fn news_sent_before_the_continuation_still_wakes_sync() {
    let server = TestServer::spawn(TestState {
        idle_news_first: true,
        ..idle_state()
    });
    let (acct, backend) = idle_backend(&server);
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let cursor = store.sync_cursor(&acct.id).unwrap();
    let outcome = backend
        .wait_for_change(
            cursor.as_ref(),
            std::time::Duration::from_secs(20),
            &mut || false,
        )
        .unwrap();
    assert_eq!(outcome, InboxWait::Changed);
}

#[test]
fn a_server_that_ends_idle_with_bad_returns_promptly() {
    let server = TestServer::spawn(TestState {
        idle_bad: true,
        qresync: false,
        expunge_during_search: false,
        vanished_reply: None,
        ..idle_state()
    });
    let (acct, backend) = idle_backend(&server);
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let cursor = store.sync_cursor(&acct.id).unwrap();
    let started = std::time::Instant::now();
    let outcome = backend.wait_for_change(
        cursor.as_ref(),
        std::time::Duration::from_secs(20),
        &mut || false,
    );
    assert!(outcome.is_err(), "{outcome:?}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// The stored row for `message_id`, if any.
fn stored(store: &Store, acct: &Account, message_id: &str) -> Option<crate::Message> {
    store
        .list_threads(&acct.id)
        .unwrap()
        .iter()
        .flat_map(|thread| store.thread_messages(&acct.id, &thread.thread_id).unwrap())
        .find(|message| message.headers.message_id.as_str() == message_id)
}

fn thread_of(store: &Store, acct: &Account, message_id: &str) -> ThreadId {
    stored(store, acct, message_id)
        .unwrap_or_else(|| panic!("{message_id} is stored"))
        .thread_id
}

fn mailbox<'a>(state: &'a mut TestState, name: &str) -> &'a mut FixtureMailbox {
    state
        .mailboxes
        .iter_mut()
        .find(|mailbox| mailbox.name == name)
        .unwrap()
}

/// Expunge `uid` from `name` on the server. UIDNEXT stays where it was.
fn expunge(server: &TestServer, name: &str, uid: u32) -> FixtureMessage {
    let mut state = server.state.lock().expect("state");
    let mailbox = mailbox(&mut state, name);
    let next = mailbox
        .uidnext
        .unwrap_or_else(|| mailbox.messages.iter().map(|m| m.uid).max().unwrap_or(0) + 1);
    mailbox.uidnext = Some(next);
    let index = mailbox
        .messages
        .iter()
        .position(|message| message.uid == uid)
        .unwrap();
    mailbox.messages.remove(index)
}

fn empty_mailbox(name: &str, uidvalidity: u32) -> FixtureMailbox {
    FixtureMailbox {
        name: name.into(),
        list_flags: String::new(),
        uidvalidity,
        uidnext: None,
        omit_uidnext: false,
        reject_examine: false,
        reject_fetch: false,
        messages: Vec::new(),
        vanished: Vec::new(),
    }
}

fn folder_subjects(store: &Store, acct: &Account, folder: &str) -> Vec<String> {
    store
        .folder_threads(&acct.id, folder)
        .unwrap()
        .into_iter()
        .map(|thread| thread.subject)
        .collect()
}

#[test]
fn mail_deleted_on_the_server_is_gone_locally() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(store.search(&acct.id, "reply").unwrap().hits.len(), 1);

    // The reply lives only in INBOX. The root also has a Sent copy.
    expunge(&server, "INBOX", 2);
    expunge(&server, "INBOX", 1);
    sync_once(&acct, &backend, &store).unwrap();

    assert!(stored(&store, &acct, "reply@x").is_none());
    assert!(store.search(&acct.id, "reply").unwrap().hits.is_empty());
    let root = stored(&store, &acct, "root@x").unwrap();
    assert_eq!(root.folders, vec!["Sent"]);
    let proton = store
        .list_threads(&acct.id)
        .unwrap()
        .into_iter()
        .find(|thread| thread.thread_id == ThreadId::proton("conv-9"))
        .unwrap();
    assert_eq!(proton.message_count, 1);
    assert!(!folder_subjects(&store, &acct, "INBOX").contains(&"Hello".to_string()));
    assert!(folder_subjects(&store, &acct, "Sent").contains(&"Hello".to_string()));
    assert_read_only(&server);

    // Nothing else changed, so the next pass sends no listing at all.
    server.state.lock().expect("state").commands.clear();
    sync_once(&acct, &backend, &store).unwrap();
    assert!(
        !commands(&server)
            .iter()
            .any(|line| line.contains("SEARCH") || line.contains("HEADER.FIELDS"))
    );
    assert!(stored(&store, &acct, "html@x").is_some());
}

#[test]
fn mail_moved_on_the_server_lists_only_the_new_folder() {
    let mut state = sample_state();
    state.mailboxes.push(empty_mailbox("Archive", 30));
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let thread = thread_of(&store, &acct, "html@x");
    store
        .label_thread(&acct.id, &thread, LabelChange::Add("mine".into()))
        .unwrap();

    // Archived in Proton web: a new UID in Archive, gone from INBOX. INBOX
    // syncs first, so the copy leaves before the new one arrives.
    let mut moved = expunge(&server, "INBOX", 4);
    moved.uid = 1;
    mailbox(&mut server.state.lock().expect("state"), "Archive")
        .messages
        .push(moved);
    sync_once(&acct, &backend, &store).unwrap();

    let html = stored(&store, &acct, "html@x").unwrap();
    assert_eq!(html.folders, vec!["Archive"]);
    // The same row: local state is still there.
    assert_eq!(html.thread_id, thread);
    assert!(html.labels.iter().any(|label| label == "mine"));
    assert!(!folder_subjects(&store, &acct, "INBOX").contains(&"Html".to_string()));
    assert_eq!(folder_subjects(&store, &acct, "Archive"), vec!["Html"]);

    // And back again, so the new folder syncs first this time.
    let mut back = expunge(&server, "Archive", 1);
    back.uid = 6;
    mailbox(&mut server.state.lock().expect("state"), "INBOX")
        .messages
        .push(back);
    {
        let mut state = server.state.lock().expect("state");
        mailbox(&mut state, "INBOX").uidnext = Some(7);
    }
    sync_once(&acct, &backend, &store).unwrap();
    let html = stored(&store, &acct, "html@x").unwrap();
    assert_eq!(html.folders, vec!["INBOX"]);
    assert!(html.labels.iter().any(|label| label == "mine"));
    assert!(folder_subjects(&store, &acct, "Archive").is_empty());
    assert_read_only(&server);
}

#[test]
fn a_merged_copy_keeps_the_row_while_another_copy_lives() {
    let body = "Here is the whole plan for the launch next week, with dates.";
    let mut state = sample_state();
    state.mailboxes = vec![empty_mailbox("INBOX", 20), empty_mailbox("Sent", 10)];
    state.mailboxes[0].messages.push(FixtureMessage {
        uid: 1,
        flags: vec![],
        modseq: 1,
        body_form: BodyForm::Literal,
        raw: raw_message(
            "post-list@x",
            None,
            "[team] Plan",
            "Ann <ann@x>",
            body,
            "List-Id: Team <team.x.com>\r\n",
        ),
    });
    state.mailboxes[1].messages.push(FixtureMessage {
        uid: 1,
        flags: vec!["\\Seen".into()],
        modseq: 1,
        body_form: BodyForm::Literal,
        raw: raw_message("post@x", None, "Plan", "Ann <ann@x>", body, ""),
    });
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert_eq!(threads.len(), 1, "the list echo merged into one row");
    let row = store
        .thread_messages(&acct.id, &threads[0].thread_id)
        .unwrap()
        .remove(0);
    assert_eq!(row.folders, vec!["INBOX", "Sent"]);

    // The row's own copy goes. The merged Sent copy keeps it alive.
    expunge(&server, "INBOX", 1);
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    assert_eq!(threads.len(), 1);
    let row = store
        .thread_messages(&acct.id, &threads[0].thread_id)
        .unwrap()
        .remove(0);
    assert_eq!(row.folders, vec!["Sent"]);
    assert!(store.id_is_stored(&acct.id, "post@x").unwrap());

    // Then the last copy goes, and so do the row and its alias.
    expunge(&server, "Sent", 1);
    sync_once(&acct, &backend, &store).unwrap();
    assert!(store.list_threads(&acct.id).unwrap().is_empty());
    assert!(!store.id_is_stored(&acct.id, "post@x").unwrap());
    assert!(store.search(&acct.id, "launch").unwrap().hits.is_empty());
}

#[test]
fn an_expunge_during_the_listing_waits_for_the_next_pass() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    expunge(&server, "INBOX", 2);
    // Sequence numbers shift under an expunge, so this listing is not
    // trusted and nothing is removed.
    server.state.lock().expect("state").expunge_during_search = true;
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "reply@x").is_some());
    assert!(stored(&store, &acct, "html@x").is_some());
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "reply@x").is_none());
    assert!(stored(&store, &acct, "html@x").is_some());
}

#[test]
fn a_mailbox_that_cannot_be_read_keeps_rows_whose_copy_may_have_moved_there() {
    let mut state = sample_state();
    state.mailboxes.push(empty_mailbox("Archive", 30));
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let mut moved = expunge(&server, "INBOX", 4);
    moved.uid = 1;
    {
        let mut state = server.state.lock().expect("state");
        let archive = mailbox(&mut state, "Archive");
        archive.messages.push(moved);
        archive.reject_examine = true;
    }
    sync_once(&acct, &backend, &store).unwrap();
    // INBOX lost the copy, but Archive was not read, so the row waits.
    assert!(stored(&store, &acct, "html@x").is_some());
    mailbox(&mut server.state.lock().expect("state"), "Archive").reject_examine = false;
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(
        stored(&store, &acct, "html@x").unwrap().folders,
        vec!["Archive"]
    );
}

#[test]
fn qresync_reports_vanished_mail_without_a_listing() {
    let mut state = sample_state();
    state.condstore = true;
    state.qresync = true;
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        let index = inbox.messages.iter().position(|m| m.uid == 2).unwrap();
        inbox.messages.remove(index);
        inbox.uidnext = Some(6);
        inbox.vanished.push((2, 5));
        state.commands.clear();
    }
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "reply@x").is_none());
    assert!(stored(&store, &acct, "html@x").is_some());
    let logged = commands(&server);
    assert!(
        logged
            .iter()
            .any(|line| line.contains("CHANGEDSINCE") && line.contains("VANISHED"))
    );
    assert!(!logged.iter().any(|line| line.contains("SEARCH")));
    assert_read_only(&server);
}

#[test]
fn mail_that_moved_before_copies_were_recorded_still_leaves_the_old_folder() {
    let mut state = sample_state();
    state.mailboxes.push(empty_mailbox("Archive", 30));
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    // A mailbox file from before copies were recorded: the rows name one
    // copy each and the cursor has never listed a mailbox.
    let mut pairs = crate::imap::decode_cursor(store.sync_cursor(&acct.id).unwrap().as_ref());
    for (_, checkpoint) in &mut pairs {
        assert!(checkpoint.pruned.is_some());
        checkpoint.pruned = None;
    }
    store
        .apply_delta(
            &acct.id,
            &crate::MailboxDelta {
                cursor: Some(crate::imap::encode_cursor("a1", &pairs)),
                ..crate::MailboxDelta::default()
            },
        )
        .unwrap();
    store
        .connection()
        .execute_batch("DELETE FROM message_copies")
        .unwrap();
    let mut moved = expunge(&server, "INBOX", 4);
    moved.uid = 1;
    mailbox(&mut server.state.lock().expect("state"), "Archive")
        .messages
        .push(moved);
    server.state.lock().expect("state").commands.clear();
    sync_once(&acct, &backend, &store).unwrap();

    assert_eq!(
        stored(&store, &acct, "html@x").unwrap().folders,
        vec!["Archive"]
    );
    // Mail still in INBOX was learned from its headers and stays.
    for id in ["root@x", "reply@x", "uid.20.5.INBOX@imap.fell"] {
        assert!(
            stored(&store, &acct, id)
                .unwrap()
                .folders
                .contains(&"INBOX".to_string()),
            "{id}"
        );
    }
    let logged = commands(&server);
    assert!(
        logged
            .iter()
            .any(|line| line.contains("FETCH 1:4 (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])"))
    );
    assert_read_only(&server);
}

#[test]
fn a_uidvalidity_change_keeps_mail_the_refetch_skips() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    // The reply lives only in INBOX. Flagged \Deleted (not expunged), the
    // full refetch after the reset skips its body, but it is still there,
    // under a UID that the old UIDVALIDITY gave nothing (UID 2 is free now).
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        inbox.uidvalidity = 99;
        let index = inbox.messages.iter().position(|m| m.uid == 2).unwrap();
        let mut reply = inbox.messages.remove(index);
        reply.uid = 7;
        reply.flags.push("\\Deleted".into());
        inbox.messages.push(reply);
    }
    for _ in 0..3 {
        sync_once(&acct, &backend, &store).unwrap();
        let reply = stored(&store, &acct, "reply@x").expect("reply@x is kept");
        assert_eq!(reply.folders, vec!["INBOX"]);
    }
    let proton = store
        .list_threads(&acct.id)
        .unwrap()
        .into_iter()
        .find(|thread| thread.thread_id == ThreadId::proton("conv-9"))
        .unwrap();
    assert_eq!(proton.message_count, 2);

    // Expunged later, it goes like any other message.
    expunge(&server, "INBOX", 7);
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "reply@x").is_none());
    assert!(stored(&store, &acct, "html@x").is_some());
    assert_read_only(&server);
}

#[test]
fn a_uidvalidity_change_keeps_a_skipped_message_with_no_message_id() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    // No Message-ID, so the headers cannot name its row. Its UID is still
    // listed and no new copy was recorded, so the old copy stays.
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        inbox.uidvalidity = 99;
        let no_id = inbox.messages.iter_mut().find(|m| m.uid == 5).unwrap();
        no_id.flags.push("\\Deleted".into());
    }
    for _ in 0..3 {
        sync_once(&acct, &backend, &store).unwrap();
        let kept = stored(&store, &acct, "uid.20.5.INBOX@imap.fell").expect("row is kept");
        assert_eq!(kept.folders, vec!["INBOX"]);
    }
    expunge(&server, "INBOX", 5);
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "uid.20.5.INBOX@imap.fell").is_none());
}

#[test]
fn a_vanished_set_ending_in_star_does_not_drop_higher_uids() {
    let mut state = sample_state();
    state.condstore = true;
    state.qresync = true;
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        let index = inbox.messages.iter().position(|m| m.uid == 2).unwrap();
        inbox.messages.remove(index);
        inbox.uidnext = Some(6);
        // `*` is the highest UID in the mailbox, which is still present.
        state.vanished_reply = Some("2:*".into());
        state.commands.clear();
    }
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "html@x").is_some());
    assert!(stored(&store, &acct, "uid.20.5.INBOX@imap.fell").is_some());
    // The listing that replaced the VANISHED still finds the expunge.
    assert!(stored(&store, &acct, "reply@x").is_none());
    assert!(commands(&server).iter().any(|line| line.contains("SEARCH")));
    // And the next pass does not trust the same reply either.
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "html@x").is_some());
    assert!(stored(&store, &acct, "smile@x").is_some());
    assert_read_only(&server);
}

#[test]
fn an_expunge_qresync_did_not_report_is_still_found() {
    let mut state = sample_state();
    state.condstore = true;
    state.qresync = true;
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    // EXISTS drops by one, but no VANISHED names the UID.
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        let index = inbox.messages.iter().position(|m| m.uid == 2).unwrap();
        inbox.messages.remove(index);
        inbox.uidnext = Some(6);
        state.commands.clear();
    }
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "reply@x").is_none());
    assert!(stored(&store, &acct, "html@x").is_some());
    assert!(commands(&server).iter().any(|line| line.contains("SEARCH")));

    // With the drop accounted for, later passes go back to VANISHED alone.
    server.state.lock().expect("state").commands.clear();
    sync_once(&acct, &backend, &store).unwrap();
    assert!(!commands(&server).iter().any(|line| line.contains("SEARCH")));
    assert_read_only(&server);
}

/// A provider's folder layout: `(name, LIST attributes)`. One message sits in
/// INBOX, and a copy of it in `also_in` (Gmail keeps every message in All Mail).
fn provider_state(special_use: bool, folders: &[(&str, &str)], also_in: Option<&str>) -> TestState {
    let message = || FixtureMessage {
        uid: 1,
        flags: vec!["\\Seen".into()],
        modseq: 1,
        body_form: BodyForm::Literal,
        raw: raw_message(
            "role@x",
            None,
            "Roles",
            "Ann <ann@x>",
            "where does it go",
            "",
        ),
    };
    let mut state = sample_state();
    state.special_use = special_use;
    state.mailboxes = folders
        .iter()
        .enumerate()
        .map(|(index, (name, flags))| {
            let mut mailbox = empty_mailbox(name, 100 + index as u32);
            mailbox.list_flags = (*flags).to_string();
            if *name == "INBOX" || Some(*name) == also_in {
                mailbox.messages.push(message());
            }
            mailbox
        })
        .collect();
    state
}

/// Sync `state`, then check what each role resolved to and where archive
/// and delete put the INBOX message.
fn assert_provider_roles(
    state: TestState,
    overrides: &[(FolderRole, &str)],
    expected: &[(FolderRole, &str)],
    archive_lands_in: &str,
) -> TestServer {
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let mut imap = config(server.addr.port());
    imap.folder_overrides = overrides
        .iter()
        .map(|(role, name)| (*role, (*name).to_string()))
        .collect();
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: imap,
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let roles = store.folder_roles(&acct.id).unwrap();
    for (role, name) in expected {
        assert_eq!(roles.name(*role), *name, "{role:?}");
        assert_eq!(roles.label(name), role.label(), "{name}");
    }
    assert_eq!(roles.archive_name(), archive_lands_in);

    let thread = thread_of(&store, &acct, "role@x");
    store.archive_thread(&acct.id, &thread, "INBOX").unwrap();
    let archived = stored(&store, &acct, "role@x").unwrap();
    assert!(
        archived.folders.iter().any(|f| f == archive_lands_in),
        "{:?}",
        archived.folders
    );
    assert!(!archived.folders.iter().any(|f| f == "INBOX"));
    assert_eq!(
        folder_subjects(&store, &acct, archive_lands_in),
        vec!["Roles"]
    );
    store.undo(&acct.id).unwrap();

    let trash = roles.name(FolderRole::Trash).to_string();
    store.delete_thread(&acct.id, &thread, "INBOX").unwrap();
    let deleted = stored(&store, &acct, "role@x").unwrap();
    assert!(deleted.folders.contains(&trash), "{:?}", deleted.folders);
    assert!(!deleted.folders.iter().any(|f| f == "INBOX"));
    // Delete from Trash drops the folder rather than moving it again.
    store.delete_thread(&acct.id, &thread, &trash).unwrap();
    assert!(folder_subjects(&store, &acct, &trash).is_empty());
    server
}

#[test]
fn an_xlist_inbox_mark_alone_keeps_the_name_table() {
    // The server marks only INBOX (`\\Inbox` is XLIST, not RFC 6154), so
    // Sent and Trash still come from their names.
    let state = provider_state(
        false,
        &[
            ("INBOX", "\\Inbox \\HasNoChildren"),
            ("Sent Items", "\\HasNoChildren"),
            ("Deleted Messages", "\\HasNoChildren"),
            ("Archive", "\\HasNoChildren"),
        ],
        None,
    );
    assert_provider_roles(
        state,
        &[],
        &[
            (FolderRole::Inbox, "INBOX"),
            (FolderRole::Sent, "Sent Items"),
            (FolderRole::Trash, "Deleted Messages"),
            (FolderRole::Archive, "Archive"),
        ],
        "Archive",
    );
}

#[test]
fn proton_bridge_folders_marked_on_a_plain_list() {
    let state = provider_state(
        false,
        &[
            ("INBOX", ""),
            ("Drafts", "\\Drafts"),
            ("Sent", "\\Sent"),
            ("Starred", "\\Flagged"),
            ("Archive", "\\Archive"),
            ("Spam", "\\Junk"),
            ("Trash", "\\Trash"),
            ("All Mail", "\\All"),
            ("Folders", "\\Noselect \\HasChildren"),
            ("Folders/Work", ""),
            ("Labels", "\\Noselect \\HasChildren"),
            ("Labels/Travel", ""),
        ],
        Some("All Mail"),
    );
    let server = assert_provider_roles(
        state,
        &[],
        &[
            (FolderRole::Inbox, "INBOX"),
            (FolderRole::Sent, "Sent"),
            (FolderRole::Drafts, "Drafts"),
            (FolderRole::Trash, "Trash"),
            (FolderRole::Junk, "Spam"),
            (FolderRole::Archive, "Archive"),
            (FolderRole::All, "All Mail"),
            (FolderRole::Flagged, "Starred"),
        ],
        "Archive",
    );
    let lists: Vec<String> = commands(&server)
        .into_iter()
        .filter(|command| command.to_ascii_uppercase().contains("LIST"))
        .collect();
    assert!(
        lists.iter().all(|command| !command.contains("RETURN")),
        "{lists:?}"
    );
}

#[test]
fn gmail_roles_come_from_special_use_and_archive_goes_to_all_mail() {
    let state = provider_state(
        true,
        &[
            ("INBOX", ""),
            ("[Gmail]", "\\Noselect \\HasChildren"),
            ("[Gmail]/All Mail", "\\All \\HasNoChildren"),
            ("[Gmail]/Drafts", "\\Drafts \\HasNoChildren"),
            ("[Gmail]/Important", "\\Important \\HasNoChildren"),
            ("[Gmail]/Sent Mail", "\\Sent \\HasNoChildren"),
            ("[Gmail]/Spam", "\\Junk \\HasNoChildren"),
            ("[Gmail]/Starred", "\\Flagged \\HasNoChildren"),
            ("[Gmail]/Trash", "\\Trash \\HasNoChildren"),
            // A user label that happens to share a role's usual name.
            ("Sent", ""),
        ],
        Some("[Gmail]/All Mail"),
    );
    let server = assert_provider_roles(
        state,
        &[],
        &[
            (FolderRole::Sent, "[Gmail]/Sent Mail"),
            (FolderRole::Drafts, "[Gmail]/Drafts"),
            (FolderRole::Trash, "[Gmail]/Trash"),
            (FolderRole::Junk, "[Gmail]/Spam"),
            (FolderRole::All, "[Gmail]/All Mail"),
            (FolderRole::Flagged, "[Gmail]/Starred"),
        ],
        "[Gmail]/All Mail",
    );
    assert!(
        commands(&server)
            .iter()
            .any(|command| command.ends_with("LIST \"\" \"*\" RETURN (SPECIAL-USE)")),
        "{:?}",
        commands(&server)
    );
}

#[test]
fn fastmail_sent_items_is_sent() {
    let state = provider_state(
        true,
        &[
            ("INBOX", ""),
            ("Archive", "\\Archive"),
            ("Drafts", "\\Drafts"),
            ("Sent Items", "\\Sent"),
            ("Spam", "\\Junk"),
            ("Trash", "\\Trash"),
        ],
        None,
    );
    assert_provider_roles(
        state,
        &[],
        &[
            (FolderRole::Sent, "Sent Items"),
            (FolderRole::Drafts, "Drafts"),
            (FolderRole::Trash, "Trash"),
            (FolderRole::Junk, "Spam"),
            (FolderRole::Archive, "Archive"),
        ],
        "Archive",
    );
}

#[test]
fn a_server_without_special_use_falls_back_to_folder_names() {
    let state = provider_state(
        false,
        &[
            ("INBOX", ""),
            ("Drafts", ""),
            ("Sent Messages", ""),
            ("INBOX/Trash", ""),
            ("Junk", ""),
            ("Projects", ""),
        ],
        None,
    );
    let server = assert_provider_roles(
        state,
        &[],
        &[
            (FolderRole::Sent, "Sent Messages"),
            (FolderRole::Drafts, "Drafts"),
            (FolderRole::Trash, "INBOX/Trash"),
            (FolderRole::Junk, "Junk"),
        ],
        // Nothing is an archive here, so archive keeps Fell's own folder.
        "Archive",
    );
    let lists: Vec<String> = commands(&server)
        .into_iter()
        .filter(|command| command.to_ascii_uppercase().contains("LIST"))
        .collect();
    assert!(lists.iter().all(|command| !command.contains("RETURN")));
}

#[test]
fn account_overrides_name_folders_a_server_does_not_mark() {
    let state = provider_state(
        false,
        &[
            ("INBOX", ""),
            ("Gesendet", ""),
            ("Papierkorb", ""),
            ("Ablage", ""),
        ],
        None,
    );
    assert_provider_roles(
        state,
        &[
            (FolderRole::Sent, "gesendet"),
            (FolderRole::Trash, "Papierkorb"),
            (FolderRole::Archive, "Ablage"),
        ],
        &[
            (FolderRole::Sent, "Gesendet"),
            (FolderRole::Trash, "Papierkorb"),
            (FolderRole::Archive, "Ablage"),
        ],
        "Ablage",
    );
}

#[test]
fn a_server_that_refuses_list_return_still_gets_its_roles_read() {
    let mut state = provider_state(
        false,
        &[("INBOX", ""), ("Sent Items", "\\Sent"), ("Bin", "\\Trash")],
        None,
    );
    state.reject_list_return = true;
    let server = assert_provider_roles(
        state,
        &[],
        &[(FolderRole::Sent, "Sent Items"), (FolderRole::Trash, "Bin")],
        "Archive",
    );
    let lists: Vec<String> = commands(&server)
        .into_iter()
        .filter(|command| command.to_ascii_uppercase().contains(" LIST "))
        .collect();
    assert!(lists.iter().any(|command| command.contains("RETURN")));
    assert!(lists.iter().any(|command| !command.contains("RETURN")));
}

#[test]
fn a_later_listing_replaces_the_stored_roles() {
    let server = TestServer::spawn(provider_state(
        true,
        &[("INBOX", ""), ("Sent", "\\Sent")],
        None,
    ));
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(
        store.folder_roles(&acct.id).unwrap().name(FolderRole::Sent),
        "Sent"
    );
    {
        let mut state = server.state.lock().expect("state");
        mailbox(&mut state, "Sent").list_flags = String::new();
        state.mailboxes.push(empty_mailbox("Sent Items", 77));
        mailbox(&mut state, "Sent Items").list_flags = "\\Sent".into();
    }
    sync_once(&acct, &backend, &store).unwrap();
    let roles = store.folder_roles(&acct.id).unwrap();
    assert_eq!(roles.name(FolderRole::Sent), "Sent Items");
    assert_eq!(roles.get(FolderRole::Sent).unwrap().raw, "Sent Items");
}

#[test]
fn sync_stores_parts_and_old_mail_is_fetched_again_by_uid() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let body = |id: &str| {
        store
            .message_body(&acct.id, &crate::MessageId::new(id))
            .unwrap()
    };
    let html = body("html@x").expect("html parts stored by sync");
    assert_eq!(
        html.html.as_deref().map(str::trim),
        Some("<html><body><p>Hello from html</p></body></html>")
    );
    assert_eq!(html.plain, None);
    let root = body("root@x").expect("plain parts stored by sync");
    assert_eq!(root.plain.as_deref().map(str::trim), Some("hello body"));

    // Mail synced before parts were kept: rows with no parts. The reply's
    // only copy has since gone from the server.
    store
        .conn
        .execute_batch(
            "DELETE FROM message_bodies;
             INSERT INTO body_backfill SELECT account_id, message_id FROM messages;",
        )
        .unwrap();
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        let index = inbox.messages.iter().position(|m| m.uid == 2).unwrap();
        inbox.messages.remove(index);
        state.commands.clear();
    }
    let recorded = crate::fetch_missing_bodies(&acct.id, &backend, &store, 5).unwrap();
    assert_eq!(recorded, 5);
    assert_eq!(body("html@x"), Some(html));
    assert_eq!(body("root@x"), Some(root));
    assert_eq!(body("reply@x"), Some(crate::MessageParts::default()));
    let logged = commands(&server);
    let fetches: Vec<_> = logged
        .iter()
        .filter(|line| line.contains("UID FETCH"))
        .collect();
    // One fetch per mailbox, the UIDs as one set, read with PEEK.
    assert!(
        fetches
            .iter()
            .any(|line| line.contains("UID FETCH 1,2,4,5 (UID FLAGS BODY.PEEK[])")),
        "{fetches:?}"
    );
    assert!(fetches.iter().all(|line| line.contains("BODY.PEEK[]")));
    assert_read_only(&server);

    // Nothing is left, so the next pass does not connect.
    server.state.lock().expect("state").commands.clear();
    assert_eq!(
        crate::fetch_missing_bodies(&acct.id, &backend, &store, 5).unwrap(),
        0
    );
    assert!(commands(&server).is_empty());

    // Opening the reply asks again; its copy is back under the same UID.
    {
        let mut state = server.state.lock().expect("state");
        let inbox = mailbox(&mut state, "INBOX");
        inbox.messages.insert(
            1,
            FixtureMessage {
                uid: 2,
                flags: vec![],
                modseq: 1,
                body_form: BodyForm::Literal,
                raw: raw_message(
                    "reply@x",
                    Some("root@x"),
                    "Re: Hello",
                    "Bob <bob@x>",
                    "reply body",
                    "",
                ),
            },
        );
    }
    let opened = crate::fetch_message_body(
        &acct.id,
        &backend,
        &store,
        &crate::MessageId::new("reply@x"),
    )
    .unwrap()
    .expect("fetched on open");
    assert_eq!(opened.plain.as_deref().map(str::trim), Some("reply body"));
    // The reply is still unread: the fetch did not set \Seen.
    assert!(stored(&store, &acct, "reply@x").unwrap().unread);
    assert_read_only(&server);
}

#[test]
fn a_new_uidvalidity_records_old_copies_without_bodies() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    store
        .conn
        .execute_batch(
            "DELETE FROM message_bodies WHERE message_id = 'smile@x';
             INSERT INTO body_backfill VALUES ('a1', 'smile@x');",
        )
        .unwrap();
    mailbox(&mut server.state.lock().expect("state"), "&Jjo-").uidvalidity = 8;
    server.state.lock().expect("state").commands.clear();
    assert_eq!(
        crate::fetch_missing_bodies(&acct.id, &backend, &store, 5).unwrap(),
        1
    );
    // The stale copy is not fetched by UID under the new UIDVALIDITY.
    assert!(
        !commands(&server)
            .iter()
            .any(|line| line.contains("UID FETCH"))
    );
    assert_eq!(
        store
            .message_body(&acct.id, &crate::MessageId::new("smile@x"))
            .unwrap(),
        Some(crate::MessageParts::default())
    );
}

/// A synced store whose mail is queued again, as if stored before parts.
fn synced_without_parts(server: &TestServer, acct: &Account, backend: &ImapBackend) -> Store {
    let store = Store::open_in_memory().unwrap();
    sync_once(acct, backend, &store).unwrap();
    store
        .conn
        .execute_batch(
            "DELETE FROM message_bodies;
             INSERT INTO body_backfill SELECT account_id, message_id FROM messages;",
        )
        .unwrap();
    server.state.lock().expect("state").commands.clear();
    store
}

#[test]
fn a_refused_or_cut_short_body_fetch_stores_nothing_and_is_tried_again() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = synced_without_parts(&server, &acct, &backend);
    let queued = store.missing_bodies(&acct.id, 100).unwrap();
    let body = |id: &str| {
        store
            .message_body(&acct.id, &crate::MessageId::new(id))
            .unwrap()
    };

    mailbox(&mut server.state.lock().expect("state"), "INBOX").reject_fetch = true;
    assert!(crate::fetch_missing_bodies(&acct.id, &backend, &store, 5).is_err());
    assert_eq!(body("root@x"), None, "not recorded as gone");
    assert_eq!(store.missing_bodies(&acct.id, 100).unwrap(), queued);
    mailbox(&mut server.state.lock().expect("state"), "INBOX").reject_fetch = false;

    {
        // A reply cut off past the untagged cap.
        let _limit = super::session::limit_untagged_to(2);
        assert!(crate::fetch_missing_bodies(&acct.id, &backend, &store, 5).is_err());
    }
    assert_eq!(body("root@x"), None);
    assert_eq!(store.missing_bodies(&acct.id, 100).unwrap(), queued);

    assert_eq!(
        crate::fetch_missing_bodies(&acct.id, &backend, &store, 5).unwrap(),
        queued.len()
    );
    assert_eq!(
        body("root@x").unwrap().plain.as_deref().map(str::trim),
        Some("hello body")
    );
    assert_read_only(&server);
}

#[test]
fn a_body_backfill_batch_stops_at_its_byte_cap() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = synced_without_parts(&server, &acct, &backend);
    let queued = store.missing_bodies(&acct.id, 100).unwrap().len();
    assert!(queued > 2, "{queued}");
    let _cap = super::bodies::limit_body_bytes(1);
    // Each batch keeps one message's source and asks for the rest later.
    assert_eq!(
        crate::fetch_missing_bodies(&acct.id, &backend, &store, 1).unwrap(),
        1
    );
    assert_eq!(
        store.missing_bodies(&acct.id, 100).unwrap().len(),
        queued - 1
    );
    let fetches = commands(&server)
        .iter()
        .filter(|line| line.contains("UID FETCH"))
        .count();
    assert_eq!(fetches, 1, "no fetch is sent past the cap");
    assert_eq!(
        crate::fetch_missing_bodies(&acct.id, &backend, &store, 100).unwrap(),
        queued - 1
    );
    assert!(store.missing_bodies(&acct.id, 100).unwrap().is_empty());
    let empty = store
        .conn
        .query_row(
            "SELECT COUNT(*) FROM message_bodies WHERE html IS NULL AND plain IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(empty, 0, "nothing deferred was recorded as gone");
}

/// INBOX alone, holding `messages` as (uid, raw), all unread.
fn inbox_state(messages: Vec<(u32, Vec<u8>)>) -> TestState {
    let mut state = sample_state();
    state.mailboxes = vec![FixtureMailbox {
        name: "INBOX".into(),
        list_flags: String::new(),
        uidvalidity: 20,
        uidnext: None,
        omit_uidnext: false,
        reject_examine: false,
        reject_fetch: false,
        vanished: Vec::new(),
        messages: messages
            .into_iter()
            .map(|(uid, raw)| FixtureMessage {
                uid,
                flags: Vec::new(),
                modseq: 1,
                body_form: BodyForm::Literal,
                raw,
            })
            .collect(),
    }];
    state
}

fn temp_cache(tag: &str) -> (std::path::PathBuf, crate::parts::PartCache) {
    let root = std::env::temp_dir().join(format!(
        "fell-part-fetch-{tag}-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let cache = crate::parts::PartCache::open(&root, crate::parts::DEFAULT_CACHE_BYTES).unwrap();
    (root, cache)
}

#[test]
fn attachments_are_listed_on_sync_and_fetched_by_section() {
    use crate::imap::mime_fixtures;
    let fixtures = [
        ("mix@x", mime_fixtures::mixed_two_attachments("mix@x")),
        ("rel@x", mime_fixtures::related_cid_images("rel@x")),
        ("2231@x", mime_fixtures::rfc2231_name("2231@x")),
        ("fwd@x", mime_fixtures::nested_message("fwd@x")),
        ("sig@x", mime_fixtures::pgp_signed("sig@x")),
    ];
    let server = TestServer::spawn(inbox_state(
        fixtures
            .iter()
            .enumerate()
            .map(|(index, (_, fixture))| (index as u32 + 1, fixture.raw.clone()))
            .collect(),
    ));
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();

    let listed = |id: &str| {
        store
            .message_attachments(&acct.id, &crate::MessageId::new(id))
            .unwrap()
            .expect("sync lists parts")
    };
    let summary = |id: &str| {
        listed(id)
            .into_iter()
            .map(|part| (part.section, part.role, part.mime_type, part.filename))
            .collect::<Vec<_>>()
    };
    use crate::PartRole::{Attachment, Inline, Signature};
    let some = |name: &str| Some(name.to_string());
    assert_eq!(
        summary("mix@x"),
        [
            (
                "2".into(),
                Attachment,
                "application/pdf".into(),
                some("report.pdf")
            ),
            (
                "3".into(),
                Attachment,
                "text/csv".into(),
                some("totals.csv")
            ),
        ]
    );
    assert_eq!(
        summary("rel@x"),
        [
            ("2".into(), Inline, "image/png".into(), None),
            (
                "3".into(),
                Attachment,
                "image/png".into(),
                some("stray.png")
            ),
        ]
    );
    assert_eq!(
        summary("2231@x"),
        [(
            "2".into(),
            Attachment,
            "application/octet-stream".into(),
            some("Ñandú 日本語.txt")
        )]
    );
    assert_eq!(
        summary("fwd@x"),
        [
            (
                "2".into(),
                Attachment,
                "message/rfc822".into(),
                some("Original.eml")
            ),
            (
                "2.2".into(),
                Attachment,
                "application/zip".into(),
                some("inner.zip")
            ),
        ]
    );
    assert_eq!(
        summary("sig@x"),
        [(
            "2".into(),
            Signature,
            "application/pgp-signature".into(),
            some("signature.asc")
        )]
    );
    assert!(!stored(&store, &acct, "sig@x").unwrap().has_attachment);
    assert!(stored(&store, &acct, "mix@x").unwrap().has_attachment);

    // Every part comes back as its exact decoded bytes, a window at a time.
    super::part_fetch::set_window(1000);
    let (root, cache) = temp_cache("sections");
    server.state.lock().expect("state").commands.clear();
    for (id, fixture) in &fixtures {
        for (section, bytes) in &fixture.bytes {
            let request = store
                .part_request(&acct.id, &crate::MessageId::new(*id), section)
                .unwrap()
                .unwrap_or_else(|| panic!("{id} {section} is listed"));
            let mut heard = Vec::new();
            let cached =
                crate::parts::fetch_part(&backend, &cache, &request, &mut |n| heard.push(n))
                    .unwrap();
            assert_eq!(
                &std::fs::read(&cached.path).unwrap(),
                bytes,
                "{id} {section}"
            );
            assert_eq!(cached.size, bytes.len() as u64);
            assert_eq!(
                heard.last().copied(),
                Some(request.part.encoded_size),
                "{id}"
            );
            assert!(heard.windows(2).all(|pair| pair[0] < pair[1]), "{heard:?}");
        }
    }
    let logged = commands(&server);
    assert!(
        logged
            .iter()
            .any(|line| line.ends_with("UID FETCH 1 (UID BODY.PEEK[2]<1000.1000>)")),
        "{logged:?}"
    );
    assert!(
        logged
            .iter()
            .any(|line| line.ends_with("UID FETCH 4 (UID BODY.PEEK[2.2]<0.1000>)")),
        "{logged:?}"
    );
    assert!(
        logged
            .iter()
            .filter(|line| line.contains("UID FETCH"))
            .all(|line| line.contains("BODY.PEEK[")),
    );
    assert_read_only(&server);
    assert!(stored(&store, &acct, "mix@x").unwrap().unread);

    // A second fetch is read from the cache, without a connection.
    server.state.lock().expect("state").commands.clear();
    let again = store
        .part_request(&acct.id, &crate::MessageId::new("mix@x"), "2")
        .unwrap()
        .unwrap();
    crate::parts::fetch_part(&backend, &cache, &again, &mut |_| {}).unwrap();
    assert!(commands(&server).is_empty());

    // Opening the related message fetches the image its HTML shows.
    let requests = store
        .part_requests(&acct.id, &crate::MessageId::new("rel@x"))
        .unwrap();
    let inline = crate::parts::prefetch_inline_parts(&backend, &cache, &requests).unwrap();
    assert_eq!(inline.len(), 1);
    assert_eq!(inline[0].0, "logo@x");
    assert_eq!(
        std::fs::read(&inline[0].1.path).unwrap(),
        fixtures[1].1.bytes[0].1
    );
    super::part_fetch::set_window(2 * 1024 * 1024);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_part_whose_copy_is_gone_is_an_error_and_caches_nothing() {
    let fixture = crate::imap::mime_fixtures::mixed_two_attachments("mix@x");
    let server = TestServer::spawn(inbox_state(vec![(1, fixture.raw.clone())]));
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let request = store
        .part_request(&acct.id, &crate::MessageId::new("mix@x"), "2")
        .unwrap()
        .unwrap();
    let (root, cache) = temp_cache("gone");
    // The copy now numbers another part 2: its bytes are not handed over.
    mailbox(&mut server.state.lock().expect("state"), "INBOX").messages[0].raw =
        crate::imap::mime_fixtures::rfc2231_name("mix@x").raw;
    let err = crate::parts::fetch_part(&backend, &cache, &request, &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("where it was listed"), "{err}");
    assert!(cache.lookup(&request).is_none());
    mailbox(&mut server.state.lock().expect("state"), "INBOX")
        .messages
        .clear();
    let err = crate::parts::fetch_part(&backend, &cache, &request, &mut |_| {}).unwrap_err();
    assert!(err.to_string().contains("no server copy"), "{err}");
    assert!(cache.lookup(&request).is_none());
    // A new UIDVALIDITY is gone too, without a UID FETCH.
    mailbox(&mut server.state.lock().expect("state"), "INBOX").uidvalidity = 21;
    server.state.lock().expect("state").commands.clear();
    assert!(crate::parts::fetch_part(&backend, &cache, &request, &mut |_| {}).is_err());
    assert!(
        !commands(&server)
            .iter()
            .any(|line| line.contains("UID FETCH"))
    );
    // A section that is not a part number never reaches the server.
    let mut odd = request.clone();
    odd.part.section = "1 (FLAGS)".into();
    assert!(crate::parts::fetch_part(&backend, &cache, &odd, &mut |_| {}).is_err());
    let _ = std::fs::remove_dir_all(&root);
}

/// Sync `fixture` from a server that honours `<offset.len>` or ignores it,
/// and the request for its part 2.
fn part_server(
    fixture: &crate::imap::mime_fixtures::MimeFixture,
    ignores_window: bool,
) -> (TestServer, ImapBackend, crate::parts::PartRequest) {
    let mut state = inbox_state(vec![(1, fixture.raw.clone())]);
    state.part_ignores_window = ignores_window;
    let server = TestServer::spawn(state);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let request = store
        .part_request(&acct.id, &crate::MessageId::new("big@x"), "2")
        .unwrap()
        .unwrap();
    (server, backend, request)
}

fn part_fetches(server: &TestServer) -> usize {
    commands(server)
        .iter()
        .filter(|line| line.contains("BODY.PEEK[2]"))
        .count()
}

#[test]
fn a_server_that_ignores_the_window_sends_a_10_mb_part_whole() {
    let size = 10 * 1024 * 1024;
    let fixture = crate::imap::mime_fixtures::large_attachment("big@x", size);
    let (server, backend, request) = part_server(&fixture, true);
    assert!(
        request.part.encoded_size > 8 * 1024 * 1024,
        "over the reply cap"
    );
    let (root, cache) = temp_cache("whole");
    server.state.lock().expect("state").commands.clear();
    let mut heard = Vec::new();
    let cached =
        crate::parts::fetch_part(&backend, &cache, &request, &mut |n| heard.push(n)).unwrap();
    assert_eq!(std::fs::read(&cached.path).unwrap(), fixture.bytes[0].1);
    assert_eq!(heard, [request.part.encoded_size]);
    assert_eq!(part_fetches(&server), 1, "one reply held the whole part");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_part_exactly_one_window_long_is_fetched_once() {
    let fixture = crate::imap::mime_fixtures::large_attachment("big@x", 3000);
    for ignores_window in [false, true] {
        let (server, backend, request) = part_server(&fixture, ignores_window);
        let window = usize::try_from(request.part.encoded_size).unwrap();
        super::part_fetch::set_window(window);
        let (root, cache) = temp_cache(if ignores_window { "exact-i" } else { "exact" });
        server.state.lock().expect("state").commands.clear();
        let cached = crate::parts::fetch_part(&backend, &cache, &request, &mut |_| {});
        super::part_fetch::set_window(2 * 1024 * 1024);
        let cached = cached.unwrap();
        assert_eq!(
            std::fs::read(&cached.path).unwrap(),
            fixture.bytes[0].1,
            "ignores the window: {ignores_window}"
        );
        assert_eq!(
            part_fetches(&server),
            1,
            "ignores the window: {ignores_window}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    // Smaller windows still add up to the part when the server honours them.
    let (server, backend, request) = part_server(&fixture, false);
    let window = usize::try_from(request.part.encoded_size).unwrap() / 2;
    super::part_fetch::set_window(window);
    let (root, cache) = temp_cache("halves");
    server.state.lock().expect("state").commands.clear();
    let cached = crate::parts::fetch_part(&backend, &cache, &request, &mut |_| {});
    super::part_fetch::set_window(2 * 1024 * 1024);
    assert_eq!(
        std::fs::read(&cached.unwrap().path).unwrap(),
        fixture.bytes[0].1
    );
    assert!(
        (2..=3).contains(&part_fetches(&server)),
        "{:?}",
        commands(&server)
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_10_mb_part_holds_up_neither_the_caller_nor_a_sync_pass() {
    use std::time::{Duration, Instant};
    let size = 10 * 1024 * 1024;
    let fixture = crate::imap::mime_fixtures::large_attachment("big@x", size);
    let syncing = TestServer::spawn(inbox_state(vec![(1, fixture.raw.clone())]));
    // The part comes from its own connection, slowly: about 2 s for 13 MB.
    let mut slow = inbox_state(vec![(1, fixture.raw.clone())]);
    slow.part_write_delay = Some(Duration::from_millis(10));
    let parts_server = TestServer::spawn(slow);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(syncing.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let request = store
        .part_request(&acct.id, &crate::MessageId::new("big@x"), "2")
        .unwrap()
        .unwrap();
    assert_eq!(request.part.size, size as u64);

    let (root, cache) = temp_cache("big");
    let part_backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(parts_server.addr.port()),
    };
    let fetcher = crate::parts::PartFetcher::start(
        move || Ok(Box::new(part_backend.clone()) as Box<dyn MailBackend + Send>),
        cache,
    );
    let (events, heard) = std::sync::mpsc::channel();
    let asked = Instant::now();
    fetcher.fetch(request.clone(), move |event| {
        let _ = events.send(event);
    });
    assert!(
        asked.elapsed() < Duration::from_millis(100),
        "fetch() returns at once"
    );
    match heard.recv_timeout(Duration::from_secs(30)).unwrap() {
        crate::parts::PartEvent::Progress { fetched, total } => {
            assert!(fetched < total);
            assert_eq!(total, request.part.encoded_size);
        }
        crate::parts::PartEvent::Done(result) => panic!("done too soon: {result:?}"),
    }

    // A sync pass runs to the end while the part is still downloading.
    mailbox(&mut syncing.state.lock().expect("state"), "INBOX")
        .messages
        .push(FixtureMessage {
            uid: 2,
            flags: Vec::new(),
            modseq: 2,
            body_form: BodyForm::Literal,
            raw: raw_message("new@x", None, "New", "Ann <ann@x>", "new mail", ""),
        });
    sync_once(&acct, &backend, &store).unwrap();
    assert!(stored(&store, &acct, "new@x").is_some());
    let mut done = None;
    while let Ok(event) = heard.try_recv() {
        if let crate::parts::PartEvent::Done(result) = event {
            done = Some(result);
        }
    }
    assert!(done.is_none(), "the sync pass finished first");

    let deadline = Instant::now() + Duration::from_secs(120);
    let cached = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match heard.recv_timeout(left).expect("the fetch finishes") {
            crate::parts::PartEvent::Progress { .. } => continue,
            crate::parts::PartEvent::Done(result) => break result.unwrap(),
        }
    };
    assert_eq!(cached.size, size as u64);
    assert!(std::fs::read(&cached.path).unwrap() == fixture.bytes[0].1);
    let windows = commands(&parts_server)
        .iter()
        .filter(|line| line.contains("BODY.PEEK[2]<"))
        .count();
    assert_eq!(windows, 7, "2 MB windows of a 13.6 MB part");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_cancelled_part_fetch_stops_and_closes_its_session_and_drop_waits_for_it() {
    use std::time::{Duration, Instant};
    let size = 10 * 1024 * 1024;
    let fixture = crate::imap::mime_fixtures::large_attachment("big@x", size);
    let syncing = TestServer::spawn(inbox_state(vec![(1, fixture.raw.clone())]));
    let mut slow = inbox_state(vec![(1, fixture.raw.clone())]);
    slow.part_write_delay = Some(Duration::from_millis(10));
    let parts_server = TestServer::spawn(slow);
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(syncing.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let request = store
        .part_request(&acct.id, &crate::MessageId::new("big@x"), "2")
        .unwrap()
        .unwrap();
    let (root, cache) = temp_cache("cancel");
    let part_backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(parts_server.addr.port()),
    };
    let fetcher = crate::parts::PartFetcher::start(
        move || Ok(Box::new(part_backend.clone()) as Box<dyn MailBackend + Send>),
        cache.clone(),
    );
    let channel = || std::sync::mpsc::channel::<crate::parts::PartEvent>();
    let (running_events, running) = channel();
    let (queued_events, queued) = channel();
    let first = fetcher.fetch(request.clone(), move |event| {
        let _ = running_events.send(event);
    });
    let second = fetcher.fetch(request.clone(), move |event| {
        let _ = queued_events.send(event);
    });
    // The queued job, cancelled before it starts, never connects.
    second.cancel();
    assert!(matches!(
        running.recv_timeout(Duration::from_secs(30)).unwrap(),
        crate::parts::PartEvent::Progress { .. }
    ));
    let asked = Instant::now();
    first.cancel();
    let done = loop {
        match running.recv_timeout(Duration::from_secs(30)).unwrap() {
            crate::parts::PartEvent::Progress { .. } => {}
            crate::parts::PartEvent::Done(result) => break result,
        }
    };
    assert!(
        asked.elapsed() < Duration::from_secs(5),
        "stopped after a window, not the whole part: {:?}",
        asked.elapsed()
    );
    assert!(done.unwrap_err().to_string().contains("cancelled"));
    match queued.recv_timeout(Duration::from_secs(30)).unwrap() {
        crate::parts::PartEvent::Done(result) => {
            assert!(result.unwrap_err().to_string().contains("cancelled"));
        }
        other @ crate::parts::PartEvent::Progress { .. } => panic!("{other:?}"),
    }
    let logged = commands(&parts_server);
    let count = |verb: &str| logged.iter().filter(|line| line.contains(verb)).count();
    assert_eq!(count("EXAMINE"), 1, "one session: {logged:?}");
    assert_eq!(count("LOGOUT"), 1, "the cancelled session is closed");
    assert!(
        cache.lookup(&request).is_none(),
        "nothing half-fetched is kept"
    );

    // Dropping the fetcher cancels the running job and waits for its thread.
    parts_server.state.lock().expect("state").commands.clear();
    let (events, heard) = channel();
    fetcher.fetch(request.clone(), move |event| {
        let _ = events.send(event);
    });
    assert!(matches!(
        heard.recv_timeout(Duration::from_secs(30)).unwrap(),
        crate::parts::PartEvent::Progress { .. }
    ));
    drop(fetcher);
    let mut finished = None;
    while let Ok(event) = heard.try_recv() {
        if let crate::parts::PartEvent::Done(result) = event {
            finished = Some(result);
        }
    }
    let finished = finished.expect("Done arrived before drop returned");
    assert!(finished.unwrap_err().to_string().contains("cancelled"));
    assert!(
        commands(&parts_server)
            .iter()
            .any(|line| line.contains("LOGOUT"))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn bodies_opened_together_are_fetched_in_one_login() {
    let server = TestServer::spawn(sample_state());
    let acct = account("a1", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    store
        .conn
        .execute_batch("DELETE FROM message_bodies WHERE message_id IN ('root@x', 'smile@x');")
        .unwrap();
    server.state.lock().expect("state").commands.clear();
    let ids = [
        crate::MessageId::new("root@x"),
        crate::MessageId::new("smile@x"),
        // Unknown here: nothing to fetch, which is an answer.
        crate::MessageId::new("nowhere@x"),
    ];
    let answers = crate::fetch_message_bodies(&acct.id, &backend, &store, &ids).unwrap();
    assert_eq!(answers.len(), 3);
    assert!(
        answers[0]
            .as_ref()
            .is_some_and(|parts| parts.plain.is_some() || parts.html.is_some())
    );
    assert!(
        answers[1]
            .as_ref()
            .is_some_and(|parts| parts.plain.is_some() || parts.html.is_some())
    );
    assert_eq!(answers[2], Some(crate::MessageParts::default()));
    let logins = commands(&server)
        .iter()
        .filter(|line| line.starts_with("LOGIN"))
        .count();
    assert_eq!(logins, 1, "{:?}", commands(&server));
    assert_read_only(&server);
}
