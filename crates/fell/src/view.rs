//! Two-pane inbox. The window reads the local store and never talks to IMAP.

use std::time::{Instant, SystemTime, UNIX_EPOCH};

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fell_core::{
    AccountConfig, AccountId, DropReason, FolderRole, FolderRoles, FolderSpan, ListKey, ThreadId,
};
use futures::StreamExt;
use gpui::{
    App, Application, Bounds, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, Keystroke, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Render,
    ScrollHandle, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled,
    TitlebarOptions, UniformListScrollHandle, Window, WindowBounds, WindowOptions, div, point,
    prelude::*, px, rgb, size, uniform_list,
};

use fell_core::compose::{DraftSummary, ReplyKind};

use crate::draft_host::DraftHome;
use crate::frame_log::{FrameLog, SharedFrameLog};
use crate::inbox_motion::{InboxMotion, PaneEntry, StatusMotion};
use crate::keymap::KeyId;
use crate::mailbox;
use crate::model::{
    DRAFT_NOT_THREAD, Fetch, Overlay, QueryFault, Session, Step, SyncKind, Toast, row_key,
};
use crate::motion::{self, Glide, ListShift, Motion, RowMotion};
use crate::present::{self, ConvMessage, NameSpan, Pane, ThreadRow};
use crate::store_actor::StoreClient;
use crate::sync_worker::{self, SyncHandle};
use crate::text_cut;
use crate::text_render::BodyBlock;
use crate::theme::{self, Theme};

pub fn run() {
    Application::new().run(|cx: &mut App| {
        // Agents' drafts open in a compose window from here; see draft_host.
        let _hub = crate::draft_host::host_agent_drafts(cx);
        let (width, height) = window_size();
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let view = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Fell".into()),
                    ..Default::default()
                }),
                app_id: Some("dev.predmore.fell".into()),
                focus: true,
                show: true,
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(Inbox::new);
                let focus = view.read(cx).focus.clone();
                window.focus(&focus);
                view
            },
        );
        if view.is_err() {
            eprintln!("fell: could not open a window");
        }
        cx.activate(true);
    });
}

/// Why the folder is being reread. Requests made while a read is in flight
/// merge into one, so a sync refresh can never drop what the user asked for.
#[derive(Clone, Debug, Default)]
struct ReadIntent {
    /// The folder on screen changed. A failure goes back to `list_folder`; a
    /// selected thread missing from the new folder gives way to the first row.
    switch: bool,
    /// Archive, move, or undo wrote to the store. Scroll to the selection
    /// instead of holding the viewport still.
    changed: bool,
    /// Undo: select this thread when it is back in the folder, loading the
    /// rows around it if they are not loaded.
    select: Option<ListKey>,
    /// Rows the list asked for beyond the ones it holds.
    fetch: Fetch,
}

impl ReadIntent {
    fn merge(mut self, later: ReadIntent) -> ReadIntent {
        self.switch |= later.switch;
        self.changed |= later.changed;
        self.select = later.select.or(self.select);
        self.fetch = self.fetch.merge(later.fetch);
        self
    }

    /// Only sync asked: nobody is waiting, so the read can use the
    /// background lane.
    fn is_refresh(&self) -> bool {
        !self.switch && !self.changed && self.select.is_none() && self.fetch.is_empty()
    }

    /// The selected row should stay where it is on screen, because nothing
    /// asked to move to another one.
    fn holds_viewport(&self) -> bool {
        !self.switch && !self.changed && self.select.is_none()
    }
}

/// One folder read: a window of rows, its edges, the unread count, and the
/// folder names for the picker.
struct ListRead {
    rows: Vec<ThreadRow>,
    more_before: bool,
    more_after: bool,
    unread: usize,
    names: Vec<String>,
    /// Where the selected thread sits now, when it is still in the folder
    /// but not among `rows`: new mail moved it outside the window.
    followed: Option<ListKey>,
    /// In Drafts, the person's compose drafts, shown above the synced rows
    /// when the window reaches the top of the folder.
    drafts: Vec<DraftSummary>,
    roles: FolderRoles,
}

type ThreadsResult = Result<ListRead, String>;

/// What the inbox says while a reply or forward's window comes up:
/// "reply all · Jon".
/// One line for moves the server will never get. The change stays in Fell,
/// and no key would fix it, so none is shown.
fn not_sent_notice(session: &Session, not_sent: &sync_worker::NotSent) -> String {
    let folder = session.folder_text(&not_sent.folder);
    let messages = if not_sent.count == 1 {
        "1 message".to_string()
    } else {
        format!("{} messages", not_sent.count)
    };
    match not_sent.reason {
        DropReason::FolderReset => {
            format!("{folder} was reset on the server; {messages} moved only in Fell")
        }
        DropReason::Refused => format!("The server refused to move {messages}"),
        DropReason::NotFound => format!("{messages} not found in {folder} on the server"),
        DropReason::NoMove => "This server can't move mail; moves stay in Fell".to_string(),
    }
}

fn reply_notice(kind: ReplyKind, sender: &str) -> String {
    format!("{}{sender}", reply_notice_prefix(kind))
}

fn reply_notice_prefix(kind: ReplyKind) -> &'static str {
    match kind {
        ReplyKind::Reply => "reply · ",
        ReplyKind::ReplyAll => "reply all · ",
        ReplyKind::Forward => "forward · ",
    }
}

/// Height of one thread row. Rows never change height, so the list can
/// place, scroll to and animate any row from its index.
const ROW_PX: f32 = 68.0;
/// A glide further than this (about four rows) eases on the gentler
/// curve, so one late frame cannot carry it half the way ([`Glide::gentle_beyond`]).
const LONG_GLIDE_PX: f32 = 4.0 * ROW_PX;

struct Inbox {
    focus: FocusHandle,
    /// The mailbox connection's own thread. The window never calls SQLite.
    store: StoreClient,
    account_id: AccountId,
    session: Session,
    theme: Theme,
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    palette_scroll: ScrollHandle,
    folder_scroll: ScrollHandle,
    split: f32,
    dragging: bool,
    open_thread: Option<String>,
    /// The open thread's subject when its messages arrived, for when its
    /// row has left the list (an archive) before the next thread's arrive.
    open_subject: String,
    /// Thread `m` just moved. `u` selects it again when it returns to this folder.
    moved_thread: Option<ListKey>,
    /// Background sync for the account file. `None` for the demo mailbox.
    sync: Option<SyncHandle>,
    /// The folder the rows on screen came from. `session.folder` changes on
    /// the key press; this changes when that folder's read lands. Archive and
    /// move act on this one, the list the reader sees.
    list_folder: String,
    /// One folder read at a time is on the store thread.
    read_in_flight: bool,
    /// The read in flight was only a sync refresh, on the background lane.
    refresh_in_flight: bool,
    /// Bumped by every folder read. A read the user superseded is dropped.
    read_gen: u64,
    /// Asked for while a read was in flight. Issued when it lands.
    read_wanted: Option<ReadIntent>,
    /// Threads taken out of the list whose archive or move has not landed.
    /// A read that started before the write cannot paint them back.
    pending_removals: std::collections::HashSet<String>,
    /// Bumped by every conversation read. The store thread skips a read that
    /// is already stale, so holding `j` does not build a backlog.
    conv_gen: Arc<AtomicU64>,
    /// Where `c` keeps a new draft. `None` when the mailbox did not open.
    draft_home: Option<DraftHome>,
    /// The compose drafts at the top of the Drafts list, as last read.
    drafts: Vec<DraftSummary>,
    /// Transitions running in this window. See [`crate::motion`].
    motion: InboxMotion,
    /// The time this frame draws at, so every part of it agrees.
    frame_now: Instant,
    /// `FELL_FRAME_LOG`: frame times, for the key-to-frame budget.
    frames: Option<SharedFrameLog>,
    /// The folder's last rows and their thread, read ahead for a `G` and
    /// drawn unseen once ([`Self::warm_layer`]).
    warm: Option<Warm>,
    /// The folder the last rows were read ahead for.
    warmed_folder: Option<String>,
}

/// What a `G` will show, read ahead: the folder's last rows and the
/// conversation of the last one.
struct Warm {
    folder: String,
    rows: Vec<ThreadRow>,
    subject: String,
    messages: Vec<ConvMessage>,
    /// Rows drawn so far. They are drawn [`WARM_ROWS`] a frame, then the
    /// conversation in a frame of its own, so no frame does much of it.
    rows_drawn: usize,
}

/// Rows a frame draws ahead ([`Inbox::warm_layer`]): a couple of
/// milliseconds.
const WARM_ROWS: usize = 2;

