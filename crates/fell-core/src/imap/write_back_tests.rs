//! Write-back against the in-process IMAP server: the outbox rows archive
//! and move queue, what replay sends for them, and undo.

use crate::account::{Account, AccountId, BackendKind};
use crate::backend::ImapBackend;
use crate::outbox::{DropReason, OutboxState};
use crate::store::Store;
use crate::sync::sync_once;
use crate::threading::ThreadId;

use super::test_server::{
    BodyForm, FixtureMailbox, FixtureMessage, TestServer, TestState, raw_message,
};
use super::{ImapConfig, ImapSecurity, replay_outbox};

fn mailbox(name: &str, flags: &str, uidvalidity: u32, messages: &[(u32, &str)]) -> FixtureMailbox {
    FixtureMailbox {
        name: name.into(),
        list_flags: flags.into(),
        uidvalidity,
        uidnext: None,
        omit_uidnext: false,
        reject_examine: false,
        reject_fetch: false,
        vanished: Vec::new(),
        messages: messages
            .iter()
            .map(|(uid, subject)| FixtureMessage {
                uid: *uid,
                flags: vec!["\\Seen".into()],
                modseq: 1,
                body_form: BodyForm::Literal,
                raw: raw_message(
                    &format!("{}@x", subject.to_ascii_lowercase()),
                    None,
                    subject,
                    "Ann <ann@x>",
                    &format!("{subject} body"),
                    "",
                ),
            })
            .collect(),
    }
}

/// INBOX with Alpha (UID 1) and Bravo (UID 2); Archive and Work are empty.
fn state() -> TestState {
    state_with(vec![
        mailbox("INBOX", "", 7, &[(1, "Alpha"), (2, "Bravo")]),
        mailbox("Archive", "\\Archive", 30, &[]),
        mailbox("Work", "", 40, &[]),
    ])
}

fn state_with(mailboxes: Vec<FixtureMailbox>) -> TestState {
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
        special_use: true,
        reject_list_return: false,
        mailboxes,
    }
}

struct Fixture {
    server: TestServer,
    account: Account,
    backend: ImapBackend,
    store: Store,
}

impl Fixture {
    /// Synced once, with write-back on and a server that offers MOVE and
    /// UIDPLUS.
    fn new() -> Self {
        Self::with_store(Store::open_in_memory().unwrap())
    }

    fn with_store(store: Store) -> Self {
        Self::with(store, state())
    }

    fn with(store: Store, state: TestState) -> Self {
        let server = TestServer::spawn(state);
        {
            let mut writes = server.writes.lock().unwrap();
            writes.moves = true;
            writes.uidplus = true;
        }
        let account = Account {
            id: AccountId("acct".into()),
            display_name: "Caleb".into(),
            email: "c@x".into(),
            backend_kind: BackendKind::Imap,
        };
        let backend = ImapBackend {
            account_id: account.id.clone(),
            config: config(server.addr.port()),
        };
        sync_once(&account, &backend, &store).unwrap();
        store.set_write_back(&account.id, true).unwrap();
        Self {
            server,
            account,
            backend,
            store,
        }
    }

    fn thread(&self, folder: &str, subject: &str) -> ThreadId {
        self.store
            .threads_in_folder(&self.account.id, folder)
            .unwrap()
            .into_iter()
            .find(|thread| thread.subject == subject)
            .unwrap_or_else(|| panic!("{subject} is not in {folder}"))
            .thread_id
    }

    fn subjects(&self, folder: &str) -> Vec<String> {
        let mut subjects: Vec<String> = self
            .store
            .threads_in_folder(&self.account.id, folder)
            .unwrap()
            .into_iter()
            .map(|thread| thread.subject)
            .collect();
        subjects.sort();
        subjects
    }

    fn replay(&self) -> crate::outbox::ReplayReport {
        replay_outbox(&self.backend.config, &self.account.id, &self.store).unwrap()
    }

    fn sync(&self) {
        sync_once(&self.account, &self.backend, &self.store).unwrap();
    }

