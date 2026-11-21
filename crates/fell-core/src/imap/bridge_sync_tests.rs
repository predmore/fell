use std::sync::atomic::Ordering;

use crate::backend::{BridgeBackend, ImapBackend, MailBackend};
use crate::store::{FolderSpan, ListFilter, ListSource, Store};
use crate::sync::sync_once;
use crate::{BackendKind, FolderRole, SearchField};

use super::sync_tests::{
    account, body_fetches, commands, config, empty_mailbox, sample_state, stored, unused_smtp,
};
use super::test_server::{FixtureMailbox, FixtureMessage, TestServer, TestState, raw_message};

fn message(uid: u32, id: &str, flags: &[&str]) -> FixtureMessage {
    let text = "A body large enough to measure first-sync protocol overhead.\r\n".repeat(800);
    FixtureMessage {
        uid,
        flags: flags.iter().map(ToString::to_string).collect(),
        modseq: 1,
        body_form: Default::default(),
        raw: raw_message(id, None, id, "Ann <ann@x>", &text, ""),
    }
}

fn mailbox(name: &str, validity: u32, messages: Vec<FixtureMessage>) -> FixtureMailbox {
    let mut mailbox = empty_mailbox(name, validity);
    mailbox.uidnext = Some(messages.last().map_or(1, |message| message.uid + 1));
    mailbox.messages = messages;
    mailbox
}

fn bridge_state() -> TestState {
    let mut state = sample_state();
    state.mailboxes = vec![
        mailbox("INBOX", 10, vec![message(1, "inbox@x", &[])]),
        mailbox("Labels/A", 11, vec![message(7, "inbox@x", &[])]),
        mailbox("Labels/B", 12, vec![message(9, "inbox@x", &[])]),
        mailbox("Starred", 13, vec![message(3, "inbox@x", &[])]),
        mailbox(
            "All Mail",
            14,
            vec![message(5, "inbox@x", &[]), message(8, "all-only@x", &[])],
        ),
        mailbox(
            "Folders/Projects/2026",
            15,
            vec![message(4, "projects@x", &[])],
        ),
    ];
    state
}

