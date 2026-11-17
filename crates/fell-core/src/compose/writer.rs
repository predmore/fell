//! Draft writes off the lock that the editor paints from.
//!
//! A live draft is shared by the compose window, the agent socket, and the
//! file watcher through one mutex. Writing `draft.md` (with fsync), the hunk
//! file, and the mailbox row under that mutex made the window wait for disk
//! on every save and on every agent edit. A [`DraftWriter`] owns one thread
//! per live draft: [`super::DraftSession::flush`] serializes the draft under
//! the lock and hands the text here, and this thread does the I/O in order.
//! When several saves queue up, only the newest is written.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};

use super::persist::{self, StoredCompose};
use super::workspace;

/// Everything one save writes. Built under the session lock, run without it.
pub(crate) struct SaveJob {
    pub folder: PathBuf,
    pub text: String,
    pub hunks_json: String,
    pub mailbox: Option<MailboxSave>,
}

/// The mailbox row for the draft. `store_path` is `None` when the session
/// was not bound to a mailbox, so the path is looked up from the draft id.
pub(crate) struct MailboxSave {
    pub store_path: Option<PathBuf>,
    pub draft: StoredCompose,
}

/// State a run keeps between saves: the attachment stamp last copied into
/// the mailbox and, on the writer thread, the open mailbox.
#[derive(Default)]
pub(crate) struct SaveCache {
    attachment_stamp: Vec<(String, u64, u128)>,
    /// Kept open on the writer thread. A synchronous save opens and drops it.
    store: Option<(PathBuf, crate::Store)>,
    keep_store: bool,
}

impl SaveJob {
    /// Write the draft file, the hunk file, then the mailbox row.
    pub(crate) fn run(self, cache: &mut SaveCache) -> Result<(), String> {
        workspace::write_draft_file(&self.folder, &self.text)?;
        let control = workspace::control_dir(&self.folder);
        let _ = std::fs::create_dir_all(&control);
        let _ = std::fs::write(control.join("hunks.json"), self.hunks_json);
        if let Some(mailbox) = self.mailbox {
            persist_mailbox(&self.folder, mailbox, cache);
        }
        Ok(())
    }
}

fn persist_mailbox(folder: &Path, mailbox: MailboxSave, cache: &mut SaveCache) {
    let path = mailbox
        .store_path
        .or_else(|| persist::remembered_store_path(&mailbox.draft.id));
    let Some(path) = path.filter(|path| path.is_file()) else {
        return;
    };
    let reuse = cache
        .store
        .as_ref()
        .is_some_and(|(open, _)| open.as_path() == path.as_path());
    if !reuse {
        cache.store = None;
        let Ok(store) = crate::Store::open(&path) else {
            return;
        };
        cache.store = Some((path, store));
    }
    let Some((_, store)) = cache.store.as_ref() else {
        return;
    };
    let draft = mailbox.draft;
    let stamp = super::session::attachment_stamp(folder, &draft.fields, &draft.body);
    if stamp == cache.attachment_stamp {
        let _ = store.save_compose_text(&draft);
    } else {
        let files = workspace::resolve_closure(folder, &draft.fields, &draft.body).files;
        if store
            .save_compose(&StoredCompose { files, ..draft })
            .is_ok()
        {
            cache.attachment_stamp = stamp;
        }
    }
    if !cache.keep_store {
        cache.store = None;
    }
}

struct Progress {
    /// Saves handed to the thread so far.
    submitted: u64,
    /// The newest save the thread has finished, written or skipped.
    completed: u64,
    /// The newest failed write, until the session reports it.
    error: Option<String>,
}

struct Shared {
    progress: Mutex<Progress>,
    done: Condvar,
}

/// The writer thread for one live draft. Dropping it lets the thread finish
/// the queued save and exit; drop does not wait.
pub(crate) struct DraftWriter {
    jobs: Sender<(u64, SaveJob)>,
    shared: Arc<Shared>,
}

impl DraftWriter {
    pub(crate) fn spawn() -> Self {
        let (jobs, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            progress: Mutex::new(Progress {
                submitted: 0,
                completed: 0,
                error: None,
            }),
            done: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        // A failed spawn drops `rx`, so saves report an error instead of hanging.
        let _ = std::thread::Builder::new()
            .name("fell-draft-writer".into())
            .spawn(move || write_loop(rx, thread_shared));
        Self { jobs, shared }
    }

    /// Queue a save. Returns its number for [`Self::completed`].
    pub(crate) fn submit(&self, job: SaveJob) -> u64 {
        let mut progress = self.shared.progress.lock().expect("draft writer");
        progress.submitted += 1;
        let number = progress.submitted;
        if self.jobs.send((number, job)).is_err() {
            progress.completed = number;
            progress.error = Some("the draft writer stopped".into());
            self.shared.done.notify_all();
        }
        number
    }

    /// The newest save the thread has finished.
    pub(crate) fn completed(&self) -> u64 {
        self.shared.progress.lock().expect("draft writer").completed
    }

    pub(crate) fn take_error(&self) -> Option<String> {
        self.shared
            .progress
            .lock()
            .expect("draft writer")
            .error
            .take()
    }

    /// A handle that can wait for the queue to drain without the session lock.
    pub(crate) fn waiter(&self) -> SaveWaiter {
        let progress = self.shared.progress.lock().expect("draft writer");
        SaveWaiter {
            shared: Arc::clone(&self.shared),
            until: progress.submitted,
        }
    }
}

/// Waits until every save queued before it was made is on disk.
pub struct SaveWaiter {
    shared: Arc<Shared>,
    until: u64,
}

impl SaveWaiter {
    pub fn wait(self) {
        let mut progress = self.shared.progress.lock().expect("draft writer");
        while progress.completed < self.until {
            progress = self.shared.done.wait(progress).expect("draft writer");
        }
    }
}

fn write_loop(rx: Receiver<(u64, SaveJob)>, shared: Arc<Shared>) {
    let mut cache = SaveCache {
        keep_store: true,
        ..SaveCache::default()
    };
    while let Ok(mut next) = rx.recv() {
        // Only the newest queued save is worth writing.
        while let Ok(newer) = rx.try_recv() {
            next = newer;
        }
        let (number, job) = next;
        let result = job.run(&mut cache);
        let mut progress = shared.progress.lock().expect("draft writer");
        progress.completed = progress.completed.max(number);
        if let Err(err) = result {
            progress.error = Some(err);
        }
        shared.done.notify_all();
    }
}

/// Texts Fell asked to write that the file may still show. The watcher
/// ignores these as Fell's own writes. Once save `n` is finished, the file
/// shows save `n` or a later one, so older texts are dropped.
#[derive(Default)]
pub(crate) struct OwnWrites {
    texts: VecDeque<(u64, String)>,
}

impl OwnWrites {
    pub(crate) fn note(&mut self, number: u64, text: String) {
        self.texts.push_back((number, text));
    }

    pub(crate) fn contains(&mut self, completed: u64, text: &str) -> bool {
        // Keep the newest finished save: the file still shows it.
        while self.texts.len() > 1 && self.texts.get(1).is_some_and(|(n, _)| *n <= completed) {
            self.texts.pop_front();
        }
        self.texts.iter().any(|(_, own)| own == text)
    }
}
