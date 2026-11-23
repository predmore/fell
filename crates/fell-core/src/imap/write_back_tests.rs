//! Write-back against the in-process IMAP server: the outbox rows archive
//! and move queue, what replay sends for them, and undo.

use crate::FlagChange;
use crate::account::{Account, AccountId, BackendKind};
use crate::backend::ImapBackend;
use crate::outbox::{DropReason, OutboxKind, OutboxState};
use crate::store::Store;
use crate::sync::sync_once;
use crate::threading::ThreadId;

use super::sync_tests::unused_smtp;
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
        part_write_delay: None,
        part_ignores_window: false,
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
            smtp: unused_smtp(),
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
        replay_outbox(&self.backend.config, &self.account.id, &self.store, true).unwrap()
    }

    fn account_config(&self) -> crate::AccountConfig {
        let mut config = crate::AccountConfig::bridge(
            "c@x",
            "caleb",
            crate::PasswordSource::Inline("secret".into()),
        );
        config.id = self.account.id.as_str().into();
        config.backend = BackendKind::Imap;
        config.port = self.server.addr.port();
        config.security = ImapSecurity::Plain;
        config.write_back = true;
        config
    }

    /// Replay as the sync thread does. This is what the status line sees.
    fn replay_as_window(&self) -> crate::outbox::ReplayReport {
        self.account_config().replay_outbox(&self.store).unwrap()
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

    /// Commands that change the server, or could.
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

    /// Add a message to a server mailbox, as another client would.
    fn deliver(&self, name: &str, uid: u32, raw: Vec<u8>) {
        let mut state = self.server.state.lock().unwrap();
        let mailbox = state.mailboxes.iter_mut().find(|m| m.name == name).unwrap();
        if let Some(next) = &mut mailbox.uidnext {
            *next = (*next).max(uid + 1);
        }
        mailbox.messages.push(FixtureMessage {
            uid,
            flags: Vec::new(),
            modseq: 9,
            body_form: BodyForm::Literal,
            raw,
        });
    }

    /// Whether `message_copies` still has the copy at `uid` in `mailbox`.
    fn copy_recorded(&self, mailbox: &str, uid: u32) -> bool {
        self.store
            .connection()
            .query_row(
                "SELECT COUNT(*) FROM message_copies
                 WHERE account_id = ?1 AND mailbox = ?2 AND uid = ?3",
                rusqlite::params![self.account.id.as_str(), mailbox, uid],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
            > 0
    }

    /// Replay until the rows are dropped, as many tries as it takes.
    fn replay_until_dropped(&self) -> Vec<crate::outbox::DroppedWrite> {
        let mut dropped = Vec::new();
        for _ in 0..super::write_back::MAX_ATTEMPTS {
            self.make_due();
            dropped.extend(self.replay().dropped);
        }
        dropped
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
    // The server said NO, so it did not move it: the next try is a first send.
    assert!(!row.sent);

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
fn labels_stay_local_with_write_back_off() {
    let fx = Fixture::new();
    fx.store.set_write_back(&fx.account.id, false).unwrap();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .label_thread(
            &fx.account.id,
            &alpha,
            crate::LabelChange::Add("work".into()),
        )
        .unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
    fx.store.set_write_back(&fx.account.id, true).unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
    fx.clear_commands();
    assert_eq!(fx.replay(), Default::default());
    assert!(fx.commands().is_empty());
    assert!(
        fx.store.thread_messages(&fx.account.id, &alpha).unwrap()[0]
            .labels
            .contains(&"work".into())
    );
    fx.store.undo(&fx.account.id).unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
}

#[test]
fn keyword_undo_survives_a_fetch_after_an_interrupted_store() {
    let mut state = state();
    state.condstore = true;
    let fx = Fixture::with(Store::open_in_memory().unwrap(), state);
    let account = &fx.account.id;
    let alpha = fx.thread("INBOX", "Alpha");
    let action = fx
        .store
        .label_thread(account, &alpha, crate::LabelChange::Add("Review".into()))
        .unwrap();
    fx.server.writes.lock().unwrap().lose_reply_to = Some("STORE");
    assert!(fx.replay().interrupted.is_some());
    assert!(fx.flags("INBOX", 1).contains(&"Review".into()));

    assert_eq!(fx.store.undo(account).unwrap().id, action.id);
    fx.clear_commands();
    fx.sync();
    assert!(
        fx.commands()
            .iter()
            .any(|command| command.contains("CHANGEDSINCE"))
    );
    assert!(
        fx.store.thread_messages(account, &alpha).unwrap()[0]
            .labels
            .is_empty()
    );

    assert_eq!(fx.replay().waiting, 0);
    assert!(!fx.flags("INBOX", 1).contains(&"Review".into()));
    fx.sync();
    assert!(
        fx.store.thread_messages(account, &alpha).unwrap()[0]
            .labels
            .is_empty()
    );
}

#[test]
fn keyword_undone_during_the_last_attempt_retires_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mail.db");
    let fx = Fixture::with_store(Store::open(&path).unwrap());
    let account = &fx.account.id;
    let alpha = fx.thread("INBOX", "Alpha");
    let action = fx
        .store
        .label_thread(account, &alpha, crate::LabelChange::Add("Review".into()))
        .unwrap();
    let rows = fx.store.outbox(account).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, OutboxKind::Keyword);
    fx.store
        .connection()
        .execute(
            "UPDATE server_outbox SET attempts = ?1 WHERE id = ?2",
            rusqlite::params![super::write_back::MAX_ATTEMPTS - 1, rows[0].id],
        )
        .unwrap();

    // SELECT is in flight, but the label STORE has not been sent yet.
    let undo_account = account.clone();
    fx.server.writes.lock().unwrap().before_refusing_select = Some(Box::new(move || {
        let store = Store::open(path).unwrap();
        assert_eq!(store.undo(&undo_account).unwrap().id, action.id);
        assert!(store.outbox(&undo_account).unwrap().is_empty());
    }));
    fx.clear_commands();
    let report = fx.replay_as_window();
    assert!(
        fx.writes()
            .iter()
            .any(|command| command.contains(" SELECT "))
    );
    assert!(fx.stores().is_empty());
    assert!(
        fx.store.thread_messages(account, &alpha).unwrap()[0]
            .labels
            .is_empty()
    );
    assert_eq!(report, Default::default());
}

#[test]
fn keyword_labels_preserve_reserved_flags_and_undo_per_copy() {
    let mut state = flag_state(false);
    state.mailboxes[0].messages[0]
        .flags
        .push("$Forwarded".into());
    state.mailboxes[1].messages[0].flags.push("Review".into());
    let fx = Fixture::with(Store::open_in_memory().unwrap(), state);
    let account = &fx.account.id;
    let alpha = fx.thread("INBOX", "Alpha");
    let initial = fx.store.thread_messages(account, &alpha).unwrap()[0]
        .labels
        .clone();
    assert_eq!(initial, ["Review"]);
    // Save a local removal first, leaving the server copies' flags as they were.
    fx.store.set_write_back(account, false).unwrap();
    fx.store
        .label_thread(account, &alpha, crate::LabelChange::Remove("Review".into()))
        .unwrap();
    fx.store.set_write_back(account, true).unwrap();
    let action = fx
        .store
        .label_thread(account, &alpha, crate::LabelChange::Add("Review".into()))
        .unwrap();
    assert_eq!(
        fx.store
            .outbox(account)
            .unwrap()
            .iter()
            .filter(|row| row.kind == OutboxKind::Keyword)
            .count(),
        3
    );
    fx.clear_commands();
    assert!(fx.replay().dropped.is_empty());
    assert!(fx.flags("INBOX", 4).contains(&"$Forwarded".into()));
    assert!(fx.flags("INBOX", 4).contains(&"Review".into()));
    assert!(fx.flags(ESCAPED_FOLDER, 12).contains(&"Review".into()));
    assert_eq!(
        fx.stores(),
        [
            "UID STORE 4 +FLAGS.SILENT (Review)",
            "UID STORE 12 +FLAGS.SILENT (Review)"
        ]
    );

    assert_eq!(fx.store.undo(account).unwrap().id, action.id);
    fx.clear_commands();
    assert!(fx.replay().dropped.is_empty());
    assert!(fx.flags("INBOX", 4).contains(&"$Forwarded".into()));
    assert!(!fx.flags("INBOX", 4).contains(&"Review".into()));
    assert!(fx.flags(ESCAPED_FOLDER, 9).contains(&"Review".into()));
    assert_eq!(
        fx.stores(),
        [
            "UID STORE 4 -FLAGS.SILENT (Review)",
            "UID STORE 12 -FLAGS.SILENT (Review)"
        ]
    );
}

#[test]
fn names_outside_the_imap_keyword_grammar_stay_local() {
    let fx = Fixture::new();
    let account = &fx.account.id;
    let thread = fx.thread("INBOX", "Alpha");
    let names = ["a\\Deleted", "x \\Seen", "foo)", "tag]", "Café"];
    for name in names {
        fx.store
            .label_thread(account, &thread, crate::LabelChange::Add(name.into()))
            .unwrap();
    }
    assert!(fx.store.outbox(account).unwrap().is_empty());
    assert_eq!(
        fx.store.thread_messages(account, &thread).unwrap()[0].labels,
        names
    );
    fx.clear_commands();
    assert_eq!(fx.replay(), Default::default());
    assert!(fx.commands().is_empty());
    assert_eq!(fx.flags("INBOX", 1), ["\\Seen"]);
}

#[test]
fn early_keyword_edit_replays_and_undoes_for_every_copy_in_order() {
    let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(false));
    let account = &fx.account.id;
    let thread = fx.thread("INBOX", "Alpha");
    let copies = [("INBOX", 4), (ESCAPED_FOLDER, 12), (ESCAPED_FOLDER, 9)];
    let server_has_label = |value| {
        for (folder, uid) in copies {
            assert_eq!(fx.flags(folder, uid).contains(&"Review".into()), value);
        }
    };
    let mut message = fx
        .store
        .thread_messages(account, &thread)
        .unwrap()
        .remove(0);
    message.remote_id = crate::RemoteId("local-preview".into());
    let store = Store::open_in_memory().unwrap();
    store.upsert_account(&fx.account).unwrap();
    store.upsert_message(&message).unwrap();
    store.set_write_back(account, true).unwrap();
    let added = store
        .label_thread(account, &thread, crate::LabelChange::Add("Review".into()))
        .unwrap();
    let outbox = store.outbox(account).unwrap();
    assert_eq!(outbox.len(), 1);
    assert_eq!(outbox[0].uid, None);

    let replay = || replay_outbox(&fx.backend.config, account, &store, false).unwrap();
    assert_eq!(replay().waiting, 1);
    server_has_label(false);
    sync_once(&fx.account, &fx.backend, &store).unwrap();
    // A later edit must wait for the earlier write on all three copies,
    // including those whose rows are only inserted when replay starts.
    let removed = store
        .label_thread(
            account,
            &thread,
            crate::LabelChange::Remove("Review".into()),
        )
        .unwrap();
    fx.clear_commands();
    assert_eq!(replay().waiting, 0);
    let expected: Vec<_> = ['+', '-']
        .into_iter()
        .flat_map(|sign| {
            copies
                .into_iter()
                .map(move |(_, uid)| format!("UID STORE {uid} {sign}FLAGS.SILENT (Review)"))
        })
        .collect();
    assert_eq!(fx.stores(), expected);
    let early_rows: Vec<_> = store
        .outbox(account)
        .unwrap()
        .into_iter()
        .filter(|row| row.action_id == Some(added.id))
        .collect();
    assert_eq!(early_rows.len(), copies.len());
    assert!(
        early_rows
            .iter()
            .all(|row| row.state == OutboxState::Done && row.sent)
    );
    server_has_label(false);

    assert_eq!(store.undo(account).unwrap().id, removed.id);
    assert_eq!(replay().waiting, 0);
    server_has_label(true);
    assert_eq!(store.undo(account).unwrap().id, added.id);
    assert_eq!(replay().waiting, 0);
    server_has_label(false);
}

#[test]
fn agent_keyword_labels_use_the_same_outbox_and_undo() {
    let fx = Fixture::new();
    let account = &fx.account.id;
    let alpha = fx.thread("INBOX", "Alpha");
    let action = fx
        .store
        .add_thread_label(account, &alpha, "Review", "test-agent")
        .unwrap()
        .unwrap();
    assert!(fx.replay().dropped.is_empty());
    assert!(fx.flags("INBOX", 1).contains(&"Review".into()));
    assert_eq!(
        fx.store.undo_actor(account, "test-agent").unwrap().id,
        action.id
    );
    assert!(fx.replay().dropped.is_empty());
    assert!(!fx.flags("INBOX", 1).contains(&"Review".into()));
}

