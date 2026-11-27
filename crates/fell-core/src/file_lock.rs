use std::fs::{File, TryLockError};
use std::thread;
use std::time::{Duration, Instant};

const LOCK_WAIT: Duration = Duration::from_secs(1);
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Try for an exclusive lock, retrying contention for up to a second.
/// A child forked on another thread can hold a flock after the parent closes
/// its descriptor, until the child's copy is closed on exec.
pub fn try_lock_briefly(file: &File) -> Result<(), TryLockError> {
    retry_lock(file, thread::sleep)
}

fn retry_lock(file: &File, mut wait: impl FnMut(Duration)) -> Result<(), TryLockError> {
    let deadline = Instant::now() + LOCK_WAIT;
    loop {
        match file.try_lock() {
            Err(TryLockError::WouldBlock) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(TryLockError::WouldBlock);
                }
                wait(remaining.min(RETRY_INTERVAL));
            }
            result => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn retries_after_a_separately_opened_file_is_unlocked() {
        let holder = tempfile::NamedTempFile::new().unwrap();
        let contender = holder.reopen().unwrap();
        holder.as_file().lock().unwrap();
        let (release, contended) = mpsc::sync_channel(0);
        let (unlocked, released) = mpsc::sync_channel(0);
        thread::scope(|scope| {
            let file = holder.as_file();
            scope.spawn(move || {
                contended.recv().unwrap();
                file.unlock().unwrap();
                unlocked.send(()).unwrap();
            });
            let result = retry_lock(&contender, |delay| {
                thread::sleep(delay);
                release.send(()).unwrap();
                released.recv().unwrap();
            });
            drop(release);
            result.unwrap();
        });
    }

    #[test]
    fn returns_would_block_while_a_separately_opened_file_stays_locked() {
        let holder = tempfile::NamedTempFile::new().unwrap();
        let contender = holder.reopen().unwrap();
        holder.as_file().lock().unwrap();
        let (done, result) = mpsc::channel();
        thread::scope(|scope| {
            scope.spawn(move || done.send(try_lock_briefly(&contender)).unwrap());
            let outcome = result.recv_timeout(LOCK_WAIT + Duration::from_secs(2));
            holder.as_file().unlock().unwrap();
            assert!(matches!(outcome.unwrap(), Err(TryLockError::WouldBlock)));
        });
    }
}
