//! Hosts live drafts for `fell-mcp`: the draft socket, the `draft.md`
//! watcher, and the compose window an agent's `request_send` brings up.
//!
//! The inbox and the compose demo both run this. Nothing here transmits:
//! mail goes out only from the compose window's own send action.

use std::cell::RefCell;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use fell_core::AccountId;
use fell_core::compose::{
    DraftFields, DraftHub, DraftSession, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENT_FILES, ReplyParts,
    ReplyStart, ResolvedFile, StoredCompose,
};
use futures::channel::mpsc;
use gpui::{
    App, AppContext, AsyncApp, Bounds, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions,
    px, size,
};
use notify::{Event, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

use crate::compose_launch::{ComposeInput, ComposeLaunch, attachment};
use crate::compose_ui::ComposeView;

/// Answer `fell-mcp` draft calls on the user's private socket. Another Fell
/// already serving it keeps it; this one then serves no agents.
pub fn serve_socket(hub: Arc<DraftHub>) {
    if let Err(err) = serve_socket_at(hub, &fell_core::compose::draft_socket_path()) {
        fell_core::diagnostic!(
            WARN,
            "Could not serve agent drafts",
            error = &err as &dyn std::error::Error
        );
    }
}

/// Serve the hub at `path` on a thread of its own.
fn serve_socket_at(hub: Arc<DraftHub>, path: &std::path::Path) -> std::io::Result<()> {
    let listener = DraftHub::bind(path)?;
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            hub.serve_one(stream);
        }
    });
    Ok(())
}

const OPEN_COMPOSE: &str = "open-compose ";
const MAX_REQUEST_LINE: u64 = 8 * 1024 * 1024;
const SOCKET_TIMEOUT: Duration = Duration::from_secs(5);

/// Inbox startup owns the socket before GPUI or the store opens. A compose
/// invocation that loses the lock hands off its draft and exits.
pub(crate) fn inbox_socket(
    hub: Arc<DraftHub>,
    initial: Option<ComposeLaunch>,
) -> Result<Option<mpsc::Receiver<ComposeLaunch>>, String> {
    let path = fell_core::compose::draft_socket_path();
    let (mut queued, requests) = mpsc::channel(4);
    match DraftHub::bind(&path) {
        Ok(listener) => {
            if let Some(initial) = initial {
                queued.try_send(initial).expect("empty compose queue");
            }
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    serve_inbox_request(stream, &hub, &mut queued);
                }
            });
        }
        Err(err) => {
            if let Some(initial) = initial {
                if err.kind() == io::ErrorKind::AddrInUse {
                    send_compose(&path, &initial).map_err(|err| err.to_string())?;
                    return Ok(None);
                }
                return Err(format!("could not open the Fell socket: {err}"));
            }
            fell_core::diagnostic!(
                WARN,
                "Could not serve agent drafts or external compose requests",
                error = &err as &dyn std::error::Error
            );
        }
    }
    Ok(Some(requests))
}

#[derive(Serialize, Deserialize)]
struct AttachmentHeader {
    name: String,
    len: usize,
}

fn send_compose(path: &Path, draft: &ComposeLaunch) -> io::Result<()> {
    let deadline = std::time::Instant::now() + SOCKET_TIMEOUT;
    let mut stream = loop {
        match UnixStream::connect(path) {
            Ok(stream) => break stream,
            // The winner can have the lock before it has bound the socket.
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(err),
        }
    };
    stream.set_write_timeout(Some(SOCKET_TIMEOUT))?;
    stream.set_read_timeout(Some(SOCKET_TIMEOUT))?;

    let headers: Vec<_> = draft
        .files
        .iter()
        .map(|file| AttachmentHeader {
            name: file.display_name.clone(),
            len: file.bytes.len(),
        })
        .collect();
    let header = serde_json::to_string(&(&draft.input, headers))?;
    if (OPEN_COMPOSE.len() + header.len() + 1) as u64 > MAX_REQUEST_LINE {
        return Err(io::Error::other("compose request is too large"));
    }
    writeln!(stream, "{OPEN_COMPOSE}{header}")?;
    for file in &draft.files {
        stream.write_all(&file.bytes)?;
    }
    let mut reply = String::new();
    BufReader::new(stream).take(4096).read_line(&mut reply)?;
    match reply.trim_end() {
        "ok" => Ok(()),
        "" => Err(io::Error::other(
            "Fell did not acknowledge the compose request",
        )),
        error => Err(io::Error::other(error.to_string())),
    }
}

fn serve_inbox_request(
    mut stream: UnixStream,
    hub: &DraftHub,
    queued: &mut mpsc::Sender<ComposeLaunch>,
) {
    if stream.set_read_timeout(Some(SOCKET_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(SOCKET_TIMEOUT)).is_err()
    {
        return;
    }
    let mut reader = BufReader::new(&mut stream);
    let mut line = String::new();
    if reader
        .by_ref()
        .take(MAX_REQUEST_LINE)
        .read_line(&mut line)
        .is_err()
        || !line.ends_with('\n')
    {
        return;
    }
    if let Some(header) = line.trim_end().strip_prefix(OPEN_COMPOSE) {
        let result = read_compose(header, &mut reader).and_then(|draft| {
            queued
                .try_send(draft)
                .map_err(|_| io::Error::other("Fell cannot accept another compose request yet"))
        });
        match result {
            Ok(()) => {
                let _ = writeln!(stream, "ok");
            }
            Err(err) => {
                let _ = writeln!(stream, "{err}");
            }
        }
    } else {
        let _ = writeln!(stream, "{}", hub.handle_line(line.trim()));
    }
}

fn read_compose(header: &str, reader: &mut impl Read) -> io::Result<ComposeLaunch> {
    let (input, headers): (ComposeInput, Vec<AttachmentHeader>) = serde_json::from_str(header)?;
    input.check_headers().map_err(io::Error::other)?;
    if headers.len() > MAX_ATTACHMENT_FILES {
        return Err(io::Error::other("too many attachments"));
    }
    let mut files = Vec::with_capacity(headers.len());
    let mut remaining = MAX_ATTACHMENT_BYTES as usize;
    for (index, header) in headers.into_iter().enumerate() {
        remaining = remaining
            .checked_sub(header.len)
            .ok_or_else(|| io::Error::other("attachments exceed the sending limit"))?;
        let mut bytes = vec![0; header.len];
        reader.read_exact(&mut bytes)?;
        files.push(attachment(index, Some(&header.name), bytes));
    }
    Ok(ComposeLaunch { input, files })
}

/// Merge outside writes to the focused draft's `draft.md`, such as an
/// agent's script, into the live draft. Follows the hub's focus, starting
/// from `initial` when a draft is already open.
pub fn watch_folder(
    hub: Arc<DraftHub>,
    initial: Option<(String, PathBuf)>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Event, notify::Error>>();
        let Ok(mut watcher) = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        }) else {
            return;
        };
        let mut current: Option<(String, PathBuf)> = None;
        if let Some((id, folder)) = initial {
            if watcher.watch(&folder, RecursiveMode::NonRecursive).is_err() {
                return;
            }
            current = Some((id, folder));
        }
        loop {
            if let Some(id) = hub.focused_id()
                && current
                    .as_ref()
                    .is_none_or(|(current_id, _)| *current_id != id)
                && let Some(next) = hub.with_session(&id, |session| session.folder().to_path_buf())
            {
                // The old watch ends either way: a folder that is gone took
                // its watch with it. A failed watch leaves nothing current,
                // so the next pass tries again.
                if let Some((_, folder)) = current.take() {
                    let _ = watcher.unwatch(&folder);
                }
                if watcher.watch(&next, RecursiveMode::NonRecursive).is_ok() {
                    // A write that landed before the watch began sent no
                    // event; merge whatever the file holds now.
                    merge_from_disk(&hub, &id, &next);
                    current = Some((id, next));
                }
            }
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(_) => {
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(150) {
                        let remaining = Duration::from_millis(150).saturating_sub(start.elapsed());
                        if rx.recv_timeout(remaining).is_err() {
                            break;
                        }
                    }
                    let Some((id, folder)) = current.clone() else {
                        continue;
                    };
                    merge_from_disk(&hub, &id, &folder);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    })
}

