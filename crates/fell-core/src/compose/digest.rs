//! SHA-256 of attachment bytes, computed off the paint path.
//!
//! The compose window repaints often, and each repaint asks whether send is
//! blocked. That check only stats files. A digest is computed once per file
//! version, keyed by path and [`FileStamp`], on a thread of its own; send
//! hashes the bytes it is about to transmit and compares.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};

use sha2::Digest as _;

use super::workspace::{self, FileStamp};

pub(crate) type Digest = [u8; 32];

pub(crate) fn sha256(bytes: &[u8]) -> Digest {
    sha2::Sha256::digest(bytes).into()
}

/// What is known about one file version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Known {
    Digest(Digest),
    /// Two reads of the same stamp gave different bytes. The file was
    /// rewritten within one timestamp tick; no digest for it is trusted.
    Conflict,
}

type Key = (String, FileStamp);

#[derive(Default)]
struct State {
    known: HashMap<Key, Known>,
    queued: HashSet<Key>,
    /// Versions the hash thread could not read. Not retried; send reads
    /// the file itself and reports why.
    failed: HashSet<Key>,
    /// Files this cache had read on the hash thread. Tests count reads with it.
    hashed: usize,
}

struct Shared {
    state: Mutex<State>,
    idle: Condvar,
}

/// One draft's digests. Cheap to query under the session lock.
pub(crate) struct Digests {
    shared: Arc<Shared>,
}

impl Default for Digests {
    fn default() -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                idle: Condvar::new(),
            }),
        }
    }
}

impl Digests {
    pub(crate) fn get(&self, path: &str, stamp: FileStamp) -> Option<Known> {
        self.state().known.get(&(path.to_string(), stamp)).copied()
    }

    /// Record bytes read for `stamp`. A second, different digest for the
    /// same stamp marks it as a conflict.
    pub(crate) fn record(&self, path: &str, stamp: FileStamp, digest: Digest) {
        record(&mut self.state(), path, stamp, digest);
    }

    /// Hash `path` at `stamp` on the hash thread, unless it is known or queued.
    pub(crate) fn request(&self, folder: &Path, path: &str, stamp: FileStamp) {
        let key = (path.to_string(), stamp);
        {
            let mut state = self.state();
            if state.known.contains_key(&key)
                || state.failed.contains(&key)
                || !state.queued.insert(key.clone())
            {
                return;
            }
        }
        let job = Job {
            folder: folder.to_path_buf(),
            path: path.to_string(),
            stamp,
            target: Arc::downgrade(&self.shared),
        };
        let sent = hasher().is_some_and(|tx| tx.send(job).is_ok());
        if !sent {
            let mut state = self.state();
            state.queued.remove(&key);
            self.shared.idle.notify_all();
        }
    }

    /// Drop every digest for `path`. A file the person accepts again is
    /// judged by what is on disk now, not by an older read.
    pub(crate) fn forget(&self, path: &str) {
        self.state().known.retain(|(name, _), _| name != path);
    }

    /// Keep only digests for these file versions.
    pub(crate) fn retain(&self, keep: &[(String, FileStamp)]) {
        let keep: HashSet<&(String, FileStamp)> = keep.iter().collect();
        let mut state = self.state();
        state.known.retain(|key, _| keep.contains(key));
        state.failed.retain(|key| keep.contains(key));
    }

    /// Wait until no hash job for this cache is queued.
    #[cfg(test)]
    pub(crate) fn wait_idle(&self) {
        let mut state = self.state();
        while !state.queued.is_empty() {
            state = self.shared.idle.wait(state).expect("digests");
        }
    }

    #[cfg(test)]
    pub(crate) fn hashed(&self) -> usize {
        self.state().hashed
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().expect("digests")
    }
}

fn record(state: &mut State, path: &str, stamp: FileStamp, digest: Digest) {
    let key = (path.to_string(), stamp);
    match state.known.get(&key) {
        Some(Known::Digest(seen)) if *seen != digest => {
            state.known.insert(key, Known::Conflict);
        }
        Some(_) => {}
        None => {
            state.known.insert(key, Known::Digest(digest));
        }
    }
}

struct Job {
    folder: PathBuf,
    path: String,
    stamp: FileStamp,
    target: Weak<Shared>,
}

/// One hash thread for the process. Drafts are few and files are small;
/// what matters is that the window's thread never reads them.
fn hasher() -> Option<&'static Sender<Job>> {
    static HASHER: OnceLock<Option<Sender<Job>>> = OnceLock::new();
    HASHER
        .get_or_init(|| {
            let (tx, rx) = mpsc::channel::<Job>();
            std::thread::Builder::new()
                .name("fell-attachment-hash".into())
                .spawn(move || {
                    while let Ok(job) = rx.recv() {
                        run(job);
                    }
                })
                .ok()
                .map(|_| tx)
        })
        .as_ref()
}

fn run(job: Job) {
    let Some(shared) = job.target.upgrade() else {
        return;
    };
    let key = (job.path.clone(), job.stamp);
    let known = shared
        .state
        .lock()
        .expect("digests")
        .known
        .contains_key(&key);
    let result = if known {
        None
    } else {
        Some(workspace::hash_attachment(
            &job.folder,
            &job.path,
            workspace::MAX_ATTACHMENT_BYTES,
        ))
    };
    let mut state = shared.state.lock().expect("digests");
    if let Some(result) = result {
        state.hashed += 1;
        // The file may have changed since it was queued. The digest is
        // recorded for the version that was actually read.
        match result {
            Ok((digest, stamp)) => record(&mut state, &job.path, stamp, digest),
            Err(_) => {
                state.failed.insert(key.clone());
            }
        }
    }
    state.queued.remove(&key);
    shared.idle.notify_all();
}
