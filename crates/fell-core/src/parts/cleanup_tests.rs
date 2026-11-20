use super::tests::{Serves, request, temp_root};
use super::*;
use crate::backend::{FolderRemoval, MailboxDelta, RemovalKind, Settle};
use crate::{Account, BackendKind, Message};

fn open_fixture(tag: &str) -> (PathBuf, PartCache, Store) {
    let root = temp_root(tag);
    let cache = PartCache::open(root.join("parts"), DEFAULT_CACHE_BYTES).unwrap();
    let store = Store::open_for_part_fetch(&root.join("mail.db")).unwrap();
    for id in ["a1", "a2"] {
        store
            .upsert_account(&Account {
                id: AccountId(id.into()),
                display_name: id.into(),
                email: format!("{id}@example.test"),
                backend_kind: BackendKind::Imap,
            })
            .unwrap();
    }
    (root, cache, store)
}

fn message(account: &str, id: &str, folder: &str, uid: u32) -> Message {
    let account = AccountId(account.into());
    let mime = crate::imap::mime_fixtures::mixed_two_attachments(id);
    let mut message = crate::imap::to_message(&account, folder, folder, 7, uid, &[], &mime.raw);
    message.parts.as_mut().unwrap().mime_parts = vec![
        request(id, "2", Some("report.pdf")).part,
        request(id, "3", Some("totals.csv")).part,
    ];
    message
}

fn listed_parts(store: &Store, message: &Message) -> Vec<PartRequest> {
    let list = store
        .part_requests(&message.account_id, &message.headers.message_id)
        .unwrap();
    assert_eq!(list.len(), 2);
    list
}

