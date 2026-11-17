//! Local embedding index for message bodies.
//!
//! Each message stores one vector in `message_embeddings`, in the same SQLite
//! file as the mailbox and keyed by account. The vector is [`EMBEDDING_MODEL`],
//! a feature hash implemented in this crate. There is no weight file, no
//! extra embedding crate, and no network call.
//!
//! [`spawn_indexer`] fills missing rows on a background thread, recent mail
//! first. Opening the store, listing threads, and [`crate::Store::search`] do
//! not compute vectors. A different model id replaces rows left by this one.

#[cfg(test)]
#[cfg(test)]
use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{Error, Result};

/// Id stored on every vector this embedder writes.
///
/// This is code in this crate, not a downloaded or bundled weight file.
/// Changing the hash, the dimension, or the tokenization requires a new id.
/// The background indexer replaces rows whose id, dimension, or byte length differ.
pub const EMBEDDING_MODEL: &str = "local-hash-v1";

/// Length of [`embed_body`]. The stored blob is this many little-endian `f32`s.
pub const EMBEDDING_DIMS: usize = 64;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;
const VECTOR_BYTES: i64 = (EMBEDDING_DIMS * 4) as i64;

/// One stored body vector.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    /// [`EMBEDDING_MODEL`] for vectors this crate writes.
    pub model: String,
    /// Little-endian `f32` values, length [`EMBEDDING_DIMS`] for the local model.
    pub values: Vec<f32>,
}

const EMBED_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS message_embeddings (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    model TEXT NOT NULL,
    dims INTEGER NOT NULL,
    body_hash TEXT NOT NULL,
    vector BLOB NOT NULL,
    PRIMARY KEY (account_id, message_id)
);
"#;

/// Feature-hash the body into a unit vector.
///
/// Tokens are lowercase runs of letters and digits. The same letters produce
/// the same vector. An empty body is the zero vector. This does not read the
/// network or a model file.
pub fn embed_body(body: &str) -> Embedding {
    let mut values = vec![0.0f32; EMBEDDING_DIMS];
    let tokens = tokens(body);
    for (index, token) in tokens.iter().enumerate() {
        accumulate(&mut values, fnv_str(fnv_byte(FNV_OFFSET, 1), token));
        accumulate_trigrams(&mut values, token);
        if let Some(next) = tokens.get(index + 1) {
            let hash = fnv_str(
                fnv_byte(fnv_str(fnv_byte(FNV_OFFSET, 2), token), 0xff),
                next,
            );
            accumulate(&mut values, hash);
        }
    }
    normalize(&mut values);
    Embedding {
        model: EMBEDDING_MODEL.to_string(),
        values,
    }
}

/// Create the index table. This does not read message bodies.
pub(crate) fn ensure_embedding_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(EMBED_SCHEMA)?;
    Ok(())
}

/// Background indexer for one mailbox file.
///
/// Dropping it asks the thread to stop and waits for the current batch. The
/// batch is a few messages, so this does not sit on a large mailbox.
pub(crate) struct Indexer {
    stop: Arc<AtomicBool>,
    /// Tests flip this so open can return before the thread writes.
    #[cfg_attr(not(test), allow(dead_code))]
    pause: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Indexer {
    pub(crate) fn spawn(path: &Path) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(indexer_starts_paused()));
        let stop_thread = Arc::clone(&stop);
        let pause_thread = Arc::clone(&pause);
        let path = path.to_path_buf();
        let thread = thread::Builder::new()
            .name("fell-embeddings".into())
            .spawn(move || index_loop(path, stop_thread, pause_thread))
            .map_err(crate::Error::from)?;
        Ok(Self {
            stop,
            pause,
            thread: Some(thread),
        })
    }

    #[cfg(test)]
    pub(crate) fn pause(&self, paused: bool) {
        self.pause.store(paused, Ordering::Relaxed);
    }
}

