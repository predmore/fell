//! Background sync for the configured account.
//!
//! One OS thread owns its own [`Store`] connection on the mailbox file and runs
//! read-only IMAP passes: one at startup, then one every [`INTERVAL`], sooner
//! when the window asks. Between passes it waits on INBOX with IDLE, so new
//! mail starts a pass within moments instead of up to a minute later. A
//! server without IDLE falls back to the timer. The window never waits on
//! it: status arrives on an async channel, and the window rereads the store
//! on its own store thread.
//!
//! With `write_back = true` in `account.toml`, each pass first sends the
//! archive and move rows the window queued, on a separate `SELECT` session
//! ([`AccountConfig::replay_outbox`]). The window asks for that right after
//! an archive, move, or undo. A move the server will never get is reported
//! once, for the status bar.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fell_core::{
    Account, AccountConfig, BackendKind, DropReason, InboxWait, MessageId, MessageParts,
    ReplayReport, Store, SyncFailureKind, classify_sync_error, fetch_message_bodies,
    fetch_missing_bodies, sync_with_progress,
};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;

use crate::model::{SyncFailure, SyncKind};

/// Longest time between passes after a pass succeeds. IDLE on INBOX starts
/// one sooner when mail arrives; other folders are caught up on this timer.
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
/// Batches of old mail (200 messages each) whose body parts are fetched
/// after a pass. A request for a pass is checked between batches.
const BODY_BATCHES_AFTER_PASS: usize = 5;

/// Body asks served in one backend call, so a conversation of old mail is
/// one login.
const BODIES_PER_BATCH: usize = 16;
/// Body asks served between two passes. The rest wait for the next gap, so
/// opening a long thread of old mail cannot hold up the next pass.
const BODIES_BETWEEN_PASSES: usize = 48;

/// What the sync thread tells the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    /// The status to show. `Syncing`, and `Synced` with `stored > 0`, mean
    /// the store has new rows.
    Status(SyncKind),
    /// Moves the server will never get. The change stays in Fell.
    NotSent(NotSent),
    /// Parts were stored for mail that had none (the backfill after a
    /// pass). An open thread may have HTML to show now.
    BodiesStored,
}

/// The first reason in a replay's dropped moves, and how many rows it
/// covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotSent {
    pub reason: DropReason,
    /// The folder the messages were to leave on the server.
    pub folder: String,
    pub count: usize,
}

impl NotSent {
    fn from_report(report: &ReplayReport) -> Option<Self> {
        let first = report.dropped.first()?;
        let count = report
            .dropped
            .iter()
            .filter(|dropped| dropped.reason == first.reason)
            .count();
        Some(Self {
            reason: first.reason,
            folder: first.source_folder.clone(),
            count,
        })
    }
}

/// Why the window woke the sync thread for a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// `space s`: a pass now, shown as connecting.
    Sync,
    /// Archive, move, or undo was queued for the server: send it now,
    /// quietly.
    WriteBack,
}

/// Whether the fetch left a stored HTML part, or why it did not finish.
pub type BodyReply = Result<bool, String>;

pub struct SyncHandle {
    wake: Sender<Request>,
    /// `write_back` in the account file.
    write_back: bool,
}

impl SyncHandle {
    /// Start the next pass now. A pass already running finishes first.
    pub fn sync_now(&self) {
        let _ = self.wake.send(Request::Ask(Ask::Sync));
    }

    /// Send queued archive and move rows now, then run a pass, without
    /// showing "connecting". Nothing is queued while write-back is off.
    pub fn write_back_now(&self) {
        if self.write_back {
            let _ = self.wake.send(Request::Ask(Ask::WriteBack));
        }
    }

    /// Fetch one message's parts from the server and store them, for a
    /// conversation that opened on mail stored before parts were kept. It
    /// runs between passes, ahead of the backfill and the wait for mail.
    pub fn fetch_body(&self, message_id: MessageId) -> oneshot::Receiver<BodyReply> {
        let (reply, answer) = oneshot::channel();
        let _ = self.wake.send(Request::Body(BodyAsk { message_id, reply }));
        answer
    }
}