    /// `(uid, subject)` of each message in a server mailbox.
    fn on_server(&self, name: &str) -> Vec<(u32, String)> {
        let state = self.server.state.lock().unwrap();
        let mailbox = state.mailboxes.iter().find(|m| m.name == name).unwrap();
        mailbox
            .messages
            .iter()
            .map(|message| {
                let raw = String::from_utf8_lossy(&message.raw).into_owned();
                let subject = raw
                    .lines()
                    .find_map(|line| line.strip_prefix("Subject: "))
                    .unwrap_or("")
                    .to_string();
                (message.uid, subject)
            })
            .collect()
    }

    fn commands(&self) -> Vec<String> {
        self.server.state.lock().unwrap().commands.clone()
    }

    fn clear_commands(&self) {
        self.server.state.lock().unwrap().commands.clear();
    }

    /// Commands that change the server.
    fn writes(&self) -> Vec<String> {
        self.commands()
            .into_iter()
            .filter(|command| {
                let upper = command.to_ascii_uppercase();
                [" SELECT ", " MOVE ", " COPY ", " STORE ", " EXPUNGE"]
                    .iter()
                    .any(|verb| upper.contains(verb))
            })
            .collect()
    }

    /// Flags of the message at `uid` in a server mailbox.
    fn flags(&self, name: &str, uid: u32) -> Vec<String> {
        let state = self.server.state.lock().unwrap();
        let mailbox = state.mailboxes.iter().find(|m| m.name == name).unwrap();
        mailbox
            .messages
            .iter()
            .find(|message| message.uid == uid)
            .map(|message| message.flags.clone())
            .unwrap_or_default()
    }

    /// Make every waiting row due now.
    fn make_due(&self) {
        self.store
            .connection()
            .execute("UPDATE server_outbox SET retry_unix = 0", [])
            .unwrap();
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

fn without_tags(commands: &[String]) -> Vec<String> {
    commands
        .iter()
        .map(|command| {
            command
                .split_once(' ')
                .map_or(command.as_str(), |(_, rest)| rest)
                .to_string()
        })
        .collect()
}

#[test]
fn archive_moves_the_right_uid_and_undo_moves_it_back() {
    let fx = Fixture::new();
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha"]);

    let queued = fx.store.outbox(&fx.account.id).unwrap();
    assert_eq!(queued.len(), 1);
    let row = &queued[0];
    assert_eq!(row.state, OutboxState::Pending);
    assert_eq!(row.source_mailbox.as_deref(), Some("INBOX"));
    assert_eq!((row.uidvalidity, row.uid), (Some(7), Some(2)));
    assert_eq!(row.target_folder, "Archive");
    assert_eq!(row.target_mailbox.as_deref(), Some("Archive"));

    fx.clear_commands();
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (1, 0));
    assert!(report.dropped.is_empty());
    assert_eq!(
        without_tags(&fx.writes()),
        vec!["SELECT \"INBOX\"", "UID MOVE 2 \"Archive\""]
    );
    assert_eq!(fx.on_server("INBOX"), vec![(1, "Alpha".to_string())]);
    assert_eq!(fx.on_server("Archive"), vec![(1, "Bravo".to_string())]);
    let sent = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!(sent.state, OutboxState::Done);
    assert_eq!(
        (sent.placed_uidvalidity, sent.placed_uid),
        (Some(30), Some(1))
    );

    // A sync pass after the move keeps the row archived.
    fx.sync();
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha"]);
    assert_eq!(fx.subjects("Archive"), vec!["Bravo"]);

    // Undo puts it back locally at once and queues the reverse move.
    fx.store.undo(&fx.account.id).unwrap();
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);
    let rows = fx.store.outbox(&fx.account.id).unwrap();
    assert_eq!(rows.len(), 2);
    let reverse = &rows[1];
    assert_eq!(reverse.action_id, None);
    assert_eq!(reverse.source_mailbox.as_deref(), Some("Archive"));
    assert_eq!((reverse.uidvalidity, reverse.uid), (Some(30), Some(1)));
    assert_eq!(reverse.target_mailbox.as_deref(), Some("INBOX"));

    fx.clear_commands();
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(
        without_tags(&fx.writes()),
        vec!["SELECT \"Archive\"", "UID MOVE 1 \"INBOX\""]
    );
    assert_eq!(
        fx.on_server("INBOX"),
        vec![(1, "Alpha".to_string()), (3, "Bravo".to_string())]
    );
    assert!(fx.on_server("Archive").is_empty());
    fx.sync();
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);
    assert!(fx.subjects("Archive").is_empty());

    // Archiving it again finds the copy where the reverse move put it.
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    let again = fx.store.outbox(&fx.account.id).unwrap().pop().unwrap();
    assert_eq!((again.uidvalidity, again.uid), (Some(7), Some(3)));
}

