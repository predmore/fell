use crate::account::BackendKind;
use crate::backend::{BridgeBackend, ImapBackend, MailBackend};
use crate::{FlagChange, LabelChange, MessageId, Store, sync_once};

use super::sync_tests::{account, commands, config, sample_state, unused_smtp};
use super::test_server::{TestServer, TestState};

fn keyword_state(with_sent_copy: bool) -> TestState {
    let mut state = sample_state();
    state.condstore = true;
    state
        .mailboxes
        .retain(|mailbox| mailbox.name == "INBOX" || (with_sent_copy && mailbox.name == "Sent"));
    for mailbox in &mut state.mailboxes {
        mailbox.messages.retain(|message| message.uid == 1);
    }
    state
}

fn set_flags(server: &TestServer, mailbox: &str, revision: u64, flags: &[&str]) {
    let mut state = server.state.lock().unwrap();
    let copy = &mut state
        .mailboxes
        .iter_mut()
        .find(|entry| entry.name == mailbox)
        .unwrap()
        .messages[0];
    copy.flags = flags.iter().map(|flag| (*flag).into()).collect();
    copy.modseq = revision;
    state.commands.clear();
}

fn assert_flag_only(server: &TestServer) {
    let logged = commands(server);
    assert!(
        logged.iter().any(|line| line.contains("(UID FLAGS")),
        "{logged:?}"
    );
    assert!(
        !logged.iter().any(|line| line.contains("BODY.PEEK[]")),
        "{logged:?}"
    );
}

#[test]
fn changed_since_adds_and_removes_keywords_with_local_labels_and_flag_intents() {
    let server = TestServer::spawn(keyword_state(false));
    let acct = account("imap", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
        smtp: unused_smtp(),
    };
    let store = Store::open_in_memory().unwrap();
    let message = || {
        store
            .message(&acct.id, &MessageId::new("root@x"))
            .unwrap()
            .unwrap()
    };

    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["work"]);
    let thread = message().thread_id;
    for label in ["mine", "personal"] {
        store
            .label_thread(&acct.id, &thread, LabelChange::Add(label.into()))
            .unwrap();
    }
    store
        .flag_threads(
            &acct.id,
            std::slice::from_ref(&thread),
            "INBOX",
            FlagChange::Unread,
        )
        .unwrap();
    store
        .flag_threads(
            &acct.id,
            std::slice::from_ref(&thread),
            "INBOX",
            FlagChange::Star,
        )
        .unwrap();

    set_flags(&server, "INBOX", 2, &["\\Seen", "urgent", "PERSONAL"]);
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.new_messages, Some(0));
    assert_eq!(message().labels, ["mine", "personal", "urgent"]);
    assert_eq!(
        store.folder_threads(&acct.id, "INBOX").unwrap()[0].labels,
        message().labels
    );
    assert_eq!(
        store.search(&acct.id, "label:urgent").unwrap().hits.len(),
        1
    );
    assert!(message().unread);
    assert!(message().starred);
    assert_flag_only(&server);

    set_flags(&server, "INBOX", 3, &["\\Seen", "\\Answered"]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["mine", "personal"]);
    assert!(
        store
            .search(&acct.id, "label:urgent")
            .unwrap()
            .hits
            .is_empty()
    );
    assert_eq!(message().body_text.as_deref(), Some("hello body"));
    assert!(message().unread);
    assert!(message().starred);
    assert_flag_only(&server);

    set_flags(&server, "INBOX", 4, &["urgent"]);
    sync_once(&acct, &backend, &store).unwrap();
    store
        .label_thread(&acct.id, &thread, LabelChange::Remove("urgent".into()))
        .unwrap();
    store
        .label_thread(&acct.id, &thread, LabelChange::Add("urgent".into()))
        .unwrap();
    set_flags(&server, "INBOX", 5, &[]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["mine", "personal", "urgent"]);
    assert_flag_only(&server);
}

