//! Local socket the editor serves so `fell-mcp` reads the draft on screen.
//!
//! The socket is mode 0600 under the user's runtime directory. It is not a
//! network listener, and no request on it transmits mail.

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::time::Duration;

use serde_json::{Value, json};

use super::persist::SEND_IN_PROGRESS;
use super::session::{DraftSession, SendLock};
use super::workspace::{self, draft_socket_path};
use crate::AccountId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HubEvent;

/// The draft socket and the lock that makes this process its only server.
pub struct HubListener {
    listener: UnixListener,
    _lock: std::fs::File,
}

impl HubListener {
    pub fn incoming(&self) -> std::os::unix::net::Incoming<'_> {
        self.listener.incoming()
    }
}

/// Called on every hub change. Returns `false` once its subscriber is gone,
/// and is then dropped.
type Listener = Box<dyn Fn() -> bool + Send>;

/// The inbox notice for a draft that may have gone out, worded as its row
/// and preview are.
pub const MAYBE_SENT_NOTICE: &str = "may have been sent  ·  check Sent";
pub const LOCAL_SENT_COPY_FAILED: &str = "Sent · local copy could not be saved";

/// `text` with its first letter upper case.
fn sentence_case(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

use super::SERVER_DRAFT_SAVE_FAILED;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosedDraftSave {
    account: crate::AccountId,
    draft: String,
    digest: [u8; 32],
}

impl ClosedDraftSave {
    pub fn status(&self, store: &crate::Store) -> Option<super::DraftSaveStatus> {
        store
            .server_draft_save_status(&self.account, &self.draft, &self.digest)
            .ok()
            .flatten()
    }
}

/// A notice whose result can be checked on the mailbox worker after close.
pub struct DraftCloseNotice {
    account: crate::AccountId,
    text: String,
    alert: bool,
    server_save: Option<ClosedDraftSave>,
    pending_write: Option<(crate::AccountId, String, u64, super::SaveWaiter)>,
}

impl DraftCloseNotice {
    pub fn after_local_write(mut self) -> Self {
        if let Some((account, draft, revision, waiter)) = self.pending_write.take() {
            match waiter.wait_for_receipt(revision) {
                Some(super::SaveReceipt::Server(digest)) => {
                    self.server_save = Some(ClosedDraftSave {
                        account,
                        draft,
                        digest,
                    });
                }
                Some(super::SaveReceipt::ServerFailed) => {
                    self.text = SERVER_DRAFT_SAVE_FAILED.into();
                    self.alert = true;
                }
                _ => {}
            }
        }
        self
    }

    pub fn needs_local_write(&self) -> bool {
        self.pending_write.is_some()
    }

    pub fn server_save(&self) -> Option<&ClosedDraftSave> {
        self.server_save.as_ref()
    }

    pub fn resolve(self, store: &crate::Store) -> (String, bool) {
        if let Some(warning) = self
            .server_save
            .as_ref()
            .and_then(|save| save.status(store))
            .and_then(super::DraftSaveStatus::warning)
        {
            return (warning.into(), true);
        }
        self.into_line()
    }

    pub fn into_line(self) -> (String, bool) {
        (self.text, self.alert)
    }
}

pub struct DraftHub {
    server_draft_wake: Mutex<HashMap<crate::AccountId, super::server::Wake>>,
    sessions: Mutex<HashMap<String, DraftSession>>,
    listeners: Mutex<Vec<Listener>>,
    focused: Mutex<Option<String>>,
    shown: Mutex<Option<String>>,
    shown_cv: Condvar,
    /// Bumped by every `request_send`, so a host can bring the editor up
    /// again for a draft that is already focused.
    reveals: AtomicU64,
    /// One line per review window that closed, for the inbox to show.
    /// Closing notices for the inbox, each with whether it is a warning.
    closed: Mutex<Vec<DraftCloseNotice>>,
    /// The draft whose review window closed last, for the inbox to reopen.
    last_closed: Mutex<Option<String>>,
    /// Drafts being read from disk to go live, so two opens of one draft
    /// never write its folder at the same time. See [`DraftHub::begin_open`].
    opening: Mutex<OpeningDrafts>,
    draft_opened: Condvar,
}

#[derive(Default)]
struct OpeningDrafts {
    by_id: HashMap<String, AccountId>,
    removing: HashSet<AccountId>,
}

/// Keeps new drafts for this account from going live during local deletion.
pub struct AccountDraftRemoval {
    hub: Arc<DraftHub>,
    account: AccountId,
}

impl Drop for AccountDraftRemoval {
    fn drop(&mut self) {
        self.hub
            .opening
            .lock()
            .expect("opening")
            .removing
            .remove(&self.account);
    }
}

/// Held while draft `id` is read from disk to go live. Dropping it lets the
/// next open of that draft start.
pub struct OpenGuard<'a> {
    hub: &'a DraftHub,
    id: String,
}

impl Drop for OpenGuard<'_> {
    fn drop(&mut self) {
        self.hub
            .opening
            .lock()
            .expect("opening")
            .by_id
            .remove(&self.id);
        self.hub.draft_opened.notify_all();
    }
}

/// `subject` quoted for an inbox line, after the outcome. Spaced like the
/// rest of the inbox status bar.
fn quoted_subject(subject: &str) -> String {
    if subject.trim().is_empty() {
        return String::new();
    }
    let short: String = subject.chars().take(60).collect();
    let more = if subject.chars().count() > 60 {
        "…"
    } else {
        ""
    };
    format!("  ·  \u{201c}{short}{more}\u{201d}")
}

impl Default for DraftHub {
    fn default() -> Self {
        Self::new()
    }
}

impl DraftHub {
    pub fn new() -> Self {
        Self {
            server_draft_wake: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            listeners: Mutex::new(Vec::new()),
            focused: Mutex::new(None),
            shown: Mutex::new(None),
            shown_cv: Condvar::new(),
            reveals: AtomicU64::new(0),
            closed: Mutex::new(Vec::new()),
            last_closed: Mutex::new(None),
            opening: Mutex::default(),
            draft_opened: Condvar::new(),
        }
    }

    pub fn subscribe(&self) -> Receiver<()> {
        let (tx, rx) = mpsc::channel::<()>();
        self.subscribe_with(move || tx.send(()).is_ok());
        rx
    }

    /// Run `notify` after every change, from whichever thread made it. Keep
    /// it cheap and non-blocking, such as a send on an async channel; return
    /// `false` when nobody listens any more.
    pub fn subscribe_with(&self, notify: impl Fn() -> bool + Send + 'static) {
        self.listeners
            .lock()
            .expect("listeners")
            .push(Box::new(notify));
    }

    /// Add a live draft. Its saves run on a writer thread from now on, so
    /// holding the session lock never waits for disk.
    /// Replacing a session keeps its send lock, so reopening a draft that
    /// was sent, or is being sent, does not make it sendable again.
    pub fn insert(&self, mut session: DraftSession) {
        session.attach_writer();
        let id = session.id().to_string();
        let mut sessions = self.sessions.lock().expect("sessions");
        if let Some(previous) = sessions.get(&id) {
            session.inherit_send_lock(previous);
        }
        sessions.insert(id, session);
        drop(sessions);
        self.emit();
    }