#[test]
fn undo_before_replay_sends_nothing() {
    let fx = Fixture::new();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    assert_eq!(fx.store.outbox(&fx.account.id).unwrap().len(), 1);
    fx.store.undo(&fx.account.id).unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());

    fx.clear_commands();
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (0, 0));
    assert!(fx.commands().is_empty(), "{:?}", fx.commands());
    assert_eq!(fx.on_server("INBOX").len(), 2);
}

#[test]
fn a_restart_mid_queue_replays_each_move_once() {
    let path = std::env::temp_dir().join(format!(
        "fell-outbox-{}-{}.db",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let fx = Fixture::with_store(Store::open(&path).unwrap());
    let alpha = fx.thread("INBOX", "Alpha");
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.store
        .move_thread(&fx.account.id, &bravo, "INBOX", "Work")
        .unwrap();

    // The connection drops right after the first move.
    fx.server.writes.lock().unwrap().close_after_moves = Some(1);
    fx.clear_commands();
    let report = fx.replay();
    assert_eq!(report.sent, 1);
    assert_eq!(report.waiting, 1);
    assert!(report.interrupted.is_some());

    // A new process opens the same mailbox file and replays what is left.
    let Fixture {
        server,
        account,
        backend,
        store,
    } = fx;
    drop(store);
    let store = Store::open(&path).unwrap();
    let fx = Fixture {
        server,
        account,
        backend,
        store,
    };
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(fx.replay().sent, 0);

    let moves: Vec<String> = without_tags(&fx.commands())
        .into_iter()
        .filter(|command| command.starts_with("UID MOVE"))
        .collect();
    assert_eq!(moves, vec!["UID MOVE 1 \"Archive\"", "UID MOVE 2 \"Work\""]);
    assert!(fx.on_server("INBOX").is_empty());
    assert_eq!(fx.on_server("Archive"), vec![(1, "Alpha".to_string())]);
    assert_eq!(fx.on_server("Work"), vec![(1, "Bravo".to_string())]);
    drop(fx);
    for suffix in ["", "-wal", "-shm", ".lock"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
}

#[test]
fn a_uidvalidity_change_drops_the_move() {
    let fx = Fixture::new();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    {
        // The server rebuilt INBOX: UID 1 is another message now.
        let mut state = fx.server.state.lock().unwrap();
        let inbox = state
            .mailboxes
            .iter_mut()
            .find(|m| m.name == "INBOX")
            .unwrap();
        inbox.uidvalidity = 8;
        inbox.messages.swap(0, 1);
        inbox.messages[0].uid = 1;
        inbox.messages[1].uid = 2;
    }
    fx.clear_commands();
    let report = fx.replay();
    assert_eq!(report.sent, 0);
    assert_eq!(report.dropped.len(), 1);
    assert_eq!(report.dropped[0].reason, DropReason::FolderReset);
    assert_eq!(report.dropped[0].source_folder, "INBOX");
    assert_eq!(without_tags(&fx.writes()), vec!["SELECT \"INBOX\""]);
    assert_eq!(fx.on_server("INBOX").len(), 2);
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!(row.state, OutboxState::Dropped);
    // The local archive stays; nothing is sent again.
    assert_eq!(fx.subjects("Archive"), vec!["Alpha"]);
    fx.clear_commands();
    assert_eq!(fx.replay(), Default::default());
    assert!(fx.commands().is_empty());
}

#[test]
fn without_move_it_copies_then_expunges_only_those_uids() {
    let fx = Fixture::new();
    fx.server.writes.lock().unwrap().moves = false;
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    fx.clear_commands();
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(
        without_tags(&fx.writes()),
        vec![
            "SELECT \"INBOX\"",
            "UID COPY 2 \"Archive\"",
            "UID STORE 2 +FLAGS.SILENT (\\Deleted)",
            "UID EXPUNGE 2",
        ]
    );
    assert_eq!(fx.on_server("INBOX"), vec![(1, "Alpha".to_string())]);
    assert_eq!(fx.on_server("Archive"), vec![(1, "Bravo".to_string())]);
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!((row.state, row.placed_uid), (OutboxState::Done, Some(1)));
}

#[test]
fn a_server_without_move_or_uidplus_keeps_the_change_local() {
    let fx = Fixture::new();
    {
        let mut writes = fx.server.writes.lock().unwrap();
        writes.moves = false;
        writes.uidplus = false;
    }
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.clear_commands();
    let report = fx.replay();
    assert_eq!(report.dropped.len(), 1);
    assert_eq!(report.dropped[0].reason, DropReason::NoMove);
    assert!(fx.writes().is_empty(), "{:?}", fx.writes());
    assert_eq!(fx.on_server("INBOX").len(), 2);
}

#[test]
fn a_refused_move_backs_off_then_drops() {
    let fx = Fixture::new();
    fx.server.writes.lock().unwrap().refuse = Some("[CANNOT] not now".into());
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();

    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (0, 1));
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!((row.state, row.attempts), (OutboxState::Pending, 1));
    assert!(row.retry_unix > crate::outbox::now_unix());
    assert!(row.error.contains("not now"), "{}", row.error);

    // Not due yet: no connection at all.
    fx.clear_commands();
    assert_eq!(fx.replay().waiting, 1);
    assert!(fx.commands().is_empty());

    let mut dropped = Vec::new();
    for _ in 1..super::write_back::MAX_ATTEMPTS {
        fx.make_due();
        dropped = fx.replay().dropped;
    }
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].reason, DropReason::Refused);
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!(row.state, OutboxState::Dropped);
    assert_eq!(fx.on_server("INBOX").len(), 2);
}

