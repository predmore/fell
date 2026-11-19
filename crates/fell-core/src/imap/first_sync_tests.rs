//! A first sync reads each mailbox newest first, headers before bodies, and
//! resumes below what it already read.

use std::time::{Duration, Instant};

use crate::Error;
use crate::account::BackendKind;
use crate::backend::{ImapBackend, MailBackend, MailboxDelta, SyncProgress};
use crate::store::Store;
use crate::sync::{sync_once, sync_with_progress};

use super::sync_tests::{
    account, body_fetches, commands, config, empty_mailbox, sample_state, stored,
};
use super::test_server::{BodyForm, FixtureMailbox, FixtureMessage, TestServer, TestState};
use super::{Windows, decode_cursor, set_windows};

/// `count` messages, UIDs from 1, each a minute newer than the last, so the
/// highest UID is the newest. Subjects are `{prefix} {uid}`.
fn numbered(prefix: &str, count: u32) -> Vec<FixtureMessage> {
    (1..=count)
        .map(|uid| {
            let minutes = uid;
            let day = 1 + minutes / 1440;
            let hour = (minutes % 1440) / 60;
            let minute = minutes % 60;
            let raw = format!(
                "From: Ann <ann@x>\r\nSubject: {prefix} {uid}\r\nMessage-ID: <{prefix}{uid}@x>\r\n\
                 Date: {day} Jan 2026 {hour:02}:{minute:02}:00 +0000\r\n\
                 MIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n\
                 body of {prefix} {uid}\r\n"
            );
            FixtureMessage {
                uid,
                flags: Vec::new(),
                modseq: 1,
                body_form: BodyForm::Literal,
                raw: raw.into_bytes(),
            }
        })
        .collect()
}

fn folder(name: &str, flags: &str, uidvalidity: u32, prefix: &str, count: u32) -> FixtureMailbox {
    FixtureMailbox {
        list_flags: flags.into(),
        uidnext: Some(count + 1),
        messages: numbered(prefix, count),
        ..empty_mailbox(name, uidvalidity)
    }
}

fn state_with(mailboxes: Vec<FixtureMailbox>) -> TestState {
    let mut state = sample_state();
    state.mailboxes = mailboxes;
    state
}

const SMALL: Windows = Windows {
    first: 10,
    window: 50,
    inbox_head: 60,
    sent_head: 20,
    other_head: 20,
};

/// What one delta carried, in the order sync persisted them.
#[derive(Debug)]
struct Seen {
    /// Subject, and whether the row had only its headers.
    rows: Vec<(String, bool)>,
    progress: Option<SyncProgress>,
}

fn headers_only(message: &crate::Message) -> bool {
    message.parts.is_none() && message.body_text.is_none() && message.snippet.is_empty()
}

/// Run one pass, applying each delta to `store`, and return what each
/// delta carried. `stop` ends the pass with an error after a delta it
/// accepts is applied, as a crash after a commit would.
fn run_pass(
    backend: &ImapBackend,
    store: &Store,
    stop: &mut dyn FnMut(&MailboxDelta) -> bool,
) -> (Vec<Seen>, Result<(), Error>) {
    let acct = account("a1", BackendKind::Imap);
    store.upsert_account(&acct).unwrap();
    let cursor = store.sync_cursor(&acct.id).unwrap();
    let known = |mailbox: &str, uidvalidity: u32, range: (u32, u32)| {
        store.stored_uids(&acct.id, mailbox, uidvalidity, range)
    };
    let mut seen = Vec::new();
    let result = backend.sync_changes(cursor.as_ref(), &known, &mut |delta| {
        store.apply_delta(&acct.id, &delta)?;
        seen.push(Seen {
            rows: delta
                .messages
                .iter()
                .map(|message| (message.headers.subject.clone(), headers_only(message)))
                .collect(),
            progress: delta.progress.clone(),
        });
        if stop(&delta) {
            return Err(Error::Backend("stopped by the test".into()));
        }
        Ok(())
    });
    (seen, result)
}

fn backend_for(server: &TestServer) -> ImapBackend {
    ImapBackend {
        account_id: account("a1", BackendKind::Imap).id,
        config: config(server.addr.port()),
    }
}

