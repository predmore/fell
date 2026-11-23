//! On-demand body and part metadata for the open conversation.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

use fell_core::{
    AccountConfig, CancelBodyFetch, MessageId, ReaderBodyFetcher, SyncFailureKind,
    classify_sync_error, diagnostic, diagnostics,
};
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

pub const SHOW_AFTER: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
pub enum Failure {
    Failed,
    Offline,
}

pub enum Event {
    Stored,
    Finished(Result<Vec<MessageId>, Failure>),
}

pub struct Job(CancelBodyFetch);

impl Drop for Job {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

pub struct Handle(mpsc::Sender<Request>);

struct Request {
    ids: Vec<MessageId>,
    cancel: CancelBodyFetch,
    events: UnboundedSender<Event>,
}

impl Handle {
    pub fn fetch(&self, ids: Vec<MessageId>) -> (Job, UnboundedReceiver<Event>) {
        let (send, events) = unbounded();
        let cancel = CancelBodyFetch::default();
        let _ = self.0.send(Request {
            ids,
            cancel: cancel.clone(),
            events: send,
        });
        (Job(cancel), events)
    }
}

pub fn start(config: AccountConfig, path: PathBuf) -> Handle {
    let (send, requests) = mpsc::channel::<Request>();
    std::thread::Builder::new()
        .name("fell-reader-body".into())
        .spawn(move || {
            let mut store = None;
            let mut fetcher = ReaderBodyFetcher::default();
            for request in requests {
                if request.cancel.is_cancelled() || request.events.is_closed() {
                    continue;
                }
                let result = diagnostics::catch_worker(|| {
                    if store.is_none() {
                        store = Some(ReaderBodyFetcher::open_store(&path)?);
                    }
                    fetcher.fetch(
                        &config,
                        store.as_ref().expect("opened store"),
                        &request.ids,
                        &request.cancel,
                        || {
                            let _ = request.events.unbounded_send(Event::Stored);
                        },
                    )
                });
                if request.cancel.is_cancelled() {
                    continue;
                }
                let result = match result {
                    Ok(result) => result.map_err(|err| {
                        diagnostic!(
                            DEBUG,
                            "Reader body fetch failed",
                            error = &err as &dyn std::error::Error
                        );
                        match classify_sync_error(&err) {
                            SyncFailureKind::Unreachable => Failure::Offline,
                            _ => Failure::Failed,
                        }
                    }),
                    Err(_) => {
                        store = None;
                        fetcher = ReaderBodyFetcher::default();
                        Err(Failure::Failed)
                    }
                };
                let _ = request.events.unbounded_send(Event::Finished(result));
            }
        })
        .expect("spawn reader body worker");
    Handle(send)
}
