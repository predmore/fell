//! Local embedding index for message bodies.
//!
//! Each message stores one vector in `message_embeddings`, in the same SQLite
//! file as the mailbox and keyed by account. The active embedder is
//! [`LocalHashV1`]. That is a placeholder: a lexical feature hash, not a
//! semantic model. There is no weight file and no network call.
//!
//! Vectors store the embedder's model id and version. [`spawn_indexer`]
//! replaces rows whose id or version differs, recent mail first, on a
//! background thread. Opening the store, listing threads, and
//! [`crate::Store::search`] do not compute vectors. This is not semantic search.

#[cfg(test)]
use std::cell::Cell;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::{Error, Result};

/// Placeholder model id. This is a lexical feature hash, not a semantic model.
///
/// A real local model replaces this id. The indexer then rewrites stored rows.
pub const EMBEDDING_MODEL: &str = "local-hash-v1";

/// Placeholder version stored beside [`EMBEDDING_MODEL`].
///
/// Bump it when the hash or the dimension changes. Rows with another version
/// are rewritten.
pub const EMBEDDING_VERSION: &str = "1";

/// Length of the [`LocalHashV1`] placeholder. The stored blob is this many
/// little-endian `f32`s. A later model can use a different length.
pub const EMBEDDING_DIMS: usize = 64;

/// Model id and version stored on each vector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbedderId {
    pub model: &'static str,
    pub version: &'static str,
}

/// Turns one message body into a vector.
///
/// The index stores [`BodyEmbedder::id`] with the bytes. A new id or version
/// makes the background indexer replace those rows.
pub trait BodyEmbedder {
    fn id(&self) -> EmbedderId;
    fn dims(&self) -> usize;
    fn embed(&self, body: &str) -> Vec<f32>;
}

/// Placeholder embedder. Lexical feature hash, not semantic search.
///
/// No weights, bundled or downloaded. The same letters produce the same vector.
#[derive(Debug, Clone, Copy, Default)]
pub struct LocalHashV1;

impl BodyEmbedder for LocalHashV1 {
    fn id(&self) -> EmbedderId {
        EmbedderId {
            model: EMBEDDING_MODEL,
            version: EMBEDDING_VERSION,
        }
    }

    fn dims(&self) -> usize {
        EMBEDDING_DIMS
    }

    fn embed(&self, body: &str) -> Vec<f32> {
        let mut values = vec![0.0f32; self.dims()];
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
        values
    }
}

fn active_embedder() -> LocalHashV1 {
    LocalHashV1
}

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// One stored body vector.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    /// [`EMBEDDING_MODEL`] for the placeholder. A later model stores its own id.
    pub model: String,
    /// [`EMBEDDING_VERSION`] for the placeholder.
    pub version: String,
    /// Little-endian `f32` values.
    pub values: Vec<f32>,
}

const EMBED_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS message_embeddings (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    model TEXT NOT NULL,
    model_version TEXT NOT NULL DEFAULT '',
    dims INTEGER NOT NULL,
    body_hash TEXT NOT NULL,
    vector BLOB NOT NULL,
    PRIMARY KEY (account_id, message_id)
);

CREATE TABLE IF NOT EXISTS embedding_queue (
    account_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    PRIMARY KEY (account_id, message_id)
);
"#;

/// Placeholder vector for `body`.
///
/// This is [`LocalHashV1`]: a lexical feature hash, not semantic search.
/// Tokens are lowercase runs of letters and digits. An empty body is the zero
/// vector. This does not read the network or a model file.
pub fn embed_body(body: &str) -> Embedding {
    let embedder = active_embedder();
    let id = embedder.id();
    Embedding {
        model: id.model.to_string(),
        version: id.version.to_string(),
        values: embedder.embed(body),
    }
}

/// Create the index table. This does not read message bodies.
pub(crate) fn ensure_embedding_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(EMBED_SCHEMA)?;
    ensure_model_version_column(conn)?;
    Ok(())
}

fn ensure_model_version_column(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(message_embeddings)")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for name in rows {
        if name? == "model_version" {
            return Ok(());
        }
    }
    drop(stmt);
    if let Err(err) = conn.execute(
        "ALTER TABLE message_embeddings
         ADD COLUMN model_version TEXT NOT NULL DEFAULT ''",
        [],
    ) && !err
        .to_string()
        .to_ascii_lowercase()
        .contains("duplicate column")
    {
        return Err(err.into());
    }
    Ok(())
}