impl Drop for Indexer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn index_loop(path: std::path::PathBuf, stop: Arc<AtomicBool>, pause: Arc<AtomicBool>) {
    let Ok(conn) = Connection::open(&path) else {
        return;
    };
    if conn.busy_timeout(Duration::from_secs(5)).is_err() {
        return;
    }
    if ensure_embedding_schema(&conn).is_err() {
        return;
    }
    let _ = delete_orphans(&conn);
    while !stop.load(Ordering::Relaxed) {
        if pause.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        match index_stale(&conn, INDEX_BATCH) {
            Ok(0) | Err(_) => thread::sleep(Duration::from_millis(50)),
            Ok(_) => {}
        }
    }
}

const INDEX_BATCH: usize = 32;

/// Fill up to `limit` missing or outdated body vectors, recent mail first.
///
/// Also drops embedding rows whose message is gone. Callers that must not
/// block the UI use [`Indexer`] instead of this function.
#[cfg(test)]
pub(crate) fn index_embeddings(conn: &Connection, limit: usize) -> Result<usize> {
    delete_orphans(conn)?;
    index_stale(conn, limit)
}

/// Write the vector for the body text that is stored on the message.
///
/// `body` is `body_text`, or the snippet when the body is null. The same text
/// and model leave the existing row in place.
pub(crate) fn sync_message_embedding(
    conn: &Connection,
    account_id: &str,
    message_id: &str,
    body: &str,
) -> Result<()> {
    let hash = body_hash(body);
    let existing: Option<(String, i64, String, i64)> = conn
        .query_row(
            "SELECT model, dims, body_hash, length(vector)
             FROM message_embeddings
             WHERE account_id = ?1 AND message_id = ?2",
            params![account_id, message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    if let Some((model, dims, stored_hash, nbytes)) = existing
        && model == EMBEDDING_MODEL
        && dims == EMBEDDING_DIMS as i64
        && stored_hash == hash
        && nbytes == VECTOR_BYTES
    {
        return Ok(());
    }
    let embedding = embed_body(body);
    let blob = encode_vector(&embedding.values);
    conn.execute(
        "INSERT INTO message_embeddings (
            account_id, message_id, model, dims, body_hash, vector
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
            model = excluded.model,
            dims = excluded.dims,
            body_hash = excluded.body_hash,
            vector = excluded.vector",
        params![
            account_id,
            message_id,
            EMBEDDING_MODEL,
            EMBEDDING_DIMS as i64,
            hash,
            blob
        ],
    )?;
    Ok(())
}

/// Drop a stored vector that no longer matches this body.
///
/// A missing row is left missing. The background indexer fills it. This does
/// not compute a vector.
pub(crate) fn invalidate_message_embedding(
    conn: &Connection,
    account_id: &str,
    message_id: &str,
    body: &str,
) -> Result<()> {
    let hash = body_hash(body);
    let existing: Option<(String, i64, String, i64)> = conn
        .query_row(
            "SELECT model, dims, body_hash, length(vector)
             FROM message_embeddings
             WHERE account_id = ?1 AND message_id = ?2",
            params![account_id, message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((model, dims, stored_hash, nbytes)) = existing else {
        return Ok(());
    };
    if model == EMBEDDING_MODEL
        && dims == EMBEDDING_DIMS as i64
        && stored_hash == hash
        && nbytes == VECTOR_BYTES
    {
        return Ok(());
    }
    remove_message_embedding(conn, account_id, message_id)
}

pub(crate) fn remove_message_embedding(
    conn: &Connection,
    account_id: &str,
    message_id: &str,
) -> Result<()> {
    conn.execute(
        "DELETE FROM message_embeddings WHERE account_id = ?1 AND message_id = ?2",
        params![account_id, message_id],
    )?;
    Ok(())
}

pub(crate) fn load_embedding(
    conn: &Connection,
    account_id: &str,
    message_id: &str,
) -> Result<Option<Embedding>> {
    let row: Option<(String, i64, Vec<u8>)> = conn
        .query_row(
            "SELECT model, dims, vector FROM message_embeddings
             WHERE account_id = ?1 AND message_id = ?2",
            params![account_id, message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    row.map(|(model, dims, blob)| decode_embedding(model, dims, &blob))
        .transpose()
}

fn delete_orphans(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM message_embeddings
         WHERE NOT EXISTS (
             SELECT 1 FROM messages m
             WHERE m.account_id = message_embeddings.account_id
               AND m.message_id = message_embeddings.message_id
         )",
        [],
    )?;
    Ok(())
}

fn index_stale(conn: &Connection, limit: usize) -> Result<usize> {
    if limit == 0 {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT m.account_id, m.message_id
         FROM messages m
         LEFT JOIN message_embeddings e
           ON e.account_id = m.account_id AND e.message_id = m.message_id
         WHERE e.message_id IS NULL
            OR e.model != ?1
            OR e.dims != ?2
            OR length(e.vector) != ?3
         ORDER BY m.date_unix DESC, m.rowid DESC
         LIMIT ?4",
    )?;
    let rows = stmt.query_map(
        params![
            EMBEDDING_MODEL,
            EMBEDDING_DIMS as i64,
            VECTOR_BYTES,
            i64::try_from(limit).unwrap_or(i64::MAX)
        ],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
    )?;
    let mut pending = Vec::new();
    for row in rows {
        pending.push(row?);
    }
    drop(stmt);
    let mut wrote = 0;
    for (index, (account_id, message_id)) in pending.into_iter().enumerate() {
        interrupt_rebuild(index)?;
        if write_current_body(conn, &account_id, &message_id)? {
            wrote += 1;
        }
    }
    Ok(wrote)
}

fn write_current_body(conn: &Connection, account_id: &str, message_id: &str) -> Result<bool> {
    let tx = conn.unchecked_transaction()?;
    let body: Option<String> = tx
        .query_row(
            "SELECT COALESCE(body_text, snippet)
             FROM messages
             WHERE account_id = ?1 AND message_id = ?2",
            params![account_id, message_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(body) = body else {
        remove_message_embedding(&tx, account_id, message_id)?;
        tx.commit()?;
        return Ok(false);
    };
    sync_message_embedding(&tx, account_id, message_id, &body)?;
    tx.commit()?;
    Ok(true)
}

fn indexer_starts_paused() -> bool {
    #[cfg(test)]
    {
        START_INDEXER_PAUSED.with(|cell| cell.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static START_INDEXER_PAUSED: Cell<bool> = const { Cell::new(false) };
}

/// When set, the next file-backed indexer starts paused so a test can observe
/// that open returned before any vector was written.
#[cfg(test)]
pub(crate) fn testing_start_indexer_paused(paused: bool) {
    START_INDEXER_PAUSED.with(|cell| cell.set(paused));
}

fn interrupt_rebuild(index: usize) -> Result<()> {
    #[cfg(test)]
    if stop_embedding_rebuild_at(index) {
        return Err(Error::Search("embedding rebuild interrupted".into()));
    }
    #[cfg(not(test))]
    let _ = index;
    Ok(())
}

#[cfg(test)]
thread_local! {
    static REBUILD_STOP_AT: Cell<Option<usize>> = const { Cell::new(None) };
}

#[cfg(test)]
fn stop_embedding_rebuild_at(index: usize) -> bool {
    REBUILD_STOP_AT.with(|cell| cell.get() == Some(index))
}

/// Test hook. `Some(n)` makes the embedding rebuild fail before message `n`.
#[cfg(test)]
pub(crate) fn testing_stop_embedding_rebuild_at(index: Option<usize>) {
    REBUILD_STOP_AT.with(|cell| cell.set(index));
}

fn decode_embedding(model: String, dims: i64, blob: &[u8]) -> Result<Embedding> {
    let dims =
        usize::try_from(dims).map_err(|_| Error::Search("embedding dims are negative".into()))?;
    let (chunks, rest) = blob.as_chunks::<4>();
    if !rest.is_empty() || chunks.len() != dims {
        return Err(Error::Search(
            "embedding vector length does not match its dims".into(),
        ));
    }
    let values = chunks
        .iter()
        .map(|bytes| f32::from_le_bytes(*bytes))
        .collect();
    Ok(Embedding { model, values })
}

fn encode_vector(values: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(values.len() * 4);
    for value in values {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

fn body_hash(body: &str) -> String {
    format!("{:016x}", fnv_str(FNV_OFFSET, body))
}

fn tokens(body: &str) -> Vec<String> {
    body.to_lowercase()
        .split(|ch: char| !ch.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_string)
        .collect()
}

fn accumulate_trigrams(values: &mut [f32], token: &str) {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() < 3 {
        return;
    }
    for window in chars.windows(3) {
        let mut hash = fnv_byte(FNV_OFFSET, 3);
        for ch in window {
            let mut buf = [0u8; 4];
            hash = fnv_str(hash, ch.encode_utf8(&mut buf));
            hash = fnv_byte(hash, 0);
        }
        accumulate(values, hash);
    }
}

fn accumulate(values: &mut [f32], hash: u64) {
    let dim = ((hash >> 1) % (EMBEDDING_DIMS as u64)) as usize;
    let sign = if hash & 1 == 0 { 1.0 } else { -1.0 };
    values[dim] += sign;
}

fn normalize(values: &mut [f32]) {
    let mut sum = 0.0f32;
    for value in &*values {
        sum += value * value;
    }
    let norm = sum.sqrt();
    if norm == 0.0 {
        return;
    }
    for value in values {
        *value /= norm;
    }
}

fn fnv_byte(hash: u64, byte: u8) -> u64 {
    (hash ^ u64::from(byte)).wrapping_mul(FNV_PRIME)
}

fn fnv_str(mut hash: u64, text: &str) -> u64 {
    for byte in text.as_bytes() {
        hash = fnv_byte(hash, *byte);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{Account, AccountId, BackendKind};
    use crate::actions::{FOLDER_INBOX, FOLDER_TRASH};
    use crate::message::{Message, MessageHeaders, MessageId, RemoteId};
    use crate::store::Store;
    use crate::threading::ThreadId;

    fn cosine(left: &Embedding, right: &Embedding) -> f32 {
        left.values
            .iter()
            .zip(&right.values)
            .map(|(a, b)| a * b)
            .sum()
    }

    fn account(id: &str) -> Account {
        Account {
            id: AccountId(id.into()),
            display_name: id.into(),
            email: format!("{id}@x"),
            backend_kind: BackendKind::Bridge,
        }
    }

    fn message(owner: &str, id: &str, body: &str) -> Message {
        Message {
            account_id: AccountId(owner.into()),
            headers: MessageHeaders {
                message_id: MessageId::new(id),
                proton_conversation_id: None,
                in_reply_to: None,
                references: Vec::new(),
                from: crate::message::Address {
                    name: None,
                    email: "from@x".into(),
                },
                subject: "Subject".into(),
                date_unix: 1_700_000_000,
            },
            remote_id: RemoteId(format!("{owner}:{id}")),
            thread_id: ThreadId::from_message_id(&MessageId::new(id)),
            snippet: body.into(),
            body_text: Some(body.into()),
            unread: true,
            folders: vec![FOLDER_INBOX.into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: Vec::new(),
            cc: Vec::new(),
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
        }
    }

    fn open(ids: &[&str]) -> Store {
        let store = Store::open_in_memory().unwrap();
        for id in ids {
            store.upsert_account(&account(id)).unwrap();
        }
        store
    }

    fn stored(store: &Store, owner: &str, id: &str) -> Embedding {
        store
            .message_embedding(&AccountId(owner.into()), &MessageId::new(id))
            .unwrap()
            .expect("embedding row")
    }

    fn embedding_count(store: &Store) -> i64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM message_embeddings", [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    fn index_all(store: &Store) {
        loop {
            let wrote = index_embeddings(&store.conn, 64).unwrap();
            if wrote == 0 {
                break;
            }
        }
    }

    fn wait_indexed(store: &Store, owner: &str, id: &str) -> Embedding {
        let start = std::time::Instant::now();
        loop {
            if let Some(embedding) = store
                .message_embedding(&AccountId(owner.into()), &MessageId::new(id))
                .unwrap()
                && embedding.model == EMBEDDING_MODEL
            {
                return embedding;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(2),
                "background indexer did not store {id}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    struct PauseReset;
    impl Drop for PauseReset {
        fn drop(&mut self) {
            testing_start_indexer_paused(false);
        }
    }

    #[test]
    fn embed_body_is_a_local_unit_vector() {
        let invoice = embed_body("Please pay the March invoice.");
        let again = embed_body("please pay the march invoice");
        let near = embed_body("please pay the invoice");
        let other = embed_body("team offsite calendar invite");
        assert_eq!(invoice.model, EMBEDDING_MODEL);
        assert_eq!(invoice.values.len(), EMBEDDING_DIMS);
        assert_eq!(invoice, again);
        assert_ne!(invoice, other);
        let norm: f32 = invoice.values.iter().map(|value| value * value).sum();
        assert!((norm.sqrt() - 1.0).abs() < 1e-5);
        assert!(cosine(&invoice, &near) > cosine(&invoice, &other));
        assert!(cosine(&invoice, &near) > 0.4);
        assert_eq!(embed_body(" \n\t!! ").values, vec![0.0; EMBEDDING_DIMS]);
    }

    #[test]
    fn upsert_stores_the_body_vector_not_the_snippet() {
        let store = open(&["a1"]);
        let mut message = message("a1", "m@x", "the real body");
        message.snippet = "snippet only".into();
        store.upsert_message(&message).unwrap();
        index_all(&store);
        let vector = stored(&store, "a1", "m@x");
        assert_eq!(vector, embed_body("the real body"));
        store.upsert_message(&message).unwrap();
        assert_eq!(stored(&store, "a1", "m@x"), vector);
        let blob: Vec<u8> = store
            .conn
            .query_row(
                "SELECT vector FROM message_embeddings WHERE account_id = 'a1' AND message_id = 'm@x'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(blob, encode_vector(&vector.values));
        assert_eq!(embedding_count(&store), 1);
    }

    #[test]
    fn missing_body_uses_the_snippet() {
        let store = open(&["a1"]);
        let mut message = message("a1", "m@x", "ignored");
        message.body_text = None;
        message.snippet = "snippet only".into();
        store.upsert_message(&message).unwrap();
        index_all(&store);
        assert_eq!(stored(&store, "a1", "m@x"), embed_body("snippet only"));
    }

    #[test]
    fn body_change_replaces_one_row_and_keyword_search_follows() {
        let store = open(&["a1"]);
        let id = AccountId("a1".into());
        store
            .upsert_message(&message("a1", "m@x", "alpha invoice"))
            .unwrap();
        index_all(&store);
        let mut updated = message("a1", "m@x", "beta calendar");
        updated.folders = vec![FOLDER_INBOX.into()];
        store.upsert_message(&updated).unwrap();
        assert_eq!(embedding_count(&store), 0);
        assert_eq!(store.search(&id, "calendar").unwrap().hits.len(), 1);
        assert!(store.search(&id, "invoice").unwrap().hits.is_empty());
        index_all(&store);
        assert_eq!(embedding_count(&store), 1);
        assert_eq!(stored(&store, "a1", "m@x"), embed_body("beta calendar"));
    }

    #[test]
    fn delete_drops_only_that_accounts_vector() {
        let store = open(&["a1", "a2"]);
        store
            .upsert_message(&message("a1", "same@x", "account one body"))
            .unwrap();
        store
            .upsert_message(&message("a2", "same@x", "account two body"))
            .unwrap();
        index_all(&store);
        assert!(
            store
                .delete_message(&AccountId("a1".into()), &MessageId::new("same@x"))
                .unwrap()
        );
        assert!(
            store
                .message_embedding(&AccountId("a1".into()), &MessageId::new("same@x"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            stored(&store, "a2", "same@x"),
            embed_body("account two body")
        );
        assert_eq!(embedding_count(&store), 1);
    }

    #[test]
    fn archive_keeps_the_vector_and_trash_expunge_restores_it() {
        let store = open(&["a1"]);
        let account_id = AccountId("a1".into());
        let thread = store
            .upsert_message(&message("a1", "m@x", "please pay the invoice"))
            .unwrap();
        index_all(&store);
        let before = stored(&store, "a1", "m@x");
        store
            .archive_thread(&account_id, &thread, FOLDER_INBOX)
            .unwrap();
        assert_eq!(stored(&store, "a1", "m@x"), before);

        let trash = store
            .upsert_message(&{
                let mut message = message("a1", "gone@x", "original secret body");
                message.folders = vec![FOLDER_TRASH.into()];
                message
            })
            .unwrap();
        store
            .delete_thread(&account_id, &trash, FOLDER_TRASH)
            .unwrap();
        assert!(
            store
                .message_embedding(&account_id, &MessageId::new("gone@x"))
                .unwrap()
                .is_none()
        );
        store.undo(&account_id).unwrap();
        index_all(&store);
        assert_eq!(
            stored(&store, "a1", "gone@x"),
            embed_body("original secret body")
        );
    }

    #[test]
    fn undo_of_expunge_rewrites_a_reinserted_body() {
        let store = open(&["a1"]);
        let account_id = AccountId("a1".into());
        let thread = store
            .upsert_message(&message("a1", "note@x", "original secret body"))
            .unwrap();
        store
            .delete_thread(&account_id, &thread, FOLDER_INBOX)
            .unwrap();
        store
            .delete_thread(&account_id, &thread, FOLDER_TRASH)
            .unwrap();
        store
            .upsert_message(&message("a1", "note@x", "replacement body"))
            .unwrap();
        index_all(&store);
        assert_eq!(
            stored(&store, "a1", "note@x"),
            embed_body("replacement body")
        );
        store.undo(&account_id).unwrap();
        index_all(&store);
        assert_eq!(
            stored(&store, "a1", "note@x"),
            embed_body("original secret body")
        );
        assert_eq!(embedding_count(&store), 1);
    }

    #[test]
    fn keyword_search_does_not_read_the_embedding_index() {
        let store = open(&["a1"]);
        let account_id = AccountId("a1".into());
        store
            .upsert_message(&message("a1", "m@x", "please pay the invoice"))
            .unwrap();
        index_all(&store);
        let before = store.search(&account_id, "invoice").unwrap();
        store
            .conn
            .execute("DELETE FROM message_embeddings", [])
            .unwrap();
        assert_eq!(store.search(&account_id, "invoice").unwrap(), before);
        assert!(
            store
                .message_embedding(&account_id, &MessageId::new("m@x"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn writes_do_not_embed_and_reads_do_not_wait() {
        let _reset = PauseReset;
        testing_start_indexer_paused(true);
        let dir = std::env::temp_dir().join(format!(
            "fell-embed-pause-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.sqlite");
        let store = Store::open(&path).unwrap();
        store.upsert_account(&account("a1")).unwrap();
        let account_id = AccountId("a1".into());
        store
            .upsert_message(&message("a1", "m@x", "please pay the invoice"))
            .unwrap();
        assert!(
            store
                .message_embedding(&account_id, &MessageId::new("m@x"))
                .unwrap()
                .is_none()
        );
        assert_eq!(store.search(&account_id, "invoice").unwrap().hits.len(), 1);
        assert_eq!(store.list_threads(&account_id).unwrap().len(), 1);
        store.testing_pause_indexer(false);
        assert_eq!(
            wait_indexed(&store, "a1", "m@x"),
            embed_body("please pay the invoice")
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recent_mail_is_indexed_first() {
        let store = open(&["a1"]);
        let mut older = message("a1", "old@x", "older invoice");
        older.headers.date_unix = 10;
        let mut newer = message("a1", "new@x", "newer calendar");
        newer.headers.date_unix = 50;
        store.upsert_message(&older).unwrap();
        store.upsert_message(&newer).unwrap();
        assert_eq!(index_embeddings(&store.conn, 1).unwrap(), 1);
        assert_eq!(stored(&store, "a1", "new@x"), embed_body("newer calendar"));
        assert!(
            store
                .message_embedding(&AccountId("a1".into()), &MessageId::new("old@x"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reopen_fills_missing_rows_and_drops_orphans() {
        let dir = std::env::temp_dir().join(format!(
            "fell-embed-reopen-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.sqlite");
        {
            let _reset = PauseReset;
            testing_start_indexer_paused(true);
            let store = Store::open(&path).unwrap();
            store.upsert_account(&account("a1")).unwrap();
            store
                .upsert_message(&message("a1", "kept@x", "please pay the invoice"))
                .unwrap();
            store
                .upsert_message(&message("a1", "drop@x", "goes away"))
                .unwrap();
            index_all(&store);
            store
                .conn
                .execute(
                    "DELETE FROM message_embeddings WHERE message_id = 'kept@x'",
                    [],
                )
                .unwrap();
            store
                .conn
                .execute("DELETE FROM messages WHERE message_id = 'drop@x'", [])
                .unwrap();
            assert_eq!(embedding_count(&store), 1);
        }
        testing_start_indexer_paused(false);
        let store = Store::open(&path).unwrap();
        assert_eq!(
            wait_indexed(&store, "a1", "kept@x"),
            embed_body("please pay the invoice")
        );
        assert!(
            store
                .message_embedding(&AccountId("a1".into()), &MessageId::new("drop@x"))
                .unwrap()
                .is_none()
        );
        assert_eq!(embedding_count(&store), 1);
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reopen_replaces_a_different_model_id() {
        let dir = std::env::temp_dir().join(format!(
            "fell-embed-model-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.sqlite");
        {
            let _reset = PauseReset;
            testing_start_indexer_paused(true);
            let store = Store::open(&path).unwrap();
            store.upsert_account(&account("a1")).unwrap();
            store
                .upsert_message(&message("a1", "m@x", "local body"))
                .unwrap();
            index_all(&store);
            store
                .conn
                .execute(
                    "UPDATE message_embeddings SET model = 'cloud-opt-in' WHERE message_id = 'm@x'",
                    [],
                )
                .unwrap();
        }
        testing_start_indexer_paused(false);
        let store = Store::open(&path).unwrap();
        assert_eq!(wait_indexed(&store, "a1", "m@x"), embed_body("local body"));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn interrupted_index_keeps_rows_already_written() {
        let store = open(&["a1"]);
        store
            .upsert_message(&message("a1", "a@x", "please pay the invoice"))
            .unwrap();
        store
            .upsert_message(&message("a1", "b@x", "team calendar"))
            .unwrap();
        store
            .conn
            .execute("DELETE FROM message_embeddings", [])
            .unwrap();

        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                testing_stop_embedding_rebuild_at(None);
            }
        }
        let _reset = Reset;
        testing_stop_embedding_rebuild_at(Some(1));
        let err = index_embeddings(&store.conn, 64).unwrap_err();
        assert!(err.to_string().contains("interrupted"));
        assert_eq!(embedding_count(&store), 1);

        testing_stop_embedding_rebuild_at(None);
        index_all(&store);
        assert_eq!(
            stored(&store, "a1", "a@x"),
            embed_body("please pay the invoice")
        );
        assert_eq!(stored(&store, "a1", "b@x"), embed_body("team calendar"));
    }
}
