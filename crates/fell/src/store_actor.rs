//! The mailbox connections live on their own threads. The window never
//! touches SQLite: it sends a closure here and awaits the answer, so a slow
//! query, a checkpoint, or a sync write can delay a result but never a frame.
//!
//! There are two lanes, each with its own connection. The interactive lane
//! runs what the reader is waiting for: opening a conversation, a folder
//! switch, archive, move, undo. The background lane runs folder refreshes
//! that sync asks for. SQLite's WAL lets the two read at once, so holding
//! `j` during a first sync never waits behind a refresh.

use std::sync::{Arc, OnceLock, mpsc};

use fell_core::Store;
use futures::channel::oneshot;

type Job = Box<dyn FnOnce(&Store) + Send>;

#[derive(Clone)]
pub struct StoreClient {
    interactive: mpsc::Sender<Job>,
    /// Set once the second connection is open. Until then, and for an
    /// in-memory mailbox, background jobs run on the interactive lane.
    background: Arc<OnceLock<mpsc::Sender<Job>>>,
}

impl StoreClient {
    /// Open the store on a new thread with `open`, which also returns
    /// whatever the window needs to know about the mailbox it opened. A
    /// file-backed mailbox then gets its background connection.
    pub fn spawn<T: Send + 'static>(
        open: impl FnOnce() -> (Store, T) + Send + 'static,
    ) -> (Self, oneshot::Receiver<T>) {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready) = oneshot::channel();
        let background = Arc::new(OnceLock::new());
        let lane = Arc::clone(&background);
        std::thread::Builder::new()
            .name("fell-store".into())
            .spawn(move || {
                let (store, info) = open();
                // The first connection has finished opening and migrating
                // the file, so the second one never races it for the lock.
                if let Some(path) = store.db_path().map(std::path::Path::to_path_buf) {
                    spawn_background(path, lane);
                }
                let _ = ready_tx.send(info);
                // Jobs run in order. The loop ends when the window drops its client.
                while let Ok(job) = rx.recv() {
                    job(&store);
                }
            })
            .expect("spawn store thread");
        (
            Self {
                interactive: jobs,
                background,
            },
            ready,
        )
    }

    /// Run `job` on the interactive lane. The receiver is canceled if the thread is gone.
    pub fn call<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Send + 'static,
    ) -> oneshot::Receiver<R> {
        send(&self.interactive, job)
    }

    /// Run `job` on the background lane: work nobody is waiting on, such as
    /// rereading a folder because sync wrote to it.
    pub fn call_background<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Send + 'static,
    ) -> oneshot::Receiver<R> {
        match self.background.get() {
            Some(lane) => send(lane, job),
            None => send(&self.interactive, job),
        }
    }

    #[cfg(test)]
    fn has_background_lane(&self) -> bool {
        self.background.get().is_some()
    }
}

fn send<R: Send + 'static>(
    lane: &mpsc::Sender<Job>,
    job: impl FnOnce(&Store) -> R + Send + 'static,
) -> oneshot::Receiver<R> {
    let (tx, rx) = oneshot::channel();
    let _ = lane.send(Box::new(move |store| {
        let _ = tx.send(job(store));
    }));
    rx
}

/// Open the second connection on its own thread. If it cannot open, the
/// lane stays unset and background jobs keep using the interactive lane.
fn spawn_background(path: std::path::PathBuf, lane: Arc<OnceLock<mpsc::Sender<Job>>>) {
    let _ = std::thread::Builder::new()
        .name("fell-store-background".into())
        .spawn(move || {
            let Ok(store) = Store::open(&path) else {
                return;
            };
            let (jobs, rx) = mpsc::channel::<Job>();
            if lane.set(jobs).is_err() {
                return;
            }
            // Jobs reach this lane only after it is set, and the loop ends
            // when the window drops its clients.
            while let Ok(job) = rx.recv() {
                job(&store);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn jobs_run_off_the_calling_thread_in_order() {
        let (client, ready) = StoreClient::spawn(|| {
            (
                Store::open_in_memory().unwrap(),
                std::thread::current().id(),
            )
        });
        let store_thread = futures::executor::block_on(ready).unwrap();
        assert_ne!(store_thread, std::thread::current().id());
        let first = client.call(|_| 1);
        let second = client.call(|_| 2);
        assert_eq!(futures::executor::block_on(second).unwrap(), 2);
        assert_eq!(futures::executor::block_on(first).unwrap(), 1);
    }

    #[test]
    fn an_in_memory_mailbox_runs_background_jobs_on_its_one_lane() {
        let (client, ready) = StoreClient::spawn(|| (Store::open_in_memory().unwrap(), ()));
        futures::executor::block_on(ready).unwrap();
        let answer = client.call_background(|store| store.db_path().is_none());
        assert!(futures::executor::block_on(answer).unwrap());
        assert!(!client.has_background_lane());
    }

    #[test]
    fn a_slow_background_read_does_not_hold_up_the_reader() {
        let dir = std::env::temp_dir().join(format!(
            "fell-lanes-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mail.db");
        let opened = path.clone();
        let (client, ready) = StoreClient::spawn(move || (Store::open(&opened).unwrap(), ()));
        futures::executor::block_on(ready).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !client.has_background_lane() {
            assert!(Instant::now() < deadline, "background lane never opened");
            std::thread::sleep(Duration::from_millis(5));
        }
        let started = Instant::now();
        let slow = client.call_background(|_| std::thread::sleep(Duration::from_millis(400)));
        let quick = client.call(|store| store.list_accounts().map(|accounts| accounts.len()));
        assert_eq!(futures::executor::block_on(quick).unwrap().unwrap(), 0);
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "the reader waited {:?} behind a background job",
            started.elapsed()
        );
        futures::executor::block_on(slow).unwrap();
        drop(client);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
