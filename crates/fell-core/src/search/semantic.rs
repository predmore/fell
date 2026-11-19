//! The semantic half of hybrid search.
//!
//! Search embeds the query only with a real sentence model, never with the
//! `local-hash-v1` placeholder. When no model is installed this module is
//! [`State::Off`] from the start and search is exactly keyword search.
//!
//! The vectors the indexer stored for that model are kept in memory as one
//! packed int8 matrix, one row per message with a scale per row (384 bytes
//! plus 4 for a MiniLM row, so about 39 MB at 100k messages). A query is a
//! brute-force dot product over the rows of one account. The matrix is loaded
//! once per store, in one read transaction, and then follows the index
//! through `embedding_changes`: every write to `message_embeddings` appends
//! the message's key there (a trigger), whichever process the indexer runs
//! in. Each search reads the entries past the last one it applied. A search
//! that fell behind further than the log keeps loads the matrix again.
//!
//! Everything here reads. In WAL mode a read never waits for a writer, so
//! search does not block on the indexer or sync.

use std::cmp::{Ordering, Reverse};
use std::collections::{BinaryHeap, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

use crate::Result;
use crate::embed::{CHANGES_KEPT, EmbedderId, EmbedderSource, SendEmbedder};

/// Cosine similarity below which a message is not related to the query.
/// Unrelated mail scores about 0 to 0.2 with all-MiniLM-L6-v2.
pub(crate) const MIN_SIMILARITY: f32 = 0.3;
/// Nearest messages the scan hands on, before they are grouped by thread.
pub(crate) const CANDIDATES: usize = 300;
/// Threads the semantic side brings to the fusion.
pub(crate) const SEMANTIC_THREADS: usize = 50;
/// How long a model that is installed but did not load stays off before a
/// search tries it again.
const RETRY_LOAD: Duration = Duration::from_secs(60);

/// Query model and vector matrix for one store.
pub(crate) struct Semantic {
    source: EmbedderSource,
    state: State,
    /// Tests: [`Semantic::warm`] was called, whatever it then did.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) warm_asked: bool,
}

enum State {
    /// A model is installed. Nothing is loaded until a search or a warm-up.
    Unstarted,
    /// Loading on a background thread.
    Warming(JoinHandle<State>),
    Ready(Box<Ready>),
    /// No model is installed, or the installed one is the placeholder.
    Off,
    /// The files are there but did not load. Tried again after [`RETRY_LOAD`].
    Failed(Instant),
}

/// What a search needs: the query model and the matrix of its vectors.
pub(crate) struct Ready {
    embedder: SendEmbedder,
    pub(crate) index: VectorIndex,
}

impl Semantic {
    pub(crate) fn new(source: EmbedderSource) -> Self {
        // A stat, not a hash. The hash is checked when the model is loaded.
        let state = if source.may_be_semantic() {
            State::Unstarted
        } else {
            State::Off
        };
        Self {
            source,
            state,
            #[cfg(any(test, feature = "test-utils"))]
            warm_asked: false,
        }
    }

    /// Start loading the model and the matrix on a background thread with
    /// its own read-only connection, so the first search does not wait for
    /// it. Does nothing when there is no model or it is already loading.
    pub(crate) fn warm(&mut self, path: Option<&Path>) {
        #[cfg(any(test, feature = "test-utils"))]
        {
            self.warm_asked = true;
        }
        let (State::Unstarted, Some(path)) = (&self.state, path) else {
            return;
        };
        let source = self.source.clone();
        let path = path.to_path_buf();
        let spawned = std::thread::Builder::new()
            .name("fell-search-warm".into())
            .spawn(move || match open_reader(&path) {
                Ok(conn) => load(&source, &conn),
                Err(_) => State::Unstarted,
            });
        if let Ok(handle) = spawned {
            self.state = State::Warming(handle);
        }
    }

    /// The loaded model and matrix, brought up to date with the index, or
    /// `None` when search should be keyword only. Loads on this thread when
    /// no warm-up ran, and waits for one that is still running.
    pub(crate) fn ready(&mut self, conn: &Connection) -> Option<&mut Ready> {
        loop {
            match std::mem::replace(&mut self.state, State::Off) {
                State::Off => return None,
                State::Failed(at) if at.elapsed() < RETRY_LOAD => {
                    self.state = State::Failed(at);
                    return None;
                }
                State::Failed(_) | State::Unstarted => self.state = load(&self.source, conn),
                State::Warming(handle) => {
                    self.state = handle
                        .join()
                        .unwrap_or_else(|_| State::Failed(Instant::now()));
                }
                State::Ready(ready) => {
                    self.state = State::Ready(ready);
                    break;
                }
            }
        }
        let State::Ready(ready) = &mut self.state else {
            return None;
        };
        // A failed read leaves the matrix as it was. The next search retries,
        // and this one is keyword only rather than an error.
        ready.index.refresh(conn).ok()?;
        Some(ready)
    }
}

impl Semantic {
    /// Search with `embedder` from now on, over the vectors it stored.
    #[cfg(any(test, feature = "test-utils"))]
    pub(crate) fn use_embedder(&mut self, embedder: SendEmbedder, conn: &Connection) -> Result<()> {
        let index = VectorIndex::load(conn, embedder.id(), embedder.dims())?;
        self.state = State::Ready(Box::new(Ready { embedder, index }));
        Ok(())
    }

    /// Tests: rows, matrix bytes, and key bytes of the loaded matrix.
    #[cfg(test)]
    pub(crate) fn sizes(&mut self, conn: &Connection) -> Option<(usize, usize, usize)> {
        let ready = self.ready(conn)?;
        Some((
            ready.index.len(),
            ready.index.matrix_bytes(),
            ready.index.key_bytes(),
        ))
    }
}

pub(crate) fn open_reader(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(1))?;
    Ok(conn)
}

