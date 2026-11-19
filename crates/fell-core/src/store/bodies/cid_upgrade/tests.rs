use std::collections::BTreeSet;

use super::*;
use crate::{Account, BackendKind, LabelChange, Message, MessageId, MessageParts, Store};

fn account() -> Account {
    Account {
        id: AccountId("a1".into()),
        display_name: "Test".into(),
        email: "me@x".into(),
        backend_kind: BackendKind::Imap,
    }
}

fn raw(id: &str, body: &str, body_type: &str, disposition: &str) -> String {
    format!(
        "From: a@x\r\nSubject: {id}\r\nMessage-ID: <{id}>\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/related; boundary=rel\r\n\r\n\
         --rel\r\nContent-Type: {body_type}\r\n\r\n{body}\r\n\
         --rel\r\nContent-Type: image/png; name=logo.png\r\n\
         Content-ID: <logo@x>\r\n{disposition}\r\nlogo bytes\r\n--rel--\r\n"
    )
}

fn message(uid: u32, raw: &str) -> Message {
    crate::imap::to_message(&account().id, "INBOX", "INBOX", 7, uid, &[], raw.as_bytes())
}

fn with_legacy_role(mut message: Message, nested: bool, role: PartRole) -> Message {
    let list = &mut message.parts.as_mut().unwrap().mime_parts;
    list.iter_mut()
        .find(|part| part.nested == nested && candidate(part))
        .unwrap()
        .role = role;
    message.has_attachment = crate::imap::has_attachment(list);
    message.attachment_names = crate::imap::attachment_names(list);
    message
}

fn store_message(store: &Store, id: &str) -> Message {
    store
        .message(&account().id, &MessageId::new(id))
        .unwrap()
        .unwrap()
}

fn stored_parts(store: &Store, id: &str) -> MessageParts {
    store
        .message_body(&account().id, &MessageId::new(id))
        .unwrap()
        .unwrap()
}

fn simulate_v5(store: &Store) {
    store
        .conn
        .execute("DELETE FROM meta WHERE key = ?1", [MIGRATION_KEY])
        .unwrap();
}