#[test]
fn a_keyword_survives_until_its_last_copy_loses_it() {
    let server = TestServer::spawn(keyword_state(true));
    let acct = account("imap", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
        smtp: unused_smtp(),
    };
    let store = Store::open_in_memory().unwrap();
    let message = || {
        store
            .message(&acct.id, &MessageId::new("root@x"))
            .unwrap()
            .unwrap()
    };

    sync_once(&acct, &backend, &store).unwrap();
    set_flags(&server, "Sent", 2, &["WORK", "sentonly"]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["work", "sentonly"]);
    assert_flag_only(&server);

    set_flags(&server, "INBOX", 3, &[]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["work", "sentonly"]);
    assert_flag_only(&server);

    set_flags(&server, "Sent", 4, &["sentonly"]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["sentonly"]);
    assert_flag_only(&server);

    {
        let mut state = server.state.lock().unwrap();
        let sent = state
            .mailboxes
            .iter_mut()
            .find(|entry| entry.name == "Sent")
            .unwrap();
        sent.messages.clear();
        sent.uidnext = Some(2);
        state.commands.clear();
    }
    sync_once(&acct, &backend, &store).unwrap();
    assert!(message().labels.is_empty());
    assert_eq!(message().folders, ["INBOX"]);
    assert_flag_only(&server);
}

#[test]
fn the_flag_baseline_updates_an_upgraded_row_and_keeps_legacy_labels() {
    let mut state = keyword_state(false);
    state.condstore = false;
    let server = TestServer::spawn(state);
    let acct = account("imap", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
        smtp: unused_smtp(),
    };
    let path = std::env::temp_dir().join(format!(
        "fell-keyword-migration-{}.sqlite",
        std::process::id()
    ));
    {
        let store = Store::open(&path).unwrap();
        store.upsert_account(&acct).unwrap();
        let mut delta = backend.fetch_changes(None).unwrap();
        // The old backend sent initial labels on the message only.
        delta.flags.clear();
        store.apply_delta(&acct.id, &delta).unwrap();
        store
            .conn
            .execute_batch(
                "ALTER TABLE message_copies DROP COLUMN keywords;
             DROP TABLE message_keyword_labels;",
            )
            .unwrap();
    }

    let store = Store::open(&path).unwrap();
    let message = || {
        store
            .message(&acct.id, &MessageId::new("root@x"))
            .unwrap()
            .unwrap()
    };
    server.state.lock().unwrap().condstore = true;
    set_flags(&server, "INBOX", 2, &["fresh"]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["work", "fresh"]);
    let logged = commands(&server);
    assert!(logged.iter().any(|line| line.contains("UID FETCH 1:* (UID FLAGS") && !line.contains("CHANGEDSINCE")), "{logged:?}");
    assert_flag_only(&server);

    set_flags(&server, "INBOX", 3, &[]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["work"]);
    assert_flag_only(&server);
    drop(store);
    std::fs::remove_file(path).unwrap();
}

#[test]
fn bridge_keeps_mailbox_labels_when_copy_flags_change() {
    let mut state = keyword_state(true);
    state
        .mailboxes
        .iter_mut()
        .find(|entry| entry.name == "Sent")
        .unwrap()
        .name = "Labels/Projects".into();
    let server = TestServer::spawn(state);
    let acct = account("bridge", BackendKind::Bridge);
    let backend = BridgeBackend {
        account_id: acct.id.clone(),
        imap: config(server.addr.port()),
        ..BridgeBackend::default()
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let id = MessageId::new("root@x");
    let before = store.message(&acct.id, &id).unwrap().unwrap();
    assert!(
        before
            .folders
            .iter()
            .any(|folder| folder == "Labels/Projects")
    );

    set_flags(&server, "INBOX", 2, &["outside"]);
    sync_once(&acct, &backend, &store).unwrap();
    let after = store.message(&acct.id, &id).unwrap().unwrap();
    assert_eq!(after.folders, before.folders);
    assert_eq!(after.labels, before.labels);
    assert!(after.unread);
    assert_flag_only(&server);
}
