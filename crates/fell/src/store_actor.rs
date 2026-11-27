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
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use fell_core::{Store, diagnostic};
use futures::channel::{mpsc::UnboundedSender, oneshot};

use crate::worker::{Restart, Worker, WorkerEvent};

type Job = Box<dyn FnOnce(&Store) + Send>;

#[derive(Clone)]
pub struct StoreClient {
    interactive: mpsc::Sender<Job>,
    /// Set once the second connection is open. Until then, and for an
    /// in-memory mailbox, background jobs run on the interactive lane.
    background_ready: Arc<AtomicBool>,
    file_backed: Arc<AtomicBool>,
    background: mpsc::Sender<Job>,
    stopping: Arc<AtomicBool>,
    thread: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl StoreClient {
    /// Open the store on a new thread with `open`, which also returns
    /// whatever the window needs to know about the mailbox it opened. A
    /// file-backed mailbox then gets its background connection.
    pub fn spawn<T: Send + 'static>(
        open: impl Fn() -> (Store, T) + Send + 'static,
        notices: UnboundedSender<WorkerEvent>,
    ) -> (Self, oneshot::Receiver<T>) {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready) = oneshot::channel();
        let (background, background_jobs) = mpsc::channel();
        let background_ready = Arc::new(AtomicBool::new(false));
        let lane = Arc::clone(&background_ready);
        let file_backed = Arc::new(AtomicBool::new(false));
        let has_file = Arc::clone(&file_backed);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let thread = std::thread::Builder::new()
            .name("fell-store".into())
            .spawn(move || {
                let mut restart = Restart::new(Worker::Store, |worker| {
                    notices.unbounded_send(worker).is_ok()
                });
                let Some((store, info)) = restart.run(|recovered| {
                    let opened = open();
                    recovered();
                    opened
                }) else {
                    return;
                };
                diagnostic!(INFO, "Mailbox worker opened the store");
                // The first connection has finished opening and migrating
                // the file, so the second one never races it for the lock.
                let background_thread =
                    if let Some(path) = store.db_path().map(std::path::Path::to_path_buf) {
                        has_file.store(true, Ordering::Release);
                        spawn_background(path, lane, background_jobs, notices.clone(), stop.clone())
                    } else {
                        drop(background_jobs);
                        None
                    };
                let _ = ready_tx.send(info);
                // Do not replay an interrupted job. Unwinding rolls back its
                // SQLite transaction and drops its sender (an Err to the caller).
                // The channel still owns all jobs we have not started.
                let mut seen_snoozes = store.snooze_revision().ok();
                restart.run(|recovered| {
                    run_interactive(&store, &rx, &notices, &mut seen_snoozes, recovered, &stop);
                });
                drop(store);
                if let Some(background) = background_thread {
                    // On ordinary window close, the background senders drop
                    // with the view. Explicit retirement wakes both lanes.
                    let _ = background.join();
                }
            })
            .expect("spawn store thread");
        (
            Self {
                interactive: jobs,
                background_ready,
                file_backed,
                background,
                stopping,
                thread: Arc::new(Mutex::new(Some(thread))),
            },
            ready,
        )
    }

    pub fn stop(&self) -> Option<JoinHandle<()>> {
        self.stopping.store(true, Ordering::Release);
        let _ = self.interactive.send(Box::new(|_| {}));
        let _ = self.background.send(Box::new(|_| {}));
        self.thread.lock().expect("store thread").take()
    }

    /// Run `job` on the interactive lane. An interrupted job or a closed
    /// worker returns `Err(Canceled)` on the receiver.
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

    /// Run full queries on the background lane once it is ready. Use the
    /// open connection while the second one is starting or unavailable.
    pub fn call_search<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Send + 'static,
    ) -> oneshot::Receiver<R> {
        if self.background_ready.load(Ordering::Acquire) {
            send(&self.background, job, Some(&self.interactive))
        } else {
            self.call(job)
        }
    }

    /// Queue completion while the second connection opens. If it cannot
    /// serve the job, retry the bounded lookup on the interactive lane.
    pub async fn call_search_completion<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Clone + Send + 'static,
    ) -> Result<R, oneshot::Canceled> {
        if self.file_backed.load(Ordering::Acquire)
            && let Ok(values) = send(&self.background, job.clone(), None).await
        {
            return Ok(values);
        }
        self.call(job).await
    }

    /// Queue maintenance on the background connection even while it opens.
    /// If opening fails, cancel the reply instead of blocking the reader.
    pub fn call_maintenance<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Send + 'static,
    ) -> oneshot::Receiver<R> {
        send(&self.background, job, None)
    }

    /// Large body reads can queue while the file-backed lane opens.
    pub fn call_body_read<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Send + 'static,
    ) -> oneshot::Receiver<R> {
        if self.file_backed.load(Ordering::Acquire) {
            self.call_maintenance(job)
        } else {
            self.call(job)
        }
    }

    #[cfg(test)]
    fn has_background_lane(&self) -> bool {
        self.background_ready.load(Ordering::Acquire)
    }
}