#[test]
fn bridge_labels_create_copy_and_remove_only_the_label_copy() {
    let server = TestServer::spawn(state());
    {
        let mut writes = server.writes.lock().unwrap();
        writes.uidplus = true;
        writes.moves = true;
        writes.lose_reply_to = Some("COPY");
    }
    let account = super::sync_tests::account("bridge", BackendKind::Bridge);
    let store = Store::open_in_memory().unwrap();
    let backend = crate::backend::BridgeBackend {
        account_id: account.id.clone(),
        imap: config(server.addr.port()),
        ..Default::default()
    };
    sync_once(&account, &backend, &store).unwrap();
    store.set_write_back(&account.id, true).unwrap();
    let threads: Vec<_> = store
        .threads_in_folder(&account.id, "INBOX")
        .unwrap()
        .into_iter()
        .map(|row| row.thread_id)
        .collect();
    let label = r#"Café & "travel\plan""#;
    let raw = r#"Labels/Caf&AOk- &- "travel\plan""#;
    let action = store
        .label_threads(&account.id, &threads, crate::LabelChange::Add(label.into()))
        .unwrap();
    let replay = || replay_outbox(&backend.imap, &account.id, &store, false).unwrap();
    assert!(replay().interrupted.is_some());
    assert_eq!(
        replay().waiting,
        0,
        "recover the lost reply without copying again"
    );
    let page = store
        .label_page(
            &account.id,
            label,
            &crate::FolderSpan::Top { rows: 2 },
            None,
        )
        .unwrap()
        .0;
    assert_eq!(page.threads.len(), 2);
    {
        let state = server.state.lock().unwrap();
        let destination = state.mailboxes.iter().find(|m| m.name == raw).unwrap();
        assert_eq!(destination.messages.len(), 2);
        assert_eq!(
            state
                .commands
                .iter()
                .filter(|cmd| cmd.contains(" UID COPY "))
                .count(),
            2
        );
        assert_eq!(state.mailboxes[0].messages.len(), 2);
    }
    assert_eq!(store.undo(&account.id).unwrap().id, action.id);
    assert_eq!(replay().waiting, 0);
    sync_once(&account, &backend, &store).unwrap();
    {
        let state = server.state.lock().unwrap();
        assert_eq!(state.mailboxes[0].messages.len(), 2);
        assert!(
            state
                .mailboxes
                .iter()
                .find(|m| m.name == raw)
                .unwrap()
                .messages
                .is_empty()
        );
        assert_eq!(
            state
                .commands
                .iter()
                .filter(|cmd| cmd.contains(" UID EXPUNGE "))
                .count(),
            2
        );
        assert!(!state.commands.iter().any(|cmd| cmd.contains(" MOVE ")));
        let mut selected = "";
        for command in &state.commands {
            if let Some((_, mailbox)) = command.split_once(" SELECT ") {
                selected = mailbox;
            }
            if command.contains(" UID STORE ") || command.contains(" UID EXPUNGE ") {
                assert_eq!(selected, format!("{raw:?}"));
            }
        }
    }
    assert!(
        store
            .label_page(
                &account.id,
                label,
                &crate::FolderSpan::Top { rows: 2 },
                None
            )
            .unwrap()
            .0
            .threads
            .is_empty()
    );
    assert_eq!(store.list_labels(&account.id).unwrap(), [label]);
}

#[test]
fn bridge_label_undo_survives_a_fetch_after_an_interrupted_copy() {
    let server = TestServer::spawn(state());
    {
        let mut writes = server.writes.lock().unwrap();
        writes.uidplus = true;
        writes.lose_reply_to = Some("COPY");
    }
    let account = super::sync_tests::account("bridge", BackendKind::Bridge);
    let store = Store::open_in_memory().unwrap();
    let backend = crate::backend::BridgeBackend {
        account_id: account.id.clone(),
        imap: config(server.addr.port()),
        ..Default::default()
    };
    sync_once(&account, &backend, &store).unwrap();
    store.set_write_back(&account.id, true).unwrap();
    let thread = store.threads_in_folder(&account.id, "INBOX").unwrap()[0]
        .thread_id
        .clone();
    let label = "Field notes";
    let action = store
        .label_thread(&account.id, &thread, crate::LabelChange::Add(label.into()))
        .unwrap();
    let replay = || replay_outbox(&backend.imap, &account.id, &store, false).unwrap();
    assert!(replay().interrupted.is_some());
    assert_eq!(store.undo(&account.id).unwrap().id, action.id);
    sync_once(&account, &backend, &store).unwrap();
    assert_eq!(store.list_labels(&account.id).unwrap(), [label]);
    assert!(
        store.thread_messages(&account.id, &thread).unwrap()[0]
            .labels
            .is_empty()
    );

    assert_eq!(replay().waiting, 0);
    sync_once(&account, &backend, &store).unwrap();
    assert!(
        store.thread_messages(&account.id, &thread).unwrap()[0]
            .labels
            .is_empty()
    );
}

#[test]
fn bridge_label_removal_resumes_each_copy_after_a_lost_reply() {
    for command in ["STORE", "EXPUNGE"] {
        let mailboxes = vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha")]),
            mailbox("Labels/Review", "", 8, &[(10, "Alpha"), (11, "Alpha")]),
        ];
        let server = TestServer::spawn(state_with(mailboxes));
        {
            let mut writes = server.writes.lock().unwrap();
            writes.uidplus = true;
            writes.lose_reply_to = Some(command);
        }
        let account = super::sync_tests::account("bridge", BackendKind::Bridge);
        let store = Store::open_in_memory().unwrap();
        let backend = crate::backend::BridgeBackend {
            account_id: account.id.clone(),
            imap: config(server.addr.port()),
            ..Default::default()
        };
        sync_once(&account, &backend, &store).unwrap();
        store.set_write_back(&account.id, true).unwrap();
        let thread = &store.folder_threads(&account.id, "INBOX").unwrap()[0].thread_id;
        store
            .label_thread(
                &account.id,
                thread,
                crate::LabelChange::Remove("Review".into()),
            )
            .unwrap();
        let replay = || replay_outbox(&backend.imap, &account.id, &store, false).unwrap();
        assert!(replay().interrupted.is_some(), "{command}");
        {
            let state = server.state.lock().unwrap();
            let copies = &state.mailboxes[1].messages;
            if command == "STORE" {
                assert_eq!(copies.len(), 2);
                assert!(
                    copies
                        .iter()
                        .any(|copy| copy.flags.contains(&"\\Deleted".into()))
                );
            } else {
                assert_eq!(copies.len(), 1);
            }
        }
        assert_eq!(replay().waiting, 0, "{command}");
        assert_eq!(
            store.outbox(&account.id).unwrap()[0].state,
            OutboxState::Done
        );
        let state = server.state.lock().unwrap();
        assert!(state.mailboxes[1].messages.is_empty());
        assert_eq!(state.mailboxes[0].messages[0].flags, ["\\Seen"]);
        for (verb, count) in [(" UID EXPUNGE ", 2), (" UID STORE ", 2)] {
            assert_eq!(
                state
                    .commands
                    .iter()
                    .filter(|cmd| cmd.contains(verb))
                    .count(),
                count
            );
        }
    }
}

/// INBOX with Alpha and Bravo; Trash and Junk named as this server names
/// them, found by SPECIAL-USE.
fn with_trash_and_junk() -> Fixture {
    Fixture::with(
        Store::open_in_memory().unwrap(),
        state_with(vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha"), (2, "Bravo")]),
            mailbox("Deleted Items", "\\Trash", 50, &[]),
            mailbox("Bulk", "\\Junk", 60, &[]),
        ]),
    )
}

#[test]
fn delete_and_spam_of_a_selection_are_server_moves_and_undo_reverses_them() {
    let fx = with_trash_and_junk();
    let both = [fx.thread("INBOX", "Alpha"), fx.thread("INBOX", "Bravo")];
    fx.store
        .delete_threads(&fx.account.id, &both, "INBOX")
        .unwrap();
    let rows = fx.store.outbox(&fx.account.id).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().all(|row| row.target_folder == "Deleted Items"
        && row.source_mailbox.as_deref() == Some("INBOX")));

    fx.clear_commands();
    assert_eq!(fx.replay().sent, 2);
    let writes = without_tags(&fx.writes());
    assert!(
        writes
            .iter()
            .all(|command| !command.contains("STORE") && !command.contains("EXPUNGE")),
        "{writes:?}"
    );
    assert!(fx.on_server("INBOX").is_empty());
    assert_eq!(fx.on_server("Deleted Items").len(), 2);

    // One undo puts both back, on the server too.
    fx.store.undo(&fx.account.id).unwrap();
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);
    assert_eq!(fx.replay().sent, 2);
    assert_eq!(fx.on_server("INBOX").len(), 2);
    assert!(fx.on_server("Deleted Items").is_empty());
    fx.sync();

    // Spam goes to the server's Junk; from there, not spam goes to INBOX.
    let bravo = fx.thread("INBOX", "Bravo");
    let one = std::slice::from_ref(&bravo);
    fx.store.spam_threads(&fx.account.id, one, "INBOX").unwrap();
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(fx.on_server("Bulk").len(), 1);
    fx.sync();
    assert_eq!(fx.subjects("Bulk"), vec!["Bravo"]);
    let rescued = fx.store.spam_threads(&fx.account.id, one, "Bulk").unwrap();
    assert_eq!(rescued.kind, crate::ActionKind::NotSpam);
    let last = fx.store.outbox(&fx.account.id).unwrap().pop().unwrap();
    assert_eq!(
        (last.source_folder.as_str(), last.target_folder.as_str()),
        ("Bulk", "INBOX")
    );
    assert_eq!(fx.replay().sent, 1);
    assert!(fx.on_server("Bulk").is_empty());
    assert_eq!(fx.on_server("INBOX").len(), 2);
}

#[test]
fn permanent_delete_never_reaches_the_server() {
    for state in ["queued", "sent", "settled"] {
        let fx = with_trash_and_junk();
        let alpha = fx.thread("INBOX", "Alpha");
        let bravo = fx.thread("INBOX", "Bravo");
        let batch = fx
            .store
            .delete_threads(&fx.account.id, &[alpha.clone(), bravo], "INBOX")
            .unwrap();
        if state == "sent" {
            fx.server.writes.lock().unwrap().lose_reply_to = Some("MOVE");
            assert!(fx.replay().interrupted.is_some());
        } else if state == "settled" {
            assert_eq!(fx.replay().sent, 2);
        }
        let before_purge = fx.store.outbox(&fx.account.id).unwrap();

        fx.store
            .purge_threads(&fx.account.id, &[alpha], "Deleted Items")
            .unwrap();
        assert_eq!(fx.store.outbox(&fx.account.id).unwrap(), before_purge);
        assert_eq!(fx.store.undo(&fx.account.id).unwrap().id, batch.id);
        assert_eq!(fx.subjects("INBOX"), vec!["Bravo"], "{state}");
        assert!(fx.subjects("Deleted Items").is_empty(), "{state}");

        fx.clear_commands();
        let report = fx.replay();
        assert_eq!(report.waiting, 0, "{state}: {report:?}");
        fx.sync();
        assert!(
            fx.writes()
                .iter()
                .all(|command| !command.contains("STORE") && !command.contains("EXPUNGE")),
            "{state}"
        );
        assert_eq!(
            fx.on_server("Deleted Items"),
            vec![(1, "Alpha".to_string())],
            "{state}"
        );
        assert_eq!(fx.subjects("INBOX"), vec!["Bravo"], "{state}");
        assert!(fx.subjects("Deleted Items").is_empty(), "{state}");
    }
}

#[test]
fn queued_moves_can_still_carry_a_purged_message_to_server_trash() {
    let fx = Fixture::with(
        Store::open_in_memory().unwrap(),
        state_with(vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha")]),
            mailbox("Archive", "\\Archive", 30, &[]),
            mailbox("Deleted Items", "\\Trash", 50, &[]),
        ]),
    );
    let alpha = fx.thread("INBOX", "Alpha");
    let one = std::slice::from_ref(&alpha);
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.store
        .delete_threads(&fx.account.id, one, "Archive")
        .unwrap();
    fx.store
        .purge_threads(&fx.account.id, one, "Deleted Items")
        .unwrap();
    assert_eq!(fx.replay().sent, 2);
    fx.sync();
    assert!(fx.on_server("INBOX").is_empty());
    assert!(fx.on_server("Archive").is_empty());
    assert_eq!(
        fx.on_server("Deleted Items"),
        vec![(1, "Alpha".to_string())]
    );
    assert!(fx.subjects("Deleted Items").is_empty());
}