/// Merge `draft.md` in `folder` into live draft `id`, as an outside edit.
fn merge_from_disk(hub: &DraftHub, id: &str, folder: &std::path::Path) {
    if fell_core::compose::draft_file_is_symlink(folder) {
        return;
    }
    // Sample before reading: the read happens outside the lock, and a save
    // can land in between.
    let since = hub
        .with_session(id, |session| session.disk_generation())
        .unwrap_or(0);
    let content = std::fs::read_to_string(folder.join("draft.md")).ok();
    hub.with_session(id, |session| {
        session.push_disk_seen(content, since, 0);
        session.poll_disk(150);
    });
}

/// Where the inbox keeps the person's drafts: their folders under the
/// runtime drafts root, the same place agents' drafts live, and their rows
/// in the mailbox file the inbox reads.
#[derive(Clone, Debug)]
pub(crate) struct DraftHome {
    pub root: PathBuf,
    pub store: PathBuf,
}

impl DraftHome {
    /// The real drafts location for the mailbox at `store`.
    pub fn real(store: PathBuf) -> Self {
        Self {
            root: fell_core::compose::draft_root(),
            store,
        }
    }

    /// Start a new, empty draft from `from` on `account` and make it live
    /// in `hub`. Its mailbox row is written before its folder. Disk only;
    /// run it off the UI thread. Returns the draft's id.
    pub fn new_draft(
        &self,
        hub: &DraftHub,
        account: &AccountId,
        from: &str,
    ) -> Result<String, String> {
        let fields = DraftFields {
            from: from.to_string(),
            ..DraftFields::default()
        };
        self.new_reply(
            hub,
            account,
            ReplyStart {
                fields,
                parts: ReplyParts::default(),
            },
        )
    }

    /// Start a reply or forward on `account` from `start`, as
    /// [`Self::new_draft`] starts a new draft. Its References and quoted
    /// original go into the mailbox row with it. Disk only.
    pub fn new_reply(
        &self,
        hub: &DraftHub,
        account: &AccountId,
        start: ReplyStart,
    ) -> Result<String, String> {
        self.new_reply_with_files(hub, account, start, Vec::new())
    }

    /// Received files have already been copied out of the part cache. Save
    /// them in the same transaction as the draft before opening its folder.
    pub fn new_reply_with_files(
        &self,
        hub: &DraftHub,
        account: &AccountId,
        start: ReplyStart,
        files: Vec<ResolvedFile>,
    ) -> Result<String, String> {
        self.new_seeded(hub, account, start, String::new(), files)
    }

    pub(crate) fn new_seeded(
        &self,
        hub: &DraftHub,
        account: &AccountId,
        start: ReplyStart,
        body: String,
        files: Vec<ResolvedFile>,
    ) -> Result<String, String> {
        let id = new_draft_id();
        if files.is_empty() {
            DraftSession::start_reply_in(
                &self.root,
                &id,
                account.clone(),
                start.fields,
                &body,
                &start.parts,
                &self.store,
            )?;
        } else {
            let store = fell_core::Store::open(&self.store).map_err(|err| err.to_string())?;
            store
                .save_compose(&StoredCompose {
                    id: id.clone(),
                    account_id: account.clone(),
                    fields: start.fields,
                    body,
                    files,
                    identities: Vec::new(),
                    owner: String::new(),
                    reply: start.parts,
                })
                .map_err(|err| err.to_string())?;
        }
        self.open(hub, &id, account)?;
        Ok(id)
    }

    pub fn open_server_draft(
        &self,
        hub: &DraftHub,
        account: &AccountId,
        source: &fell_core::RemoteId,
    ) -> Result<String, fell_core::Error> {
        let config = crate::mailbox::send_account_config(account)
            .map_err(|_| fell_core::Error::Action("The account is unavailable".into()))?
            .ok_or_else(|| {
                fell_core::Error::Action("There is no server draft in the demo mailbox".into())
            })?;
        let store = fell_core::Store::open(&self.store)?;
        let id = config.import_server_draft_copy(&store, source, &new_draft_id())?;
        self.open(hub, &id, account)
            .map_err(fell_core::Error::Action)?;
        Ok(id)
    }

    pub fn new_suggested_reply(
        &self,
        hub: &DraftHub,
        account: &AccountId,
        start: ReplyStart,
        note: &crate::model::NoteRequest,
    ) -> Result<String, String> {
        let id = new_draft_id();
        self.open_suggested_reply(hub, account, start, note, &id)?;
        Ok(id)
    }