/// Recheck wall time on the store thread, including while offline or idle.
fn run_interactive(
    store: &Store,
    jobs: &mpsc::Receiver<Job>,
    notices: &UnboundedSender<WorkerEvent>,
    seen_snoozes: &mut Option<i64>,
    recovered: &mut dyn FnMut(),
    stop: &AtomicBool,
) {
    let mut next_snooze = Instant::now();
    let mut failed = false;
    while !stop.load(Ordering::Acquire) {
        let now = Instant::now();
        if now >= next_snooze {
            next_snooze = now + Duration::from_millis(500);
            match store
                .wake_snoozes(crate::present::now_unix())
                .and_then(|()| store.snooze_revision())
            {
                Ok(revision) => {
                    failed = false;
                    if seen_snoozes.replace(revision) != Some(revision) {
                        let _ = notices.unbounded_send(WorkerEvent::SnoozesChanged);
                    }
                }
                Err(err) if !failed => {
                    failed = true;
                    diagnostic!(
                        WARN,
                        "Could not wake snoozed mail",
                        error = &err as &dyn std::error::Error
                    );
                }
                Err(_) => {}
            }
        }
        match jobs.recv_timeout(next_snooze.saturating_duration_since(Instant::now())) {
            Ok(job) => {
                job(store);
                recovered();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        }
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
const BACKGROUND_RETRIES: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_secs(1),
    Duration::from_secs(5),
    Duration::from_secs(30),
];

const PART_CACHE_SWEEP_INTERVAL: Duration = Duration::from_secs(1);
const CORRESPONDENT_RECHECK: Duration = Duration::from_secs(1);
const CORRESPONDENT_BATCH_GAP: Duration = Duration::from_millis(2);

/// Open the second connection on its own thread, retrying a few times. If
/// it never opens, background jobs keep using the interactive lane.
fn spawn_background(
    path: std::path::PathBuf,
    ready: Arc<AtomicBool>,
    jobs: mpsc::Receiver<Job>,
    notices: UnboundedSender<WorkerEvent>,
    stop: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("fell-store-background".into())
        .spawn(move || {
            let mut restart = Restart::new(Worker::BackgroundStore, |worker| {
                notices.unbounded_send(worker).is_ok()
            });
            let Some(Some(store)) = restart.run(|recovered| {
                let mut waits = BACKGROUND_RETRIES.iter();
                loop {
                    if stop.load(Ordering::Acquire) {
                        break None;
                    }
                    match Store::open_existing(&path) {
                        Ok(store) => {
                            store.warm_search();
                            recovered();
                            break Some(store);
                        }
                        Err(err) => {
                            diagnostic!(
                                DEBUG,
                                "Mailbox background connection failed",
                                error = &err as &dyn std::error::Error
                            );
                            match waits.next() {
                                // Nobody is left to serve once the window is gone.
                                Some(_) if Arc::strong_count(&ready) == 1 => break None,
                                Some(wait) => std::thread::sleep(*wait),
                                None => break None,
                            }
                        }
                    }
                }
            }) else {
                return;
            };
            diagnostic!(DEBUG, "Mailbox background connection opened");
            ready.store(true, Ordering::Release);
            drop(ready);
            restart.run(|recovered| run_background(&store, &jobs, recovered, &stop));
        })
        .ok()
}

fn run_background(
    store: &Store,
    jobs: &mpsc::Receiver<Job>,
    recovered: &mut dyn FnMut(),
    stop: &AtomicBool,
) {
    // Sweep after startup and between jobs, even while working offline or
    // when nobody has opened an attachment in this window.
    let mut next_sweep = Instant::now();
    let mut next_correspondents = next_sweep;
    let mut failed = false;
    while !stop.load(Ordering::Acquire) {
        if Instant::now() >= next_sweep {
            match store.clean_default_part_cache() {
                Ok(()) => failed = false,
                Err(err) => {
                    if !failed {
                        diagnostic!(
                            WARN,
                            "Could not remove cached parts",
                            error = &err as &dyn std::error::Error
                        );
                    }
                    failed = true;
                }
            }
            next_sweep = Instant::now() + PART_CACHE_SWEEP_INTERVAL;
        }
        let due = next_sweep.min(next_correspondents);
        match jobs.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(job) => {
                job(store);
                recovered();
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= next_correspondents {
                    // A queued read gets a turn between batches, and a
                    // waiting writer gets time to take SQLite's lock.
                    let more = match store.maintain_correspondents() {
                        Ok(more) => more,
                        Err(err) => {
                            diagnostic!(
                                DEBUG,
                                "Could not refresh correspondents",
                                error = &err as &dyn std::error::Error
                            );
                            false
                        }
                    };
                    let wait = if more {
                        CORRESPONDENT_BATCH_GAP
                    } else {
                        CORRESPONDENT_RECHECK
                    };
                    next_correspondents = Instant::now() + wait;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = store.clean_default_part_cache();
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt as _;

    fn spawn<T: Send + 'static>(
        open: impl Fn() -> (Store, T) + Send + 'static,
    ) -> (StoreClient, oneshot::Receiver<T>) {
        StoreClient::spawn(open, futures::channel::mpsc::unbounded().0)
    }

    #[test]
    fn a_panicked_job_rolls_back_and_answers_with_an_error_on_both_lanes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mail.db");
        let opened = path.clone();
        let (notices, mut restarts) = futures::channel::mpsc::unbounded();
        let (client, ready) =
            StoreClient::spawn(move || (Store::open(&opened).unwrap(), ()), notices);
        futures::executor::block_on(ready).unwrap();
        futures::executor::block_on(client.call_maintenance(|_| ())).unwrap();
        futures::executor::block_on(async {
            let job = |store: &Store| {
                store.testing_with_uncommitted_write(|| panic!("interrupted write"))
            };
            let complete = |store: &Store| store.testing_with_uncommitted_write(|| ());
            // A concurrent SQLite connection sees neither interrupted write.
            assert!(client.call(job).await.is_err());
            assert_eq!(
                restarts.next().await,
                Some(WorkerEvent::Restarted(Worker::Store))
            );
            assert!(client.call_maintenance(job).await.is_err());
            assert_eq!(
                restarts.next().await,
                Some(WorkerEvent::Restarted(Worker::BackgroundStore))
            );
            client.call(complete).await.unwrap().unwrap();
            let connection = rusqlite::Connection::open(path).unwrap();
            let value: String = connection
                .query_row(
                    "SELECT value FROM meta WHERE key = 'uncommitted-test'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(value, "1");
        });
    }

    #[test]
    fn jobs_run_off_the_calling_thread_in_order() {
        let (client, ready) = spawn(|| {
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
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let opened = dir.join("mail.db");
        let (client, ready) = spawn(move || (Store::open(&opened).unwrap(), ()));
        futures::executor::block_on(ready).unwrap();
        let asked = client.call_maintenance(|store| store.testing_search_warm_asked());
        assert!(futures::executor::block_on(asked).unwrap());
        drop(client);
    }

    #[test]
    fn an_in_memory_mailbox_runs_background_jobs_on_its_one_lane() {
        let (client, ready) = spawn(|| (Store::open_in_memory().unwrap(), ()));
        futures::executor::block_on(ready).unwrap();
        let answer = client.call_background(|store| store.db_path().is_none());
        assert!(futures::executor::block_on(answer).unwrap());
        let search = client.call_search(|store| store.db_path().is_none());
        assert!(futures::executor::block_on(search).unwrap());
        assert!(!client.has_background_lane());
    }

    #[test]
    fn search_uses_the_open_connection_when_the_background_lane_is_unavailable() {
        let (interactive, reader_jobs) = mpsc::channel();
        let (background, background_jobs) = mpsc::channel();
        let client = StoreClient {
            interactive,
            background_ready: Arc::new(AtomicBool::new(false)),
            file_backed: Arc::new(AtomicBool::new(true)),
            background,
            stopping: Arc::new(AtomicBool::new(false)),
            thread: Arc::new(Mutex::new(None)),
        };
        let store = Store::open_in_memory().unwrap();

        // The second connection is still opening and cannot take jobs yet.
        let search = client.call_search(|_| 1);
        assert!(matches!(
            background_jobs.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        reader_jobs.try_recv().unwrap()(&store);
        assert_eq!(futures::executor::block_on(search).unwrap(), 1);

        // A disconnected lane must also fall back, even if readiness was set.
        client.background_ready.store(true, Ordering::Release);
        drop(background_jobs);
        let search = client.call_search(|_| 2);
        reader_jobs.try_recv().unwrap()(&store);
        assert_eq!(futures::executor::block_on(search).unwrap(), 2);
    }

    #[test]
    fn search_completion_works_with_one_connection() {
        let (client, ready) = spawn(|| {
            let store = Store::open_in_memory().unwrap();
            let account = crate::fixtures::fixture_account();
            crate::fixtures::seed_if_empty(&store, &account).unwrap();
            (store, account.id)
        });
        let account = futures::executor::block_on(ready).unwrap();
        let reply = client.call_search_completion(move |store| {
            store.search_suggestions(&account, fell_core::SearchField::Folder, "in", 4)
        });
        assert_eq!(
            futures::executor::block_on(reply).unwrap().unwrap(),
            ["INBOX"]
        );
    }

    #[test]
    fn search_completion_waits_for_the_background_lane_and_recovers_if_it_closes() {
        let (interactive, reader_jobs) = mpsc::channel();
        let (background, background_jobs) = mpsc::channel();
        let client = StoreClient {
            interactive,
            background_ready: Arc::new(AtomicBool::new(false)),
            file_backed: Arc::new(AtomicBool::new(true)),
            background,
            stopping: Arc::new(AtomicBool::new(false)),
            thread: Arc::new(Mutex::new(None)),
        };
        let store = Store::open_in_memory().unwrap();

        futures::executor::block_on(async {
            let mut completion = std::pin::pin!(client.call_search_completion(|_| 1));
            assert!(futures::poll!(completion.as_mut()).is_pending());
            assert!(matches!(
                reader_jobs.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            let job = background_jobs.try_recv().unwrap();
            job(&store);
            assert_eq!(completion.await.unwrap(), 1);

            let mut completion = std::pin::pin!(client.call_search_completion(|_| 2));
            assert!(futures::poll!(completion.as_mut()).is_pending());
            drop(background_jobs);
            assert!(futures::poll!(completion.as_mut()).is_pending());
            let job = reader_jobs.try_recv().unwrap();
            job(&store);
            assert_eq!(completion.await.unwrap(), 2);
        });
    }

    #[test]
    fn a_slow_search_does_not_hold_up_the_reader() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let path = dir.join("mail.db");
        let opened = path.clone();
        let (client, ready) = spawn(move || (Store::open(&opened).unwrap(), ()));
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
    }

    #[test]
    fn the_background_lane_ends_with_its_clients() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let path = dir.join("mail.db");
        let opened = path.clone();
        let (client, ready) = spawn(move || (Store::open(&opened).unwrap(), ()));
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
    }
}
