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
//! server actions the window queued, on a separate `SELECT` session
//! ([`AccountConfig::replay_outbox`]). The window asks for that right after
//! a local edit. An action the server will never get is reported
//! once, for the status bar.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use fell_core::{
    Account, AccountConfig, BackendKind, DroppedWrite, InboxWait, MessageId, MessageParts,
    ReplyToBackfill, ReplyToProgress, Store, SyncFailureKind, SyncReport, diagnostic,
    fetch_message_bodies, fetch_missing_bodies, sync_with_arrivals,
};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;

use crate::model::{SyncFailure, SyncKind};
use crate::notifications;
use crate::worker::{Restart, Worker, WorkerEvent};

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
/// Space out progress so a fast first sync does not ask the window to reread
/// the folder for every 32-message batch.
const PROGRESS_EVERY: Duration = Duration::from_millis(750);
/// A passive checkpoint after this many committed batches keeps the WAL from
/// growing without bound during a long first sync.
const CHECKPOINT_EVERY: usize = 16;
/// Quiet retries of opening the mailbox before the window shows an error.
const OPEN_RETRIES_BEFORE_ERROR: usize = 3;
/// Batches of old mail to verify or fill after a pass. The worker checks
/// for a new pass or an on-demand body request between batches.
const BACKFILL_BATCHES_AFTER_PASS: usize = 5;

/// Body asks served in one backend call, so a conversation of old mail is
/// one login.
const BODIES_PER_BATCH: usize = 16;
/// Body asks served between two passes. The rest wait for the next gap, so
/// opening a long thread of old mail cannot hold up the next pass.
const BODIES_BETWEEN_PASSES: usize = 48;

/// What the sync thread tells the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncEvent {
    Worker(WorkerEvent),
    /// The status to show. `Syncing`, and `Synced` with `changed`, can also
    /// refresh mail that has just been written.
    Status(SyncKind),
    /// Exact Sent copies confirmed these drafts.
    SendsConfirmed(Vec<String>),
    /// A conflict during replay changed a local flag.
    FlagsChanged,
    /// Write-back problems to show together in one notice.
    WriteBackNotice {
        dropped: Vec<DroppedWrite>,
        flag_recovery_delayed: bool,
        sent_append_delayed: bool,
        draft_append_delayed: bool,
        draft_append_recovered: bool,
    },
    /// Parts were stored for mail that had none (the backfill after a
    /// pass). An open thread may have HTML to show now.
    BodiesStored,
    /// Cached previews were rebuilt from their stored HTML.
    SnippetsStored,
    ReplyTo(ReplyToProgress),
}

#[derive(Default)]
struct PassProgress {
    last_sent: Option<Instant>,
    sent_positive: bool,
    sent_total: bool,
}

impl PassProgress {
    fn update(&mut self, report: &SyncReport, now: Instant) -> Option<SyncKind> {
        let positive = report.new_messages.is_some_and(|count| count > 0);
        // A copy or flag update can use the first slot. The first real
        // arrival still gets through immediately; later counts share the
        // regular cadence and the final total goes in the completion event.
        let first_positive = positive && !self.sent_positive;
        // The first header batch carries EXISTS. Publish its total even if
        // an earlier update took the first slot in the progress cadence.
        let first_total = report.progress.is_some() && !self.sent_total;
        let due = self
            .last_sent
            .is_none_or(|at| now.duration_since(at) >= PROGRESS_EVERY);
        if !due && !first_positive && !first_total {
            return None;
        }
        self.last_sent = Some(now);
        self.sent_positive |= positive;
        self.sent_total |= report.progress.is_some();
        Some(SyncKind::Syncing {
            new_messages: report.new_messages,
            progress: report.progress.clone(),
        })
    }
}

/// Why the window woke the sync thread for a pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ask {
    /// `space s`: a pass now, shown as connecting.
    Sync,
    /// A local edit was queued for the server: send it now, quietly.
    WriteBack,
}

/// Whether the fetch left a stored HTML part, or why it did not finish.
pub type BodyReply = Result<bool, String>;

#[derive(Clone)]
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

    /// Send queued writes now, then run a pass, without
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
    notifications: Option<notifications::Queue>,
) -> (SyncHandle, UnboundedReceiver<SyncEvent>) {
    start_with(config, store_path, notifications, || {})
}