/// What the window asks of the sync thread.
enum Request {
    /// A pass now.
    Ask(Ask),
    Body(BodyAsk),
}

struct BodyAsk {
    message_id: MessageId,
    reply: oneshot::Sender<BodyReply>,
}

/// Body asks waiting for the sync thread, served a batch per backend call
/// and at most [`BODIES_BETWEEN_PASSES`] between two passes.
#[derive(Default)]
struct BodyQueue {
    waiting: Vec<BodyAsk>,
    served: usize,
}

impl BodyQueue {
    fn push(&mut self, ask: BodyAsk) {
        self.waiting.push(ask);
    }

    /// A pass finished: the next gap has its full allowance.
    fn new_gap(&mut self) {
        self.served = 0;
    }

    /// The next batch this gap may still serve. Asks the window stopped
    /// waiting for are dropped, and cost nothing.
    fn take_batch(&mut self) -> Vec<BodyAsk> {
        self.waiting.retain(|ask| !ask.reply.is_canceled());
        let room = BODIES_BETWEEN_PASSES
            .saturating_sub(self.served)
            .min(BODIES_PER_BATCH)
            .min(self.waiting.len());
        self.served += room;
        self.waiting.drain(..room).collect()
    }

    /// Serve what this gap allows, one backend call per batch.
    fn serve(&mut self, config: &AccountConfig, store: &Store, account: &Account) {
        loop {
            let batch = self.take_batch();
            if batch.is_empty() {
                return;
            }
            let ids: Vec<MessageId> = batch.iter().map(|ask| ask.message_id.clone()).collect();
            let answers = config
                .backend()
                .and_then(|backend| {
                    fetch_message_bodies(&account.id, backend.as_ref(), store, &ids)
                })
                .map_err(|err| err.to_string());
            match answers {
                Ok(answers) => {
                    for (ask, parts) in batch.into_iter().zip(answers) {
                        let _ = ask.reply.send(body_answer(parts));
                    }
                }
                Err(err) => {
                    for ask in batch {
                        let _ = ask.reply.send(Err(err.clone()));
                    }
                }
            }
        }
    }
}

/// The window only needs to know whether to reread for HTML. A fetch
/// that stopped short of this message can be asked for again later.
fn body_answer(parts: Option<MessageParts>) -> BodyReply {
    match parts {
        None => Err("not fetched yet".to_string()),
        Some(parts) => Ok(parts.html.is_some()),
    }
}

/// The mailbox is not open yet: nothing can be fetched.
fn refuse_body(ask: BodyAsk) {
    let _ = ask
        .reply
        .send(Err("the mailbox is not open yet".to_string()));
}

pub fn start(
    config: AccountConfig,
    store_path: PathBuf,
) -> (SyncHandle, UnboundedReceiver<SyncEvent>) {
    let (events_tx, events) = unbounded();
    let (wake, wake_rx) = mpsc::channel();
    let write_back = config.write_back;
    std::thread::Builder::new()
        .name("fell-sync".into())
        .spawn(move || run(config, store_path, events_tx, wake_rx))
        .expect("spawn sync thread");
    (SyncHandle { wake, write_back }, events)
}