#[test]
fn a_second_move_before_the_first_was_sent_follows_it() {
    let fx = Fixture::new();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.store
        .move_thread(&fx.account.id, &alpha, "Archive", "Work")
        .unwrap();
    let rows = fx.store.outbox(&fx.account.id).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].uid, None, "the Archive copy does not exist yet");

    fx.clear_commands();
    assert_eq!(fx.replay().sent, 2);
    assert_eq!(
        without_tags(&fx.writes()),
        vec![
            "SELECT \"INBOX\"",
            "UID MOVE 1 \"Archive\"",
            "SELECT \"Archive\"",
            "UID MOVE 1 \"Work\"",
        ]
    );
    assert_eq!(fx.on_server("Work"), vec![(1, "Alpha".to_string())]);
    assert!(fx.on_server("Archive").is_empty());

    // Undo the second move only: the message goes back to Archive.
    fx.store.undo(&fx.account.id).unwrap();
    fx.clear_commands();
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(
        without_tags(&fx.writes()),
        vec!["SELECT \"Work\"", "UID MOVE 1 \"Archive\""]
    );
    assert_eq!(fx.on_server("Archive"), vec![(2, "Alpha".to_string())]);
}

#[test]
fn write_back_off_queues_nothing_and_turning_it_off_clears_the_queue() {
    let fx = Fixture::new();
    fx.store.set_write_back(&fx.account.id, false).unwrap();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());

    fx.store.set_write_back(&fx.account.id, true).unwrap();
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    assert_eq!(fx.store.outbox(&fx.account.id).unwrap().len(), 1);
    fx.store.set_write_back(&fx.account.id, false).unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
    fx.clear_commands();
    assert_eq!(fx.replay(), Default::default());
    assert!(fx.commands().is_empty());
}

#[test]
fn labels_and_delete_stay_local() {
    let fx = Fixture::new();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .label_thread(
            &fx.account.id,
            &alpha,
            crate::LabelChange::Add("work".into()),
        )
        .unwrap();
    fx.store
        .delete_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
    fx.store.undo(&fx.account.id).unwrap();
    fx.store.undo(&fx.account.id).unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
}

#[test]
fn sync_passes_still_send_no_writes() {
    let fx = Fixture::new();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.clear_commands();
    fx.sync();
    assert!(fx.writes().is_empty(), "{:?}", fx.writes());
}

