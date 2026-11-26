use crate::account::BackendKind;
use crate::backend::{BridgeBackend, ImapBackend, MailBackend, MailboxDelta};
use crate::{AgentGrant, FlagChange, LabelChange, MessageId, Store, sync_once};

use super::sync_tests::{account, commands, config, sample_state, unused_smtp};
use super::test_server::{TestServer, TestState, raw_message};
use super::{decode_cursor, encode_cursor, is_reserved_keyword};

#[test]
fn reserved_keywords_match_exact_atoms_without_case() {
    for flag in [
        r"\Seen",
        "$fOrWaRdEd",
        "$MDNSent",
        "$SubmitPending",
        "$Submitted",
        "$Junk",
        "$NotJunk",
        "$Phishing",
        "$Important",
        "$Draft",
        "$Seen",
        "$Flagged",
        "$Answered",
        "$Recent",
        "$AutoSent",
        "$CanUnsubscribe",
        "$Followed",
        "$HasAttachment",
        "$HasMemo",
        "$HasNoAttachment",
        "$Imported",
        "$IsTrusted",
        "$MailFlagBit0",
        "$MailFlagBit1",
        "$MailFlagBit2",
        "$MaskedEmail",
        "$Memo",
        "$Muted",
        "$New",
        "$Notify",
        "$Unsubscribed",
        "forwarded",
        "redirected",
        "JUNK",
        "NonJunk",
        "notjunk",
    ] {
        assert!(is_reserved_keyword(flag), "{flag}");
    }
    for label in [
        "Travel",
        "$Travel",
        "$ForwardedToTeam",
        "$ImportantProject",
        "$MailFlagBit3",
        "$label1",
        "$Label5",
    ] {
        assert!(!is_reserved_keyword(label), "{label}");
    }
}

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
fn a_legacy_checkpoint_at_highest_modseq_gets_one_keyword_baseline() {
    let mut state = keyword_state(false);
    state.mailboxes[0].messages[0].flags = vec!["\\Seen".into()];
    let server = TestServer::spawn(state);
    let acct = account("imap", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
        smtp: unused_smtp(),
    };
    let store = Store::open_in_memory().unwrap();
    let id = MessageId::new("root@x");
    sync_once(&acct, &backend, &store).unwrap();
    let message = store.message(&acct.id, &id).unwrap().unwrap();
    store
        .flag_threads(&acct.id, &[message.thread_id], "INBOX", FlagChange::Unread)
        .unwrap();
    let pending = store.pending_flag_changes(&acct.id).unwrap();
    assert!(!pending.is_empty());

    set_flags(&server, "INBOX", 7, &["fieldwork"]);
    // An older client reached revision 7 and retained the body, but did
    // not record the keyword. It also has a pending local read change.
    let mut checkpoints = decode_cursor(store.sync_cursor(&acct.id).unwrap().as_ref());
    checkpoints[0].1.modseq = Some(7);
    checkpoints[0].1.keyword_baseline = false;
    let cursor = encode_cursor(acct.id.as_str(), &checkpoints);
    store
        .apply_delta(
            &acct.id,
            &MailboxDelta {
                cursor: Some(cursor),
                ..MailboxDelta::default()
            },
        )
        .unwrap();
    store
        .conn
        .execute(
            "UPDATE message_copies SET modseq = ?1, seen_handled = 0",
            [7u64.to_be_bytes()],
        )
        .unwrap();

    // An interrupted baseline must stay pending. CHANGEDSINCE succeeds at
    // the saved revision and has no changed rows to return.
    let known =
        |mailbox: &str, validity, range| store.stored_uids(&acct.id, mailbox, validity, range);
    let cursor = store.sync_cursor(&acct.id).unwrap();
    let err = backend
        .sync_changes(cursor.as_ref(), &known, &mut |delta| {
            if decode_cursor(delta.cursor.as_ref())
                .iter()
                .any(|(_, cp)| cp.keyword_baseline)
            {
                return Err(crate::Error::Backend("interrupted keyword baseline".into()));
            }
            store.apply_delta(&acct.id, &delta)
        })
        .unwrap_err();
    assert!(err.to_string().contains("interrupted keyword baseline"));
    assert_eq!(
        decode_cursor(store.sync_cursor(&acct.id).unwrap().as_ref()),
        checkpoints
    );
    assert!(
        store
            .message(&acct.id, &id)
            .unwrap()
            .unwrap()
            .labels
            .is_empty()
    );

    server.state.lock().unwrap().commands.clear();
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.new_messages, Some(0));
    let after = store.message(&acct.id, &id).unwrap().unwrap();
    assert_eq!(after.labels, ["fieldwork"]);
    assert_eq!(after.body_text, message.body_text);
    assert_eq!(store.pending_flag_changes(&acct.id).unwrap(), pending);
    assert_flag_only(&server);
    let commands_after_upgrade = commands(&server);
    assert!(
        commands_after_upgrade
            .iter()
            .any(|line| line.contains("(CHANGEDSINCE 7)"))
    );
    assert!(
        commands_after_upgrade
            .iter()
            .any(|line| line.contains("UID FETCH 1 (UID FLAGS MODSEQ)"))
    );
    checkpoints[0].1.keyword_baseline = true;
    assert_eq!(
        decode_cursor(store.sync_cursor(&acct.id).unwrap().as_ref()),
        checkpoints
    );

    server.state.lock().unwrap().commands.clear();
    sync_once(&acct, &backend, &store).unwrap();
    assert!(
        commands(&server)
            .iter()
            .filter(|line| line.contains("(UID FLAGS"))
            .all(|line| line.contains("CHANGEDSINCE"))
    );
}