fn uid_of(subject: &str) -> u32 {
    subject.rsplit(' ').next().unwrap().parse().unwrap()
}

#[test]
fn a_first_sync_shows_the_newest_inbox_rows_before_any_body() {
    let _sizes = set_windows(SMALL);
    let server = TestServer::spawn(state_with(vec![
        folder("INBOX", "", 3, "In", 200),
        folder("Sent", "\\Sent", 4, "Out", 100),
        folder("Archive", "\\Archive", 5, "Old", 100),
    ]));
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    let (seen, result) = run_pass(&backend, &store, &mut |_| false);
    result.unwrap();

    // The first rows are the newest window of the Inbox, headers only.
    let first = seen
        .iter()
        .find(|delta| !delta.rows.is_empty())
        .expect("rows were stored");
    let mut subjects: Vec<u32> = first
        .rows
        .iter()
        .map(|(subject, _)| uid_of(subject))
        .collect();
    subjects.sort_unstable();
    assert_eq!(subjects, (191..=200).collect::<Vec<_>>());
    assert!(
        first
            .rows
            .iter()
            .all(|(subject, bare)| subject.starts_with("In ") && *bare)
    );
    assert_eq!(
        first.progress,
        Some(SyncProgress {
            folder: "Inbox".into(),
            done: 10,
            total: 200,
        })
    );

    // The newest window's bodies came before any row of the next window,
    // and bodies came newest first throughout.
    let bodies: Vec<(String, u32)> = seen
        .iter()
        .flat_map(|delta| delta.rows.iter())
        .filter(|(_, bare)| !bare)
        .map(|(subject, _)| {
            let (prefix, _) = subject.split_once(' ').unwrap();
            (prefix.to_string(), uid_of(subject))
        })
        .collect();
    let inbox: Vec<u32> = bodies
        .iter()
        .filter(|(prefix, _)| prefix == "In")
        .map(|(_, uid)| *uid)
        .collect();
    assert_eq!(inbox.len(), 200);
    assert!(inbox[..10].iter().all(|uid| *uid > 190), "{inbox:?}");
    // Windows of 10, then 50: 191..=200, 141..=190, 91..=140, 41..=90, 1..=40.
    let window = |uid: u32| match uid {
        191.. => 0,
        141.. => 1,
        91.. => 2,
        41.. => 3,
        _ => 4,
    };
    let windows: Vec<u32> = inbox.iter().map(|uid| window(*uid)).collect();
    assert!(
        windows.windows(2).all(|pair| pair[0] <= pair[1]),
        "{inbox:?}"
    );

    // Inbox head, then the newest Sent, then the rest of the Inbox, then
    // the newest of other folders, then older history.
    let examined: Vec<String> = commands(&server)
        .iter()
        .filter_map(|line| {
            line.split_once(" EXAMINE \"")
                .map(|(_, rest)| rest.to_string())
        })
        .map(|rest| rest.split('"').next().unwrap().to_string())
        .collect();
    assert_eq!(
        examined,
        vec!["INBOX", "Sent", "INBOX", "Archive", "Sent", "Archive"]
    );
    let order: Vec<String> =
        bodies
            .iter()
            .map(|(prefix, _)| prefix.clone())
            .fold(Vec::new(), |mut runs, prefix| {
                if runs.last() != Some(&prefix) {
                    runs.push(prefix);
                }
                runs
            });
    assert_eq!(order, vec!["In", "Out", "In", "Old", "Out", "Old"]);

    // Progress counts up to the whole folder.
    let inbox_progress: Vec<u32> = seen
        .iter()
        .filter_map(|delta| delta.progress.as_ref())
        .filter(|progress| progress.folder == "Inbox")
        .map(|progress| progress.done)
        .collect();
    assert!(inbox_progress.windows(2).all(|pair| pair[0] <= pair[1]));
    assert_eq!(inbox_progress.last(), Some(&200));
    assert!(
        seen.iter()
            .filter_map(|delta| delta.progress.as_ref())
            .any(|progress| progress.folder == "Sent" && progress.total == 100)
    );

    // Every message ended with its body, once.
    let acct = account("a1", BackendKind::Imap);
    assert_eq!(store.list_threads(&acct.id).unwrap().len(), 400);
    for id in ["In1@x", "In200@x", "Out1@x", "Old100@x"] {
        let message = stored(&store, &acct, id).unwrap();
        assert!(!message.snippet.is_empty(), "{id}");
    }
    let cursor = store.sync_cursor(&acct.id).unwrap();
    for (name, checkpoint) in decode_cursor(cursor.as_ref()) {
        assert_eq!(checkpoint.floor, None, "{name} is read to the oldest");
    }
}