    /// Start reading draft `id` from disk to make it live. `None` when it is
    /// live already, or another open of it is running: that one shows it,
    /// and this one must not touch its folder. Checked under the sessions
    /// lock, so a draft that goes live meanwhile is seen.
    pub fn begin_open(
        &self,
        id: &str,
        account: &AccountId,
    ) -> Result<Option<OpenGuard<'_>>, String> {
        let sessions = self.sessions.lock().expect("sessions");
        let mut opening = self.opening.lock().expect("opening");
        if opening.removing.contains(account) {
            return Err("Local mail for this account is being deleted.".into());
        }
        if sessions.contains_key(id) || opening.by_id.contains_key(id) {
            return Ok(None);
        }
        opening.by_id.insert(id.to_string(), account.clone());
        Ok(Some(OpenGuard {
            hub: self,
            id: id.to_string(),
        }))
    }

    /// Add a live draft unless one with its id is already live, which then
    /// stays as it is, unsaved typing included. Returns whether it was added.
    pub fn insert_new(&self, mut session: DraftSession) -> bool {
        let id = session.id().to_string();
        let mut sessions = self.sessions.lock().expect("sessions");
        if sessions.contains_key(&id) {
            return false;
        }
        session.attach_writer();
        sessions.insert(id, session);
        drop(sessions);
        self.emit();
        true
    }

    /// Bring the editor up on live draft `id`, as the person asked from the
    /// inbox. Unlike `request_send`, no agent is named. `false` when the
    /// draft is not live.
    pub fn show(&self, id: &str) -> bool {
        let mut sessions = self.sessions.lock().expect("sessions");
        let Some(session) = sessions.get_mut(id) else {
            return false;
        };
        session.open_for_edit();
        session.server_save.allow();
        *self.focused.lock().expect("focused") = Some(id.to_string());
        self.reveals.fetch_add(1, Ordering::SeqCst);
        drop(sessions);
        self.emit();
        true
    }

    /// Discard from the editor, using the mailbox this draft was opened from.
    pub fn discard_open(&self, id: &str) -> crate::Result<()> {
        let Some((Some(path), account)) = self.with_session(id, |session| {
            (session.mailbox_path(), session.account_id().clone())
        }) else {
            return Err(crate::Error::Action("Draft is no longer available".into()));
        };
        if !path.is_file() {
            return Err(crate::Error::Action(
                "Draft mailbox is no longer available".into(),
            ));
        }
        let store = crate::Store::open(&path)?;
        self.discard(&store, &account, &[id.to_string()])
    }

    /// Discard the selection as one local undo action. Stop live sessions
    /// from writing or reopening it, and leave the workspace available for
    /// undo along with the saved draft and its dependents in the mailbox.
    pub fn discard(
        &self,
        store: &crate::Store,
        account: &crate::AccountId,
        ids: &[String],
    ) -> crate::Result<()> {
        // Reserve the drafts before waiting for their saves. A send in this
        // hub or a second app must finish before discard can proceed.
        let _guards = ids
            .iter()
            .map(|id| store.lock_compose_send(id))
            .collect::<crate::Result<Vec<_>>>()?;
        let changed = || crate::Error::Action("Draft changed while discarding · try again".into());
        let mut saves = Vec::new();
        {
            let mut sessions = self.sessions.lock().expect("sessions");
            let opening = self.opening.lock().expect("opening");
            for id in ids {
                if opening.by_id.contains_key(id) {
                    return Err(changed());
                }
                if let Some(session) = sessions.get_mut(id) {
                    if session.account_id() != account {
                        return Err(crate::Error::Action(
                            "Draft belongs to another account".into(),
                        ));
                    }
                    match session.send_lock() {
                        Some(SendLock::InFlight) => {
                            return Err(crate::Error::Action(SEND_IN_PROGRESS.into()));
                        }
                        Some(SendLock::Sent) => {
                            return Err(crate::Error::Action(sentence_case(
                                SendLock::Sent.reason(),
                            )));
                        }
                        _ => {}
                    }
                    let waiter = session.save_for_discard().map_err(crate::Error::Action)?;
                    saves.push((id, session.paint_generation(), waiter));
                }
            }
        }
        let mut generations = HashMap::new();
        for (id, generation, waiter) in saves {
            if let Some(waiter) = waiter {
                waiter.wait_for_mailbox().map_err(|err| {
                    crate::Error::Action(format!("Could not save draft for undo · {err}"))
                })?;
            }
            generations.insert(id, generation);
        }
        let mut sessions = self.sessions.lock().expect("sessions");
        let opening = self.opening.lock().expect("opening");
        for id in ids {
            if opening.by_id.contains_key(id)
                || sessions.get(id).map(DraftSession::paint_generation)
                    != generations.get(id).copied()
            {
                return Err(changed());
            }
        }
        let action = store.discard_compose_drafts(account, ids)?;
        for id in ids {
            sessions.remove(id);
        }
        for slot in [&self.focused, &self.shown, &self.last_closed] {
            let mut slot = slot.lock().expect("draft window");
            if slot.as_ref().is_some_and(|id| ids.contains(id)) {
                *slot = None;
            }
        }
        drop(opening);
        drop(sessions);
        self.wake_server_draft(account);
        self.closed.lock().expect("closed").push(DraftCloseNotice {
            account: account.clone(),
            text: format!("{}  ·  u", action.summary),
            alert: false,
            server_save: None,
            pending_write: None,
        });
        self.emit();
        Ok(())
    }

    pub fn with_session<T>(
        &self,
        id: &str,
        body: impl FnOnce(&mut DraftSession) -> T,
    ) -> Option<T> {
        let mut sessions = self.sessions.lock().expect("sessions");
        let session = sessions.get_mut(id)?;
        let before = (session.paint_generation(), session.send_lock());
        let value = body(session);
        let changed = (session.paint_generation(), session.send_lock()) != before;
        drop(sessions);
        if changed {
            self.emit();
        }
        Some(value)
    }

    pub fn confirm_synced_sends(&self, account: &crate::AccountId, ids: &[String]) {
        let mut sessions = self.sessions.lock().expect("sessions");
        let mut closed = Vec::new();
        for id in ids {
            if let Some(session) = sessions.get_mut(id)
                && session.account_id() == account
                && session.send_lock() == Some(SendLock::MaybeSent)
            {
                session.confirm_uncertain_send();
                if !session.is_open() {
                    closed.push((id, quoted_subject(&session.fields().subject)));
                }
            }
        }
        drop(sessions);
        for (id, subject) in closed {
            self.push_closed(id, format!("Confirmed in Sent{subject}"), true, false);
        }
        // The Drafts list must reread the mailbox even if none of these
        // drafts are live in the hub.
        self.emit();
    }

    /// The GUI's sync worker wakes once an editor save reaches the outbox.
    pub fn on_server_draft_queued(
        &self,
        account: crate::AccountId,
        wake: impl Fn() + Send + Sync + 'static,
    ) {
        self.server_draft_wake
            .lock()
            .expect("server draft wake")
            .insert(account, Arc::new(wake));
    }

    pub fn remove_server_draft_wake(&self, account: &crate::AccountId) {
        self.server_draft_wake
            .lock()
            .expect("server draft wake")
            .remove(account);
    }

    pub fn wake_server_draft(&self, account: &crate::AccountId) {
        if let Some(wake) = self.server_draft_wake(account) {
            wake();
        }
    }

    fn server_draft_wake(&self, account: &crate::AccountId) -> Option<super::server::Wake> {
        self.server_draft_wake
            .lock()
            .expect("server draft wake")
            .get(account)
            .cloned()
    }

    /// Called only by the compose window. Agent request_send can reveal a
    /// window but does not grant permission to publish that draft.
    pub fn poll_server_draft(&self, id: &str, now_ms: u64) {
        if self.shown.lock().expect("shown").as_deref() != Some(id) {
            return;
        }
        self.with_session(id, |session| {
            if let Some(wake) = self.server_draft_wake(session.account_id()) {
                session.flush_server_if_due(Some(now_ms), &wake);
            }
        });
    }

    /// Queue every save still waiting on the editor's debounce, then wait
    /// until each live draft's saves are on disk. The app calls this when it
    /// quits.
    pub fn settle_all(&self) {
        let shown = self.shown.lock().expect("shown").clone();
        let waiters: Vec<super::SaveWaiter> = self
            .sessions
            .lock()
            .expect("sessions")
            .values_mut()
            .filter_map(|session| {
                if shown.as_deref() == Some(session.id())
                    && let Some(wake) = self.server_draft_wake(session.account_id())
                {
                    session.flush_server_if_due(None, &wake);
                }
                let _ = session.flush_pending();
                session.save_waiter()
            })
            .collect();
        for waiter in waiters {
            let _ = waiter.wait();
        }
    }

    /// The compose window calls this after it is showing `id`.
    pub fn note_shown(&self, id: &str) {
        let mut shown = self.shown.lock().expect("shown");
        if shown.as_deref() == Some(id) {
            return;
        }
        *shown = Some(id.to_string());
        self.shown_cv.notify_all();
    }

    /// The compose window showing `id` closed. A later `request_send` waits
    /// for a window to show it again before it reports the draft opened.
    pub fn note_hidden(&self, id: &str) {
        let was_shown = {
            let mut shown = self.shown.lock().expect("shown");
            let was = shown.as_deref() == Some(id);
            if was {
                *shown = None;
            }
            was
        };
        // No window has it open for editing any more. Typing still waiting
        // on the save debounce is saved now.
        let summary = self.with_session(id, |session| {
            let wake = was_shown
                .then(|| self.server_draft_wake(session.account_id()))
                .flatten();
            if let Some(wake) = &wake {
                session.flush_server_if_due(None, wake);
            }
            let _ = session.flush_pending();
            let receipt = session.save_receipt();
            let pending_write = (wake.is_some()
                && matches!(receipt, None | Some(super::SaveReceipt::Pending)))
            .then(|| session.save_waiter())
            .flatten()
            .map(|waiter| (session.revision(), waiter));
            session.close_editor();
            let snap = session.snapshot();
            (
                snap.send_lock_reason(),
                snap.unreviewed,
                snap.send_requested_by.is_some(),
                snap.subject.clone(),
                receipt,
                pending_write,
                session.account_id().clone(),
            )
        });
        if was_shown
            && let Some((locked, unreviewed, asked, subject, receipt, pending_write, account)) =
                summary
        {
            let sent = locked == Some(SendLock::Sent.reason());
            let failed = locked.is_none() && receipt == Some(super::SaveReceipt::ServerFailed);
            let line = match locked {
                None if failed => SERVER_DRAFT_SAVE_FAILED.into(),
                // What happened, as docs/ux.md has it: "Sent" and the subject.
                Some(_) if sent => format!("Sent{}", quoted_subject(&subject)),
                // The key to what the warning asks: look in Sent.
                Some(reason) if reason == SendLock::MaybeSent.reason() => format!(
                    "{MAYBE_SENT_NOTICE}{}  ·  gs sent",
                    quoted_subject(&subject)
                ),
                Some(reason) => format!("{reason}{}", quoted_subject(&subject)),
                None if asked && unreviewed > 0 => format!(
                    "not sent{}  ·  {unreviewed} unreviewed  ·  space a r reopen",
                    quoted_subject(&subject)
                ),
                None if asked => {
                    format!("not sent{}  ·  space a r reopen", quoted_subject(&subject))
                }
                // The person's own draft: kept, and in the drafts list.
                None => format!("draft saved{}  ·  gd drafts", quoted_subject(&subject)),
            };
            let server_save = match receipt {
                Some(super::SaveReceipt::Server(digest)) if locked.is_none() => {
                    Some(ClosedDraftSave {
                        account: account.clone(),
                        draft: id.to_owned(),
                        digest,
                    })
                }
                _ => None,
            };
            self.push_notice(
                id,
                sent,
                DraftCloseNotice {
                    account: account.clone(),
                    text: line,
                    alert: failed || locked == Some(SendLock::MaybeSent.reason()),
                    server_save,
                    pending_write: pending_write
                        .filter(|_| locked.is_none())
                        .map(|(revision, waiter)| (account, id.to_owned(), revision, waiter)),
                },
            );
        }
    }

    /// Tell the inbox how a send of `id` ended, for a send that finished
    /// after the compose window moved on to another draft or closed: no
    /// window is left to say it. `failure` is what went wrong: a send that
    /// did not go out, may have, or went out but was not recorded.
    pub fn report_send(&self, id: &str, failure: Option<&str>) {
        // Any failure is a warning: mail that did not go out, may have, or
        // went out unrecorded.
        let warn_failure = failure.is_some();
        let Some((locked, subject)) = self.with_session(id, |session| {
            let snap = session.snapshot();
            (snap.send_lock_reason(), snap.subject.clone())
        }) else {
            return;
        };
        let name = quoted_subject(&subject);
        let sent = locked == Some(SendLock::Sent.reason());
        let line = match (locked, failure) {
            (_, Some(failure)) if failure == MAYBE_SENT_NOTICE => {
                format!("{failure}{name}  ·  gs sent")
            }
            (_, Some(failure)) => format!("{failure}{name}  ·  gd drafts"),
            (Some(_), None) if sent => format!("Sent{name}"),
            (Some(reason), None) if reason == SendLock::MaybeSent.reason() => {
                format!("{MAYBE_SENT_NOTICE}{name}  ·  gs sent")
            }
            (Some(reason), None) => format!("{reason}{name}"),
            (None, None) => format!("not sent{name}  ·  gd drafts"),
        };
        let warn = warn_failure || locked == Some(SendLock::MaybeSent.reason());
        self.push_closed(id, line, sent, warn);
    }

    /// The SMTP send succeeded but Fell could not save its local copy.
    pub fn report_sent_copy_failure(&self, id: &str) {
        self.push_closed(id, LOCAL_SENT_COPY_FAILED.into(), true, true);
    }

    /// Queue `line` for the inbox. Mail that went out is not offered again
    /// by reopen; anything else is the draft reopen brings back.
    fn push_closed(&self, id: &str, line: String, sent: bool, warn: bool) {
        let Some(account) = self.with_session(id, |session| session.account_id().clone()) else {
            return;
        };
        self.push_notice(
            id,
            sent,
            DraftCloseNotice {
                account,
                text: line,
                alert: warn,
                server_save: None,
                pending_write: None,
            },
        );
    }

    fn push_notice(&self, id: &str, sent: bool, mut notice: DraftCloseNotice) {
        {
            let mut last = self.last_closed.lock().expect("last closed");
            if !sent {
                *last = Some(id.to_string());
            } else if last.as_deref() == Some(id) {
                *last = None;
            }
        }
        // Inbox notices start with a capital, as "Sent “Lunch”" does.
        notice.text = sentence_case(&notice.text);
        self.closed.lock().expect("closed").push(notice);
        self.emit();
    }

    /// Show the review window that closed last again, as a `request_send`
    /// would. `false` when there is none, its draft is gone, or it was sent.
    pub fn reopen_last(&self) -> bool {
        let Some(id) = self.last_closed.lock().expect("last closed").clone() else {
            return false;
        };
        let mut sessions = self.sessions.lock().expect("sessions");
        let Some(session) = sessions.get_mut(&id) else {
            return false;
        };
        if session.send_lock() == Some(SendLock::Sent) {
            // Sent since it closed: nothing is left to reopen.
            let mut last = self.last_closed.lock().expect("last closed");
            if last.as_deref() == Some(id.as_str()) {
                *last = None;
            }
            return false;
        }
        session.request_send();
        session.server_save.allow();
        *self.focused.lock().expect("focused") = Some(id);
        self.reveals.fetch_add(1, Ordering::SeqCst);
        drop(sessions);
        self.emit();
        true
    }

    /// Lines about review windows that closed since the last call.
    pub fn take_closed(&self) -> Vec<String> {
        self.take_notices()
            .into_iter()
            .map(|(line, _)| line)
            .collect()
    }

    /// The closing notices since the last take, each with whether it is a
    /// warning the inbox shows in its error colour.
    pub fn take_notices(&self) -> Vec<(String, bool)> {
        self.take_close_notices()
            .into_iter()
            .map(DraftCloseNotice::into_line)
            .collect()
    }

    pub fn take_close_notices(&self) -> Vec<DraftCloseNotice> {
        std::mem::take(&mut *self.closed.lock().expect("closed"))
    }

    pub fn take_close_notices_for(&self, account: &crate::AccountId) -> Vec<DraftCloseNotice> {
        self.closed
            .lock()
            .expect("closed")
            .extract_if(.., |notice| &notice.account == account)
            .collect()
    }

    fn wait_shown(&self, id: &str) -> bool {
        {
            let shown = self.shown.lock().expect("shown");
            if shown.as_deref() == Some(id) {
                return true;
            }
        }
        if self.listeners.lock().expect("listeners").is_empty() {
            return false;
        }
        let shown = self.shown.lock().expect("shown");
        if shown.as_deref() == Some(id) {
            return true;
        }
        let (shown, _) = self
            .shown_cv
            .wait_timeout(shown, Duration::from_millis(500))
            .expect("shown");
        shown.as_deref() == Some(id)
    }

    pub fn snapshot_ids(&self) -> Vec<String> {
        self.sessions
            .lock()
            .expect("sessions")
            .keys()
            .cloned()
            .collect()
    }

    /// Explicit account deletion closes its draft writers before the mailbox
    /// and workspaces are moved. An in-flight send must finish first.
    /// Wait on a background thread so a draft opening for this account can finish.
    pub fn release_account_drafts(
        self: &Arc<Self>,
        account: &AccountId,
    ) -> Result<AccountDraftRemoval, String> {
        {
            let mut opening = self.opening.lock().expect("opening");
            if !opening.removing.insert(account.clone()) {
                return Err("Local mail for this account is already being deleted.".into());
            }
        }
        let removal = AccountDraftRemoval {
            hub: self.clone(),
            account: account.clone(),
        };
        drop(
            self.draft_opened
                .wait_while(self.opening.lock().expect("opening"), |opening| {
                    opening.by_id.values().any(|owner| owner == account)
                })
                .expect("opening"),
        );
        let mut sessions = self.sessions.lock().expect("sessions");
        let mut ids = Vec::new();
        for (id, session) in sessions.iter() {
            if session.account_id() == account {
                if session.send_lock() == Some(SendLock::InFlight) {
                    return Err(SEND_IN_PROGRESS.into());
                }
                ids.push(id.clone());
            }
        }
        let released: Vec<_> = ids.iter().filter_map(|id| sessions.remove(id)).collect();
        for slot in [&self.focused, &self.shown, &self.last_closed] {
            let mut slot = slot.lock().expect("draft window");
            if slot.as_ref().is_some_and(|id| ids.contains(id)) {
                *slot = None;
            }
        }
        drop(sessions);
        drop(released);
        self.emit();
        Ok(removal)
    }

    /// How many `request_send` calls have asked for the editor. A host opens
    /// or raises the compose window when this changes.
    pub fn reveal_count(&self) -> u64 {
        self.reveals.load(Ordering::SeqCst)
    }

    pub fn focused_id(&self) -> Option<String> {
        self.focused.lock().expect("focused").clone()
    }

    fn open_agent_draft(&self, draft_id: &str, agent: &str) -> Result<(), String> {
        if self
            .opening
            .lock()
            .expect("opening")
            .by_id
            .contains_key(draft_id)
        {
            return Err("the draft is opening in Fell; try again".into());
        }
        let account = super::persist::stored_compose_account(draft_id)
            .map(AccountId)
            .ok_or("draft has no mailbox record")?;
        let Some(_opening) = self.begin_open(draft_id, &account)? else {
            return if self
                .sessions
                .lock()
                .expect("sessions")
                .contains_key(draft_id)
            {
                Ok(())
            } else {
                Err("the draft is opening in Fell; try again".into())
            };
        };
        match super::persist::stored_compose_owner(draft_id).as_deref() {
            Some(owner) if owner == agent => {}
            Some(_) => return Err("this draft belongs to another agent".into()),
            None => return Err("draft has no mailbox record".into()),
        }
        let folder = super::persist::ensure_draft_folder(draft_id)?;
        if workspace::draft_file_is_symlink(&folder) || !folder.join("draft.md").is_file() {
            return Err("draft has no mailbox record".into());
        }
        let mut session = DraftSession::open_folder(&folder, account)?;
        // As in insert_new, keep any live session that was inserted while opening.
        session.attach_writer();
        self.sessions
            .lock()
            .expect("sessions")
            .entry(draft_id.into())
            .or_insert(session);
        Ok(())
    }

    pub fn handle_line(&self, line: &str) -> String {
        let request: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => {
                return json!({
                    "ok": false,
                    "transmitted": false,
                    "error": "draft request is not JSON"
                })
                .to_string();
            }
        };
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let op = request.get("op").and_then(Value::as_str).unwrap_or("");
        let draft_id = request
            .get("draft_id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let agent = request
            .get("agent")
            .and_then(Value::as_str)
            .unwrap_or("agent")
            .to_string();
        let mut sessions = self.sessions.lock().expect("sessions");
        if !draft_id.is_empty() && !sessions.contains_key(&draft_id) {
            drop(sessions);
            if let Err(err) = self.open_agent_draft(&draft_id, &agent) {
                return denied(&id, &err);
            }
            sessions = self.sessions.lock().expect("sessions");
        }
        let Some(session) = sessions.get(&draft_id) else {
            return denied(&id, "unknown draft");
        };
        if matches!(
            op,
            "get_draft"
                | "get_compose_context"
                | "edit_draft"
                | "replace_draft_body"
                | "request_send"
        ) && !session.allows_agent(&agent)
        {
            return denied(&id, "this draft belongs to another agent");
        }
        let writes = matches!(op, "edit_draft" | "replace_draft_body" | "request_send");
        let store;
        let (access, mut sessions) = if writes {
            let path = session.mailbox_path();
            let account = session.account_id().clone();
            drop(sessions);
            let Some(path) = path.as_ref().filter(|path| path.is_file()) else {
                return denied(&id, "draft has no mailbox record");
            };
            store = match crate::Store::open(path) {
                Ok(store) => store,
                Err(err) => return denied(&id, &err.to_string()),
            };
            // The editor and discard can already hold sessions when they
            // write SQLite. Release the transaction before waiting for them,
            // and never wait for SQLite with the UI's session lock held.
            let (tx, sessions) = loop {
                let tx = match crate::store::begin_immediate(store.connection()) {
                    Ok(tx) => tx,
                    Err(err) => return denied(&id, &err.to_string()),
                };
                match self.sessions.try_lock() {
                    Ok(locked) => break (tx, locked),
                    Err(TryLockError::WouldBlock) => {
                        drop(tx);
                        drop(self.sessions.lock().expect("sessions"));
                    }
                    Err(TryLockError::Poisoned(err)) => panic!("sessions: {err}"),
                }
            };
            let Some(session) = sessions.get(&draft_id) else {
                return denied(&id, "unknown draft");
            };
            if session.account_id() != &account || session.mailbox_path().as_ref() != Some(path) {
                return denied(&id, "the draft mailbox changed; try again");
            }
            if !session.allows_agent(&agent) {
                return denied(&id, "this draft belongs to another agent");
            }
            if let Err(err) = crate::Store::check_agent_draft_access(&tx, &account, &agent) {
                return denied(&id, &err.to_string());
            }
            (Some(tx), sessions)
        } else {
            (None, sessions)
        };
        let session = sessions.get_mut(&draft_id).expect("checked live draft");
        let mut flush = false;
        let mut reveal = None;
        let mut reply = match op {
            "get_draft" => ok_draft(id, session, &agent),
            "get_compose_context" => {
                let changes = session.take_user_changes(&agent);
                json!({
                    "id": id,
                    "ok": true,
                    "transmitted": false,
                    "draft_id": session.id(),
                    "cwd": session.folder(),
                    "context": "Use fell-mcp for contacts, search, read_thread, folders, and labels. This folder has no thread copy.",
                    "in_reply_to": session.fields().in_reply_to,
                    "user_changes_since_your_last_read": changes,
                })
            }
            "edit_draft" => {
                let changes = session.take_user_changes(&agent);
                let base = request
                    .get("base_revision")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let edits = request
                    .get("edits")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut error = None;
                for edit in edits {
                    let old = edit.get("old").and_then(Value::as_str).unwrap_or("");
                    let new = edit.get("new").and_then(Value::as_str).unwrap_or("");
                    if let Err(err) = session.edit_as_agent(base, old, new) {
                        error = Some(err);
                        break;
                    }
                }
                match error {
                    Some(err) => json!({
                        "id": id,
                        "ok": false,
                        "transmitted": false,
                        "error": err,
                        "revision": session.revision(),
                        "draft_md": session.serialize(),
                        "user_changes_since_your_last_read": changes,
                    }),
                    None => {
                        flush = true;
                        ok_draft_with(id, session, &agent, changes)
                    }
                }
            }
            "replace_draft_body" => {
                let changes = session.take_user_changes(&agent);
                let body = request.get("body").and_then(Value::as_str).unwrap_or("");
                match session.replace_body(body) {
                    Ok(()) => {
                        flush = true;
                        ok_draft_with(id, session, &agent, changes)
                    }
                    Err(err) => json!({
                        "id": id,
                        "ok": false,
                        "transmitted": false,
                        "error": err,
                        "user_changes_since_your_last_read": changes,
                    }),
                }
            }
            "request_send" => {
                let changes = session.take_user_changes(&agent);
                let transmitted = session.request_send();
                session.note_send_request(&agent);
                *self.focused.lock().expect("focused") = Some(draft_id.clone());
                self.reveals.fetch_add(1, Ordering::SeqCst);
                reveal = Some(draft_id.clone());
                let snap = session.snapshot();
                json!({
                    "id": id,
                    "ok": true,
                    "transmitted": transmitted,
                    "opened": false,
                    "focused": false,
                    "draft_id": snap.id,
                    "revision": snap.revision,
                    "to": snap.to.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
                    "cc": snap.cc.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
                    "bcc": snap.bcc.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
                    "subject": snap.subject,
                    "body": snap.body,
                    "in_reply_to": snap.in_reply_to,
                    "attachments": snap.attachments.iter().map(|item| {
                        json!({"path": item.path, "name": item.name})
                    }).collect::<Vec<_>>(),
                    "unreviewed": snap.unreviewed,
                    "user_changes_since_your_last_read": changes,
                    "note": "Nothing was sent. Mail leaves only when the person presses send in the compose editor.",
                })
            }
            _ => json!({
                "id": id,
                "ok": false,
                "transmitted": false,
                "error": "unknown draft op"
            }),
        };
        drop(access);
        if flush {
            let _ = session.flush();
        }
        let saved = session.save_waiter();
        drop(sessions);
        self.emit();
        // An agent reads draft.md right after a tool returns, so the reply
        // waits for the save. The session lock is already released, so the
        // editor keeps painting meanwhile.
        if let Some(saved) = saved
            && let Err(err) = saved.wait()
            && let Some(object) = reply.as_object_mut()
        {
            // The edit is in the live draft and on screen; only the file
            // write failed, so the agent should not repeat the edit.
            object.insert("saved".into(), json!(false));
            object.insert("save_error".into(), json!(err));
        }
        if let Some(id) = reveal {
            let shown = self.wait_shown(&id);
            if let Some(object) = reply.as_object_mut() {
                object.insert("opened".into(), json!(shown));
                object.insert("focused".into(), json!(shown));
            }
        }
        reply.to_string()
    }

    /// Serve agent drafts at `path`. One Fell process owns the socket: a
    /// lock beside it is held for as long as the returned listener lives, and
    /// a second process gets an error instead of taking the path over. A
    /// socket file left by a process that exited is replaced.
    pub fn bind(path: &Path) -> std::io::Result<HubListener> {
        use fs4::fs_std::FileExt;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        std::fs::set_permissions(&lock_path, std::fs::Permissions::from_mode(0o600))?;
        if !lock.try_lock_exclusive()? {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AddrInUse,
                "another Fell is serving agent drafts",
            ));
        }
        if path.exists() {
            std::fs::remove_file(path)?;
        }
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        Ok(HubListener {
            listener,
            _lock: lock,
        })
    }

    pub fn serve_one(self: &Arc<Self>, stream: UnixStream) {
        let Ok(incoming) = stream.try_clone() else {
            return;
        };
        let mut reader = BufReader::new(incoming);
        let mut line = String::new();
        if reader.read_line(&mut line).is_err() {
            return;
        }
        let reply = self.handle_line(line.trim());
        let mut out = stream;
        let _ = writeln!(out, "{reply}");
    }

    fn emit(&self) {
        let mut listeners = self.listeners.lock().expect("listeners");
        listeners.retain(|listener| listener());
    }
}

