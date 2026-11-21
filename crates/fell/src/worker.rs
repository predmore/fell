//! Restart a panicked worker without dropping its request channel.

use std::time::{Duration, Instant};

use fell_core::{diagnostic, diagnostics};

#[derive(Clone, Copy)]
pub enum Worker {
    Sync,
    Store,
    BackgroundStore,
}

impl Worker {
    pub fn notice(self) -> &'static str {
        match self {
            Self::Sync => "Sync worker stopped · restarting",
            Self::Store => "Mailbox worker stopped · restarting",
            Self::BackgroundStore => "Mailbox background worker stopped · restarting",
        }
    }
}

pub struct Restart<Notify> {
    worker: Worker,
    notify: Notify,
    previous: Option<Instant>,
    delay: Duration,
}

impl<Notify: FnMut(Worker) -> bool> Restart<Notify> {
    pub fn new(worker: Worker, notify: Notify) -> Self {
        Self {
            worker,
            notify,
            previous: None,
            delay: Duration::ZERO,
        }
    }

    /// Work may return normally when its channel closes. A panic unwinds all
    /// work locals (including SQLite transactions and active reply senders)
    /// before we wait and enter it again.
    pub fn run<T>(&mut self, mut work: impl FnMut() -> T) -> Option<T> {
        loop {
            match diagnostics::catch_worker(&mut work) {
                Ok(result) => return Some(result),
                Err(_) => {
                    let now = Instant::now();
                    if self
                        .previous
                        .is_some_and(|at| now.duration_since(at) < Duration::from_secs(60))
                    {
                        self.delay = (self.delay * 2).min(Duration::from_secs(30));
                    } else {
                        self.delay = Duration::from_millis(500);
                    }
                    self.previous = Some(now);
                    let retry_ms = self.delay.as_millis() as u64;
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
                    if !(self.notify)(self.worker) {
                        return None;
                    }
                    std::thread::sleep(self.delay);
                }
            }
        }
    }
}