/// Background indexer for one mailbox file.
///
/// Dropping it asks the thread to stop and waits for the current batch. The
/// batch is a few messages, so this does not sit on a large mailbox. The
/// sidecar lock is what stops a second process from starting another thread.
pub(crate) struct Indexer {
    stop: Arc<AtomicBool>,
    /// Tests flip this so open can return before the thread writes.
    #[cfg_attr(not(test), allow(dead_code))]
    pause: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    /// True only while this worker holds the embed lock.
    #[cfg_attr(not(test), allow(dead_code))]
    active: Arc<AtomicBool>,
}

impl Indexer {
    /// Watch the embed lock and index once this process can take it.
    ///
    /// `None` means the thread could not be started. The mailbox still opens.
    /// If another process holds the lock, this worker waits and starts when
    /// the lock is free. The lock is dropped when the worker stops.
    pub(crate) fn try_spawn(path: &Path) -> Option<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(AtomicBool::new(indexer_starts_paused()));
        let active = Arc::new(AtomicBool::new(false));
        let stop_thread = Arc::clone(&stop);
        let pause_thread = Arc::clone(&pause);
        let active_thread = Arc::clone(&active);
        let path = path.to_path_buf();
        let abort = abort_this_indexer();
        let thread = thread::Builder::new()
            .name("fell-embeddings".into())
            .spawn(move || {
                if abort {
                    if let Ok(Some(lock)) = try_embed_lock(&path) {
                        active_thread.store(true, Ordering::Relaxed);
                        drop(lock);
                        active_thread.store(false, Ordering::Relaxed);
                    }
                    return;
                }
                loop {
                    if stop_thread.load(Ordering::Relaxed) {
                        return;
                    }
                    match try_embed_lock(&path) {
                        Ok(Some(lock)) => {
                            active_thread.store(true, Ordering::Relaxed);
                            index_loop(&path, &stop_thread, &pause_thread);
                            active_thread.store(false, Ordering::Relaxed);
                            drop(lock);
                            return;
                        }
                        _ => sleep_unless_stopped(BUSY_RETRY, &stop_thread),
                    }
                }
            })
            .ok()?;
        Some(Self {
            stop,
            pause,
            thread: Some(thread),
            active,
        })
    }

    #[cfg(test)]
    pub(crate) fn is_running(&self) -> bool {
        self.active.load(Ordering::Relaxed)
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

fn open_index_conn(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(Duration::from_secs(5))?;
    ensure_embedding_schema(&conn)?;
    delete_orphans(&conn)?;
    Ok(conn)
}

fn abort_this_indexer() -> bool {
    #[cfg(test)]
    {
        ABORT_THIS_INDEXER.with(|cell| cell.get())
    }
    #[cfg(not(test))]
    {
        false
    }
}

#[cfg(test)]
thread_local! {
    static ABORT_THIS_INDEXER: Cell<bool> = const { Cell::new(false) };
}

/// The next file-backed worker exits immediately and drops its embed lock.
#[cfg(test)]
pub(crate) fn testing_abort_next_indexer(abort: bool) {
    ABORT_THIS_INDEXER.with(|cell| cell.set(abort));
}

fn try_embed_lock(db: &Path) -> Result<Option<Connection>> {
    let path = embed_lock_path(db);
    create_lock_file(&path)?;
    let conn = Connection::open(&path)?;
    // Fail at once when another process already holds the exclusive lock.
    conn.busy_timeout(Duration::ZERO)?;
    match conn.execute_batch("BEGIN EXCLUSIVE") {
        Ok(()) => {
            restrict_lock_mode(&path)?;
            Ok(Some(conn))
        }
        Err(err) if sqlite_lock_held(&err) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn embed_lock_path(db: &Path) -> std::path::PathBuf {
    let mut name = db.as_os_str().to_os_string();
    name.push(".embed.lock");
    std::path::PathBuf::from(name)
}

fn sqlite_lock_held(err: &rusqlite::Error) -> bool {
    match err {
        rusqlite::Error::SqliteFailure(code, _) => {
            code.code == rusqlite::ErrorCode::DatabaseBusy
                || code.code == rusqlite::ErrorCode::DatabaseLocked
        }
        _ => false,
    }
}

fn create_lock_file(path: &Path) -> Result<()> {
    if !path.exists() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
            {
                Ok(_) => return Ok(()),
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(err.into()),
            }
        }
        #[cfg(not(unix))]
        {
            if let Err(err) = std::fs::File::create(path)
                && err.kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(err.into());
            }
        }
    }
    restrict_lock_mode(path)
}