fn run(
    config: AccountConfig,
    store_path: PathBuf,
    events: UnboundedSender<SyncEvent>,
    wake: Receiver<Request>,
) {
    let bridge = config.backend == BackendKind::Bridge;
    let send = |kind: SyncKind| events.unbounded_send(SyncEvent::Status(kind)).is_ok();
    let bodies_stored = || {
        let _ = events.unbounded_send(SyncEvent::BodiesStored);
    };
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
                    Ok(Request::Body(ask)) => refuse_body(ask),
                    Ok(Request::Ask(_)) | Err(RecvTimeoutError::Timeout) => continue,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    };
    let account = config.account();
    failures = 0;
    // Cleared for good once the server says it has no IDLE.
    let mut idle = true;
    let mut bodies = BodyQueue::default();
    loop {
        // Moves go first, so the pass that follows sees where they landed.
        // A failure here is a connection problem the pass reports.
        if let Ok(report) = config.replay_outbox(&store)
            && let Some(not_sent) = NotSent::from_report(&report)
            && events.unbounded_send(SyncEvent::NotSent(not_sent)).is_err()
        {
            return;
        }
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
                        progress: report.progress.clone(),
                    });
                }
            })
        });
        if !alive {
            return;
        }
        // Fold the log back into the file between passes so readers stay fast.
        let _ = store.checkpoint(true);
        let synced = result.is_ok();
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
        // A body asked for on open is served at once, then the wait goes on
        // for the rest of the delay. Asks left over from the last gap go
        // first.
        bodies.new_gap();
        bodies.serve(&config, &store, &account);
        let until = Instant::now() + delay;
        let mut pending = if synced {
            fill_old_bodies(
                &config,
                &store,
                &account,
                &wake,
                &mut bodies,
                &bodies_stored,
            )
        } else {
            None
        };
        let woke = loop {
            match pending.take() {
                Some(Woke::Body(ask)) => {
                    bodies.push(ask);
                    // The asks a conversation sends together go in one batch.
                    if let Some(woke) = queue_bodies(&wake, &mut bodies) {
                        break woke;
                    }
                    bodies.serve(&config, &store, &account);
                }
                Some(woke) => break woke,
                None => {}
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Woke::Timer;
            }
            pending = Some(if synced && idle {
                wait_on_inbox(&config, &store, &account, left, &wake, &mut idle)
            } else {
                wait_on_timer(&wake, left)
            });
        };
        match woke {
            Woke::Asked(ask) => {
                // Later asks for a pass fold into this one; a body ask
                // waiting behind them is answered before the pass starts.
                let mut shown = ask == Ask::Sync;
                while let Ok(request) = wake.try_recv() {
                    match request {
                        Request::Body(ask) => bodies.push(ask),
                        Request::Ask(more) => shown |= more == Ask::Sync,
                    }
                }
                bodies.serve(&config, &store, &account);
                if shown && !send(SyncKind::Connecting) {
                    return;
                }
            }
            Woke::Mail | Woke::Timer | Woke::Body(_) => {}
            Woke::Closed => return,
        }
    }
}

/// Fetch body parts for mail stored before they were kept, a batch at a
/// time. Best effort: a failure waits for the next pass. `Some` when the
/// window asked for a pass, or closed, in the meantime.
fn fill_old_bodies(
    config: &AccountConfig,
    store: &Store,
    account: &Account,
    wake: &Receiver<Request>,
    bodies: &mut BodyQueue,
    stored: &dyn Fn(),
) -> Option<Woke> {
    let Ok(backend) = config.backend() else {
        return None;
    };
    for _ in 0..BODY_BATCHES_AFTER_PASS {
        if let Some(woke) = queue_bodies(wake, bodies) {
            return Some(woke);
        }
        bodies.serve(config, store, account);
        match fetch_missing_bodies(&account.id, backend.as_ref(), store, 1) {
            Ok(0) | Err(_) => break,
            Ok(_) => stored(),
        }
    }
    None
}

/// Queue every body ask already sent. `Some` when a pass was asked for or
/// the window closed meanwhile; body asks before it are queued.
fn queue_bodies(wake: &Receiver<Request>, bodies: &mut BodyQueue) -> Option<Woke> {
    loop {
        match wake.try_recv() {
            Ok(Request::Ask(ask)) => return Some(Woke::Asked(ask)),
            Ok(Request::Body(ask)) => bodies.push(ask),
            Err(TryRecvError::Disconnected) => return Some(Woke::Closed),
            Err(TryRecvError::Empty) => return None,
        }
    }
}

/// Why the wait between passes ended.
enum Woke {
    /// The window asked for a pass now.
    Asked(Ask),
    /// The window asked for one message's parts.
    Body(BodyAsk),
    /// IDLE saw INBOX change.
    Mail,
    Timer,
    /// The window is gone.
    Closed,
}

fn wait_on_timer(wake: &Receiver<Request>, delay: Duration) -> Woke {
    match wake.recv_timeout(delay) {
        Ok(Request::Ask(ask)) => Woke::Asked(ask),
        Ok(Request::Body(ask)) => Woke::Body(ask),
        Err(RecvTimeoutError::Timeout) => Woke::Timer,
        Err(RecvTimeoutError::Disconnected) => Woke::Closed,
    }
}