#[test]
fn sync_settle_cleans_current_and_retired_sections_in_only_the_deleted_account() {
    let (root, cache, store) = open_fixture("sync-cleanup");
    let mut target = message("a1", "target@x", "INBOX", 1);
    let other_account = message("a2", "target@x", "INBOX", 1);
    let other_message = message("a1", "other@x", "INBOX", 2);
    for message in [&target, &other_account, &other_message] {
        store.upsert_message(message).unwrap();
    }
    let target_parts = listed_parts(&store, &target);
    let current = cache.store(&target_parts[0], b"data").unwrap();
    let retired = cache.store(&target_parts[1], b"data").unwrap();
    let account_neighbor = cache
        .store(&listed_parts(&store, &other_account)[0], b"data")
        .unwrap();
    let message_neighbor = cache
        .store(&listed_parts(&store, &other_message)[0], b"data")
        .unwrap();
    let mut old_name = target_parts[0].clone();
    old_name.part.filename = Some("old.pdf".into());
    let old_name = cache.store(&old_name, b"data").unwrap();

    target.parts.as_mut().unwrap().mime_parts.truncate(1);
    store.upsert_message(&target).unwrap();
    let removal = FolderRemoval {
        mailbox: "INBOX".into(),
        folder: "INBOX".into(),
        kind: RemovalKind::Vanished {
            uidvalidity: 7,
            ranges: vec![(1, 1)],
        },
    };
    store
        .apply_delta(
            &target.account_id,
            &MailboxDelta {
                removals: vec![removal],
                settle: Settle::KeepRows,
                ..MailboxDelta::default()
            },
        )
        .unwrap();
    store.clean_part_cache(&cache).unwrap();
    assert!(current.path.exists());
    assert!(retired.path.exists());

    store
        .apply_delta(
            &target.account_id,
            &MailboxDelta {
                settle: Settle::All,
                ..MailboxDelta::default()
            },
        )
        .unwrap();
    assert!(store.cached_part_stamp(&target_parts[0]).unwrap().is_none());
    store.clean_part_cache(&cache).unwrap();
    for removed in [current, retired, old_name] {
        assert!(!removed.path.parent().unwrap().exists());
    }
    for retained in [account_neighbor, message_neighbor] {
        assert_eq!(fs::read(retained.path).unwrap(), b"data");
    }
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn duplicate_merge_cleans_the_absorbed_rows_cache_and_keeps_the_canonical_rows_cache() {
    let (root, cache, store) = open_fixture("merge-cleanup");
    let mut keeper = message("a1", "keeper@x", "INBOX", 1);
    let mut duplicate = message("a1", "echo@x", "INBOX", 2);
    keeper.body_text = Some("A synthetic message body for the list echo. ".repeat(6));
    duplicate.body_text.clone_from(&keeper.body_text);
    store.upsert_message(&keeper).unwrap();
    store.upsert_message(&duplicate).unwrap();
    let keeper_part = &listed_parts(&store, &keeper)[0];
    let duplicate_part = &listed_parts(&store, &duplicate)[0];
    let retained = cache.store(keeper_part, b"keep").unwrap();
    let removed = cache.store(duplicate_part, b"drop").unwrap();

    duplicate.list_id = Some("list.example.test".into());
    store.upsert_message(&duplicate).unwrap();
    assert_eq!(
        store
            .message(&duplicate.account_id, &duplicate.headers.message_id)
            .unwrap()
            .unwrap()
            .headers
            .message_id,
        keeper.headers.message_id,
    );
    store.clean_part_cache(&cache).unwrap();
    assert!(!removed.path.exists());
    assert_eq!(fs::read(retained.path).unwrap(), b"keep");
    drop(store);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_rolled_back_delete_keeps_its_cache_and_a_committed_purge_retries_after_reopen() {
    let (root, cache, store) = open_fixture("purge-cleanup");
    let target = message("a1", "purge@x", "Trash", 1);
    let thread = store.upsert_message(&target).unwrap();
    let requests = listed_parts(&store, &target);
    let blocked = cache.store(&requests[0], b"data").unwrap();
    let ready = cache.store(&requests[1], b"data").unwrap();
    let store_path = root.join("mail.db");
    let reader = Store::open_for_part_fetch(&store_path).unwrap();
    let tx = crate::store::begin_immediate(&store.conn).unwrap();
    assert_eq!(
        tx.execute("DELETE FROM messages WHERE message_id = 'purge@x'", [])
            .unwrap(),
        1,
    );
    reader.clean_part_cache(&cache).unwrap();
    assert!(store.clean_part_cache(&cache).is_err());
    tx.rollback().unwrap();
    reader.clean_part_cache(&cache).unwrap();
    assert!(blocked.path.exists() && ready.path.exists());

    store
        .purge_threads(&target.account_id, &[thread], "Trash")
        .unwrap();
    // Replace one entry directory with a regular file to make its unlink fail.
    let dir = blocked.path.parent().unwrap();
    let backup = root.join("blocked");
    fs::rename(dir, &backup).unwrap();
    fs::write(dir, b"not a directory").unwrap();
    assert!(reader.clean_part_cache(&cache).is_err());
    assert!(
        !ready.path.exists(),
        "a failure did not hold up the other part"
    );
    drop((store, reader));

    fs::remove_file(dir).unwrap();
    fs::rename(backup, dir).unwrap();
    let reopened = Store::open_for_part_fetch(&store_path).unwrap();
    reopened.clean_part_cache(&cache).unwrap();
    assert!(!blocked.path.exists());
    reopened.clean_part_cache(&cache).unwrap();
    drop(reopened);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn an_in_flight_fetch_cannot_repopulate_the_cache_after_expunge_and_undo() {
    let (root, cache, store) = open_fixture("cleanup-during-fetch");
    let mut target = message("a1", "expunge@x", "Trash", 1);
    for part in &mut target.parts.as_mut().unwrap().mime_parts {
        part.size = 5;
    }
    let thread = store.upsert_message(&target).unwrap();
    let parts = listed_parts(&store, &target);
    let earlier_hit = cache.store(&parts[1], b"stale").unwrap();
    let start = || {
        PartFetcher::start_for_store(
            || Ok(Box::new(Serves(b"ZnJlc2g="))), // "fresh"
            cache.clone(),
            root.join("mail.db"),
        )
    };
    let worker = start();
    let (entered, downloading) = mpsc::channel();
    let (resume, paused) = mpsc::channel();
    let (done, result) = mpsc::channel();
    let timeout = Duration::from_secs(10);
    worker.fetch(parts[0].clone(), move |event| match event {
        PartEvent::Progress { .. } => {
            entered.send(()).unwrap();
            paused.recv_timeout(timeout).unwrap();
        }
        PartEvent::Done(value) => done.send(value).unwrap(),
        PartEvent::Downloading { .. } => {}
    });
    downloading.recv_timeout(timeout).unwrap();
    store
        .delete_thread(&target.account_id, &thread, "Trash")
        .unwrap();
    store.undo(&target.account_id).unwrap();

    // Opening the other part after undo evicts its old bytes, even before
    // the background sweep has run and while the first download is paused.
    let other_worker = start();
    let (send, receive) = mpsc::channel();
    other_worker.fetch(listed_parts(&store, &target)[1].clone(), move |event| {
        if let PartEvent::Done(value) = event {
            send.send(value).unwrap();
        }
    });
    let new_hit = receive.recv_timeout(timeout).unwrap().unwrap();
    assert_eq!(new_hit.path, earlier_hit.path);
    assert_eq!(fs::read(&new_hit.path).unwrap(), b"fresh");

    store.clean_part_cache(&cache).unwrap();
    resume.send(()).unwrap();
    let error = result.recv_timeout(timeout).unwrap().unwrap_err();
    assert_eq!(error.to_string(), part_removed().to_string());
    assert!(cache.lookup(&parts[0]).is_none());
    assert_eq!(fs::read(new_hit.path).unwrap(), b"fresh");
    drop((worker, other_worker, store));
    fs::remove_dir_all(root).unwrap();
}