fn denied(id: &Value, error: &str) -> String {
    json!({
        "id": id,
        "ok": false,
        "transmitted": false,
        "error": error
    })
    .to_string()
}

fn ok_draft(id: Value, session: &mut DraftSession, agent: &str) -> Value {
    let changes = session.take_user_changes(agent);
    ok_draft_with(id, session, agent, changes)
}

fn ok_draft_with(id: Value, session: &DraftSession, _agent: &str, changes: String) -> Value {
    let snap = session.snapshot();
    json!({
        "id": id,
        "ok": true,
        "transmitted": false,
        "draft_id": snap.id,
        "revision": snap.revision,
        "draft_md": snap.text,
        "to": snap.to.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
        "cc": snap.cc.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
        "bcc": snap.bcc.iter().map(|item| item.raw.clone()).collect::<Vec<_>>(),
        "subject": snap.subject,
        "body": snap.body,
        "in_reply_to": snap.in_reply_to,
        "unreviewed": snap.unreviewed,
        "user_changes_since_your_last_read": changes,
        "cwd": session.folder(),
    })
}

pub fn rpc_call(request: &Value) -> Result<Value, String> {
    let path = draft_socket_path();
    let mut stream = UnixStream::connect(&path).map_err(|err| {
        if err.kind() == std::io::ErrorKind::NotFound
            || err.kind() == std::io::ErrorKind::ConnectionRefused
        {
            "compose editor is not running".into()
        } else {
            err.to_string()
        }
    })?;
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(2)))
        .ok();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(2)))
        .ok();
    writeln!(stream, "{request}").map_err(|err| err.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| err.to_string())?;
    serde_json::from_str(line.trim()).map_err(|err| err.to_string())
}