    fn open_suggested_reply(
        &self,
        hub: &DraftHub,
        account: &AccountId,
        start: ReplyStart,
        note: &crate::model::NoteRequest,
        id: &str,
    ) -> Result<(), String> {
        let fell_core::notes::NoteDecision::Accept(position) = note.decision else {
            return Err("The agent note changed".into());
        };
        let Some(_opening) = hub.begin_open(id) else {
            return Err("The reply draft is already opening".into());
        };
        let store = fell_core::Store::open(&self.store).map_err(|err| err.to_string())?;
        let action = store
            .save_suggested_reply(
                &StoredCompose {
                    id: id.to_owned(),
                    account_id: account.clone(),
                    fields: start.fields,
                    body: String::new(),
                    files: Vec::new(),
                    identities: Vec::new(),
                    owner: String::new(),
                    reply: start.parts,
                },
                &fell_core::ThreadId(note.thread.clone()),
                note.id,
                position,
            )
            .map_err(|err| err.to_string())?;
        if let Err(mut err) = self.open_reserved(hub, id, account, &store) {
            if let Err(rollback) = store.rollback_suggested_reply(account, id, action) {
                err.push_str(&format!(". Couldn't roll back the reply: {rollback}"));
            }
            let folder = self.root.join(id);
            for path in [&folder, &fell_core::compose::control_dir(&folder)] {
                if let Err(cleanup) = std::fs::remove_dir_all(path)
                    && !matches!(
                        cleanup.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
                    )
                {
                    err.push_str(&format!(". Couldn't remove the reply files: {cleanup}"));
                }
            }
            return Err(err);
        }
        Ok(())
    }

    /// Make draft `id` live in `hub` as the mailbox holds it. Its folder is
    /// written from the mailbox row first: the row is Fell's own copy, while
    /// anything on the machine may have changed the folder since the draft
    /// was last open. `draft.md` is kept when it holds Fell's own last save,
    /// which is newer than a row that save could not write.
    ///
    /// A draft that is live already, or that another open is reading, is
    /// left alone: its folder may hold typing the mailbox does not have yet.
    /// The caller shows it either way. Disk only; run it off the UI thread.
    pub fn open(&self, hub: &DraftHub, id: &str, account: &AccountId) -> Result<(), String> {
        let Some(_opening) = hub.begin_open(id) else {
            return Ok(());
        };
        let store = fell_core::Store::open(&self.store).map_err(|err| err.to_string())?;
        self.open_reserved(hub, id, account, &store)
    }

    fn open_reserved(
        &self,
        hub: &DraftHub,
        id: &str,
        account: &AccountId,
        store: &fell_core::Store,
    ) -> Result<(), String> {
        let folder = self.root.join(id);
        let refreshed = store
            .refresh_compose_folder(id, &folder)
            .map_err(|err| err.to_string())?;
        if refreshed.is_none() {
            return Err("the draft is no longer in the mailbox".into());
        }
        let session = DraftSession::open_with_store(&folder, account.clone(), store)?;
        if hub.insert_new(session) {
            // The sync notification could have arrived after the lock was
            // read and before the session became visible to the hub.
            hub.with_session(id, |live| live.refresh_send_confirmation(store));
        } else {
            // The socket made it live meanwhile, and the copy read here has
            // just saved over its folder. Put the live text back.
            hub.with_session(id, |live| live.flush()).transpose()?;
        }
        Ok(())
    }
}

/// A draft id no other draft has: `c` and the time in nanoseconds.
fn new_draft_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("c{nanos:x}")
}

/// The app's draft hub, for windows that want to hear from it.
pub(crate) struct AgentDrafts(pub Arc<DraftHub>);

impl gpui::Global for AgentDrafts {}

/// Bring the compose window up on live draft `id`. There is one window and
/// one hub: an open window moves to this draft.
pub(crate) fn show_draft(hub: &DraftHub, id: &str) -> bool {
    hub.show(id)
}

/// Serve agent drafts from the inbox: the socket, the folder watcher, and a
/// compose window that opens, or comes to the front, whenever an agent calls
/// `request_send` or the person opens a draft. Saves still queued when Fell
/// quits are waited for.
pub(crate) fn host_agent_drafts(cx: &mut App, hub: Arc<DraftHub>) {
    cx.set_global(AgentDrafts(hub.clone()));
    // Subscribe before the socket accepts anything, and count reveals from
    // zero, so a request_send that lands while the inbox is still opening
    // is not lost.
    let (tx, mut events) = futures::channel::mpsc::unbounded::<()>();
    hub.subscribe_with(move || tx.unbounded_send(()).is_ok());
    // The watcher thread lives as long as the app.
    let _ = watch_folder(hub.clone(), None);
    let quitting = hub.clone();
    cx.on_app_quit(move |_| {
        let hub = quitting.clone();
        async move { hub.settle_all() }
    })
    .detach();
    let windows: Rc<RefCell<Option<WindowHandle<ComposeView>>>> = Rc::default();
    follow_theme(cx, windows.clone());
    let host = hub.clone();
    cx.spawn(async move |cx| {
        let mut seen = 0;
        loop {
            let reveals = host.reveal_count();
            if reveals > seen {
                seen = reveals;
                if let Some(id) = host.focused_id() {
                    reveal(cx, &host, &id, &windows);
                }
            }
            if futures::StreamExt::next(&mut events).await.is_none() {
                break;
            }
            while events.try_recv().is_ok() {}
        }
    })
    .detach();
}

/// Bring the compose window to the front on draft `id`, opening one if
/// there is none. An open window follows the hub's focus by itself.
fn reveal(
    cx: &mut AsyncApp,
    hub: &Arc<DraftHub>,
    id: &str,
    window: &Rc<RefCell<Option<WindowHandle<ComposeView>>>>,
) {
    let open = *window.borrow();
    let raised = open.is_some_and(|handle| {
        handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    });
    if !raised {
        let opened = open_compose_window(cx, hub.clone(), id);
        if opened.is_none() {
            // No window shows it, so it is not open for editing.
            hub.with_session(id, |session| session.close_editor());
            let _ = cx.update(|cx| crate::view::compose_draft_window_failed(id, cx));
        }
        *window.borrow_mut() = opened;
    }
}