#[test]
fn delete_and_spam_queue_nothing_with_write_back_off() {
    let fx = with_trash_and_junk();
    fx.store.set_write_back(&fx.account.id, false).unwrap();
    let alpha = fx.thread("INBOX", "Alpha");
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .delete_threads(&fx.account.id, std::slice::from_ref(&alpha), "INBOX")
        .unwrap();
    fx.store
        .spam_threads(&fx.account.id, std::slice::from_ref(&bravo), "INBOX")
        .unwrap();
    fx.store
        .purge_threads(
            &fx.account.id,
            std::slice::from_ref(&alpha),
            "Deleted Items",
        )
        .unwrap();
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
    assert_eq!(fx.subjects("Bulk"), vec!["Bravo"]);
    fx.clear_commands();
    assert_eq!(fx.replay(), Default::default());
    assert!(fx.commands().is_empty());
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
        crate::outbox::Outcome::Done { placed: (30, 1) },
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

/// A message with this Message-ID header, verbatim, and this subject.
fn raw_with_message_id(header: &str, subject: &str) -> Vec<u8> {
    format!(
        "From: Ann <ann@x>\r\nSubject: {subject}\r\n{header}Date: Mon, 16 Nov 2026 12:00:00 +0000\r\n\
         MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{subject} body"
    )
    .into_bytes()
}

#[test]
fn a_server_without_move_keeps_the_change_local_and_says_so_once() {
    let fx = Fixture::new();
    // UIDPLUS without MOVE: Fell could copy and expunge, but does not.
    fx.server.writes.lock().unwrap().moves = false;
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.clear_commands();
    let report = fx.replay_as_window();
    assert_eq!(report.dropped.len(), 1);
    assert_eq!(report.dropped[0].reason, DropReason::NoMove);
    assert!(fx.writes().is_empty(), "{:?}", fx.writes());

    // The next archive stays local too, and the notice is not repeated.
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    let report = fx.replay_as_window();
    assert!(report.dropped.is_empty(), "{report:?}");
    assert_eq!(report.sent, 0);
    assert!(fx.writes().is_empty(), "{:?}", fx.writes());
    assert_eq!(fx.on_server("INBOX").len(), 2);
    assert!(fx.on_server("Archive").is_empty());
    assert_eq!(fx.subjects("Archive"), vec!["Alpha", "Bravo"]);
}

#[test]
fn a_half_done_copy_from_an_earlier_build_is_never_expunged() {
    let fx = Fixture::new();
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    // An earlier build copied Bravo to Archive and flagged the INBOX copy
    // \Deleted, and stopped before UID EXPUNGE.
    fx.store
        .connection()
        .execute(
            "UPDATE server_outbox SET state = 'copied', sent = 1,
                 placed_uidvalidity = 30, placed_uid = 1",
            [],
        )
        .unwrap();
    let raw = fx.server.state.lock().unwrap().mailboxes[0].messages[1]
        .raw
        .clone();
    fx.deliver("Archive", 1, raw);
    fx.server.state.lock().unwrap().mailboxes[0].messages[1]
        .flags
        .push("\\Deleted".into());

    fx.clear_commands();
    assert_eq!(fx.replay(), Default::default());
    assert!(fx.commands().is_empty(), "{:?}", fx.commands());
    let abandoned = crate::outbox::take_abandoned(&fx.store, &fx.account.id).unwrap();
    assert_eq!(abandoned.len(), 1);
    assert_eq!(abandoned[0].reason, DropReason::Abandoned);
    assert_eq!(abandoned[0].source_folder, "INBOX");
    assert!(
        crate::outbox::take_abandoned(&fx.store, &fx.account.id)
            .unwrap()
            .is_empty()
    );
    // Undo of that archive has nothing to reverse on the server.
    fx.store.undo(&fx.account.id).unwrap();
    fx.make_due();
    fx.clear_commands();
    assert_eq!(fx.replay(), Default::default());
    assert!(fx.commands().is_empty(), "{:?}", fx.commands());
    assert_eq!(fx.on_server("INBOX").len(), 2);
    assert!(fx.flags("INBOX", 2).contains(&"\\Deleted".to_string()));
}

#[test]
fn a_move_that_found_nothing_takes_no_look_alike_and_no_older_copy() {
    // Archive already has a message the Message-ID search also matches
    // (it ignores case), and an exact duplicate of Bravo.
    let fx = Fixture::with(
        Store::open_in_memory().unwrap(),
        state_with(vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha"), (2, "Bravo")]),
            mailbox("Archive", "\\Archive", 30, &[]),
            mailbox("Work", "", 40, &[]),
        ]),
    );
    fx.deliver(
        "Archive",
        1,
        raw_with_message_id("Message-ID: <BRAVO@x>\r\n", "Look-alike"),
    );
    fx.deliver(
        "Archive",
        2,
        raw_with_message_id("Message-ID: <bravo@x>\r\n", "Bravo"),
    );
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    // Another client deletes Bravo from INBOX: the move finds nothing.
    fx.server.state.lock().unwrap().mailboxes[0]
        .messages
        .retain(|message| message.uid != 2);

    fx.clear_commands();
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (0, 1), "{report:?}");
    let commands = without_tags(&fx.commands());
    assert!(
        commands.contains(&"STATUS \"Archive\" (UIDVALIDITY UIDNEXT)".to_string()),
        "{commands:?}"
    );
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!(row.state, OutboxState::Pending);
    assert_eq!((row.placed_uidvalidity, row.placed_uid), (None, None));
    assert_eq!(row.attempts, 1);
    // The INBOX copy stays on record: nothing replaced it.
    assert!(fx.copy_recorded("INBOX", 2));

    let dropped = fx.replay_until_dropped();
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].reason, DropReason::NotFound);
    assert_eq!(fx.on_server("Archive").len(), 2);
}

#[test]
fn a_lost_move_reply_takes_only_the_exact_message_id_that_arrived_after() {
    let fx = Fixture::new();
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    fx.server.writes.lock().unwrap().lose_reply_to = Some("MOVE");
    // A look-alike the search also matches lands in Archive after replay
    // read UIDNEXT and before the move, so it is above the bound too.
    let report = super::write_back::replay_with(
        &fx.backend.config,
        &fx.account.id,
        &fx.store,
        true,
        &|| {
            fx.deliver(
                "Archive",
                1,
                raw_with_message_id("Message-ID: <BRAVO@x>\r\n", "Look-alike"),
            )
        },
    )
    .unwrap();
    assert!(report.interrupted.is_some());
    assert_eq!(
        fx.on_server("Archive"),
        vec![(1, "Look-alike".to_string()), (2, "Bravo".to_string())]
    );
    // And another after the move.
    fx.deliver(
        "Archive",
        3,
        raw_with_message_id("Message-ID: <bravo@X>\r\n", "Look-alike"),
    );

    fx.clear_commands();
    assert_eq!(fx.replay().sent, 1);
    let commands = without_tags(&fx.commands());
    assert!(
        commands.contains(&"UID FETCH 1:3 (UID BODY.PEEK[HEADER.FIELDS (MESSAGE-ID)])".into()),
        "{commands:?}"
    );
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!(row.state, OutboxState::Done);
    assert_eq!(
        (row.placed_uidvalidity, row.placed_uid),
        (Some(30), Some(2))
    );
}

#[test]
fn a_refused_search_leaves_the_row_waiting() {
    let fx = Fixture::new();
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    fx.server.writes.lock().unwrap().lose_reply_to = Some("MOVE");
    assert!(fx.replay().interrupted.is_some());

    fx.server.writes.lock().unwrap().refuse_search = true;
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (0, 1));
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!((row.state, row.placed_uid), (OutboxState::Pending, None));
    assert!(row.sent);

    // Once the search works, the copy is found from the first bound.
    fx.server.writes.lock().unwrap().refuse_search = false;
    fx.make_due();
    assert_eq!(fx.replay().sent, 1);
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!((row.state, row.placed_uid), (OutboxState::Done, Some(1)));
}

#[test]
fn turning_write_back_off_after_the_pass_loaded_its_rows_sends_nothing() {
    let fx = Fixture::new();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.clear_commands();
    let report = super::write_back::replay_with(
        &fx.backend.config,
        &fx.account.id,
        &fx.store,
        true,
        &|| fx.store.set_write_back(&fx.account.id, false).unwrap(),
    )
    .unwrap();
    assert_eq!(report.sent, 0);
    let moves: Vec<String> = without_tags(&fx.commands())
        .into_iter()
        .filter(|command| command.starts_with("UID MOVE"))
        .collect();
    assert!(moves.is_empty(), "{moves:?}");
    assert_eq!(fx.on_server("INBOX").len(), 2);
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
}

#[test]
fn an_undo_after_the_pass_loaded_its_rows_sends_nothing() {
    let fx = Fixture::new();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.clear_commands();
    super::write_back::replay_with(&fx.backend.config, &fx.account.id, &fx.store, true, &|| {
        fx.store.undo(&fx.account.id).unwrap();
    })
    .unwrap();
    assert!(
        !without_tags(&fx.commands())
            .iter()
            .any(|command| command.starts_with("UID MOVE")),
        "{:?}",
        fx.commands()
    );
    assert_eq!(fx.on_server("INBOX").len(), 2);
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
}

#[test]
fn a_move_with_no_message_id_and_no_copyuid_is_never_done() {
    let fx = Fixture::with(
        Store::open_in_memory().unwrap(),
        state_with(vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha")]),
            mailbox("Archive", "\\Archive", 30, &[]),
            mailbox("Work", "", 40, &[]),
        ]),
    );
    fx.deliver("INBOX", 2, raw_with_message_id("", "Nameless"));
    fx.sync();
    // MOVE without UIDPLUS: the reply never says where the message went.
    fx.server.writes.lock().unwrap().uidplus = false;
    let nameless = fx.thread("INBOX", "Nameless");
    fx.store
        .archive_thread(&fx.account.id, &nameless, "INBOX")
        .unwrap();

    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (0, 1), "{report:?}");
    assert_eq!(fx.on_server("Archive"), vec![(1, "Nameless".to_string())]);
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!((row.state, row.placed_uid), (OutboxState::Pending, None));
    assert!(fx.copy_recorded("INBOX", 2));

    let dropped = fx.replay_until_dropped();
    assert_eq!(dropped.len(), 1);
    assert_eq!(dropped[0].reason, DropReason::NotFound);
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!(row.state, OutboxState::Dropped);
    // Never recorded as moved, so the copy Fell knows of is still there.
    assert!(fx.copy_recorded("INBOX", 2));
}

#[test]
fn an_undo_while_a_sent_move_is_unsettled_still_moves_it_back() {
    let fx = Fixture::new();
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    // The server moves Bravo, and the reply is lost: the row stays pending
    // and sent, with its new UID not known yet.
    fx.server.writes.lock().unwrap().lose_reply_to = Some("MOVE");
    assert!(fx.replay().interrupted.is_some());
    assert_eq!(fx.on_server("Archive"), vec![(1, "Bravo".to_string())]);
    let row = &fx.store.outbox(&fx.account.id).unwrap()[0];
    assert_eq!((row.state, row.sent), (OutboxState::Pending, true));

    // Undo before the next try settles it.
    fx.store.undo(&fx.account.id).unwrap();
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);
    let rows = fx.store.outbox(&fx.account.id).unwrap();
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[1].reverses, Some(rows[0].id));

    // The next pass finds where Bravo went, then moves it back from there.
    fx.make_due();
    fx.clear_commands();
    let report = fx.replay();
    assert_eq!(report.sent, 2, "{report:?}");
    assert!(report.dropped.is_empty(), "{report:?}");
    assert!(
        without_tags(&fx.writes()).contains(&"UID MOVE 1 \"INBOX\"".to_string()),
        "{:?}",
        fx.writes()
    );
    assert!(fx.on_server("Archive").is_empty());
    assert_eq!(
        fx.on_server("INBOX"),
        vec![(1, "Alpha".to_string()), (3, "Bravo".to_string())]
    );
    fx.sync();
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);
    assert!(fx.subjects("Archive").is_empty());
}

#[test]
fn an_undo_of_a_sent_move_the_server_then_refuses_sends_no_reverse() {
    let fx = Fixture::new();
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    // The MOVE went out, and the connection dropped before it reached the
    // server.
    fx.store
        .connection()
        .execute("UPDATE server_outbox SET sent = 1", [])
        .unwrap();
    fx.store.undo(&fx.account.id).unwrap();
    assert_eq!(fx.store.outbox(&fx.account.id).unwrap().len(), 2);

    // Its retry is refused: the server never moved Bravo, so neither row
    // is sent again and nothing is reported.
    fx.server.writes.lock().unwrap().refuse = Some("[CANNOT] not now".into());
    fx.clear_commands();
    let report = fx.replay();
    assert!(report.dropped.is_empty(), "{report:?}");
    assert_eq!((report.sent, report.waiting), (0, 0), "{report:?}");
    let rows = fx.store.outbox(&fx.account.id).unwrap();
    assert!(
        rows.iter().all(|row| row.state == OutboxState::Dropped),
        "{rows:?}"
    );
    fx.server.writes.lock().unwrap().refuse = None;
    fx.make_due();
    fx.clear_commands();
    assert_eq!(fx.replay(), Default::default());
    assert!(fx.commands().is_empty(), "{:?}", fx.commands());
    assert_eq!(fx.on_server("INBOX").len(), 2);
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);
}

#[test]
fn an_undo_during_a_sync_pass_keeps_the_restored_folder() {
    use crate::backend::MailBackend;

    // Bravo is in Work as well, so its row keeps a folder when INBOX goes.
    let fx = Fixture::with(
        Store::open_in_memory().unwrap(),
        state_with(vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha"), (2, "Bravo")]),
            mailbox("Archive", "\\Archive", 30, &[]),
            mailbox("Work", "", 40, &[(1, "Bravo")]),
        ]),
    );
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();
    // The server moves Bravo; the reply is lost, so the move is not settled.
    fx.server.writes.lock().unwrap().lose_reply_to = Some("MOVE");
    assert!(fx.replay().interrupted.is_some());
    assert_eq!(fx.on_server("INBOX"), vec![(1, "Alpha".to_string())]);

    // A pass sees Bravo gone from INBOX, and the user undoes before the
    // pass ends and settles what went.
    let id = &fx.account.id;
    let cursor = fx.store.sync_cursor(id).unwrap();
    let known = |mailbox: &str, uidvalidity: u32, range: (u32, u32)| {
        fx.store.stored_uids(id, mailbox, uidvalidity, range)
    };
    let mut undone = false;
    fx.backend
        .sync_changes(cursor.as_ref(), &known, &mut |delta| {
            fx.store.apply_delta(id, &delta)?;
            let left_inbox = delta
                .removals
                .iter()
                .any(|removal| removal.folder == "INBOX");
            if left_inbox && !undone {
                assert_eq!(delta.settle, crate::backend::Settle::Later);
                fx.store.undo(id)?;
                undone = true;
            }
            Ok(())
        })
        .unwrap();
    assert!(undone);
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);

    // Another pass before the reverse is sent keeps it there too.
    fx.sync();
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);

    fx.make_due();
    assert_eq!(fx.replay().sent, 2);
    assert_eq!(fx.on_server("INBOX").len(), 2);
    assert!(fx.on_server("Archive").is_empty());
    fx.sync();
    assert_eq!(fx.subjects("INBOX"), vec!["Alpha", "Bravo"]);
    assert!(
        fx.subjects("Archive").is_empty(),
        "{:?}",
        fx.subjects("Archive")
    );
    assert_eq!(fx.subjects("Work"), vec!["Bravo"]);
}

#[test]
fn no_move_is_said_even_when_a_half_done_copy_is_reported_too() {
    let fx = Fixture::new();
    fx.server.writes.lock().unwrap().moves = false;
    // An earlier build left Alpha's archive half done...
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.store
        .connection()
        .execute("UPDATE server_outbox SET state = 'copied', sent = 1", [])
        .unwrap();
    // ...and Bravo's archive is due now, on a server without MOVE.
    let bravo = fx.thread("INBOX", "Bravo");
    fx.store
        .archive_thread(&fx.account.id, &bravo, "INBOX")
        .unwrap();

    // The status line names the first reason: each notice gets a pass.
    let first = fx.replay_as_window();
    assert_eq!(
        first.dropped.first().map(|dropped| dropped.reason),
        Some(DropReason::NoMove),
        "{first:?}"
    );
    let second = fx.replay_as_window();
    assert_eq!(
        second.dropped.first().map(|dropped| dropped.reason),
        Some(DropReason::Abandoned),
        "{second:?}"
    );
    // Each is said once, and nothing was moved.
    assert!(fx.replay_as_window().dropped.is_empty());
    assert!(
        fx.writes().iter().all(|write| !write.contains("MOVE")),
        "{:?}",
        fx.writes()
    );
}

