//! Background sync for the configured account.
//!
//! One OS thread owns its own [`Store`] connection on the mailbox file and runs
//! read-only IMAP passes: one at startup, then one every [`INTERVAL`], sooner
//! when the window asks. The window never waits on it: status arrives on an
//! async channel, and the window rereads the store on its own store thread.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fell_core::{
    AccountConfig, BackendKind, Store, SyncFailureKind, classify_sync_error, sync_with_progress,
};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

use crate::model::{SyncFailure, SyncKind};

/// Time between passes after a pass succeeds. IDLE is later work.
const INTERVAL: Duration = Duration::from_secs(60);
/// Waits after consecutive failures. The last one repeats.
const BACKOFF: [Duration; 4] = [
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(180),
];
/// Progress is reported at most this often, so a fast first sync does not
/// ask the window to reread the folder for every 32-message batch.
const PROGRESS_EVERY: Duration = Duration::from_millis(750);
/// A passive checkpoint after this many committed batches keeps the WAL from
/// growing without bound during a long first sync.
const CHECKPOINT_EVERY: usize = 16;
/// Quiet retries of opening the mailbox before the window shows an error.
const OPEN_RETRIES_BEFORE_ERROR: usize = 3;

/// The status to show. `Syncing`, and `Synced` with `stored > 0`, mean the
/// store has new rows.
pub type SyncEvent = SyncKind;

pub struct SyncHandle {
    wake: Sender<()>,
}

impl SyncHandle {
    /// Start the next pass now. A pass already running finishes first.
    pub fn sync_now(&self) {
        let _ = self.wake.send(());
    }
}

pub fn start(
    config: AccountConfig,
    store_path: PathBuf,
) -> (SyncHandle, UnboundedReceiver<SyncEvent>) {
    let (events_tx, events) = unbounded();
    let (wake, wake_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("fell-sync".into())
        .spawn(move || run(config, store_path, events_tx, wake_rx))
        .expect("spawn sync thread");
    (SyncHandle { wake }, events)
}

fn run(
    config: AccountConfig,
    store_path: PathBuf,
    events: UnboundedSender<SyncEvent>,
    wake: Receiver<()>,
) {
    let bridge = config.backend == BackendKind::Bridge;
    let send = |kind: SyncKind| events.unbounded_send(kind).is_ok();
    let failed = |kind: SyncFailureKind, detail: String| {
        SyncKind::Failed(SyncFailure {
            kind,
            bridge,
            detail,
        })
    };

    // Another connection may still be opening or migrating the file. Retry
    // instead of giving up on a lock.
    let mut failures = 0usize;
    let store = loop {
        match Store::open(&store_path) {
            Ok(store) => break store,
            Err(err) => {
                let delay = BACKOFF[failures.min(BACKOFF.len() - 1)] / 5;
                failures += 1;
                // A lock held while the window's connection finishes opening
                // clears in a moment. Say "offline" only if it keeps failing.
                let status = if failures < OPEN_RETRIES_BEFORE_ERROR {
                    SyncKind::Connecting
                } else {
                    failed(
                        SyncFailureKind::Other,
                        format!("sync could not open the mailbox: {err}"),
                    )
                };
                if !send(status) {
                    return;
                }
                match wake.recv_timeout(delay) {
                    Ok(()) | Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    };
    let account = config.account();
    failures = 0;
    loop {
        let mut alive = true;
        let mut last_progress: Option<Instant> = None;
        let mut batches = 0usize;
        let result = config.backend().and_then(|backend| {
            sync_with_progress(&account, backend.as_ref(), &store, &mut |report| {
                batches += 1;
                if batches.is_multiple_of(CHECKPOINT_EVERY) {
                    let _ = store.checkpoint(false);
                }
                if last_progress.is_none_or(|at| at.elapsed() >= PROGRESS_EVERY) {
                    last_progress = Some(Instant::now());
                    alive &= send(SyncKind::Syncing {
                        stored: report.upserted,
                    });
                }
            })
        });
        if !alive {
            return;
        }
        // Fold the log back into the file between passes so readers stay fast.
        let _ = store.checkpoint(true);
        let delay = match result {
            Ok(report) => {
                failures = 0;
                if !send(SyncKind::Synced {
                    at: unix_now(),
                    stored: report.upserted,
                }) {
                    return;
                }
                INTERVAL
            }
            Err(err) => {
                let delay = BACKOFF[failures.min(BACKOFF.len() - 1)];
                failures += 1;
                if !send(failed(classify_sync_error(&err), err.to_string())) {
                    return;
                }
                delay
            }
        };
        match wake.recv_timeout(delay) {
            Ok(()) => {
                while wake.try_recv().is_ok() {}
                if !send(SyncKind::Connecting) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}