/// One Omarchy theme watcher for the compose window, as the inbox has its
/// own. A theme change repaints the window if it still follows the theme.
fn follow_theme(cx: &mut App, window: Rc<RefCell<Option<WindowHandle<ComposeView>>>>) {
    let (changed, mut changes) = futures::channel::mpsc::unbounded();
    let watching = crate::theme::watch(changed);
    let settings_path = crate::settings::path();
    let defaults = crate::settings::Values::default();
    let mut previous = crate::settings::Snapshot::read(settings_path.as_deref(), defaults);
    cx.spawn(async move |cx| {
        loop {
            if watching {
                if futures::StreamExt::next(&mut changes).await.is_none() {
                    break;
                }
            } else {
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
            let path = settings_path.clone();
            let (snapshot, next, motion) = cx
                .background_executor()
                .spawn(async move {
                    let snapshot = previous.reload(path.as_deref(), defaults);
                    let values = snapshot.values;
                    (
                        snapshot,
                        crate::theme::load_with(values),
                        crate::motion::Motion::for_preference(values.reduce_motion),
                    )
                })
                .await;
            previous = snapshot;
            let Some(handle) = *window.borrow() else {
                continue;
            };
            let _ = handle.update(cx, |view, _, cx| {
                let motion_changed = view.set_motion(motion);
                let font_changed = view.set_font_size(next.ui_px);
                if view.follows_look() {
                    view.set_look(next);
                    cx.notify();
                } else if font_changed || motion_changed {
                    cx.notify();
                }
            });
        }
    })
    .detach();
}

/// A compose window on draft `id`, focused. `None` when the draft is not in
/// the hub or the window could not open.
fn open_compose_window(
    cx: &mut AsyncApp,
    hub: Arc<DraftHub>,
    id: &str,
) -> Option<WindowHandle<ComposeView>> {
    let folder = hub.with_session(id, |session| {
        session.open_for_edit();
        session.folder().to_path_buf()
    })?;
    let id = id.to_string();
    cx.update(|cx| {
        let (width, height) = compose_window_size();
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Fell compose".into()),
                    ..Default::default()
                }),
                // Its own app id, so a window manager rule can float and size the
                // compose window before it maps; the hidden desktop entry of
                // the same name keeps the Fell icon.
                app_id: Some("dev.predmore.fell.compose".into()),
                focus: true,
                show: true,
                ..Default::default()
            },
            move |window, cx| {
                cx.new(|cx| {
                    let mut compose = ComposeView::new(hub, id, folder, cx);
                    compose.set_look(crate::theme::load());
                    compose.start_on_first_edit();
                    compose.close_after_send();
                    compose.watch_live(window, cx);
                    compose
                })
            },
        );
        cx.activate(true);
        handle.ok()
    })
    .ok()
    .flatten()
}