fn start_with(
    config: AccountConfig,
    store_path: PathBuf,
    notifications: Option<notifications::Queue>,
    mut before_run: impl FnMut() + Send + 'static,
) -> (SyncHandle, UnboundedReceiver<SyncEvent>) {
    let (events_tx, events) = unbounded();
    let (wake, wake_rx) = mpsc::channel();
    let write_back = config.write_back;
    std::thread::Builder::new()
        .name("fell-sync".into())
        .spawn(move || {
            let mut restart = Restart::new(Worker::Sync, |event| {
                events_tx.unbounded_send(SyncEvent::Worker(event)).is_ok()
            });
            restart.run(|recovered| {
                before_run();
                run(
                    &config,
                    &store_path,
                    &events_tx,
                    &wake_rx,
                    recovered,
                    notifications.as_ref(),
                );
            });
        })
        .expect("spawn sync thread");
    (SyncHandle { wake, write_back }, events)
}

fn run(
    config: &AccountConfig,
    store_path: &std::path::Path,
    events: &UnboundedSender<SyncEvent>,
    wake: &Receiver<Request>,
    recovered: &mut dyn FnMut(),
    notifications: Option<&notifications::Queue>,
) {
    let bridge = config.backend == BackendKind::Bridge;
    let send = |kind: SyncKind| events.unbounded_send(SyncEvent::Status(kind)).is_ok();

    // Another connection may still be opening or migrating the file. Retry
    // instead of giving up on a lock.
    let mut failures = 0usize;
    let store = loop {
        match Store::open(store_path) {
            Ok(store) => break store,
            Err(err) => {
                diagnostic!(
                    DEBUG,
                    "Sync could not open mailbox",
                    error = &err as &dyn std::error::Error
                );
                let delay = BACKOFF[failures.min(BACKOFF.len() - 1)] / 5;
                failures += 1;
                // A lock held while the window's connection finishes opening
                // clears in a moment. Say "offline" only if it keeps failing.
                let status = if failures < OPEN_RETRIES_BEFORE_ERROR {
                    SyncKind::Connecting
                } else {
                    SyncKind::Failed(SyncFailure {
                        kind: SyncFailureKind::Other,
                        bridge,
                        detail: format!("sync could not open the mailbox: {err}"),
                        recovery: None,
                    })
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
    let mut reply_to = ReplyToBackfill::default();
    let mut snippets_repaired = false;
    loop {
        // Replay writes before the read-only pass, so it sees their results.
        // A failure here is a connection problem the pass reports.
        if let Ok(report) = config.replay_outbox(&store) {
            if report.flags_changed
                && let Some(queue) = &notifications
            {
                queue.refresh();
            }
            if report.flags_changed && events.unbounded_send(SyncEvent::FlagsChanged).is_err() {
                return;
            }
            if (!report.dropped.is_empty()
                || report.flag_recovery_delayed
                || report.sent_append_delayed
                || report.draft_append_delayed
                || report.draft_append_recovered)
                && events
                    .unbounded_send(SyncEvent::WriteBackNotice {
                        dropped: report.dropped,
                        flag_recovery_delayed: report.flag_recovery_delayed,
                        sent_append_delayed: report.sent_append_delayed,
                        draft_append_delayed: report.draft_append_delayed,
                        draft_append_recovered: report.draft_append_recovered,
                    })
                    .is_err()
            {
                return;
            }
        }
        diagnostic!(DEBUG, "Sync pass started", write_back = config.write_back);
        let mut alive = true;
        let mut progress = PassProgress::default();
        let mut batches = 0usize;
        let mut confirmed = 0;
        let mut on_arrivals = |arrivals| {
            if let Some(queue) = notifications {
                queue.committed(arrivals);
            }
        };
        let result = config.backend().and_then(|backend| {
            sync_with_arrivals(
                &account,
                backend.as_ref(),
                &store,
                &mut |report| {
                    if report.confirmed_drafts.len() > confirmed {
                        alive &= events
                            .unbounded_send(SyncEvent::SendsConfirmed(
                                report.confirmed_drafts[confirmed..].to_vec(),
                            ))
                            .is_ok();
                        confirmed = report.confirmed_drafts.len();
                    }
                    batches += 1;
                    if batches.is_multiple_of(CHECKPOINT_EVERY) {
                        let _ = store.checkpoint(false);
                    }
                    if let Some(status) = progress.update(report, Instant::now()) {
                        alive &= send(status);
                    }
                },
                notifications.map(|_| &mut on_arrivals as &mut dyn FnMut(_)),
            )
        });
        if let Some(queue) = notifications {
            queue.pass_ended();
        }
        if !alive {
            return;
        }
        // Fold the log back into the file between passes so readers stay fast.
        let _ = store.checkpoint(true);
        let synced = result.is_ok();
        let delay = match result {
            Ok(report) => {
                recovered();
                diagnostic!(
                    INFO,
                    "Sync pass completed",
                    batches = batches as u64,
                    changed = report.changed
                );
                failures = 0;
                if !send(SyncKind::Synced {
                    at: unix_now(),
                    changed: report.changed,
                    new_messages: report.new_messages,
                }) {
                    return;
                }
                INTERVAL
            }
            Err(err) => {
                let delay = BACKOFF[failures.min(BACKOFF.len() - 1)];
                failures += 1;
                diagnostic!(
                    WARN,
                    "Sync pass failed",
                    error = &err as &dyn std::error::Error
                );
                if !send(SyncKind::Failed(SyncFailure::connection(config, &err))) {
                    return;
                }
                delay
            }
        };
        // A body asked for on open is served at once, then the wait goes on
        // for the rest of the delay. Asks left over from the last gap go
        // first.
        bodies.new_gap();
        bodies.serve(config, &store, &account);
        let until = Instant::now() + delay;
        if !snippets_repaired {
            let mut changed = false;
            for _ in 0..BACKFILL_BATCHES_AFTER_PASS {
                match store.repair_html_snippets() {
                    Ok(Some(updated)) => changed |= updated,
                    Ok(None) => {
                        snippets_repaired = true;
                        break;
                    }
                    Err(_) => break,
                }
            }
            if changed && events.unbounded_send(SyncEvent::SnippetsStored).is_err() {
                return;
            }
        }
        let mut pending = if synced {
            fill_old_mail(
                config,
                &store,
                &account,
                wake,
                &mut bodies,
                &mut reply_to,
                events,
            )
        } else {
            None
        };
        let woke = loop {
            match pending.take() {
                Some(Woke::Body(ask)) => {
                    bodies.push(ask);
                    // The asks a conversation sends together go in one batch.
                    if let Some(woke) = queue_bodies(wake, &mut bodies) {
                        break woke;
                    }
                    bodies.serve(config, &store, &account);
                }
                Some(woke) => break woke,
                None => {}
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break Woke::Timer;
            }
            pending = Some(if synced && idle {
                wait_on_inbox(config, &store, &account, left, wake, &mut idle)
            } else {
                wait_on_timer(wake, left)
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
                bodies.serve(config, &store, &account);
                if shown && !send(SyncKind::Connecting) {
                    return;
                }
            }
            Woke::Mail | Woke::Timer | Woke::Body(_) => {}
            Woke::Closed => return,
        }
    }
}

/// Fetch missing Reply-To headers and body parts, a batch at a time.
/// Best effort: a failure waits for the next pass. `Some` when the
/// window asked for a pass, or closed, in the meantime.
fn fill_old_mail(
    config: &AccountConfig,
    store: &Store,
    account: &Account,
    wake: &Receiver<Request>,
    bodies: &mut BodyQueue,
    reply_to: &mut ReplyToBackfill,
    events: &UnboundedSender<SyncEvent>,
) -> Option<Woke> {
    let Ok(backend) = config.backend() else {
        return None;
    };
    let (mut more_headers, mut more_bodies) = (true, true);
    for _ in 0..BACKFILL_BATCHES_AFTER_PASS {
        if let Some(woke) = queue_bodies(wake, bodies) {
            return Some(woke);
        }
        bodies.serve(config, store, account);
        if more_headers {
            more_headers = matches!(
                reply_to.run_batch_with_progress(
                    &account.id,
                    backend.as_ref(),
                    store,
                    &mut |progress| {
                        let _ = events.unbounded_send(SyncEvent::ReplyTo(progress));
                    },
                ),
                Ok(true)
            );
        }
        if more_bodies {
            more_bodies = fetch_missing_bodies(&account.id, backend.as_ref(), store, 1)
                .is_ok_and(|filled| filled > 0);
            if more_bodies {
                let _ = events.unbounded_send(SyncEvent::BodiesStored);
            }
        }
        if !more_headers && !more_bodies {
            break;
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
mod progress_tests {
    use super::*;

    #[test]
    fn the_first_mailbox_total_is_published_between_progress_ticks() {
        let mut pass = PassProgress::default();
        let start = Instant::now();
        let mut report = SyncReport {
            new_messages: Some(61),
            ..SyncReport::default()
        };
        assert!(pass.update(&report, start).is_some());

        let total = fell_core::SyncProgress {
            folder: "Inbox".into(),
            done: 0,
            total: 11_591,
        };
        report.progress = Some(total.clone());
        let soon = start + Duration::from_millis(1);
        assert!(matches!(
            pass.update(&report, soon),
            Some(SyncKind::Syncing { progress: Some(progress), .. }) if progress == total
        ));
        assert!(pass.update(&report, soon).is_none());
    }

    #[test]
    fn arrival_after_a_copy_bypasses_the_throttle_once() {
        let mut progress = PassProgress::default();
        let mut report = SyncReport {
            changed: true,
            ..SyncReport::default()
        };
        let start = Instant::now();
        assert_eq!(
            progress.update(&report, start),
            Some(SyncKind::Syncing {
                new_messages: Some(0),
                progress: None,
            })
        );

        let arrival = start + Duration::from_millis(20);
        report.new_messages = Some(1);
        assert_eq!(
            progress.update(&report, arrival),
            Some(SyncKind::Syncing {
                new_messages: Some(1),
                progress: None,
            })
        );
        assert_eq!(
            progress.update(&report, arrival + Duration::from_millis(10)),
            None,
            "another folder copy leaves the cumulative count at one"
        );

        report.new_messages = Some(2);
        assert_eq!(progress.update(&report, start + PROGRESS_EVERY), None);
        assert_eq!(
            progress.update(&report, arrival + PROGRESS_EVERY),
            Some(SyncKind::Syncing {
                new_messages: Some(2),
                progress: None,
            })
        );
    }
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

#[cfg(test)]
mod restart_tests {
    use super::*;
    use fell_core::diagnostics::{self, DiagnosticFile};
    use std::os::unix::fs::PermissionsExt as _;

    #[test]
    fn injected_panic_writes_report_and_sync_restarts() {
        // Installing a process-global panic hook in the child keeps it out
        // of concurrent tests in the unit test process.
        const CHILD: &str = "FELL_TEST_SYNC_PANIC_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let state = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "sync_worker::restart_tests::injected_panic_writes_report_and_sync_restarts",
                    "--nocapture",
                ])
                .env(CHILD, "1")
                .env("XDG_STATE_HOME", state.path())
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        diagnostics::init().unwrap();
        let config = AccountConfig::bridge(
            "demo@example.test",
            "demo",
            // Fail before connecting to any host.
            fell_core::PasswordSource::Command("exit 1".into()),
        );
        let path =
            std::path::PathBuf::from(std::env::var_os("XDG_STATE_HOME").unwrap()).join("mail.db");
        let mut first = true;
        let (_handle, mut events) = start_with(config, path, None, move || {
            assert!(!std::mem::take(&mut first), "synthetic-panic-payload");
        });

        // Symbolizing the panic can take several seconds in a debug build
        // when the rest of the workspace suite is busy.
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut restarting = false;
        loop {
            match events.try_recv() {
                Ok(SyncEvent::Worker(WorkerEvent::Restarted(Worker::Sync))) => restarting = true,
                Ok(SyncEvent::Status(SyncKind::Failed(failure))) => {
                    assert!(restarting);
                    assert_eq!(failure.kind, SyncFailureKind::Password);
                    break;
                }
                Ok(event) => panic!("unexpected sync event: {event:?}"),
                Err(_) => {
                    if Instant::now() >= deadline {
                        eprintln!(
                            "sync worker did not finish another pass; restart notice: {restarting}"
                        );
                        panic!("sync worker never restarted");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
        let path = DiagnosticFile::Crash.path().unwrap();
        assert_eq!(path.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        let report = std::fs::read_to_string(path).unwrap();
        assert!(report.contains("Thread: fell-sync"));
        assert!(report.contains("Backtrace"));
        assert!(report.contains("Recovered by worker: true"));
        assert!(!report.contains("synthetic-panic-payload"));
        assert!(diagnostics::take_crash_notice().is_none());
    }
}