#[test]
fn bridge_projects_labels_and_stars_and_downloads_one_body_per_message() {
    let state = bridge_state();
    let unique_bytes = state.mailboxes[0].messages[0].raw.len()
        + state.mailboxes[4].messages[1].raw.len()
        + state.mailboxes[5].messages[0].raw.len();
    let server = TestServer::spawn(state);
    let acct = account("proton", BackendKind::Bridge);
    let backend = BridgeBackend {
        account_id: acct.id.clone(),
        imap: config(server.addr.port()),
        ..BridgeBackend::default()
    };
    let store = Store::open_in_memory().unwrap();
    let sync = || sync_once(&acct, &backend, &store).unwrap();
    sync();

    let inbox = || stored(&store, &acct, "inbox@x").unwrap();
    let inbox_thread = inbox().thread_id;
    assert_eq!(inbox().folders, ["INBOX"]);
    assert_eq!(inbox().labels, ["A", "B"]);
    assert!(inbox().starred);
    let folder = |name| {
        store
            .folder_page(&acct.id, name, &FolderSpan::Top { rows: 20 })
            .unwrap()
            .threads
    };
    let label = |name| {
        store
            .thread_list(
                &acct.id,
                ListSource::Label(name),
                &ListFilter::default(),
                None,
                20,
            )
            .unwrap()
            .threads
    };
    let inbox_rows = folder("INBOX");
    assert_eq!(inbox_rows.len(), 1);
    assert_eq!(inbox_rows[0].message_count, 1);
    assert_eq!(inbox_rows[0].labels, ["A", "B"]);
    assert!(inbox_rows[0].starred);
    assert!(folder("Labels/A").is_empty());
    let search = |query| store.search(&acct.id, query).unwrap().hits;
    assert_eq!(search("label:A").len(), 1);
    assert_eq!(search("in:Starred").len(), 1);
    assert_eq!(search("in:\"All Mail\"").len(), 2);
    for (prefix, name) in [
        ("pro", "Projects/2026"),
        ("all", "All Mail"),
        ("sta", "Starred"),
    ] {
        assert_eq!(
            store
                .search_suggestions(&acct.id, SearchField::Folder, prefix, 5)
                .unwrap(),
            [name]
        );
    }
    let project_query = "projects in:Projects/2026";
    let project_hits = search(project_query);
    assert_eq!(project_hits.len(), 1);
    assert_eq!(project_hits, search("projects in:Folders/Projects/2026"));
    let project = &project_hits[0];
    let locations = store
        .search_locations(&acct.id, project_query, "INBOX", &project_hits)
        .unwrap();
    assert_eq!(
        locations[&project.message_id].folder.as_deref(),
        Some("Folders/Projects/2026")
    );
    assert!(
        store
            .search_previews(
                &acct.id,
                project_query,
                std::slice::from_ref(&project.message_id)
            )
            .unwrap()
            .contains_key(&project.message_id)
    );
    let all_query = "in:\"All Mail\"";
    let hits = search(all_query);
    let locations = store
        .search_locations(&acct.id, all_query, "INBOX", &hits)
        .unwrap();
    assert!(
        hits.iter()
            .all(|hit| { locations[&hit.message_id].folder.as_deref() == Some("All Mail") })
    );
    let starred_hits = search("in:Starred");
    let locations = store
        .search_locations(&acct.id, "in:Starred", "INBOX", &starred_hits)
        .unwrap();
    let location = &locations[&starred_hits[0].message_id];
    assert_eq!(location.folder.as_deref(), Some("Starred"));
    assert!(location.starred);
    assert_eq!(label("A")[0].thread_id, inbox_thread);
    assert_eq!(label("B")[0].thread_id, inbox_thread);
    assert_eq!(folder("Starred")[0].thread_id, inbox_thread);
    let all_only = stored(&store, &acct, "all-only@x").unwrap();
    assert_eq!(folder("All Mail").len(), 2);
    assert!(
        folder("All Mail")
            .iter()
            .any(|row| row.thread_id == all_only.thread_id)
    );
    assert!(all_only.body_text.unwrap().contains("protocol overhead"));
    assert_eq!(folder("Folders/Projects/2026").len(), 1);
    assert_eq!(
        store.folder_names(&acct.id).unwrap(),
        ["INBOX", "Folders/Projects/2026"]
    );
    let roles = store.folder_roles(&acct.id).unwrap();
    assert_eq!(roles.label("Folders/Projects/2026"), "Projects/2026");
    assert_eq!(roles.name(FolderRole::Flagged), "Starred");
    assert_eq!(roles.name(FolderRole::All), "All Mail");
    assert!(
        commands(&server)
            .iter()
            .any(|command| command.contains("EXAMINE \"Folders/Projects/2026\""))
    );
    assert!(
        commands(&server)
            .iter()
            .filter(|command| command.contains("UID FETCH")
                && command.contains("HEADER.FIELDS (MESSAGE-ID X-PM-INTERNAL-ID)"))
            .count()
            >= 4
    );
    assert_eq!(server.body_responses.load(Ordering::Relaxed), 3);
    let bytes = server.sent.load(Ordering::Relaxed);
    eprintln!("Bridge first sync sent {bytes} bytes for {unique_bytes} unique message bytes");
    assert!(
        bytes * 10 <= unique_bytes * 11,
        "{bytes} wire bytes for {unique_bytes} unique message bytes"
    );

    {
        let mut state = server.state.lock().unwrap();
        state.commands.clear();
        state.mailboxes[1].messages.clear();
        state.mailboxes[3].messages.clear();
        state.mailboxes[0].messages[0]
            .flags
            .push("\\Flagged".into());
        // A new cross-mailbox UID for a body already stored.
        state.mailboxes[2]
            .messages
            .push(message(10, "projects@x", &[]));
        state.mailboxes[2].uidnext = Some(11);
    }
    sync();
    assert_eq!(inbox().labels, ["B"]);
    assert!(label("A").is_empty());
    assert!(search("label:A").is_empty());
    assert_eq!(label("B").len(), 2);
    assert!(inbox().starred);
    assert_eq!(folder("Starred")[0].thread_id, inbox_thread);
    assert!(body_fetches(&server).is_empty());

    {
        let mut state = server.state.lock().unwrap();
        state.mailboxes[0].messages[0].flags.clear();
        state.mailboxes.retain(|mailbox| mailbox.name != "Labels/B");
    }
    sync();
    assert!(!inbox().starred);
    assert!(inbox().labels.is_empty());
    assert!(label("B").is_empty());
    assert!(folder("Starred").is_empty());
    assert!(search("in:Starred").is_empty());
    assert!(body_fetches(&server).is_empty());

    // If the ordinary copy goes away, its All Mail copy keeps it accessible.
    server.state.lock().unwrap().mailboxes[0].messages.clear();
    sync();
    assert!(folder("INBOX").is_empty());
    assert!(inbox().folders.is_empty());
    assert_eq!(folder("All Mail").len(), 2);
    assert_eq!(server.body_responses.load(Ordering::Relaxed), 3);
    server
        .state
        .lock()
        .unwrap()
        .mailboxes
        .iter_mut()
        .find(|mailbox| mailbox.name == "All Mail")
        .unwrap()
        .messages
        .remove(0);
    sync();
    assert!(stored(&store, &acct, "inbox@x").is_none());
    assert_eq!(folder("All Mail").len(), 1);
}