fn restrict_lock_mode(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if path.exists() {
            let mut perms = std::fs::metadata(path)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(path, perms)?;
        }
        let mut journal = path.as_os_str().to_os_string();
        journal.push("-journal");
        let journal = std::path::PathBuf::from(journal);
        if journal.exists() {
            let mut perms = std::fs::metadata(&journal)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&journal, perms)?;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn index_loop(path: &Path, stop: &AtomicBool, pause: &AtomicBool) {
    let conn = loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        match open_index_conn(path) {
            Ok(conn) => break conn,
            Err(_) => sleep_unless_stopped(BUSY_RETRY, stop),
        }
    };
    let mut scan = Scan::default();
    let mut idle = IDLE_MIN;
    while !stop.load(Ordering::Relaxed) {
        if pause.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(5));
            continue;
        }
        let kind = match scan.step(&conn, INDEX_BATCH) {
            Ok(IndexStep::Wrote(_)) => AfterBatch::Progress,
            Ok(IndexStep::Idle) => AfterBatch::Idle,
            // Busy and snapshot errors are not an empty index. Retry soon.
            Err(_) => AfterBatch::Retry,
        };
        let (next_idle_wait, delay) = pause_after(kind, idle);
        idle = next_idle_wait;
        sleep_unless_stopped(delay, stop);
    }
}

const INDEX_BATCH: usize = 32;
const IDLE_MIN: Duration = Duration::from_secs(1);
const IDLE_MAX: Duration = Duration::from_secs(30);
const BATCH_YIELD: Duration = Duration::from_millis(20);
const BUSY_RETRY: Duration = Duration::from_millis(200);

#[derive(Clone, Copy)]
enum AfterBatch {
    Progress,
    Idle,
    Retry,
}

/// `(next idle wait, delay before the next batch)`.
///
/// Only an empty pass grows the idle wait. A lock or snapshot error keeps it.
fn pause_after(kind: AfterBatch, idle: Duration) -> (Duration, Duration) {
    match kind {
        AfterBatch::Progress => (IDLE_MIN, BATCH_YIELD),
        AfterBatch::Idle => (next_idle(idle), idle),
        AfterBatch::Retry => (idle, BUSY_RETRY),
    }
}

fn next_idle(current: Duration) -> Duration {
    current.saturating_mul(2).clamp(IDLE_MIN, IDLE_MAX)
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let step = Duration::from_millis(50);
    let mut left = total;
    while left > Duration::ZERO && !stop.load(Ordering::Relaxed) {
        let slice = left.min(step);
        thread::sleep(slice);
        left = left.saturating_sub(slice);
    }
}

/// Fill up to `limit` missing or outdated body vectors, recent mail first.
///
/// Also drops embedding rows whose message is gone. Callers that must not
/// block the UI use [`Indexer`] instead of this function.
#[cfg(test)]
pub(crate) fn index_embeddings(conn: &Connection, limit: usize) -> Result<usize> {
    delete_orphans(conn)?;
    match Scan::default().step(conn, limit)? {
        IndexStep::Wrote(n) => Ok(n),
        IndexStep::Idle => Ok(0),
    }
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
    let embedder = active_embedder();
    let id = embedder.id();
    let hash = body_hash(body);
    let dims = i64::try_from(embedder.dims()).unwrap_or(i64::MAX);
    let expected_bytes = dims.saturating_mul(4);
    let existing: Option<(String, String, i64, String, i64)> = conn
        .query_row(
            "SELECT model, model_version, dims, body_hash, length(vector)
             FROM message_embeddings
             WHERE account_id = ?1 AND message_id = ?2",
            params![account_id, message_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((model, version, stored_dims, stored_hash, nbytes)) = existing else {
        return Ok(());
    };
    if model == id.model
        && version == id.version
        && stored_dims == dims
        && stored_hash == hash
        && nbytes == expected_bytes
    {
        return Ok(());
    }
    remove_message_embedding(conn, account_id, message_id)?;
    enqueue_message(conn, account_id, message_id)
}

pub(crate) fn enqueue_message(conn: &Connection, account_id: &str, message_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO embedding_queue (account_id, message_id) VALUES (?1, ?2)
         ON CONFLICT(account_id, message_id) DO NOTHING",
        params![account_id, message_id],
    )?;
    Ok(())
}

pub(crate) fn dequeue_message(conn: &Connection, account_id: &str, message_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM embedding_queue WHERE account_id = ?1 AND message_id = ?2",
        params![account_id, message_id],
    )?;
    Ok(())
}