fn open_draft_for_agent(
    draft_id: &str,
    agent: &str,
    account: &str,
) -> Result<std::path::PathBuf, String> {
    let folder = workspace::draft_folder(draft_id)?;
    if workspace::draft_file_is_symlink(&folder) {
        return Err("draft.md is a symlink".into());
    }
    if let Some(owner) = super::persist::stored_compose_owner(draft_id) {
        if owner != agent {
            return Err("this draft belongs to another agent".into());
        }
    } else if super::persist::stored_compose_account(draft_id).is_some() {
        return Err("draft has no mailbox record".into());
    }
    if let Some(stored) = super::persist::stored_compose_account(draft_id)
        && stored != account
    {
        return Err("this draft belongs to another account".into());
    }
    if folder.join("draft.md").is_file() {
        return Ok(folder);
    }
    super::persist::open_owned_draft(draft_id, agent, account)
}

pub fn editor_is_running() -> bool {
    UnixStream::connect(draft_socket_path()).is_ok()
}

pub fn file_fallback_get(draft_id: &str, agent: &str, account: &str) -> Result<Value, String> {
    let folder = open_draft_for_agent(draft_id, agent, account)?;
    let text = workspace::read_draft_file(&folder)?;
    Ok(json!({
        "ok": true,
        "transmitted": false,
        "draft_id": draft_id,
        "draft_md": text,
        "cwd": folder,
        "user_changes_since_your_last_read": "",
    }))
}