/// The compose window's first size. `FELL_COMPOSE_WINDOW=WxH` overrides it,
/// as `FELL_WINDOW` does for the inbox.
fn compose_window_size() -> (f32, f32) {
    std::env::var("FELL_COMPOSE_WINDOW")
        .ok()
        .and_then(|raw| {
            let (width, height) = raw.split_once('x')?;
            Some((width.parse().ok()?, height.parse().ok()?))
        })
        .unwrap_or((900.0, 760.0))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Write};

    #[test]
    fn a_second_compose_invocation_delivers_a_personal_draft_to_the_inbox() {
        let Some(root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let account = me();
        let home = temp_home("external-draft", &account);
        let inbox = Arc::new(DraftHub::new());
        let mut requests = inbox_socket(inbox.clone(), None).unwrap().unwrap();
        let file = root.join("agenda.txt");
        std::fs::write(&file, b"agenda\0\n").unwrap();
        let launch = crate::compose_launch::cli(&[
            "--to=team@example.org".into(),
            "--cc=editor@example.org".into(),
            "--bcc=archive@example.org".into(),
            "--subject=Planning".into(),
            "--body=First line\nSecond line".into(),
            "--attach".into(),
            file.into(),
        ])
        .unwrap();
        assert!(
            inbox_socket(Arc::new(DraftHub::new()), Some(launch))
                .unwrap()
                .is_none()
        );
        let queued = requests.try_recv().expect("draft reached the first inbox");
        let (fields, body) = queued.input.into_fields_and_body(account.email.clone());
        let id = home
            .new_seeded(
                &inbox,
                &account.id,
                ReplyStart {
                    fields,
                    parts: ReplyParts::default(),
                },
                body,
                queued.files,
            )
            .unwrap();
        assert!(show_draft(&inbox, &id));
        let store = fell_core::Store::open(&home.store).unwrap();
        let drafts = store.unsent_compose_drafts(&account.id).unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].id, id);
        inbox
            .with_session(&id, |session| {
                let snap = session.snapshot();
                assert_eq!(snap.to[0].raw, "team@example.org");
                assert_eq!(snap.cc[0].raw, "editor@example.org");
                assert_eq!(snap.bcc[0].raw, "archive@example.org");
                assert_eq!(snap.subject, "Planning");
                assert_eq!(snap.body.trim_end(), "First line\nSecond line");
                assert_eq!(snap.attachments[0].name, "agenda.txt");
                assert_eq!(
                    std::fs::read(session.folder().join(&snap.attachments[0].path)).unwrap(),
                    b"agenda\0\n"
                );
            })
            .unwrap();
    }

    /// A drafts root and a mailbox with `account` in it, under a fresh temp dir.
    pub(crate) fn temp_home(tag: &str, account: &fell_core::Account) -> DraftHome {
        let root = std::env::temp_dir().join(format!(
            "fell-inbox-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let store = root.join("mail.db");
        fell_core::Store::open(&store)
            .unwrap()
            .upsert_account(account)
            .unwrap();
        DraftHome {
            root: root.join("drafts"),
            store,
        }
    }

    pub(crate) fn me() -> fell_core::Account {
        fell_core::Account {
            id: AccountId("mail".into()),
            display_name: "Jon".into(),
            email: "jon@example.com".into(),
            backend_kind: fell_core::BackendKind::Bridge,
        }
    }

    fn call(socket: &std::path::Path, request: Value) -> Value {
        let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        writeln!(stream, "{request}").unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    #[test]
    fn c_keeps_a_new_draft_in_the_drafts_folder_and_the_mailbox() {
        let account = me();
        let home = temp_home("new", &account);
        let hub = DraftHub::new();
        let id = home.new_draft(&hub, &account.id, &account.email).unwrap();
        let (folder, from, owner) = hub
            .with_session(&id, |session| {
                (
                    session.folder().to_path_buf(),
                    session.fields().from.clone(),
                    session.owner().to_string(),
                )
            })
            .expect("the new draft is live");
        assert!(folder.starts_with(&home.root));
        assert!(folder.join("draft.md").is_file());
        assert_eq!(from, "jon@example.com");
        assert!(owner.is_empty(), "the person's own draft");
        let listed = fell_core::Store::open(&home.store)
            .unwrap()
            .unsent_compose_drafts(&account.id)
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        // Reopening reads the mailbox's copy, even with the folder gone.
        drop(hub);
        std::fs::remove_dir_all(home.root.join(&id)).unwrap();
        let hub = DraftHub::new();
        home.open(&hub, &id, &account.id).unwrap();
        assert!(hub.with_session(&id, |_| ()).is_some());
        assert!(home.open(&hub, "c0", &account.id).is_err());
    }

    #[test]
    fn undo_discard_restores_live_text_files_and_agent_review_in_one_action() {
        use fell_core::compose::{ResolvedFile, StoredCompose};

        let account = me();
        let home = temp_home("discard-review", &account);
        let store = fell_core::Store::open(&home.store).unwrap();
        store
            .set_agent_grant(
                &account.id,
                "atlas",
                fell_core::AgentGrant::enabled(false, false, true),
            )
            .unwrap();
        let hub = DraftHub::new();
        let personal = home.new_draft(&hub, &account.id, &account.email).unwrap();
        hub.with_session(&personal, |session| {
            session.set_subject_from_user("Unsaved edit".into());
            session.set_body_from_user("A newer body\n".into());
            session.arm_outbound(0);
        });
        let review = store
            .create_review_draft(
                fell_core::ReviewDraftInput {
                    account_id: account.id.clone(),
                    to: vec!["ada@example.com".into()],
                    cc: Vec::new(),
                    bcc: vec!["robin@example.com".into()],
                    subject: "Review".into(),
                    body: "Original\n".into(),
                    in_reply_to: Some(fell_core::MessageId::new("parent@example.com")),
                    thread_id: None,
                },
                "agent:atlas",
            )
            .unwrap();
        let agent = format!("r{}", review.draft.id);
        store
            .save_compose(&StoredCompose {
                id: agent.clone(),
                account_id: account.id.clone(),
                fields: DraftFields {
                    from: account.email.clone(),
                    to: review.draft.to,
                    bcc: review.draft.bcc,
                    subject: review.draft.subject,
                    in_reply_to: Some("<parent@example.com>".into()),
                    ..Default::default()
                },
                body: review.draft.body,
                files: vec![ResolvedFile {
                    path: "files/map.txt".into(),
                    display_name: "Map.txt".into(),
                    bytes: b"synthetic map".to_vec(),
                    inline: false,
                }],
                identities: vec![account.email.clone()],
                owner: "agent:atlas".into(),
                reply: ReplyParts {
                    references: vec!["<parent@example.com>".into()],
                    quote_text: "Original message".into(),
                    quote_html: "<p>Original message</p>".into(),
                    reply_to_unverified: true,
                },
            })
            .unwrap();
        home.open(&hub, &agent, &account.id).unwrap();
        hub.with_session(&agent, |session| {
            session.replace_body("Updated review\n").unwrap();
            assert!(session.snapshot().unreviewed > 0);
        });
        let ids = [personal.clone(), agent.clone()];
        hub.discard(&store, &account.id, &ids).unwrap();
        assert_eq!(hub.take_closed(), ["2 drafts discarded  ·  u"]);
        assert!(store.unsent_compose_drafts(&account.id).unwrap().is_empty());
        assert!(store.list_review_drafts(&account.id).unwrap().is_empty());
        assert!(store.undo_actor(&account.id, "agent:atlas").is_err());
        assert!(!hub.show(&agent));
        assert!(home.open(&hub, &agent, &account.id).is_err());
        // Undo works even if the runtime directory went away.
        std::fs::remove_dir_all(&home.root).unwrap();
        let hub = DraftHub::new();
        let undo = store.undo(&account.id).unwrap();
        assert_eq!(undo.kind, fell_core::ActionKind::DiscardDrafts);
        assert_eq!(undo.summary, "2 drafts restored");
        assert!(store.undo(&account.id).is_err());
        assert_eq!(store.unsent_compose_drafts(&account.id).unwrap().len(), 2);
        assert_eq!(store.list_review_drafts(&account.id).unwrap().len(), 1);
        let restored = store.load_compose(&personal).unwrap().unwrap();
        assert_eq!(restored.fields.subject, "Unsaved edit");
        assert_eq!(restored.body, "A newer body\n");
        home.open(&hub, &agent, &account.id).unwrap();
        hub.with_session(&agent, |session| {
            assert_eq!(session.body(), "Updated review\n");
            assert_eq!(session.owner(), "agent:atlas");
            assert_eq!(session.fields().bcc, ["robin@example.com"]);
            assert_eq!(session.reply_parts().quote_text, "Original message");
            assert!(session.reply_parts().reply_to_unverified);
            assert!(session.snapshot().unreviewed > 0);
        });
        assert_eq!(
            std::fs::read(home.root.join(agent).join("files/map.txt")).unwrap(),
            b"synthetic map"
        );
    }

    #[test]
    fn discard_refuses_live_and_confirmed_sends_and_undo_keeps_uncertainty() {
        use fell_core::compose::SendLock;

        let account = me();
        let home = temp_home("discard-send", &account);
        let store = fell_core::Store::open(&home.store).unwrap();
        let hub = DraftHub::new();
        let id = home.new_draft(&hub, &account.id, &account.email).unwrap();
        let other = home.new_draft(&hub, &account.id, &account.email).unwrap();
        let message = fell_core::MessageId::new("submission@example.com");
        hub.with_session(&id, |session| session.begin_send(&message))
            .unwrap()
            .unwrap();
        let selection = [other.clone(), id.clone()];
        let other_hub = DraftHub::new();
        for trying in [&hub, &other_hub] {
            let err = trying.discard(&store, &account.id, &selection).unwrap_err();
            assert_eq!(err.to_string(), "Retry after send");
            assert!(store.load_compose(&other).unwrap().is_some());
        }
        hub.with_session(&id, |session| session.end_send(Some(SendLock::MaybeSent)));
        let mut stale =
            DraftSession::open_with_store(&home.root.join(&id), account.id.clone(), &store)
                .unwrap();
        hub.discard_open(&id).unwrap();
        assert!(stale.release_maybe_sent().is_err());
        assert!(store.load_compose(&id).unwrap().is_none());
        store.undo(&account.id).unwrap();
        home.open(&hub, &id, &account.id).unwrap();
        hub.with_session(&id, |session| {
            assert_eq!(session.send_lock(), Some(SendLock::MaybeSent));
            assert!(session.begin_send(&message).is_err());
            session.end_send(Some(SendLock::Sent));
        });
        assert!(hub.discard(&store, &account.id, &selection).is_err());
        assert!(other_hub.discard(&store, &account.id, &selection).is_err());
        assert_eq!(
            store.unsent_compose_drafts(&account.id).unwrap()[0].id,
            other
        );
        assert!(store.undo(&account.id).is_err());
    }

    fn parent_message(account: &fell_core::Account) -> fell_core::Message {
        let id = fell_core::MessageId::new("parent@x");
        fell_core::Message {
            account_id: account.id.clone(),
            headers: fell_core::MessageHeaders {
                message_id: id.clone(),
                proton_conversation_id: None,
                in_reply_to: Some(fell_core::MessageId::new("root@x")),
                references: vec![fell_core::MessageId::new("root@x")],
                from: fell_core::Address {
                    name: Some("Ada".into()),
                    email: "ada@x".into(),
                },
                subject: "Re: Plans".into(),
                date_unix: 1_700_000_000,
            },
            remote_id: fell_core::RemoteId("r".into()),
            thread_id: fell_core::ThreadId::from_message_id(&id),
            snippet: "See you".into(),
            body_text: Some("See you at noon.".into()),
            unread: false,
            folders: vec!["INBOX".into()],
            labels: Vec::new(),
            arrived_via: Vec::new(),
            to: vec![fell_core::Address {
                name: None,
                email: account.email.clone(),
            }],
            cc: vec![fell_core::Address {
                name: Some("Sam".into()),
                email: "sam@x".into(),
            }],
            bcc: Vec::new(),
            starred: false,
            has_attachment: false,
            archived: false,
            snooze_until: None,
            list_id: None,
            reply_to: Some(Vec::new()),
            attachment_names: Vec::new(),
            parts: None,
        }
    }

    #[test]
    fn a_reply_keeps_its_quote_and_references_when_reopened() {
        let account = me();
        let home = temp_home("reply", &account);
        let parent = parent_message(&account);
        let start = fell_core::compose::start_reply(
            fell_core::compose::ReplyKind::ReplyAll,
            &parent,
            &account.email,
            std::slice::from_ref(&account.email),
            &fell_core::FolderRoles::default(),
        );
        let hub = DraftHub::new();
        let id = home.new_reply(&hub, &account.id, start).unwrap();
        let check = |hub: &DraftHub| {
            hub.with_session(&id, |session| {
                let snap = session.snapshot();
                let to: Vec<String> = snap.to.iter().map(|item| item.raw.clone()).collect();
                let cc: Vec<String> = snap.cc.iter().map(|item| item.raw.clone()).collect();
                assert_eq!(to, vec!["Ada <ada@x>"]);
                assert_eq!(cc, vec!["Sam <sam@x>"]);
                assert_eq!(snap.subject, "Re: Plans");
                assert_eq!(snap.in_reply_to.as_deref(), Some("parent@x"));
                let quoted = snap.quoted.expect("the quote shows below the draft");
                assert!(quoted.contains("See you at noon."), "{quoted}");
                assert_eq!(
                    session.reply_parts().references,
                    vec!["root@x".to_string(), "parent@x".to_string()]
                );
            })
            .expect("the reply is live")
        };
        check(&hub);
        hub.settle_all();
        drop(hub);
        // gd, then Enter: the mailbox's copy, quote and chain included.
        let hub = DraftHub::new();
        home.open(&hub, &id, &account.id).unwrap();
        check(&hub);
    }

    #[test]
    fn a_reply_started_before_verification_keeps_its_recipient_hold_after_sync() {
        let account = me();
        let home = temp_home("reply-unverified", &account);
        let mut parent = parent_message(&account);
        parent.reply_to = None;
        let start = fell_core::compose::start_reply(
            fell_core::compose::ReplyKind::Reply,
            &parent,
            &account.email,
            std::slice::from_ref(&account.email),
            &fell_core::FolderRoles::default(),
        );

        // Backfill can finish between reading the parent and opening compose.
        parent.reply_to = Some(vec![fell_core::Address {
            name: None,
            email: "plans@lists.x".into(),
        }]);
        fell_core::Store::open(&home.store)
            .unwrap()
            .upsert_message(&parent)
            .unwrap();

        let check = |hub: &DraftHub, id: &str| {
            hub.with_session(id, |session| {
                session.mark_seen();
                let snap = session.snapshot();
                assert!(snap.to.is_empty());
                assert!(snap.reply_to_unverified);
                assert_eq!(
                    snap.reply_context.unwrap().reply_to.unwrap(),
                    ["plans@lists.x"]
                );
                assert_eq!(
                    snap.send_block.as_deref(),
                    Some("add at least one recipient")
                );
            })
            .unwrap();
        };
        let hub = DraftHub::new();
        let id = home.new_reply(&hub, &account.id, start).unwrap();
        check(&hub, &id);
        hub.settle_all();
        drop(hub);

        let reopened = DraftHub::new();
        home.open(&reopened, &id, &account.id).unwrap();
        check(&reopened, &id);
    }

    #[test]
    fn failing_to_open_a_suggested_reply_removes_the_draft_and_restores_the_suggestion() {
        use fell_core::{AgentGrant, notes::NoteDecision};

        let account = me();
        let home = temp_home("suggested-reply-failure", &account);
        let draft = "failed-reply";
        let folder = home.root.join(draft);
        let control = fell_core::compose::control_dir(&folder);
        // Materialization writes files, then rejects the unexpected thread copy.
        std::fs::create_dir_all(folder.join("thread")).unwrap();
        std::fs::create_dir_all(&control).unwrap();
        let store = fell_core::Store::open(&home.store).unwrap();
        let parent = parent_message(&account);
        let thread = store.upsert_message(&parent).unwrap();
        store
            .set_agent_grant(
                &account.id,
                "reviewer",
                AgentGrant::enabled(false, false, false).with_annotation(true),
            )
            .unwrap();
        let note = store
            .annotate_thread(
                &account.id,
                &thread,
                "reviewer",
                None,
                &["reply".into()],
                "Reviewed",
            )
            .unwrap();
        let request = crate::model::NoteRequest {
            thread: thread.0.clone(),
            id: note.id,
            decision: NoteDecision::Accept(0),
            reply: true,
        };
        let start = fell_core::compose::start_reply(
            fell_core::compose::ReplyKind::Reply,
            &parent,
            &account.email,
            std::slice::from_ref(&account.email),
            &fell_core::FolderRoles::default(),
        );
        let error = home
            .open_suggested_reply(&DraftHub::new(), &account.id, start, &request, draft)
            .unwrap_err();
        assert!(error.contains("draft folder must not contain a thread copy"));
        assert!(store.load_compose(draft).unwrap().is_none());
        assert!(!folder.exists());
        assert!(!control.exists());
        let notes = store.thread_notes(&account.id, &thread).unwrap();
        assert_eq!(
            notes[0].suggestions[0].action,
            fell_core::notes::Suggestion::Reply
        );
        assert_eq!(
            store.undo(&account.id).unwrap().kind,
            fell_core::ActionKind::AnnotateThread
        );
        assert!(store.thread_notes(&account.id, &thread).unwrap().is_empty());
    }

    #[test]
    fn a_sent_reply_opens_normally_with_an_unverified_original_header() {
        let account = me();
        let home = temp_home("sent-reply", &account);
        let mut parent = parent_message(&account);
        parent.folders = vec!["Sent".into()];
        parent.to = vec![fell_core::Address {
            name: None,
            email: "ada@x".into(),
        }];
        parent.reply_to = None;
        fell_core::Store::open(&home.store)
            .unwrap()
            .upsert_message(&parent)
            .unwrap();
        let roles = fell_core::FolderRoles::from_names(&parent.folders);
        let start = fell_core::compose::start_reply(
            fell_core::compose::ReplyKind::Reply,
            &parent,
            &account.email,
            std::slice::from_ref(&account.email),
            &roles,
        );
        let hub = DraftHub::new();
        let id = home.new_reply(&hub, &account.id, start).unwrap();
        hub.with_session(&id, |session| {
            let snap = session.snapshot();
            assert_eq!(snap.to[0].email, "ada@x");
            assert!(!snap.reply_to_unverified);
            assert_eq!(snap.reply_context.unwrap().reply_to, None);
        })
        .unwrap();
    }

    #[test]
    fn a_forward_keeps_carried_files_and_distinct_paths_when_reopened() {
        let account = me();
        let home = temp_home("forward-files", &account);
        let mut parent = parent_message(&account);
        parent.has_attachment = true;
        parent.attachment_names = vec!["missing.pdf".into()];
        let start = fell_core::compose::start_reply(
            fell_core::compose::ReplyKind::Forward,
            &parent,
            &account.email,
            std::slice::from_ref(&account.email),
            &fell_core::FolderRoles::default(),
        );
        let files = [b"one", b"two"]
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| ResolvedFile {
                path: format!("attachments/{index}/notes.pdf"),
                display_name: "notes.pdf".into(),
                bytes: bytes.to_vec(),
                inline: false,
            })
            .collect::<Vec<_>>();
        let hub = DraftHub::new();
        let id = home
            .new_reply_with_files(&hub, &account.id, start, files.clone())
            .unwrap();
        hub.settle_all();
        drop(hub);
        std::fs::remove_dir_all(home.root.join(&id)).unwrap();
        let hub = DraftHub::new();
        home.open(&hub, &id, &account.id).unwrap();
        hub.with_session(&id, |session| {
            let snap = session.snapshot();
            assert_eq!(snap.attachments.len(), 2);
            assert!(snap.attachments.iter().all(|file| file.name == "notes.pdf"));
            assert_eq!(
                snap.forwarded.unwrap().not_forwarded.as_deref(),
                Some("missing.pdf")
            );
            for file in files {
                assert_eq!(
                    std::fs::read(session.folder().join(file.path)).unwrap(),
                    file.bytes
                );
            }
        })
        .unwrap();
    }

    #[test]
    fn a_reply_started_without_its_chain_gets_it_from_the_parent() {
        let account = me();
        let home = temp_home("agent-reply", &account);
        let store = fell_core::Store::open(&home.store).unwrap();
        // An agent's reply names only its parent, which is not stored yet.
        store
            .save_compose(&fell_core::compose::StoredCompose {
                id: "c1".into(),
                account_id: account.id.clone(),
                fields: DraftFields {
                    from: account.email.clone(),
                    to: vec!["ada@x".into()],
                    subject: "Re: Plans".into(),
                    in_reply_to: Some("parent@x".into()),
                    ..DraftFields::default()
                },
                body: "Yes.\n".into(),
                files: Vec::new(),
                identities: vec![account.email.clone()],
                owner: String::new(),
                reply: Default::default(),
            })
            .unwrap();
        drop(store);
        // Opened before the parent syncs: no chain is made up and kept.
        let hub = DraftHub::new();
        home.open(&hub, "c1", &account.id).unwrap();
        let references = hub
            .with_session("c1", |session| session.reply_parts().references.clone())
            .unwrap();
        assert!(references.is_empty(), "{references:?}");
        drop(hub);
        // Once the parent is in the mailbox, the next open builds it.
        let store = fell_core::Store::open(&home.store).unwrap();
        store.upsert_message(&parent_message(&account)).unwrap();
        drop(store);
        let hub = DraftHub::new();
        home.open(&hub, "c1", &account.id).unwrap();
        let references = hub
            .with_session("c1", |session| session.reply_parts().references.clone())
            .unwrap();
        assert_eq!(references, vec!["root@x", "parent@x"]);
    }

    #[test]
    fn an_agent_reply_in_a_long_old_thread_keeps_its_root() {
        // Synced before In-Reply-To was appended: [mid, root, m0..m19].
        let account = me();
        let home = temp_home("agent-long", &account);
        let store = fell_core::Store::open(&home.store).unwrap();
        let mut parent = parent_message(&account);
        parent.headers.in_reply_to = Some(fell_core::MessageId::new("mid@x"));
        let mut chain = vec![
            fell_core::MessageId::new("mid@x"),
            fell_core::MessageId::new("root@x"),
        ];
        chain.extend((0..20).map(|n| fell_core::MessageId::new(format!("m{n}@x"))));
        parent.headers.references = chain;
        store.upsert_message(&parent).unwrap();
        store
            .save_compose(&fell_core::compose::StoredCompose {
                id: "c1".into(),
                account_id: account.id.clone(),
                fields: DraftFields {
                    from: account.email.clone(),
                    to: vec!["ada@x".into()],
                    subject: "Re: Plans".into(),
                    in_reply_to: Some("parent@x".into()),
                    ..DraftFields::default()
                },
                body: "Yes.\n".into(),
                files: Vec::new(),
                identities: vec![account.email.clone()],
                owner: String::new(),
                reply: Default::default(),
            })
            .unwrap();
        drop(store);
        let hub = DraftHub::new();
        home.open(&hub, "c1", &account.id).unwrap();
        let references = hub
            .with_session("c1", |session| session.reply_parts().references.clone())
            .unwrap();
        assert_eq!(references.len(), fell_core::compose::MAX_REFERENCES);
        assert_eq!(&references[..2], ["mid@x", "root@x"]);
        assert_eq!(references.last().map(String::as_str), Some("parent@x"));
    }

    #[test]
    fn a_new_draft_whose_folder_cannot_be_made_leaves_no_row() {
        let account = me();
        let home = temp_home("no-folder", &account);
        // The drafts root is a file, so no draft folder can go under it.
        std::fs::write(&home.root, "not a folder").unwrap();
        let hub = DraftHub::new();
        assert!(home.new_draft(&hub, &account.id, &account.email).is_err());
        let listed = fell_core::Store::open(&home.store)
            .unwrap()
            .unsent_compose_drafts(&account.id)
            .unwrap();
        assert!(listed.is_empty(), "{listed:?}");
        assert!(hub.snapshot_ids().is_empty());
    }

    /// Write `text` into draft `id`'s `draft.md` as something outside Fell.
    fn write_outside(home: &DraftHome, id: &str, text: &str) -> std::path::PathBuf {
        let file = home.root.join(id).join("draft.md");
        let mut current = std::fs::read_to_string(&file).unwrap();
        current.push_str(text);
        std::fs::write(&file, &current).unwrap();
        file
    }

    #[test]
    fn opening_a_live_draft_again_leaves_its_folder_and_typing_alone() {
        let account = me();
        let home = temp_home("open-live", &account);
        let hub = DraftHub::new();
        let id = home.new_draft(&hub, &account.id, &account.email).unwrap();
        hub.with_session(&id, |session| {
            session.set_body_from_user("typed, not saved yet\n".into());
            session.arm_outbound(u64::MAX);
        });
        // Not the mailbox's copy: an open that restored the folder would
        // write over it, and the watcher would merge that into the draft.
        let file = write_outside(&home, &id, "outside\n");
        let before = std::fs::read_to_string(&file).unwrap();
        home.open(&hub, &id, &account.id).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        let body = hub
            .with_session(&id, |session| session.body().to_string())
            .unwrap();
        assert_eq!(body, "typed, not saved yet\n");
    }

    #[test]
    fn a_second_open_while_the_first_reads_the_draft_writes_nothing() {
        let account = me();
        let home = temp_home("open-twice", &account);
        let hub = DraftHub::new();
        let id = home.new_draft(&hub, &account.id, &account.email).unwrap();
        hub.settle_all();
        let cold = DraftHub::new();
        let file = write_outside(&home, &id, "outside\n");
        let before = std::fs::read_to_string(&file).unwrap();
        // Another Enter's open is still reading this draft.
        let first = cold.begin_open(&id).expect("nothing opens it yet");
        home.open(&cold, &id, &account.id).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
        assert!(
            cold.with_session(&id, |_| ()).is_none(),
            "the first open adds it"
        );
        drop(first);
    }

    #[test]
    fn a_cold_open_keeps_fells_own_save_that_the_mailbox_missed() {
        let account = me();
        let home = temp_home("own-save", &account);
        let hub = DraftHub::new();
        let id = home.new_draft(&hub, &account.id, &account.email).unwrap();
        // A save whose mailbox row did not land: draft.md is newer than it.
        let missing = home.store.with_file_name("gone.db");
        hub.with_session(&id, |session| {
            session.bind_store(&missing);
            session.set_body_from_user("saved to draft.md only\n".into());
            session.flush().unwrap();
        });
        hub.settle_all();
        drop(hub);
        let row = fell_core::Store::open(&home.store)
            .unwrap()
            .load_compose(&id)
            .unwrap()
            .unwrap();
        assert!(row.body.is_empty(), "the row is the older copy");
        let hub = DraftHub::new();
        home.open(&hub, &id, &account.id).unwrap();
        let body = hub
            .with_session(&id, |session| session.body().to_string())
            .unwrap();
        assert_eq!(body, "saved to draft.md only\n");
        // A change Fell did not save is not kept: the row wins.
        drop(hub);
        let file = write_outside(&home, &id, "outside\n");
        let hub = DraftHub::new();
        home.open(&hub, &id, &account.id).unwrap();
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("outside"), "{text}");
    }

    #[test]
    fn a_draft_from_c_answers_on_the_socket_and_request_send_only_focuses_it() {
        let account = me();
        let home = temp_home("socket", &account);
        let hub = Arc::new(DraftHub::new());
        // A unix socket path must fit in 108 bytes, and TMPDIR can be deep
        // (a worktree's target dir), so bind in a short private dir.
        let socket_dir = tempfile::Builder::new()
            .prefix("fell-s")
            .tempdir_in("/tmp")
            .unwrap();
        let socket = socket_dir.path().join("live.sock");
        serve_socket_at(hub.clone(), &socket).unwrap();
        let id = home.new_draft(&hub, &account.id, &account.email).unwrap();
        assert!(show_draft(&hub, &id));
        assert_eq!(hub.focused_id().as_deref(), Some(id.as_str()));
        let reveals = hub.reveal_count();
        // The compose window is up, and the person has typed, not yet saved.
        hub.note_shown(&id);
        hub.with_session(&id, |session| {
            session.set_body_from_user("typed in the editor\n".into());
            session.arm_outbound(u64::MAX);
        });
        // No agent was given this draft, so none may read it.
        let refused = call(
            &socket,
            json!({ "id": 1, "op": "get_draft", "draft_id": id, "agent": "agent:bot" }),
        );
        assert_eq!(refused["ok"], false, "{refused}");
        assert_eq!(refused["transmitted"], false);
        // Once it is given to one, that agent reads the live draft, unsaved
        // typing included.
        fell_core::Store::open(&home.store)
            .unwrap()
            .set_agent_grant(
                &account.id,
                "bot",
                fell_core::AgentGrant::enabled(false, false, true),
            )
            .unwrap();
        hub.with_session(&id, |session| session.set_owner("agent:bot"));
        let draft = call(
            &socket,
            json!({ "id": 2, "op": "get_draft", "draft_id": id, "agent": "agent:bot" }),
        );
        assert_eq!(draft["ok"], true, "{draft}");
        assert_eq!(draft["body"], "typed in the editor\n");
        let asked = call(
            &socket,
            json!({ "id": 3, "op": "request_send", "draft_id": id, "agent": "agent:bot" }),
        );
        assert_eq!(asked["ok"], true, "{asked}");
        assert_eq!(asked["transmitted"], false);
        assert_eq!(asked["focused"], true);
        assert_eq!(hub.focused_id().as_deref(), Some(id.as_str()));
        assert_eq!(hub.reveal_count(), reveals + 1);
        let lock = hub.with_session(&id, |session| session.send_lock());
        assert_eq!(lock, Some(None), "nothing was sent or claimed");
    }
}
