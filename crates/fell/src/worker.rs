//! Restart a panicked worker without dropping its request channel.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use fell_core::{diagnostic, diagnostics};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Worker {
    Sync,
    Store,
    BackgroundStore,
}

impl Worker {
    pub fn restart_notice(self) -> &'static str {
        match self {
            Self::Sync => "Sync restarted after an error",
            Self::Store | Self::BackgroundStore => "Mailbox restarted after an error",
        }
    }

    pub fn outage_notice(self) -> &'static str {
        match self {
            Self::Sync => "Sync stopped after repeated errors",
            Self::Store | Self::BackgroundStore => "Mailbox unavailable",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorkerEvent {
    Restarted(Worker),
    Stopped(Worker),
    Recovered(Worker),
    SnoozesChanged,
}

const PANIC_WINDOW: Duration = Duration::from_secs(60);
const PANIC_LIMIT: usize = 3;

#[derive(Default)]
struct Crashes {
    recent: VecDeque<Instant>,
    delay: Duration,
    stopped: bool,
}

impl Crashes {
    fn record(&mut self, now: Instant) {
        self.recent
            .retain(|at| now.duration_since(*at) <= PANIC_WINDOW);
        self.delay = if self.recent.is_empty() || self.delay.is_zero() {
            Duration::from_millis(500)
        } else {
            (self.delay * 2).min(Duration::from_secs(30))
        };
        if self.recent.len() == PANIC_LIMIT {
            self.recent.pop_front();
        }
        self.recent.push_back(now);
        self.stopped |= self.recent.len() == PANIC_LIMIT;
    }

    fn recover(&mut self) -> bool {
        self.delay = Duration::ZERO;
        if std::mem::take(&mut self.stopped) {
            self.recent.clear();
            return true;
        }
        false
    }
}

pub struct Restart<Notify> {
    worker: Worker,
    notify: Notify,
    crashes: Crashes,
}

impl<Notify: FnMut(WorkerEvent) -> bool> Restart<Notify> {
    pub fn new(worker: Worker, notify: Notify) -> Self {
        Self {
            worker,
            notify,
            crashes: Crashes::default(),
        }
    }

    /// Work may return normally when its channel closes. A panic unwinds all
    /// work locals (including SQLite transactions and active reply senders)
    /// before we wait and enter it again.
    /// Call the recovery callback after a complete operation on the lane.
    pub fn run<T>(&mut self, mut work: impl FnMut(&mut dyn FnMut()) -> T) -> Option<T> {
        loop {
            match diagnostics::catch_worker(|| {
                work(&mut || {
                    if self.crashes.recover() {
                        (self.notify)(WorkerEvent::Recovered(self.worker));
                    }
                })
            }) {
                Ok(result) => return Some(result),
                Err(_) => self.crashes.record(Instant::now()),
            }
            let retry_ms = self.crashes.delay.as_millis() as u64;
            match self.worker {
                Worker::Sync => diagnostic!(
                    ERROR,
                    "Sync worker panicked; restarting",
                    retry_ms = retry_ms
                ),
                Worker::Store => diagnostic!(
                    ERROR,
                    "Mailbox worker panicked; restarting",
                    retry_ms = retry_ms
                ),
                Worker::BackgroundStore => diagnostic!(
                    ERROR,
                    "Mailbox background worker panicked; restarting",
                    retry_ms = retry_ms
                ),
            }
            // Keep checking that the receiver is live in a crash loop. The
            // window treats further Stopped events as the same outage.
            if self.crashes.stopped && !(self.notify)(WorkerEvent::Stopped(self.worker)) {
                return None;
            }
            std::thread::sleep(self.crashes.delay);
            if !self.crashes.stopped && !(self.notify)(WorkerEvent::Restarted(self.worker)) {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_panics_in_a_minute_stop_notices_until_a_clean_run() {
        let start = Instant::now();
        let mut crashes = Crashes::default();
        for second in [0, 25, 61] {
            crashes.record(start + Duration::from_secs(second));
            assert!(!crashes.stopped);
        }
        // An isolated successful operation can reset the backoff without
        // hiding several panics in the same minute.
        assert!(!crashes.recover());
        crashes.record(start + Duration::from_secs(62));
        assert!(crashes.stopped);
        for second in [95, 130, 170, 240] {
            crashes.record(start + Duration::from_secs(second));
            assert!(crashes.stopped);
        }
        assert!(crashes.recover());
        assert!(!crashes.recover());
        crashes.record(start + Duration::from_secs(241));
        assert!(!crashes.stopped);
        assert_eq!(crashes.delay, Duration::from_millis(500));
    }
}