pub fn file_fallback_edit(
    store: &crate::Store,
    draft_id: &str,
    agent: &str,
    account: &str,
    old: &str,
    new: &str,
) -> Result<Value, String> {
    let folder = open_draft_for_agent(draft_id, agent, account)?;
    let mut session =
        DraftSession::open_with_store(&folder, crate::AccountId(account.into()), store)?;
    let access = store
        .lock_agent_draft_access(session.account_id(), agent)
        .map_err(|err| err.to_string())?;
    session.edit_as_agent(session.revision(), old, new)?;
    drop(access);
    session.flush()?;
    let snap = session.snapshot();
    Ok(json!({
        "ok": true,
        "transmitted": false,
        "draft_id": draft_id,
        "draft_md": snap.text,
        "in_reply_to": snap.in_reply_to,
        "unreviewed": snap.unreviewed,
    }))
}

pub fn file_fallback_replace_body(
    store: &crate::Store,
    draft_id: &str,
    agent: &str,
    account: &str,
    body: &str,
) -> Result<Value, String> {
    let folder = open_draft_for_agent(draft_id, agent, account)?;
    let mut session =
        DraftSession::open_with_store(&folder, crate::AccountId(account.into()), store)?;
    let access = store
        .lock_agent_draft_access(session.account_id(), agent)
        .map_err(|err| err.to_string())?;
    let previous = session.body().to_string();
    session.replace_body(body)?;
    drop(access);
    session.flush()?;
    Ok(json!({
        "ok": true,
        "transmitted": false,
        "draft_id": draft_id,
        "draft_md": session.serialize(),
        "previous_body": previous,
        "in_reply_to": session.fields().in_reply_to,
    }))
}

pub fn request_send_without_editor() -> Value {
    json!({
        "ok": false,
        "transmitted": false,
        "opened": false,
        "error": "compose editor is not running; nothing was sent",
    })
}

#[cfg(test)]
mod tests {
    use super::super::frontmatter::DraftFields;
    use super::*;
    use crate::AccountId;
    use std::fs;

    #[test]
    fn request_send_and_edit_never_transmit() {
        let hub = hub_with_d1("edit");
        grant_d1(&hub);
        let edited = hub.handle_line(
            &json!({
                "id": 1,
                "op": "edit_draft",
                "draft_id": "d1",
                "agent": "agent:bot",
                "base_revision": 1,
                "edits": [{"old": "alpha", "new": "beta"}],
                "send": true
            })
            .to_string(),
        );
        let edited: Value = serde_json::from_str(&edited).unwrap();
        assert_eq!(edited["transmitted"], false);
        assert_eq!(edited["ok"], true);
        assert!(edited["draft_md"].as_str().unwrap().contains("beta"));
        let requested = hub.handle_line(
            &json!({
                "id": 2,
                "op": "request_send",
                "draft_id": "d1",
                "agent": "agent:bot",
                "send": true,
                "confirm": true
            })
            .to_string(),
        );
        let requested: Value = serde_json::from_str(&requested).unwrap();
        assert_eq!(requested["transmitted"], false);
        assert_eq!(requested["opened"], false);
        hub.note_shown("d1");
        let shown = hub.handle_line(
            &json!({
                "id": 3,
                "op": "request_send",
                "draft_id": "d1",
                "agent": "agent:bot"
            })
            .to_string(),
        );
        let shown: Value = serde_json::from_str(&shown).unwrap();
        assert_eq!(shown["opened"], true);
        assert_eq!(shown["transmitted"], false);
        assert!(
            requested["note"]
                .as_str()
                .unwrap()
                .contains("Nothing was sent")
        );
        assert!(requested.get("remote_id").is_none());
        let offline = request_send_without_editor();
        assert_eq!(offline["transmitted"], false);
    }