const ESCAPED_FOLDER: &str = "Work \"quotes\" \\ project";

fn flag_state(condstore: bool) -> TestState {
    let mut state = state_with(vec![
        mailbox("INBOX", "", 7, &[(4, "Alpha")]),
        mailbox(ESCAPED_FOLDER, "", 21, &[(9, "Alpha"), (12, "Alpha")]),
    ]);
    state.condstore = condstore;
    for message in state
        .mailboxes
        .iter_mut()
        .flat_map(|folder| &mut folder.messages)
    {
        message.flags = vec!["\\Answered".into()];
        message.modseq = u64::from(message.uid);
    }
    state
}

impl Fixture {
    fn flag(&self, thread: &ThreadId, change: FlagChange) {
        self.store
            .flag_threads(
                &self.account.id,
                std::slice::from_ref(thread),
                "INBOX",
                change,
            )
            .unwrap()
            .unwrap();
    }

    fn local_flags(&self, thread: &ThreadId) -> (bool, bool) {
        let messages = self
            .store
            .thread_messages(&self.account.id, thread)
            .unwrap();
        assert_eq!(messages.len(), 1);
        (messages[0].unread, messages[0].starred)
    }

    fn stores(&self) -> Vec<String> {
        without_tags(&self.commands())
            .into_iter()
            .filter(|command| command.starts_with("UID STORE "))
            .collect()
    }
}

#[test]
fn open_marks_each_server_copy_once_and_sync_stays_read_only() {
    let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(true));
    // Flag write-back works even on a server without MOVE.
    fx.server.writes.lock().unwrap().moves = false;
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Read);
    assert_eq!(fx.store.outbox(&fx.account.id).unwrap().len(), 3);

    fx.clear_commands();
    fx.sync();
    assert!(fx.writes().is_empty(), "{:?}", fx.commands());
    assert!(fx.commands().iter().any(|cmd| cmd.contains(" EXAMINE ")));
    assert_eq!(fx.local_flags(&alpha), (false, false));
    assert_eq!(fx.replay_as_window().sent, 3);
    assert_eq!(
        fx.stores(),
        [
            "UID STORE 4 (UNCHANGEDSINCE 4) +FLAGS.SILENT (\\Seen)",
            "UID STORE 9 (UNCHANGEDSINCE 9) +FLAGS.SILENT (\\Seen)",
            "UID STORE 12 (UNCHANGEDSINCE 12) +FLAGS.SILENT (\\Seen)",
        ]
    );
    assert!(
        without_tags(&fx.writes())
            .contains(&"SELECT \"Work \\\"quotes\\\" \\\\ project\" (CONDSTORE)".into())
    );
    for (folder, uid) in [("INBOX", 4), (ESCAPED_FOLDER, 9), (ESCAPED_FOLDER, 12)] {
        assert_eq!(fx.flags(folder, uid), ["\\Answered", "\\Seen"]);
    }
    assert!(
        fx.store
            .pending_flag_changes(&fx.account.id)
            .unwrap()
            .is_empty()
    );

    fx.clear_commands();
    fx.sync();
    assert!(fx.writes().is_empty(), "{:?}", fx.commands());
    assert_eq!(fx.local_flags(&alpha), (false, false));
}

#[test]
fn ten_star_toggles_coalesce_for_each_copy_and_keep_the_seen_edit() {
    let mut state = flag_state(true);
    for message in state
        .mailboxes
        .iter_mut()
        .flat_map(|folder| &mut folder.messages)
    {
        message.flags.push("\\Flagged".into());
    }
    let fx = Fixture::with(Store::open_in_memory().unwrap(), state);
    let alpha = fx.thread("INBOX", "Alpha");
    for _ in 0..5 {
        fx.flag(&alpha, FlagChange::Star);
        fx.flag(&alpha, FlagChange::Unstar);
    }
    fx.flag(&alpha, FlagChange::Read);
    assert_eq!(fx.store.outbox(&fx.account.id).unwrap().len(), 6);
    assert_eq!(fx.replay().sent, 6);
    let stores = fx.stores();
    assert_eq!(
        stores
            .iter()
            .filter(|cmd| cmd.ends_with("-FLAGS.SILENT (\\Flagged)"))
            .count(),
        3
    );
    for (folder, uid) in [("INBOX", 4), (ESCAPED_FOLDER, 9), (ESCAPED_FOLDER, 12)] {
        assert_eq!(fx.flags(folder, uid), ["\\Answered", "\\Seen"]);
    }
    assert!(
        fx.store
            .pending_flag_changes(&fx.account.id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn uidvalidity_reset_drops_only_that_folders_flag_writes_with_a_notice() {
    let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(true));
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Star);
    fx.server.state.lock().unwrap().mailboxes[1].uidvalidity += 1;
    let report = fx.replay_as_window();
    assert_eq!((report.sent, report.waiting), (1, 0));
    assert_eq!(report.dropped.len(), 2);
    assert!(
        report
            .dropped
            .iter()
            .all(|drop| drop.kind == OutboxKind::Flagged
                && drop.source_folder == ESCAPED_FOLDER
                && drop.reason == DropReason::FolderReset)
    );
    assert_eq!(
        fx.stores(),
        ["UID STORE 4 (UNCHANGEDSINCE 4) +FLAGS.SILENT (\\Flagged)"]
    );
    assert!(fx.replay_as_window().dropped.is_empty());
    assert_eq!(fx.local_flags(&alpha), (true, true));
}

fn write_back_db(label: &str) -> std::path::PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "fell-write-back-{label}-{}-{stamp}",
        std::process::id()
    ));
    std::fs::create_dir(&dir).unwrap();
    dir.join("mail.db")
}

#[test]
fn flag_queue_recovers_a_lost_reply_on_restart_without_repeating_store() {
    for condstore in [false, true] {
        let path = write_back_db("restart");
        let mut fx = Fixture::with(Store::open(&path).unwrap(), flag_state(condstore));
        let alpha = fx.thread("INBOX", "Alpha");
        fx.flag(&alpha, FlagChange::Read);
        fx.server.writes.lock().unwrap().lose_reply_to = Some("STORE");
        assert!(fx.replay().interrupted.is_some());
        assert_eq!(fx.stores().len(), 1);

        drop(fx.store);
        fx.store = Store::open(&path).unwrap();
        assert_eq!(fx.replay().waiting, 0);
        assert_eq!(fx.replay().sent, 0);
        let stores = fx.stores();
        assert_eq!(stores.len(), 3, "{stores:?}");
        for (folder, uid) in [("INBOX", 4), (ESCAPED_FOLDER, 9), (ESCAPED_FOLDER, 12)] {
            assert_eq!(fx.flags(folder, uid), ["\\Answered", "\\Seen"]);
        }
        drop(fx);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}

#[test]
fn an_unrelated_modseq_bump_rebases_either_flag() {
    for (change, target, other, modified_no) in [
        (FlagChange::Read, "\\Seen", "\\Flagged", false),
        (FlagChange::Star, "\\Flagged", "\\Seen", true),
    ] {
        let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(true));
        fx.server.writes.lock().unwrap().modified_no = modified_no;
        let alpha = fx.thread("INBOX", "Alpha");
        fx.flag(&alpha, change);
        {
            let mut state = fx.server.state.lock().unwrap();
            let copy = &mut state.mailboxes[1].messages[0];
            copy.modseq = 14;
            copy.flags.push(other.into());
        }
        let report = fx.replay();
        assert_eq!(
            (report.sent, report.waiting, report.flags_changed),
            (3, 0, false)
        );
        assert_eq!(
            fx.stores(),
            [
                format!("UID STORE 4 (UNCHANGEDSINCE 4) +FLAGS.SILENT ({target})"),
                format!("UID STORE 9 (UNCHANGEDSINCE 9) +FLAGS.SILENT ({target})"),
                format!("UID STORE 9 (UNCHANGEDSINCE 14) +FLAGS.SILENT ({target})"),
                format!("UID STORE 12 (UNCHANGEDSINCE 12) +FLAGS.SILENT ({target})"),
            ],
        );
        assert_eq!(fx.flags("INBOX", 4), ["\\Answered", target]);
        assert_eq!(fx.flags(ESCAPED_FOLDER, 9), ["\\Answered", other, target]);
        assert_eq!(fx.flags(ESCAPED_FOLDER, 12), ["\\Answered", target]);
        assert!(
            fx.store
                .pending_flag_changes(&fx.account.id)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn a_seen_conflict_only_rebases_a_later_star_with_compatible_flag_evidence() {
    for sibling_changed in [false, true] {
        let mut state = flag_state(true);
        let copy = &mut state.mailboxes[1].messages[0];
        copy.flags.push("\\Seen".into());
        if sibling_changed {
            copy.flags.push("\\Flagged".into());
        }
        let fx = Fixture::with(Store::open_in_memory().unwrap(), state);
        let alpha = fx.thread("INBOX", "Alpha");
        // An earlier local-only edit can leave the row different from a
        // server copy. Set that up without depending on the sync order.
        fx.store.set_write_back(&fx.account.id, false).unwrap();
        let (unread, starred) = fx.local_flags(&alpha);
        if !unread {
            fx.flag(&alpha, FlagChange::Unread);
        }
        if starred {
            fx.flag(&alpha, FlagChange::Unstar);
        }
        fx.store.set_write_back(&fx.account.id, true).unwrap();
        fx.flag(&alpha, FlagChange::Read);
        fx.flag(&alpha, FlagChange::Star);
        {
            let mut state = fx.server.state.lock().unwrap();
            let copy = &mut state.mailboxes[1].messages[0];
            copy.flags.truncate(1);
            copy.modseq = 14;
        }

        let report = fx.replay();
        assert_eq!(
            (report.sent, report.waiting, report.flags_changed),
            (if sibling_changed { 2 } else { 4 }, 0, true)
        );
        let mut expected = vec![
            "UID STORE 4 (UNCHANGEDSINCE 4) +FLAGS.SILENT (\\Seen)".to_string(),
            "UID STORE 9 (UNCHANGEDSINCE 9) +FLAGS.SILENT (\\Seen)".into(),
            "UID STORE 4 (UNCHANGEDSINCE 5) +FLAGS.SILENT (\\Flagged)".into(),
        ];
        let star_rev = if sibling_changed { 9 } else { 14 };
        expected.push(format!(
            "UID STORE 9 (UNCHANGEDSINCE {star_rev}) +FLAGS.SILENT (\\Flagged)"
        ));
        if !sibling_changed {
            expected.push("UID STORE 12 (UNCHANGEDSINCE 12) +FLAGS.SILENT (\\Flagged)".into());
        }
        assert_eq!(fx.stores(), expected);

        let untouched = if sibling_changed {
            vec!["\\Answered"]
        } else {
            vec!["\\Answered", "\\Flagged"]
        };
        assert_eq!(fx.flags("INBOX", 4), ["\\Answered", "\\Seen", "\\Flagged"]);
        assert_eq!(fx.flags(ESCAPED_FOLDER, 9), untouched);
        assert_eq!(fx.flags(ESCAPED_FOLDER, 12), untouched);
        // In the second case only the copy that already accepted the star
        // still has it. Bridge's any-copy star must stay on in Fell too.
        assert_eq!(fx.local_flags(&alpha), (true, true));
        assert!(
            fx.store
                .pending_flag_changes(&fx.account.id)
                .unwrap()
                .is_empty()
        );
        fx.sync();
        assert_eq!(fx.local_flags(&alpha), (true, true));
    }
}

#[test]
fn a_flagged_conflict_keeps_its_revision_for_the_next_toggle() {
    for untouched_starred in [false, true] {
        let mut state = flag_state(true);
        state.mailboxes[0].messages[0]
            .flags
            .push("\\Flagged".into());
        if untouched_starred {
            state.mailboxes[1].messages[1]
                .flags
                .push("\\Flagged".into());
        }
        let fx = Fixture::with(Store::open_in_memory().unwrap(), state);
        let alpha = fx.thread("INBOX", "Alpha");
        fx.flag(&alpha, FlagChange::Star);
        {
            let mut state = fx.server.state.lock().unwrap();
            let copy = &mut state.mailboxes[0].messages[0];
            copy.flags.pop();
            copy.modseq = 15;
        }
        let report = fx.replay();
        assert_eq!(
            (report.sent, report.waiting, report.flags_changed),
            (0, 0, !untouched_starred)
        );
        assert_eq!(fx.local_flags(&alpha), (true, untouched_starred));
        assert_eq!(fx.flags("INBOX", 4), ["\\Answered"]);
        assert_eq!(fx.flags(ESCAPED_FOLDER, 9), ["\\Answered"]);
        assert_eq!(
            fx.flags(ESCAPED_FOLDER, 12).contains(&"\\Flagged".into()),
            untouched_starred
        );
        assert_eq!(
            fx.stores(),
            ["UID STORE 4 (UNCHANGEDSINCE 4) +FLAGS.SILENT (\\Flagged)"]
        );

        fx.clear_commands();
        let (change, sign) = if untouched_starred {
            (FlagChange::Unstar, '-')
        } else {
            (FlagChange::Star, '+')
        };
        fx.flag(&alpha, change);
        assert_eq!(fx.replay().sent, 3);
        assert_eq!(
            fx.stores(),
            [
                format!("UID STORE 4 (UNCHANGEDSINCE 15) {sign}FLAGS.SILENT (\\Flagged)"),
                format!("UID STORE 9 (UNCHANGEDSINCE 9) {sign}FLAGS.SILENT (\\Flagged)"),
                format!("UID STORE 12 (UNCHANGEDSINCE 12) {sign}FLAGS.SILENT (\\Flagged)"),
            ]
        );
    }
}

#[test]
fn fetch_confirmed_stores_record_a_flagged_revision_and_rebase_seen() {
    let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(true));
    fx.server.writes.lock().unwrap().omit_store_modseq = true;
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Star);
    assert_eq!(fx.replay().sent, 3);
    fx.clear_commands();

    fx.flag(&alpha, FlagChange::Unstar);
    fx.flag(&alpha, FlagChange::Read);
    assert_eq!(fx.replay().sent, 6);
    assert_eq!(
        fx.stores(),
        [
            "UID STORE 4 (UNCHANGEDSINCE 5) -FLAGS.SILENT (\\Flagged)",
            "UID STORE 9 (UNCHANGEDSINCE 13) -FLAGS.SILENT (\\Flagged)",
            "UID STORE 12 (UNCHANGEDSINCE 14) -FLAGS.SILENT (\\Flagged)",
            "UID STORE 4 (UNCHANGEDSINCE 6) +FLAGS.SILENT (\\Seen)",
            "UID STORE 9 (UNCHANGEDSINCE 15) +FLAGS.SILENT (\\Seen)",
            "UID STORE 12 (UNCHANGEDSINCE 16) +FLAGS.SILENT (\\Seen)",
        ]
    );
}

#[test]
fn a_seen_change_first_observed_during_star_replay_can_still_sync() {
    let mut state = flag_state(true);
    state.mailboxes.truncate(1);
    let fx = Fixture::with(Store::open_in_memory().unwrap(), state);
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Star);
    {
        let mut state = fx.server.state.lock().unwrap();
        let copy = &mut state.mailboxes[0].messages[0];
        copy.flags.push("\\Seen".into());
        copy.modseq = 15;
    }
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(fx.local_flags(&alpha), (true, true));
    fx.clear_commands();
    fx.sync();
    assert!(fx.writes().is_empty());
    assert_eq!(fx.local_flags(&alpha), (false, true));
}