#[test]
fn keyword_spellings_collapse_and_mixed_case_readds_keep_local_ownership() {
    let mut state = keyword_state(true);
    let body = "The complete field report includes the dates and the route schedule.";
    let sent = state
        .mailboxes
        .iter_mut()
        .find(|mailbox| mailbox.name == "Sent")
        .unwrap();
    sent.messages[0].flags = vec!["Work".into(), "local".into()];
    sent.messages[0].raw = raw_message("post@x", None, "Field report", "Ann <ann@x>", body, "");
    let echo = |text| {
        raw_message(
            "post-list@x",
            None,
            "[team] Field report",
            "Ann <ann@x>",
            text,
            "List-Id: Team <team.x.com>\r\n",
        )
    };
    let inbox = state
        .mailboxes
        .iter_mut()
        .find(|mailbox| mailbox.name == "INBOX")
        .unwrap();
    inbox.messages[0].flags = vec!["WORK".into()];
    inbox.messages[0].raw = echo("Earlier material with a different body.");
    let server = TestServer::spawn(state);
    let acct = account("imap", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
        smtp: unused_smtp(),
    };
    let store = Store::open_in_memory().unwrap();
    let sent_id = MessageId::new("post@x");
    let echo_id = MessageId::new("post-list@x");
    let message = || store.message(&acct.id, &sent_id).unwrap().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    let list_copy = store.message(&acct.id, &echo_id).unwrap().unwrap();
    assert_eq!(list_copy.labels, ["WORK"]);
    assert_eq!(message().labels, ["Work", "local"]);
    for label in ["LOCAL", "Tag", "TAG"] {
        store
            .label_thread(
                &acct.id,
                &list_copy.thread_id,
                LabelChange::Add(label.into()),
            )
            .unwrap();
    }

    {
        let mut state = server.state.lock().unwrap();
        let inbox = state
            .mailboxes
            .iter_mut()
            .find(|mailbox| mailbox.name == "INBOX")
            .unwrap();
        inbox.uidvalidity += 1;
        inbox.omit_uidnext = true;
        inbox.messages[0].raw = echo(body);
    }
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(store.list_threads(&acct.id).unwrap().len(), 1);
    assert_eq!(message().labels, ["Work", "LOCAL", "Tag", "TAG"]);
    let change = |label| {
        store
            .label_thread(&acct.id, &message().thread_id, label)
            .unwrap()
    };
    change(LabelChange::Remove("Work".into()));
    change(LabelChange::Add("wOrK".into()));
    store.undo(&acct.id).unwrap();
    assert_eq!(message().labels, ["LOCAL", "Tag", "TAG"]);
    store.undo(&acct.id).unwrap();
    assert_eq!(message().labels, ["Work", "LOCAL", "Tag", "TAG"]);

    set_flags(&server, "INBOX", 2, &[]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["Work", "LOCAL", "Tag", "TAG"]);
    set_flags(&server, "Sent", 2, &[]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["LOCAL", "Tag", "TAG"]);
    assert_flag_only(&server);

    set_flags(&server, "Sent", 3, &["WORK"]);
    sync_once(&acct, &backend, &store).unwrap();
    change(LabelChange::Remove("WORK".into()));
    change(LabelChange::Add("wOrK".into()));
    set_flags(&server, "Sent", 4, &[]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["LOCAL", "Tag", "TAG", "wOrK"]);
    assert_flag_only(&server);
}