    #[test]
    fn reopening_a_sent_draft_keeps_it_locked() {
        let root = std::env::temp_dir().join(format!(
            "fell-hub-sent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&root, "d1", &fields, "alpha\n", &AccountId("a".into()))
                .unwrap();
        let db = root.join("mail.db");
        crate::Store::open(&db)
            .unwrap()
            .upsert_account(&crate::Account {
                id: AccountId("a".into()),
                display_name: "Jon".into(),
                email: "jon@example.com".into(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        let open = || {
            let mut session = DraftSession::testing_create_local(
                folder.clone(),
                "d1",
                AccountId("a".into()),
                fields.clone(),
                "alpha\n",
            )
            .unwrap();
            session.bind_store(&db);
            session
        };
        let hub = DraftHub::new();
        hub.insert(open());
        hub.with_session("d1", |session| {
            session
                .begin_send(&crate::MessageId::new("test@compose.fell"))
                .unwrap();
            session.end_send(Some(super::super::SendLock::Sent));
        });
        hub.insert(open());
        let lock = hub.with_session("d1", |session| {
            session.mark_seen();
            assert!(session.prepare_send().is_err());
            session.send_lock()
        });
        assert_eq!(lock, Some(Some(super::super::SendLock::Sent)));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reading_a_session_does_not_wake_the_window() {
        let root = std::env::temp_dir().join(format!(
            "fell-hub-idle-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&root, "d1", &fields, "alpha\n", &AccountId("a".into()))
                .unwrap();
        let session = DraftSession::testing_create_local(
            folder,
            "d1",
            AccountId("a".into()),
            fields,
            "alpha\n",
        )
        .unwrap();
        let hub = DraftHub::new();
        hub.insert(session);
        let rx = hub.subscribe();
        while rx.try_recv().is_ok() {}
        hub.with_session("d1", |session| {
            let _ = session.snapshot();
            session.mark_seen();
        });
        assert!(rx.try_recv().is_err());
        hub.with_session("d1", |session| session.open_for_edit());
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn request_send_waits_until_the_window_shows_that_draft() {
        let hub = hub_with_d1("show-wait");
        grant_d1(&hub);
        let _inbox = hub.subscribe();
        let waiter = Arc::clone(&hub);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(40));
            waiter.note_shown("d1");
        });
        let requested = hub.handle_line(
            &json!({
                "id": 1,
                "op": "request_send",
                "draft_id": "d1",
                "agent": "agent:bot"
            })
            .to_string(),
        );
        let requested: Value = serde_json::from_str(&requested).unwrap();
        assert_eq!(requested["opened"], true);
        assert_eq!(requested["transmitted"], false);
    }

    fn hub_with_d1(name: &str) -> Arc<DraftHub> {
        let root = std::env::temp_dir().join(format!(
            "fell-hub-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let folder =
            workspace::materialize_in(&root, "d1", &fields, "alpha\n", &AccountId("a".into()))
                .unwrap();
        let mut session = DraftSession::testing_create_local(
            folder,
            "d1",
            AccountId("a".into()),
            fields,
            "alpha\n",
        )
        .unwrap();
        session.set_owner("agent:bot");
        let hub = Arc::new(DraftHub::new());
        hub.insert(session);
        hub
    }

    fn grant_d1(hub: &DraftHub) -> crate::Store {
        let (path, account_id) = hub
            .with_session("d1", |session| {
                (
                    session.folder().with_file_name("mail.db"),
                    session.account_id().clone(),
                )
            })
            .unwrap();
        let store = crate::Store::open(&path).unwrap();
        store
            .upsert_account(&crate::Account {
                id: account_id.clone(),
                display_name: "Demo".into(),
                email: "demo@example.test".into(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        store
            .set_agent_grant(
                &account_id,
                "bot",
                crate::AgentGrant::enabled(true, false, true),
            )
            .unwrap();
        hub.with_session("d1", |session| session.bind_store(path))
            .unwrap();
        store
    }

    #[test]
    fn live_draft_writes_recheck_scope_after_mcp_authorizes() {
        let hub = hub_with_d1("revoked");
        let store = grant_d1(&hub);
        let account = AccountId("a".into());
        let settings = crate::Store::open(store.db_path().unwrap()).unwrap();
        for revoked in [
            crate::AgentGrant::DISABLED,
            crate::AgentGrant::enabled(true, false, false),
        ] {
            let enabled = crate::AgentGrant::enabled(true, false, true);
            settings.set_agent_grant(&account, "bot", enabled).unwrap();
            assert_eq!(store.agent_grant(&account, "bot").unwrap(), enabled);
            settings.set_agent_grant(&account, "bot", revoked).unwrap();
            for op in ["edit_draft", "replace_draft_body", "request_send"] {
                let reply: Value = serde_json::from_str(
                    &hub.handle_line(
                        &json!({
                            "id": 1,
                            "op": op,
                            "draft_id": "d1",
                            "agent": "agent:bot",
                            "edits": [{"old": "alpha", "new": "beta"}],
                            "body": "beta"
                        })
                        .to_string(),
                    ),
                )
                .unwrap();
                assert_eq!(reply["ok"], false, "{reply}");
                assert_eq!(reply["transmitted"], false, "{reply}");
                let error = reply["error"].as_str().unwrap();
                assert!(
                    error.contains(if revoked.enabled {
                        "draft scope"
                    } else {
                        "disabled"
                    }),
                    "{reply}"
                );
            }
        }
        assert_eq!(hub.reveal_count(), 0);
        assert_eq!(hub.focused_id(), None);
        assert_eq!(
            hub.with_session("d1", |session| session.body().to_owned())
                .unwrap(),
            "alpha\n"
        );
    }

    #[test]
    fn a_blocked_mailbox_open_leaves_sessions_available_to_the_ui() {
        let hub = hub_with_d1("blocked-agent-write");
        let store = grant_d1(&hub);
        let write = crate::store::begin_immediate(store.connection()).unwrap();
        let agent_hub = Arc::clone(&hub);
        let agent = std::thread::spawn(move || request_send_d1(&agent_hub));
        let start = std::time::Instant::now();
        while !store.write_is_waiting() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "agent did not reach SQLite"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        let ui_can_take_sessions = hub.sessions.try_lock().is_ok();
        drop(write);
        assert_eq!(agent.join().unwrap()["ok"], true);
        assert!(
            ui_can_take_sessions,
            "the agent held sessions while waiting on SQLite"
        );
    }

    #[test]
    fn offline_draft_writes_recheck_scope_after_mcp_authorizes() {
        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let id = "fallback";
        let account = AccountId("offline".into());
        let fields = DraftFields {
            subject: "Demo draft".into(),
            ..DraftFields::default()
        };
        let folder = workspace::materialize_workspace(id, &fields, "alpha\n", &account).unwrap();
        let db = root.join("drafts.db");
        let store = crate::Store::open(&db).unwrap();
        store
            .upsert_account(&crate::Account {
                id: account.clone(),
                display_name: "Demo".into(),
                email: "demo@example.test".into(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        store
            .save_compose(&super::super::persist::StoredCompose {
                id: id.to_string(),
                account_id: account.clone(),
                fields,
                body: "alpha\n".into(),
                files: Vec::new(),
                identities: Vec::new(),
                managed_signature: None,
                owner: "agent:bot".into(),
                reply: Default::default(),
            })
            .unwrap();
        super::super::persist::remember_compose_store(id, &db).unwrap();
        let before = workspace::read_draft_file(&folder).unwrap();
        let enabled = crate::AgentGrant::enabled(true, false, true);
        let settings = crate::Store::open(&db).unwrap();
        for (revoked, expected) in [
            (crate::AgentGrant::DISABLED, "disabled"),
            (
                crate::AgentGrant::enabled(true, false, false),
                "draft scope",
            ),
        ] {
            settings.set_agent_grant(&account, "bot", enabled).unwrap();
            assert_eq!(store.agent_grant(&account, "bot").unwrap(), enabled);
            settings.set_agent_grant(&account, "bot", revoked).unwrap();
            let edit =
                file_fallback_edit(&store, id, "agent:bot", account.as_str(), "alpha", "beta");
            let replace =
                file_fallback_replace_body(&store, id, "agent:bot", account.as_str(), "beta");
            for result in [edit, replace] {
                let error = result.unwrap_err();
                assert!(error.contains(expected), "{error}");
            }
        }
        assert_eq!(workspace::read_draft_file(&folder).unwrap(), before);
        assert_eq!(store.load_compose(id).unwrap().unwrap().body, "alpha\n");
    }

    fn request_send_d1(hub: &DraftHub) -> Value {
        let reply = hub.handle_line(
            &json!({ "id": 1, "op": "request_send", "draft_id": "d1", "agent": "agent:bot" })
                .to_string(),
        );
        serde_json::from_str(&reply).unwrap()
    }

    #[test]
    fn each_request_send_asks_the_host_to_show_the_editor_again() {
        let hub = hub_with_d1("reveal");
        grant_d1(&hub);
        assert_eq!(hub.reveal_count(), 0);
        request_send_d1(&hub);
        assert_eq!(hub.reveal_count(), 1);
        // Same draft, already focused: still a new request to bring it up.
        request_send_d1(&hub);
        assert_eq!(hub.reveal_count(), 2);
        assert_eq!(hub.focused_id().as_deref(), Some("d1"));
        let snap = hub
            .with_session("d1", |session| session.snapshot())
            .unwrap();
        assert_eq!(snap.send_requested_by.as_deref(), Some("agent:bot"));
        // Reading or editing a draft never asks for the editor.
        hub.handle_line(
            &json!({ "id": 2, "op": "get_draft", "draft_id": "d1", "agent": "agent:bot" })
                .to_string(),
        );
        assert_eq!(hub.reveal_count(), 2);
    }

    #[test]
    fn a_closed_window_does_not_count_as_showing_the_draft() {
        let hub = hub_with_d1("hidden");
        grant_d1(&hub);
        let _host = hub.subscribe();
        hub.note_shown("d1");
        assert_eq!(request_send_d1(&hub)["opened"], true);
        hub.note_hidden("d1");
        let started = std::time::Instant::now();
        let reply = request_send_d1(&hub);
        assert_eq!(reply["opened"], false);
        assert_eq!(reply["transmitted"], false);
        assert!(started.elapsed() >= std::time::Duration::from_millis(400));
        // Hiding another draft leaves this one shown.
        hub.note_shown("d1");
        hub.note_hidden("other");
        assert_eq!(request_send_d1(&hub)["opened"], true);
    }

    #[test]
    fn a_second_process_cannot_take_over_the_draft_socket() {
        let root = std::env::temp_dir().join(format!(
            "fell-hub-bind-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("live.sock");
        let first = DraftHub::bind(&path).unwrap();
        let second = DraftHub::bind(&path);
        assert_eq!(
            second.err().map(|err| err.kind()),
            Some(std::io::ErrorKind::AddrInUse)
        );
        // The first server still owns the path it bound.
        assert!(UnixStream::connect(&path).is_ok());
        drop(first);
        // A socket file left behind is replaced once nobody holds the lock.
        let _again = DraftHub::bind(&path).unwrap();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_listener_that_is_gone_is_dropped() {
        let hub = hub_with_d1("listeners");
        grant_d1(&hub);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        hub.subscribe_with(move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            false
        });
        request_send_d1(&hub);
        request_send_d1(&hub);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn hiding_the_window_closes_the_draft_for_editing() {
        let hub = hub_with_d1("closed");
        grant_d1(&hub);
        request_send_d1(&hub);
        assert!(
            hub.with_session("d1", |session| session.snapshot().open)
                .unwrap()
        );
        hub.note_hidden("d1");
        assert!(
            !hub.with_session("d1", |session| session.snapshot().open)
                .unwrap()
        );
    }

    #[test]
    fn a_closed_review_window_leaves_one_notice() {
        let hub = hub_with_d1("notice");
        grant_d1(&hub);
        let _host = hub.subscribe();
        request_send_d1(&hub);
        hub.note_shown("d1");
        hub.note_hidden("d1");
        // The window's own release reports it again; that adds nothing.
        hub.note_hidden("d1");
        let lines = hub.take_closed();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Not sent"), "{lines:?}");
        assert!(hub.take_closed().is_empty());
    }

    /// Typing that is still waiting on the save debounce.
    fn type_unsaved(hub: &DraftHub, body: &str) -> std::path::PathBuf {
        hub.with_session("d1", |session| {
            session.set_body_from_user(body.into());
            session.arm_outbound(u64::MAX);
            assert!(!session.poll_outbound(0), "the debounce has not elapsed");
            session.folder().join("draft.md")
        })
        .unwrap()
    }

    #[test]
    fn closing_the_editor_saves_typing_the_debounce_held_back() {
        let hub = hub_with_d1("close-flush");
        let file = type_unsaved(&hub, "typed before close\n");
        hub.note_shown("d1");
        hub.note_hidden("d1");
        hub.with_session("d1", |session| session.save_waiter())
            .flatten()
            .unwrap()
            .wait()
            .unwrap();
        assert!(
            fs::read_to_string(file)
                .unwrap()
                .contains("typed before close")
        );
    }

    #[test]
    fn quitting_saves_typing_the_debounce_held_back() {
        let hub = hub_with_d1("quit-flush");
        let file = type_unsaved(&hub, "typed before quit\n");
        hub.settle_all();
        assert!(
            fs::read_to_string(file)
                .unwrap()
                .contains("typed before quit")
        );
    }

    #[test]
    fn the_inbox_shows_a_draft_without_naming_an_agent() {
        let hub = hub_with_d1("show");
        assert!(!hub.show("missing"));
        assert!(hub.show("d1"));
        assert_eq!(hub.reveal_count(), 1);
        assert_eq!(hub.focused_id().as_deref(), Some("d1"));
        let snap = hub
            .with_session("d1", |session| session.snapshot())
            .unwrap();
        assert!(snap.open);
        assert_eq!(snap.send_requested_by, None);
        // The person's own draft closes to the drafts list, not a review.
        hub.note_shown("d1");
        hub.note_hidden("d1");
        let lines = hub.take_closed();
        assert!(lines[0].starts_with("Draft saved"), "{lines:?}");
        assert!(lines[0].ends_with("gd drafts"), "{lines:?}");
    }

    #[test]
    fn a_live_draft_is_not_replaced_by_a_second_open() {
        let hub = hub_with_d1("insert-new");
        hub.with_session("d1", |session| session.set_body_from_user("live\n".into()));
        let folder = hub
            .with_session("d1", |session| session.folder().to_path_buf())
            .unwrap();
        let again = DraftSession::testing_create_local(
            folder,
            "d1",
            AccountId("a".into()),
            DraftFields::default(),
            "from disk\n",
        )
        .unwrap();
        assert!(!hub.insert_new(again));
        let body = hub
            .with_session("d1", |session| session.body().to_string())
            .unwrap();
        assert_eq!(body, "live\n");
    }

    #[test]
    fn a_late_send_that_may_have_gone_out_is_a_warning() {
        let hub = hub_with_d1("late");
        hub.report_send("d1", Some(MAYBE_SENT_NOTICE));
        let notices = hub.take_notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        let (line, warn) = &notices[0];
        assert!(line.starts_with("May have been sent"), "{line}");
        assert!(line.contains("\u{201c}Hi\u{201d}"), "{line}");
        assert!(*warn, "a possible double send is shown as a warning");
        // A draft simply left unsent is news, not a warning.
        hub.report_send("d1", None);
        let notices = hub.take_notices();
        assert_eq!(
            notices[0].0,
            "Not sent  ·  \u{201c}Hi\u{201d}  ·  gd drafts"
        );
        assert!(!notices[0].1);
    }

    #[test]
    fn the_inbox_can_reopen_the_review_that_closed() {
        let hub = hub_with_d1("reopen");
        grant_d1(&hub);
        let _host = hub.subscribe();
        assert!(!hub.reopen_last(), "nothing has closed yet");
        request_send_d1(&hub);
        hub.note_shown("d1");
        hub.note_hidden("d1");
        let before = hub.reveal_count();
        assert!(hub.take_closed()[0].contains("space a r reopen"));
        assert!(hub.reopen_last());
        assert_eq!(hub.reveal_count(), before + 1);
        assert_eq!(hub.focused_id().as_deref(), Some("d1"));
        assert!(
            hub.with_session("d1", |session| session.snapshot().open)
                .unwrap()
        );
    }

    #[test]
    fn reopen_does_not_raise_a_draft_that_was_sent_after_it_closed() {
        let hub = hub_with_d1("reopen-sent");
        grant_d1(&hub);
        let _host = hub.subscribe();
        request_send_d1(&hub);
        hub.note_shown("d1");
        hub.note_hidden("d1");
        assert!(hub.reopen_last());
        hub.note_shown("d1");
        hub.with_session("d1", |session| {
            session.end_send(Some(SendLock::Sent));
        });
        // The sent close clears what the first close left for reopen.
        hub.note_hidden("d1");
        let lines = hub.take_closed();
        assert!(lines.last().unwrap().starts_with("Sent"), "{lines:?}");
        assert!(!lines.last().unwrap().contains("gs sent"), "{lines:?}");
        let before = hub.reveal_count();
        assert!(
            !hub.reopen_last(),
            "mail that went out is not offered again"
        );
        assert_eq!(hub.reveal_count(), before);
    }

    #[test]
    fn reopen_refuses_a_draft_sent_while_no_window_showed_it() {
        let hub = hub_with_d1("reopen-sent-hidden");
        grant_d1(&hub);
        let _host = hub.subscribe();
        request_send_d1(&hub);
        hub.note_shown("d1");
        hub.note_hidden("d1");
        // Sent without a window closing on it, so no close cleared it.
        hub.with_session("d1", |session| {
            session.end_send(Some(SendLock::Sent));
        });
        assert!(!hub.reopen_last());
        assert!(!hub.reopen_last(), "and it stays refused");
    }

    #[test]
    fn a_send_reported_after_the_window_moved_on_reaches_the_inbox() {
        let hub = hub_with_d1("report-send");
        hub.show("d1");
        hub.note_shown("d1");
        // The window moved to another draft while the send was in flight.
        hub.note_hidden("d1");
        let _ = hub.take_closed();
        hub.with_session("d1", |session| {
            session.end_send(Some(SendLock::Sent));
        });
        hub.report_send("d1", None);
        let lines = hub.take_closed();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Sent"), "{lines:?}");
        assert!(lines[0].contains("Hi"), "{lines:?}");
        assert!(!hub.reopen_last());
        // A send that did not go out says why, and the draft stays.
        hub.with_session("d1", |session| session.end_send(None));
        hub.report_send("d1", Some("Not sent: refused"));
        let lines = hub.take_closed();
        assert!(lines[0].starts_with("Not sent: refused"), "{lines:?}");
        assert!(lines[0].ends_with("gd drafts"), "{lines:?}");
    }

    #[test]
    fn one_open_of_a_draft_at_a_time_and_none_once_it_is_live() {
        let hub = DraftHub::new();
        let account = AccountId("a".into());
        let first = hub
            .begin_open("d1", &account)
            .unwrap()
            .expect("nothing is opening d1");
        assert!(
            hub.begin_open("d1", &account).unwrap().is_none(),
            "d1 is being opened"
        );
        assert!(
            hub.begin_open("d2", &account).unwrap().is_some(),
            "other drafts are not held"
        );
        drop(first);
        assert!(hub.begin_open("d1", &account).unwrap().is_some());
        let live = hub_with_d1("open-guard");
        assert!(
            live.begin_open("d1", &account).unwrap().is_none(),
            "d1 is live"
        );
    }

    #[test]
    fn account_removal_waits_only_for_drafts_from_that_account() {
        let hub = Arc::new(DraftHub::new());
        let mail = AccountId("mail".into());
        let work = AccountId("work".into());
        let work_open = hub.begin_open("work-draft", &work).unwrap().unwrap();
        let mail_open = hub.begin_open("mail-draft", &mail).unwrap().unwrap();
        let (ready, removal) = mpsc::channel();
        let worker = std::thread::spawn({
            let hub = hub.clone();
            let mail = mail.clone();
            move || ready.send(hub.release_account_drafts(&mail)).unwrap()
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !hub.opening.lock().unwrap().removing.contains(&mail) {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(removal.try_recv().is_err());
        assert!(hub.begin_open("another-mail-draft", &mail).is_err());
        assert!(
            hub.begin_open("another-work-draft", &work)
                .unwrap()
                .is_some()
        );
        drop(mail_open);
        let removal = removal
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert!(hub.begin_open("another-mail-draft", &mail).is_err());
        drop(removal);
        assert!(
            hub.begin_open("another-mail-draft", &mail)
                .unwrap()
                .is_some()
        );
        drop(work_open);
        worker.join().unwrap();
    }

    #[test]
    fn the_socket_waits_for_a_draft_the_inbox_is_opening() {
        let hub = DraftHub::new();
        let _opening = hub
            .begin_open("d9", &AccountId("a".into()))
            .unwrap()
            .unwrap();
        let reply: Value = serde_json::from_str(
            &hub.handle_line(
                &json!({ "id": 1, "op": "get_draft", "draft_id": "d9", "agent": "agent:bot" })
                    .to_string(),
            ),
        )
        .unwrap();
        assert_eq!(reply["ok"], false, "{reply}");
        assert!(
            reply["error"].as_str().unwrap().contains("opening"),
            "{reply}"
        );
        assert!(hub.with_session("d9", |_| ()).is_none());
    }

    #[test]
    fn another_agent_cannot_read_the_draft() {
        let root = std::env::temp_dir().join(format!(
            "fell-hub-owner-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&root);
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            bcc: vec!["secret-bcc@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let folder = workspace::materialize_in(
            &root,
            "d1",
            &fields,
            "hidden body\n",
            &AccountId("a".into()),
        )
        .unwrap();
        let mut session = DraftSession::testing_create_local(
            folder.clone(),
            "d1",
            AccountId("a".into()),
            fields,
            "hidden body\n",
        )
        .unwrap();
        session.set_owner("agent:ada");
        workspace::write_owner(&folder, "agent:bob").unwrap();
        let hub = DraftHub::new();
        hub.insert(session);
        let denied = hub.handle_line(
            &json!({
                "id": 1,
                "op": "get_draft",
                "draft_id": "d1",
                "agent": "agent:bob"
            })
            .to_string(),
        );
        assert!(denied.contains("another agent"), "{denied}");
        assert!(!denied.contains("secret-bcc@example.com"));
        assert!(!denied.contains("hidden body"));
        let allowed = hub.handle_line(
            &json!({
                "id": 2,
                "op": "get_draft",
                "draft_id": "d1",
                "agent": "agent:ada"
            })
            .to_string(),
        );
        assert!(allowed.contains("hidden body"), "{allowed}");
        assert!(allowed.contains("secret-bcc@example.com"), "{allowed}");
    }

    #[test]
    fn the_socket_loads_the_stored_account_not_local() {
        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let id = "stored-account";
        let db = root.join("drafts.db");
        let fields = DraftFields {
            from: "jon@example.com".into(),
            to: vec!["ada@example.com".into()],
            bcc: vec!["secret-bcc@example.com".into()],
            subject: "Hi".into(),
            ..DraftFields::default()
        };
        let folder = workspace::materialize_workspace(
            id,
            &fields,
            "hidden body\n",
            &AccountId("acct-real".into()),
        )
        .unwrap();
        workspace::write_owner(&folder, "agent:bob").unwrap();
        let store = crate::Store::open(&db).unwrap();
        store
            .upsert_account(&crate::Account {
                id: AccountId("acct-real".into()),
                display_name: "Jon".into(),
                email: "jon@example.com".into(),
                backend_kind: crate::BackendKind::Bridge,
            })
            .unwrap();
        store
            .save_compose(&super::super::persist::StoredCompose {
                id: id.to_string(),
                account_id: AccountId("acct-real".into()),
                fields: fields.clone(),
                body: "hidden body\n".into(),
                files: Vec::new(),
                identities: vec!["jon@example.com".into()],
                managed_signature: None,
                owner: "agent:ada".into(),
                reply: Default::default(),
            })
            .unwrap();
        super::super::persist::remember_compose_store(id, &db).unwrap();
        let hub = DraftHub::new();
        let denied = hub.handle_line(
            &json!({
                "id": 1,
                "op": "get_draft",
                "draft_id": id,
                "agent": "agent:bob"
            })
            .to_string(),
        );
        assert!(denied.contains("another agent"), "{denied}");
        assert!(!denied.contains("hidden body"), "{denied}");
        assert!(!denied.contains("secret-bcc"), "{denied}");
        let opened = hub.handle_line(
            &json!({
                "id": 2,
                "op": "get_draft",
                "draft_id": id,
                "agent": "agent:ada"
            })
            .to_string(),
        );
        assert!(opened.contains("hidden body"), "{opened}");
        let account = hub.with_session(id, |session| session.account_id().as_str().to_string());
        assert_eq!(account.as_deref(), Some("acct-real"));
        let missing = DraftHub::new().handle_line(
            &json!({
                "id": 3,
                "op": "get_draft",
                "draft_id": format!("{id}x"),
                "agent": "agent:ada"
            })
            .to_string(),
        );
        assert!(missing.contains("mailbox"), "{missing}");
        assert!(!missing.contains("another agent"), "{missing}");
    }
}