#[test]
fn undo_while_a_move_is_on_the_wire_queues_the_reverse() {
    let fx = Fixture::new();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    let sending = fx.store.outbox(&fx.account.id).unwrap().remove(0);

    // Undo lands after replay read the row but before the server answered.
    fx.store.undo(&fx.account.id).unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
    crate::outbox::settle(
        &fx.store,
        &fx.account.id,
        &sending,
        Some(("INBOX", 7, 1)),
        Some("Archive"),
        crate::outbox::Outcome::Done {
            placed: Some((30, 1)),
        },
    )
    .unwrap();

    let rows = fx.store.outbox(&fx.account.id).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        (rows[0].state, rows[0].action_id),
        (OutboxState::Done, None)
    );
    let reverse = &rows[1];
    assert_eq!(reverse.state, OutboxState::Pending);
    assert_eq!(reverse.source_mailbox.as_deref(), Some("Archive"));
    assert_eq!((reverse.uidvalidity, reverse.uid), (Some(30), Some(1)));
    assert_eq!(reverse.target_mailbox.as_deref(), Some("INBOX"));
}

#[test]
fn a_half_done_copy_survives_neither_the_off_switch_nor_a_later_on() {
    let fx = Fixture::new();
    fx.server.writes.lock().unwrap().moves = false;
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    // The server copies and flags the source \Deleted, and the connection
    // drops before UID EXPUNGE.
    fx.server.writes.lock().unwrap().lose_reply_to = Some("STORE");
    let report = fx.replay();
    assert!(report.interrupted.is_some());
    let half_done = fx.store.outbox(&fx.account.id).unwrap().remove(0);
    assert_eq!(half_done.state, OutboxState::Copied);
    assert!(fx.flags("INBOX", 2).contains(&"\\Deleted".to_string()));

    // Off: the row is given up, and reported once.
    fx.store.set_write_back(&fx.account.id, false).unwrap();
    let abandoned = crate::outbox::take_abandoned(&fx.store, &fx.account.id).unwrap();
    assert_eq!(abandoned.len(), 1);
    assert_eq!(abandoned[0].reason, DropReason::Abandoned);
    assert_eq!(abandoned[0].source_folder, "INBOX");
    assert!(
        crate::outbox::take_abandoned(&fx.store, &fx.account.id)
            .unwrap()
            .is_empty()
    );
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!(row.state, OutboxState::Dropped);

    // On again: nothing from before is sent, so UID 2 is never expunged.
    fx.store.set_write_back(&fx.account.id, true).unwrap();
    fx.make_due();
    fx.clear_commands();
    assert_eq!(fx.replay(), Default::default());
    assert!(fx.commands().is_empty(), "{:?}", fx.commands());
    assert_eq!(fx.on_server("INBOX").len(), 2);

    // A reply from before the off switch, answered late, changes nothing.
    let live = crate::outbox::settle(
        &fx.store,
        &fx.account.id,
        &half_done,
        Some(("INBOX", 7, 2)),
        Some("Archive"),
        crate::outbox::Outcome::Done {
            placed: Some((30, 1)),
        },
    )
    .unwrap();
    assert!(!live);
    assert_eq!(
        fx.store.outbox(&fx.account.id).unwrap()[0].state,
        OutboxState::Dropped
    );
    // Undo of that archive is local only: no reverse of the old row.
    fx.store.undo(&fx.account.id).unwrap();
    assert_eq!(fx.store.outbox(&fx.account.id).unwrap().len(), 1);

    // A new action still goes out, and expunges only its own UID.
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.clear_commands();
    assert_eq!(fx.replay().sent, 1);
    let writes = without_tags(&fx.writes());
    assert!(writes.contains(&"UID EXPUNGE 1".to_string()), "{writes:?}");
    assert!(
        !writes.iter().any(|command| command.contains(" 2")),
        "{writes:?}"
    );
    assert_eq!(fx.on_server("INBOX"), vec![(2, "Bravo".to_string())]);
}