#[test]
fn a_restart_resumes_below_the_windows_already_read() {
    let _sizes = set_windows(Windows {
        inbox_head: 1_000,
        ..SMALL
    });
    let server = TestServer::spawn(state_with(vec![folder("INBOX", "", 3, "In", 120)]));
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    // Windows: 111..=120, 61..=110, 11..=60, 1..=10. Stop once the second
    // is stored, as a crash after that commit would.
    let (_, result) = run_pass(&backend, &store, &mut |delta| {
        let checkpoints = decode_cursor(delta.cursor.as_ref());
        checkpoints
            .iter()
            .any(|(_, checkpoint)| checkpoint.floor == Some(61))
    });
    assert!(result.is_err());
    server.state.lock().expect("state").commands.clear();

    let acct = account("a1", BackendKind::Imap);
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 60, "only the 60 older messages are new");
    let logged = commands(&server);
    let headers: Vec<&String> = logged
        .iter()
        .filter(|line| line.contains("BODY.PEEK[HEADER]"))
        .collect();
    assert!(!headers.is_empty());
    for line in &headers {
        let range = line
            .split("UID FETCH ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap();
        let high: u32 = range.split(':').next_back().unwrap().parse().unwrap();
        assert!(
            high <= 60,
            "a window already read is not read again: {line}"
        );
    }
    for line in body_fetches(&server) {
        let set = line
            .split("UID FETCH ")
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap();
        for part in set.split(',') {
            let high: u32 = part.split(':').next_back().unwrap().parse().unwrap();
            assert!(
                high <= 60,
                "a body already stored is not fetched again: {line}"
            );
        }
    }
    assert_eq!(store.list_threads(&acct.id).unwrap().len(), 120);
    assert!(!stored(&store, &acct, "In1@x").unwrap().snippet.is_empty());
}

#[test]
fn progress_reaches_the_report_while_a_first_sync_runs() {
    let _sizes = set_windows(SMALL);
    let server = TestServer::spawn(state_with(vec![folder("INBOX", "", 3, "In", 80)]));
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    let acct = account("a1", BackendKind::Imap);
    let mut reports = Vec::new();
    let report = sync_with_progress(&acct, &backend, &store, &mut |report| {
        reports.push(report.clone());
    })
    .unwrap();
    assert_eq!(report.upserted, 80);
    let first = reports
        .iter()
        .find_map(|report| report.progress.clone())
        .expect("progress was reported");
    assert_eq!(
        first,
        SyncProgress {
            folder: "Inbox".into(),
            done: 10,
            total: 80,
        }
    );
    // Rows from headers do not count as stored until their bodies come.
    assert_eq!(reports[0].upserted, 0);
}

#[test]
fn a_second_pass_after_a_first_sync_fetches_no_body_again() {
    let _sizes = set_windows(SMALL);
    let mut state = state_with(vec![folder("INBOX", "", 3, "In", 70)]);
    state.condstore = true;
    let server = TestServer::spawn(state);
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    let acct = account("a1", BackendKind::Imap);
    sync_once(&acct, &backend, &store).unwrap();
    server.state.lock().expect("state").commands.clear();
    let report = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(report.upserted, 0);
    assert!(body_fetches(&server).is_empty(), "{:?}", commands(&server));
    assert!(
        commands(&server)
            .iter()
            .all(|line| !line.contains("BODY.PEEK[HEADER]"))
    );
}

