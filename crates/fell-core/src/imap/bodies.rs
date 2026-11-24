//! Fetch the bodies of copies already stored, to fill parts kept by later code.
//!
//! Mail synced before body parts were stored has only `body_text`. This reads
//! those copies again by UID with `BODY.PEEK[]`, so nothing is marked read.

use std::collections::BTreeMap;
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};

use crate::Error;
use crate::backend::FetchedBody;
use crate::message::RemoteId;

use super::{BodyStructure, ImapConfig, parse_remote_id, session, unescape_mailbox};

/// UIDs per `UID FETCH`. Bytes past the cap in one reply are read and
/// dropped, so this also bounds what a full batch fetches for nothing.
const UIDS_PER_FETCH: usize = 32;

/// Bytes of message source one call holds, like sync's batch. Once reached,
/// no further `UID FETCH` is sent and the copies not fetched are left out.
fn byte_cap() -> usize {
    #[cfg(test)]
    {
        TEST_BYTE_CAP.with(|cap| cap.get())
    }
    #[cfg(not(test))]
    {
        super::BATCH_BYTES
    }
}

#[cfg(test)]
thread_local! {
    static TEST_BYTE_CAP: std::cell::Cell<usize> = const { std::cell::Cell::new(super::BATCH_BYTES) };
}

#[cfg(test)]
pub(super) struct ByteCapGuard;

#[cfg(test)]
pub(super) fn limit_body_bytes(bytes: usize) -> ByteCapGuard {
    TEST_BYTE_CAP.with(|cell| cell.set(bytes));
    ByteCapGuard
}

#[cfg(test)]
impl Drop for ByteCapGuard {
    fn drop(&mut self) {
        TEST_BYTE_CAP.with(|cell| cell.set(super::BATCH_BYTES));
    }
}

/// A reader can close the socket of an abandoned request even if its
/// worker is blocked waiting for the server. An unstarted request never
/// connects. The next request reconnects if the previous socket was closed.
#[derive(Clone, Default)]
pub struct CancelBodyFetch(Arc<Mutex<CancelState>>);

#[derive(Default)]
struct CancelState {
    cancelled: bool,
    socket: Option<TcpStream>,
}

impl CancelBodyFetch {
    pub fn cancel(&self) {
        let mut state = self.0.lock().unwrap_or_else(|err| err.into_inner());
        state.cancelled = true;
        if let Some(socket) = state.socket.take() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .cancelled
    }

    fn attach(&self, socket: &TcpStream) -> Result<(), Error> {
        let mut state = self.0.lock().unwrap_or_else(|err| err.into_inner());
        if state.cancelled {
            return Err(cancelled());
        }
        state.socket = Some(socket.try_clone()?);
        Ok(())
    }

    /// Release the socket on success without closing a session we can reuse.
    fn detach(&self) -> Result<(), Error> {
        let mut state = self.0.lock().unwrap_or_else(|err| err.into_inner());
        state.socket = None;
        if state.cancelled {
            Err(cancelled())
        } else {
            Ok(())
        }
    }
}

pub(crate) fn cancelled() -> Error {
    Error::Backend("message fetch cancelled".into())
}

#[derive(Default)]
pub(crate) struct BodyConnection(Option<SelectedSession>);

struct SelectedSession {
    session: session::Session,
    selected: Option<(String, u32)>,
}

impl SelectedSession {
    fn examine(&mut self, mailbox: &str, validity: u32) -> Result<bool, Error> {
        if self
            .selected
            .as_ref()
            .is_some_and(|(name, id)| name == mailbox && *id == validity)
        {
            return Ok(true);
        }
        self.selected = self
            .session
            .examine(mailbox)?
            .map(|examined| (mailbox.to_owned(), examined.uidvalidity));
        Ok(self
            .selected
            .as_ref()
            .is_some_and(|(_, id)| *id == validity))
    }
}

impl BodyConnection {
    pub(crate) fn logout(&mut self) {
        if let Some(mut selected) = self.0.take() {
            selected.session.logout();
        }
    }

    fn run<T>(
        &mut self,
        config: impl Fn() -> Result<ImapConfig, Error>,
        cancel: &CancelBodyFetch,
        mut run: impl FnMut(&mut SelectedSession) -> Result<T, Error>,
    ) -> Result<T, Error> {
        if cancel.is_cancelled() {
            return Err(cancelled());
        }
        if let Some(mut warm) = self.0.take() {
            let result = warm
                .session
                .tcp()
                .and_then(|socket| cancel.attach(socket))
                .and_then(|()| run(&mut warm));
            cancel.detach()?;
            if result.is_ok() {
                self.0 = Some(warm);
                return result;
            }
            // The peer may have closed an idle connection. Retry once on a
            // fresh connection, since these commands are read-only.
        }
        let mut fresh = SelectedSession {
            session: match session::Session::open_with(&config()?, |socket| cancel.attach(socket)) {
                Ok(session) => session,
                Err(err) => {
                    cancel.detach()?;
                    return Err(err);
                }
            },
            selected: None,
        };
        let result = run(&mut fresh);
        cancel.detach()?;
        if result.is_ok() {
            self.0 = Some(fresh);
        }
        result
    }