fn load(source: &EmbedderSource, conn: &Connection) -> State {
    let embedder = match source.load_semantic() {
        Ok(Some(embedder)) => embedder,
        Ok(None) => return State::Off,
        Err(_) => return State::Failed(Instant::now()),
    };
    match VectorIndex::load(conn, embedder.id(), embedder.dims()) {
        Ok(index) => State::Ready(Box::new(Ready { embedder, index })),
        Err(_) => State::Failed(Instant::now()),
    }
}

impl Ready {
    /// The query's vector, or `None` when the model gave none (an error, or
    /// text with no word pieces).
    pub(crate) fn embed_query(&self, text: &str) -> Option<Vec<f32>> {
        let values = self.embedder.embed(text).ok()?;
        (values.len() == self.index.dims && values.iter().any(|value| *value != 0.0))
            .then_some(values)
    }
}

/// A read transaction of its own, or nothing when the connection is already
/// in one (a search holds one from the refresh to the last hit it reads).
/// Deferred: it never asks for the write lock.
pub(crate) struct Snapshot<'conn>(Option<rusqlite::Transaction<'conn>>);

impl<'conn> Snapshot<'conn> {
    pub(crate) fn begin(conn: &'conn Connection) -> Result<Self> {
        Ok(Self(if conn.is_autocommit() {
            Some(conn.unchecked_transaction()?)
        } else {
            None
        }))
    }

    pub(crate) fn commit(self) -> Result<()> {
        if let Some(tx) = self.0 {
            tx.commit()?;
        }
        Ok(())
    }
}

/// Every stored vector of one model, as int8 rows.
pub(crate) struct VectorIndex {
    id: EmbedderId,
    dims: usize,
    /// The last `embedding_changes` entry applied.
    seen: i64,
    /// `rows × dims` codes, row after row.
    codes: Vec<i8>,
    /// Per row: multiply a code dot product by this to get the cosine.
    scales: Vec<f32>,
    /// Per row: index into `accounts`.
    owners: Vec<u32>,
    /// Per row: the message id.
    keys: Vec<Arc<str>>,
    accounts: Vec<String>,
    /// Per account: message id to row.
    rows: Vec<HashMap<Arc<str>, u32>>,
}