/// The acceptance check for a first sync: on loopback, the newest 50
/// INBOX threads of a 30,000-message mailbox are in the store within 3 s
/// of login. Run it with `cargo test -p fell-core -- --ignored
/// newest_inbox_threads`; it prints the time.
#[test]
#[ignore = "builds a 30,000-message mailbox; run by hand to measure"]
fn newest_inbox_threads_of_a_large_mailbox_arrive_quickly() {
    let server = TestServer::spawn(state_with(vec![
        folder("INBOX", "", 3, "In", 30_000),
        folder("Sent", "\\Sent", 4, "Out", 2_000),
    ]));
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    let acct = account("a1", BackendKind::Imap);
    let started = Instant::now();
    let mut visible_at: Option<Duration> = None;
    let (_, _) = run_pass(&backend, &store, &mut |_| {
        let threads = store.list_threads(&acct.id).unwrap();
        if threads.len() >= 50 {
            visible_at = Some(started.elapsed());
            return true;
        }
        false
    });
    let elapsed = visible_at.expect("50 threads arrived");
    let threads = store.list_threads(&acct.id).unwrap();
    let newest: Vec<u32> = threads
        .iter()
        .take(50)
        .map(|thread| uid_of(&thread.subject))
        .collect();
    assert_eq!(newest, (29_951..=30_000).rev().collect::<Vec<_>>());
    eprintln!("newest 50 INBOX threads stored {elapsed:?} after login began");
    assert!(elapsed <= Duration::from_secs(3), "{elapsed:?}");
}

#[test]
fn a_body_too_large_to_keep_is_marked_and_not_asked_for_again() {
    let _sizes = set_windows(SMALL);
    let mut inbox = folder("INBOX", "", 3, "In", 30);
    let big = inbox
        .messages
        .iter_mut()
        .find(|message| message.uid == 25)
        .unwrap();
    big.raw.extend(std::iter::repeat_n(b'Q', 4_000));
    big.raw.extend_from_slice(b"\r\n");
    let _limit = super::session::limit_literals_to(2_000);
    let server = TestServer::spawn(state_with(vec![inbox]));
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    let acct = account("a1", BackendKind::Imap);
    sync_once(&acct, &backend, &store).unwrap();

    // The row keeps its headers and says why it has no body.
    let ids = [
        crate::MessageId::new("In25@x"),
        crate::MessageId::new("In24@x"),
    ];
    let row = stored(&store, &acct, "In25@x").expect("the row stays");
    assert_eq!(row.headers.subject, "In 25");
    assert_eq!(
        store.body_states(&acct.id, &ids).unwrap(),
        vec![crate::BodyState::TooLarge, crate::BodyState::Stored]
    );
    let cursor = store.sync_cursor(&acct.id).unwrap();
    for (name, checkpoint) in decode_cursor(cursor.as_ref()) {
        assert_eq!(checkpoint.floor, None, "{name} is read to the oldest");
    }

    // Later passes count it as read and never download it again.
    server.state.lock().expect("state").commands.clear();
    let again = sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(again.upserted, 0);
    assert!(body_fetches(&server).is_empty(), "{:?}", commands(&server));
    assert!(store.missing_bodies(&acct.id, 100).unwrap().is_empty());
}

#[test]
fn a_header_row_still_waiting_for_its_body_reads_as_pending() {
    let _sizes = set_windows(SMALL);
    let server = TestServer::spawn(state_with(vec![folder("INBOX", "", 3, "In", 30)]));
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    // Stop once the newest window's headers are stored, before its bodies.
    let (_, result) = run_pass(&backend, &store, &mut |delta| {
        !delta.messages.is_empty() && delta.messages.iter().all(headers_only)
    });
    assert!(result.is_err());
    let acct = account("a1", BackendKind::Imap);
    assert_eq!(
        store
            .body_states(&acct.id, &[crate::MessageId::new("In30@x")])
            .unwrap(),
        vec![crate::BodyState::Pending]
    );
    sync_once(&acct, &backend, &store).unwrap();
    assert_eq!(
        store
            .body_states(&acct.id, &[crate::MessageId::new("In30@x")])
            .unwrap(),
        vec![crate::BodyState::Stored]
    );
}