    pub(crate) fn structures(
        &mut self,
        config: impl Fn() -> Result<ImapConfig, Error>,
        cancel: &CancelBodyFetch,
        wanted: &[RemoteId],
    ) -> Result<Vec<(RemoteId, BodyStructure)>, Error> {
        let (by_mailbox, _) = group(wanted);
        if by_mailbox.is_empty() {
            return Ok(Vec::new());
        }
        self.run(config, cancel, |session| {
            let mut found = Vec::new();
            for ((mailbox, validity), copies) in &by_mailbox {
                if !session.examine(mailbox, *validity)? {
                    continue;
                }
                for chunk in copies.chunks(UIDS_PER_FETCH) {
                    let set = uid_set(chunk);
                    for (uid, structure) in session.session.uid_structures(&set)? {
                        if let Some((_, id)) = chunk.iter().find(|(copy, _)| *copy == uid) {
                            found.push((id.clone(), structure));
                        }
                    }
                }
            }
            Ok(found)
        })
    }

    pub(crate) fn bodies(
        &mut self,
        config: impl Fn() -> Result<ImapConfig, Error>,
        cancel: &CancelBodyFetch,
        wanted: &[RemoteId],
    ) -> Result<Vec<FetchedBody>, Error> {
        let (by_mailbox, invalid) = group(wanted);
        if by_mailbox.is_empty() {
            return Ok(invalid);
        }
        self.run(config, cancel, |session| {
            read_bodies(session, &by_mailbox, invalid.clone())
        })
    }
}

type MailboxCopies = BTreeMap<(String, u32), Vec<(u32, RemoteId)>>;

fn group(wanted: &[RemoteId]) -> (MailboxCopies, Vec<FetchedBody>) {
    let mut by_mailbox = MailboxCopies::new();
    let mut invalid = Vec::new();
    for remote_id in wanted {
        let named = parse_remote_id(&remote_id.0)
            .and_then(|copy| Some((unescape_mailbox(&copy.mailbox)?, copy.uidvalidity, copy.uid)));
        let Some((raw_name, uidvalidity, uid)) = named else {
            invalid.push(FetchedBody {
                remote_id: remote_id.clone(),
                raw: None,
            });
            continue;
        };
        by_mailbox
            .entry((raw_name, uidvalidity))
            .or_default()
            .push((uid, remote_id.clone()));
    }
    for copies in by_mailbox.values_mut() {
        copies.sort_by_key(|(uid, _)| *uid);
        copies.dedup_by_key(|(uid, _)| *uid);
    }
    (by_mailbox, invalid)
}

fn uid_set(chunk: &[(u32, RemoteId)]) -> String {
    chunk
        .iter()
        .map(|(uid, _)| uid.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// Fetch copies again for backfill on a short-lived connection.
pub(crate) fn fetch_bodies(
    config: &ImapConfig,
    wanted: &[RemoteId],
) -> Result<Vec<FetchedBody>, Error> {
    let (by_mailbox, invalid) = group(wanted);
    if by_mailbox.is_empty() {
        return Ok(invalid);
    }
    let mut session = SelectedSession {
        session: session::Session::open(config)?,
        selected: None,
    };
    let result = read_bodies(&mut session, &by_mailbox, invalid);
    session.session.logout();
    result
}

/// A copy answered without its body (expunged, too large, a refused
/// mailbox, or changed UIDVALIDITY) comes back with no source. Capped
/// copies are left for the next request. An incomplete reply is an error.
fn read_bodies(
    session: &mut SelectedSession,
    by_mailbox: &MailboxCopies,
    mut out: Vec<FetchedBody>,
) -> Result<Vec<FetchedBody>, Error> {
    let cap = byte_cap();
    let mut held = 0usize;
    'mailboxes: for ((mailbox, uidvalidity), copies) in by_mailbox {
        if held >= cap {
            break;
        }
        if !session.examine(mailbox, *uidvalidity)? {
            out.extend(copies.iter().map(|(_, remote_id)| FetchedBody {
                remote_id: remote_id.clone(),
                raw: None,
            }));
            continue;
        }
        for chunk in copies.chunks(UIDS_PER_FETCH) {
            if held >= cap {
                break 'mailboxes;
            }
            let set = uid_set(chunk);
            let mut answered: BTreeMap<u32, Option<Vec<u8>>> = BTreeMap::new();
            // Answered past the cap: read off the wire, dropped, asked again.
            let mut deferred: Vec<u32> = Vec::new();
            let stop = session.session.uid_fetch_each(
                &set,
                None,
                session::FetchItems::Bodies,
                |item| {
                    if item.body_missing {
                        return Ok(());
                    }
                    let raw = (!item.skipped).then_some(item.body);
                    let size = raw.as_ref().map_or(0, Vec::len);
                    if size > 0 && held > 0 && held + size > cap {
                        deferred.push(item.uid);
                        return Ok(());
                    }
                    held += size;
                    answered.insert(item.uid, raw);
                    Ok(())
                },
            )?;
            if !matches!(stop, session::FetchStop::Done) {
                let missing = chunk
                    .iter()
                    .any(|(uid, _)| !answered.contains_key(uid) && !deferred.contains(uid));
                if missing {
                    let why = match stop {
                        session::FetchStop::Refused => "refused",
                        _ => "cut short",
                    };
                    return Err(Error::Backend(format!(
                        "imap: UID FETCH of stored copies in {mailbox} {why}"
                    )));
                }
            }
            for (uid, remote_id) in chunk {
                if deferred.contains(uid) {
                    continue;
                }
                out.push(FetchedBody {
                    remote_id: remote_id.clone(),
                    // Not in a finished reply: the server no longer has it.
                    raw: answered.remove(uid).flatten(),
                });
            }
        }
    }
    Ok(out)
}