/// IDLE on INBOX for up to `delay`. A server without IDLE clears `idle` and
/// waits on the timer instead. A dropped connection waits out the rest of
/// the delay on the timer; the next pass reports the failure.
fn wait_on_inbox(
    config: &AccountConfig,
    store: &Store,
    account: &Account,
    delay: Duration,
    wake: &Receiver<Request>,
    idle: &mut bool,
) -> Woke {
    let started = Instant::now();
    let cursor = store.sync_cursor(&account.id).ok().flatten();
    let mut asked = None;
    let mut body = None;
    let mut closed = false;
    let outcome = config.backend().and_then(|backend| {
        backend.wait_for_change(cursor.as_ref(), delay, &mut || match wake.try_recv() {
            Ok(Request::Ask(ask)) => {
                asked = Some(ask);
                true
            }
            Ok(Request::Body(ask)) => {
                body = Some(ask);
                true
            }
            Err(TryRecvError::Disconnected) => {
                closed = true;
                true
            }
            Err(TryRecvError::Empty) => false,
        })
    });
    if closed {
        return Woke::Closed;
    }
    if let Some(ask) = asked {
        return Woke::Asked(ask);
    }
    if let Some(ask) = body {
        return Woke::Body(ask);
    }
    match outcome {
        Ok(InboxWait::Changed) => Woke::Mail,
        Ok(InboxWait::Quiet | InboxWait::Stopped) => Woke::Timer,
        Ok(InboxWait::Unsupported) => {
            *idle = false;
            wait_on_timer(wake, delay.saturating_sub(started.elapsed()))
        }
        Err(_) => wait_on_timer(wake, delay.saturating_sub(started.elapsed())),
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod body_queue_tests {
    use super::*;

    fn ask(id: &str) -> (BodyAsk, oneshot::Receiver<BodyReply>) {
        let (reply, answer) = oneshot::channel();
        (
            BodyAsk {
                message_id: MessageId::new(id),
                reply,
            },
            answer,
        )
    }

    #[test]
    fn asks_go_in_batches_and_a_gap_serves_only_so_many() {
        let mut queue = BodyQueue::default();
        let mut answers = Vec::new();
        for index in 0..BODIES_BETWEEN_PASSES + 5 {
            let (body, answer) = ask(&format!("m{index}@x"));
            queue.push(body);
            answers.push(answer);
        }
        let mut sizes = Vec::new();
        loop {
            let batch = queue.take_batch();
            if batch.is_empty() {
                break;
            }
            sizes.push(batch.len());
        }
        assert!(
            sizes.iter().all(|size| *size <= BODIES_PER_BATCH),
            "{sizes:?}"
        );
        assert_eq!(sizes.iter().sum::<usize>(), BODIES_BETWEEN_PASSES);
        // The rest wait for the next gap.
        assert_eq!(queue.waiting.len(), 5);
        queue.new_gap();
        assert_eq!(queue.take_batch().len(), 5);
    }

    #[test]
    fn an_ask_nobody_waits_for_is_dropped() {
        let mut queue = BodyQueue::default();
        let (gone, answer) = ask("gone@x");
        drop(answer);
        let (kept, _answer) = ask("kept@x");
        queue.push(gone);
        queue.push(kept);
        let batch = queue.take_batch();
        assert_eq!(batch.len(), 1);
        assert_eq!(batch[0].message_id, MessageId::new("kept@x"));
    }

    #[test]
    fn only_a_real_answer_is_final() {
        assert!(body_answer(None).is_err());
        assert_eq!(body_answer(Some(MessageParts::default())), Ok(false));
        assert_eq!(
            body_answer(Some(MessageParts {
                plain: Some("plain".into()),
                ..MessageParts::default()
            })),
            Ok(false)
        );
        let parts = MessageParts {
            html: Some("<p>x</p>".into()),
            plain: None,
            bulk_headers: Vec::new(),
            mime_parts: Vec::new(),
        };
        assert_eq!(body_answer(Some(parts)), Ok(true));
    }
}