#[test]
fn bridge_upgrades_flattened_folders_and_existing_flag_observations() {
    let mut state = bridge_state();
    state.condstore = true;
    state.mailboxes[0].messages[0]
        .flags
        .push("\\Flagged".into());
    // With no Starred copy, the upgrade must read the existing Inbox flag.
    state.mailboxes.retain(|mailbox| mailbox.name != "Starred");
    let server = TestServer::spawn(state);
    let acct = account("upgrade", BackendKind::Bridge);
    let imap = config(server.addr.port());
    // The plain backend produces the same flattened folders as old Bridge
    // sync. Start with valid UID checkpoints and bodies already on disk.
    let old = ImapBackend {
        account_id: acct.id.clone(),
        config: imap.clone(),
        smtp: unused_smtp(),
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &old, &store).unwrap();
    let inbox = || stored(&store, &acct, "inbox@x").unwrap();
    assert!(inbox().folders.contains(&"Labels/A".into()));
    assert!(inbox().folders.contains(&"All Mail".into()));
    store
        .conn
        .execute("UPDATE messages SET starred = 0", [])
        .unwrap();
    {
        let mut state = server.state.lock().unwrap();
        state.commands.clear();
        state.mailboxes.retain(|mailbox| mailbox.name != "Labels/B");
    }
    let bridge = BridgeBackend {
        account_id: acct.id.clone(),
        imap,
        ..BridgeBackend::default()
    };
    sync_once(&acct, &bridge, &store).unwrap();
    assert_eq!(inbox().folders, ["INBOX"]);
    assert_eq!(inbox().labels, ["A"]);
    assert!(inbox().starred);
    assert_eq!(store.folder_threads(&acct.id, "All Mail").unwrap().len(), 2);
    assert!(body_fetches(&server).is_empty());
}