#[test]
fn a_move_the_server_applied_without_a_reply_records_its_new_uid() {
    let fx = Fixture::new();
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    fx.server.writes.lock().unwrap().lose_reply_to = Some("MOVE");
    let report = fx.replay();
    assert!(report.interrupted.is_some());
    assert_eq!(report.sent, 0);
    assert_eq!(fx.on_server("Archive"), vec![(1, "Bravo".to_string())]);
    // A sync pass may run before the next try.
    fx.sync();

    // The retry's UID MOVE finds nothing to move, so no COPYUID: the copy
    // is found in Archive by its Message-ID instead.
    fx.clear_commands();
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(
        without_tags(&fx.writes()),
        vec![
            "SELECT \"INBOX\"",
            "UID MOVE 2 \"Archive\"",
            "SELECT \"Archive\"",
        ]
    );
    assert!(
        without_tags(&fx.commands()).contains(&"UID SEARCH HEADER Message-ID \"<bravo@x>\"".into())
    );
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!(row.state, OutboxState::Done);
    assert_eq!(
        (row.placed_uidvalidity, row.placed_uid),
        (Some(30), Some(1))
    );

    // Undo moves that copy back.
    fx.store.undo(&fx.account.id).unwrap();
    fx.clear_commands();
    assert_eq!(fx.replay().sent, 1);
    assert!(
        without_tags(&fx.writes()).contains(&"UID MOVE 1 \"INBOX\"".to_string()),
        "{:?}",
        fx.writes()
    );
    assert!(fx.on_server("Archive").is_empty());
    assert_eq!(fx.on_server("INBOX").len(), 2);
}

#[test]
fn a_move_whose_message_is_in_neither_folder_waits() {
    let fx = Fixture::new();
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    // Another client deleted the message: the move finds nothing, and
    // nothing is in Archive either.
    fx.server.state.lock().unwrap().mailboxes[0]
        .messages
        .retain(|message| message.uid != 2);
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (0, 1));
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!((row.state, row.attempts), (OutboxState::Pending, 1));
}

#[test]
fn a_lost_copy_reply_does_not_copy_twice() {
    let fx = Fixture::new();
    fx.server.writes.lock().unwrap().moves = false;
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    fx.server.writes.lock().unwrap().lose_reply_to = Some("COPY");
    assert!(fx.replay().interrupted.is_some());
    assert_eq!(fx.on_server("Archive"), vec![(1, "Bravo".to_string())]);

    fx.clear_commands();
    assert_eq!(fx.replay().sent, 1);
    let writes = without_tags(&fx.writes());
    assert!(
        !writes.iter().any(|command| command.starts_with("UID COPY")),
        "{writes:?}"
    );
    assert_eq!(
        writes,
        vec![
            "SELECT \"INBOX\"",
            "SELECT \"Archive\"",
            "SELECT \"INBOX\"",
            "UID STORE 2 +FLAGS.SILENT (\\Deleted)",
            "UID EXPUNGE 2",
        ]
    );
    assert_eq!(fx.on_server("Archive"), vec![(1, "Bravo".to_string())]);
    assert_eq!(fx.on_server("INBOX"), vec![(1, "Alpha".to_string())]);
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!((row.state, row.placed_uid), (OutboxState::Done, Some(1)));
}

#[test]
fn a_second_move_follows_the_copy_the_first_places_not_one_already_there() {
    // Archive already holds another copy of Bravo.
    let fx = Fixture::with(
        Store::open_in_memory().unwrap(),
        state_with(vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha"), (2, "Bravo")]),
            mailbox("Archive", "\\Archive", 30, &[(1, "Bravo")]),
            mailbox("Work", "", 40, &[]),
        ]),
    );
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    fx.store
        .move_thread(&fx.account.id, &bravo, "Archive", "Work")
        .unwrap();

    fx.clear_commands();
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (3, 0), "{report:?}");
    assert_eq!(
        without_tags(&fx.writes()),
        vec![
            "SELECT \"INBOX\"",
            "UID MOVE 2 \"Archive\"",
            "SELECT \"Archive\"",
            "UID MOVE 1 \"Work\"",
            "UID MOVE 2 \"Work\"",
        ]
    );
    // Neither copy stays behind in Archive.
    assert!(
        fx.on_server("Archive").is_empty(),
        "{:?}",
        fx.on_server("Archive")
    );
    assert_eq!(fx.on_server("Work").len(), 2);
}

#[test]
fn a_store_error_while_queueing_rolls_back_the_local_move() {
    let fx = Fixture::new();
    // Break the outbox so the check for a copy already being moved fails.
    fx.store
        .connection()
        .execute_batch("ALTER TABLE server_outbox DROP COLUMN uid")
        .unwrap();
    let bravo = fx.thread("INBOX", "Bravo");
    assert!(
        fx.store
            .archive_thread(&fx.account.id, &bravo, "INBOX")
            .is_err()
    );
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);
    assert!(fx.subjects("Archive").is_empty());
}