#[test]
fn an_edit_during_select_retires_the_unsent_snapshot() {
    let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(true));
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Star);
    let report = super::write_back::replay_with(
        &fx.backend.config,
        &fx.account.id,
        &fx.store,
        true,
        &|| {
            assert!(without_tags(&fx.commands()).contains(&"SELECT \"INBOX\" (CONDSTORE)".into()));
            fx.flag(&alpha, FlagChange::Unstar);
        },
    )
    .unwrap();

    assert_eq!((report.sent, report.waiting), (0, 0));
    assert!(fx.stores().is_empty());
    assert_eq!(fx.local_flags(&alpha), (true, false));
    assert_eq!(fx.replay().sent, 3);
    assert_eq!(fx.stores().len(), 3);
    assert!(
        fx.stores()
            .iter()
            .all(|store| store.ends_with("-FLAGS.SILENT (\\Flagged)"))
    );
}

#[test]
fn a_conflict_in_flight_keeps_a_newer_local_intent_even_with_the_same_value() {
    let server_state = flag_state(true);
    let path = write_back_db("in-flight");
    let fx = Fixture::with(Store::open(&path).unwrap(), server_state);
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Read);
    fx.server.state.lock().unwrap().mailboxes[0].messages[0].modseq = 15;
    let action_path = path.clone();
    let account = fx.account.id.clone();
    let thread = alpha.clone();
    fx.server.writes.lock().unwrap().before_store = Some(Box::new(move || {
        let store = Store::open(action_path).unwrap();
        for edit in [FlagChange::Unread, FlagChange::Read] {
            store
                .flag_threads(&account, std::slice::from_ref(&thread), "INBOX", edit)
                .unwrap()
                .unwrap();
        }
    }));
    assert!(!fx.replay().flags_changed);
    assert_eq!(fx.local_flags(&alpha), (false, false));
    let revision: [u8; 8] = fx
        .store
        .connection()
        .query_row(
            "SELECT modseq FROM message_copies WHERE uid = 4",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(u64::from_be_bytes(revision), 15);
    assert_eq!(fx.replay().sent, 3);
    assert!(
        fx.stores()
            .contains(&"UID STORE 4 (UNCHANGEDSINCE 15) +FLAGS.SILENT (\\Seen)".into())
    );
    assert_eq!(fx.flags("INBOX", 4), ["\\Answered", "\\Seen"]);
    drop(fx);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn a_copy_without_a_revision_fetches_a_baseline_before_writing() {
    let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(true));
    fx.store
        .connection()
        .execute(
            "UPDATE message_copies SET modseq = NULL, seen = NULL, flagged = NULL",
            [],
        )
        .unwrap();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Star);
    let changed_once = std::cell::Cell::new(false);
    let report = super::write_back::replay_with(
        &fx.backend.config,
        &fx.account.id,
        &fx.store,
        true,
        &|| {
            if !changed_once.replace(true) {
                fx.server.state.lock().unwrap().mailboxes[0].messages[0].modseq = 15;
            }
        },
    )
    .unwrap();
    assert_eq!((report.sent, report.waiting), (3, 0));
    assert_eq!(
        fx.stores(),
        [
            "UID STORE 4 (UNCHANGEDSINCE 4) +FLAGS.SILENT (\\Flagged)",
            "UID STORE 4 (UNCHANGEDSINCE 15) +FLAGS.SILENT (\\Flagged)",
            "UID STORE 9 (UNCHANGEDSINCE 9) +FLAGS.SILENT (\\Flagged)",
            "UID STORE 12 (UNCHANGEDSINCE 12) +FLAGS.SILENT (\\Flagged)",
        ]
    );
}

#[test]
fn an_old_revision_without_flag_bits_honors_the_first_conflicting_fetch() {
    let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(true));
    fx.store
        .connection()
        .execute("UPDATE message_copies SET flagged = NULL", [])
        .unwrap();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Star);
    fx.server.state.lock().unwrap().mailboxes[0].messages[0].modseq = 15;
    let report = fx.replay();
    assert_eq!(
        (report.sent, report.waiting, report.flags_changed),
        (0, 0, true)
    );
    assert_eq!(fx.local_flags(&alpha), (true, false));
    assert_eq!(
        fx.stores(),
        ["UID STORE 4 (UNCHANGEDSINCE 4) +FLAGS.SILENT (\\Flagged)"]
    );
    assert_eq!(fx.flags("INBOX", 4), ["\\Answered"]);
}

#[test]
fn a_fetch_missing_flag_bits_records_its_revision_and_waits() {
    let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(true));
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Star);
    fx.server.state.lock().unwrap().mailboxes[0].messages[0].modseq = 15;
    fx.server.writes.lock().unwrap().omit_fetch_flags = true;
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (2, 1));
    assert_eq!(fx.local_flags(&alpha), (true, true));
    let (revision, flagged): ([u8; 8], Option<bool>) = fx
        .store
        .connection()
        .query_row(
            "SELECT modseq, flagged FROM message_copies WHERE uid = 4",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((u64::from_be_bytes(revision), flagged), (15, None));

    fx.server.writes.lock().unwrap().omit_fetch_flags = false;
    fx.make_due();
    fx.clear_commands();
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(
        fx.stores()[0],
        "UID STORE 4 (UNCHANGEDSINCE 15) +FLAGS.SILENT (\\Flagged)"
    );
}

#[test]
fn an_unstar_survives_a_restart_after_the_older_star_reply_is_lost() {
    let path = write_back_db("superseded");
    let mut fx = Fixture::with(Store::open(&path).unwrap(), flag_state(true));
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Star);
    fx.server.writes.lock().unwrap().lose_reply_to = Some("STORE");
    assert!(fx.replay().interrupted.is_some());
    fx.flag(&alpha, FlagChange::Unstar);
    drop(fx.store);
    fx.store = Store::open(&path).unwrap();
    fx.server.writes.lock().unwrap().omit_fetch_flags = true;
    let max_attempts = super::write_back::MAX_ATTEMPTS;
    for attempt in 1..=max_attempts + 1 {
        fx.make_due();
        let report = fx.replay();
        assert_eq!(
            (report.sent, report.waiting),
            (if attempt == 1 { 2 } else { 0 }, 2)
        );
        assert_eq!(report.flag_recovery_delayed, attempt == max_attempts);
        assert_eq!(
            fx.stores()
                .iter()
                .filter(|command| command.starts_with("UID STORE 4 "))
                .count(),
            1,
            "the replacement on that copy waits for its baseline"
        );
        assert_eq!(fx.stores().len(), 3);
        assert_eq!(fx.local_flags(&alpha), (true, false));
    }
    fx.server.writes.lock().unwrap().omit_fetch_flags = false;
    fx.make_due();
    let report = fx.replay();
    assert_eq!(report.sent, 1);
    assert!(!report.flag_recovery_delayed);
    assert_eq!(fx.stores().len(), 4, "{:?}", fx.stores());
    assert!(
        fx.stores()
            .contains(&"UID STORE 4 (UNCHANGEDSINCE 5) -FLAGS.SILENT (\\Flagged)".into())
    );
    assert_eq!(fx.flags("INBOX", 4), ["\\Answered"]);
    assert_eq!(fx.local_flags(&alpha), (true, false));
    drop(fx);
    std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
}

#[test]
fn a_refused_store_leaves_other_copies_and_a_later_move_free() {
    for move_already_queued in [false, true] {
        let mut state = flag_state(true);
        state
            .mailboxes
            .push(mailbox("Archive", "\\Archive", 30, &[]));
        let fx = Fixture::with(Store::open_in_memory().unwrap(), state);
        let alpha = fx.thread("INBOX", "Alpha");
        let archive = || {
            fx.store
                .archive_thread(&fx.account.id, &alpha, "INBOX")
                .unwrap()
        };
        fx.flag(&alpha, FlagChange::Star);
        fx.server.writes.lock().unwrap().refuse_store_uid = Some(4);
        if move_already_queued {
            archive();
        }

        let report = fx.replay();
        assert_eq!(
            (report.sent, report.waiting),
            (2 + usize::from(move_already_queued), 1)
        );
        assert_eq!(fx.flags(ESCAPED_FOLDER, 9), ["\\Answered", "\\Flagged"]);
        assert_eq!(fx.flags(ESCAPED_FOLDER, 12), ["\\Answered", "\\Flagged"]);
        if !move_already_queued {
            archive();
            fx.clear_commands();
            let report = fx.replay();
            assert_eq!((report.sent, report.waiting), (1, 1));
            assert!(fx.stores().is_empty());
        }
        assert_eq!(fx.on_server("Archive"), [(1, "Alpha".into())]);
        assert_eq!(fx.flags("Archive", 1), ["\\Answered"]);
        let rows = fx.store.outbox(&fx.account.id).unwrap();
        let pending = rows
            .iter()
            .find(|row| row.state == OutboxState::Pending)
            .unwrap();
        assert_eq!(
            (pending.source_mailbox.as_deref(), pending.uid, pending.sent),
            (Some("Archive"), Some(1), false)
        );

        fx.make_due();
        assert_eq!(fx.replay().sent, 1);
        assert_eq!(fx.flags("Archive", 1), ["\\Answered", "\\Flagged"]);
    }
}

#[test]
fn an_unconfirmed_store_holds_its_move_but_allows_other_copies_to_write() {
    let mut state = flag_state(true);
    state
        .mailboxes
        .push(mailbox("Archive", "\\Archive", 30, &[]));
    let fx = Fixture::with(Store::open_in_memory().unwrap(), state);
    let alpha = fx.thread("INBOX", "Alpha");
    fx.flag(&alpha, FlagChange::Star);
    fx.server.writes.lock().unwrap().lose_reply_to = Some("STORE");
    assert!(fx.replay().interrupted.is_some());
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.flag(&alpha, FlagChange::Unstar);

    fx.server.writes.lock().unwrap().omit_fetch_flags = true;
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (2, 3));
    assert_eq!(fx.flags("INBOX", 4), ["\\Answered", "\\Flagged"]);
    assert!(fx.on_server("Archive").is_empty());

    fx.clear_commands();
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (0, 3));
    assert!(fx.stores().is_empty());
    assert!(fx.on_server("Archive").is_empty());

    fx.server.writes.lock().unwrap().omit_fetch_flags = false;
    fx.make_due();
    let report = fx.replay();
    assert_eq!((report.sent, report.waiting), (2, 0));
    assert_eq!(fx.flags("Archive", 1), ["\\Answered"]);
    assert!(fx.on_server("INBOX").is_empty());
}