#[test]
fn generic_special_use_all_reuses_bodies_and_keeps_ordinary_names_and_keywords() {
    let mut state = sample_state();
    state.condstore = true;
    state.special_use = true;
    state.mailboxes = vec![
        mailbox("INBOX", 1, vec![message(1, "inbox@x", &["work"])]),
        mailbox(
            "Everything",
            2,
            vec![
                message(5, "inbox@x", &["\\Flagged"]),
                message(6, "all-only@x", &[]),
            ],
        ),
        mailbox("Labels/Plain", 3, vec![message(2, "ordinary@x", &[])]),
        mailbox(
            "Folders/Plain",
            4,
            vec![message(1, "ordinary-folder@x", &[])],
        ),
    ];
    state.mailboxes[1].list_flags = "\\All".into();
    let server = TestServer::spawn(state);
    let acct = account("generic", BackendKind::Imap);
    let backend = ImapBackend {
        account_id: acct.id.clone(),
        config: config(server.addr.port()),
        smtp: unused_smtp(),
    };
    let store = Store::open_in_memory().unwrap();
    // The one-shot backend entry point also deduplicates virtual copies.
    let delta = backend.fetch_changes(None).unwrap();
    store.upsert_account(&acct).unwrap();
    store.apply_delta(&acct.id, &delta).unwrap();
    assert_eq!(server.body_responses.load(Ordering::Relaxed), 4);
    let inbox = || stored(&store, &acct, "inbox@x").unwrap();
    assert_eq!(inbox().folders, ["INBOX"]);
    assert_eq!(inbox().labels, ["work"]);
    // Generic IMAP still keeps the observed Flagged value for its existing
    // write-back path. The displayed star import is Bridge-specific.
    assert!(!inbox().starred);
    assert_eq!(
        store.folder_threads(&acct.id, "Everything").unwrap().len(),
        2
    );
    assert_eq!(
        store
            .folder_threads(&acct.id, "Labels/Plain")
            .unwrap()
            .len(),
        1
    );
    let roles = store.folder_roles(&acct.id).unwrap();
    assert_eq!(roles.label("Folders/Plain"), "Folders/Plain");
    assert_eq!(roles.get(FolderRole::All).unwrap().raw, "Everything");
    {
        let mut state = server.state.lock().unwrap();
        state.commands.clear();
        let copy = &mut state.mailboxes[1].messages[0];
        copy.flags.clear();
        copy.modseq += 1;
    }
    sync_once(&acct, &backend, &store).unwrap();
    assert!(!inbox().starred);
    assert_eq!(inbox().labels, ["work"]);
    assert!(body_fetches(&server).is_empty());
}