#[test]
fn agent_label_undo_keeps_later_server_labels() {
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
    let thread = message().thread_id;
    store
        .set_agent_grant(&acct.id, "bot", AgentGrant::enabled(false, true, false))
        .unwrap();
    store
        .add_thread_label(&acct.id, &thread, "personal", "agent:bot")
        .unwrap();

    set_flags(&server, "INBOX", 2, &["fieldwork"]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["personal", "fieldwork"]);
    store.undo_actor(&acct.id, "agent:bot").unwrap();
    assert_eq!(message().labels, ["fieldwork"]);
    assert_flag_only(&server);

    store
        .add_thread_label(&acct.id, &thread, "FIELDWORK", "agent:bot")
        .unwrap();
    store.undo_actor(&acct.id, "agent:bot").unwrap();
    assert_eq!(message().labels, ["fieldwork"]);

    store
        .add_thread_label(&acct.id, &thread, "personal", "agent:bot")
        .unwrap();
    set_flags(&server, "INBOX", 3, &["fieldwork", "PERSONAL"]);
    sync_once(&acct, &backend, &store).unwrap();
    store.undo_actor(&acct.id, "agent:bot").unwrap();
    assert_eq!(message().labels, ["fieldwork", "personal"]);

    set_flags(&server, "INBOX", 4, &["fieldwork"]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["fieldwork"]);
    assert_flag_only(&server);
}

#[test]
fn local_label_undo_restores_a_keyword_only_while_a_copy_supports_it() {
    let mut state = keyword_state(true);
    for mailbox in &mut state.mailboxes {
        mailbox.messages[0].flags = vec!["triage".into()];
    }
    let server = TestServer::spawn(state);
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
    let thread = message().thread_id;
    let remove = || {
        store
            .label_thread(&acct.id, &thread, LabelChange::Remove("triage".into()))
            .unwrap()
    };

    remove();
    store
        .label_thread(&acct.id, &thread, LabelChange::Add("triage".into()))
        .unwrap();
    store.undo(&acct.id).unwrap();
    assert!(message().labels.is_empty());

    set_flags(&server, "INBOX", 2, &["fieldwork"]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["fieldwork"]);
    store.undo(&acct.id).unwrap();
    assert!(message().labels.iter().any(|label| label == "triage"));
    assert_flag_only(&server);

    remove();
    set_flags(&server, "Sent", 2, &[]);
    sync_once(&acct, &backend, &store).unwrap();
    store.undo(&acct.id).unwrap();
    assert_eq!(message().labels, ["fieldwork"]);
    assert_flag_only(&server);
}

#[test]
fn local_undo_readd_preserves_a_fresh_server_keyword() {
    undo_readd_preserves_a_fresh_server_keyword(false, "triage");
}

#[test]
fn agent_undo_readd_preserves_a_fresh_server_keyword() {
    undo_readd_preserves_a_fresh_server_keyword(true, "TrIaGe");
}

