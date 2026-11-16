use crate::account::{Account, AccountId, BackendKind};
use crate::backend::{BridgeBackend, ImapBackend, MailBackend};
use crate::imap::{ImapConfig, ImapSecurity};
use crate::store::Store;
use crate::sync::sync_once;
use crate::threading::ThreadId;

use super::test_server::{FixtureMailbox, FixtureMessage, TestServer, TestState, raw_message};

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
    }
}

fn sample_state() -> TestState {
    TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        commands: Vec::new(),
        mailboxes: vec![
            FixtureMailbox {
                name: "Parent".into(),
                list_flags: "\\Noselect".into(),
                uidvalidity: 1,
                uidnext: None,
                messages: Vec::new(),
            },
            FixtureMailbox {
                name: "Sent".into(),
                list_flags: String::new(),
                uidvalidity: 10,
                uidnext: None,
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
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
                messages: vec![
                    FixtureMessage {
                        uid: 1,
                        flags: vec!["\\Seen".into(), "work".into()],
                        modseq: 1,
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
                        raw: raw_message("gone@x", None, "Gone", "Ann <ann@x>", "gone", ""),
                    },
                    FixtureMessage {
                        uid: 4,
                        flags: vec!["\\Seen".into()],
                        modseq: 1,
                        raw: b"From: Ann <ann@x>\r\nSubject: Html\r\nMessage-ID: <html@x>\r\nDate: Mon, 16 Nov 2026 12:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<html><body><p>Hello from html</p></body></html>".to_vec(),
                    },
                    FixtureMessage {
                        uid: 5,
                        flags: vec!["\\Seen".into()],
                        modseq: 1,
                        raw: b"From: Ann <ann@x>\r\nSubject: No id\r\nDate: Mon, 16 Nov 2026 12:00:00 +0000\r\n\r\nmissing id\r\n".to_vec(),
                    },
                ],
            },
            FixtureMailbox {
                name: "&Jjo-".into(),
                list_flags: String::new(),
                uidvalidity: 7,
                uidnext: None,
                messages: vec![FixtureMessage {
                    uid: 1,
                    flags: vec!["\\Seen".into()],
                    modseq: 1,
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
    assert_eq!(root.folders, vec!["Sent", "INBOX"]);
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
    sync_once(&acct, &backend, &store).unwrap();
    let threads = store.list_threads(&acct.id).unwrap();
    // The message with no Message-ID is keyed by UIDVALIDITY, so that one row
    // is new. Messages that have a Message-ID stay one row.
    assert_eq!(threads.len(), before + 1);
    let proton = threads
        .iter()
        .find(|thread| thread.thread_id == ThreadId::proton("conv-9"))
        .unwrap();
    assert_eq!(proton.message_count, 2);
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
    }
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 1);
    assert_eq!(store.list_threads(&acct.id).unwrap().len(), 1);
    assert!(store.list_threads(&acct.id).unwrap()[0].unread);
    let logged = commands(&server);
    assert!(logged.iter().any(|line| line.contains("CHANGEDSINCE")));
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
            raw: raw_message("one@x", None, "Same", "Ann <ann@x>", "same body", ""),
        },
        FixtureMessage {
            uid: 2,
            flags: vec!["\\Seen".into()],
            modseq: 1,
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

/// RFC 3501: `6:*` still returns UID 5 when 5 is the highest UID. UIDNEXT is
/// past that, so the client must fetch and then drop the already-stored UID.
#[test]
fn star_range_does_not_reapply_the_highest_uid() {
    let server = TestServer::spawn(TestState {
        username: "caleb".into(),
        password: "secret".into(),
        condstore: false,
        close_after_starttls: false,
        commands: Vec::new(),
        mailboxes: vec![FixtureMailbox {
            name: "INBOX".into(),
            list_flags: String::new(),
            uidvalidity: 3,
            uidnext: Some(10),
            messages: vec![FixtureMessage {
                uid: 5,
                flags: vec!["\\Seen".into()],
                modseq: 1,
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
    {
        let mut state = server.state.lock().expect("state");
        state.commands.clear();
    }
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 0);
    assert_eq!(store.list_threads(&acct.id).unwrap().len(), 1);
    let logged = commands(&server);
    assert!(
        logged
            .iter()
            .any(|line| line.contains("UID FETCH") && line.contains("6:*")),
        "high-water fetch should run because UIDNEXT is past the checkpoint: {logged:?}"
    );
}