#[test]
fn bridge_links_copies_without_a_message_id_before_downloading_a_body() {
    let without_id = |uid| FixtureMessage {
        raw: b"From: Ann <ann@x>\r\nSubject: Message without an RFC ID\r\n\
               X-Pm-Internal-Id: aBc_42-XY=\r\nContent-Type: text/plain\r\n\r\nBridge body"
            .to_vec(),
        ..message(uid, "unused", &[])
    };
    let mut state = sample_state();
    state.mailboxes = vec![
        mailbox("INBOX", 10, vec![without_id(2)]),
        mailbox("All Mail", 11, vec![without_id(7)]),
        mailbox("Labels/A", 12, vec![without_id(5)]),
        mailbox("Starred", 13, vec![without_id(9)]),
    ];
    let server = TestServer::spawn(state);
    let acct = account("proton", BackendKind::Bridge);
    let backend = BridgeBackend {
        account_id: acct.id.clone(),
        imap: config(server.addr.port()),
        ..BridgeBackend::default()
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();

    let count: i64 = store
        .connection()
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    assert_eq!(server.body_responses.load(Ordering::Relaxed), 1);
    let mail = stored(&store, &acct, "bridge.aBc_42-XY=@imap.fell").unwrap();
    assert_eq!(mail.body_text.as_deref(), Some("Bridge body"));
    assert_eq!(mail.labels, ["A"]);
    assert!(mail.starred);
    for folder in ["INBOX", "All Mail", "Starred"] {
        let page = store
            .folder_page(&acct.id, folder, &FolderSpan::Top { rows: 2 })
            .unwrap();
        assert_eq!(page.threads.len(), 1);
        assert_eq!(page.threads[0].thread_id, mail.thread_id);
    }

    // The same Bridge message can also reach a real folder on a later pass.
    let mut state = server.state.lock().unwrap();
    state.commands.clear();
    state.mailboxes.push(mailbox("Folders/Trips", 14, vec![]));
    drop(state);
    sync_once(&acct, &backend, &store).unwrap();
    let mut state = server.state.lock().unwrap();
    state.mailboxes[4].messages.push(without_id(4));
    state.mailboxes[4].uidnext = Some(5);
    drop(state);
    sync_once(&acct, &backend, &store).unwrap();
    let mail = stored(&store, &acct, "bridge.aBc_42-XY=@imap.fell").unwrap();
    assert_eq!(mail.folders, ["INBOX", "Folders/Trips"]);
    assert_eq!(server.body_responses.load(Ordering::Relaxed), 1);
}

#[test]
fn bridge_views_are_not_move_targets_and_actions_from_them_move_real_copies() {
    let mut state = bridge_state();
    state.mailboxes[4]
        .messages
        .push(message(9, "projects@x", &[]));
    state.mailboxes[4].uidnext = Some(10);
    state.mailboxes.extend([
        mailbox("Archive", 16, vec![]),
        mailbox("Trash", 17, vec![]),
        mailbox("Spam", 18, vec![]),
    ]);
    let server = TestServer::spawn(state);
    {
        let mut writes = server.writes.lock().unwrap();
        writes.moves = true;
        writes.uidplus = true;
    }
    let acct = account("proton", BackendKind::Bridge);
    let backend = BridgeBackend {
        account_id: acct.id.clone(),
        imap: config(server.addr.port()),
        ..BridgeBackend::default()
    };
    let store = Store::open_in_memory().unwrap();
    sync_once(&acct, &backend, &store).unwrap();
    store.set_write_back(&acct.id, true).unwrap();

    let inbox = stored(&store, &acct, "inbox@x").unwrap();
    let project = stored(&store, &acct, "projects@x").unwrap();
    for view in ["All Mail", "Starred", "Labels/A"] {
        assert!(
            store
                .move_thread(&acct.id, &inbox.thread_id, "INBOX", view)
                .is_err()
        );
    }
    assert_eq!(stored(&store, &acct, "inbox@x").unwrap().folders, ["INBOX"]);
    assert!(store.outbox(&acct.id).unwrap().is_empty());

    store
        .archive_thread(&acct.id, &inbox.thread_id, "All Mail")
        .unwrap();
    store
        .delete_threads(&acct.id, std::slice::from_ref(&inbox.thread_id), "Starred")
        .unwrap();
    let query = "in:\"All Mail\"";
    let project_hit = store
        .search(&acct.id, query)
        .unwrap()
        .hits
        .into_iter()
        .find(|hit| hit.message_id.as_str() == "projects@x")
        .unwrap();
    let locations = store
        .search_locations(&acct.id, query, "INBOX", std::slice::from_ref(&project_hit))
        .unwrap();
    let folder = locations[&project_hit.message_id].folder.clone().unwrap();
    store
        .spam_scoped_threads(&acct.id, &[(project.thread_id, folder)])
        .unwrap();

    let query = "in:Starred";
    let hits = store.search(&acct.id, query).unwrap().hits;
    let locations = store
        .search_locations(&acct.id, query, "INBOX", &hits)
        .unwrap();
    let folder = locations[&hits[0].message_id].folder.clone().unwrap();
    // Already in Trash, so a deletion from the same view changes nothing.
    assert!(
        store
            .delete_scoped_threads(&acct.id, &[(inbox.thread_id, folder)])
            .is_err()
    );

    assert_eq!(stored(&store, &acct, "inbox@x").unwrap().folders, ["Trash"]);
    assert_eq!(
        stored(&store, &acct, "projects@x").unwrap().folders,
        ["Spam"]
    );
    let report = super::replay_outbox(&backend.imap, &acct.id, &store).unwrap();
    assert_eq!(report.sent, 3);
    let commands = commands(&server);
    let moves: Vec<_> = commands
        .iter()
        .filter(|cmd| cmd.contains("UID MOVE"))
        .collect();
    assert_eq!(moves.len(), 3);
    for target in ["Archive", "Trash", "Spam"] {
        assert!(
            moves
                .iter()
                .any(|cmd| cmd.ends_with(&format!("\"{target}\""))),
            "{moves:?}"
        );
    }
    assert!(!commands.iter().any(|cmd| {
        ["All Mail", "Starred", "Labels/A"]
            .iter()
            .any(|name| cmd.contains(&format!("SELECT \"{name}\"")))
    }));
}