#[test]
fn a_conflict_includes_flags_kept_by_an_earlier_move() {
    let mut state = state_with(vec![
        mailbox("INBOX", "", 7, &[(4, "Alpha")]),
        mailbox("All Mail", "\\All", 21, &[(9, "Alpha")]),
        mailbox("Archive", "\\Archive", 30, &[]),
    ]);
    state.condstore = true;
    for copy in state
        .mailboxes
        .iter_mut()
        .flat_map(|mailbox| &mut mailbox.messages)
    {
        copy.flags.push("\\Flagged".into());
        copy.modseq = u64::from(copy.uid);
    }
    let fx = Fixture::with(Store::open_in_memory().unwrap(), state);
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.store
        .flag_threads(
            &fx.account.id,
            std::slice::from_ref(&alpha),
            "Archive",
            FlagChange::Star,
        )
        .unwrap()
        .unwrap();
    {
        let mut server = fx.server.state.lock().unwrap();
        let all_mail = &mut server.mailboxes[1].messages[0];
        all_mail.flags.retain(|flag| flag != "\\Flagged");
        all_mail.modseq = 15;
    }

    let report = fx.replay();
    assert_eq!(
        (report.sent, report.waiting, report.flags_changed),
        (1, 0, false)
    );
    assert_eq!(
        fx.stores(),
        ["UID STORE 9 (UNCHANGEDSINCE 9) +FLAGS.SILENT (\\Flagged)"]
    );
    assert!(fx.on_server("INBOX").is_empty());
    assert_eq!(fx.flags("Archive", 1), ["\\Seen", "\\Flagged"]);
    assert_eq!(fx.local_flags(&alpha), (false, true));
    let (seen, flagged, revision): (bool, bool, Option<Vec<u8>>) = fx
        .store
        .connection()
        .query_row(
            "SELECT seen, flagged, modseq FROM message_copies WHERE mailbox = 'Archive'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((seen, flagged, revision), (true, true, None));
}

#[test]
fn flags_follow_a_queued_move_and_undo_uses_the_same_queue() {
    let fx = Fixture::new();
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store
        .archive_thread(&fx.account.id, &alpha, "INBOX")
        .unwrap();
    fx.flag(&alpha, FlagChange::Unread);
    assert_eq!(fx.replay().sent, 2);
    assert_eq!(fx.on_server("Archive"), [(1, "Alpha".into())]);
    assert!(fx.flags("Archive", 1).is_empty());
    fx.store.undo(&fx.account.id).unwrap();
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(fx.flags("Archive", 1), ["\\Seen"]);
    assert_eq!(
        fx.stores(),
        [
            "UID STORE 1 -FLAGS.SILENT (\\Seen)",
            "UID STORE 1 +FLAGS.SILENT (\\Seen)"
        ]
    );
}

#[test]
fn the_write_back_switch_gates_flags_and_nomodseq_uses_plain_store() {
    let fx = Fixture::with(Store::open_in_memory().unwrap(), flag_state(true));
    let alpha = fx.thread("INBOX", "Alpha");
    fx.store.set_write_back(&fx.account.id, false).unwrap();
    fx.flag(&alpha, FlagChange::Star);
    assert!(fx.store.outbox(&fx.account.id).unwrap().is_empty());
    fx.store.set_write_back(&fx.account.id, true).unwrap();
    fx.flag(&alpha, FlagChange::Unstar);
    fx.store.set_write_back(&fx.account.id, false).unwrap();
    assert_eq!(fx.replay_as_window().sent, 0);
    fx.store.set_write_back(&fx.account.id, true).unwrap();
    assert_eq!(fx.replay_as_window().sent, 0);
    assert!(fx.stores().is_empty());

    fx.server.writes.lock().unwrap().nomodseq = true;
    fx.flag(&alpha, FlagChange::Read);
    fx.flag(&alpha, FlagChange::Star);
    assert_eq!(fx.replay_as_window().sent, 6);
    assert!(
        fx.stores()
            .iter()
            .all(|cmd| !cmd.contains("UNCHANGEDSINCE"))
    );
    let (modseq, seen, flagged, seen_handled): ([u8; 8], bool, bool, bool) = fx
        .store
        .connection()
        .query_row(
            "SELECT modseq, seen, flagged, seen_handled FROM message_copies WHERE uid = 4",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (u64::from_be_bytes(modseq), seen, flagged, seen_handled),
        (4, true, true, true)
    );
}

fn sent_fixture() -> Fixture {
    Fixture::with(
        Store::open_in_memory().unwrap(),
        state_with(vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha")]),
            mailbox("Filed mail", "\\Sent", 65, &[]),
        ]),
    )
}

const REPLY_MIME: &[u8] = b"From: Robin <c@x>\r\nTo: Ann <ann@x>\r\n\
Subject: Re: Alpha\r\nDate: Sat, 21 Nov 2026 10:00:00 +0000\r\n\
Message-ID: <reply@compose.fell>\r\nIn-Reply-To: <alpha@x>\r\n\
Content-Type: text/plain; charset=utf-8\r\n\r\nThanks for the update.\r\n";

fn record_reply(fixture: &Fixture, append: bool) -> ThreadId {
    let (thread, _) = fixture
        .store
        .record_sent_message(
            &fixture.account.id,
            "fixture-reply",
            &crate::MessageId::new("reply@compose.fell"),
            &[crate::Address {
                name: None,
                email: "hidden@example.com".into(),
            }],
            REPLY_MIME,
            append,
        )
        .unwrap();
    thread
}

#[test]
fn sent_reply_is_inline_before_sync_and_dedups_with_bcc_after_sync() {
    let fixture = sent_fixture();
    let parent = fixture.thread("INBOX", "Alpha");
    let thread = record_reply(&fixture, false);
    assert_eq!(thread, parent);
    let check = || {
        let messages = fixture
            .store
            .thread_messages(&fixture.account.id, &thread)
            .unwrap();
        assert_eq!(messages.len(), 2);
        let reply = messages
            .iter()
            .find(|message| message.headers.message_id.as_str() == "reply@compose.fell")
            .unwrap();
        assert_eq!(reply.folders, ["Filed mail"]);
        assert_eq!(
            reply.body_text.as_deref().unwrap().trim(),
            "Thanks for the update."
        );
        assert_eq!(reply.bcc[0].email, "hidden@example.com");
        assert!(!reply.unread);
    };
    check();
    // A new empty listing must keep the local message, even across UID reset.
    fixture.server.state.lock().unwrap().mailboxes[1].uidvalidity += 1;
    fixture.sync();
    check();
    fixture.deliver("Filed mail", 1, REPLY_MIME.to_vec());
    fixture.server.state.lock().unwrap().mailboxes[1].messages[0].flags = vec!["\\Seen".into()];
    fixture.sync();
    check();
    assert!(fixture.copy_recorded("Filed mail", 1));
    assert!(
        fixture
            .store
            .search(&fixture.account.id, "hidden")
            .unwrap()
            .hits
            .is_empty()
    );
}

#[test]
fn sent_append_recovers_a_lost_reply_without_a_second_append() {
    let fixture = sent_fixture();
    record_reply(&fixture, true);
    {
        let mut writes = fixture.server.writes.lock().unwrap();
        writes.lose_reply_to = Some("APPEND");
        // The first HEADER search succeeds but omits the stored copy.
        writes.empty_header_searches = 1;
    }
    let report = fixture.replay();
    assert!(report.sent_append_delayed);
    assert_eq!(report.waiting, 1);
    fixture.make_due();
    let report = fixture.replay();
    assert_eq!(report.sent, 1);
    assert!(!report.sent_append_delayed);
    assert_eq!(fixture.replay().sent, 0);
    assert_eq!(fixture.on_server("Filed mail").len(), 1);
    let state = fixture.server.state.lock().unwrap();
    let appended = &state.mailboxes[1].messages[0];
    assert_eq!(appended.flags, ["\\Seen"]);
    assert_eq!(appended.raw, REPLY_MIME);
    assert_eq!(
        state
            .commands
            .iter()
            .filter(|command| command.contains(" APPEND "))
            .count(),
        1
    );
}

#[test]
fn append_connection_failures_do_not_use_the_refusal_budget() {
    let fixture = sent_fixture();
    record_reply(&fixture, true);
    fixture.server.writes.lock().unwrap().close_on_login = true;
    for attempt in 0..=super::write_back::MAX_ATTEMPTS {
        fixture.make_due();
        let report = fixture.replay();
        assert!(report.interrupted.is_some());
        assert_eq!(report.sent_append_delayed, attempt == 0);
        assert!(report.dropped.is_empty());
        assert_eq!(report.waiting, 1);
        let row = &fixture.store.outbox(&fixture.account.id).unwrap()[0];
        assert_eq!(row.state, OutboxState::Pending);
        assert_eq!(row.attempts, 0);
        assert!(!row.sent);
        assert!(row.retry_unix > crate::outbox::now_unix());
    }
    {
        let mut writes = fixture.server.writes.lock().unwrap();
        writes.close_on_login = false;
        writes.lose_reply_to = Some("APPEND");
    }
    fixture.make_due();
    assert!(fixture.replay().interrupted.is_some());
    for _ in 0..=super::write_back::MAX_ATTEMPTS {
        fixture.server.writes.lock().unwrap().lose_reply_to = Some("SEARCH");
        fixture.make_due();
        let report = fixture.replay();
        assert!(report.interrupted.is_some());
        assert!(report.dropped.is_empty());
        assert_eq!(report.waiting, 1);
        let row = &fixture.store.outbox(&fixture.account.id).unwrap()[0];
        assert_eq!(row.attempts, 0);
        assert!(row.sent);
    }
    fixture.make_due();
    assert_eq!(fixture.replay().sent, 1);
    assert_eq!(fixture.on_server("Filed mail").len(), 1);
}

fn lose_sent_append(fixture: &Fixture) {
    record_reply(fixture, true);
    fixture
        .server
        .writes
        .lock()
        .unwrap()
        .lose_append_before_commit = true;
    assert!(fixture.replay().interrupted.is_some());
    fixture.make_due();
}

#[test]
fn sent_append_is_reissued_with_unchanged_uidnext() {
    let fixture = sent_fixture();
    lose_sent_append(&fixture);
    assert_eq!(fixture.replay().sent, 1);
    assert_eq!(fixture.on_server("Filed mail").len(), 1);
}

#[test]
fn sent_append_waits_for_uidnext_then_retries_an_interrupted_append() {
    let fixture = sent_fixture();
    record_reply(&fixture, true);
    fixture.server.state.lock().unwrap().mailboxes[1].omit_uidnext = true;
    fixture
        .server
        .writes
        .lock()
        .unwrap()
        .lose_append_before_commit = true;
    let append_commands = || {
        fixture
            .commands()
            .iter()
            .filter(|command| command.contains(" APPEND "))
            .count()
    };

    let report = fixture.replay();
    assert!(report.sent_append_delayed);
    assert_eq!(report.waiting, 1);
    assert!(report.interrupted.is_none());
    let row = &fixture.store.outbox(&fixture.account.id).unwrap()[0];
    assert_eq!(row.state, OutboxState::Pending);
    assert!(!row.sent);
    assert_eq!(row.attempts, 0);
    assert!(row.retry_unix > crate::outbox::now_unix());
    assert_eq!(append_commands(), 0);

    fixture.server.state.lock().unwrap().mailboxes[1].omit_uidnext = false;
    fixture.make_due();
    assert!(fixture.replay().interrupted.is_some());
    assert!(fixture.store.outbox(&fixture.account.id).unwrap()[0].sent);
    assert!(fixture.on_server("Filed mail").is_empty());

    fixture.make_due();
    assert_eq!(fixture.replay().sent, 1);
    assert_eq!(fixture.on_server("Filed mail").len(), 1);
    assert_eq!(append_commands(), 2);
}

#[test]
fn sent_append_rechecks_the_new_range_and_records_a_fresh_bound() {
    let fixture = sent_fixture();
    lose_sent_append(&fixture);
    fixture.deliver(
        "Filed mail",
        3,
        b"Subject: Unrelated\r\nMessage-ID: <other@x>\r\n\r\n".to_vec(),
    );
    fixture.server.writes.lock().unwrap().lose_reply_to = Some("APPEND");
    assert!(fixture.replay().interrupted.is_some());
    let row = &fixture.store.outbox(&fixture.account.id).unwrap()[0];
    fixture.make_due();
    let pending = crate::outbox::pending_append(&fixture.store, row)
        .unwrap()
        .unwrap();
    assert_eq!((pending.validity, pending.next, pending.checks), (65, 4, 0));
    assert_eq!(
        fixture.on_server("Filed mail"),
        [(3, "Unrelated".into()), (4, "Re: Alpha".into())]
    );
    let commands = without_tags(&fixture.commands());
    assert!(
        commands
            .iter()
            .any(|command| command == "UID SEARCH UID 1:3")
    );
    assert!(
        commands
            .iter()
            .any(|command| command.starts_with("UID FETCH 3 "))
    );
    assert_eq!(fixture.replay().sent, 1);
    assert_eq!(
        fixture.on_server("Filed mail"),
        [(3, "Unrelated".into()), (4, "Re: Alpha".into())]
    );
}

#[test]
fn sent_append_is_reissued_after_a_uidvalidity_reset() {
    let fixture = sent_fixture();
    lose_sent_append(&fixture);
    fixture.server.state.lock().unwrap().mailboxes[1].uidvalidity += 1;
    assert_eq!(fixture.replay().sent, 1);
    assert_eq!(fixture.on_server("Filed mail").len(), 1);
}

#[test]
fn a_missing_uidnext_drops_an_unconfirmed_append() {
    let fixture = sent_fixture();
    lose_sent_append(&fixture);
    fixture.server.state.lock().unwrap().mailboxes[1].omit_uidnext = true;
    assert_unconfirmed_append_drops(&fixture);
    assert!(fixture.on_server("Filed mail").is_empty());
}

#[test]
fn search_hits_without_an_exact_message_id_drop_an_unconfirmed_append() {
    let fixture = sent_fixture();
    lose_sent_append(&fixture);
    fixture.deliver(
        "Filed mail",
        1,
        b"Subject: Unrelated\r\nMessage-ID: <REPLY@compose.fell>\r\n\r\n".to_vec(),
    );
    assert_unconfirmed_append_drops(&fixture);
    assert_eq!(fixture.on_server("Filed mail").len(), 1);
}

#[test]
fn a_range_with_missing_headers_does_not_repeat_append() {
    let fixture = sent_fixture();
    lose_sent_append(&fixture);
    fixture.deliver(
        "Filed mail",
        1,
        b"Subject: Unrelated\r\nMessage-ID: <other@x>\r\n\r\n".to_vec(),
    );
    fixture.server.writes.lock().unwrap().omit_message_id_header = Some(1);
    assert_unconfirmed_append_drops(&fixture);
    assert_eq!(fixture.on_server("Filed mail").len(), 1);
}

#[test]
fn a_huge_uid_span_drops_an_unconfirmed_append_without_searching_the_range() {
    let fixture = sent_fixture();
    lose_sent_append(&fixture);
    fixture.server.state.lock().unwrap().mailboxes[1].uidnext = Some(u32::MAX);
    let before = fixture.commands().len();
    assert_unconfirmed_append_drops(&fixture);
    assert!(
        without_tags(&fixture.commands()[before..])
            .iter()
            .all(|command| !command.starts_with("UID SEARCH UID "))
    );
}