fn select_queued(conn: &Connection, limit: usize) -> Result<Vec<StaleRow>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT m.rowid, m.date_unix, m.account_id, m.message_id
         FROM embedding_queue q
         JOIN messages m
           ON m.account_id = q.account_id AND m.message_id = q.message_id
         ORDER BY m.date_unix DESC, m.rowid DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![limit], |row| {
        Ok(StaleRow {
            key: MailKey {
                rowid: row.get(0)?,
                date_unix: row.get(1)?,
            },
            account_id: row.get(2)?,
            message_id: row.get(3)?,
        })
    })?;
    let mut pending = Vec::new();
    for row in rows {
        pending.push(row?);
    }
    Ok(pending)
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
    let row: Option<(String, String, i64, Vec<u8>)> = conn
        .query_row(
            "SELECT model, model_version, dims, vector FROM message_embeddings
             WHERE account_id = ?1 AND message_id = ?2",
            params![account_id, message_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    row.map(|(model, version, dims, blob)| decode_embedding(model, version, dims, &blob))
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

#[derive(Clone, Copy)]
struct MailKey {
    date_unix: i64,
    rowid: i64,
}

struct StaleRow {
    key: MailKey,
    account_id: String,
    message_id: String,
}

enum Bound {
    Any,
    Older(MailKey),
    Newer(MailKey),
    /// Strictly after `after` and strictly before `before`.
    Between {
        after: MailKey,
        before: MailKey,
    },
}

enum IndexStep {
    Wrote(#[allow(dead_code)] usize),
    Idle,
}

/// One newest-to-oldest pass over rows that still need a vector.
///
/// `before` is the oldest key already taken. The next older batch starts
/// strictly below it. `high` stays the floor of that pass. Mail newer than
/// `high` is paged downward from `newer_before` until a short batch shows the
/// range is exhausted, and only then does the walk continue below `before`.
#[derive(Default)]
struct Scan {
    before: Option<MailKey>,
    high: Option<MailKey>,
    /// Exclusive upper bound of the next page above `high`. `None` starts at
    /// the newest row above the floor.
    newer_before: Option<MailKey>,
}

impl Scan {
    fn step(&mut self, conn: &Connection, limit: usize) -> Result<IndexStep> {
        if limit == 0 {
            return Ok(IndexStep::Idle);
        }
        let queued = select_queued(conn, limit)?;
        if !queued.is_empty() {
            let wrote = write_batch(conn, &queued)?;
            return Ok(IndexStep::Wrote(wrote));
        }
        if let Some(high) = self.high {
            let bound = match self.newer_before {
                Some(upper) => Bound::Between {
                    after: high,
                    before: upper,
                },
                None => Bound::Newer(high),
            };
            let newer = select_stale(conn, limit, bound)?;
            if !newer.is_empty() {
                let wrote = write_batch(conn, &newer)?;
                if newer.len() == limit {
                    self.newer_before = newer.last().map(|row| row.key);
                } else {
                    self.newer_before = None;
                }
                return Ok(IndexStep::Wrote(wrote));
            }
            self.newer_before = None;
        }
        let bound = match self.before {
            Some(key) => Bound::Older(key),
            None => Bound::Any,
        };
        let older = select_stale(conn, limit, bound)?;
        if !older.is_empty() {
            let wrote = write_batch(conn, &older)?;
            if self.high.is_none() {
                self.high = older.first().map(|row| row.key);
            }
            self.before = older.last().map(|row| row.key);
            return Ok(IndexStep::Wrote(wrote));
        }
        if self.before.is_some() || self.high.is_some() {
            self.before = None;
            self.high = None;
            self.newer_before = None;
            let top = select_stale(conn, limit, Bound::Any)?;
            if top.is_empty() {
                return Ok(IndexStep::Idle);
            }
            let wrote = write_batch(conn, &top)?;
            self.high = top.first().map(|row| row.key);
            self.before = top.last().map(|row| row.key);
            return Ok(IndexStep::Wrote(wrote));
        }
        Ok(IndexStep::Idle)
    }
}

fn select_stale(conn: &Connection, limit: usize, bound: Bound) -> Result<Vec<StaleRow>> {
    let embedder = active_embedder();
    let id = embedder.id();
    let dims = i64::try_from(embedder.dims()).unwrap_or(i64::MAX);
    let bytes = dims.saturating_mul(4);
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let stale = "e.message_id IS NULL
                OR e.model != ?1
                OR e.model_version != ?2
                OR e.dims != ?3
                OR length(e.vector) != ?4";
    let (sql, keys) = match bound {
        Bound::Any => (
            format!(
                "SELECT m.rowid, m.date_unix, m.account_id, m.message_id
                 FROM messages m
                 LEFT JOIN message_embeddings e
                   ON e.account_id = m.account_id AND e.message_id = m.message_id
                 WHERE {stale}
                 ORDER BY m.date_unix DESC, m.rowid DESC
                 LIMIT ?5"
            ),
            Vec::new(),
        ),
        Bound::Older(key) => (
            format!(
                "SELECT m.rowid, m.date_unix, m.account_id, m.message_id
                 FROM messages m
                 LEFT JOIN message_embeddings e
                   ON e.account_id = m.account_id AND e.message_id = m.message_id
                 WHERE ({stale})
                   AND (m.date_unix < ?6 OR (m.date_unix = ?6 AND m.rowid < ?7))
                 ORDER BY m.date_unix DESC, m.rowid DESC
                 LIMIT ?5"
            ),
            vec![key.date_unix, key.rowid],
        ),
        Bound::Newer(key) => (
            format!(
                "SELECT m.rowid, m.date_unix, m.account_id, m.message_id
                 FROM messages m
                 LEFT JOIN message_embeddings e
                   ON e.account_id = m.account_id AND e.message_id = m.message_id
                 WHERE ({stale})
                   AND (m.date_unix > ?6 OR (m.date_unix = ?6 AND m.rowid > ?7))
                 ORDER BY m.date_unix DESC, m.rowid DESC
                 LIMIT ?5"
            ),
            vec![key.date_unix, key.rowid],
        ),
        Bound::Between { after, before } => (
            format!(
                "SELECT m.rowid, m.date_unix, m.account_id, m.message_id
                 FROM messages m
                 LEFT JOIN message_embeddings e
                   ON e.account_id = m.account_id AND e.message_id = m.message_id
                 WHERE ({stale})
                   AND (m.date_unix > ?6 OR (m.date_unix = ?6 AND m.rowid > ?7))
                   AND (m.date_unix < ?8 OR (m.date_unix = ?8 AND m.rowid < ?9))
                 ORDER BY m.date_unix DESC, m.rowid DESC
                 LIMIT ?5"
            ),
            vec![after.date_unix, after.rowid, before.date_unix, before.rowid],
        ),
    };
    let mut stmt = conn.prepare(&sql)?;
    let mapped = |row: &rusqlite::Row<'_>| {
        Ok(StaleRow {
            key: MailKey {
                rowid: row.get(0)?,
                date_unix: row.get(1)?,
            },
            account_id: row.get(2)?,
            message_id: row.get(3)?,
        })
    };
    let rows = match keys.as_slice() {
        [] => stmt.query_map(params![id.model, id.version, dims, bytes, limit], mapped)?,
        [date, rowid] => stmt.query_map(
            params![id.model, id.version, dims, bytes, limit, date, rowid],
            mapped,
        )?,
        [after_date, after_rowid, before_date, before_rowid] => stmt.query_map(
            params![
                id.model,
                id.version,
                dims,
                bytes,
                limit,
                after_date,
                after_rowid,
                before_date,
                before_rowid
            ],
            mapped,
        )?,
        _ => stmt.query_map(params![id.model, id.version, dims, bytes, limit], mapped)?,
    };
    let mut pending = Vec::new();
    for row in rows {
        pending.push(row?);
    }
    Ok(pending)
}

struct Prepared {
    account_id: String,
    message_id: String,
    body: Option<String>,
    values: Vec<f32>,
}

fn write_batch(conn: &Connection, rows: &[StaleRow]) -> Result<usize> {
    // Hash outside the write lock. A multi-megabyte batch must not hold it
    // for the whole hash, or sync and triage outlive the 5s busy timeout.
    let mut prepared = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        interrupt_rebuild(index)?;
        let body = message_body(conn, &row.account_id, &row.message_id)?;
        let values = match &body {
            Some(body) => active_embedder().embed(body),
            None => Vec::new(),
        };
        prepared.push(Prepared {
            account_id: row.account_id.clone(),
            message_id: row.message_id.clone(),
            body,
            values,
        });
    }
    if !conn.is_autocommit() {
        conn.execute_batch("ROLLBACK")?;
    }
    conn.execute_batch("BEGIN IMMEDIATE")?;
    let wrote = store_prepared(conn, &prepared);
    match wrote {
        Ok(n) => match conn.execute_batch("COMMIT") {
            Ok(()) => Ok(n),
            Err(err) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(err.into())
            }
        },
        Err(err) => {
            let _ = conn.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

fn store_prepared(conn: &Connection, rows: &[Prepared]) -> Result<usize> {
    let mut wrote = 0;
    for row in rows {
        let Some(hashed) = row.body.as_deref() else {
            remove_message_embedding(conn, &row.account_id, &row.message_id)?;
            dequeue_message(conn, &row.account_id, &row.message_id)?;
            continue;
        };
        let Some(current) = message_body(conn, &row.account_id, &row.message_id)? else {
            remove_message_embedding(conn, &row.account_id, &row.message_id)?;
            dequeue_message(conn, &row.account_id, &row.message_id)?;
            continue;
        };
        if current != hashed {
            enqueue_message(conn, &row.account_id, &row.message_id)?;
            continue;
        }
        store_embedding(conn, &row.account_id, &row.message_id, hashed, &row.values)?;
        dequeue_message(conn, &row.account_id, &row.message_id)?;
        wrote += 1;
    }
    Ok(wrote)
}

fn message_body(conn: &Connection, account_id: &str, message_id: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT COALESCE(body_text, snippet)
         FROM messages
         WHERE account_id = ?1 AND message_id = ?2",
        params![account_id, message_id],
        |row| row.get(0),
    )
    .optional()
    .map_err(Into::into)
}

fn store_embedding(
    conn: &Connection,
    account_id: &str,
    message_id: &str,
    body: &str,
    values: &[f32],
) -> Result<()> {
    let id = active_embedder().id();
    let hash = body_hash(body);
    let dims = i64::try_from(values.len()).unwrap_or(i64::MAX);
    conn.execute(
        "INSERT INTO message_embeddings (
            account_id, message_id, model, model_version, dims, body_hash, vector
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(account_id, message_id) DO UPDATE SET
            model = excluded.model,
            model_version = excluded.model_version,
            dims = excluded.dims,
            body_hash = excluded.body_hash,
            vector = excluded.vector",
        params![
            account_id,
            message_id,
            id.model,
            id.version,
            dims,
            hash,
            encode_vector(values)
        ],
    )?;
    Ok(())
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

fn decode_embedding(model: String, version: String, dims: i64, blob: &[u8]) -> Result<Embedding> {
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
    Ok(Embedding {
        model,
        version,
        values,
    })
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

    fn wait_until(mut ready: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while !ready() {
            assert!(
                start.elapsed() < std::time::Duration::from_secs(2),
                "timed out waiting for the embedding indexer"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
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
                && embedding.version == EMBEDDING_VERSION
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
        assert_eq!(invoice.version, EMBEDDING_VERSION);
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
    fn a_second_open_does_not_start_another_indexer() {
        let _reset = PauseReset;
        testing_start_indexer_paused(true);
        let dir = std::env::temp_dir().join(format!(
            "fell-embed-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.sqlite");
        let first = Store::open(&path).unwrap();
        wait_until(|| first.embedding_indexer_running());
        let second = Store::open(&path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!second.embedding_indexer_running());
        second.upsert_account(&account("a1")).unwrap();
        second
            .upsert_message(&message("a1", "m@x", "please pay the invoice"))
            .unwrap();
        assert_eq!(
            second
                .search(&AccountId("a1".into()), "invoice")
                .unwrap()
                .hits
                .len(),
            1
        );
        drop(first);
        wait_until(|| second.embedding_indexer_running());
        let third = Store::open(&path).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!third.embedding_indexer_running());
        drop(second);
        drop(third);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn upsert_survives_a_concurrent_writer_on_wal() {
        let _reset = PauseReset;
        testing_start_indexer_paused(true);
        let dir = std::env::temp_dir().join(format!(
            "fell-embed-wal-write-{}-{}",
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
        let path = path.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stop_writer = std::sync::Arc::clone(&stop);
        let writer = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.busy_timeout(std::time::Duration::from_millis(50))
                .unwrap();
            while !stop_writer.load(std::sync::atomic::Ordering::Relaxed) {
                if conn.execute_batch("BEGIN IMMEDIATE").is_err() {
                    continue;
                }
                let _ = conn.execute("UPDATE messages SET snippet = snippet", []);
                let _ = conn.execute_batch("COMMIT");
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        });
        for n in 0..30 {
            let mut message = message("a1", &format!("m{n}@x"), "body");
            message.headers.date_unix = n;
            store.upsert_message(&message).unwrap();
        }
        assert_eq!(
            store.list_threads(&AccountId("a1".into())).unwrap().len(),
            30
        );
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer.join().unwrap();
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn open_skips_the_indexer_when_the_lock_file_cannot_be_created() {
        let dir = std::env::temp_dir().join(format!(
            "fell-embed-bad-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.sqlite");
        std::fs::create_dir(embed_lock_path(&path)).unwrap();
        let store = Store::open(&path).unwrap();
        assert!(!store.embedding_indexer_running());
        store.upsert_account(&account("a1")).unwrap();
        assert!(
            store
                .list_threads(&AccountId("a1".into()))
                .unwrap()
                .is_empty()
        );
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_worker_that_exits_releases_the_embed_lock() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                testing_abort_next_indexer(false);
            }
        }
        let _reset = Reset;
        testing_abort_next_indexer(true);
        let dir = std::env::temp_dir().join(format!(
            "fell-embed-release-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.sqlite");
        let store = Store::open(&path).unwrap();
        let lock_path = embed_lock_path(&path);
        let start = std::time::Instant::now();
        loop {
            let conn = rusqlite::Connection::open(&lock_path).unwrap();
            conn.busy_timeout(std::time::Duration::ZERO).unwrap();
            match conn.execute_batch("BEGIN EXCLUSIVE") {
                Ok(()) => break,
                Err(err) if sqlite_lock_held(&err) => {
                    assert!(
                        start.elapsed() < std::time::Duration::from_secs(2),
                        "embed lock stayed held after the worker exited"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(err) => panic!("{err}"),
            }
        }
        wait_until(|| !store.embedding_indexer_running());
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
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
    fn a_different_model_version_is_rewritten() {
        let store = open(&["a1"]);
        store
            .upsert_message(&message("a1", "m@x", "local body"))
            .unwrap();
        index_all(&store);
        store
            .conn
            .execute(
                "UPDATE message_embeddings SET model_version = '0' WHERE message_id = 'm@x'",
                [],
            )
            .unwrap();
        index_all(&store);
        let stored = stored(&store, "a1", "m@x");
        assert_eq!(stored.version, EMBEDDING_VERSION);
        assert_eq!(stored, embed_body("local body"));
    }

    #[test]
    fn model_version_column_is_added_to_an_older_index() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE message_embeddings (
                account_id TEXT NOT NULL,
                message_id TEXT NOT NULL,
                model TEXT NOT NULL,
                dims INTEGER NOT NULL,
                body_hash TEXT NOT NULL,
                vector BLOB NOT NULL,
                PRIMARY KEY (account_id, message_id)
             );",
        )
        .unwrap();
        ensure_embedding_schema(&conn).unwrap();
        let mut stmt = conn
            .prepare("PRAGMA table_info(message_embeddings)")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get(1))
            .unwrap()
            .map(|name| name.unwrap())
            .collect();
        assert!(names.iter().any(|name| name == "model_version"));
    }

    #[test]
    fn idle_backoff_grows_and_caps() {
        assert_eq!(next_idle(IDLE_MIN), Duration::from_secs(2));
        assert_eq!(next_idle(Duration::from_secs(16)), IDLE_MAX);
        assert_eq!(next_idle(IDLE_MAX), IDLE_MAX);
        let (idle, delay) = pause_after(AfterBatch::Retry, Duration::from_secs(8));
        assert_eq!(idle, Duration::from_secs(8));
        assert_eq!(delay, BUSY_RETRY);
        let (idle, delay) = pause_after(AfterBatch::Idle, Duration::from_secs(8));
        assert_eq!(delay, Duration::from_secs(8));
        assert_eq!(idle, Duration::from_secs(16));
    }

    #[test]
    fn catch_up_resumes_below_the_row_it_just_stored() {
        let store = open(&["a1"]);
        for (id, date) in [("new@x", 30_i64), ("mid@x", 20), ("old@x", 10)] {
            let mut message = message("a1", id, id);
            message.headers.date_unix = date;
            store.upsert_message(&message).unwrap();
        }
        let mut scan = Scan::default();
        assert!(matches!(
            scan.step(&store.conn, 1).unwrap(),
            IndexStep::Wrote(1)
        ));
        assert!(stored(&store, "a1", "new@x").model == EMBEDDING_MODEL);
        assert!(
            store
                .message_embedding(&AccountId("a1".into()), &MessageId::new("mid@x"))
                .unwrap()
                .is_none()
        );
        let mut arrived = message("a1", "later@x", "later mail");
        arrived.headers.date_unix = 40;
        store.upsert_message(&arrived).unwrap();
        assert!(matches!(
            scan.step(&store.conn, 1).unwrap(),
            IndexStep::Wrote(1)
        ));
        assert_eq!(stored(&store, "a1", "later@x"), embed_body("later mail"));
        assert!(
            store
                .message_embedding(&AccountId("a1".into()), &MessageId::new("mid@x"))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            scan.step(&store.conn, 1).unwrap(),
            IndexStep::Wrote(1)
        ));
        assert_eq!(stored(&store, "a1", "mid@x").version, EMBEDDING_VERSION);
        assert!(
            store
                .message_embedding(&AccountId("a1".into()), &MessageId::new("old@x"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_newer_burst_is_paged_without_skipping_the_tail() {
        let store = open(&["a1"]);
        let mut floor = message("a1", "floor@x", "floor");
        floor.headers.date_unix = 1;
        store.upsert_message(&floor).unwrap();
        let mut scan = Scan::default();
        assert!(matches!(
            scan.step(&store.conn, 1).unwrap(),
            IndexStep::Wrote(1)
        ));
        for (id, date) in [("a@x", 10_i64), ("b@x", 20), ("c@x", 30)] {
            let mut message = message("a1", id, id);
            message.headers.date_unix = date;
            store.upsert_message(&message).unwrap();
        }
        // The queue would hide a cursor bug. This pass has to see the rows itself.
        store
            .conn
            .execute("DELETE FROM embedding_queue", [])
            .unwrap();
        assert!(matches!(
            scan.step(&store.conn, 2).unwrap(),
            IndexStep::Wrote(2)
        ));
        assert_eq!(stored(&store, "a1", "c@x").version, EMBEDDING_VERSION);
        assert_eq!(stored(&store, "a1", "b@x").version, EMBEDDING_VERSION);
        assert!(
            store
                .message_embedding(&AccountId("a1".into()), &MessageId::new("a@x"))
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            scan.step(&store.conn, 2).unwrap(),
            IndexStep::Wrote(1)
        ));
        assert_eq!(stored(&store, "a1", "a@x").version, EMBEDDING_VERSION);
    }

    #[test]
    fn a_failed_batch_does_not_keep_the_write_lock() {
        let store = open(&["a1"]);
        store
            .upsert_message(&message("a1", "m@x", "please pay the invoice"))
            .unwrap();
        store.conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(!store.conn.is_autocommit());
        assert_eq!(index_embeddings(&store.conn, 4).unwrap(), 1);
        assert!(store.conn.is_autocommit());
        assert_eq!(
            stored(&store, "a1", "m@x"),
            embed_body("please pay the invoice")
        );
    }

    #[test]
    fn interrupted_batch_does_not_commit() {
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
        assert_eq!(embedding_count(&store), 0);

        testing_stop_embedding_rebuild_at(None);
        index_all(&store);
        assert_eq!(
            stored(&store, "a1", "a@x"),
            embed_body("please pay the invoice")
        );
        assert_eq!(stored(&store, "a1", "b@x"), embed_body("team calendar"));
    }
}