impl Inbox {
    fn new(cx: &mut Context<Self>) -> Self {
        let startup = mailbox::startup();
        let theme = theme::load();
        let config = startup.config.clone();
        let path = startup.path.clone();
        let draft_home = startup.path.clone().ok().map(DraftHome::real);
        let (store, ready) = StoreClient::spawn({
            let config = config.clone();
            move || mailbox::open_store(path, config.as_ref())
        });
        let (name, email) = match &config {
            Some(config) => {
                let account = config.account();
                (account.display_name, account.email)
            }
            None => (String::new(), String::new()),
        };
        let initial = if config.is_some() {
            SyncKind::Connecting
        } else {
            SyncKind::LocalOffline
        };
        let mut session = Session::new(name, email, initial, startup.keymap);
        session.loading = true;
        if let Some(error) = startup.error {
            session.record_startup_error(error);
        }
        let inbox = Self {
            focus: cx.focus_handle(),
            account_id: config
                .as_ref()
                .map(|config| config.account().id)
                .unwrap_or_else(|| AccountId(String::new())),
            session,
            store,
            theme,
            list_scroll: UniformListScrollHandle::new(),
            conv_scroll: ScrollHandle::new(),
            palette_scroll: ScrollHandle::new(),
            folder_scroll: ScrollHandle::new(),
            split: startup.split,
            dragging: false,
            open_thread: None,
            open_subject: String::new(),
            moved_thread: None,
            sync: None,
            list_folder: FolderRole::Inbox.default_name().into(),
            read_in_flight: false,
            refresh_in_flight: false,
            read_gen: 0,
            read_wanted: None,
            pending_removals: std::collections::HashSet::new(),
            conv_gen: Arc::new(AtomicU64::new(0)),
            draft_home,
            drafts: Vec::new(),
            motion: InboxMotion::new(Motion::from_env()),
            frame_now: Instant::now(),
            frames: FrameLog::from_env(),
            warm: None,
            warmed_folder: None,
        };
        let sync_with = config.zip(startup.path.ok());
        cx.spawn(async move |this, cx| {
            let Ok(opened) = ready.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| inbox.on_store_ready(opened, sync_with, cx));
        })
        .detach();
        inbox.watch_theme(cx);
        inbox.watch_reviews(cx);
        inbox.watch_dates(cx);
        inbox
    }

    fn on_store_ready(
        &mut self,
        opened: mailbox::Opened,
        sync_with: Option<(AccountConfig, std::path::PathBuf)>,
        cx: &mut Context<Self>,
    ) {
        self.account_id = opened.account.id.clone();
        self.session.account_name = opened.account.display_name.clone();
        self.session.account_email = opened.account.email.clone();
        if let Some(error) = opened.error {
            let joined = match self.session.startup_error.take() {
                Some(earlier) => format!("{earlier}\n{error}"),
                None => error,
            };
            self.session.record_startup_error(joined);
        }
        let store_failed = opened.sync == SyncKind::StoreError;
        if store_failed {
            // The window reads a stand-in; a draft kept there would be lost.
            self.draft_home = None;
        }
        self.session.sync = opened.sync;
        // Sync opens its own connection only after this one finished opening
        // and migrating the file, so the two never race for the first lock.
        if !store_failed && let Some((config, path)) = sync_with {
            let (handle, events) = sync_worker::start(config, path);
            self.sync = Some(handle);
            self.watch_sync(events, cx);
        }
        self.request_threads(ReadIntent::default(), cx);
        cx.notify();
    }

    /// Apply sync status as soon as it arrives. The sync thread writes the
    /// store; the window only asks its store thread to reread the folder.
    fn watch_sync(
        &self,
        mut events: futures::channel::mpsc::UnboundedReceiver<sync_worker::SyncEvent>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                let alive = this
                    .update(cx, |inbox, cx| {
                        let kind = match event {
                            sync_worker::SyncEvent::Status(kind) => kind,
                            sync_worker::SyncEvent::NotSent(not_sent) => {
                                let text = not_sent_notice(&inbox.session, &not_sent);
                                inbox.session.show_toast(text, true);
                                cx.notify();
                                return;
                            }
                        };
                        let wrote = matches!(kind, SyncKind::Syncing { .. })
                            || matches!(kind, SyncKind::Synced { stored, .. } if stored > 0);
                        inbox.session.sync = kind;
                        if wrote {
                            inbox.request_threads(ReadIntent::default(), cx);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Reread the folder on screen on the store thread. While one read is in
    /// flight, further requests merge into a single follow-up, so a burst of
    /// sync batches is one read and never a queue.
    fn request_threads(&mut self, intent: ReadIntent, cx: &mut Context<Self>) {
        // A sync refresh in flight does not make the reader wait: their read
        // starts now on the interactive lane, and the refresh is dropped
        // when it lands. This read sees everything the refresh would have.
        let supersede = self.read_in_flight && self.refresh_in_flight && !intent.is_refresh();
        if self.read_in_flight && !supersede {
            self.read_wanted = Some(match self.read_wanted.take() {
                Some(earlier) => earlier.merge(intent),
                None => intent,
            });
            return;
        }
        self.read_in_flight = true;
        self.refresh_in_flight = intent.is_refresh();
        self.read_gen += 1;
        let generation = self.read_gen;
        let account = self.account_id.clone();
        let folder = self.session.folder.clone();
        let me = self.session.account_email.clone();
        let reading = folder.clone();
        // A folder the rows on screen do not come from opens at its top.
        let span = if folder != self.list_folder || self.session.threads.is_empty() {
            FolderSpan::Top {
                rows: self.session.load_rows,
            }
        } else {
            self.session
                .span(intent.fetch, intent.select.as_ref(), self.visible_rows())
        };
        // The selected thread, unless the reader took it out of the list.
        let follow = self
            .session
            .cursor_id
            .clone()
            .filter(|id| folder == self.list_folder && !self.pending_removals.contains(id));
        let read = move |store: &fell_core::Store| -> ThreadsResult {
            let page = store
                .folder_page(&account, &reading, &span)
                .map_err(|err| err.to_string())?;
            let names = store.folder_names(&account).unwrap_or_default();
            let roles = store.folder_roles(&account).unwrap_or_default();
            let followed = match follow {
                Some(id) if !page.threads.iter().any(|thread| thread.thread_id.0 == id) => store
                    .folder_key_of(&account, &reading, &ThreadId(id))
                    .ok()
                    .flatten(),
                _ => None,
            };
            let drafts = if roles.is(FolderRole::Drafts, &reading) {
                store
                    .unsent_compose_drafts(&account)
                    .map_err(|err| err.to_string())?
            } else {
                Vec::new()
            };
            Ok(ListRead {
                rows: present::thread_rows(&page.threads, &me),
                more_before: page.more_before,
                more_after: page.more_after,
                unread: page.unread,
                names,
                followed,
                drafts,
                roles,
            })
        };
        // Only sync asked: nobody is waiting, so keep the interactive lane
        // free for the conversation the reader opens next.
        let reply = if intent.is_refresh() {
            self.store.call_background(read)
        } else {
            self.store.call(read)
        };
        cx.spawn(async move |this, cx| {
            let Ok(result) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if inbox.read_gen == generation {
                    let log = inbox.frames.clone();
                    FrameLog::event(log.as_ref(), "rows", || {
                        inbox.read_landed(folder, result, intent, cx)
                    });
                }
            });
        })
        .detach();
    }

    fn read_landed(
        &mut self,
        folder: String,
        result: ThreadsResult,
        intent: ReadIntent,
        cx: &mut Context<Self>,
    ) {
        self.read_in_flight = false;
        self.refresh_in_flight = false;
        let wanted = self.read_wanted.take();
        // A user action (switch, archive, move, undo) since this read started
        // makes it stale. Skip it and read again with both intents, so the
        // list never flashes a folder or a row the user already left.
        let stale =
            folder != self.session.folder || wanted.as_ref().is_some_and(|next| !next.is_refresh());
        if stale {
            let merged = match wanted {
                Some(next) => intent.merge(next),
                None => intent,
            };
            self.request_threads(merged, cx);
            return;
        }
        self.show_threads(folder, result, intent, cx);
        // Only sync wrote while that read ran. Show it first, then read once
        // more, so a fast sync cannot starve the list.
        if let Some(next) = wanted {
            self.request_threads(next, cx);
        }
    }

    fn show_threads(
        &mut self,
        folder: String,
        result: ThreadsResult,
        intent: ReadIntent,
        cx: &mut Context<Self>,
    ) {
        self.session.loading = false;
        let ListRead {
            rows: threads,
            more_before,
            more_after,
            mut unread,
            names,
            followed,
            drafts,
            roles,
        } = match result {
            Ok(found) => found,
            Err(err) => {
                if intent.switch {
                    self.session.folder = self.list_folder.clone();
                }
                self.session.fail_query(QueryFault::Folder, err);
                cx.notify();
                return;
            }
        };
        self.session.clear_query_fault(QueryFault::Folder);
        // The person's drafts first, as the editor has them right now. They
        // sit above the folder's first row, so they show only when the rows
        // read reach the top of the folder.
        self.drafts = self.live_drafts(drafts, cx);
        let mut rows = if more_before {
            Vec::new()
        } else {
            present::draft_rows(&self.drafts)
        };
        rows.extend(threads);
        if !names.is_empty() {
            self.session.folders = names;
        }
        self.session.roles = roles;
        let switched = folder != self.list_folder;
        if !switched {
            // A read from before the write still counts the rows taken out.
            let hidden_unread = rows
                .iter()
                .filter(|row| row.unread && self.pending_removals.contains(&row.id))
                .count();
            unread = unread.saturating_sub(hidden_unread);
            rows.retain(|row| !self.pending_removals.contains(&row.id));
        }
        // New mail moved the selected thread outside the rows read (they do
        // not reach the top of the folder). Read the rows around it, as undo
        // does, instead of selecting whichever row took its place.
        if !switched
            && intent.select.is_none()
            && let Some(key) = self.session.moved_selection(&rows, followed)
        {
            self.request_threads(
                ReadIntent {
                    select: Some(key),
                    ..intent
                },
                cx,
            );
            return;
        }
        self.list_folder = folder;
        if switched {
            self.session.forget_window();
            if let Some(id) = &self.session.cursor_id
                && !rows.iter().any(|row| &row.id == id)
            {
                // A new folder opens at its top, not at the old folder's index.
                self.session.cursor_id = None;
            }
        }
        let row_height = self.list_row_height();
        let now = self.motion.clock.now();
        let drawn = self.drawn_rows(now);
        let before_id = self.session.cursor_id.clone();
        let before_index = before_id
            .as_ref()
            .and_then(|id| self.session.threads.iter().position(|row| &row.id == id));
        let open_count = self.open_thread.as_ref().and_then(|id| {
            rows.iter()
                .find(|row| &row.id == id)
                .map(|row| row.message_count)
        });
        self.session.set_page(rows, more_before, more_after, unread);
        if let Some(key) = &intent.select {
            self.session.select_if_present(&key.thread_id.0);
        }
        let after_index = self
            .session
            .cursor_id
            .as_ref()
            .and_then(|id| self.session.threads.iter().position(|row| &row.id == id));
        let same_thread = self.session.cursor_id == before_id;
        match present::landing(switched, intent.holds_viewport(), same_thread) {
            present::Landing::Hold => {
                // Rows sync put above the selection, or rows loaded above
                // the first one, push it down. Scroll by the same amount so
                // the selected row stays where the reader left it.
                if let (Some(before), Some(after), Some(row)) =
                    (before_index, after_index, row_height)
                {
                    self.shift_list(after as i64 - before as i64, row);
                }
            }
            present::Landing::Glide => {
                // A `G`, `gg` or page that ran past the loaded rows landed:
                // glide there as if the rows had been loaded all along.
                let moved = before_id
                    .as_ref()
                    .and_then(|id| self.session.threads.iter().position(|row| &row.id == id))
                    .zip(after_index)
                    .map(|(from, to)| to as i64 - from as i64);
                self.glide_list_to_cursor(moved);
            }
            present::Landing::Jump => self.scroll_list_to_cursor(),
        }
        self.start_list_shift(drawn, switched);
        if self.warmed_folder.as_deref() != Some(self.list_folder.as_str()) {
            self.warm_bottom(more_after, cx);
        }
        let open_changed = self.open_thread != self.session.cursor_id;
        let grew = open_count.is_some_and(|count| count != self.session.messages.len());
        // A draft's preview follows its edits.
        if open_changed || grew || self.session.cursor_draft().is_some() {
            self.request_messages(true, cx);
        }
        // A cursor move that waited for these rows may need more.
        self.issue_fetch(cx);
        cx.notify();
    }

    /// Read the folder's last rows and their thread's conversation ahead,
    /// on the background lane, for [`Self::warm_layer`] to draw unseen
    /// while nothing moves. A folder whose rows all fit on screen has
    /// nothing a `G` would show for the first time.
    fn warm_bottom(&mut self, more_after: bool, cx: &mut Context<Self>) {
        let folder = self.list_folder.clone();
        self.warmed_folder = Some(folder.clone());
        self.warm = None;
        let shown = (self.list_viewport().unwrap_or(760.0) / ROW_PX).ceil() as usize + 1;
        if !more_after && self.session.threads.len() <= shown {
            return;
        }
        // A `G` glides in over the last viewport from one viewport above
        // it ([`motion::GLIDE_MAX_VIEWPORTS`]): its first frames show those
        // rows, its last ones the folder's end.
        let reach = 2 * shown;
        let account = self.account_id.clone();
        let me = self.session.account_email.clone();
        let roles = self.session.roles.clone();
        let names = self.session.folders.clone();
        let reading = folder.clone();
        let reply = self.store.call_background(move |store: &fell_core::Store| {
            let page = store
                .folder_page(&account, &reading, &FolderSpan::Bottom { rows: reach })
                .ok()?;
            let last = page.threads.last()?;
            let messages = store.thread_messages(&account, &last.thread_id).ok()?;
            Some((
                present::thread_rows(&page.threads, &me),
                present::conversation(&messages, &me, &roles, &names),
            ))
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some((rows, messages))) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if inbox.list_folder != folder {
                    return;
                }
                let subject = rows
                    .last()
                    .map(|row| row.subject.clone())
                    .unwrap_or_default();
                inbox.warm = Some(Warm {
                    folder,
                    rows,
                    subject,
                    messages,
                    rows_drawn: 0,
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// What a `G` will show, drawn unseen under the panes so the frame
    /// that shows it does not shape and rasterize it cold: once per
    /// folder, in frames where nothing moves, the folder's last rows a few
    /// at a time and then their thread's conversation, read ahead by
    /// [`Self::warm_bottom`]. Their shaping stays in the row cache and
    /// their glyphs in the atlas.
    ///
    /// Drawn at opacity 0, and taking no pointer input ([`Live`]).
    fn warm_layer(
        &mut self,
        width: f32,
        (show_list, show_conv): (bool, bool),
        animating: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        let theme = self.theme.clone();
        let now = self.frame_now;
        let mut parts = Vec::new();
        let ahead = self
            .warm
            .take()
            .filter(|warm| warm.folder == self.list_folder);
        match ahead {
            Some(warm) if animating => self.warm = Some(warm),
            Some(mut warm) if warm.rows_drawn < warm.rows.len() => {
                let list_width = if show_list && show_conv {
                    list_width(width, self.split)
                } else {
                    width
                };
                let look = RowLook {
                    selected: false,
                    fading: None,
                    ghost: true,
                    bar_dx: 0.0,
                };
                let text_width = row_text_width(list_width);
                let from = warm.rows_drawn;
                let chunk: Vec<ThreadRow> = warm
                    .rows
                    .iter()
                    .skip(from)
                    .take(WARM_ROWS)
                    .cloned()
                    .collect();
                let count = chunk.len();
                let row_theme = theme.clone();
                let entity = cx.entity();
                // Built where the list builds its rows, in prepaint, so they
                // shape in the UI font the list's text style sets: built
                // here in render they would use GPUI's default text style,
                // and their shaping would never be the list's.
                parts.push(
                    uniform_list("warm-rows", count, move |range, window, app| {
                        entity.update(app, |_, cx| {
                            range
                                .map(|index| {
                                    thread_row(
                                        &row_theme,
                                        &chunk[index],
                                        index,
                                        look,
                                        text_width,
                                        window,
                                        cx,
                                    )
                                })
                                .collect::<Vec<_>>()
                        })
                    })
                    .w(px(list_width))
                    .h(px(count as f32 * ROW_PX))
                    .flex_none()
                    .into_any_element(),
                );
                warm.rows_drawn = (from + WARM_ROWS).min(warm.rows.len());
                self.warm = Some(warm);
                window.request_animation_frame();
            }
            Some(mut warm) => {
                let conv_width = if show_list && show_conv {
                    (width - list_width(width, self.split)).max(120.0)
                } else {
                    width
                };
                // Drawn as the session would show it once `G` opens it.
                std::mem::swap(&mut self.session.messages, &mut warm.messages);
                let cursor = std::mem::replace(
                    &mut self.session.message_cursor,
                    self.session.messages.len().saturating_sub(1),
                );
                parts.push(
                    conversation(
                        &theme,
                        &self.session,
                        warm.subject.clone(),
                        &self.motion,
                        now,
                        None,
                        conv_width,
                        Live(false),
                        window,
                        cx,
                    )
                    .into_any_element(),
                );
                std::mem::swap(&mut self.session.messages, &mut warm.messages);
                self.session.message_cursor = cursor;
            }
            None => {}
        }
        if parts.is_empty() {
            return None;
        }
        Some(
            div()
                .id("warm")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .overflow_hidden()
                .opacity(0.0)
                .children(parts.into_iter().enumerate().map(|(index, part)| {
                    div()
                        .id(gpui::ElementId::named_usize("warm", index))
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                        .flex()
                        .flex_row()
                        .child(part)
                }))
                .into_any_element(),
        )
    }

    /// Read the rows the list asked for. While the reader's own read is in
    /// flight the ask is dropped: when that read lands, the list asks again
    /// for whatever it still lacks.
    fn issue_fetch(&mut self, cx: &mut Context<Self>) {
        let Some(fetch) = self.session.take_fetch() else {
            return;
        };
        // A jump the reader cancelled must reach the read in flight, or its
        // rows would replace the ones the reader moved in.
        if self.read_in_flight && !self.refresh_in_flight && !fetch.drop_jump {
            return;
        }
        self.request_threads(
            ReadIntent {
                fetch,
                ..ReadIntent::default()
            },
            cx,
        );
    }

    /// Rows the list viewport shows, from its scroll offset and row height.
    fn visible_rows(&self) -> Option<std::ops::Range<usize>> {
        let row = self.list_row_height()?;
        let state = self.list_scroll.0.borrow();
        let size = state.last_item_size?;
        let top = (-f32::from(state.base_handle.offset().y)).max(0.0);
        let start = (top / row).floor() as usize;
        let shown = (f32::from(size.item.height) / row).ceil() as usize + 1;
        let end = start.saturating_add(shown).min(self.session.threads.len());
        (start < end).then_some(start..end)
    }

    /// One row's height as the list last laid it out. GPUI records the
    /// viewport as `item` and the whole list as `contents`.
    fn list_row_height(&self) -> Option<f32> {
        let state = self.list_scroll.0.borrow();
        let size = state.last_item_size?;
        let rows = self.session.threads.len();
        if rows == 0 {
            return None;
        }
        let row = f32::from(size.contents.height) / rows as f32;
        (row > 0.0).then_some(row)
    }

    /// Move the list viewport by `rows` rows of `row` pixels without moving
    /// the selection. GPUI offsets grow more negative further down the list.
    fn shift_list(&mut self, rows: i64, row: f32) {
        if rows == 0 {
            return;
        }
        let state = self.list_scroll.0.borrow();
        let handle = &state.base_handle;
        let offset = handle.offset();
        let max = f32::from(handle.max_offset().height) + rows.max(0) as f32 * row;
        let next = (f32::from(offset.y) - rows as f32 * row).clamp(-max, 0.0);
        handle.set_offset(point(offset.x, px(next)));
        // A glide in flight moves with the rows, so it still lands on the
        // selection.
        if let Some(glide) = &mut self.motion.list_glide {
            glide.from = (glide.from - rows as f32 * row).clamp(-max, 0.0);
            glide.to = (glide.to - rows as f32 * row).clamp(-max, 0.0);
        }
        // Rows that left move down with the rows put above them.
        self.motion.move_ghosts(rows as f32 * row);
    }

    /// The list's scroll offset as last drawn (0 at the top, negative below).
    fn list_offset(&self) -> f32 {
        f32::from(self.list_scroll.0.borrow().base_handle.offset().y)
    }

    fn set_list_offset(&self, y: f32) {
        let state = self.list_scroll.0.borrow();
        let offset = state.base_handle.offset();
        state.base_handle.set_offset(point(offset.x, px(y)));
    }

    /// Height of the list viewport, once the list has been laid out.
    fn list_viewport(&self) -> Option<f32> {
        let state = self.list_scroll.0.borrow();
        let height = f32::from(state.last_item_size?.item.height);
        (height > 0.0).then_some(height)
    }

    /// The rows on screen and two past each edge, with their top in the
    /// viewport as the last frame painted them: where a list change
    /// animates them from. A key that settled a glide has already moved
    /// the offset to its target, so the painted offset is used instead.
    fn drawn_rows(&self, now: Instant) -> Vec<Drawn> {
        let Some(view) = self.list_viewport() else {
            return Vec::new();
        };
        let offset = self
            .motion
            .painted_list_offset()
            .unwrap_or_else(|| self.list_offset());
        let first = ((-offset / ROW_PX).floor() as i64 - 2).max(0) as usize;
        let last = (((-offset + view) / ROW_PX).ceil() as i64 + 2).max(0) as usize;
        let rows = &self.session.threads;
        (first.min(rows.len())..last.min(rows.len()))
            .map(|index| {
                let row = &rows[index];
                let slot = index as f32 * ROW_PX + offset;
                Drawn {
                    top: slot + self.motion.row_offset(&row.id, slot, now),
                    slot,
                    row: row.clone(),
                }
            })
            .collect()
    }

    /// Animate the rows from where `drawn` had them to where they are now.
    /// Rows among them that left the list fade out where they were, and
    /// scroll with the list from there. `replace`: the list is another
    /// folder's; it fades in and nothing of the old one lingers.
    fn start_list_shift(&mut self, drawn: Vec<Drawn>, replace: bool) {
        let now = self.motion.clock.now();
        // Ghosts keep their place in the list's content, as of the offset
        // the next frame draws at.
        let offset = self.list_offset();
        let slots: std::collections::HashMap<&str, usize> = self
            .session
            .threads
            .iter()
            .enumerate()
            .map(|(index, row)| (row.id.as_str(), index))
            .collect();
        // Every row on screen kept its slot (rows read above it, the view
        // scrolled by as much): nothing moved, and a shift still running
        // (a landing's fade) carries on.
        let kept = !replace
            && !drawn.is_empty()
            && drawn.iter().all(|old| {
                slots
                    .get(old.row.id.as_str())
                    .is_some_and(|index| (*index as f32 * ROW_PX + offset - old.slot).abs() < 0.5)
            });
        if kept {
            return;
        }
        let ids = slots;
        let before = drawn
            .iter()
            .map(|old| (old.row.id.clone(), old.top))
            .collect();
        let left = drawn
            .iter()
            .filter(|old| !ids.contains_key(old.row.id.as_str()))
            .map(|old| (old.row.clone(), old.top - offset))
            .collect();
        // A row that is back (a quick undo) stops fading where it was.
        let mut carried = self.motion.carried_ghosts();
        carried.retain(|ghost| !ids.contains_key(ghost.row.id.as_str()));
        let reach = self.list_viewport().unwrap_or(640.0);
        // Rows of a list this one replaced that sat below its last row:
        // nothing arrives over them, so they fade out where they were.
        let end = self.session.threads.len() as f32 * ROW_PX;
        let below = drawn
            .into_iter()
            .map(|old| {
                let top = old.top - offset;
                (old.row, top)
            })
            .filter(|(_, top)| *top >= end)
            .collect();
        self.motion.list = ListShift::start(
            &self.motion.motion,
            before,
            left,
            carried,
            reach,
            replace,
            now,
        )
        .map(|shift| shift.fading_below(below));
    }

    /// Move each glide one frame along, and end the ones that arrived.
    /// Offsets land on whole device pixels, so text stays crisp and its
    /// glyphs come from the atlas instead of being rasterized again.
    fn advance_glides(&mut self, now: Instant, scale: f32) {
        if let Some(glide) = self.motion.list_glide {
            self.set_list_offset(motion::snap(glide.value(now), scale));
            if !glide.running(now) {
                self.motion.list_glide = None;
            }
        }
        if let Some(glide) = self.motion.conv_glide {
            let offset = self.conv_scroll.offset();
            self.conv_scroll
                .set_offset(point(offset.x, px(motion::snap(glide.value(now), scale))));
            if !glide.running(now) {
                self.motion.conv_glide = None;
            }
        }
    }

    /// A key arrived: jump every transition to its end state first.
    fn settle_motion(&mut self) {
        let now = self.motion.clock.now();
        let settled = self.motion.settle(now);
        if let Some(y) = settled.list {
            self.set_list_offset(y);
        }
        if let Some(y) = settled.conversation {
            let offset = self.conv_scroll.offset();
            self.conv_scroll.set_offset(point(offset.x, px(y)));
        }
    }

    /// Read the selected thread on the store thread. A read that is stale
    /// by the time the store thread reaches it is skipped there.
    fn request_messages(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(id) = self.session.cursor_id.clone() else {
            self.conv_gen.fetch_add(1, Ordering::SeqCst);
            self.clear_open_message();
            return;
        };
        if !force && self.open_thread.as_deref() == Some(id.as_str()) {
            return;
        }
        if let Some(draft) = self.session.cursor_draft() {
            // A draft is shown from the drafts list, not read as a thread.
            self.conv_gen.fetch_add(1, Ordering::SeqCst);
            let shown = self
                .drafts
                .iter()
                .find(|item| item.id == draft)
                .map(|item| present::draft_conversation(item, &self.session.account_email))
                .unwrap_or_default();
            let same = self.open_thread.as_deref() == Some(id.as_str());
            if same {
                self.session.refresh_messages(shown, true);
            } else {
                self.session.set_messages(shown);
            }
            self.session.clear_query_fault(QueryFault::Message);
            self.mark_open(id);
            return;
        }
        let generation = self.conv_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let current = self.conv_gen.clone();
        let account = self.account_id.clone();
        let me = self.session.account_email.clone();
        let roles = self.session.roles.clone();
        let names = self.session.folders.clone();
        let thread = id.clone();
        let reply = self.store.call(move |store| {
            if current.load(Ordering::SeqCst) != generation {
                return None;
            }
            Some(
                store
                    .thread_messages(&account, &ThreadId(thread))
                    .map(|messages| present::conversation(&messages, &me, &roles, &names))
                    .map_err(|err| err.to_string()),
            )
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some(result)) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if inbox.conv_gen.load(Ordering::SeqCst) != generation
                    || inbox.session.cursor_id.as_deref() != Some(id.as_str())
                {
                    return;
                }
                let log = inbox.frames.clone();
                FrameLog::event(log.as_ref(), "messages", || match result {
                    Ok(view) => {
                        inbox.session.clear_query_fault(QueryFault::Message);
                        let same = inbox.open_thread.as_deref() == Some(id.as_str());
                        // Sync adding to the thread being read must not pull a
                        // reader who scrolled up back to the bottom.
                        let follow = if same {
                            let bottom = inbox.conversation_at_bottom();
                            inbox.session.refresh_messages(view, bottom)
                        } else {
                            inbox.session.set_messages(view);
                            inbox.motion.thread_arrived();
                            true
                        };
                        inbox.mark_open(id);
                        if follow {
                            inbox.scroll_conversation();
                        }
                    }
                    Err(err) => {
                        // Never leave another thread's body under this row.
                        if inbox.open_thread.as_deref() != Some(id.as_str()) {
                            inbox.clear_open_message();
                        }
                        inbox.session.fail_query(QueryFault::Message, err);
                    }
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// Run a mailbox change on the store thread, then reread the folder.
    fn change_mailbox(
        &mut self,
        change: impl FnOnce(&fell_core::Store) -> Result<(), fell_core::Error> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<(), fell_core::Error>) -> Option<ReadIntent> + 'static,
        cx: &mut Context<Self>,
    ) {
        let reply = self.store.call(change);
        cx.spawn(async move |this, cx| {
            let Ok(result) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if let Some(after) = done(inbox, result) {
                    inbox.request_threads(after, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Say so in the status bar when a review window an agent opened closes,
    /// so the person knows the draft was kept and nothing was sent.
    fn watch_reviews(&self, cx: &mut Context<Self>) {
        let Some(hub) = cx
            .try_global::<crate::draft_host::AgentDrafts>()
            .map(|drafts| drafts.0.clone())
        else {
            return;
        };
        let (tx, mut events) = futures::channel::mpsc::unbounded::<()>();
        hub.subscribe_with(move || tx.unbounded_send(()).is_ok());
        cx.spawn(async move |this, cx| {
            while events.next().await.is_some() {
                while events.try_recv().is_ok() {}
                let closed = hub.take_notices();
                let alive = this
                    .update(cx, |inbox, cx| {
                        // A draft changed, was sent, or is new: the drafts
                        // list on screen follows.
                        if inbox.session.in_drafts() {
                            inbox.request_threads(ReadIntent::default(), cx);
                        }
                        if let Some((line, warn)) = closed.last().cloned() {
                            inbox.session.show_toast(line, warn);
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Reload the theme when Omarchy, GTK, or Alacritty settings change.
    /// inotify wakes this; without it, a slow poll does.
    fn watch_theme(&self, cx: &mut Context<Self>) {
        let (changed, mut changes) = futures::channel::mpsc::unbounded();
        let watching = theme::watch(changed);
        cx.spawn(async move |this, cx| {
            loop {
                if watching {
                    if changes.next().await.is_none() {
                        break;
                    }
                } else {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                }
                let next = cx
                    .background_executor()
                    .spawn(async { theme::load() })
                    .await;
                let alive = this
                    .update(cx, |inbox, cx| {
                        if inbox.theme != next {
                            inbox.theme = next;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn watch_dates(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(15))
                    .await;
                let alive = this.update(cx, |_, cx| cx.notify()).is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Move messages in the folder on screen to Archive. A Sent copy stays in
    /// Sent. The row leaves the list at once and the row in its place is
    /// selected, so a second `e` archives that one.
    fn archive_current(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.session.cursor_id.clone() else {
            self.session.show_toast("No conversation", true);
            return;
        };
        if self.session.cursor_draft().is_some() {
            self.session.show_toast(DRAFT_NOT_THREAD, true);
            return;
        }
        if !self.session.threads.iter().any(|thread| thread.id == id) {
            self.session.show_toast("No conversation", true);
            return;
        }
        let folder = self.list_folder.clone();
        let account = self.account_id.clone();
        let thread = ThreadId(id.clone());
        let key = self.selected_key();
        self.take_out_of_list(&id, cx);
        self.change_mailbox(
            move |store| store.archive_thread(&account, &thread, &folder).map(|_| ()),
            move |inbox, result| match result {
                Ok(()) => {
                    inbox.pending_removals.remove(&id);
                    inbox.moved_thread = key;
                    inbox.session.show_toast("Archived · u", false);
                    inbox.send_to_server();
                    Some(ReadIntent {
                        changed: true,
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    inbox.pending_removals.remove(&id);
                    inbox.session.show_toast(archive_failure(&err), true);
                    Some(ReadIntent {
                        changed: true,
                        select: key,
                        ..ReadIntent::default()
                    })
                }
            },
            cx,
        );
    }

    fn apply_move(&mut self, destination: String, cx: &mut Context<Self>) {
        let Some(id) = self.session.cursor_id.clone() else {
            self.session.show_toast("No conversation", true);
            return;
        };
        if self.session.cursor_draft().is_some() {
            self.session.show_toast(DRAFT_NOT_THREAD, true);
            return;
        }
        if !self.session.threads.iter().any(|thread| thread.id == id) {
            self.session.show_toast("No conversation", true);
            return;
        }
        let account = self.account_id.clone();
        let thread = ThreadId(id.clone());
        let from = self.list_folder.clone();
        let to = destination.clone();
        let key = self.selected_key();
        self.take_out_of_list(&id, cx);
        self.change_mailbox(
            move |store| store.move_thread(&account, &thread, &from, &to).map(|_| ()),
            move |inbox, result| match result {
                Ok(()) => {
                    inbox.pending_removals.remove(&id);
                    inbox.moved_thread = key;
                    let label = inbox.session.folder_text(&destination);
                    inbox.session.show_toast(format!("Moved to {label}"), false);
                    inbox.send_to_server();
                    Some(ReadIntent {
                        changed: true,
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    inbox.pending_removals.remove(&id);
                    inbox.session.show_toast(err.to_string(), true);
                    Some(ReadIntent {
                        changed: true,
                        select: key,
                        ..ReadIntent::default()
                    })
                }
            },
            cx,
        );
    }

    /// Archive, move, and undo may have queued a server move: send it now
    /// rather than at the next pass.
    fn send_to_server(&self) {
        if let Some(sync) = &self.sync {
            sync.write_back_now();
        }
    }

    /// Remove `id` from the rows on screen before its write lands and open
    /// the row that took its place.
    fn take_out_of_list(&mut self, id: &str, cx: &mut Context<Self>) {
        self.pending_removals.insert(id.to_string());
        let now = self.motion.clock.now();
        let drawn = self.drawn_rows(now);
        self.session.remove_thread(id);
        self.scroll_list_to_cursor();
        self.start_list_shift(drawn, false);
        self.request_messages(true, cx);
        // The removed row was the last one loaded: load the row after it.
        self.issue_fetch(cx);
    }

    /// List position of the selected row, so undo can find it again even
    /// after it scrolled out of the loaded rows.
    fn selected_key(&self) -> Option<ListKey> {
        let id = self.session.cursor_id.as_ref()?;
        self.session
            .threads
            .iter()
            .find(|row| &row.id == id)
            .map(row_key)
    }

    fn apply_undo(&mut self, cx: &mut Context<Self>) {
        let account = self.account_id.clone();
        self.change_mailbox(
            move |store| store.undo(&account).map(|_| ()),
            |inbox, result| match result {
                Ok(()) => {
                    inbox.session.show_toast("Undone", false);
                    inbox.send_to_server();
                    Some(ReadIntent {
                        changed: true,
                        select: inbox.moved_thread.take(),
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    let raw = err.to_string();
                    if raw == "nothing to undo" {
                        inbox.session.show_toast("Nothing to undo", false);
                    } else {
                        inbox.session.show_toast(raw, true);
                    }
                    None
                }
            },
            cx,
        );
    }

    /// `c` (no id) or Enter on a draft row: bring the compose window up on
    /// the draft. A live draft shows at once; otherwise it is created or read
    /// from the mailbox off this thread, then shown.
    fn open_draft(&mut self, id: Option<String>, cx: &mut Context<Self>) {
        let Some(hub) = cx
            .try_global::<crate::draft_host::AgentDrafts>()
            .map(|drafts| drafts.0.clone())
        else {
            self.session
                .show_toast("Compose is not available here", true);
            return;
        };
        // The window takes a moment to appear: say what is coming now.
        if id.is_none() {
            self.session.show_toast("new draft", false);
        } else {
            let subject = self
                .session
                .cursor_thread()
                .map(|row| row.subject.clone())
                .unwrap_or_default();
            self.session
                .show_toast(format!("opening draft \u{201c}{subject}\u{201d}"), false);
        }
        if let Some(id) = &id
            && crate::draft_host::show_draft(&hub, id)
        {
            self.settle_opening_notice(cx);
            return;
        }
        let Some(home) = self.draft_home.clone() else {
            self.session.show_toast(
                "The mailbox did not open, so a draft has nowhere to go · ? details",
                true,
            );
            return;
        };
        let account = self.account_id.clone();
        let email = self.session.account_email.clone();
        let creating = id.is_none();
        // The draft goes live in the hub off this thread; only showing it
        // happens here.
        let task = cx.background_executor().spawn({
            let hub = hub.clone();
            async move {
                match id {
                    Some(id) => home.open(&hub, &id, &account).map(|()| id),
                    None => home.new_draft(&hub, &account, &email),
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |inbox, cx| {
                match result {
                    Ok(id) => {
                        // `false` while another open of it is still reading
                        // it; that one shows it when it lands.
                        if crate::draft_host::show_draft(&hub, &id) {
                            inbox.settle_opening_notice(cx);
                        }
                    }
                    Err(err) => {
                        let what = if creating {
                            "start a draft"
                        } else {
                            "open the draft"
                        };
                        inbox
                            .session
                            .show_toast(format!("Couldn't {what}: {err}"), true);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// `r`, `R`, `f`: start a reply, reply all, or forward of the message
    /// the model picked, and bring the compose window up on it. The parent
    /// is read on the store thread and the draft is written off this one.
    fn start_reply(&mut self, kind: ReplyKind, cx: &mut Context<Self>) {
        // The conversation on screen must be the selected row's: right
        // after a move, it can still be the last row's.
        let loaded = self.open_thread.is_some() && self.open_thread == self.session.cursor_id;
        let Some(target) = self.session.reply_target().filter(|_| loaded) else {
            self.session
                .show_toast("The conversation is still loading", false);
            return;
        };
        let parent_id = fell_core::MessageId::new(&target.id);
        let sender = target.sender.clone();
        let Some(hub) = cx
            .try_global::<crate::draft_host::AgentDrafts>()
            .map(|drafts| drafts.0.clone())
        else {
            self.session
                .show_toast("Compose is not available here", true);
            return;
        };
        let Some(home) = self.draft_home.clone() else {
            self.session.show_toast(
                "The mailbox did not open, so a draft has nowhere to go · ? details",
                true,
            );
            return;
        };
        self.session.show_toast(reply_notice(kind, &sender), false);
        let account = self.account_id.clone();
        let email = self.session.account_email.clone();
        let roles = self.session.roles.clone();
        let read = self.store.call({
            let account = account.clone();
            move |store| store.message(&account, &parent_id)
        });
        cx.spawn(async move |this, cx| {
            let Ok(found) = read.await else {
                return;
            };
            let result = match found {
                Ok(Some(parent)) => {
                    let own = fell_core::compose::own_addresses(&parent, &email, &roles);
                    let offset = crate::present::local_offset(parent.headers.date_unix);
                    let start = fell_core::compose::start_reply_in_zone(
                        kind, &parent, &email, &own, offset, &roles,
                    );
                    let hub = hub.clone();
                    cx.background_executor()
                        .spawn(async move { home.new_reply(&hub, &account, start) })
                        .await
                }
                Ok(None) => Err("the message is no longer in the mailbox".to_string()),
                Err(err) => Err(err.to_string()),
            };
            let _ = this.update(cx, |inbox, cx| {
                match result {
                    Ok(id) => {
                        if crate::draft_host::show_draft(&hub, &id) {
                            inbox.settle_opening_notice(cx);
                        }
                    }
                    Err(err) => {
                        let what = if kind == ReplyKind::Forward {
                            "start the forward"
                        } else {
                            "start the reply"
                        };
                        inbox
                            .session
                            .show_toast(format!("Couldn't {what}: {err}"), true);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The window is on its way: once it has had time to show, the inbox's
    /// "opening" notice has done its job and goes, unless something newer
    /// replaced it.
    fn settle_opening_notice(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(700))
                .await;
            let _ = this.update(cx, |inbox, cx| {
                let opening = inbox.session.toast.as_ref().is_some_and(|toast| {
                    toast.text.starts_with("opening draft")
                        || toast.text == "new draft"
                        || [ReplyKind::Reply, ReplyKind::ReplyAll, ReplyKind::Forward]
                            .into_iter()
                            .any(|kind| toast.text.starts_with(reply_notice_prefix(kind)))
                });
                if opening {
                    inbox.session.clear_toast();
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The drafts the store listed, with what the editor holds for any that
    /// are live: unsaved typing, and a send that just went out.
    fn live_drafts(&self, stored: Vec<DraftSummary>, cx: &mut Context<Self>) -> Vec<DraftSummary> {
        let Some(hub) = cx
            .try_global::<crate::draft_host::AgentDrafts>()
            .map(|drafts| drafts.0.clone())
        else {
            return stored;
        };
        stored
            .into_iter()
            .filter_map(|mut draft| {
                let live = hub.with_session(&draft.id, |session| {
                    (session.snapshot(), session.send_lock())
                });
                let Some((snap, lock)) = live else {
                    return Some(draft);
                };
                match lock {
                    Some(fell_core::compose::SendLock::Sent) => return None,
                    Some(_) => draft.maybe_sent = true,
                    None => {}
                }
                let raw = |items: &[fell_core::compose::AddressView]| {
                    items.iter().map(|item| item.raw.clone()).collect()
                };
                draft.to = raw(&snap.to);
                draft.cc = raw(&snap.cc);
                draft.bcc = raw(&snap.bcc);
                draft.subject = snap.subject;
                draft.body = snap.body;
                draft.from = snap.from;
                Some(draft)
            })
            .collect()
    }

    fn clear_open_message(&mut self) {
        self.session.messages.clear();
        self.open_thread = None;
        self.open_subject.clear();
    }

    /// The conversation now shows thread `id`'s messages.
    fn mark_open(&mut self, id: String) {
        self.open_subject = self
            .session
            .threads
            .iter()
            .find(|row| row.id == id)
            .map(|row| row.subject.clone())
            .unwrap_or_default();
        self.open_thread = Some(id);
    }

    /// The subject over the conversation: the thread whose messages show.
    /// The selection moves at once, but the messages follow when their
    /// read lands; the subject goes with them, so it never names another
    /// thread than the body under it.
    fn shown_subject(&self) -> String {
        let Some(open) = &self.open_thread else {
            return String::new();
        };
        self.session
            .threads
            .iter()
            .find(|row| &row.id == open)
            .map_or_else(|| self.open_subject.clone(), |row| row.subject.clone())
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = key_from(&event.keystroke) else {
            return;
        };
        if let Some(log) = &self.frames {
            log.borrow_mut()
                .key(Instant::now(), event.keystroke.unparse());
        }
        // Input never waits on an animation.
        self.settle_motion();
        window.focus(&self.focus);
        // The wheel moves the viewport without moving message_cursor. j, k,
        // and q aim at the message now on screen in either pane. Enter and o
        // do that only while the conversation is focused, so they expand the
        // message on screen. An open palette, folder filter, or help card
        // types those keys and must leave the cursor alone.
        if should_follow_visible(
            &self.session.overlay,
            self.session.chord.is_empty(),
            self.session.pane,
            key,
        ) {
            self.follow_visible_message();
        }
        let page = self.page_rows();
        let message_before = self.session.message_cursor;
        let thread_before = self.session.cursor_id.clone();
        let folder_before = self.session.folder.clone();
        self.session.clear_toast();
        let step = self.session.on_key(key, page);
        self.apply_step(step, message_before, thread_before, folder_before, cx);
        cx.stop_propagation();
    }

    fn apply_step(
        &mut self,
        step: Step,
        message_before: usize,
        thread_before: Option<String>,
        folder_before: String,
        cx: &mut Context<Self>,
    ) {
        if step.apply_move
            && let Some(destination) = self.session.pending_move.take()
        {
            self.apply_move(destination, cx);
        }
        if step.undo {
            self.apply_undo(cx);
        }
        if step.archive {
            self.archive_current(cx);
        }
        if step.reopen_review {
            let reopened = cx
                .try_global::<crate::draft_host::AgentDrafts>()
                .is_some_and(|drafts| drafts.0.reopen_last());
            if !reopened {
                self.session.show_toast("no agent review to reopen", false);
            }
        }
        if step.compose {
            self.open_draft(None, cx);
        }
        if step.open_draft
            && let Some(id) = self.session.cursor_draft().map(str::to_string)
        {
            self.open_draft(Some(id), cx);
        }
        if let Some(kind) = step.reply {
            self.start_reply(kind, cx);
        }
        if step.sync_now {
            match &self.sync {
                Some(handle) => {
                    handle.sync_now();
                    self.session.show_toast("Syncing…", false);
                }
                None => self
                    .session
                    .show_toast("No account is set up · run fell account setup", true),
            }
        }
        // Reads go to the store thread. The list moves on this frame; the
        // conversation follows when its read lands.
        if step.reload_threads && self.session.folder != folder_before {
            self.request_threads(
                ReadIntent {
                    switch: true,
                    ..ReadIntent::default()
                },
                cx,
            );
        } else if step.reload_threads {
            self.request_threads(ReadIntent::default(), cx);
        } else if step.reload_messages {
            self.request_messages(true, cx);
        }
        // A move past the loaded rows, or near their end, asks for more.
        self.issue_fetch(cx);
        if step.arm_which {
            self.arm_which(cx);
        }
        let thread_moved = self.session.cursor_id != thread_before;
        if thread_moved {
            // Rows the selection moved by, when both rows are on the list.
            let moved = thread_before
                .as_ref()
                .and_then(|id| self.session.threads.iter().position(|row| &row.id == id))
                .zip(self.session.selected_index())
                .map(|(from, to)| to as i64 - from as i64);
            match moved {
                Some(rows) => self.glide_list_to_cursor(Some(rows)),
                None => self.scroll_list_to_cursor(),
            }
        }
        // A tall message aligns to its top when scrolled into view. Skip that
        // unless the reader moved to another message, or a new thread loaded.
        // Paging already moved the viewport by pixels, so it must not jump
        // the current message back to the top.
        if step.scroll_pages != 0 || step.scroll_lines != 0 {
            self.nudge_conversation(step.scroll_pages, step.scroll_lines);
        } else if self.session.message_cursor != message_before {
            if self.session.cursor_id == thread_before {
                self.glide_conversation_to_cursor();
            } else {
                self.scroll_conversation();
            }
        }
        if self.session.overlay == Overlay::Palette {
            self.palette_scroll
                .scroll_to_item(self.session.palette_index);
        }
        if self.session.overlay == Overlay::Folders {
            self.folder_scroll.scroll_to_item(self.session.folder_index);
        }
        self.motion.end_key();
        cx.notify();
    }

    fn follow_visible_message(&mut self) {
        if self.session.messages.is_empty() || self.conv_scroll.bounds_for_item(0).is_none() {
            return;
        }
        let index = self.conv_scroll.top_item();
        if index < self.session.messages.len() {
            self.session.message_cursor = index;
        }
    }

    /// The conversation viewport is scrolled to its end (or does not scroll).
    fn conversation_at_bottom(&self) -> bool {
        let max = f32::from(self.conv_scroll.max_offset().height);
        let offset = f32::from(self.conv_scroll.offset().y);
        max <= 1.0 || offset <= -max + 24.0
    }

    fn scroll_conversation(&mut self) {
        if !self.session.messages.is_empty() {
            self.motion.conv_glide = None;
            self.conv_scroll.scroll_to_item(self.session.message_cursor);
        }
    }

    /// `j`/`k` in the conversation: glide to the message, by the rule
    /// GPUI's `scroll_to_item` uses, once its bounds are known.
    fn glide_conversation_to_cursor(&mut self) {
        let index = self.session.message_cursor;
        let Some(item) = self.conv_scroll.bounds_for_item(index) else {
            self.scroll_conversation();
            return;
        };
        let view = self.conv_scroll.bounds();
        let offset = f32::from(self.conv_scroll.offset().y);
        let max = f32::from(self.conv_scroll.max_offset().height);
        let (top, bottom) = (f32::from(item.top()), f32::from(item.bottom()));
        let target = if top + offset < f32::from(view.top()) {
            f32::from(view.top()) - top
        } else if bottom + offset > f32::from(view.bottom()) {
            f32::from(view.bottom()) - bottom
        } else {
            offset
        };
        self.glide_conversation(offset, target.clamp(-max, 0.0));
    }

    /// Set the conversation's offset to `to`, gliding there from where it
    /// is drawn. `offset` is the offset the key saw.
    fn glide_conversation(&mut self, offset: f32, to: f32) {
        let from = self.motion.glide_from_conversation(offset);
        let reach = f32::from(self.conv_scroll.bounds().size.height) * motion::GLIDE_MAX_VIEWPORTS;
        let x = self.conv_scroll.offset().x;
        self.conv_scroll.set_offset(point(x, px(to)));
        let now = self.motion.clock.now();
        self.motion.conv_glide =
            Glide::start(&self.motion.motion, from, to, reach, motion::SLIDE, now)
                .map(|glide| glide.gentle_beyond(LONG_GLIDE_PX));
        if let Some(glide) = self.motion.conv_glide {
            self.conv_scroll.set_offset(point(x, px(glide.from)));
        }
    }

    fn nudge_conversation(&mut self, pages: i32, lines: i32) {
        let viewport = f32::from(self.conv_scroll.bounds().size.height);
        let viewport = if viewport < 1.0 { 640.0 } else { viewport };
        let delta = pages as f32 * viewport + lines as f32 * 24.0;
        let measured = self.conv_scroll.offset();
        let scrollport_top = f32::from(self.conv_scroll.bounds().top());
        let mut item_bounds = Vec::new();
        let mut index = 0;
        while let Some(bounds) = self.conv_scroll.bounds_for_item(index) {
            item_bounds.push((f32::from(bounds.top()), f32::from(bounds.bottom())));
            index += 1;
        }
        let max = f32::from(self.conv_scroll.max_offset().height);
        let measured_y = f32::from(measured.y);
        let next_y = (measured_y - delta).clamp(-max, 0.0);
        let top = present::message_after_page(&item_bounds, scrollport_top, measured_y, next_y)
            .filter(|index| *index < self.session.messages.len());
        self.session.message_cursor = present::cursor_after_nudge(
            self.session.message_cursor,
            top,
            next_y < measured_y,
            next_y != measured_y,
        );
        self.glide_conversation(measured_y, next_y);
    }

    fn arm_which(&mut self, cx: &mut Context<Self>) {
        let generation = self.session.chord_gen;
        let delay = self.session.keymap.which_key_ms;
        if self.session.chord.is_empty() {
            self.session.which_visible = false;
            return;
        }
        if delay == 0 {
            self.session.which_visible = true;
            return;
        }
        self.session.which_visible = false;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay))
                .await;
            let _ = this.update(cx, |inbox, cx| {
                if inbox.session.chord_gen == generation && !inbox.session.chord.is_empty() {
                    inbox.session.which_visible = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn page_rows(&self) -> usize {
        let height = match self.session.pane {
            Pane::List => f32::from(self.list_scroll.0.borrow().base_handle.bounds().size.height),
            Pane::Conversation => f32::from(self.conv_scroll.bounds().size.height),
        };
        let height = if height < 1.0 { 640.0 } else { height };
        let row = match self.session.pane {
            Pane::List => 68.0,
            Pane::Conversation => 160.0,
        };
        ((height / row).floor() as usize).max(1)
    }

    /// Bring the selection into view at once: a row off screen goes to the
    /// top. Nothing selected (the row after an archived one is loading): stay.
    fn scroll_list_to_cursor(&mut self) {
        let Some(index) = self.session.selected_index() else {
            return;
        };
        let Some(view) = self.list_viewport() else {
            self.list_scroll.scroll_to_item(index, ScrollStrategy::Top);
            return;
        };
        let current = self
            .motion
            .list_glide
            .map_or_else(|| self.list_offset(), |glide| glide.to);
        let rows = self.session.threads.len();
        let target = present::list_scroll_target(current, index, None, rows, ROW_PX, view);
        self.motion.list_glide = None;
        self.set_list_offset(target);
    }

    /// The selection moved by `moved` rows: glide the list so the
    /// selection shows, moving the view by as many rows when the selection
    /// would leave it (`Ctrl-d`, `G`), so it stays where the eye is. A
    /// glide still running is aimed again from where it is drawn.
    fn glide_list_to_cursor(&mut self, moved: Option<i64>) {
        let Some(index) = self.session.selected_index() else {
            return;
        };
        let Some(view) = self.list_viewport() else {
            self.list_scroll.scroll_to_item(index, ScrollStrategy::Top);
            return;
        };
        let drawn = self.list_offset();
        let offset = self.motion.list_glide.map_or(drawn, |glide| glide.to);
        let rows = self.session.threads.len();
        let target = present::list_scroll_target(offset, index, moved, rows, ROW_PX, view);
        let from = self.motion.glide_from_list(drawn);
        let now = self.motion.clock.now();
        self.motion.list_glide = Glide::start(
            &self.motion.motion,
            from,
            target,
            view * motion::GLIDE_MAX_VIEWPORTS,
            motion::SLIDE,
            now,
        )
        .map(|glide| glide.gentle_beyond(LONG_GLIDE_PX));
        self.set_list_offset(self.motion.list_glide.map_or(target, |glide| glide.from));
    }

    /// [`Session::cancel_pending_keys`], and the read that drops a jump it
    /// cancelled.
    fn cancel_pending_keys(&mut self, cx: &mut Context<Self>) {
        self.session.cancel_pending_keys();
        self.issue_fetch(cx);
    }

    fn select_thread(&mut self, id: String, cx: &mut Context<Self>) {
        self.cancel_pending_keys(cx);
        let changed = self.session.cursor_id.as_deref() != Some(id.as_str());
        self.session.cursor_id = Some(id);
        self.session.pane = Pane::List;
        if changed {
            self.request_messages(true, cx);
        }
        self.scroll_list_to_cursor();
    }
}

impl Render for Inbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let real = Instant::now();
        // Motion time: a frame the compositor held back moves what is
        // moving one step on, not to its end.
        let now = self.motion.clock.frame(real);
        self.frame_now = now;
        let width = f32::from(window.viewport_size().width);
        let panes = present::visible_panes(width, self.session.pane, self.session.full_width);
        self.motion.observe(&self.session, panes, now);
        let viewport_h = f32::from(window.viewport_size().height);
        self.motion
            .fit_card(card_fit(&self.session, viewport_h), now);
        self.advance_glides(now, window.scale_factor());
        let animating = self.motion.tick(now);
        if animating {
            window.request_animation_frame();
        } else {
            self.motion.clock.idle();
        }
        if let Some(log) = &self.frames {
            log.borrow_mut().render_started(real, animating);
        }
        let frames = self.frames.clone();
        let theme = self.theme.clone();
        let list_scroll = self.list_scroll.clone();
        let conv_scroll = self.conv_scroll.clone();
        let palette_scroll = self.palette_scroll.clone();
        let folder_scroll = self.folder_scroll.clone();
        let focus = self.focus.clone();
        let split = self.split;
        let card = self
            .motion
            .card_height(now)
            .map(|height| motion::snap(height, window.scale_factor()));
        // An overlay or which-key a key just closed fades out as it was
        // drawn: its state goes back into the session only while its
        // element is built. It takes no input.
        let mut leaving = Vec::new();
        if let Some((layer, opacity)) = self.motion.closing_which(now) {
            layer.swap(&mut self.session);
            leaving.push(which_key_popup(&theme, &self.session, (opacity, 0.0)).into_any_element());
            layer.swap(&mut self.session);
        }
        if let Some((layer, opacity)) = self.motion.closing_overlay(now) {
            layer.swap(&mut self.session);
            leaving.push(
                overlay(
                    &theme,
                    &self.session,
                    (opacity, 0.0),
                    card,
                    false,
                    &palette_scroll,
                    &folder_scroll,
                    cx,
                )
                .into_any_element(),
            );
            layer.swap(&mut self.session);
        }
        let warm = self.warm_layer(width, panes, animating, window, cx);
        let subject = self.shown_subject();
        shell(
            theme,
            &self.session,
            &self.motion,
            now,
            Shown {
                subject,
                folder: self.list_folder.clone(),
                leaving,
                warm,
                card,
                frames: frames.clone(),
            },
            width,
            split,
            list_scroll,
            conv_scroll,
            palette_scroll,
            folder_scroll,
            focus,
            window,
            cx,
        )
        .when_some(frames, |root, frames| {
            frames.borrow_mut().built(Instant::now());
            let last = frames.clone();
            // Painted last, so it marks the end of the frame's paint.
            root.child(
                gpui::canvas(
                    move |_, _, _| last.borrow_mut().mark(1, Instant::now()),
                    move |_, _, _, _| frames.borrow_mut().painted(Instant::now()),
                )
                .absolute()
                .size_0(),
            )
        })
    }
}

/// A row as the last frame drew it.
struct Drawn {
    row: ThreadRow,
    /// Its top in the viewport, as painted (in flight or not).
    top: f32,
    /// The top of its slot then.
    slot: f32,
}

/// What the frame shows beyond the session.
struct Shown {
    /// The open conversation's subject. It changes with the messages, not
    /// with the selection, so a subject never sits over another thread.
    subject: String,
    /// Overlays fading out, drawn under any open one.
    leaving: Vec<gpui::AnyElement>,
    /// What a `G` will show, drawn unseen under the panes.
    warm: Option<gpui::AnyElement>,
    /// The palette's or folder picker's height as drawn this frame.
    card: Option<f32>,
    /// The folder the rows on screen come from. The header names it, so
    /// after `gs` its name and count change in the frame the rows do.
    folder: String,
    frames: Option<SharedFrameLog>,
}

#[allow(clippy::too_many_arguments)]
fn shell(
    theme: Theme,
    session: &Session,
    motion: &InboxMotion,
    now: Instant,
    shown: Shown,
    width: f32,
    split: f32,
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    palette_scroll: ScrollHandle,
    folder_scroll: ScrollHandle,
    focus: FocusHandle,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    let (show_list, show_conv) = present::visible_panes(width, session.pane, session.full_width);
    let list_width = list_width(width, split);
    let list_entry = motion.pane_entry(Pane::List, now);
    let conv_entry = motion.pane_entry(Pane::Conversation, now);
    let status = motion.status(session, now);
    let Shown {
        subject,
        folder,
        leaving,
        warm,
        card,
        frames,
    } = shown;
    let mut subject = Some(subject);
    // A narrow window's pane that a key just swapped out stays beneath the
    // one coming in, where it was, full width. The incoming pane is drawn
    // on an opaque fill from its first frame, so the old pane shows only
    // in the strip its slide uncovers, and only the new pane's content
    // fades in. The old pane is drawn for paint only: a layer that takes
    // every click and scroll sits on it, strip included, so a click during
    // the swap never selects a row under the conversation.
    let swap = motion.pane_swap((show_list, show_conv), now);
    let swapped_out = swap.map(|swap| {
        let (pane, entry) = (swap.leaving, swap.out);
        let pane = match pane {
            Pane::List => thread_list(&theme, session, false, width, list_scroll.clone(), cx),
            Pane::Conversation => conversation(
                &theme,
                session,
                subject.take().unwrap_or_default(),
                motion,
                now,
                Some(conv_scroll.clone()),
                width,
                // Paint only: the shield over it takes the pointer too.
                Live(false),
                window,
                cx,
            ),
        };
        let (cover_left, cover_width) = swap.cover(width);
        [
            // A flex row, as the panes' own row is, so the pane fills it.
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .flex()
                .flex_row()
                .child(entering(pane, entry))
                .into_any_element(),
            div()
                .id("pane-swap-shield")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .into_any_element(),
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(cover_left))
                .w(px(cover_width))
                .bg(rgb(theme.background))
                .into_any_element(),
        ]
    });
    div()
        .id("fell-root")
        .track_focus(&focus)
        .on_key_down(cx.listener(|inbox, event: &KeyDownEvent, window, cx| {
            inbox.on_key_down(event, window, cx);
        }))
        // The wheel takes over from a glide where the viewport is.
        .on_scroll_wheel(cx.listener(|inbox, _: &gpui::ScrollWheelEvent, _, _| {
            inbox.motion.cancel_glides();
        }))
        .on_mouse_move(cx.listener(|inbox, event: &MouseMoveEvent, window, cx| {
            if !inbox.dragging {
                return;
            }
            let width = f32::from(window.viewport_size().width).max(1.0);
            let x = f32::from(event.position.x);
            inbox.split = (x / width).clamp(0.22, 0.52);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|inbox, _: &MouseUpEvent, _, cx| {
                if inbox.dragging {
                    inbox.dragging = false;
                    let split = inbox.split;
                    cx.background_executor()
                        .spawn(async move { mailbox::save_split(split) })
                        .detach();
                }
            }),
        )
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme.background))
        .text_color(rgb(theme.foreground))
        .text_size(px(theme.ui_px))
        .font_family_opt(theme.ui_family.clone())
        .overflow_hidden()
        .when_some(frames, |root, frames| {
            let first = frames.clone();
            // Prepainted and painted first: marks the end of layout and the
            // start of paint.
            root.child(
                gpui::canvas(
                    move |_, _, _| first.borrow_mut().mark(0, Instant::now()),
                    move |_, _, _, _| frames.borrow_mut().mark(2, Instant::now()),
                )
                .absolute()
                .size_0(),
            )
        })
        .child(header(&theme, session, &folder))
        .child(
            div()
                .relative()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_row()
                .overflow_hidden()
                .children(warm)
                .children(swapped_out.into_iter().flatten())
                .when(show_list, |row| {
                    row.child(entering(
                        thread_list(
                            &theme,
                            session,
                            show_conv,
                            if show_conv { list_width } else { width },
                            list_scroll,
                            cx,
                        ),
                        list_entry,
                    ))
                })
                .when(show_list && show_conv, |row| {
                    row.child(splitter(&theme, cx))
                })
                .when(show_conv, |row| {
                    let conv_width = if show_list {
                        (width - list_width).max(120.0)
                    } else {
                        width
                    };
                    row.child(entering(
                        conversation(
                            &theme,
                            session,
                            subject.take().unwrap_or_default(),
                            motion,
                            now,
                            Some(conv_scroll),
                            conv_width,
                            Live(true),
                            window,
                            cx,
                        ),
                        conv_entry,
                    ))
                }),
        )
        .child(status_bar(&theme, session, status, width, window))
        .children(leaving)
        .when(
            session.which_visible && session.overlay == Overlay::None && !session.chord.is_empty(),
            |root| root.child(which_key_popup(&theme, session, motion.which(now))),
        )
        .when(session.overlay != Overlay::None, |root| {
            root.child(overlay(
                &theme,
                session,
                motion.overlay(now),
                card,
                true,
                &palette_scroll,
                &folder_scroll,
                cx,
            ))
        })
}

/// Whether a conversation drawn takes the pointer. The warm copies
/// ([`Inbox::warm_layer`]) do not: they are drawn under the panes, and
/// GPUI passes a click through to every element under the pointer.
#[derive(Clone, Copy)]
struct Live(bool);

/// A pane that just came into view slides in from its side as it fades in.
fn entering(pane: gpui::Stateful<gpui::Div>, entry: PaneEntry) -> gpui::Stateful<gpui::Div> {
    if entry.opacity >= 1.0 {
        return pane;
    }
    pane.relative()
        .left(px(entry.dx.round()))
        .opacity(entry.opacity)
}

fn header(theme: &Theme, session: &Session, folder: &str) -> gpui::Div {
    let unread = session.unread_threads();
    div()
        .h(px(44.))
        .px(px(16.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.))
        .border_b(px(1.))
        .border_color(rgb(theme.border))
        .bg(rgb(theme.surface))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(session.account_name.clone()),
        )
        .child(
            div()
                .text_color(rgb(theme.dimmed))
                .child(session.account_email.clone()),
        )
        .child(div().flex_1())
        .child(
            div()
                .flex()
                .flex_row()
                .items_baseline()
                .gap(px(6.))
                .min_w(px(0.))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(session.roles.label(folder)),
                )
                // The folder's own name gives way first in a narrow window.
                .children(session.folder_detail(folder).map(|name| {
                    div()
                        .min_w(px(0.))
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_color(rgb(theme.dimmed))
                        .child(name)
                })),
        )
        .child(
            div()
                .flex_none()
                .text_color(rgb(if unread > 0 {
                    theme.accent
                } else {
                    theme.dimmed
                }))
                .child(if unread == 1 {
                    "1 unread".to_string()
                } else {
                    format!("{unread} unread")
                }),
        )
}

fn thread_list(
    theme: &Theme,
    session: &Session,
    beside: bool,
    list_width: f32,
    scroll: UniformListScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    let entity = cx.entity();
    let theme_for_rows = theme.clone();
    let rows = uniform_list(
        "thread-rows",
        session.threads.len(),
        move |range, window, app| {
            entity.update(app, |inbox, cx| {
                // Scrolling near an end of the loaded rows loads more.
                inbox.session.need_rows(Some(range.clone()));
                if inbox.session.has_fetch() {
                    cx.defer_in(window, |inbox, _, cx| inbox.issue_fetch(cx));
                }
                let highlighted = inbox.session.highlighted();
                let text_width = row_text_width(list_width);
                let now = inbox.frame_now;
                let offset = inbox.list_offset();
                let last = inbox.session.threads.len().saturating_sub(1);
                let bar_dx = unread_bar_dx(inbox.motion.pane_entry(Pane::List, now).dx);
                range
                    .map(|index| {
                        let row = inbox.session.threads[index].clone();
                        let selected = highlighted.contains(&row.id);
                        let look = RowLook {
                            selected,
                            fading: inbox
                                .motion
                                .selection_left(&row.id, now)
                                .filter(|_| !selected),
                            ghost: false,
                            bar_dx,
                        };
                        let element =
                            thread_row(&theme_for_rows, &row, index, look, text_width, window, cx);
                        let Some(shift) = inbox.motion.list.as_mut() else {
                            return element;
                        };
                        // Where the row was before the list changed, and the
                        // rows that left from under it.
                        let top = index as f32 * ROW_PX + offset;
                        let below = inbox
                            .session
                            .threads
                            .get(index + 1)
                            .map(|next| (next.id.as_str(), top + ROW_PX));
                        let mut moving = shift.row(&row.id, top, below, now);
                        moving.dy = motion::snap(moving.dy, window.scale_factor());
                        let reach = if index == last { f32::INFINITY } else { ROW_PX };
                        // Ghosts are kept in content pixels, so they scroll.
                        let ghosts = shift.ghosts_in(index as f32 * ROW_PX, reach, now);
                        if moving == RowMotion::STILL && ghosts.is_empty() {
                            return element;
                        }
                        let ghosts: Vec<_> = ghosts
                            .into_iter()
                            .map(|(gone, dy, opacity)| {
                                let dy = motion::snap(dy, window.scale_factor());
                                let look = RowLook {
                                    selected: false,
                                    fading: None,
                                    ghost: true,
                                    bar_dx,
                                };
                                slot_layer(dy, opacity).child(thread_row(
                                    &theme_for_rows,
                                    &gone,
                                    index,
                                    look,
                                    text_width,
                                    window,
                                    cx,
                                ))
                            })
                            .collect();
                        div()
                            .id(gpui::ElementId::named_usize("thread-slot", index))
                            .relative()
                            .h(px(ROW_PX))
                            .w_full()
                            .children(ghosts)
                            .child(
                                slot_layer(moving.dy, moving.opacity)
                                    .when(moving.reveal < 1.0, |layer| {
                                        // Only the part of the gap that has
                                        // opened shows, so the rows sliding
                                        // down never cover it.
                                        let shown = motion::snap(
                                            ROW_PX * moving.reveal,
                                            window.scale_factor(),
                                        );
                                        layer.h(px(shown)).overflow_hidden()
                                    })
                                    .child(element),
                            )
                            .into_any_element()
                    })
                    .collect::<Vec<_>>()
            })
        },
    )
    .flex_1()
    .min_h(px(0.))
    .track_scroll(scroll);
    let mut pane = div()
        .id("thread-list")
        .flex()
        .flex_col()
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_hidden()
        .border_l(px(LIST_BORDER_PX))
        .border_color(rgb(theme.border));
    pane = if beside {
        pane.w(px(list_width)).flex_none()
    } else {
        pane.flex_1()
    };
    if session.threads.is_empty() {
        pane.child(empty_state(theme, session))
    } else {
        pane.child(rows)
    }
}

/// The list's width beside the conversation in a window `width` wide.
fn list_width(width: f32, split: f32) -> f32 {
    (width * split).clamp(280.0, (width - 400.0).max(280.0))
}

/// The width of a row's text in a list `list_width` wide: less the
/// border, the unread bar and the row's padding.
fn row_text_width(list_width: f32) -> f32 {
    (list_width - LIST_BORDER_PX - 3.0 - 24.0).max(48.0)
}

/// One layer of a row slot while the list animates: a row drawn `dy` from
/// its slot.
fn slot_layer(dy: f32, opacity: f32) -> gpui::Div {
    div()
        .absolute()
        .top(px(dy))
        .left(px(0.))
        .right(px(0.))
        .h(px(ROW_PX))
        .opacity(opacity)
}

/// How a thread row is drawn.
#[derive(Clone, Copy)]
struct RowLook {
    selected: bool,
    /// The selection just left this row: how much of its highlight shows.
    fading: Option<f32>,
    /// A row that left the list, fading where it was. Not clickable.
    ghost: bool,
    /// How far right of its place the unread bar is drawn ([`unread_bar_dx`]).
    bar_dx: f32,
}

/// The list's left border, which the unread bars sit just inside.
const LIST_BORDER_PX: f32 = 2.0;

/// A list sliding in from the left by `pane_dx` (negative) would carry the
/// unread bars at its left edge outside the window until the last frames
/// of the slide, so they would pop in at full colour. They are drawn at
/// the window's edge instead until the list brings them past it: this is
/// how far right of their place that is.
fn unread_bar_dx(pane_dx: f32) -> f32 {
    (-(pane_dx.round() + LIST_BORDER_PX)).max(0.0)
}

fn thread_row(
    theme: &Theme,
    row: &ThreadRow,
    index: usize,
    look: RowLook,
    text_width: f32,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let selected = look.selected;
    let id = row.id.clone();
    let element_id = if look.ghost {
        gpui::ElementId::Name(format!("ghost-{}", row.id).into())
    } else {
        gpui::ElementId::named_usize("thread", index)
    };
    let background = match look.fading {
        _ if selected => theme.selection,
        Some(left) => motion::mix(theme.background, theme.selection, left),
        None => theme.background,
    };
    let weight = if row.unread {
        FontWeight::BOLD
    } else {
        FontWeight::NORMAL
    };
    div()
        .id(element_id)
        .h(px(ROW_PX))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .bg(rgb(background))
        .text_color(rgb(if selected {
            theme.on_selection
        } else {
            theme.foreground
        }))
        .when(!look.ghost, |row| {
            row.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |inbox, _, window, cx| {
                    window.focus(&inbox.focus);
                    inbox.select_thread(id.clone(), cx);
                    cx.notify();
                }),
            )
        })
        // Shorter than the row, so bars on adjacent unread rows stay apart.
        // A read row keeps the gap but no fill, so a selected row's
        // highlight shows through it.
        .child(
            div()
                .w(px(3.))
                .h(px(44.))
                .rounded_r_sm()
                .when(look.bar_dx > 0.0, |bar| {
                    bar.relative().left(px(look.bar_dx))
                })
                .when(row.unread, |bar| bar.bg(rgb(theme.accent))),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .px(px(12.))
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(3.))
                .child(thread_line_one(theme, row, weight, selected))
                .child(thread_line_two(
                    theme, row, weight, selected, text_width, window,
                )),
        )
        .into_any_element()
}

fn thread_line_one(
    theme: &Theme,
    row: &ThreadRow,
    weight: FontWeight,
    selected: bool,
) -> gpui::Div {
    let dim = if selected {
        theme.on_selection
    } else {
        theme.dimmed
    };
    div()
        .h(px(22.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .min_w(px(0.))
        .overflow_hidden()
        .child(names(theme, &row.spans, row.extra, weight, selected))
        .when(row.message_count > 1, |line| {
            line.child(
                div()
                    .text_color(rgb(dim))
                    .text_size(px(theme.ui_px - 1.0))
                    .child(row.message_count.to_string()),
            )
        })
        .when(row.starred, |line| {
            line.child(div().text_color(rgb(theme.star)).child("★"))
        })
        .when(row.has_attachment, |line| {
            line.child(div().text_color(rgb(dim)).child("⧉"))
        })
        .children(row.labels.iter().take(2).enumerate().map(|(index, label)| {
            let warn = row.warn && index == 0;
            div()
                .flex_shrink_0()
                .px(px(6.))
                .py(px(1.))
                .rounded_md()
                .border_1()
                .border_color(rgb(if warn { theme.error } else { theme.border }))
                .text_size(px((theme.ui_px - 2.0).max(11.0)))
                .text_color(rgb(if warn { theme.error } else { dim }))
                .child(label.clone())
        }))
        .child(div().flex_1())
        .child(
            div()
                .text_color(rgb(dim))
                .text_size(px(theme.ui_px - 1.0))
                .child(present::relative_date(now_unix(), row.date_unix)),
        )
}

fn names(
    theme: &Theme,
    spans: &[NameSpan],
    extra: usize,
    weight: FontWeight,
    selected: bool,
) -> gpui::Div {
    let dim = if selected {
        theme.on_selection
    } else {
        theme.dimmed
    };
    let mut line = div().flex().flex_row().min_w(px(0.)).overflow_hidden();
    for (index, span) in spans.iter().enumerate() {
        let span_weight = if weight == FontWeight::BOLD {
            FontWeight::BOLD
        } else if span.emphasis {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };
        let last = index + 1 == spans.len() && extra == 0;
        let text = if last {
            span.text.clone()
        } else {
            format!("{}, ", span.text)
        };
        line = line.child(
            div()
                .font_weight(span_weight)
                .text_color(rgb(if selected {
                    theme.on_selection
                } else {
                    theme.foreground
                }))
                .child(text),
        );
    }
    if extra > 0 {
        line = line.child(div().text_color(rgb(dim)).child(format!("+{extra}")));
    }
    line
}

fn thread_line_two(
    theme: &Theme,
    row: &ThreadRow,
    weight: FontWeight,
    selected: bool,
    text_width: f32,
    window: &Window,
) -> gpui::Div {
    let snippet_color = if selected {
        theme.on_selection_dim
    } else {
        theme.dimmed
    };
    let subject_w = line_px(window, &row.subject, weight).min(280.0);
    let snippet_w = (text_width - subject_w - 8.0).max(24.0);
    // Shaped once here and drawn from that shaping: a row that comes into
    // view shapes each of its two texts at most twice.
    let subject_color = if selected {
        theme.on_selection
    } else {
        theme.foreground
    };
    let subject = clip_shaped(
        window,
        &row.subject,
        subject_w.min(text_width),
        weight,
        Some(subject_color),
    );
    let snippet = clip_shaped(
        window,
        &row.snippet,
        snippet_w,
        FontWeight::NORMAL,
        Some(snippet_color),
    );
    div()
        .w(px(text_width))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .overflow_hidden()
        .children(subject.map(|line| shaped_text(line, window)))
        .children(snippet.map(|line| {
            div()
                .w(px(snippet_w))
                .overflow_hidden()
                .child(shaped_text(line, window))
        }))
}

fn line_px(window: &Window, text: &str, weight: FontWeight) -> f32 {
    shape_ui_line(window, text, weight, None).map_or(0.0, |line| f32::from(line.width))
}

/// `text` shaped as one line in the UI font at `weight`, in `color` (the
/// inherited one when `None`).
fn shape_ui_line(
    window: &Window,
    text: &str,
    weight: FontWeight,
    color: Option<u32>,
) -> Option<gpui::ShapedLine> {
    if text.is_empty() {
        return None;
    }
    let mut style = window.text_style();
    style.font_weight = weight;
    if let Some(color) = color {
        style.color = rgb(color).into();
    }
    let font_px = style.font_size.to_pixels(window.rem_size());
    let key = LineKey {
        text: text.to_string(),
        font: style.font(),
        px: f32::from(font_px).to_bits(),
        color: style.color,
    };
    if let Some(line) = SHAPED.with(|lines| lines.borrow_mut().get(&key)) {
        return Some(line);
    }
    let run = style.to_run(text.len());
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        font_px,
        &[run],
        None,
    );
    SHAPED.with(|lines| lines.borrow_mut().insert(key, line.clone()));
    Some(line)
}

/// What a line was shaped from: its text, font, size and colour.
#[derive(Clone, PartialEq, Eq, Hash)]
struct LineKey {
    text: String,
    font: gpui::Font,
    px: u32,
    color: gpui::Hsla,
}

/// Lines the rows and the status bar shaped, kept across frames. GPUI keeps
/// a line's shaping only while every frame draws it, so a folder the reader
/// comes back to (`gs` then `gi`) or rows that scroll back into view would
/// be shaped again on the frame they appear: 10–20 ms, more than a frame.
/// Two generations: when the newer one fills up it becomes the older, and
/// lines still drawn move back into the newer one as they are used.
#[derive(Default)]
struct ShapedLines {
    recent: std::collections::HashMap<LineKey, gpui::ShapedLine>,
    older: std::collections::HashMap<LineKey, gpui::ShapedLine>,
}

impl ShapedLines {
    /// Lines kept in each generation.
    const KEEP: usize = 4096;

    fn get(&mut self, key: &LineKey) -> Option<gpui::ShapedLine> {
        if let Some(line) = self.recent.get(key) {
            return Some(line.clone());
        }
        let line = self.older.remove(key)?;
        self.insert(key.clone(), line.clone());
        Some(line)
    }

    fn insert(&mut self, key: LineKey, line: gpui::ShapedLine) {
        if self.recent.len() >= Self::KEEP {
            self.older = std::mem::take(&mut self.recent);
        }
        self.recent.insert(key, line);
    }
}

thread_local! {
    /// The UI thread's [`ShapedLines`]; only the UI thread draws.
    static SHAPED: std::cell::RefCell<ShapedLines> = std::cell::RefCell::default();
}

/// Shorten one line to `max_px`, ending on an ellipsis that reaches that width.
fn clip_line(window: &Window, text: &str, max_px: f32, weight: FontWeight) -> String {
    clip_shaped(window, text, max_px, weight, None)
        .map_or_else(String::new, |line| line.text.to_string())
}

/// [`clip_line`], shaped and ready to paint ([`shaped_text`]), so the row
/// does not shape the same text again to draw it.
///
/// The text is shaped once, whole (only as much of a long snippet as could
/// ever fit), and cut from that shaping at a cluster the shaper made, so an
/// emoji sequence or a flag is never split, and by logical index, so a
/// right-to-left subject keeps its first words ([`text_cut::LineCuts`]).
/// The cut line is shaped once more to draw; only when kerning makes it
/// wider than the room does it shape again, a cluster shorter at least.
fn clip_shaped(
    window: &Window,
    text: &str,
    max_px: f32,
    weight: FontWeight,
    color: Option<u32>,
) -> Option<gpui::ShapedLine> {
    // No glyph of a UI font is narrower than a fifth of an em, so more
    // characters than this can never fit.
    let font_px = f32::from(window.text_style().font_size.to_pixels(window.rem_size()));
    let most = (max_px / (font_px * 0.2).max(1.0)).ceil() as usize + 2;
    let probe = match text.char_indices().nth(most) {
        Some((cut, _)) => &text[..cut],
        None => text,
    };
    let mut line = shape_ui_line(window, probe, weight, color)?;
    if f32::from(line.width) <= max_px {
        if probe.len() == text.len() {
            return Some(line);
        }
        // Narrower glyphs than any font has: measure the whole line.
        line = shape_ui_line(window, text, weight, color)?;
        if f32::from(line.width) <= max_px {
            return Some(line);
        }
    }
    let shaped = line.text.clone();
    let cuts = text_cut::LineCuts::new(
        line.runs.iter().flat_map(|run| {
            run.glyphs
                .iter()
                .map(|glyph| (glyph.index, f32::from(glyph.position.x)))
        }),
        f32::from(line.width),
        shaped.len(),
    );
    let mut room = max_px - line_px(window, "…", weight);
    // The whole line does not fit: the cut is before its end.
    let mut before = shaped.len();
    loop {
        let kept = text_cut::kept(&shaped[..cuts.fit(room, before)]);
        let cut = shape_ui_line(window, &format!("{kept}…"), weight, color)?;
        if kept.is_empty() || f32::from(cut.width) <= max_px {
            return Some(cut);
        }
        // Kerning made the cut line wider than its prefix was inside the
        // whole line: take that much more off, and cut shorter than this.
        room -= f32::from(cut.width) - max_px;
        before = kept.len();
    }
}

/// A line [`clip_shaped`] already shaped, drawn as it is.
fn shaped_text(line: gpui::ShapedLine, window: &Window) -> gpui::Canvas<()> {
    let height = window.text_style().line_height_in_pixels(window.rem_size());
    let width = line.width;
    gpui::canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            let _ = line.paint(bounds.origin, height, window, cx);
        },
    )
    .w(width)
    .h(height)
    .flex_none()
}

/// The width of `text` as the status bar draws it: its size and family.
fn bar_px(window: &Window, text: &str, size: f32, family: Option<&String>) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let mut style = window.text_style();
    if let Some(family) = family {
        style.font_family = SharedString::from(family.clone());
    }
    let run = style.to_run(text.len());
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        px(size),
        &[run],
        None,
    );
    f32::from(line.width)
}

fn empty_state(theme: &Theme, session: &Session) -> gpui::Div {
    let banner = session.list_banner();
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .px(px(24.))
        .child(
            div().text_color(rgb(theme.foreground)).child(
                banner
                    .as_ref()
                    .map(|item| item.title.clone())
                    .unwrap_or_default(),
            ),
        )
        .child(
            div().text_color(rgb(theme.dimmed)).child(
                banner
                    .as_ref()
                    .map(|item| item.hint.clone())
                    .unwrap_or_default(),
            ),
        )
}

fn splitter(theme: &Theme, cx: &mut Context<Inbox>) -> impl IntoElement {
    div()
        .id("split")
        .w(px(7.))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        // The line sits on the list's edge, so the selection meets it; the
        // rest of the 7 px is grab area.
        .justify_start()
        .cursor_col_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|inbox, _, _, cx| {
                inbox.dragging = true;
                cx.notify();
            }),
        )
        .child(div().w(px(1.)).h_full().bg(rgb(theme.border)))
}

#[allow(clippy::too_many_arguments)]
fn conversation(
    theme: &Theme,
    session: &Session,
    subject: String,
    motion: &InboxMotion,
    now: Instant,
    // `None`: a copy drawn unseen, which neither scrolls nor takes the wheel.
    scroll: Option<ScrollHandle>,
    conv_width: f32,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    // The card under the cursor shows it as much as the pane is focused,
    // so on Enter and Esc its background turns with the focus edge.
    let focus = motion.focus_edge(session.pane, now);
    let arrival = motion.arrival(now);
    let messages: Vec<_> = session
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let cursor = if session.message_cursor == index {
                focus
            } else {
                0.0
            };
            message_block(
                theme, session, message, index, cursor, conv_width, live, window, cx,
            )
        })
        .collect();
    div()
        .id("conversation")
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .border_l(px(2.))
        .border_color(rgb(motion::mix(
            theme.background,
            theme.accent,
            motion.focus_edge(session.pane, now),
        )))
        .when(!subject.is_empty(), |pane| {
            pane.child(
                div()
                    .px(px(28.))
                    .pt(px(22.))
                    .pb(px(14.))
                    .text_size(px(theme.ui_px + 11.0))
                    .font_weight(FontWeight::BOLD)
                    .line_height(px(theme.ui_px + 16.0))
                    .border_b(px(1.))
                    .border_color(rgb(theme.border))
                    .child(
                        div()
                            .when(arrival < 1.0, |text| text.opacity(arrival))
                            .child(subject),
                    ),
            )
        })
        .child(
            div()
                .id("conversation-scroll")
                .flex_1()
                .min_h(px(0.))
                .map(|body| match &scroll {
                    Some(scroll) => body.overflow_y_scroll().track_scroll(scroll),
                    None => body.overflow_hidden(),
                })
                .px(px(20.))
                .pb(px(28.))
                .when(arrival < 1.0, |body| body.opacity(arrival))
                .children(messages),
        )
}

/// `cursor`: how much the card shows the message cursor, 0 to 1.
#[allow(clippy::too_many_arguments)]
fn message_block(
    theme: &Theme,
    session: &Session,
    message: &ConvMessage,
    index: usize,
    cursor: f32,
    conv_width: f32,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    if message.expanded {
        expanded_message(
            theme, session, message, index, cursor, conv_width, live, window, cx,
        )
    } else {
        collapsed_message(theme, message, index, cursor, conv_width, live, window, cx)
    }
}

#[allow(clippy::too_many_arguments)]
fn collapsed_message(
    theme: &Theme,
    message: &ConvMessage,
    index: usize,
    cursor: f32,
    conv_width: f32,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let date = present::relative_date(now_unix(), message.date_unix);
    let dimmed = motion::mix(theme.dimmed, theme.on_selection, cursor);
    let date_w = line_px(window, &date, FontWeight::NORMAL);
    // Sender column, gaps, row padding, and a little air before the date.
    let snippet_w = (conv_width - 56.0 - 140.0 - date_w).max(24.0);
    let snippet = clip_line(window, &message.snippet, snippet_w, FontWeight::NORMAL);
    div()
        .id(gpui::ElementId::named_usize("message", index))
        .h(px(40.))
        .w_full()
        .min_w(px(0.))
        .px(px(8.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .overflow_hidden()
        .rounded_md()
        .bg(rgb(motion::mix(theme.background, theme.selection, cursor)))
        .text_color(rgb(motion::mix(
            theme.foreground,
            theme.on_selection,
            cursor,
        )))
        .when(live.0, |row| {
            row.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |inbox, _, window, cx| {
                    window.focus(&inbox.focus);
                    inbox.cancel_pending_keys(cx);
                    inbox.session.pane = Pane::Conversation;
                    inbox.session.message_cursor = index;
                    if let Some(message) = inbox.session.messages.get_mut(index) {
                        message.expanded = true;
                    }
                    cx.notify();
                }),
            )
        })
        .child(
            div()
                .w(px(140.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .font_weight(FontWeight::MEDIUM)
                .child(message.sender.clone()),
        )
        .child(
            div()
                .w(px(snippet_w))
                .flex_none()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(rgb(dimmed))
                .child(snippet),
        )
        .child(div().flex_none().text_color(rgb(dimmed)).child(date))
        .into_any_element()
}

#[allow(clippy::too_many_arguments)]
fn expanded_message(
    theme: &Theme,
    session: &Session,
    message: &ConvMessage,
    index: usize,
    cursor: f32,
    conv_width: f32,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let quotes = session.is_open(index);
    let dim = theme.dimmed;
    div()
        .id(gpui::ElementId::named_usize("message", index))
        .px(px(8.))
        .py(px(10.))
        .mb(px(8.))
        .rounded_md()
        .bg(rgb(motion::mix(theme.background, theme.surface, cursor)))
        .when(live.0, |card| {
            card.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |inbox, _, window, cx| {
                    window.focus(&inbox.focus);
                    inbox.cancel_pending_keys(cx);
                    inbox.session.pane = Pane::Conversation;
                    inbox.session.message_cursor = index;
                    cx.notify();
                }),
            )
        })
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                // The recipient line belongs to this header, so it sits close.
                .pb(px(if message.recipients.is_some() { 2. } else { 8. }))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(message.sender.clone()),
                )
                .child(
                    div()
                        .text_color(rgb(dim))
                        .text_size(px(theme.ui_px - 1.0))
                        .child(message.email.clone()),
                )
                .when(!message.folders.is_empty(), |row| {
                    // A draft that may have gone out says so as its row does.
                    let warn = message.folders.contains("may have been sent");
                    row.child(
                        div()
                            .text_color(rgb(if warn { theme.error } else { dim }))
                            .text_size(px(theme.ui_px - 1.0))
                            .child(message.folders.clone()),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(rgb(dim))
                        .child(present::relative_date(now_unix(), message.date_unix)),
                ),
        )
        .children(message.recipients.as_ref().map(|line| {
            let open = session.recipients_are_open(index);
            recipient_lines(theme, line, open, index, conv_width, live, window, cx)
        }))
        .children({
            // `q` opens every collapsed block in the message at once, so
            // only the first one names the key.
            // The recipient line comes first and names the key itself.
            let first_hidden = message
                .body
                .blocks
                .iter()
                .position(|block| !matches!(block, BodyBlock::Text(_)))
                .filter(|_| message.recipients.is_none());
            message
                .body
                .blocks
                .iter()
                .enumerate()
                .map(|(at, block)| {
                    body_block(
                        theme,
                        block,
                        quotes,
                        Some(at) == first_hidden,
                        index,
                        live,
                        cx,
                    )
                })
                .collect::<Vec<_>>()
        })
        .into_any_element()
}

/// Who else got the message, under its sender. Closed: one line of names
/// that never wraps, then "+N more" and the key. Open (`q`, or a click): a
/// row per group with every address.
#[allow(clippy::too_many_arguments)]
fn recipient_lines(
    theme: &Theme,
    line: &present::RecipientLine,
    open: bool,
    index: usize,
    conv_width: f32,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let toggle = cx.listener(move |inbox, _, window, cx| {
        window.focus(&inbox.focus);
        inbox.cancel_pending_keys(cx);
        inbox.session.pane = Pane::Conversation;
        inbox.session.message_cursor = index;
        inbox.session.toggle_recipients(index);
        cx.notify();
    });
    let size = px(theme.ui_px - 1.0);
    let body = if open {
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(line.groups.iter().enumerate().map(|(row, group)| {
                let last_row = row + 1 == line.groups.len();
                div()
                    .flex()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex_shrink_0()
                            .w(px(28.))
                            .text_color(rgb(theme.dimmed))
                            .child(group.label),
                    )
                    .child({
                        // Each person is one piece that never breaks, with
                        // its comma attached, so a line never starts with an
                        // address or a comma. The row wraps between pieces.
                        let last = group.people.len().saturating_sub(1);
                        div()
                            .flex()
                            .flex_wrap()
                            .flex_1()
                            .min_w(px(0.))
                            .gap_x(px(6.))
                            .text_color(rgb(theme.foreground))
                            .children(group.people.iter().enumerate().map(|(at, person)| {
                                // The name leads; the address is quieter.
                                let (name, address) = match person.find(" <") {
                                    Some(split) => (&person[..split], &person[split..]),
                                    None => (person.as_str(), ""),
                                };
                                let comma = if at < last { "," } else { "" };
                                div()
                                    .flex()
                                    .min_w(px(0.))
                                    .whitespace_nowrap()
                                    .overflow_hidden()
                                    .child(name.to_string())
                                    .child(
                                        div()
                                            .text_color(rgb(theme.dimmed))
                                            .child(format!("{address}{comma}")),
                                    )
                            }))
                            // One dim note ends the row, so its separators
                            // space evenly: "only you · q to hide".
                            .children({
                                let note = match (group.only_you, last_row) {
                                    (true, true) => Some("only you · q to hide"),
                                    (true, false) => Some("only you"),
                                    (false, true) => Some("· q to hide"),
                                    (false, false) => None,
                                };
                                note.map(|note| {
                                    div()
                                        .whitespace_nowrap()
                                        .text_color(rgb(theme.dimmed))
                                        .child(note)
                                })
                            })
                    })
            }))
    } else {
        // Whole people only, measured like the thread list, so the count
        // always matches the names on screen; the Bcc and key always show.
        // Only a single name too long for the pane is cut, with an ellipsis.
        // The text takes the box's colour, so hover reaches it.
        let room = (conv_width - 48.0).min(680.0);
        let (names, tail, _) = present::fit_recipients(line, |names, tail| {
            line_px(window, names, FontWeight::NORMAL) + line_px(window, tail, FontWeight::NORMAL)
                <= room
        });
        let names = clip_line(
            window,
            &names,
            (room - line_px(window, &tail, FontWeight::NORMAL)).max(40.0),
            FontWeight::NORMAL,
        );
        div()
            .flex()
            .min_w(px(0.))
            .text_color(rgb(theme.dimmed))
            .when(live.0, |names| {
                names.hover(|style| style.text_color(rgb(theme.foreground)))
            })
            .child(div().whitespace_nowrap().child(names))
            .child(div().flex_shrink_0().whitespace_nowrap().child(tail))
    };
    div()
        .id(gpui::ElementId::named_usize("recipients", index))
        .max_w(px(680.))
        .pb(px(10.))
        .text_size(size)
        .child(body.when(live.0, |body| {
            body.cursor_pointer()
                .on_mouse_down(MouseButton::Left, toggle)
        }))
        .into_any_element()
}

fn body_block(
    theme: &Theme,
    block: &BodyBlock,
    quotes_open: bool,
    show_key: bool,
    index: usize,
    live: Live,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    match block {
        BodyBlock::Text(text) => div()
            .max_w(px(680.))
            .pb(px(8.))
            .line_height(px(theme.ui_px * 1.45))
            .child(text.clone())
            .into_any_element(),
        BodyBlock::Quote(text) | BodyBlock::Signature(text) => {
            let label = if matches!(block, BodyBlock::Quote(_)) {
                "Quoted text"
            } else {
                "Signature"
            };
            if quotes_open {
                div()
                    .max_w(px(680.))
                    .pl(px(10.))
                    .mb(px(8.))
                    .border_l(px(2.))
                    .border_color(rgb(theme.border))
                    .text_color(rgb(theme.dimmed))
                    .line_height(px(theme.ui_px * 1.4))
                    .child(text.clone())
                    .into_any_element()
            } else {
                div()
                    .h(px(28.))
                    .mb(px(6.))
                    .flex()
                    .items_center()
                    .text_color(rgb(theme.dimmed))
                    .child(if show_key {
                        format!("{label} · q")
                    } else {
                        label.to_string()
                    })
                    .when(live.0, |label| {
                        label.on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |inbox, _, window, cx| {
                                window.focus(&inbox.focus);
                                inbox.cancel_pending_keys(cx);
                                inbox.session.pane = Pane::Conversation;
                                inbox.session.message_cursor = index;
                                inbox.session.toggle_quotes(index);
                                cx.notify();
                            }),
                        )
                    })
                    .into_any_element()
            }
        }
    }
}

/// Mode, sync state and one notice. At rest there are no key hints
/// (docs/ux.md, Quiet): the right side shows only what the reader is
/// typing, a count or a chord's prefix, while which-key lists the rest.
fn status_bar(
    theme: &Theme,
    session: &Session,
    status: StatusMotion,
    width: f32,
    window: &Window,
) -> gpui::Div {
    // A notice takes the room of the keys being typed (a count, a chord's
    // prefix), entering or leaving, so its text never re-flows as it fades.
    // Those keys fade under it at the right.
    let noticed = status.notice.is_some() || status.leaving.is_some();
    let notice_opacity = status
        .notice
        .iter()
        .chain(status.leaving.iter())
        .map(|(_, opacity)| *opacity)
        .fold(0.0, f32::max);
    let hint = if session.chord.is_empty() {
        String::new()
    } else {
        session.prefix_text()
    };
    let count = if session.count.is_empty() {
        String::new()
    } else {
        format!("{} ", session.count)
    };
    let size = (theme.ui_px - 1.0).max(12.0);
    let ui = theme.ui_family.as_ref();
    let mono = theme.mono_family.as_ref();
    let hint_opacity = status.hints;
    // What the bar spends before the notice, measured as drawn: the
    // padding, the mode in mono, sync, the dots, the gaps between the
    // children. No typed keys show beside a notice, so no air after it.
    let mut used = 28.0 + 4.0 * 12.0;
    used += bar_px(window, session.mode_label(), size, mono)
        + bar_px(window, &session.sync.status(), size, ui)
        + 2.0 * bar_px(window, "·", size, ui);
    if session.detail_text().is_some() {
        used += 12.0 + bar_px(window, "· ? details", size, ui);
    }
    let scale = window.scale_factor();
    let notice_text = |toast: &Toast, opacity: f32, dy: f32| {
        let text = present::fit_notice(&toast.text, (width - used).max(80.0), |text| {
            bar_px(window, text, size, ui)
        });
        div()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_color(rgb(if toast.error {
                theme.error
            } else {
                theme.foreground
            }))
            .when(opacity < 1.0, |text| text.opacity(opacity))
            .when(dy.abs() >= 0.01, |text| {
                text.relative().top(px(motion::snap(dy, scale)))
            })
            .child(text)
    };
    let (notice_dy, leaving_dy) = (status.notice_dy, status.leaving_dy);
    let hints = (!count.is_empty() || !hint.is_empty()).then(|| {
        div()
            .font_family_opt(theme.mono_family.clone())
            .text_color(rgb(if session.chord.is_empty() {
                theme.dimmed
            } else {
                theme.accent
            }))
            .when(hint_opacity < 1.0, |hints| hints.opacity(hint_opacity))
            .child(format!("{count}{hint}"))
    });
    let (current, leaving) = (status.notice, status.leaving);
    div()
        .relative()
        .h(px(30.))
        .px(px(14.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .border_t(px(1.))
        .border_color(rgb(theme.border))
        .bg(rgb(theme.surface))
        .text_size(px(size))
        .child(
            div()
                .font_family_opt(theme.mono_family.clone())
                .text_color(rgb(if session.visual {
                    theme.accent
                } else {
                    theme.dimmed
                }))
                .child(session.mode_label().to_string()),
        )
        .child(div().text_color(rgb(theme.dimmed)).child("·"))
        .child(
            div()
                .text_color(rgb(if session.sync.is_error() {
                    theme.error
                } else {
                    theme.dimmed
                }))
                .child(session.sync.status()),
        )
        .when(session.detail_text().is_some(), |bar| {
            bar.child(div().text_color(rgb(theme.error)).child("· ? details"))
        })
        // The dot before a notice comes and goes with it.
        .when(noticed, |bar| {
            bar.child(
                div()
                    .text_color(rgb(theme.dimmed))
                    .when(notice_opacity < 1.0, |dot| dot.opacity(notice_opacity))
                    .child("·"),
            )
        })
        .child(
            div()
                .relative()
                .flex_1()
                .min_w(px(0.))
                // Air before the typed keys, so a long message never runs into them.
                .when(!noticed, |cell| cell.pr(px(16.)))
                .overflow_hidden()
                .map(|cell| match (current, leaving) {
                    (Some((toast, opacity)), Some((old, gone))) => cell
                        .child(notice_text(&toast, opacity, notice_dy))
                        // The notice it replaced fades out over it.
                        .child(
                            div()
                                .absolute()
                                .top(px(0.))
                                .bottom(px(0.))
                                .left(px(0.))
                                .right(px(0.))
                                .flex()
                                .items_center()
                                .child(notice_text(&old, gone, leaving_dy)),
                        ),
                    (Some((toast, opacity)), None) => {
                        cell.child(notice_text(&toast, opacity, notice_dy))
                    }
                    (None, Some((toast, opacity))) => {
                        cell.child(notice_text(&toast, opacity, leaving_dy))
                    }
                    (None, None) => cell,
                }),
        )
        .map(|bar| match hints {
            Some(hints) if !noticed => bar.child(hints),
            // Beside a notice the hints sit where they would be, out of its
            // way, while they fade.
            Some(hints) if hint_opacity > 0.0 => bar.child(
                div()
                    .absolute()
                    .top(px(0.))
                    .bottom(px(0.))
                    .right(px(14.))
                    .flex()
                    .items_center()
                    .child(hints),
            ),
            _ => bar,
        })
}

/// `(opacity, rise)`: the popup fades in as it rises the last few pixels.
fn which_key_popup(theme: &Theme, session: &Session, (opacity, rise): (f32, f32)) -> gpui::Div {
    let items = session.which_items();
    div()
        .absolute()
        .bottom(px(40. - rise))
        .opacity(opacity)
        .left(px(16.))
        .min_w(px(220.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .px(px(12.))
        .py(px(10.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .pb(px(4.))
                .text_color(rgb(theme.dimmed))
                .font_family_opt(theme.mono_family.clone())
                .child(session.prefix_text()),
        )
        .children(items.into_iter().map(|item| {
            div()
                .flex()
                .flex_row()
                .gap(px(12.))
                .child(
                    div()
                        .w(px(72.))
                        .font_family_opt(theme.mono_family.clone())
                        .text_color(rgb(theme.accent))
                        .child(item.key),
                )
                .child(div().child(item.label))
        }))
}

const PALETTE_PX: f32 = 560.0;
const PICKER_PX: f32 = 420.0;
const PALETTE_MAX_PX: f32 = 420.0;
const PICKER_MAX_PX: f32 = 380.0;
const PALETTE_ROW_PX: f32 = 36.0;
const PICKER_ROW_PX: f32 = 34.0;
/// Where the palette, folder picker and help card sit below the window's top.
const CARD_TOP_PX: f32 = 72.0;

/// How tall the open palette or folder picker should be to hold its items
/// (its border, the 44 px query line and 4 px above and below the rows),
/// up to its maximum and the window. `None` for any other overlay.
fn card_fit(session: &Session, viewport_h: f32) -> Option<f32> {
    let (rows, row_px, max) = match session.overlay {
        Overlay::Palette => (session.palette().len(), PALETTE_ROW_PX, PALETTE_MAX_PX),
        Overlay::Folders => (session.folder_choices().len(), PICKER_ROW_PX, PICKER_MAX_PX),
        _ => return None,
    };
    Some(present::card_height(
        rows,
        row_px,
        max,
        viewport_h - CARD_TOP_PX - 16.0,
    ))
}
const HELP_PX: f32 = 520.0;

/// `(opacity, drop)`: the card fades in as it settles the last few pixels
/// down; its width never changes, so nothing in it re-flows. `card`: the
/// palette's or folder picker's height, easing to fit its items. `live`: false
/// for one fading out after it closed, which takes no input: clicks go
/// through to what is under it.
#[allow(clippy::too_many_arguments)]
fn overlay(
    theme: &Theme,
    session: &Session,
    (opacity, drop): (f32, f32),
    card: Option<f32>,
    live: bool,
    palette_scroll: &ScrollHandle,
    folder_scroll: &ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .right(px(0.))
        .bottom(px(0.))
        .flex()
        .justify_center()
        .pt(px(CARD_TOP_PX - drop))
        .when(live, |layer| {
            layer.occlude().on_mouse_down(
                MouseButton::Left,
                cx.listener(|inbox, _, _, cx| {
                    inbox.session.overlay = Overlay::None;
                    cx.notify();
                }),
            )
        })
        .child(match session.overlay {
            Overlay::Palette => palette(theme, session, palette_scroll, cx)
                .when_some(card, |card, height| card.h(px(height)))
                .opacity(opacity)
                .into_any_element(),
            Overlay::Folders => folder_picker(theme, session, folder_scroll, cx)
                .when_some(card, |card, height| card.h(px(height)))
                .opacity(opacity)
                .into_any_element(),
            Overlay::Help => help(theme, session, cx).opacity(opacity).into_any_element(),
            Overlay::None => div().into_any_element(),
        })
}

fn palette(
    theme: &Theme,
    session: &Session,
    scroll: &ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let items = session.palette();
    let selected = session.palette_index;
    div()
        .w(px(PALETTE_PX))
        .max_h(px(PALETTE_MAX_PX))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            // A card fading out after it closed lets clicks through.
            cx.listener(|inbox, _, _, cx| {
                if inbox.session.overlay != Overlay::None {
                    cx.stop_propagation();
                }
            }),
        )
        .child(
            div()
                .h(px(44.))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .child(div().text_color(rgb(theme.accent)).child(":"))
                .child(div().child(session.palette_query.clone()))
                .child(div().w(px(2.)).h(px(16.)).bg(rgb(theme.accent))),
        )
        .child(
            div()
                .id("palette-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .py(px(4.))
                .children(items.iter().enumerate().map(|(index, item)| {
                    let action = item.action;
                    div()
                        .id(gpui::ElementId::named_usize("palette", index))
                        .h(px(PALETTE_ROW_PX))
                        .mx(px(4.))
                        .px(px(10.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .rounded_md()
                        .bg(rgb(if index == selected {
                            theme.selection
                        } else {
                            theme.surface
                        }))
                        .text_color(rgb(if index == selected {
                            theme.on_selection
                        } else {
                            theme.foreground
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |inbox, _, window, cx| {
                                // A palette fading out takes no clicks.
                                if inbox.session.overlay != Overlay::Palette {
                                    return;
                                }
                                cx.stop_propagation();
                                window.focus(&inbox.focus);
                                inbox.session.overlay = Overlay::None;
                                let page = inbox.page_rows();
                                let message_before = inbox.session.message_cursor;
                                let thread_before = inbox.session.cursor_id.clone();
                                let folder_before = inbox.session.folder.clone();
                                let step = inbox.session.perform(action, page);
                                inbox.apply_step(
                                    step,
                                    message_before,
                                    thread_before,
                                    folder_before,
                                    cx,
                                );
                            }),
                        )
                        .child(div().flex_1().child(item.label.clone()))
                        .child(
                            div()
                                .font_family_opt(theme.mono_family.clone())
                                .text_size(px((theme.mono_px).max(12.0)))
                                .text_color(rgb(if index == selected {
                                    theme.on_selection
                                } else {
                                    theme.dimmed
                                }))
                                .child(item.binding.clone()),
                        )
                })),
        )
}

fn folder_picker(
    theme: &Theme,
    session: &Session,
    scroll: &ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let choices = session.folder_choices();
    let selected = session.folder_index;
    div()
        .w(px(PICKER_PX))
        .max_h(px(PICKER_MAX_PX))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            // A card fading out after it closed lets clicks through.
            cx.listener(|inbox, _, _, cx| {
                if inbox.session.overlay != Overlay::None {
                    cx.stop_propagation();
                }
            }),
        )
        .child(
            div()
                .h(px(44.))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .child(
                    div()
                        .text_color(rgb(
                            if session.folder_intent == crate::model::FolderIntent::Move {
                                theme.accent
                            } else {
                                theme.dimmed
                            },
                        ))
                        .child(session.folder_prompt()),
                )
                .child(div().child(session.folder_query.clone()))
                .child(div().w(px(2.)).h(px(16.)).bg(rgb(theme.accent))),
        )
        .child(
            div()
                .id("folder-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .py(px(4.))
                .children(choices.iter().enumerate().map(|(index, name)| {
                    let name = name.clone();
                    let label = session.roles.label(&name);
                    let detail = session.folder_detail(&name);
                    let detail_color = if index == selected {
                        theme.on_selection_dim
                    } else {
                        theme.dimmed
                    };
                    div()
                        .id(gpui::ElementId::named_usize("folder", index))
                        .h(px(PICKER_ROW_PX))
                        .mx(px(4.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .bg(rgb(if index == selected {
                            theme.selection
                        } else {
                            theme.surface
                        }))
                        .text_color(rgb(if index == selected {
                            theme.on_selection
                        } else {
                            theme.foreground
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |inbox, _, window, cx| {
                                // A picker fading out takes no clicks.
                                if inbox.session.overlay != Overlay::Folders {
                                    return;
                                }
                                cx.stop_propagation();
                                window.focus(&inbox.focus);
                                inbox.session.clear_toast();
                                inbox.session.folder_index = index;
                                let message_before = inbox.session.message_cursor;
                                let thread_before = inbox.session.cursor_id.clone();
                                let folder_before = inbox.session.folder.clone();
                                let step = inbox.session.confirm_folder();
                                inbox.apply_step(
                                    step,
                                    message_before,
                                    thread_before,
                                    folder_before,
                                    cx,
                                );
                            }),
                        )
                        .gap(px(8.))
                        .overflow_hidden()
                        .child(div().flex_none().child(label))
                        .children(detail.map(|name| {
                            div()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(rgb(detail_color))
                                .child(name)
                        }))
                })),
        )
}

fn help(theme: &Theme, session: &Session, cx: &mut Context<Inbox>) -> gpui::Div {
    let lines: Vec<_> = crate::keymap::Action::all()
        .iter()
        .copied()
        .map(|action| {
            (
                action.label().to_string(),
                crate::keymap::binding_text(&session.keymap, action),
            )
        })
        .collect();
    div()
        .w(px(HELP_PX))
        .max_h(px(460.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            // A card fading out after it closed lets clicks through.
            cx.listener(|inbox, _, _, cx| {
                if inbox.session.overlay != Overlay::None {
                    cx.stop_propagation();
                }
            }),
        )
        .child(
            div()
                .px(px(14.))
                .py(px(10.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Keys"),
        )
        .when_some(session.detail_text(), |card, error| {
            card.child(
                div()
                    .px(px(14.))
                    .py(px(8.))
                    .text_color(rgb(theme.error))
                    .child(error),
            )
        })
        .child(
            div()
                .id("help-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .px(px(14.))
                .py(px(8.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .children(lines.into_iter().map(|(label, binding)| {
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(12.))
                        .child(div().flex_1().child(label))
                        .child(
                            div()
                                .font_family_opt(theme.mono_family.clone())
                                .text_color(rgb(theme.dimmed))
                                .child(binding),
                        )
                })),
        )
}

fn archive_failure(err: &fell_core::Error) -> String {
    match err {
        fell_core::Error::Action(message) if message.contains("into Archive") => {
            "Already in Archive".into()
        }
        fell_core::Error::Action(message) if message.starts_with("no messages") => {
            "Nothing here to archive".into()
        }
        fell_core::Error::Action(message) if message.contains("no such thread") => {
            "No conversation".into()
        }
        other => other.to_string(),
    }
}

fn key_from(stroke: &Keystroke) -> Option<KeyId> {
    let bare = stroke.key.to_ascii_lowercase();
    if stroke.modifiers.control && stroke.modifiers.shift && bare == "p" {
        return Some(KeyId::CtrlShiftP);
    }
    if stroke.modifiers.control
        && !stroke.modifiers.alt
        && !stroke.modifiers.platform
        && let Some(ch) = ctrl_letter(&stroke.key)
    {
        return Some(KeyId::Ctrl(ch));
    }
    if stroke.modifiers.control || stroke.modifiers.alt || stroke.modifiers.platform {
        return None;
    }
    if let Some(ch) = physical_letter(&stroke.key) {
        return Some(KeyId::Char(if stroke.modifiers.shift {
            ch.to_ascii_uppercase()
        } else {
            ch.to_ascii_lowercase()
        }));
    }
    let typed = stroke
        .key_char
        .clone()
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| stroke.key.clone());
    match typed.as_str() {
        "escape" | "esc" => Some(KeyId::Escape),
        "backspace" => Some(KeyId::Backspace),
        "enter" | "return" => Some(KeyId::Enter),
        "space" | " " => Some(KeyId::Space),
        "up" => Some(KeyId::Up),
        "down" => Some(KeyId::Down),
        "left" | "right" | "tab" => None,
        other => {
            let mut chars = other.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return named_key(&stroke.key, stroke.modifiers.shift);
            }
            if stroke.modifiers.shift {
                Some(KeyId::Char(match ch {
                    '/' => '?',
                    ';' => ':',
                    other => other.to_ascii_uppercase(),
                }))
            } else {
                Some(KeyId::Char(ch))
            }
        }
    }
}

fn should_follow_visible(overlay: &Overlay, chord_empty: bool, pane: Pane, key: KeyId) -> bool {
    if *overlay != Overlay::None || !chord_empty {
        return false;
    }
    match key {
        KeyId::Char('j' | 'k' | 'q') => true,
        // Expand, reply, reply all, and forward act on the message on screen.
        KeyId::Enter | KeyId::Char('o' | 'r' | 'R' | 'f') => pane == Pane::Conversation,
        _ => false,
    }
}

fn ctrl_letter(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?.to_ascii_lowercase();
    if chars.next().is_some() || !ch.is_ascii_alphabetic() {
        return None;
    }
    Some(ch)
}

fn physical_letter(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || !ch.is_ascii_alphabetic() {
        return None;
    }
    Some(ch)
}

fn named_key(key: &str, shift: bool) -> Option<KeyId> {
    match key {
        "escape" => Some(KeyId::Escape),
        "backspace" => Some(KeyId::Backspace),
        "enter" | "return" => Some(KeyId::Enter),
        "space" => Some(KeyId::Space),
        "up" => Some(KeyId::Up),
        "down" => Some(KeyId::Down),
        "g" if shift => Some(KeyId::Char('G')),
        "v" if shift => Some(KeyId::Char('V')),
        "slash" if shift => Some(KeyId::Char('?')),
        "semicolon" if shift => Some(KeyId::Char(':')),
        other if other.chars().count() == 1 => {
            let ch = other.chars().next().unwrap();
            Some(KeyId::Char(if shift {
                ch.to_ascii_uppercase()
            } else {
                ch
            }))
        }
        _ => None,
    }
}

fn window_size() -> (f32, f32) {
    if let Ok(raw) = std::env::var("FELL_WINDOW")
        && let Some((width, height)) = raw.split_once('x')
        && let (Ok(width), Ok(height)) = (width.parse::<f32>(), height.parse::<f32>())
    {
        return (width, height);
    }
    (1180.0, 760.0)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

trait FontOpt: Styled {
    fn font_family_opt(self, family: Option<String>) -> Self;
}

impl<T: Styled> FontOpt for T {
    fn font_family_opt(self, family: Option<String>) -> Self {
        if let Some(family) = family {
            self.font_family(family)
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LIST_BORDER_PX, ctrl_letter, key_from, not_sent_notice, should_follow_visible, unread_bar_dx,
    };
    use crate::keymap::KeyId;
    use crate::model::Overlay;
    use crate::motion;
    use crate::present::Pane;
    use gpui::{Keystroke, Modifiers};

    fn stroke(key: &str, key_char: &str, shift: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                shift,
                ..Modifiers::default()
            },
            key: key.into(),
            key_char: Some(key_char.into()),
        }
    }

    #[test]
    fn unread_bars_stay_in_the_window_while_the_list_slides_in() {
        // The list 24 px left of its place: its bars would sit at -22 px.
        let dx = unread_bar_dx(-motion::PANE_SLIDE_PX);
        assert_eq!(
            -motion::PANE_SLIDE_PX + LIST_BORDER_PX + dx,
            0.0,
            "at the edge"
        );
        // Once the list brings them into the window they ride with it.
        assert_eq!(unread_bar_dx(-2.0), 0.0);
        assert_eq!(unread_bar_dx(-1.0), 0.0);
        assert_eq!(unread_bar_dx(0.0), 0.0);
        // A pane sliding in from the right never moves them.
        assert_eq!(unread_bar_dx(motion::PANE_SLIDE_PX), 0.0);
        // Half way, they are still held at the edge.
        assert_eq!(-10.0 + LIST_BORDER_PX + unread_bar_dx(-10.0), 0.0);
    }

    #[test]
    fn caps_lock_uses_the_physical_letter() {
        assert_eq!(key_from(&stroke("j", "J", false)), Some(KeyId::Char('j')));
        assert_eq!(key_from(&stroke("k", "K", false)), Some(KeyId::Char('k')));
        assert_eq!(key_from(&stroke("g", "G", false)), Some(KeyId::Char('g')));
        assert_eq!(key_from(&stroke("g", "G", true)), Some(KeyId::Char('G')));
        assert_eq!(
            key_from(&stroke("slash", "?", true)),
            Some(KeyId::Char('?'))
        );
    }

    fn controlled(key: &str) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
            key: key.into(),
            key_char: None,
        }
    }

    #[test]
    fn ctrl_letter_is_only_a_single_character_name() {
        assert_eq!(ctrl_letter("d"), Some('d'));
        assert_eq!(ctrl_letter("U"), Some('u'));
        assert_eq!(ctrl_letter("down"), None);
        assert_eq!(ctrl_letter("up"), None);
        assert_eq!(ctrl_letter("delete"), None);
        assert_eq!(key_from(&controlled("d")), Some(KeyId::Ctrl('d')));
        assert_eq!(key_from(&controlled("u")), Some(KeyId::Ctrl('u')));
        assert_eq!(key_from(&controlled("down")), None);
        assert_eq!(key_from(&controlled("up")), None);
        assert_eq!(key_from(&controlled("delete")), None);
    }

    #[test]
    fn visible_message_follow_skips_overlays() {
        assert!(should_follow_visible(
            &Overlay::None,
            true,
            Pane::List,
            KeyId::Char('j')
        ));
        assert!(should_follow_visible(
            &Overlay::None,
            true,
            Pane::Conversation,
            KeyId::Char('q')
        ));
        assert!(should_follow_visible(
            &Overlay::None,
            true,
            Pane::Conversation,
            KeyId::Enter
        ));
        assert!(should_follow_visible(
            &Overlay::None,
            true,
            Pane::Conversation,
            KeyId::Char('o')
        ));
        assert!(!should_follow_visible(
            &Overlay::None,
            true,
            Pane::List,
            KeyId::Enter
        ));
        assert!(!should_follow_visible(
            &Overlay::None,
            true,
            Pane::List,
            KeyId::Char('o')
        ));
        for overlay in [&Overlay::Palette, &Overlay::Folders, &Overlay::Help] {
            assert!(!should_follow_visible(
                overlay,
                true,
                Pane::Conversation,
                KeyId::Char('j')
            ));
            assert!(!should_follow_visible(
                overlay,
                true,
                Pane::Conversation,
                KeyId::Enter
            ));
        }
        assert!(!should_follow_visible(
            &Overlay::None,
            false,
            Pane::Conversation,
            KeyId::Char('k')
        ));
        // r, R, and f answer the message on screen in the conversation, and
        // the latest one from the list.
        for key in ['r', 'R', 'f'] {
            assert!(should_follow_visible(
                &Overlay::None,
                true,
                Pane::Conversation,
                KeyId::Char(key)
            ));
            assert!(!should_follow_visible(
                &Overlay::None,
                true,
                Pane::List,
                KeyId::Char(key)
            ));
        }
    }

    #[test]
    fn moves_the_server_will_not_get_say_so_in_one_line() {
        use crate::keymap::Keymap;
        use crate::model::{Session, SyncKind};
        use crate::sync_worker::NotSent;
        use fell_core::DropReason;

        let session = Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        let notice = |reason, count| {
            not_sent_notice(
                &session,
                &NotSent {
                    reason,
                    folder: "INBOX".into(),
                    count,
                },
            )
        };
        assert_eq!(
            notice(DropReason::FolderReset, 1),
            "Inbox was reset on the server; 1 message moved only in Fell"
        );
        assert_eq!(
            notice(DropReason::Refused, 3),
            "The server refused to move 3 messages"
        );
        assert_eq!(
            notice(DropReason::NotFound, 1),
            "1 message not found in Inbox on the server"
        );
        for reason in [
            DropReason::FolderReset,
            DropReason::Refused,
            DropReason::NotFound,
            DropReason::NoMove,
        ] {
            let text = notice(reason, 2);
            assert!(!text.contains('\n') && text.chars().count() <= 64, "{text}");
        }
    }
}