#[test]
fn too_many_search_hits_drop_an_unconfirmed_append_without_fetching_headers() {
    let fixture = sent_fixture();
    lose_sent_append(&fixture);
    fixture.server.state.lock().unwrap().mailboxes[1].uidnext = Some(2);
    fixture.server.writes.lock().unwrap().range_search_hits =
        Some(super::write_back::APPEND_UID_LIMIT + 1);
    let before = fixture.commands().len();
    assert_unconfirmed_append_drops(&fixture);
    let commands = without_tags(&fixture.commands()[before..]);
    assert!(
        commands
            .iter()
            .any(|command| command == "UID SEARCH UID 1:1")
    );
    assert!(
        commands
            .iter()
            .all(|command| !command.starts_with("UID FETCH "))
    );
}

#[test]
fn an_oversized_search_line_counts_as_a_check_and_disconnects() {
    let fixture = sent_fixture();
    lose_sent_append(&fixture);
    fixture.server.state.lock().unwrap().mailboxes[1].uidnext = Some(2);
    // Each UID adds two bytes, so the line exceeds the real 1 MiB cap.
    fixture.server.writes.lock().unwrap().range_search_hits = Some(512 * 1024);
    let before = fixture.commands().len();
    assert_unconfirmed_append_drops(&fixture);
    let recorded = fixture.commands();
    let commands = without_tags(&recorded[before..]);
    let check_count = super::write_back::MAX_APPEND_CHECKS as usize;
    assert_eq!(
        recorded[before..]
            .iter()
            .filter(|command| command.starts_with("LOGIN"))
            .count(),
        check_count,
    );
    assert_eq!(
        commands
            .iter()
            .filter(|command| *command == "UID SEARCH UID 1:1")
            .count(),
        check_count,
    );
    assert!(commands.iter().all(|command| command != "LOGOUT"));
}

fn assert_unconfirmed_append_drops(fixture: &Fixture) {
    for check in 1..=super::write_back::MAX_APPEND_CHECKS {
        fixture.make_due();
        let report = fixture.replay();
        let row = &fixture.store.outbox(&fixture.account.id).unwrap()[0];
        assert_eq!(row.attempts, 0);
        if check < super::write_back::MAX_APPEND_CHECKS {
            assert_eq!(report.waiting, 1);
            assert!(report.dropped.is_empty());
            assert_eq!(row.state, OutboxState::Pending);
            fixture.make_due();
            let pending = crate::outbox::pending_append(&fixture.store, row)
                .unwrap()
                .unwrap();
            assert_eq!(pending.checks, check);
        } else {
            assert_eq!(report.waiting, 0);
            assert_eq!(report.dropped[0].kind, OutboxKind::AppendSent);
            assert_eq!(report.dropped[0].reason, DropReason::UnconfirmedAppend);
            assert_eq!(row.state, OutboxState::Dropped);
        }
    }
    assert_eq!(
        fixture
            .commands()
            .iter()
            .filter(|command| command.contains(" APPEND "))
            .count(),
        1
    );
}

#[test]
fn sent_copy_survives_a_write_lock_past_both_old_save_attempts() {
    let database = write_back_db("sent-write-lock");
    let fixture = Fixture::with(
        Store::open(&database).unwrap(),
        state_with(vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha")]),
            mailbox("Filed mail", "\\Sent", 65, &[]),
        ]),
    );
    let parent = fixture.thread("INBOX", "Alpha");
    fixture
        .store
        .connection()
        .busy_timeout(std::time::Duration::from_millis(20))
        .unwrap();
    let writer = rusqlite::Connection::open(database).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    // Establish that the same connection used for saving cannot take the
    // write lock. Keep the lock past the former local/queue-only timeouts.
    assert!(crate::store::immediate_transaction(fixture.store.connection()).is_err());
    let writer = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(200));
        writer.execute_batch("COMMIT").unwrap();
    });
    let thread = record_reply(&fixture, true);
    writer.join().unwrap();
    assert_eq!(thread, parent);
    assert_eq!(fixture.store.outbox(&fixture.account.id).unwrap().len(), 1);
    assert_eq!(fixture.replay().sent, 1);
    assert_eq!(fixture.on_server("Filed mail").len(), 1);
}