fn undo_readd_preserves_a_fresh_server_keyword(agent: bool, local_spelling: &str) {
    let mut state = keyword_state(true);
    for mailbox in &mut state.mailboxes {
        let keyword = if mailbox.name == "INBOX" {
            "triage"
        } else {
            "TRIAGE"
        };
        mailbox.messages[0].flags = vec![keyword.into()];
    }
    let server = TestServer::spawn(state);
    let acct = account("imap", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
        smtp: unused_smtp(),
    };
    let store = Store::open_in_memory().unwrap();
    let id = MessageId::new("root@x");
    let message = || store.message(&acct.id, &id).unwrap().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(message().labels, ["triage"]);
    let thread = message().thread_id;
    if agent {
        store
            .set_agent_grant(&acct.id, "bot", AgentGrant::enabled(false, true, false))
            .unwrap();
    }
    let readd = || {
        if agent {
            store
                .remove_thread_label(&acct.id, &thread, "triage", "agent:bot")
                .unwrap()
                .unwrap();
            store
                .add_thread_label(&acct.id, &thread, local_spelling, "agent:bot")
                .unwrap()
                .unwrap();
        } else {
            store
                .label_thread(&acct.id, &thread, LabelChange::Remove("triage".into()))
                .unwrap();
            store
                .label_thread(&acct.id, &thread, LabelChange::Add(local_spelling.into()))
                .unwrap();
        }
    };
    let undo = || {
        if agent {
            store.undo_actor(&acct.id, "agent:bot").unwrap();
        } else {
            store.undo(&acct.id).unwrap();
        }
    };
    let sync_flags = |mailbox, revision, flags: &[&str]| {
        set_flags(&server, mailbox, revision, flags);
        assert_eq!(
            sync_once(&acct, &backend, &store).unwrap().new_messages,
            Some(0)
        );
        assert_flag_only(&server);
    };

    readd();
    sync_flags("INBOX", 2, &[]);
    sync_flags("INBOX", 3, &["TRIAGE"]);
    undo();
    assert!(
        message().labels.is_empty(),
        "Sent always carried the keyword"
    );
    undo();
    assert_eq!(message().labels, ["triage"]);

    readd();
    sync_flags("INBOX", 4, &[]);
    sync_flags("Sent", 2, &[]);
    assert_eq!(message().labels, [local_spelling]);
    sync_flags("INBOX", 5, &["TRIAGE"]);
    sync_flags("INBOX", 6, &[]);
    assert_eq!(
        message().labels,
        [local_spelling],
        "the active local add owns the label"
    );

    sync_flags("Sent", 3, &["tRiAgE"]);
    undo();
    assert_eq!(
        message().labels,
        [local_spelling],
        "the fresh server assertion remains visible"
    );
    sync_flags("Sent", 4, &[]);
    assert!(
        message().labels.is_empty(),
        "undo returned ownership to sync"
    );
    undo();
    assert!(
        message().labels.is_empty(),
        "the original remove still recorded a server label"
    );
}

