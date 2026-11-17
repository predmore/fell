//! The mailbox connection lives on its own thread. The window never touches
//! SQLite: it sends a closure here and awaits the answer, so a slow query, a
//! checkpoint, or a sync write can delay a result but never a frame.

use std::sync::mpsc;

use fell_core::Store;
use futures::channel::oneshot;

type Job = Box<dyn FnOnce(&Store) + Send>;

#[derive(Clone)]
pub struct StoreClient {
    jobs: mpsc::Sender<Job>,
}

impl StoreClient {
    /// Open the store on a new thread with `open`, which also returns
    /// whatever the window needs to know about the mailbox it opened.
    pub fn spawn<T: Send + 'static>(
        open: impl FnOnce() -> (Store, T) + Send + 'static,
    ) -> (Self, oneshot::Receiver<T>) {
        let (jobs, rx) = mpsc::channel::<Job>();
        let (ready_tx, ready) = oneshot::channel();
        std::thread::Builder::new()
            .name("fell-store".into())
            .spawn(move || {
                let (store, info) = open();
                let _ = ready_tx.send(info);
                // Jobs run in order. The loop ends when the window drops its client.
                while let Ok(job) = rx.recv() {
                    job(&store);
                }
            })
            .expect("spawn store thread");
        (Self { jobs }, ready)
    }

    /// Run `job` on the store thread. The receiver is canceled if the thread is gone.
    pub fn call<R: Send + 'static>(
        &self,
        job: impl FnOnce(&Store) -> R + Send + 'static,
    ) -> oneshot::Receiver<R> {
        let (tx, rx) = oneshot::channel();
        let _ = self.jobs.send(Box::new(move |store| {
            let _ = tx.send(job(store));
        }));
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