#[test]
fn a_uidvalidity_change_between_visits_starts_the_mailbox_over() {
    let _sizes = set_windows(SMALL);
    let server = TestServer::spawn(state_with(vec![
        folder("INBOX", "", 3, "In", 200),
        folder("Sent", "\\Sent", 4, "Out", 30),
    ]));
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    // While Sent is read, the server renumbers the Inbox.
    let state = server.state.clone();
    let mut renumbered = false;
    let (_, result) = run_pass(&backend, &store, &mut |delta| {
        if !renumbered
            && delta
                .progress
                .as_ref()
                .is_some_and(|progress| progress.folder == "Sent")
        {
            renumbered = true;
            let mut state = state.lock().expect("state");
            let inbox = state
                .mailboxes
                .iter_mut()
                .find(|mailbox| mailbox.name == "INBOX")
                .unwrap();
            inbox.uidvalidity = 7;
        }
        false
    });
    result.unwrap();
    assert!(renumbered);

    let acct = account("a1", BackendKind::Imap);
    let cursor = store.sync_cursor(&acct.id).unwrap();
    let inbox = decode_cursor(cursor.as_ref())
        .into_iter()
        .find(|(name, _)| name == "INBOX")
        .map(|(_, checkpoint)| checkpoint)
        .unwrap();
    assert_eq!(inbox.uidvalidity, 7, "the walk went on under the new UIDs");
    assert_eq!(inbox.floor, None);
    // Every Inbox message is stored under the new UIDVALIDITY, with its body.
    assert_eq!(
        store.stored_uids(&acct.id, "INBOX", 7, (1, 200)).unwrap(),
        (1..=200).collect::<Vec<_>>()
    );
    assert!(
        store
            .stored_uids(&acct.id, "INBOX", 3, (1, 200))
            .unwrap()
            .is_empty(),
        "no copy is left under the old UIDVALIDITY"
    );
}

#[test]
fn without_condstore_a_flag_changed_during_the_walk_is_applied_at_its_end() {
    let _sizes = set_windows(SMALL);
    let server = TestServer::spawn(state_with(vec![
        folder("INBOX", "", 3, "In", 200),
        folder("Sent", "\\Sent", 4, "Out", 30),
    ]));
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    // The newest Inbox message is read on another device while Sent is
    // read, after its window was stored as unread.
    let state = server.state.clone();
    let mut read_elsewhere = false;
    let (_, result) = run_pass(&backend, &store, &mut |delta| {
        if !read_elsewhere
            && delta
                .progress
                .as_ref()
                .is_some_and(|progress| progress.folder == "Sent")
        {
            read_elsewhere = true;
            let mut state = state.lock().expect("state");
            let inbox = state
                .mailboxes
                .iter_mut()
                .find(|mailbox| mailbox.name == "INBOX")
                .unwrap();
            let newest = inbox
                .messages
                .iter_mut()
                .find(|message| message.uid == 200)
                .unwrap();
            newest.flags.push("\\Seen".into());
        }
        false
    });
    result.unwrap();
    assert!(read_elsewhere);
    let acct = account("a1", BackendKind::Imap);
    let newest = stored(&store, &acct, "In200@x").unwrap();
    assert!(!newest.unread, "the change made during the walk landed");
    assert!(stored(&store, &acct, "In199@x").unwrap().unread);
}

#[test]
fn with_no_sent_role_the_sent_folder_by_name_is_read_early() {
    let _sizes = set_windows(SMALL);
    // The server marks Trash but not Sent, so no folder has the Sent role.
    let server = TestServer::spawn(state_with(vec![
        folder("INBOX", "", 3, "In", 200),
        folder("Sent", "", 4, "Out", 100),
        folder("Trash", "\\Trash", 5, "Bin", 100),
    ]));
    let backend = backend_for(&server);
    let store = Store::open_in_memory().unwrap();
    let (_, result) = run_pass(&backend, &store, &mut |_| false);
    result.unwrap();
    let examined: Vec<String> = commands(&server)
        .iter()
        .filter_map(|line| {
            line.split_once(" EXAMINE \"")
                .map(|(_, rest)| rest.split('"').next().unwrap().to_string())
        })
        .collect();
    assert_eq!(
        examined,
        vec!["INBOX", "Sent", "INBOX", "Trash", "Sent", "Trash"]
    );
}