#[test]
fn legacy_label_actions_undo_only_their_own_edits_after_reopening() {
    let mut state = keyword_state(false);
    state.mailboxes[0].messages[0].flags = vec!["triage".into(), "review".into()];
    let server = TestServer::spawn(state);
    let acct = account("imap", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
        smtp: unused_smtp(),
    };
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fell-keyword-undo.sqlite");
    let id = MessageId::new("root@x");
    {
        let store = Store::open(&path).unwrap();
        sync_once(&acct, &backend, &store).unwrap();
        store
            .set_agent_grant(&acct.id, "bot", AgentGrant::enabled(false, true, false))
            .unwrap();
        let thread = store.message(&acct.id, &id).unwrap().unwrap().thread_id;
        store
            .add_thread_label(&acct.id, &thread, "personal notes", "agent:bot")
            .unwrap();
        store
            .remove_thread_label(&acct.id, &thread, "review", "agent:bot")
            .unwrap();
        store
            .label_thread(&acct.id, &thread, LabelChange::Remove("triage".into()))
            .unwrap();
        store
            .conn
            .execute_batch(
                "ALTER TABLE agent_action_messages DROP COLUMN label_was_keyword;
             ALTER TABLE local_action_messages DROP COLUMN label_was_keyword;",
            )
            .unwrap();
    }

    let store = Store::open(&path).unwrap();
    set_flags(&server, "INBOX", 2, &["fieldwork"]);
    sync_once(&acct, &backend, &store).unwrap();
    store.undo(&acct.id).unwrap();
    store.undo_actor(&acct.id, "agent:bot").unwrap();
    assert_eq!(
        store.message(&acct.id, &id).unwrap().unwrap().labels,
        ["personal notes", "fieldwork"]
    );
    store.undo_actor(&acct.id, "agent:bot").unwrap();
    assert_eq!(
        store.message(&acct.id, &id).unwrap().unwrap().labels,
        ["fieldwork"]
    );
    assert_flag_only(&server);
    drop(store);
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
fn reserved_imap_keywords_are_stored_without_becoming_labels() {
    let server = TestServer::spawn(keyword_state(false));
    set_flags(&server, "INBOX", 1, &["$Forwarded", "$Travel", "$Label1"]);
    let acct = account("imap", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
        smtp: unused_smtp(),
    };
    let store = Store::open_in_memory().unwrap();
    let stored_flags = || {
        store
            .conn
            .query_row("SELECT keywords FROM message_copies", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap()
    };
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(stored_flags(), "$Forwarded\u{1e}$Travel\u{1e}$Label1");
    assert_eq!(store.list_labels(&acct.id).unwrap(), ["$Label1", "$Travel"]);
    let thread = &store.folder_threads(&acct.id, "INBOX").unwrap()[0].thread_id;
    assert!(
        store
            .add_thread_label(&acct.id, thread, "$fOrWaRdEd", "agent")
            .is_err()
    );
    assert!(
        store
            .remove_thread_label(&acct.id, thread, "Junk", "agent")
            .is_err()
    );
    assert!(
        store
            .label_thread(&acct.id, thread, LabelChange::Add("$Junk".into()))
            .is_err()
    );

    set_flags(&server, "INBOX", 2, &["$MDNSent", "$Travel"]);
    sync_once(&acct, &backend, &store).unwrap();
    assert_flag_only(&server);
    assert_eq!(stored_flags(), "$MDNSent\u{1e}$Travel");
    let rows = store.folder_threads(&acct.id, "INBOX").unwrap();
    assert_eq!(rows[0].labels, ["$Travel"]);
    assert_eq!(store.list_labels(&acct.id).unwrap(), ["$Travel"]);
    assert!(
        store
            .search(&acct.id, "label:$MDNSent")
            .unwrap()
            .hits
            .is_empty()
    );
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
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("fell-keyword-migration.sqlite");
    {
        let store = Store::open(&path).unwrap();
        store.upsert_account(&acct).unwrap();
        let mut delta = backend.fetch_changes(None).unwrap();
        // The old backend sent initial labels on the message only.
        delta.flags.clear();
        delta.messages[0].labels.push("$Forwarded".into());
        store.apply_delta(&acct.id, &delta).unwrap();
        store
            .conn
            .execute_batch(
                "ALTER TABLE message_copies DROP COLUMN keywords;
                 DROP TABLE message_keyword_labels;
                 DROP TABLE legacy_imap_keywords;
                 DELETE FROM meta WHERE key = 'reserved_imap_labels_v1';",
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
    assert_eq!(message().labels, ["work"]);
    assert_eq!(
        store
            .conn
            .query_row("SELECT keywords FROM legacy_imap_keywords", [], |row| {
                row.get::<_, String>(0)
            })
            .unwrap(),
        "$Forwarded"
    );
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
}

#[test]
fn bridge_keeps_mailbox_labels_when_copy_flags_change() {
    let mut state = keyword_state(true);
    state
        .mailboxes
        .iter_mut()
        .find(|entry| entry.name == "Sent")
        .unwrap()
        .name = "Labels/$Forwarded".into();
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
    assert_eq!(before.folders, ["INBOX"]);
    assert!(before.labels.iter().any(|label| label == "$Forwarded"));

    set_flags(&server, "INBOX", 2, &["outside"]);
    sync_once(&acct, &backend, &store).unwrap();
    let after = store.message(&acct.id, &id).unwrap().unwrap();
    assert_eq!(after.folders, before.folders);
    assert_eq!(after.labels, before.labels);
    assert!(after.unread);
    assert_flag_only(&server);
    assert!(
        commands(&server)
            .iter()
            .filter(|line| line.contains("(UID FLAGS"))
            .all(|line| line.contains("CHANGEDSINCE"))
    );
    assert!(
        decode_cursor(store.sync_cursor(&acct.id).unwrap().as_ref())
            .iter()
            .all(|(_, cp)| !cp.keyword_baseline)
    );
}