#[test]
fn sent_copy_is_appended_after_smtp_even_if_sqlite_refuses_the_local_insert() {
    let fixture = sent_fixture();
    let account = &fixture.account.id;
    let id = crate::MessageId::new("reply@compose.fell");
    fixture
        .store
        .claim_compose_send(account, "fixture-reply", Some(id.as_str()))
        .unwrap();
    crate::smtp::tests::confirm_fixture_delivery(&fixture.backend.config, account, REPLY_MIME)
        .unwrap();
    fixture
        .store
        .connection()
        .execute_batch(
            "CREATE TEMP TRIGGER reject_local_reply BEFORE INSERT ON messages
             WHEN NEW.message_id = 'reply@compose.fell' AND NEW.remote_id = ''
             BEGIN SELECT RAISE(ABORT, 'local insert failed'); END;",
        )
        .unwrap();
    assert!(
        fixture
            .store
            .record_sent_message(account, "fixture-reply", &id, &[], REPLY_MIME, true)
            .unwrap_err()
            .to_string()
            .contains("local insert failed")
    );
    let thread = fixture.thread("INBOX", "Alpha");
    assert_eq!(
        fixture
            .store
            .thread_messages(account, &thread)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(fixture.replay().sent, 1);
    {
        let state = fixture.server.state.lock().unwrap();
        assert_eq!(state.mailboxes[1].messages[0].raw, REPLY_MIME);
    }
    fixture.sync();
    assert!(fixture.copy_recorded("Filed mail", 1));
    assert_eq!(
        fixture
            .store
            .thread_messages(account, &thread)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn sent_mime_with_bcc_is_not_queued_on_local_save_failure() {
    let fixture = sent_fixture();
    let mime = [b"Bcc:\r\n".as_slice(), REPLY_MIME].concat();
    assert!(
        fixture
            .store
            .record_sent_message(
                &fixture.account.id,
                "fixture-reply",
                &crate::MessageId::new("reply@compose.fell"),
                &[],
                &mime,
                true,
            )
            .is_err()
    );
    assert!(
        fixture
            .store
            .outbox(&fixture.account.id)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn refused_sent_append_leaves_local_reply_and_retries() {
    let fixture = sent_fixture();
    let message_id = crate::MessageId::new("reply@compose.fell");
    assert_eq!(
        fixture
            .store
            .claim_compose_send(
                &fixture.account.id,
                "fixture-reply",
                Some(message_id.as_str())
            )
            .unwrap(),
        None
    );
    let sent = crate::smtp::tests::confirm_fixture_delivery(
        &fixture.backend.config,
        &fixture.account.id,
        REPLY_MIME,
    );
    fixture.server.writes.lock().unwrap().refuse_append = true;
    let thread = record_reply(&fixture, true);
    let report = fixture.replay();
    assert!(report.sent_append_delayed);
    assert_eq!(report.waiting, 1);
    assert!(fixture.on_server("Filed mail").is_empty());
    let rows = fixture.store.outbox(&fixture.account.id).unwrap();
    assert_eq!(rows[0].state, OutboxState::Pending);
    assert_eq!(rows[0].attempts, 1);
    assert!(sent.is_ok());
    assert_eq!(
        fixture
            .store
            .load_compose_send(&fixture.account.id, "fixture-reply")
            .unwrap(),
        Some(crate::compose::SendLock::Sent)
    );
    assert_eq!(
        fixture
            .store
            .thread_messages(&fixture.account.id, &thread)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(fixture.replay().sent, 0);

    fixture.server.writes.lock().unwrap().refuse_append = false;
    fixture.make_due();
    assert_eq!(fixture.replay().sent, 1);
    assert_eq!(fixture.replay().sent, 0);
    assert_eq!(fixture.on_server("Filed mail").len(), 1);
}

#[test]
fn server_mode_and_disabled_write_back_keep_only_the_local_sent_row() {
    let mut bridge = crate::AccountConfig::bridge(
        "c@x",
        "demo",
        crate::PasswordSource::Inline("fixture".into()),
    );
    bridge.write_back = true;
    for mode in [
        crate::SaveSent::Auto,
        crate::SaveSent::Server,
        crate::SaveSent::Append,
    ] {
        let fixture = sent_fixture();
        bridge.save_sent = mode;
        if mode == crate::SaveSent::Append {
            fixture
                .store
                .set_write_back(&fixture.account.id, false)
                .unwrap();
        }
        let thread = record_reply(&fixture, bridge.append_sent());
        assert_eq!(
            fixture
                .store
                .thread_messages(&fixture.account.id, &thread)
                .unwrap()
                .len(),
            2
        );
        assert!(
            fixture
                .store
                .outbox(&fixture.account.id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(fixture.replay().sent, 0);
        assert!(
            fixture
                .commands()
                .iter()
                .all(|command| !command.contains(" APPEND "))
        );
    }
}

const SERVER_DRAFTS: &str = "Work in progress";

fn draft_fixture(store: Store) -> Fixture {
    Fixture::with(
        store,
        state_with(vec![
            mailbox("INBOX", "", 7, &[(1, "Alpha")]),
            mailbox(SERVER_DRAFTS, "\\Drafts", 64, &[]),
            mailbox("Filed mail", "\\Sent", 65, &[]),
        ]),
    )
}

fn draft_appends(fixture: &Fixture) -> usize {
    fixture
        .commands()
        .iter()
        .filter(|command| command.contains(" APPEND "))
        .count()
}

fn draft_hub(home: &std::path::Path, fixture: &Fixture) -> crate::compose::DraftHub {
    use crate::compose::{DraftFields, DraftHub, DraftSession};
    let db = fixture.store.db_path().unwrap();
    let account = fixture.account.id.clone();
    let folder = DraftSession::start_in(
        home,
        "draft-test",
        account.clone(),
        DraftFields {
            from: fixture.account.email.clone(),
            to: vec!["Avery <avery@example.test>".into()],
            bcc: vec!["Taylor <taylor@example.test>".into()],
            subject: "Weekly notes".into(),
            ..DraftFields::default()
        },
        "",
        db,
    )
    .unwrap();
    let mut session = DraftSession::open_with_store(&folder, account, &fixture.store).unwrap();
    session.bind_store(db);
    let hub = DraftHub::new();
    hub.on_server_draft_queued(|| {});
    hub.insert(session);
    hub
}

fn wait_for_draft(hub: &crate::compose::DraftHub) {
    hub.with_session("draft-test", |session| session.save_waiter())
        .flatten()
        .unwrap()
        .wait()
        .unwrap();
}

fn set_draft_body(hub: &crate::compose::DraftHub, body: String) {
    hub.with_session("draft-test", |session| session.set_body_from_user(body))
        .unwrap();
}

#[test]
fn server_draft_debounce_close_and_sync_deduplicate() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let hub = draft_hub(&dir.path().join("drafts"), &fx);
    // An agent can request an editor reveal without authorizing IMAP.
    hub.with_session("draft-test", |session| session.request_send())
        .unwrap();
    hub.note_shown("draft-test");
    hub.poll_server_draft("draft-test", 0);
    hub.poll_server_draft("draft-test", 6000);
    wait_for_draft(&hub);
    assert_eq!(fx.replay().sent, 0);
    assert!(hub.show("draft-test"));

    for edit in 0..20 {
        set_draft_body(&hub, format!("# Notes\n\nEdit {edit}"));
        hub.poll_server_draft("draft-test", 7000 + edit * 250);
        assert_eq!(fx.replay().sent, 0);
    }
    hub.poll_server_draft("draft-test", 17_000);
    wait_for_draft(&hub);
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(fx.on_server(SERVER_DRAFTS), [(1, "Weekly notes".into())]);
    assert_eq!(fx.flags(SERVER_DRAFTS, 1), ["\\Draft", "\\Seen"]);
    fx.sync();
    assert!(fx.subjects(SERVER_DRAFTS).is_empty());
    assert_eq!(
        fx.store
            .unsent_compose_drafts(&fx.account.id)
            .unwrap()
            .len(),
        1
    );

    set_draft_body(&hub, "Edited again before close".into());
    hub.note_hidden("draft-test");
    wait_for_draft(&hub);
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(fx.on_server(SERVER_DRAFTS), [(2, "Weekly notes".into())]);
    assert_eq!(draft_appends(&fx), 2);
    assert!(
        fx.commands()
            .iter()
            .any(|cmd| cmd.ends_with("UID EXPUNGE 1"))
    );
    assert!(!fx.commands().iter().any(|cmd| cmd.ends_with(" EXPUNGE")));
}

#[test]
fn server_draft_import_round_trips_and_confirmed_send_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let mut config = fx.account_config();
    config.name = "Caleb".into();
    let mime = b"From: Robin <c@x>\r\nTo: Avery <avery@example.test>\r\n\
        Cc: Blake <blake@example.test>\r\nBcc: Taylor <taylor@example.test>\r\n\
        Subject: Field notes\r\nMessage-ID: <remote-draft@example.test>\r\n\
        MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=notes\r\n\r\n\
        --notes\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n# Field notes\r\n\r\nTwo routes.\r\n\
        --notes\r\nContent-Type: application/octet-stream\r\n\
        Content-Disposition: attachment; filename=map.txt\r\n\
        Content-Transfer-Encoding: base64\r\n\r\nbWFw\r\n--notes--\r\n";
    fx.deliver(SERVER_DRAFTS, 1, mime.to_vec());
    fx.sync();
    let thread = fx.thread(SERVER_DRAFTS, "Field notes");
    let id = config
        .import_server_draft(&fx.store, &thread, "imported-draft")
        .unwrap();
    let stored = fx.store.load_compose(&id).unwrap().unwrap();
    assert_eq!(stored.fields.from, "Caleb <c@x>");
    assert_eq!(
        stored.identities.as_slice(),
        std::slice::from_ref(&fx.account.email)
    );
    assert_eq!(stored.fields.to, ["Avery <avery@example.test>"]);
    assert_eq!(stored.fields.cc, ["Blake <blake@example.test>"]);
    assert_eq!(stored.fields.bcc, ["Taylor <taylor@example.test>"]);
    assert_eq!(stored.fields.subject, "Field notes");
    assert_eq!(stored.body.trim_end(), "# Field notes\n\nTwo routes.");
    assert_eq!(stored.files[0].display_name, "map.txt");
    assert_eq!(stored.files[0].bytes, b"map");
    assert!(fx.subjects(SERVER_DRAFTS).is_empty());
    fx.sync();
    assert!(fx.subjects(SERVER_DRAFTS).is_empty());

    // Exercise the same confirmation transaction as the editor after SMTP.
    fx.store
        .record_sent_message(
            &fx.account.id,
            &id,
            &crate::MessageId::new("reply@compose.fell"),
            &[],
            REPLY_MIME,
            false,
        )
        .unwrap();
    assert_eq!(fx.replay().sent, 1);
    assert!(fx.on_server(SERVER_DRAFTS).is_empty());
    assert_eq!(draft_appends(&fx), 0);
}

#[test]
fn server_draft_recovers_a_crash_before_cleanup_and_a_lost_append_reply() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let hub = draft_hub(&dir.path().join("drafts"), &fx);
    let queue = |body: &str| {
        assert!(hub.show("draft-test"));
        hub.note_shown("draft-test");
        set_draft_body(&hub, body.into());
        hub.note_hidden("draft-test");
        wait_for_draft(&hub);
    };
    queue("Original");
    assert_eq!(fx.replay().sent, 1);

    queue("Replacement");
    fx.server.writes.lock().unwrap().lose_before_store = true;
    assert!(fx.replay().interrupted.is_some());
    assert_eq!(fx.on_server(SERVER_DRAFTS).len(), 2);
    let pending = fx.store.outbox(&fx.account.id).unwrap().pop().unwrap();
    assert_eq!(pending.placed_uid, Some(2));
    fx.sync();
    assert!(fx.subjects(SERVER_DRAFTS).is_empty());
    queue("Saved after restart");
    fx.make_due();
    assert_eq!(fx.replay().sent, 2);
    assert_eq!(fx.on_server(SERVER_DRAFTS), [(3, "Weekly notes".into())]);
    assert_eq!(draft_appends(&fx), 3);

    queue("Saved with a missing APPEND reply");
    {
        let mut writes = fx.server.writes.lock().unwrap();
        writes.lose_reply_to = Some("APPEND");
        writes.empty_header_searches = 1;
    }
    assert!(fx.replay().interrupted.is_some());
    assert_eq!(fx.on_server(SERVER_DRAFTS).len(), 2);
    fx.make_due();
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(fx.on_server(SERVER_DRAFTS), [(4, "Weekly notes".into())]);
    assert_eq!(draft_appends(&fx), 4);
    assert!(
        fx.commands()
            .iter()
            .any(|cmd| cmd.contains("UID SEARCH UID 4:4"))
    );
}

#[test]
fn server_draft_write_back_off_and_cleanup_without_uidplus() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let hub = draft_hub(&dir.path().join("drafts"), &fx);
    let queue = |body: &str| {
        hub.show("draft-test");
        hub.note_shown("draft-test");
        set_draft_body(&hub, body.into());
        hub.note_hidden("draft-test");
        wait_for_draft(&hub);
    };
    fx.server.writes.lock().unwrap().uidplus = false;
    fx.store.set_write_back(&fx.account.id, false).unwrap();
    queue("Local only");
    assert_eq!(fx.replay().sent, 0);
    assert_eq!(draft_appends(&fx), 0);
    fx.store.set_write_back(&fx.account.id, true).unwrap();
    queue("First server copy");
    assert_eq!(fx.replay().sent, 1);
    fx.deliver(SERVER_DRAFTS, 50, REPLY_MIME.to_vec());
    fx.server.state.lock().unwrap().mailboxes[1].uidnext = Some(51);
    queue("Updated");
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(fx.on_server(SERVER_DRAFTS).len(), 3);
    assert!(
        fx.flags(SERVER_DRAFTS, 1)
            .contains(&"\\Deleted".to_string())
    );
    assert!(
        !fx.flags(SERVER_DRAFTS, 50)
            .contains(&"\\Deleted".to_string())
    );
    assert!(!fx.commands().iter().any(|cmd| cmd.contains(" EXPUNGE")));
}

#[test]
fn server_draft_new_save_checks_an_unconfirmed_copy_before_replacing_it() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let hub = draft_hub(&dir.path().join("drafts"), &fx);
    let queue = |body: &str| {
        hub.show("draft-test");
        hub.note_shown("draft-test");
        set_draft_body(&hub, body.into());
        hub.note_hidden("draft-test");
        wait_for_draft(&hub);
    };
    queue("Original");
    assert_eq!(fx.replay().sent, 1);
    queue("Unconfirmed");
    {
        let mut writes = fx.server.writes.lock().unwrap();
        writes.lose_reply_to = Some("APPEND");
        writes.refuse_search = true;
    }
    assert!(fx.replay().interrupted.is_some());
    queue("Already queued before recovery stopped");
    for _ in 0..super::write_back::MAX_APPEND_CHECKS {
        fx.make_due();
        assert_eq!(fx.replay().sent, 0);
    }
    assert_eq!(draft_appends(&fx), 2);
    assert_eq!(fx.on_server(SERVER_DRAFTS).len(), 2);
    fx.server.writes.lock().unwrap().refuse_search = false;
    queue("Next authorized save");
    assert_eq!(fx.replay().sent, 2);
    assert_eq!(draft_appends(&fx), 3);
    assert_eq!(fx.on_server(SERVER_DRAFTS), [(3, "Weekly notes".into())]);
}

#[test]
fn server_draft_import_keeps_html_only_text_and_filters_header_controls() {
    let fx = draft_fixture(Store::open_in_memory().unwrap());
    let mime = "From: Other sender <other@example.test>\r\n\
        To: =?UTF-8?Q?Av=E2=80=AEery?= <avery@example.test>\r\n\
        Subject: =?UTF-8?Q?F=E2=80=AEield_notes?=\r\n\
        Message-ID: <html-draft@example.test>\r\n\
        MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
        <h1>Field notes</h1><p>Two <strong>routes</strong> &amp; a map.</p>";
    fx.deliver(SERVER_DRAFTS, 1, mime.as_bytes().to_vec());
    fx.sync();
    let thread = fx.thread(SERVER_DRAFTS, "F\u{202e}ield notes");
    let id = fx
        .account_config()
        .import_server_draft(&fx.store, &thread, "imported-draft")
        .unwrap();
    let stored = fx.store.load_compose(&id).unwrap().unwrap();
    assert_eq!(stored.body, "Field notes\nTwo routes & a map.");
    assert_eq!(stored.fields.subject, "Field notes");
    assert_eq!(stored.fields.to, ["Avery <avery@example.test>"]);
    assert_eq!(stored.fields.from, "c@x");
    assert!(fx.subjects(SERVER_DRAFTS).is_empty());
}

#[test]
fn server_draft_with_uneditable_html_stays_on_the_server_and_in_drafts() {
    let fx = draft_fixture(Store::open_in_memory().unwrap());
    let mime = b"From: Reader <c@x>\r\nSubject: Image-only draft\r\n\
        Message-ID: <image-draft@example.test>\r\n\
        MIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
        <img src=\"https://example.test/map.png\">";
    fx.deliver(SERVER_DRAFTS, 1, mime.to_vec());
    fx.sync();
    let thread = fx.thread(SERVER_DRAFTS, "Image-only draft");
    let err = fx
        .account_config()
        .import_server_draft(&fx.store, &thread, "imported-draft")
        .unwrap_err();
    assert!(
        err.to_string().contains("HTML has no editable text"),
        "{err}"
    );
    assert!(fx.store.load_compose("imported-draft").unwrap().is_none());
    assert_eq!(fx.subjects(SERVER_DRAFTS), ["Image-only draft"]);
    assert_eq!(
        fx.on_server(SERVER_DRAFTS),
        [(1, "Image-only draft".into())]
    );
}

fn queue_draft_body(hub: &crate::compose::DraftHub, body: &str) {
    assert!(hub.show("draft-test"));
    hub.note_shown("draft-test");
    set_draft_body(hub, body.into());
    hub.note_hidden("draft-test");
    wait_for_draft(hub);
}

#[test]
fn server_draft_discard_and_undo_queue_delete_then_a_new_copy() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let hub = draft_hub(&dir.path().join("drafts"), &fx);
    queue_draft_body(&hub, "Original");
    assert_eq!(fx.replay().sent, 1);

    fx.store
        .discard_compose_drafts(&fx.account.id, &["draft-test".into()])
        .unwrap();
    assert!(
        fx.store
            .unsent_compose_drafts(&fx.account.id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fx.store
            .outbox(&fx.account.id)
            .unwrap()
            .iter()
            .filter(|entry| entry.state == OutboxState::Pending)
            .map(|entry| entry.kind)
            .collect::<Vec<_>>(),
        [OutboxKind::DeleteDraft]
    );

    fx.store.undo(&fx.account.id).unwrap();
    assert_eq!(
        fx.store
            .unsent_compose_drafts(&fx.account.id)
            .unwrap()
            .len(),
        1
    );
    // Even an unchanged save needs an APPEND after the already queued delete.
    queue_draft_body(&hub, "Original");
    assert_eq!(fx.replay().sent, 2);
    assert_eq!(fx.on_server(SERVER_DRAFTS), [(2, "Weekly notes".into())]);
}

#[test]
fn server_draft_undo_saves_even_while_an_older_append_is_in_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let hub = draft_hub(&dir.path().join("drafts"), &fx);
    queue_draft_body(&hub, "Original");
    fx.server.writes.lock().unwrap().lose_reply_to = Some("APPEND");
    assert!(fx.replay().interrupted.is_some());

    fx.store
        .discard_compose_drafts(&fx.account.id, &["draft-test".into()])
        .unwrap();
    fx.store.undo(&fx.account.id).unwrap();
    queue_draft_body(&hub, "Original");

    fx.make_due();
    assert_eq!(fx.replay().sent, 3);
    assert_eq!(fx.on_server(SERVER_DRAFTS), [(2, "Weekly notes".into())]);
}

#[test]
fn server_draft_discard_with_write_back_off_reveals_the_original_on_sync() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let hub = draft_hub(&dir.path().join("drafts"), &fx);
    queue_draft_body(&hub, "Original");
    assert_eq!(fx.replay().sent, 1);
    fx.sync();
    assert!(fx.subjects(SERVER_DRAFTS).is_empty());

    fx.store.set_write_back(&fx.account.id, false).unwrap();
    fx.store
        .discard_compose_drafts(&fx.account.id, &["draft-test".into()])
        .unwrap();
    assert!(
        fx.store
            .outbox(&fx.account.id)
            .unwrap()
            .iter()
            .all(|entry| entry.state != OutboxState::Pending)
    );
    fx.sync();
    assert_eq!(fx.subjects(SERVER_DRAFTS), ["Weekly notes"]);
    assert_eq!(fx.on_server(SERVER_DRAFTS), [(1, "Weekly notes".into())]);

    fx.store.undo(&fx.account.id).unwrap();
    fx.store.set_write_back(&fx.account.id, true).unwrap();
    queue_draft_body(&hub, "Original");
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(
        fx.on_server(SERVER_DRAFTS),
        [(1, "Weekly notes".into()), (2, "Weekly notes".into())]
    );
}

#[test]
fn server_draft_read_only_cleanup_exhausts_its_budget_and_releases_later_saves() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let hub = draft_hub(&dir.path().join("drafts"), &fx);
    queue_draft_body(&hub, "Original");
    assert_eq!(fx.replay().sent, 1);
    queue_draft_body(&hub, "Replacement");

    fx.server.writes.lock().unwrap().read_only_mailbox = Some(SERVER_DRAFTS.into());
    assert_eq!(fx.replay().sent, 0);
    assert_eq!(draft_appends(&fx), 2);
    queue_draft_body(&hub, "Another revision");
    for _ in 1..super::write_back::MAX_ATTEMPTS - 1 {
        fx.make_due();
        let report = fx.replay();
        assert!(report.dropped.is_empty());
        assert_eq!(draft_appends(&fx), 2);
    }
    fx.make_due();
    let report = fx.replay();
    assert_eq!(report.dropped.len(), 1);
    assert_eq!(report.dropped[0].reason, DropReason::UnconfirmedAppend);
    assert_eq!(draft_appends(&fx), 3);

    fx.server.writes.lock().unwrap().read_only_mailbox = None;
    fx.make_due();
    assert_eq!(fx.replay().sent, 1);
    assert_eq!(
        fx.on_server(SERVER_DRAFTS),
        [(1, "Weekly notes".into()), (3, "Weekly notes".into())]
    );
    fx.sync();
    assert_eq!(fx.subjects(SERVER_DRAFTS), ["Weekly notes"]);
}

#[test]
fn server_draft_cleanup_of_an_already_expunged_uid_does_not_stall() {
    let dir = tempfile::tempdir().unwrap();
    let fx = draft_fixture(Store::open(dir.path().join("mail.db")).unwrap());
    let hub = draft_hub(&dir.path().join("drafts"), &fx);
    queue_draft_body(&hub, "Original");
    assert_eq!(fx.replay().sent, 1);
    queue_draft_body(&hub, "Replacement");
    fx.server.writes.lock().unwrap().refuse_store_uid = Some(1);
    assert_eq!(fx.replay().sent, 0);
    fx.server.state.lock().unwrap().mailboxes[1]
        .messages
        .retain(|message| message.uid != 1);

    queue_draft_body(&hub, "Another revision");
    fx.make_due();
    let report = fx.replay();
    assert_eq!(report.sent, 2);
    assert!(report.dropped.is_empty());
    assert_eq!(fx.on_server(SERVER_DRAFTS), [(3, "Weekly notes".into())]);
}