impl VectorIndex {
    /// Every row `id` stored with `dims` values, read in one snapshot: the
    /// caller's transaction when it is in one, else its own. An embedder
    /// with no dimensions is an error.
    pub(crate) fn load(conn: &Connection, id: EmbedderId, dims: usize) -> Result<Self> {
        if dims == 0 {
            return Err(crate::Error::Invalid(format!(
                "embedder {} has no dimensions",
                id.model
            )));
        }
        let tx = Snapshot::begin(conn)?;
        let mut index = Self {
            id,
            dims,
            seen: 0,
            codes: Vec::new(),
            scales: Vec::new(),
            owners: Vec::new(),
            keys: Vec::new(),
            accounts: Vec::new(),
            rows: Vec::new(),
        };
        index.seen = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM embedding_changes",
            [],
            |row| row.get(0),
        )?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM message_embeddings
             WHERE model = ?1 AND model_version = ?2 AND dims = ?3",
            params![id.model, id.version, index.dims_i64()],
            |row| row.get(0),
        )?;
        let count = usize::try_from(count).unwrap_or(0);
        index.codes.reserve_exact(count * dims);
        index.scales.reserve_exact(count);
        index.owners.reserve_exact(count);
        index.keys.reserve_exact(count);
        {
            let mut stmt = conn.prepare(
                "SELECT account_id, message_id, vector FROM message_embeddings
                 WHERE model = ?1 AND model_version = ?2 AND dims = ?3
                   AND length(vector) = ?4",
            )?;
            let mut rows = stmt.query(params![
                id.model,
                id.version,
                index.dims_i64(),
                index.dims_i64() * 4
            ])?;
            while let Some(row) = rows.next()? {
                let account: String = row.get(0)?;
                let message: String = row.get(1)?;
                let blob = row.get_ref(2)?.as_blob().map_err(rusqlite::Error::from)?;
                index.upsert(&account, &message, blob);
            }
        }
        tx.commit()?;
        Ok(index)
    }

    fn dims_i64(&self) -> i64 {
        i64::try_from(self.dims).unwrap_or(i64::MAX)
    }

    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    /// Bytes held by the matrix itself: codes and scales.
    #[cfg(test)]
    pub(crate) fn matrix_bytes(&self) -> usize {
        self.codes.capacity() + self.scales.capacity() * std::mem::size_of::<f32>()
    }

    /// Bytes held by everything else: row owners, message ids, and the
    /// lookup from id to row (estimated from the table capacity).
    #[cfg(test)]
    pub(crate) fn key_bytes(&self) -> usize {
        let ids: usize = self
            .keys
            .iter()
            .map(|key| key.len() + 2 * std::mem::size_of::<usize>())
            .sum();
        let slots: usize = self
            .rows
            .iter()
            .map(|rows| rows.capacity() * (std::mem::size_of::<(Arc<str>, u32)>() + 1))
            .sum();
        self.owners.capacity() * 4
            + self.keys.capacity() * std::mem::size_of::<Arc<str>>()
            + ids
            + slots
    }

    /// Apply the log entries written since the last call. Reads one
    /// snapshot, so an entry pruned meanwhile cannot be skipped. Inside the
    /// caller's transaction that is the caller's snapshot, so a search scans
    /// exactly the vectors of the rows it then reads.
    pub(crate) fn refresh(&mut self, conn: &Connection) -> Result<()> {
        let tx = Snapshot::begin(conn)?;
        let (first, last): (Option<i64>, Option<i64>) = conn.query_row(
            "SELECT MIN(seq), MAX(seq) FROM embedding_changes",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let last = last.unwrap_or(0);
        if last == self.seen {
            return Ok(());
        }
        let pruned = first.is_some_and(|first| first > self.seen + 1);
        if last < self.seen || pruned || last - self.seen > CHANGES_KEPT {
            drop(tx);
            *self = Self::load(conn, self.id, self.dims)?;
            return Ok(());
        }
        let mut keys = conn.prepare(
            "SELECT DISTINCT account_id, message_id FROM embedding_changes
             WHERE seq > ?1 AND seq <= ?2",
        )?;
        let changed = keys
            .query_map(params![self.seen, last], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(keys);
        let mut vector = conn.prepare(
            "SELECT vector FROM message_embeddings
             WHERE account_id = ?1 AND message_id = ?2
               AND model = ?3 AND model_version = ?4 AND dims = ?5
               AND length(vector) = ?6",
        )?;
        for (account, message) in changed {
            let blob: Option<Vec<u8>> = vector
                .query_row(
                    params![
                        account,
                        message,
                        self.id.model,
                        self.id.version,
                        self.dims_i64(),
                        self.dims_i64() * 4
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            match blob {
                Some(blob) => self.upsert(&account, &message, &blob),
                None => self.remove(&account, &message),
            }
        }
        drop(vector);
        tx.commit()?;
        self.seen = last;
        Ok(())
    }

    fn account_slot(&self, account: &str) -> Option<usize> {
        self.accounts.iter().position(|known| known == account)
    }

    /// Store `blob` (`dims` little-endian `f32`s) as the row for this
    /// message. A zero or non-finite vector is no row: it is like nothing.
    fn upsert(&mut self, account: &str, message: &str, blob: &[u8]) {
        let Some((codes, scale)) = quantize_blob(blob, self.dims) else {
            self.remove(account, message);
            return;
        };
        let slot = match self.account_slot(account) {
            Some(slot) => slot,
            None => {
                self.accounts.push(account.to_string());
                self.rows.push(HashMap::new());
                self.accounts.len() - 1
            }
        };
        if let Some(&row) = self.rows[slot].get(message) {
            let start = row as usize * self.dims;
            self.codes[start..start + self.dims].copy_from_slice(&codes);
            self.scales[row as usize] = scale;
            return;
        }
        let Ok(row) = u32::try_from(self.keys.len()) else {
            return;
        };
        let key: Arc<str> = Arc::from(message);
        self.codes.extend_from_slice(&codes);
        self.scales.push(scale);
        self.owners.push(slot as u32);
        self.keys.push(Arc::clone(&key));
        self.rows[slot].insert(key, row);
    }

    /// Drop this message's row. The last row moves into its place.
    fn remove(&mut self, account: &str, message: &str) {
        let Some(slot) = self.account_slot(account) else {
            return;
        };
        let Some(row) = self.rows[slot].remove(message) else {
            return;
        };
        let row = row as usize;
        let last = self.keys.len() - 1;
        if row != last {
            let (head, tail) = self.codes.split_at_mut(last * self.dims);
            head[row * self.dims..(row + 1) * self.dims].copy_from_slice(&tail[..self.dims]);
            let owner = self.owners[last] as usize;
            if let Some(moved) = self.rows[owner].get_mut(&self.keys[last]) {
                *moved = row as u32;
            }
        }
        self.codes.truncate(last * self.dims);
        self.scales.swap_remove(row);
        self.owners.swap_remove(row);
        self.keys.swap_remove(row);
    }

    /// Mark the rows of `messages` in `account`. Rows of other accounts and
    /// ids with no row stay unmarked.
    pub(crate) fn rows_of<'a>(
        &self,
        account: &str,
        messages: impl IntoIterator<Item = &'a str>,
    ) -> Vec<bool> {
        let mut allowed = vec![false; self.len()];
        if let Some(slot) = self.account_slot(account) {
            for message in messages {
                if let Some(&row) = self.rows[slot].get(message) {
                    allowed[row as usize] = true;
                }
            }
        }
        allowed
    }

    /// Up to `limit` messages of `account` nearest `query`, most similar
    /// first, each at least [`MIN_SIMILARITY`]. With `allowed`, only the
    /// marked rows are considered.
    pub(crate) fn nearest(
        &self,
        account: &str,
        query: &[f32],
        allowed: Option<&[bool]>,
        limit: usize,
    ) -> Vec<(Arc<str>, f32)> {
        // `load` refuses zero dimensions, and `chunks_exact(0)` would panic.
        let (Some(slot), false) = (self.account_slot(account), self.dims == 0) else {
            return Vec::new();
        };
        let Some((query, query_scale)) = quantize(query.iter().copied(), self.dims) else {
            return Vec::new();
        };
        let slot = slot as u32;
        let mut best: BinaryHeap<Reverse<Scored>> = BinaryHeap::with_capacity(limit + 1);
        for (row, codes) in self.codes.chunks_exact(self.dims).enumerate() {
            if self.owners[row] != slot || allowed.is_some_and(|allowed| !allowed[row]) {
                continue;
            }
            let score = dot(&query, codes) as f32 * self.scales[row] * query_scale;
            if score < MIN_SIMILARITY {
                continue;
            }
            if best.len() < limit {
                best.push(Reverse(Scored(score, row as u32)));
            } else if best.peek().is_some_and(|Reverse(low)| score > low.0) {
                best.pop();
                best.push(Reverse(Scored(score, row as u32)));
            }
        }
        let mut found: Vec<Scored> = best.into_iter().map(|Reverse(scored)| scored).collect();
        found.sort_by(|a, b| b.cmp(a));
        found
            .into_iter()
            .map(|Scored(score, row)| (Arc::clone(&self.keys[row as usize]), score))
            .collect()
    }
}

/// A score and its row. Higher scores order later; ties go to the lower row.
#[derive(Clone, Copy, PartialEq)]
struct Scored(f32, u32);

impl Eq for Scored {}

impl PartialOrd for Scored {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Scored {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0).then(other.1.cmp(&self.1))
    }
}

fn quantize_blob(blob: &[u8], dims: usize) -> Option<(Vec<i8>, f32)> {
    let (chunks, rest) = blob.as_chunks::<4>();
    if !rest.is_empty() || chunks.len() != dims {
        return None;
    }
    quantize(chunks.iter().map(|bytes| f32::from_le_bytes(*bytes)), dims)
}

/// Symmetric int8 codes for `values`, and the scale that turns a dot product
/// of two code rows back into a cosine. `None` for a zero or non-finite vector.
fn quantize(values: impl Iterator<Item = f32> + Clone, dims: usize) -> Option<(Vec<i8>, f32)> {
    let mut max_abs = 0f32;
    let mut norm = 0f32;
    for value in values.clone() {
        if !value.is_finite() {
            return None;
        }
        max_abs = max_abs.max(value.abs());
        norm += value * value;
    }
    let norm = norm.sqrt();
    if max_abs == 0.0 || norm == 0.0 {
        return None;
    }
    let step = max_abs / 127.0;
    let mut codes = Vec::with_capacity(dims);
    codes.extend(values.map(|value| (value / step).round().clamp(-127.0, 127.0) as i8));
    Some((codes, step / norm))
}

/// Integer dot product. Plain enough for the compiler to vectorise.
fn dot(left: &[i8], right: &[i8]) -> i32 {
    left.iter()
        .zip(right)
        .map(|(a, b)| i32::from(*a) * i32::from(*b))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(values: &[f32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_le_bytes())
            .collect()
    }

    fn index(dims: usize) -> VectorIndex {
        VectorIndex {
            id: EmbedderId {
                model: "t",
                version: "1",
            },
            dims,
            seen: 0,
            codes: Vec::new(),
            scales: Vec::new(),
            owners: Vec::new(),
            keys: Vec::new(),
            accounts: Vec::new(),
            rows: Vec::new(),
        }
    }

    #[test]
    fn int8_rows_keep_the_cosine_order() {
        let mut index = index(3);
        index.upsert("a", "near", &blob(&[0.9, 0.1, 0.0]));
        index.upsert("a", "far", &blob(&[0.0, 0.2, 0.9]));
        index.upsert("a", "mid", &blob(&[0.6, 0.6, 0.0]));
        index.upsert("b", "other", &blob(&[1.0, 0.0, 0.0]));
        let found = index.nearest("a", &[1.0, 0.0, 0.0], None, 10);
        let names: Vec<&str> = found.iter().map(|(key, _)| &**key).collect();
        // `far` is below the similarity floor; `other` is another account.
        assert_eq!(names, ["near", "mid"]);
        let near = 0.9 / (0.81f32 + 0.01).sqrt();
        assert!((found[0].1 - near).abs() < 0.01, "{found:?}");
    }

    #[test]
    fn removing_a_row_moves_the_last_one_into_its_place() {
        let mut index = index(2);
        index.upsert("a", "x", &blob(&[1.0, 0.0]));
        index.upsert("a", "y", &blob(&[0.0, 1.0]));
        index.upsert("a", "z", &blob(&[0.7, 0.7]));
        index.remove("a", "x");
        assert_eq!(index.len(), 2);
        let found = index.nearest("a", &[0.7, 0.7], None, 10);
        assert_eq!(&*found[0].0, "z");
        let allowed = index.rows_of("a", ["y"]);
        let found = index.nearest("a", &[0.0, 1.0], Some(&allowed), 10);
        assert_eq!(found.len(), 1);
        assert_eq!(&*found[0].0, "y");
        // A rewrite replaces the row in place, and a zero vector drops it.
        index.upsert("a", "y", &blob(&[1.0, 0.0]));
        assert_eq!(index.len(), 2);
        assert_eq!(&*index.nearest("a", &[1.0, 0.0], None, 1)[0].0, "y");
        index.upsert("a", "y", &blob(&[0.0, 0.0]));
        assert_eq!(index.len(), 1);
    }

    #[test]
    fn the_scan_keeps_only_the_best_rows() {
        let mut index = index(2);
        for step in 0..50 {
            let angle = step as f32 / 100.0;
            index.upsert("a", &format!("m{step}"), &blob(&[angle.cos(), angle.sin()]));
        }
        let found = index.nearest("a", &[1.0, 0.0], None, 5);
        let names: Vec<&str> = found.iter().map(|(key, _)| &**key).collect();
        assert_eq!(names, ["m0", "m1", "m2", "m3", "m4"]);
    }
}