#[test]
fn a_headless_reader_repairs_cid_roles_and_derived_reads_only_once() {
    let dir = std::env::temp_dir().join(format!(
        "fell-cid-upgrade-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("mail.db");
    let account = account();
    let cases = [
        (
            "literal@x",
            "<p>Write src=cid:logo@x here</p>",
            "",
            PartRole::Inline,
        ),
        (
            "encoded@x",
            "<img src='CID&#58;LOGO%40x'>",
            "",
            PartRole::Attachment,
        ),
        ("embedded@x", "<img src=cid:logo@x>", "", PartRole::Inline),
        (
            "explicit@x",
            "<img src=cid:logo@x>",
            "Content-Disposition: attachment\r\n",
            PartRole::Attachment,
        ),
    ];
    let before = {
        let store = Store::open(&path).unwrap();
        store.upsert_account(&account).unwrap();
        for (index, (id, html, disposition, old_role)) in cases.iter().enumerate() {
            let mut mail = message(index as u32 + 1, &raw(id, html, "text/html", disposition));
            if disposition.is_empty() {
                mail = with_legacy_role(mail, false, *old_role);
            }
            store.upsert_message(&mail).unwrap();
        }
        let literal = store_message(&store, "literal@x");
        store
            .label_thread(
                &account.id,
                &literal.thread_id,
                LabelChange::Add("local".into()),
            )
            .unwrap();
        store
            .conn
            .execute(
                "UPDATE messages SET unread = 0, starred = 1, snooze_until = 42
             WHERE message_id = 'literal@x'",
                [],
            )
            .unwrap();
        // The legacy queue lacks the CID-specific reason.
        store
            .conn
            .execute_batch(
                "ALTER TABLE body_backfill DROP COLUMN refresh_cid_roles;
             CREATE TRIGGER keep_healthy_part_rows BEFORE UPDATE ON message_parts
             WHEN old.message_id IN ('embedded@x', 'explicit@x')
             BEGIN SELECT RAISE(ABORT, 'healthy part row changed'); END;",
            )
            .unwrap();
        simulate_v5(&store);
        cases.map(|(id, _, _, _)| {
            (
                store_message(&store, id),
                stored_parts(&store, id),
                store.stored_part_kinds(&account.id, &[id]).unwrap()[id],
            )
        })
    };
    for _ in 0..2 {
        let store = Store::open(&path).unwrap();
        store.finish_cid_repair().unwrap();
        for (index, (id, html, disposition, _)) in cases.iter().enumerate() {
            let parsed = message(index as u32 + 1, &raw(id, html, "text/html", disposition));
            let (mut expected, old_body, old_kinds) = before[index].clone();
            expected.has_attachment = parsed.has_attachment;
            expected.attachment_names = parsed.attachment_names;
            let parts = stored_parts(&store, id);
            assert_eq!(store_message(&store, id), expected);
            assert_eq!(parts, parsed.parts.unwrap());
            assert_eq!(parts.html, old_body.html);
            assert_eq!(
                store.stored_part_kinds(&account.id, &[id]).unwrap()[*id],
                old_kinds
            );
        }
        let summaries: BTreeSet<_> = store
            .folder_threads(&account.id, "INBOX")
            .unwrap()
            .into_iter()
            .filter(|row| row.has_attachment)
            .map(|row| row.subject)
            .collect();
        let hits: BTreeSet<_> = store
            .search(&account.id, "has:attachment")
            .unwrap()
            .hits
            .into_iter()
            .map(|hit| hit.subject)
            .collect();
        let expected = BTreeSet::from(["literal@x".to_string(), "explicit@x".to_string()]);
        assert_eq!(summaries, expected);
        assert_eq!(hits, expected);
        assert!(store.missing_bodies(&account.id, 200).unwrap().is_empty());
        assert_eq!(cursor(&store.conn).unwrap(), None);
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn only_uncertain_matching_parts_are_recovered_from_the_queued_source() {
    let store = Store::open_in_memory().unwrap();
    let account = account();
    store.upsert_account(&account).unwrap();
    let inner = raw(
        "inner@x",
        "<p>Write src=cid:logo@x here</p>",
        "text/html",
        "",
    );
    let outer = format!(
        "From: a@x\r\nMessage-ID: <outer@x>\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=mix\r\n\r\n\
         --mix\r\nContent-Type: text/html\r\n\r\n<img src=cid:logo@x>\r\n\
         --mix\r\nContent-Type: image/png; name=logo.png\r\n\
         Content-ID: <logo@x>\r\n\r\nouter logo\r\n\
         --mix\r\nContent-Type: message/rfc822; name=original.eml\r\n\r\n{inner}\r\n--mix--\r\n"
    );
    let nested = message(1, &outer);
    store
        .upsert_message(&with_legacy_role(nested.clone(), true, PartRole::Inline))
        .unwrap();

    let html = "<p>Write src=cid:logo@x here</p>";
    let lost = message(2, &raw("lost@x", html, "text/html", ""));
    let broken = message(3, &raw("broken@x", html, "text/html", ""));
    let offline = message(4, &raw("offline@x", html, "text/html", ""));
    for mail in [&lost, &broken, &offline] {
        store
            .upsert_message(&with_legacy_role(mail.clone(), false, PartRole::Inline))
            .unwrap();
    }
    let plain = message(5, &raw("plain@x", "See cid:logo@x", "text/plain", ""));
    store.upsert_message(&plain).unwrap();
    store
        .conn
        .execute_batch(
            "UPDATE message_bodies SET html = NULL WHERE message_id = 'lost@x';
         UPDATE message_bodies SET html = X'FFFF' WHERE message_id IN ('broken@x', 'offline@x');
         DELETE FROM message_copies WHERE message_id = 'offline@x';",
        )
        .unwrap();
    simulate_v5(&store);
    store.finish_cid_repair().unwrap();

    let nested_before = stored_parts(&store, "outer@x");
    let uncertain = |id: &str| {
        store
            .message_attachments(&account.id, &MessageId::new(id))
            .unwrap()
            .unwrap()
    };
    assert_eq!(uncertain("offline@x")[0].role, PartRole::Inline);
    assert_eq!(uncertain("lost@x")[0].role, PartRole::Inline);
    assert_eq!(uncertain("broken@x")[0].role, PartRole::Inline);
    assert_eq!(stored_parts(&store, "plain@x"), plain.parts.unwrap());
    let wanted: BTreeSet<_> = store
        .missing_bodies(&account.id, 200)
        .unwrap()
        .into_iter()
        .map(|request| request.message_id.0)
        .collect();
    assert_eq!(
        wanted,
        BTreeSet::from(["lost@x".into(), "broken@x".into(), "outer@x".into()])
    );

    let mut incoming = nested.parts.unwrap();
    // A different outer body would say this same CID is an attachment.
    // Only its nested part should be repaired.
    incoming.html = Some("<p>The outer body on another copy</p>".into());
    incoming.mime_parts[0].role = PartRole::Attachment;
    let mut different = broken.parts.unwrap();
    different.mime_parts[0].filename = Some("other.png".into());
    store
        .store_fetched_parts(
            &account.id,
            &[
                (MessageId::new("outer@x"), Some(incoming)),
                (MessageId::new("lost@x"), Some(lost.parts.unwrap())),
                (MessageId::new("broken@x"), Some(different)),
            ],
        )
        .unwrap();
    let mut expected = nested_before;
    expected
        .mime_parts
        .iter_mut()
        .find(|part| part.nested)
        .unwrap()
        .role = PartRole::Attachment;
    assert_eq!(stored_parts(&store, "outer@x"), expected);
    assert_eq!(
        store_message(&store, "outer@x").attachment_names,
        ["original.eml"]
    );
    assert_eq!(
        store_message(&store, "lost@x").attachment_names,
        ["logo.png"]
    );
    assert_eq!(stored_parts(&store, "lost@x").html, None);
    assert_eq!(uncertain("broken@x")[0].role, PartRole::Inline);
    assert_eq!(
        uncertain("broken@x")[0].filename.as_deref(),
        Some("logo.png")
    );
    assert!(store.missing_bodies(&account.id, 200).unwrap().is_empty());
    store.finish_cid_repair().unwrap();
    assert!(store.missing_bodies(&account.id, 200).unwrap().is_empty());
}

#[test]
fn a_failed_batch_leaves_completed_batches_and_retries_from_their_cursor() {
    let store = Store::open_in_memory().unwrap();
    let account = account();
    store.upsert_account(&account).unwrap();
    for uid in 1..=(BATCH as u32 + 2) {
        let id = if uid == BATCH as u32 + 2 {
            "fail@x".into()
        } else {
            format!("m{uid}@x")
        };
        let mail = message(uid, &raw(&id, "<p>see src=cid:logo@x</p>", "text/html", ""));
        store
            .upsert_message(&with_legacy_role(mail, false, PartRole::Inline))
            .unwrap();
    }
    simulate_v5(&store);
    store
        .conn
        .execute_batch(
            "CREATE TRIGGER fail_cid_batch BEFORE UPDATE OF role ON message_parts
         WHEN old.message_id = 'fail@x'
         BEGIN SELECT RAISE(ABORT, 'stop migration'); END;",
        )
        .unwrap();
    assert!(store.finish_cid_repair().is_err());
    assert_eq!(cursor(&store.conn).unwrap().unwrap().after, BATCH as i64);
    assert!(store_message(&store, &format!("m{BATCH}@x")).has_attachment);
    assert!(!store_message(&store, &format!("m{}@x", BATCH + 1)).has_attachment);
    assert!(!store_message(&store, "fail@x").has_attachment);
    store
        .conn
        .execute_batch("DROP TRIGGER fail_cid_batch;")
        .unwrap();
    store.finish_cid_repair().unwrap();
    assert_eq!(cursor(&store.conn).unwrap(), None);
    assert!(store_message(&store, "fail@x").has_attachment);
    assert_eq!(
        store
            .search(&account.id, "has:attachment")
            .unwrap()
            .hits
            .len(),
        BATCH + 2
    );
}

#[test]
fn filling_a_gap_keeps_a_pending_cid_refresh_until_the_list_is_replaced() {
    use crate::store::bodies::{StoredParts, Write, write};

    let store = Store::open_in_memory().unwrap();
    let account = account();
    store.upsert_account(&account).unwrap();
    let parsed = message(1, &raw("kept@x", "See cid:logo@x", "text/plain", ""));
    store
        .upsert_message(&with_legacy_role(parsed.clone(), false, PartRole::Inline))
        .unwrap();
    store.finish_cid_repair().unwrap();
    assert!(queued(&store.conn, &account.id, "kept@x").unwrap());
    let newer = StoredParts::new(&parsed.parts.unwrap());
    assert!(!write(&store.conn, &account.id, "kept@x", &newer, Write::FillGap).unwrap());
    assert!(
        !write(
            &store.conn,
            &account.id,
            "kept@x",
            &StoredParts::default(),
            Write::Replace
        )
        .unwrap()
    );
    assert!(queued(&store.conn, &account.id, "kept@x").unwrap());
    assert!(write(&store.conn, &account.id, "kept@x", &newer, Write::Replace).unwrap());
    assert!(!queued(&store.conn, &account.id, "kept@x").unwrap());
}

#[test]
fn a_merge_keeps_the_keeper_refresh_or_inherits_it_with_the_donated_list() {
    use crate::store::{bodies, copies};

    let store = Store::open_in_memory().unwrap();
    let account = account();
    store.upsert_account(&account).unwrap();
    let html = "See cid:logo@x";
    let kept = message(1, &raw("kept@x", html, "text/plain", ""));
    let donated = message(2, &raw("donated@x", html, "text/plain", ""));
    for msg in [&kept, &donated] {
        store
            .upsert_message(&with_legacy_role(msg.clone(), false, PartRole::Inline))
            .unwrap();
    }
    let mut empty = message(3, &raw("empty@x", "A different copy", "text/plain", ""));
    empty.parts = None;
    store.upsert_message(&empty).unwrap();
    store.finish_cid_repair().unwrap();
    let tx = immediate_transaction(&store.conn).unwrap();
    assert!(
        bodies::move_into(&tx, &account.id, "donated@x", "kept@x")
            .unwrap()
            .is_none()
    );
    assert!(queued(&tx, &account.id, "kept@x").unwrap());
    let list = bodies::move_into(&tx, &account.id, "donated@x", "empty@x")
        .unwrap()
        .unwrap();
    set_attachments_from_list(&tx, &account.id, "empty@x", &list).unwrap();
    tx.execute("DELETE FROM messages WHERE message_id = 'donated@x'", [])
        .unwrap();
    copies::repoint(&tx, &account.id, "donated@x", "empty@x").unwrap();
    tx.commit().unwrap();
    let pending = store.missing_bodies(&account.id, 10).unwrap();
    let merged = pending
        .iter()
        .find(|item| item.message_id.as_str() == "empty@x")
        .unwrap();
    assert!(merged.copies.contains(&donated.remote_id));
    assert!(
        pending
            .iter()
            .any(|item| item.message_id.as_str() == "kept@x")
    );
    store
        .store_fetched_parts(&account.id, &[(MessageId::new("empty@x"), donated.parts)])
        .unwrap();
    assert_eq!(
        store_message(&store, "empty@x").attachment_names,
        ["logo.png"]
    );
    assert!(!queued(&store.conn, &account.id, "empty@x").unwrap());
    assert!(queued(&store.conn, &account.id, "kept@x").unwrap());
}

#[test]
fn a_list_moved_behind_another_connections_cursor_is_still_repaired() {
    let dir = std::env::temp_dir().join(format!(
        "fell-cid-moved-list-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    std::fs::create_dir(&dir).unwrap();
    let path = dir.join("mail.db");
    let store = Store::open(&path).unwrap();
    let other = Store::open(&path).unwrap();
    let account = account();
    store.upsert_account(&account).unwrap();
    let mut empty = message(1, &raw("kept@x", "Keeper", "text/plain", ""));
    empty.parts = None;
    store.upsert_message(&empty).unwrap();
    super::super::write(
        &store.conn,
        &account.id,
        "kept@x",
        &super::super::StoredParts::default(),
        super::super::Write::FillGap,
    )
    .unwrap();
    let first = store_message(&store, "kept@x");
    for n in 0..=BATCH {
        let id = format!("m{n}@x");
        let mail = message(
            n as u32 + 2,
            &raw(&id, "<p>See src=cid:logo@x</p>", "text/html", ""),
        );
        store
            .upsert_message(&with_legacy_role(mail, false, PartRole::Inline))
            .unwrap();
    }
    let start = store.cid_repair_progress().unwrap();
    assert!(!store_message(&store, "m0@x").has_attachment);
    let batch = other.repair_cid_batch(start).unwrap();
    assert_eq!(batch.changed_threads.len(), BATCH);
    assert!(store_message(&store, "m0@x").has_attachment);
    let tx = immediate_transaction(&store.conn).unwrap();
    super::super::move_into(&tx, &account.id, &format!("m{BATCH}@x"), "kept@x")
        .unwrap()
        .unwrap();
    tx.execute(
        "DELETE FROM messages WHERE message_id = ?1",
        [format!("m{BATCH}@x")],
    )
    .unwrap();
    tx.commit().unwrap();

    let next = other.repair_cid_batch(batch.progress).unwrap();
    assert!(next.external_progress);
    assert_eq!(
        next.changed_threads,
        [(account.id.clone(), first.thread_id)]
    );
    assert!(store_message(&store, "kept@x").has_attachment);
    assert!(store.missing_bodies(&account.id, 10).unwrap().is_empty());
    drop(other);
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
