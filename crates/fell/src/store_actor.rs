//! The mailbox connections live on their own threads. The window never
//! touches SQLite: it sends a closure here and awaits the answer, so a slow
//! query, a checkpoint, or a sync write can delay a result but never a frame.
//!
//! There are two lanes, each with its own connection. The interactive lane
//! runs what the reader is waiting for: opening a conversation, a folder
//! switch, archive, move, undo. The background lane runs folder refreshes
//! that sync asks for and full live queries. SQLite's WAL lets the two read
//! at once, so holding `j` during a first sync never waits behind a refresh.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};

use fell_core::Store;
use futures::channel::oneshot;

type Job = Box<dyn FnOnce(&Store) + Send>;

#[derive(Clone)]
pub struct StoreClient {
    interactive: mpsc::Sender<Job>,
    /// Set once the second connection is open. Until then, and for an
    /// in-memory mailbox, background jobs run on the interactive lane.
    background_ready: Arc<AtomicBool>,
    file_backed: Arc<AtomicBool>,
    background: mpsc::Sender<Job>,
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
        let (background, background_jobs) = mpsc::channel();
        let background_ready = Arc::new(AtomicBool::new(false));
        let lane = Arc::clone(&background_ready);
        let file_backed = Arc::new(AtomicBool::new(false));
        let has_file = Arc::clone(&file_backed);
        std::thread::Builder::new()
            .name("fell-store".into())
            .spawn(move || {
                let (store, info) = open();
                // The first connection has finished opening and migrating
                // the file, so the second one never races it for the lock.
                if let Some(path) = store.db_path().map(std::path::Path::to_path_buf) {
                    has_file.store(true, Ordering::Release);
                    spawn_background(path, lane, background_jobs);
                } else {
                    drop(background_jobs);
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
                background_ready,
                file_backed,
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
        send(&self.interactive, job, None)
    }

    /// Run `job` on the background lane: work nobody is waiting on, such as
    /// rereading a folder because sync wrote to it.
    pub fn call_background<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Send + 'static,
    ) -> oneshot::Receiver<R> {
        if self.background_ready.load(Ordering::Acquire) {
            send(&self.background, job, None)
        } else {
            send(&self.interactive, job, None)
        }
    }

    /// Keep full queries on the background lane, including during startup.
    /// An in-memory mailbox has only the interactive connection.
    pub fn call_search<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Send + 'static,
    ) -> oneshot::Receiver<R> {
        if self.file_backed.load(Ordering::Acquire) {
            send(&self.background, job, Some(&self.interactive))
        } else {
            self.call(job)
        }
    }

    /// Queue maintenance on the background connection even while it opens.
    /// If opening fails, cancel the reply instead of blocking the reader.
    pub fn call_maintenance<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Send + 'static,
    ) -> oneshot::Receiver<R> {
        send(&self.background, job, None)
    }

    #[cfg(test)]
    fn has_background_lane(&self) -> bool {
        self.background_ready.load(Ordering::Acquire)
    }
}

fn send<R: Send + 'static>(
    lane: &mpsc::Sender<Job>,
    job: impl FnOnce(&Store) -> R + Send + 'static,
    fallback: Option<&mpsc::Sender<Job>>,
) -> oneshot::Receiver<R> {
    let (tx, rx) = oneshot::channel();
    if let Err(error) = lane.send(Box::new(move |store| {
        let _ = tx.send(job(store));
    })) && let Some(fallback) = fallback
    {
        let _ = fallback.send(error.0);
    }
    rx
}

/// Waits between attempts to open the second connection. Until one
/// succeeds, background jobs run on the interactive lane.
const BACKGROUND_RETRIES: [std::time::Duration; 4] = [
    std::time::Duration::from_millis(250),
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(5),
    std::time::Duration::from_secs(30),
];

/// Open the second connection on its own thread, retrying a few times. If
/// it never opens, background jobs keep using the interactive lane.
fn spawn_background(path: std::path::PathBuf, ready: Arc<AtomicBool>, jobs: mpsc::Receiver<Job>) {
    let _ = std::thread::Builder::new()
        .name("fell-store-background".into())
        .spawn(move || {
            let mut waits = BACKGROUND_RETRIES.iter();
            let store = loop {
                match Store::open(&path) {
                    Ok(store) => break store,
                    Err(_) => match waits.next() {
                        // Nobody is left to serve once the window is gone.
                        Some(_) if Arc::strong_count(&ready) == 1 => return,
                        Some(wait) => std::thread::sleep(*wait),
                        None => return,
                    },
                }
            };
            store.warm_search();
            ready.store(true, Ordering::Release);
            drop(ready);
            // Only clients hold the sender. The lane closes with them.
            while let Ok(job) = jobs.recv() {
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
    fn a_mailbox_file_starts_loading_search_when_it_opens() {
        let dir = std::env::temp_dir().join(format!(
            "fell-warm-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let opened = dir.join("mail.db");
        let (client, ready) = StoreClient::spawn(move || (Store::open(&opened).unwrap(), ()));
        futures::executor::block_on(ready).unwrap();
        let asked = client.call_search(|store| store.testing_search_warm_asked());
        assert!(futures::executor::block_on(asked).unwrap());
        drop(client);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_in_memory_mailbox_runs_background_jobs_on_its_one_lane() {
        let (client, ready) = StoreClient::spawn(|| (Store::open_in_memory().unwrap(), ()));
        futures::executor::block_on(ready).unwrap();
        let answer = client.call_background(|store| store.db_path().is_none());
        assert!(futures::executor::block_on(answer).unwrap());
        let search = client.call_search(|store| store.db_path().is_none());
        assert!(futures::executor::block_on(search).unwrap());
        assert!(!client.has_background_lane());
    }

    #[test]
    fn a_slow_search_does_not_hold_up_the_reader() {
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
        // Hold the background lane busy until the reader has its answer.
        let (entered_tx, entered) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let slow = client.call_search(move |_| {
            entered_tx.send(()).unwrap();
            let _ = gate.recv();
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let (answer_tx, answer) = std::sync::mpsc::channel();
        let quick = client.call(|store| store.list_accounts().map(|accounts| accounts.len()));
        std::thread::spawn(move || {
            let _ = answer_tx.send(futures::executor::block_on(quick));
        });
        let got = answer.recv_timeout(Duration::from_secs(5));
        release.send(()).unwrap();
        futures::executor::block_on(slow).unwrap();
        assert_eq!(
            got.expect("the reader waited behind a background job")
                .unwrap()
                .unwrap(),
            0
        );
        drop(client);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_background_lane_ends_with_its_clients() {
        let dir = std::env::temp_dir().join(format!(
            "fell-lanes-end-{}-{}",
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
        let lane = Arc::clone(&client.background_ready);
        drop(client);
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&lane) > 1 {
            assert!(
                Instant::now() < deadline,
                "the background thread kept the lane"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Only this handle is left, so dropping it closes the job channel
        // and the background thread's recv loop ends.
        assert_eq!(Arc::strong_count(&lane), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
