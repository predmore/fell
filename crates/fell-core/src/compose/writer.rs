//! Draft writes off the lock that the editor paints from.
//!
//! A live draft is shared by the compose window, the agent socket, and the
//! file watcher through one mutex. Writing `draft.md` (with fsync) and the
//! mailbox row under that mutex made the window wait for disk
//! on every save and on every agent edit. A [`DraftWriter`] owns one thread
//! per live draft: [`super::DraftSession::flush`] serializes the draft under
//! the lock and hands the text here, and this thread does the I/O in order.
//! When several saves queue up, only the newest is written.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use super::persist::{self, StoredCompose, StoredReview};
use super::workspace::{self, FileStamp};

/// Everything one save writes. Built under the session lock, run without it.
pub(crate) struct SaveJob {
    pub folder: PathBuf,
    pub text: String,
    pub mailbox: Option<MailboxSave>,
}

/// The mailbox row for the draft. `store_path` is `None` when the session
/// was not bound to a mailbox, so the path is looked up from the draft id.
pub(crate) struct MailboxSave {
    pub store_path: Option<PathBuf>,
    pub draft: StoredCompose,
    /// Hunks and the text they point into, saved in the same transaction.
    pub review: StoredReview,
}

/// The attachment stamp last copied into the mailbox, kept between saves.
#[derive(Default)]
pub(crate) struct SaveCache {
    attachment_stamp: Vec<(String, FileStamp)>,
}

impl SaveJob {
    /// Write the draft file, then the mailbox row. `Err` means `draft.md`
    /// was not replaced.
    pub(crate) fn run(self, cache: &mut SaveCache) -> Result<(), String> {
        workspace::write_draft_file(&self.folder, &self.text)?;
        // Older builds kept hunks here, where agents can write. Nothing
        // reads it now; the mailbox holds them.
        let _ = std::fs::remove_file(workspace::control_dir(&self.folder).join("hunks.json"));
        if let Some(mailbox) = self.mailbox {
            persist_mailbox(&self.folder, mailbox, cache);
        }
        Ok(())
    }
}

/// Open the mailbox for this one save. Holding a connection open would keep
/// sync's WAL checkpoint from resetting the log and would pin a replaced
/// mailbox file.
fn persist_mailbox(folder: &Path, mailbox: MailboxSave, cache: &mut SaveCache) {
    let path = mailbox
        .store_path
        .or_else(|| persist::remembered_store_path(&mailbox.draft.id));
    let Some(path) = path.filter(|path| path.is_file()) else {
        return;
    };
    let Ok(store) = crate::Store::open(&path) else {
        return;
    };
    let draft = mailbox.draft;
    let review = Some(&mailbox.review);
    let stamp = super::session::attachment_stamp(folder, &draft.fields, &draft.body);
    if stamp == cache.attachment_stamp {
        let _ = store.save_compose_parts(&draft, None, review);
    } else {
        let files = workspace::resolve_closure(folder, &draft.fields, &draft.body).files;
        if store
            .save_compose_parts(&draft, Some(&files), review)
            .is_ok()
        {
            cache.attachment_stamp = stamp;
        }
    }
}

struct Progress {
    /// Saves handed to the thread so far.
    submitted: u64,
    /// The newest save the thread has finished with, written or not.
    completed: u64,
    /// The newest save whose `draft.md` rename succeeded. The file shows
    /// that save or a newer one.
    written: u64,
    /// The newest failed write, until the session reports it.
    error: Option<String>,
}

struct Shared {
    progress: Mutex<Progress>,
    done: Condvar,
}

/// The writer thread for one live draft. Dropping it waits for the queued
/// save, so a closed draft or a second copy of it never writes late.
pub(crate) struct DraftWriter {
    jobs: Option<Sender<(u64, SaveJob)>>,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl DraftWriter {
    pub(crate) fn spawn() -> Self {
        let (jobs, rx) = mpsc::channel();
        let shared = Arc::new(Shared {
            progress: Mutex::new(Progress {
                submitted: 0,
                completed: 0,
                written: 0,
                error: None,
            }),
            done: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        // A failed spawn drops `rx`, so saves report an error instead of hanging.
        let thread = std::thread::Builder::new()
            .name("fell-draft-writer".into())
            .spawn(move || write_loop(rx, thread_shared))
            .ok();
        Self {
            jobs: Some(jobs),
            shared,
            thread,
        }
    }

    /// Queue a save. Returns its number.
    pub(crate) fn submit(&self, job: SaveJob) -> u64 {
        let mut progress = self.shared.progress.lock().expect("draft writer");
        progress.submitted += 1;
        let number = progress.submitted;
        let sent = self
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.send((number, job)).is_ok());
        if !sent {
            progress.completed = number;
            progress.error = Some("the draft writer stopped".into());
            self.shared.done.notify_all();
        }
        number
    }

    /// The newest save whose `draft.md` is on disk.
    pub(crate) fn written(&self) -> u64 {
        self.shared.progress.lock().expect("draft writer").written
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

impl Drop for DraftWriter {
    fn drop(&mut self) {
        // Closing the channel ends the loop after the queued save.
        self.jobs.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Waits until every save queued before it was made is finished.
pub struct SaveWaiter {
    shared: Arc<Shared>,
    until: u64,
}

impl SaveWaiter {
    /// `Err` when the newest of those saves did not reach `draft.md`.
    pub fn wait(self) -> Result<(), String> {
        let mut progress = self.shared.progress.lock().expect("draft writer");
        while progress.completed < self.until {
            progress = self.shared.done.wait(progress).expect("draft writer");
        }
        if progress.written >= self.until {
            return Ok(());
        }
        Err(progress
            .error
            .clone()
            .unwrap_or_else(|| "the draft was not saved".into()))
    }
}

fn write_loop(rx: Receiver<(u64, SaveJob)>, shared: Arc<Shared>) {
    let mut cache = SaveCache::default();
    while let Ok(mut next) = rx.recv() {
        // Only the newest queued save is worth writing.
        while let Ok(newer) = rx.try_recv() {
            next = newer;
        }
        let (number, job) = next;
        let result = job.run(&mut cache);
        let mut progress = shared.progress.lock().expect("draft writer");
        progress.completed = progress.completed.max(number);
        match result {
            Ok(()) => progress.written = progress.written.max(number),
            Err(err) => progress.error = Some(err),
        }
        shared.done.notify_all();
    }
}

/// Texts Fell queued for `draft.md`, numbered. The watcher ignores a file
/// that shows one of them as Fell's own write.
///
/// A read of `draft.md` that began when save `since` was the newest one on
/// disk shows save `since` or a newer one, never an older one. Failed saves
/// leave the watermark where it was, so the text still on disk stays known.
#[derive(Default)]
pub(crate) struct OwnWrites {
    texts: VecDeque<(u64, String)>,
}

/// Saves remembered. Far more than can be in flight between two reads.
const OWN_WRITES_KEPT: usize = 64;

impl OwnWrites {
    pub(crate) fn note(&mut self, number: u64, text: String) {
        self.texts.push_back((number, text));
        while self.texts.len() > OWN_WRITES_KEPT {
            self.texts.pop_front();
        }
    }

    pub(crate) fn contains(&self, since: u64, text: &str) -> bool {
        self.texts
            .iter()
            .any(|(number, own)| *number >= since && own == text)
    }
}
