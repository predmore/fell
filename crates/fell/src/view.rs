//! Two-pane inbox. The window reads the local store and never talks to IMAP.

mod attachment_actions;
mod attachment_chips;
pub(crate) mod disclosure;
mod help;
mod search;

use std::cell::OnceCell;
use std::collections::HashMap;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fell_core::{
    AccountConfig, AccountId, CidRepairBatch, CidRepairProgress, DropReason, DroppedWrite,
    FolderRole, FolderRoles, FolderSpan, ListKey, MessageId, OutboxKind, ThreadId,
};
use futures::StreamExt;
use gpui::{
    App, Application, Bounds, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, Keystroke, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Render,
    ScrollHandle, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled, TextStyle,
    TitlebarOptions, UniformListScrollHandle, Window, WindowBounds, WindowOptions, div, point,
    prelude::*, px, rgb, size, uniform_list,
};

use fell_core::compose::{DraftSummary, ReplyKind};

use crate::attachments::{self, Download, Fetcher, Request as AttachmentRequest};
use crate::draft_host::DraftHome;
use crate::frame_log::{FrameLog, SharedFrameLog};
use crate::inbox_motion::{Disclosures, InboxMotion, PaneEntry, StatusMotion};
use crate::keymap::{self, Action, KeyId};
use crate::mail_body::{
    self, BodyAsks, BodyCache, BodyJob, BodyLoad, BodyLoads, HtmlBody, PreparedBody,
};
use crate::mailbox;
use crate::model::{
    DRAFT_NOT_THREAD, Fetch, FlagEdit, Overlay, QueryFault, ReadDwell, Session, Step, SyncKind,
    Toast, row_key,
};
use crate::motion::{self, Glide, ListShift, Motion, RowMotion};
use crate::present::{self, ConvMessage, Pane, Rich, ThreadRow};
use crate::render::READING_MEASURE_PX;
use crate::reply_check::{ReplyCheck, ReplyChecks};
use crate::row_widths::{self, RowFrame, RowWidths};
use crate::scroll_anchor::{
    LandingHold, LayoutProbe, LayoutSlot, ReaderLanding, Reveal, RevealPhase, ScrollAnchor,
    clamped_offset,
};
use crate::store_actor::StoreClient;
use crate::sync_worker::{self, SyncEvent, SyncHandle};
use crate::text_cut;
use crate::text_render::BodyBlock;
use crate::theme::{self, Theme};
use crate::ui_trace::UiTrace;

use self::disclosure::disclosure;

pub fn run(trace_start: Option<Instant>) {
    eprintln!("{}: opening inbox", fell_version::VERSION);
    Application::new().run(move |cx: &mut App| {
        let trace = trace_start.and_then(|start| match UiTrace::open(start) {
            Ok(trace) => Some(trace),
            Err(error) => {
                eprintln!("fell: could not open UI trace: {error}");
                None
            }
        });
        if let Some(trace) = &trace {
            cx.set_global(trace.clone());
            let trace = trace.clone();
            cx.on_app_quit(move |_| {
                trace.finish();
                std::future::ready(())
            })
            .detach();
        }
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
                if let Some(trace) = &trace {
                    trace.window_created(window);
                }
                let view = cx.new(Inbox::new);
                let focus = view.read(cx).focus.clone();
                window.focus(&focus);
                view
            },
        );
        match view {
            Ok(view) => {
                if let Some(trace) = trace {
                    cx.on_window_closed(move |cx| {
                        if !cx.windows().contains(&view.into()) {
                            trace.finish();
                        }
                    })
                    .detach();
                }
            }
            Err(_) => eprintln!("fell: could not open a window"),
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
    /// Sync changed a body. Read it after the list resolves the thread key.
    refresh_open: bool,
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
        self.refresh_open |= later.refresh_open;
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

/// Use one notice so a flag failure and a move failure can both be seen.
fn write_back_notice(
    session: &Session,
    dropped: &[DroppedWrite],
    flag_recovery_delayed: bool,
) -> String {
    let Some(first) = dropped.first() else {
        return "Read/star sync delayed. Retrying".to_string();
    };
    let is_move = |write: &DroppedWrite| write.kind == OutboxKind::Move;
    let folder_matters = matches!(
        first.reason,
        DropReason::FolderReset | DropReason::NotFound | DropReason::Abandoned
    );
    if !flag_recovery_delayed
        && dropped.iter().all(|write| {
            is_move(write) == is_move(first)
                && write.reason == first.reason
                && (!folder_matters || write.source_folder == first.source_folder)
        })
    {
        return not_sent_detail(session, first, dropped.len());
    }

    let moves = dropped.iter().any(is_move);
    let flags = dropped.iter().any(|write| !is_move(write));
    let no_move = dropped
        .iter()
        .any(|write| write.reason == DropReason::NoMove);
    let summary = match (moves, flags, no_move) {
        (true, true, true) => "Server can't move mail. Read/star sync failed",
        (true, true, false) => "Moves and read/star changes failed to sync",
        (true, false, true) => "Server can't move mail",
        (true, false, false) => "Some moves failed to sync",
        (false, _, _) => "Some read/star changes failed to sync",
    };
    if flag_recovery_delayed {
        format!("{summary}. Read/star retry pending")
    } else {
        summary.to_string()
    }
}

fn not_sent_detail(session: &Session, write: &DroppedWrite, count: usize) -> String {
    let folder = session.folder_text(&write.source_folder);
    if write.kind != OutboxKind::Move {
        return match write.reason {
            DropReason::FolderReset => {
                format!("{folder} was reset on the server. Read or star changes stayed in Fell")
            }
            DropReason::NotFound => {
                format!("Mail with read or star changes was missing from {folder} on the server")
            }
            _ => "The server refused to update read or star flags".to_string(),
        };
    }
    let messages = if count == 1 {
        "1 message".to_string()
    } else {
        format!("{count} messages")
    };
    match write.reason {
        DropReason::FolderReset => {
            format!("{folder} was reset on the server; {messages} moved only in Fell")
        }
        DropReason::Refused => format!("The server refused to move {messages}"),
        DropReason::NotFound => format!("{messages} not found in {folder} on the server"),
        DropReason::NoMove => "This server can't move mail; archive stays in Fell".to_string(),
        DropReason::Abandoned => format!("{messages} left half moved in {folder} on the server"),
    }
}

/// What the inbox says while a reply or forward's window comes up:
/// "reply all · Jon".
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
    /// Cursor observed before the first inbox read, consumed after it paints.
    cid_repair: Option<futures::channel::oneshot::Receiver<fell_core::Result<CidRepairProgress>>>,
    account_id: AccountId,
    attachment_fetcher: Fetcher,
    attachment_busy: bool,
    attachment_retry: Option<AttachmentRequest>,
    attachment_download: Option<(String, String, Download)>,
    reveal_attachment: bool,
    forwarding_attachments: bool,
    session: Session,
    theme: Theme,
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    palette_scroll: ScrollHandle,
    folder_scroll: ScrollHandle,
    help_scroll: help::HelpScroll,
    search_view: search::SearchView,
    search_preview: gpui::Entity<search::CachedPreview>,
    search_rows: gpui::Entity<search::CachedRows>,
    conversation_revision: u64,
    split: f32,
    dragging: bool,
    open_thread: Option<String>,
    /// The open thread's subject when its messages arrived, for when its
    /// row has left the list (an archive) before the next thread's arrive.
    open_subject: String,
    /// A body fill in a short conversation can reveal the newest message
    /// once the new bodies are laid out.
    reveal: Reveal,
    /// Latest body-only read, applied together with its scroll correction.
    body_refresh: BodyRefresh,
    /// The scroll handle has bounds for this conversation's current messages.
    conv_layout_current: bool,
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
    /// Read and star shown on rows before their write lands, by thread id:
    /// `(write, unread, starred)`. A read that started before the write
    /// cannot paint the old state back. Only the latest write clears it.
    pending_flags: std::collections::HashMap<String, (u64, bool, bool)>,
    /// Bumped by every read or star write.
    flag_gen: u64,
    /// Names and subject widths of the rows on screen, kept so a row
    /// marked read or unread does not move.
    row_widths: RowWidths,
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
    /// Prepared HTML bodies by message id, so reopening prepares nothing.
    bodies: BodyCache,
    /// Body revisions to load for the current conversation and work in flight.
    body_loads: BodyLoads,
    /// Messages whose parts were asked of sync, and what came back.
    bodies_asked: BodyAsks,
    /// The reader's message to hold at the top of the conversation until
    /// they scroll it. See [`Inbox::hold_landing`].
    landing: ReaderLanding,
}

/// Room kept for a body that is still being prepared: a short paragraph, so
/// a reply does not leave a hole and a newsletter grows only downward.
const PENDING_BODY_PX: f32 = 66.0;

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
        let motion = Motion::from_env();
        let mut session = Session::new(name, email, initial, startup.keymap);
        session.loading = true;
        session.read_dwell_ms = startup.read_dwell_ms;
        if let Some(error) = startup.error {
            session.record_startup_error(error);
        }
        let reader = cx.entity().downgrade();
        let inbox = Self {
            focus: cx.focus_handle(),
            account_id: config
                .as_ref()
                .map(|config| config.account().id)
                .unwrap_or_else(|| AccountId(String::new())),
            attachment_fetcher: Fetcher::new(config.clone(), startup.path.clone().ok()),
            attachment_busy: false,
            attachment_retry: None,
            attachment_download: None,
            reveal_attachment: false,
            forwarding_attachments: false,
            session,
            store,
            cid_repair: None,
            theme,
            list_scroll: UniformListScrollHandle::new(),
            conv_scroll: ScrollHandle::new(),
            palette_scroll: ScrollHandle::new(),
            folder_scroll: ScrollHandle::new(),
            help_scroll: help::HelpScroll::default(),
            search_view: search::SearchView::default(),
            search_preview: cx.new(|_| search::CachedPreview::new(reader.clone())),
            search_rows: cx.new(|_| search::CachedRows(reader)),
            conversation_revision: 0,
            split: startup.split,
            dragging: false,
            open_thread: None,
            open_subject: String::new(),
            reveal: Reveal::Waiting,
            body_refresh: BodyRefresh::default(),
            conv_layout_current: false,
            moved_thread: None,
            sync: None,
            list_folder: FolderRole::Inbox.default_name().into(),
            read_in_flight: false,
            refresh_in_flight: false,
            read_gen: 0,
            read_wanted: None,
            pending_removals: std::collections::HashSet::new(),
            pending_flags: std::collections::HashMap::new(),
            flag_gen: 0,
            row_widths: RowWidths::new(motion),
            conv_gen: Arc::new(AtomicU64::new(0)),
            draft_home,
            drafts: Vec::new(),
            motion: InboxMotion::new(motion),
            frame_now: Instant::now(),
            frames: FrameLog::from_env(),
            warm: None,
            warmed_folder: None,
            bodies: BodyCache::default(),
            body_loads: BodyLoads::default(),
            bodies_asked: BodyAsks::default(),
            landing: ReaderLanding::default(),
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

    fn refresh_reply_checks(&self, ids: Vec<fell_core::MessageId>, cx: &mut Context<Self>) {
        if ids.is_empty() {
            return;
        }
        let account = self.account_id.clone();
        let read = self.store.call(move |store| {
            let mut verified = Vec::new();
            for id in ids {
                if store
                    .message_summary(&account, &id)?
                    .is_some_and(|parent| parent.reply_to.is_some())
                {
                    verified.push(id);
                }
            }
            Ok::<_, fell_core::Error>(verified)
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(verified)) = read.await
                && !verified.is_empty()
            {
                let _ = this.update(cx, |_, cx| {
                    cx.global_mut::<ReplyChecks>()
                        .apply(fell_core::ReplyToProgress::Verified(verified));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn on_store_ready(
        &mut self,
        opened: mailbox::Opened,
        sync_with: Option<(AccountConfig, std::path::PathBuf)>,
        cx: &mut Context<Self>,
    ) {
        if let Some(trace) = cx.try_global::<UiTrace>() {
            trace.store_opened(opened.error.as_deref());
        }
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
        cx.default_global::<ReplyChecks>()
            .start(self.account_id.clone(), sync_with.is_none() || store_failed);
        if !store_failed {
            self.cid_repair = Some(self.store.call(fell_core::Store::cid_repair_progress));
        }
        // Sync opens its own connection only after this one finished opening
        // and migrating the file, so the two never race for the first lock.
        if !store_failed && let Some((config, path)) = sync_with {
            let (handle, events) = sync_worker::start(config, path);
            self.sync = Some(handle);
            self.watch_sync(events, cx);
        }
        self.request_threads(ReadIntent::default(), cx);
        self.search_store_ready(cx);
        cx.notify();
    }

    fn repair_cached_cids(
        &self,
        start: futures::channel::oneshot::Receiver<fell_core::Result<CidRepairProgress>>,
        cx: &mut Context<Self>,
    ) {
        use std::time::Duration;

        cx.spawn(async move |this, cx| {
            let mut progress = match start.await {
                Ok(Ok(progress)) => progress,
                Ok(Err(err)) => {
                    eprintln!("fell: could not check cached CID roles: {err}");
                    return;
                }
                Err(_) => return,
            };
            let mut retry = false;
            while !progress.is_done() {
                // Give folder reads and waiting writers a turn between slices.
                let pause = if retry {
                    Duration::from_secs(1)
                } else {
                    Duration::from_millis(16)
                };
                cx.background_executor().timer(pause).await;
                let Ok(reply) = this.update(cx, |inbox, _| {
                    inbox
                        .store
                        .call_maintenance(move |store| store.repair_cid_batch(progress))
                }) else {
                    return;
                };
                let batch = match reply.await {
                    Ok(Ok(batch)) => batch,
                    Ok(Err(err)) => {
                        if !retry {
                            eprintln!("fell: could not repair cached CID roles: {err}");
                        }
                        retry = true;
                        continue;
                    }
                    Err(_) => return,
                };
                progress = batch.progress;
                retry = false;
                if (batch.external_progress || !batch.changed_threads.is_empty())
                    && this
                        .update(cx, |inbox, cx| inbox.cid_roles_changed(&batch, cx))
                        .is_err()
                {
                    return;
                }
            }
        })
        .detach();
    }

    fn cid_roles_changed(&mut self, batch: &CidRepairBatch, cx: &mut Context<Self>) {
        let any_changed = |id: &str| {
            batch
                .changed_threads
                .iter()
                .any(|(account, thread)| account == &self.account_id && thread.as_str() == id)
        };
        let refresh_open =
            batch.external_progress || self.session.cursor_id.as_deref().is_some_and(any_changed);
        if refresh_open
            || self.session.threads.iter().any(|row| any_changed(&row.id))
            || (self.read_in_flight
                && batch
                    .changed_threads
                    .iter()
                    .any(|(account, _)| account == &self.account_id))
        {
            self.request_threads(
                ReadIntent {
                    refresh_open,
                    ..ReadIntent::default()
                },
                cx,
            );
        }
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
                            SyncEvent::Status(kind) => kind,
                            SyncEvent::ReplyTo(progress) => {
                                let finished =
                                    matches!(progress, fell_core::ReplyToProgress::Finished);
                                cx.global_mut::<ReplyChecks>().apply(progress);
                                if finished {
                                    // Also resolve watched drafts whose parent has since become an alias.
                                    inbox.refresh_reply_checks(
                                        cx.global::<ReplyChecks>().pending(),
                                        cx,
                                    );
                                }
                                cx.notify();
                                return;
                            }
                            SyncEvent::SendsConfirmed(ids) => {
                                if let Some(drafts) =
                                    cx.try_global::<crate::draft_host::AgentDrafts>()
                                {
                                    drafts.0.confirm_synced_sends(&inbox.account_id, &ids);
                                }
                                return;
                            }
                            SyncEvent::WriteBackNotice {
                                dropped,
                                flag_recovery_delayed,
                            } => {
                                let text = write_back_notice(
                                    &inbox.session,
                                    &dropped,
                                    flag_recovery_delayed,
                                );
                                inbox.session.show_toast(text, true);
                                cx.notify();
                                return;
                            }
                            // Refresh the open thread when either its parts
                            // or its read and star state may have changed.
                            SyncEvent::BodiesStored | SyncEvent::FlagsChanged => {
                                inbox.request_threads(
                                    ReadIntent {
                                        refresh_open: true,
                                        ..ReadIntent::default()
                                    },
                                    cx,
                                );
                                return;
                            }
                        };
                        let wrote = matches!(kind, SyncKind::Syncing { .. })
                            || matches!(kind, SyncKind::Synced { changed: true, .. });
                        if matches!(kind, SyncKind::Failed(_)) {
                            cx.global_mut::<ReplyChecks>().set_offline(true);
                        }
                        if matches!(kind, SyncKind::Synced { .. }) {
                            cx.global_mut::<ReplyChecks>().set_offline(false);
                            if wrote {
                                inbox
                                    .refresh_reply_checks(cx.global::<ReplyChecks>().pending(), cx);
                            }
                            // Sync is answering again: asks that failed
                            // while it was not can go now.
                            inbox.bodies_asked.retry_failed();
                        }
                        inbox.session.apply_sync_status(kind);
                        if wrote {
                            inbox.request_threads(
                                ReadIntent {
                                    refresh_open: true,
                                    ..ReadIntent::default()
                                },
                                cx,
                            );
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
    fn request_threads(&mut self, mut intent: ReadIntent, cx: &mut Context<Self>) {
        if self.session.search.is_some() {
            self.search_view.defer_folder_read(intent);
            return;
        }
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
        // An interactive read that replaces a refresh picks up its body reread.
        intent.refresh_open |= supersede;
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
            let mut rows = present::thread_rows(&page.threads, &me);
            let unread = page.unread;
            let drafts = if roles.is(FolderRole::Drafts, &reading) {
                // Synced drafts name who they are to, and none is unread.
                let ids: Vec<ThreadId> = page
                    .threads
                    .iter()
                    .map(|thread| thread.thread_id.clone())
                    .collect();
                let recipients = store
                    .folder_recipients(&account, &reading, &ids)
                    .unwrap_or_default();
                // The header shows no count in Drafts; the folder's own
                // count stays, so leaving Drafts never reads "0 unread".
                present::address_draft_rows(&mut rows, &recipients, &me);
                store
                    .unsent_compose_drafts(&account)
                    .map_err(|err| err.to_string())?
            } else {
                Vec::new()
            };
            Ok(ListRead {
                rows,
                more_before: page.more_before,
                more_after: page.more_after,
                unread,
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
        if self.session.search.is_some() {
            let intent = match wanted {
                Some(next) => intent.merge(next),
                None => intent,
            };
            self.search_view.defer_folder_read(intent);
            return;
        }
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
        let follow_up_pending = wanted.is_some();
        let wanted = wanted.map(|mut next| {
            next.refresh_open |= intent.refresh_open;
            next
        });
        self.show_threads(folder, result, intent, follow_up_pending, cx);
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
        follow_up_pending: bool,
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
        if let Some(trace) = cx.try_global::<UiTrace>() {
            trace.folder_ready();
        }
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
            for row in &mut rows {
                let Some(&(_, now_unread, starred)) = self.pending_flags.get(&row.id) else {
                    continue;
                };
                if row.unread != now_unread {
                    unread = if now_unread {
                        unread + 1
                    } else {
                        unread.saturating_sub(1)
                    };
                }
                row.unread = now_unread;
                row.starred = starred;
            }
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
            self.row_widths.clear();
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
        // A first sync stores rows from their headers, then their bodies.
        // While the open thread has a message whose body has not come,
        // each write is read again for it, whichever message it fills.
        let waiting =
            self.open_thread.is_some() && present::awaiting_bodies(&self.session.messages);
        for row in &rows {
            self.row_widths.observe_labels(&row.id, &row.labels, now);
        }
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
        // A new selection and draft edits load immediately. Give a queued
        // folder refresh the opportunity to resolve the open thread's key
        // before starting another body-only read.
        let new_selection = open_changed && self.session.cursor_id.is_some();
        let refresh_open = open_changed || grew || waiting || intent.refresh_open;
        if new_selection
            || self.session.cursor_draft().is_some()
            || (refresh_open && !follow_up_pending)
        {
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
            let messages = read_conversation(
                store,
                &account,
                &last.thread_id,
                (&me, &roles, &names, &reading),
            )
            .ok()?;
            Some((present::thread_rows(&page.threads, &me), messages))
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
        if self.session.search.is_some() {
            return None;
        }
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
                                    let look = RowLook {
                                        selected: false,
                                        fading: None,
                                        ghost: true,
                                        bar_dx: 0.0,
                                        frame: still_frame(&chunk[index], window),
                                        previous_labels: None,
                                    };
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
                    width - list_width(width, self.split) - SPLITTER_PX
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
                        ReaderColumns::split(conv_width),
                        Live(false),
                        &ConvFrame::default(),
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

    /// Rows the list viewport shows whole: a row cut off at an edge is not
    /// one the reader can be expected to see change.
    fn whole_rows_in_view(&self) -> Option<std::ops::Range<usize>> {
        let row = self.list_row_height()?;
        let state = self.list_scroll.0.borrow();
        let size = state.last_item_size?;
        let top = (-f32::from(state.base_handle.offset().y)).max(0.0);
        let bottom = top + f32::from(size.item.height);
        // A hair of slack, so float error never drops a row that fits.
        let start = ((top - 0.5) / row).ceil().max(0.0) as usize;
        let end = (((bottom + 0.5) / row).floor() as usize).min(self.session.threads.len());
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
        self.reveal.cancel_pending();
        let now = self.motion.clock.now();
        let settled = self.motion.settle(now);
        self.row_widths.settle();
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
        if !self.session.search_results_current() {
            return;
        }
        if let Some(draft) = self.session.cursor_draft() {
            // A draft is shown from the drafts list, not read as a thread.
            self.conv_gen.fetch_add(1, Ordering::SeqCst);
            self.body_loads.clear();
            let shown = self
                .drafts
                .iter()
                .find(|item| item.id == draft)
                .map(|item| {
                    present::draft_conversation(item, &self.session.account_email, now_unix())
                })
                .unwrap_or_default();
            let same = self.open_thread.as_deref() == Some(id.as_str());
            self.invalidate_conversation_layout();
            if same {
                self.session.refresh_messages(shown, true);
            } else {
                // Fades in as a synced draft's messages do, so `j` between
                // the two kinds never pops.
                self.reveal = Reveal::Waiting;
                self.motion.show_thread(&mut self.session, shown);
                self.scroll_conversation();
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
        let viewing = self
            .session
            .search_source(&id)
            .unwrap_or(&self.session.folder)
            .to_owned();
        let thread = ThreadId(id.clone());
        let query = self.session.search.as_ref().and_then(|search| {
            let generation = search.accepted_generation()?;
            Some((generation, search.effective_query().to_owned()))
        });
        let cached = self.bodies.stamps();
        let reply = self.store.call(move |store| {
            if current.load(Ordering::SeqCst) != generation {
                return None;
            }
            Some(
                read_conversation(store, &account, &thread, (&me, &roles, &names, &viewing))
                    .map(|mut view| {
                        let reads = mail_body::read_bodies(store, &account, &mut view, &cached);
                        let matches = query.as_ref().map(|(accepted, query)| {
                            (
                                *accepted,
                                store
                                    .search_thread_matches(&account, query, &thread)
                                    .unwrap_or_default(),
                            )
                        });
                        (view, reads, matches)
                    })
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
                    Ok((view, _, _)) if view.is_empty() && inbox.session.search.is_some() => {
                        // A hit may disappear between the two reads. Search
                        // has its own list, so a folder read cannot replace it.
                        inbox.clear_open_message();
                        inbox.session.cancel_read_delay();
                        inbox.session.clear_query_fault(QueryFault::Message);
                        if inbox.session.search_results_current() {
                            inbox.refresh_search(cx);
                        }
                    }
                    Ok((view, _, _)) if view.is_empty() && !inbox.session.messages.is_empty() => {
                        // The open thread key can disappear between the folder
                        // and message reads. Keep the conversation until a
                        // folder read confirms where its messages belong.
                        if !inbox.read_in_flight {
                            inbox.request_threads(ReadIntent::default(), cx);
                        }
                    }
                    Ok((mut view, reads, matches)) => {
                        inbox.body_loads.accept(reads, &mut inbox.bodies, &mut view);
                        inbox.session.clear_query_fault(QueryFault::Message);
                        let same = inbox.open_thread.as_deref() == Some(id.as_str());
                        // Sync may rekey the thread as its bodies arrive.
                        // A read of the same messages still belongs to the reader.
                        let in_place = (same || !view.is_empty())
                            && present::same_messages(&inbox.session.messages, &view);
                        let follow = if in_place {
                            inbox.keep_shown_text(&mut view);
                            inbox.queue_conversation_refresh(view);
                            false
                        } else if same {
                            inbox.keep_shown_text(&mut view);
                            let bottom = inbox.conversation_at_bottom();
                            inbox.invalidate_conversation_layout();
                            inbox.session.refresh_messages(view, bottom)
                        } else {
                            inbox.invalidate_conversation_layout();
                            inbox.reveal = Reveal::Waiting;
                            inbox.motion.show_thread(&mut inbox.session, view);
                            // A new thread starts at the top of the reader's
                            // message. Scrolling only far enough to show its
                            // end would cut the row above it, or its header
                            // when it is tall. A thread that fits stays put.
                            inbox.land_on_reader();
                            false
                        };
                        if let Some((accepted, matches)) = matches
                            && inbox
                                .session
                                .set_search_matches(accepted, &id, matches, !same)
                        {
                            inbox.land_on_reader();
                        }
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

    /// Keep an existing body visible while its stored HTML is prepared.
    fn keep_shown_text(&self, view: &mut [ConvMessage]) {
        for message in view.iter_mut().filter(|m| m.rich == Rich::Pending) {
            if let Some(old) = self
                .session
                .messages
                .iter()
                .find(|old| old.id == message.id)
                && old.body_state == fell_core::BodyState::Stored
                && message.body_state == fell_core::BodyState::Stored
                && old.rich != Rich::Pending
            {
                message.rich = old.rich.clone();
            }
        }
    }

    fn queue_conversation_refresh(&mut self, messages: Vec<ConvMessage>) {
        // Newly downloaded bodies can push the latest message below a short
        // pane. An already scrollable pane keeps the reader where they are.
        let downloaded = self
            .session
            .messages
            .iter()
            .zip(&messages)
            .any(|(old, new)| {
                old.body_state == fell_core::BodyState::Pending && old.body_state != new.body_state
            });
        if downloaded {
            self.reveal.body_changed(
                self.conv_layout_current
                    && present::conversation_fits(f32::from(self.conv_scroll.max_offset().height)),
            );
        }
        self.body_refresh.messages = Some(messages);
        // The new store read already consulted the cache at its accepted revision.
        self.body_refresh.rich.clear();
    }

    /// Prepare one body on the background executor. It lands in the cache
    /// and, if its thread is still open, fades in under its header.
    fn prepare_body(&mut self, job: BodyJob, load: BodyLoad, cx: &mut Context<Self>) {
        let task = cx.background_executor().spawn(async move { job.prepare() });
        cx.spawn(async move |this, cx| {
            let prepared = task.await;
            let _ = this.update(cx, |inbox, cx| {
                if inbox.body_loads.finish(&prepared.id, load) {
                    inbox.body_ready(prepared);
                }
                // A stale completion has freed the slot for a newer revision.
                cx.notify();
            });
        })
        .detach();
    }

    fn body_ready(&mut self, prepared: PreparedBody) {
        let rich = prepared.rich();
        self.bodies.insert(&prepared);
        self.body_refresh.rich.insert(prepared.id, rich);
    }

    /// Show the stored text for `id` from now on if it has no prepared body.
    fn body_is_plain(&mut self, id: &str) {
        let messages = self
            .body_refresh
            .messages
            .as_deref()
            .unwrap_or(&self.session.messages);
        if messages.iter().any(|message| {
            message.id == id && matches!(message.rich, Rich::Pending | Rich::Missing)
        }) {
            self.body_refresh.rich.insert(id.to_owned(), Rich::Plain);
        }
    }

    /// Read and prepare the body of every open message that still needs it,
    /// and ask sync for parts that were never stored. Runs before each frame
    /// is built, so any way of opening a message (a key, a click, a reread
    /// that keeps it open) starts its body.
    fn want_bodies(&mut self, cx: &mut Context<Self>) {
        let now = Instant::now();
        let mut read = Vec::new();
        let mut fetch = Vec::new();
        for message in self.session.messages.iter().filter(|m| m.expanded) {
            if let Some(load) = self.body_loads.begin(&message.id) {
                read.push((message.id.clone(), load, message.mail_headers.clone()));
            } else if message.rich == Rich::Missing
                && message.body_state == fell_core::BodyState::Stored
                && self.sync.is_some()
                && self.bodies_asked.should_ask(&message.id, now)
            {
                fetch.push(message.id.clone());
            }
        }
        for id in fetch {
            self.fetch_body(id, cx);
        }
        if read.is_empty() {
            return;
        }
        let pending: Vec<_> = read
            .iter()
            .map(|(id, load, _)| (id.clone(), *load))
            .collect();
        let account = self.account_id.clone();
        let reply = self.store.call(move |store| {
            read.into_iter()
                .map(|(id, load, headers)| {
                    mail_body::read_job_at_stamp(store, &account, &id, load.stamp, &headers)
                })
                .collect::<Vec<_>>()
        });
        cx.spawn(async move |this, cx| {
            let mut jobs = reply.await.unwrap_or_default().into_iter();
            let _ = this.update(cx, |inbox, cx| {
                let mut reread = false;
                for (id, load) in pending {
                    match jobs.next() {
                        Some(Ok(Some(job))) if inbox.body_loads.is_wanted(&id, load) => {
                            inbox.prepare_body(job, load, cx);
                        }
                        result if inbox.body_loads.finish(&id, load) => {
                            inbox.body_is_plain(&id);
                            reread |= matches!(result, Some(Ok(None)));
                        }
                        _ => {}
                    }
                }
                if reread {
                    inbox.reread_open_thread(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Ask the sync thread for one message's parts. Until they come the
    /// stored text shows. A real answer is kept for the session; an error
    /// (sync not started, no connection) is asked again after a while.
    /// Parts that come are stored, so the open thread is read again and
    /// their HTML fades in over the text.
    fn fetch_body(&mut self, id: String, cx: &mut Context<Self>) {
        let Some(sync) = &self.sync else {
            return;
        };
        self.bodies_asked.asking(&id);
        let answer = sync.fetch_body(MessageId(id.clone()));
        cx.spawn(async move |this, cx| {
            let reply = answer.await;
            let _ = this.update(cx, |inbox, cx| {
                match reply {
                    Ok(Ok(has_html)) => {
                        inbox.bodies_asked.answered(&id);
                        if has_html {
                            inbox.reread_open_thread(cx);
                        } else {
                            for message in &mut inbox.session.messages {
                                if message.id == id && message.rich == Rich::Missing {
                                    message.rich = Rich::Plain;
                                }
                            }
                        }
                    }
                    _ => {
                        inbox.bodies_asked.failed(&id, Instant::now());
                        // Ask again once the wait is over, even if nothing
                        // else redraws the window by then.
                        cx.spawn(async move |this, cx| {
                            cx.background_executor()
                                .timer(mail_body::ASK_AGAIN_AFTER)
                                .await;
                            let _ = this.update(cx, |_, cx| cx.notify());
                        })
                        .detach();
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Read the open thread again, for parts that were written since.
    fn reread_open_thread(&mut self, cx: &mut Context<Self>) {
        if self.open_thread.is_some() && self.open_thread == self.session.cursor_id {
            self.request_messages(true, cx);
        }
    }

    /// Run a mailbox change on the store thread, then reread the folder.
    fn change_mailbox<T: Send + 'static>(
        &mut self,
        change: impl FnOnce(&fell_core::Store) -> Result<T, fell_core::Error> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<T, fell_core::Error>) -> Option<ReadIntent> + 'static,
        cx: &mut Context<Self>,
    ) {
        let searching = self.session.search.is_some();
        if searching && self.session.refresh_search() {
            // Retain the current view while the write runs. It will be
            // reread once the store can see the change.
            self.cancel_search_work();
        }
        let reply = self.store.call(change);
        cx.spawn(async move |this, cx| {
            let Ok(result) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                let after = done(inbox, result);
                if inbox.session.search.is_some() {
                    let select = after
                        .as_ref()
                        .and_then(|intent| intent.select.as_ref())
                        .map(|key| key.thread_id.0.clone());
                    inbox.refresh_saved_search_folder(select, cx);
                    inbox.refresh_search(cx);
                }
                if let Some(after) = after {
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
                            inbox.row_widths.clear();
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
        let Some(folder) = self.action_folder(&id) else {
            return;
        };
        let account = self.account_id.clone();
        let thread = ThreadId(id.clone());
        let key = self.selected_key();
        if self.session.search.is_none() && !self.session.roles.is_view(&folder) {
            self.take_out_of_list(std::slice::from_ref(&id), cx);
        }
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
        let Some(from) = self.action_folder(&id) else {
            return;
        };
        let to = destination.clone();
        let key = self.selected_key();
        if self.session.search.is_none() && !self.session.roles.is_view(&from) {
            self.take_out_of_list(std::slice::from_ref(&id), cx);
        }
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

    /// Local actions may have queued a server write: send it now
    /// rather than at the next pass.
    fn send_to_server(&self) {
        if let Some(sync) = &self.sync {
            sync.write_back_now();
        }
    }

    /// `#`, `!`, or a confirmed permanent delete on `ids` (the selection,
    /// or the selected row). They leave the list in this frame, as an
    /// archived row does, and the row after them is selected.
    fn remove_threads(&mut self, ids: Vec<String>, removal: Removal, cx: &mut Context<Self>) {
        if self.session.search.is_some() {
            self.remove_search_threads(ids, removal, cx);
            return;
        }
        if self.session.folder != self.list_folder {
            self.session
                .show_toast("The folder is still loading", false);
            return;
        }
        let ids: Vec<String> = ids
            .into_iter()
            .filter(|id| self.session.threads.iter().any(|row| &row.id == id))
            .collect();
        let Some(key) = ids.first().and_then(|first| {
            self.session
                .threads
                .iter()
                .find(|row| &row.id == first)
                .map(row_key)
        }) else {
            self.session.show_toast("No conversation", true);
            return;
        };
        let roles = &self.session.roles;
        let folder = self.list_folder.clone();
        let count = ids.len();
        let done_text = match removal {
            Removal::Delete => moved_notice(
                count,
                &self.session.folder_text(roles.name(FolderRole::Trash)),
            ),
            Removal::Spam if roles.is(FolderRole::Junk, &folder) => moved_notice(
                count,
                &self.session.folder_text(roles.name(FolderRole::Inbox)),
            ),
            Removal::Spam => moved_notice(
                count,
                &self.session.folder_text(roles.name(FolderRole::Junk)),
            ),
            Removal::Purge => match count {
                1 => "Removed from Fell".to_string(),
                n => format!("Removed {n} from Fell"),
            },
        };
        let account = self.account_id.clone();
        let threads: Vec<ThreadId> = ids.iter().cloned().map(ThreadId).collect();
        self.session.picked.clear();
        self.session.visual = false;
        if !roles.is_view(&folder) {
            self.take_out_of_list(&ids, cx);
        }
        self.change_mailbox(
            move |store| {
                match removal {
                    Removal::Delete => store.delete_threads(&account, &threads, &folder),
                    Removal::Spam => store.spam_threads(&account, &threads, &folder),
                    Removal::Purge => store.purge_threads(&account, &threads, &folder),
                }
                .map(|_| ())
            },
            move |inbox, result| {
                for id in &ids {
                    inbox.pending_removals.remove(id);
                }
                match result {
                    Ok(()) => {
                        // Undo finds the first of them again; nothing
                        // comes back after a permanent delete.
                        inbox.moved_thread = (removal != Removal::Purge).then_some(key);
                        inbox.session.show_toast(done_text, false);
                        inbox.send_to_server();
                        Some(ReadIntent {
                            changed: true,
                            ..ReadIntent::default()
                        })
                    }
                    Err(err) => {
                        inbox.session.show_toast(err.to_string(), true);
                        Some(ReadIntent {
                            changed: true,
                            select: Some(key),
                            ..ReadIntent::default()
                        })
                    }
                }
            },
            cx,
        );
    }

    /// Remove `ids` from the rows on screen before their write lands and
    /// open the row that took their place. Rows below slide up together.
    fn take_out_of_list(&mut self, ids: &[String], cx: &mut Context<Self>) {
        self.pending_removals.extend(ids.iter().cloned());
        let now = self.motion.clock.now();
        let drawn = self.drawn_rows(now);
        for id in ids {
            self.session.remove_thread(id);
        }
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
            move |store| store.undo(&account),
            |inbox, result| match result {
                Ok(action) => {
                    if action.kind == fell_core::ActionKind::DiscardDrafts {
                        inbox.session.show_toast(action.summary, false);
                        return Some(ReadIntent {
                            changed: true,
                            ..ReadIntent::default()
                        });
                    }
                    inbox.session.show_toast("Undone", false);
                    inbox.send_to_server();
                    // Read and star leave the thread where it is; the thread
                    // `m` or `e` took away waits for its own undo.
                    let select = if action.kind.is_flag() {
                        None
                    } else {
                        inbox.moved_thread.take()
                    };
                    if inbox.session.search.is_some() {
                        inbox.search_view.prefer =
                            select.as_ref().map(|key| key.thread_id.0.clone());
                    }
                    Some(ReadIntent {
                        changed: true,
                        select,
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

    fn discard_drafts(&mut self, ids: Vec<String>, cx: &mut Context<Self>) {
        let Some(hub) = cx
            .try_global::<crate::draft_host::AgentDrafts>()
            .map(|drafts| drafts.0.clone())
        else {
            return;
        };
        let account = self.account_id.clone();
        self.change_mailbox(
            move |store| hub.discard(store, &account, &ids),
            |inbox, result| {
                match result {
                    Ok(()) => {
                        inbox.session.picked.clear();
                        inbox.session.visual = false;
                    }
                    Err(err) => inbox.session.show_toast(err.to_string(), true),
                }
                Some(ReadIntent {
                    changed: true,
                    ..ReadIntent::default()
                })
            },
            cx,
        );
    }

    /// Write a read, unread, or star the rows already show. A failure says
    /// so and rereads the folder, so the rows go back to what is stored.
    fn apply_flags(&mut self, edit: FlagEdit, cx: &mut Context<Self>) {
        if self.session.search.is_some() {
            let Some(scopes) = self.session.search_scopes(&edit.ids) else {
                return;
            };
            let account = self.account_id.clone();
            self.change_mailbox(
                move |store| store.flag_scoped_threads(&account, &scopes, edit.change),
                |inbox, result| {
                    match result {
                        Ok(Some(_)) => inbox.send_to_server(),
                        Ok(None) => {}
                        Err(err) => inbox.session.show_toast(err.to_string(), true),
                    }
                    Some(ReadIntent {
                        changed: true,
                        ..ReadIntent::default()
                    })
                },
                cx,
            );
            return;
        }
        self.flag_gen += 1;
        let generation = self.flag_gen;
        for row in self
            .session
            .threads
            .iter()
            .filter(|row| edit.ids.contains(&row.id))
        {
            self.pending_flags
                .insert(row.id.clone(), (generation, row.unread, row.starred));
        }
        let account = self.account_id.clone();
        let folder = self.list_folder.clone();
        let threads: Vec<ThreadId> = edit.ids.iter().cloned().map(ThreadId).collect();
        let change = edit.change;
        let ids = edit.ids;
        self.change_mailbox(
            move |store| store.flag_threads(&account, &threads, &folder, change),
            move |inbox, result| {
                inbox
                    .pending_flags
                    .retain(|id, (written, _, _)| !(ids.contains(id) && *written == generation));
                match result {
                    Ok(Some(_)) => inbox.send_to_server(),
                    Ok(None) => {}
                    Err(err) => inbox.session.show_toast(err.to_string(), true),
                }
                Some(ReadIntent {
                    changed: true,
                    ..ReadIntent::default()
                })
            },
            cx,
        );
    }

    /// Opened with a read delay: mark the thread read if it is still open
    /// when the delay runs out.
    fn read_after(&mut self, read: ReadDwell, cx: &mut Context<Self>) {
        let delay = self.session.read_dwell_ms;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay))
                .await;
            let _ = this.update(cx, |inbox, cx| {
                if let Some(edit) = inbox.session.read_after_dwell(read) {
                    inbox.apply_flags(edit, cx);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn watch_reply_check(&self, id: fell_core::MessageId, cx: &mut Context<Self>) {
        if cx.global_mut::<ReplyChecks>().watch(id.clone()) {
            self.refresh_reply_checks(vec![id], cx);
            cx.notify();
        }
    }

    fn watch_held_draft(
        &self,
        hub: &fell_core::compose::DraftHub,
        id: &str,
        cx: &mut Context<Self>,
    ) {
        let parent = hub.with_session(id, |session| {
            if session.account_id() == &self.account_id && session.reply_parts().reply_to_unverified
            {
                session
                    .fields()
                    .in_reply_to
                    .as_ref()
                    .map(fell_core::MessageId::new)
            } else {
                None
            }
        });
        if let Some(Some(parent)) = parent {
            self.watch_reply_check(parent, cx);
        }
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
            self.session.show_toast("New draft", false);
        } else {
            let subject = self
                .session
                .cursor_thread()
                .map(|row| row.subject.clone())
                .unwrap_or_default();
            self.session
                .show_toast(format!("Opening draft \u{201c}{subject}\u{201d}"), false);
        }
        let notice = self.session.toast_generation();
        if let Some(id) = &id
            && crate::draft_host::show_draft(&hub, id)
        {
            self.watch_held_draft(&hub, id, cx);
            self.settle_opening_notice(notice, cx);
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
                        inbox.watch_held_draft(&hub, &id, cx);
                        if crate::draft_host::show_draft(&hub, &id) {
                            inbox.settle_opening_notice(notice, cx);
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
        let forwarding = kind == ReplyKind::Forward;
        if forwarding && (self.forwarding_attachments || self.attachment_busy) {
            self.session
                .show_toast("An attachment action is already in progress", false);
            return;
        }
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
        self.forwarding_attachments |= forwarding;
        let notice = self.session.toast_generation();
        let account = self.account_id.clone();
        let email = self.session.account_email.clone();
        let roles = self.session.roles.clone();
        let source = self.attachment_source();
        let read = self.store.call({
            let account = account.clone();
            move |store| {
                let parent = store.message(&account, &parent_id)?;
                let parts =
                    if forwarding && parent.as_ref().is_some_and(|parent| parent.has_attachment) {
                        store.message_attachments(&account, &parent_id)?
                    } else {
                        None
                    };
                Ok::<_, fell_core::Error>((parent, parts))
            }
        });
        cx.spawn(async move |this, cx| {
            let result = match read.await {
                Ok(Ok((Some(mut parent), parts))) => {
                    let mut files = Vec::new();
                    if let Some(parts) = parts {
                        let parts = attachments::visible_parts(parts);
                        let omitted;
                        (files, omitted) = source
                            .forward_files(&this, cx, parent.headers.message_id.as_str(), parts)
                            .await;
                        // The existing quote and compose warning name just
                        // the files that could not be carried.
                        parent.has_attachment = !omitted.is_empty();
                        parent.attachment_names = omitted;
                    }
                    let own = fell_core::compose::own_addresses(&parent, &email, &roles);
                    let offset = crate::present::local_offset(parent.headers.date_unix);
                    let start = fell_core::compose::start_reply_in_zone(
                        kind, &parent, &email, &own, offset, &roles,
                    );
                    if start.parts.reply_to_unverified {
                        let id = parent.headers.message_id;
                        let _ = this.update(cx, |inbox, cx| inbox.watch_reply_check(id, cx));
                    }
                    let hub = hub.clone();
                    cx.background_executor()
                        .spawn(
                            async move { home.new_reply_with_files(&hub, &account, start, files) },
                        )
                        .await
                }
                Ok(Ok((None, _))) => Err("the message is no longer in the mailbox".to_string()),
                Ok(Err(err)) => Err(err.to_string()),
                Err(_) => Err("the mailbox reader stopped".into()),
            };
            let _ = this.update(cx, |inbox, cx| {
                if forwarding {
                    inbox.forwarding_attachments = false;
                }
                match result {
                    Ok(id) => {
                        if crate::draft_host::show_draft(&hub, &id) {
                            inbox.settle_opening_notice(notice, cx);
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
    /// "opening" notice `notice` has done its job and goes, unless something
    /// newer replaced it, such as the notice for a second draft opened since.
    fn settle_opening_notice(&mut self, notice: u64, cx: &mut Context<Self>) {
        self.dismiss_notice_after(notice, std::time::Duration::from_millis(700), cx);
    }

    fn dismiss_notice_after(
        &mut self,
        notice: u64,
        delay: std::time::Duration,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let _ = this.update(cx, |inbox, cx| {
                if inbox.session.clear_toast_if(notice) {
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
                draft.reply_context = snap.reply_context;
                draft.forwarded = snap.forwarded;
                draft.in_reply_to = snap.in_reply_to.filter(|id| !id.trim().is_empty());
                Some(draft)
            })
            .collect()
    }

    fn clear_open_message(&mut self) {
        self.conversation_revision = self.conversation_revision.wrapping_add(1);
        self.body_loads.clear();
        self.invalidate_conversation_layout();
        self.session.messages.clear();
        self.open_thread = None;
        self.open_subject.clear();
    }

    /// The conversation now shows thread `id`'s messages.
    fn mark_open(&mut self, id: String) {
        self.conversation_revision = self.conversation_revision.wrapping_add(1);
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
        let started = Instant::now();
        if self.session.search_editing() && self.search_edit_key(event, started, cx) {
            cx.stop_propagation();
            // Prepare the edited field as soon as input dispatch has
            // released the inbox entity. GPUI presents the prepared scene
            // on its normal frame callback.
            window.defer(cx, |window, cx| window.draw(cx).clear());
            return;
        }
        let Some(key) = key_from(&event.keystroke) else {
            return;
        };
        // A held key that started before a prompt appeared cannot confirm it.
        if key == KeyId::Char('y') && event.is_held {
            return;
        }
        let trace = cx
            .try_global::<UiTrace>()
            .map(|trace| (trace.clone(), Instant::now()));
        if let Some(log) = &self.frames {
            log.borrow_mut().key(started, event.keystroke.unparse());
        }
        // Input never waits on an animation.
        self.settle_motion();
        window.focus(&self.focus);
        if self.session.overlay == Overlay::Help {
            let now = self.motion.clock.now();
            self.help_scroll.prepare(now, || {
                help::Layout::for_window(&self.theme, &self.session, window)
            });
            if self.help_scroll.on_key(key, &self.motion.motion, now) {
                self.motion.end_key();
                cx.notify();
                cx.stop_propagation();
                return;
            }
        }
        // The wheel moves the viewport without moving message_cursor. j, k,
        // and q aim at the message now on screen in either pane. Enter and o
        // do that only while the conversation is focused, so they expand the
        // message on screen. An open palette, folder filter, or help card
        // types those keys and must leave the cursor alone.
        if self.search_conversation_live()
            && self.session.focused_attachment().is_none()
            && should_follow_visible(
                &self.session.overlay,
                self.session.chord.is_empty(),
                self.session.pane,
                key,
            )
        {
            self.follow_visible_message();
        }
        // A command can re-target the message on screen before it runs.
        let before = self.before_step();
        let page = self.page_rows();
        // `U` and `s` say what they did only when the row is not on screen.
        let width = f32::from(window.viewport_size().width);
        let (show_list, _) =
            present::visible_panes(width, self.session.pane, self.session.full_width);
        self.session.rows_in_view = if show_list {
            self.whole_rows_in_view()
        } else {
            None
        };
        self.session.clear_toast();
        let step = self.session.on_key(key, page);
        if let Some((trace, started)) = trace {
            trace.key(started, step.action.map_or("Inbox input", Action::label));
        }
        self.apply_step(step, before, cx);
        cx.stop_propagation();
    }

    /// Where the reader was before a key or action: [`Inbox::apply_step`]
    /// compares it with where they are after.
    fn before_step(&self) -> StepBefore {
        StepBefore {
            message: self.session.message_cursor,
            thread: self.session.cursor_id.clone(),
            folder: self.session.folder.clone(),
            pane: self.session.pane,
            searching: self.session.search.is_some(),
            search_generation: self.session.search_generation,
            help_open: self.session.overlay == Overlay::Help,
        }
    }

    fn apply_step(&mut self, step: Step, before: StepBefore, cx: &mut Context<Self>) {
        let StepBefore {
            message: message_before,
            thread: thread_before,
            folder: folder_before,
            pane: pane_before,
            searching,
            search_generation,
            help_open,
        } = before;
        if !help_open && self.session.overlay == Overlay::Help {
            self.help_scroll = help::HelpScroll::default();
        }
        let search_changed = self.after_search_step(&step, searching, search_generation, cx);
        if self.open_thread.is_some() && self.open_thread == self.session.cursor_id {
            if step.reveal_attachment {
                self.reveal_selected_attachment();
            }
            if let Some(request) = step.attachment {
                self.start_attachment_action(request, false, cx);
            }
            if let Some(target) = step.confirmed_attachment {
                self.start_attachment_action(AttachmentRequest::Open(target), true, cx);
            }
        } else if step.reveal_attachment || step.attachment.is_some() {
            self.session.leave_attachments();
            self.session
                .show_toast("The conversation is still loading", false);
        }
        if step.retry_attachment {
            if let Some(request) = self.attachment_retry.clone() {
                self.start_attachment_action(request, false, cx);
            } else {
                self.session
                    .show_toast("No attachment action to retry", false);
            }
        }
        if step.apply_move
            && let Some(destination) = self.session.pending_move.take()
        {
            self.apply_move(destination, cx);
        }
        if step.undo {
            self.apply_undo(cx);
        }
        if let Some(edit) = step.flags.clone() {
            self.apply_flags(edit, cx);
        }
        if let Some(read) = step.read_after {
            self.read_after(read, cx);
        }
        if step.archive {
            self.archive_current(cx);
        }
        if step.delete {
            let ids = self.session.action_targets();
            self.remove_threads(ids, Removal::Delete, cx);
        }
        if !step.discard.is_empty() {
            self.discard_drafts(step.discard, cx);
        }
        if step.spam {
            let ids = self.session.action_targets();
            self.remove_threads(ids, Removal::Spam, cx);
        }
        if step.purge
            && let Some(ids) = self.session.take_purge()
        {
            self.remove_threads(ids, Removal::Purge, cx);
        }
        if step.reopen_review {
            let reopened = cx
                .try_global::<crate::draft_host::AgentDrafts>()
                .is_some_and(|drafts| drafts.0.reopen_last());
            if !reopened {
                self.session.show_toast("No agent review to reopen", false);
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
                    self.session.show_sync_requested();
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
        } else if searching && self.session.search.is_none() {
            self.request_messages(false, cx);
        }
        if self.session.search.is_none()
            && let Some(intent) = self.search_view.take_folder_read()
            && self.session.folder == folder_before
        {
            self.request_threads(intent, cx);
        }
        // A move past the loaded rows, or near their end, asks for more.
        self.issue_fetch(cx);
        if step.arm_which {
            self.arm_which(cx);
        }
        let thread_moved = self.session.cursor_id != thread_before;
        // The reader moved in the list, or came back to it: a box left wide
        // when a row turned read eases back now, with this key's own glide
        // or slide, never on a timer while they read.
        if moved_in_list(
            (thread_before.as_deref(), pane_before),
            (self.session.cursor_id.as_deref(), self.session.pane),
        ) {
            self.row_widths.relax();
        }
        if thread_moved && !search_changed {
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
        if step.message_disclosure_changed {
            self.reader_took_over(true);
        }
        if step.scroll_pages != 0 || step.scroll_lines != 0 {
            self.reader_took_over(false);
            self.nudge_conversation(step.scroll_pages, step.scroll_lines);
        } else if step.reveal_search_hit {
            self.scroll_conversation();
        } else if !step.reveal_attachment
            && (step.jump_to_message || self.session.message_cursor != message_before)
        {
            if self.session.cursor_id == thread_before {
                self.reader_took_over(true);
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
        if self.session.messages.is_empty() {
            return;
        }
        let scrolled = self.reveal == (Reveal::Reading { selected: false });
        if !scrolled
            && ScrollAnchor::capture_message(&self.conv_scroll, self.session.message_cursor)
                .is_some()
        {
            return;
        }
        if let Some(anchor) = ScrollAnchor::capture(&self.conv_scroll)
            && anchor.index < self.session.messages.len()
        {
            self.session.message_cursor = anchor.index;
        }
    }

    /// The conversation viewport is scrolled to its end (or does not scroll).
    fn conversation_at_bottom(&self) -> bool {
        let max = f32::from(self.conv_scroll.max_offset().height);
        let offset = f32::from(self.conv_scroll.offset().y);
        present::conversation_fits(max) || offset <= -max + 24.0
    }

    fn focus_message(&mut self, index: usize) {
        self.reader_took_over(true);
        self.session.leave_attachments();
        self.session.pane = Pane::Conversation;
        self.session.message_cursor = index;
    }

    fn search_conversation_live(&self) -> bool {
        !self.session.search_editing() && self.search_preview_current()
    }

    fn search_preview_current(&self) -> bool {
        self.session.search.as_ref().is_none_or(|search| {
            self.open_thread == self.session.cursor_id
                && search.matches_thread(self.session.cursor_id.as_deref())
        })
    }

    fn reader_took_over(&mut self, selected: bool) {
        self.reveal = Reveal::Reading { selected };
        self.release_landing();
    }

    fn invalidate_conversation_layout(&mut self) {
        self.body_refresh = BodyRefresh::default();
        self.release_landing();
        self.conv_layout_current = false;
        self.reveal.cancel_pending();
    }

    fn apply_body_refresh(&mut self, now: Instant) -> Option<ScrollAnchor> {
        // Let a keyboard glide finish in the geometry it was calculated from.
        if self.motion.conv_glide.is_some() || self.body_refresh.is_empty() {
            return None;
        }
        let refresh = std::mem::take(&mut self.body_refresh);
        self.conversation_revision = self.conversation_revision.wrapping_add(1);
        if self.reveal.pending() || !refresh.rich.is_empty() {
            self.reveal.body_changed(false);
        }
        if self.reveal.pending() {
            self.release_landing();
        }
        let anchor = if self.conv_layout_current {
            self.reveal
                .body_anchor(&self.conv_scroll, self.session.message_cursor)
        } else {
            None
        };
        let selected = self.session.message_cursor;
        let rich_arrived = self
            .session
            .messages
            .get(selected)
            .is_some_and(|message| refresh.rich.contains_key(&message.id));
        refresh.apply(&mut self.session, &mut self.motion, now);
        if rich_arrived && self.session.reveal_matching_quote(selected) {
            self.land_on_reader();
        }
        anchor
    }

    /// The frame after bodies were filled in laid them out: glide to the
    /// newest message's start if it is out of view. Nothing moves when it
    /// shows already.
    fn reveal_newest(&mut self, window: &mut Window) {
        match self.reveal {
            Reveal::Waiting | Reveal::Following(RevealPhase::Idle) | Reveal::Reading { .. } => {}
            Reveal::Following(RevealPhase::Laying) => {
                self.reveal = Reveal::Following(RevealPhase::Ready);
                window.request_animation_frame();
            }
            Reveal::Following(RevealPhase::Ready) => {
                self.reveal = Reveal::Following(RevealPhase::Idle);
                let last = self.session.messages.len().saturating_sub(1);
                let Some(item) = self.conv_scroll.bounds_for_item(last) else {
                    return;
                };
                let view = self.conv_scroll.bounds();
                let offset = f32::from(self.conv_scroll.offset().y);
                let max = f32::from(self.conv_scroll.max_offset().height);
                if let Some(to) = present::offset_to_show_start(
                    f32::from(item.top()),
                    f32::from(view.top()),
                    f32::from(view.bottom()),
                    offset,
                    max,
                ) {
                    self.glide_conversation(offset, to);
                    window.request_animation_frame();
                }
            }
        }
    }

    /// Bring the reader's message to the top of the viewport. Scrolling
    /// only far enough to show it would pin a tall message by its end and
    /// hide its header.
    fn scroll_conversation(&mut self) {
        self.conv_layout_current = false;
        if !self.session.messages.is_empty() {
            self.motion.conv_glide = None;
            self.land_on_reader();
        }
    }

    /// Put the reader's message at the top of the viewport and keep it
    /// there while the conversation lays out: [`LandingHold`] sets the
    /// offset from each frame's own layout, before it paints.
    fn land_on_reader(&mut self) {
        self.landing.hold(
            self.session
                .messages
                .get(self.session.message_cursor)
                .map(|message| message.id.as_str()),
        );
    }

    /// Hold this message until the reader scrolls or moves to another one.
    fn hold_landing(&mut self) -> Option<ScrollAnchor> {
        let index = self.session.message_cursor;
        let current = self
            .session
            .messages
            .get(index)
            .map(|message| message.id.as_str());
        self.landing
            .anchor(current, index, self.motion.conv_glide.is_some())
    }

    /// The reader took over: stop holding their message in place.
    fn release_landing(&mut self) {
        self.landing.hold(None);
    }

    /// Message navigation aligns the selected header using laid-out bounds.
    fn glide_conversation_to_cursor(&mut self) {
        let index = self.session.message_cursor;
        self.land_on_reader();
        if self.motion.reading_sliding() {
            return;
        }
        let Some(item) = self
            .conv_layout_current
            .then(|| self.conv_scroll.bounds_for_item(index))
            .flatten()
        else {
            self.scroll_conversation();
            return;
        };
        let view = self.conv_scroll.bounds();
        let offset = f32::from(self.conv_scroll.offset().y);
        let max = f32::from(self.conv_scroll.max_offset().height);
        let target =
            present::offset_to_message_top(f32::from(item.top()), f32::from(view.top()), max);
        self.glide_conversation(offset, target);
    }

    /// Set the conversation's offset to `to`, gliding there from where it
    /// is drawn. `offset` is the offset the key saw.
    fn glide_conversation(&mut self, offset: f32, to: f32) {
        let from = self.motion.glide_from_conversation(offset);
        let x = self.conv_scroll.offset().x;
        self.conv_scroll.set_offset(point(x, px(to)));
        let now = self.motion.clock.now();
        let reach = f32::from(self.conv_scroll.bounds().size.height) * motion::GLIDE_MAX_VIEWPORTS;
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
        if self.session.focused_attachment().is_none() {
            self.session.leave_attachments();
        }
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
        if self
            .session
            .search
            .as_ref()
            .is_some_and(|search| !search.results_current() || !search.hits.contains_key(&id))
        {
            return;
        }
        self.cancel_pending_keys(cx);
        self.session.leave_attachments();
        let changed = self.session.cursor_id.as_deref() != Some(id.as_str());
        if changed || self.session.pane != Pane::List {
            self.session.cancel_read_delay();
            // A click in the list is a move in it, as `j` is.
            self.row_widths.relax();
        }
        self.session.cursor_id = Some(id);
        self.session.pane = Pane::List;
        if let Some(search) = self.session.search.as_mut() {
            search.editing = false;
            self.session.remember_search();
        }
        if changed {
            self.request_messages(true, cx);
        }
        self.scroll_list_to_cursor();
    }
}

impl Render for Inbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.session.loading
            && let Some(start) = self.cid_repair.take()
        {
            let this = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = this.update(cx, |inbox, cx| inbox.repair_cached_cids(start, cx));
            });
        }
        self.bodies
            .release_unused_images(|image| cx.drop_image(image, Some(window)));
        let real = Instant::now();
        // Motion time: a frame the compositor held back moves what is
        // moving one step on, not to its end.
        let now = self.motion.clock.frame(real);
        let previous_reading = self.motion.reading_target();
        self.frame_now = now;
        let width = f32::from(window.viewport_size().width);
        let panes = present::visible_panes(width, self.session.pane, self.session.full_width);
        self.advance_glides(now, window.scale_factor());
        let refreshing_body = !self.body_refresh.is_empty();
        let anchor = self.apply_body_refresh(now);
        let reveal_attachment = std::mem::take(&mut self.reveal_attachment);
        let spin_attachment = self.motion.motion.enabled()
            && panes.1
            && self.attachment_download.as_ref().is_some_and(|(id, _, _)| {
                self.session
                    .messages
                    .iter()
                    .any(|message| message.expanded && &message.id == id)
            });
        self.want_bodies(cx);
        self.motion.observe(&self.session, panes, now);
        let reflow = self.motion.reading_reflow(now);
        let reveal_attachment = if reflow.is_some() && reveal_attachment {
            self.reveal_attachment = true;
            false
        } else {
            reveal_attachment
        };
        self.motion
            .observe_header(&self.list_folder, self.session.unread_threads(), now);
        self.motion
            .fit_card(card_fit(&self.session, &self.theme, window), now);
        if self.body_refresh.is_empty() {
            self.reveal_newest(window);
        }
        let layout_anchor = || {
            if (previous_reading != self.motion.reading_target() || self.motion.reading_sliding())
                && self.conv_layout_current
                && self.motion.conv_glide.is_none()
            {
                ScrollAnchor::capture(&self.conv_scroll)
            } else {
                None
            }
        };
        let anchor = anchor.or_else(layout_anchor);
        let conv = ConvFrame {
            alphas: self.motion.body_alphas(now),
            disclosures: self.motion.disclosures(&self.session, now),
            anchor: self.hold_landing().or(anchor),
            frames: self.frames.clone(),
            attachment_download: self.attachment_download.clone(),
            reveal_attachment,
            spin_attachment,
            preview_current: self.search_preview_current(),
        };
        self.conv_layout_current = panes.1;
        let animating = self.motion.tick(now)
            | self.row_widths.running(now)
            | spin_attachment
            | self.search_view.animating(now)
            | (self.session.overlay == Overlay::Help && self.help_scroll.running(now));
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
        let mut overlay_card = |session: &Session, motion, live| {
            let help = self
                .help_scroll
                .card(&theme, session, window, now, live, cx);
            overlay(
                &theme,
                session,
                motion,
                card,
                live,
                &self.palette_scroll,
                &self.folder_scroll,
                help,
                cx,
            )
        };
        if let Some((layer, opacity)) = self.motion.closing_overlay(now) {
            layer.swap(&mut self.session);
            leaving.push(overlay_card(&self.session, (opacity, 0.0), false).into_any_element());
            layer.swap(&mut self.session);
        }
        let active_overlay = (self.session.overlay != Overlay::None)
            .then(|| overlay_card(&self.session, self.motion.overlay(now), true));
        let warm = self.warm_layer(width, panes, animating, window, cx);
        let subject = self.shown_subject();
        let cached_preview = self.search_preview_for(panes, animating || refreshing_body, &subject);
        let cached_search_rows = self.search_rows_for(animating);
        let reply_check = cx.try_global::<ReplyChecks>().and_then(|checks| {
            let selected = self.session.reply_target().filter(|_| {
                self.open_thread.is_some() && self.open_thread == self.session.cursor_id
            });
            selected.and_then(|target| checks.for_message(&target.id))
        });
        shell(
            theme,
            &self.session,
            &mut self.motion,
            now,
            Shown {
                subject,
                folder: self.list_folder.clone(),
                leaving,
                warm,
                active_overlay,
                conv,
                cached_preview,
                cached_search_rows,
                reply_check,
            },
            width,
            split,
            list_scroll,
            conv_scroll,
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
        .when_some(cx.try_global::<UiTrace>().cloned(), |root, trace| {
            root.child(
                gpui::canvas(|_, _, _| (), move |_, _, window, _| trace.painted(window))
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
    active_overlay: Option<gpui::Div>,
    /// The folder the rows on screen come from. The header names it, so
    /// after `gs` its name and count change in the frame the rows do.
    folder: String,
    /// What the conversation's bodies animate this frame.
    conv: ConvFrame,
    cached_preview: Option<(u64, gpui::AnyView)>,
    cached_search_rows: Option<(u64, gpui::AnyView)>,
    reply_check: Option<ReplyCheck>,
}

/// What the conversation's bodies show this frame beyond the session.
#[derive(Default)]
struct ConvFrame {
    /// Bodies fading in, by message id: their opacity.
    alphas: HashMap<String, f32>,
    disclosures: Disclosures,
    /// The message held at its previous viewport position, or at the top on open.
    anchor: Option<ScrollAnchor>,
    frames: Option<SharedFrameLog>,
    attachment_download: Option<(String, String, Download)>,
    reveal_attachment: bool,
    spin_attachment: bool,
    preview_current: bool,
}

/// Observe GPUI's scroll offset before and after it handles a wheel event.
/// The observer is painted before the scroll pane, so it runs after that
/// pane in the bubble phase. Clamping here uses its current layout.
fn observe_wheel(scroll: ScrollHandle, showing: bool, cx: &mut Context<Inbox>) -> impl IntoElement {
    use gpui::{DispatchPhase, ScrollWheelEvent};

    let inbox = cx.entity().downgrade();
    gpui::canvas(
        |_, _, _| (),
        move |_, (), window, _| {
            let mut before = px(0.);
            window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| match phase {
                DispatchPhase::Capture => before = clamped_offset(&scroll),
                DispatchPhase::Bubble => {
                    let over_reader = showing && scroll.bounds().contains(&event.position);
                    let moved = clamped_offset(&scroll) != before;
                    let _ = inbox.update(cx, |inbox, cx| {
                        if over_reader && inbox.session.overlay == Overlay::None {
                            if !moved {
                                return;
                            }
                            inbox.reader_took_over(false);
                        } else {
                            inbox.reveal.cancel_pending();
                        }
                        inbox.motion.cancel_glides();
                        cx.notify();
                    });
                }
            });
        },
    )
    .absolute()
    .size_0()
}

#[allow(clippy::too_many_arguments)]
fn shell(
    theme: Theme,
    session: &Session,
    motion: &mut InboxMotion,
    now: Instant,
    shown: Shown,
    width: f32,
    split: f32,
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    focus: FocusHandle,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    let (show_list, show_conv) = present::visible_panes(width, session.pane, session.full_width);
    let list_width = list_width(width, split);
    let beside = show_conv && width >= present::NARROW_WIDTH;
    let split_width = if beside {
        list_width + SPLITTER_PX
    } else {
        0.0
    };
    let reading = motion.reading_progress(now);
    let scale = window.scale_factor();
    let split_space = if beside {
        ReaderColumns::pane_left(split_width, reading, scale)
    } else {
        0.0
    };
    let list_entry = motion.pane_entry(Pane::List, now);
    let conv_entry = motion.pane_entry(Pane::Conversation, now);
    let status = motion.status(session, now);
    let Shown {
        subject,
        folder,
        leaving,
        warm,
        active_overlay,
        conv,
        cached_preview,
        cached_search_rows,
        reply_check,
    } = shown;
    let frames = conv.frames.clone();
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
            Pane::List => thread_list(&theme, session, false, width, list_scroll.clone(), None, cx),
            Pane::Conversation => conversation(
                &theme,
                session,
                subject.take().unwrap_or_default(),
                motion,
                now,
                Some(conv_scroll.clone()),
                ReaderColumns::split(width),
                // Paint only: the shield over it takes the pointer too.
                Live(false),
                &conv,
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
        .child(observe_wheel(conv_scroll.clone(), show_conv, cx))
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
        .when_some(frames.clone(), |root, frames| {
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
        .child(match &session.search {
            Some(search) => search::header(&theme, search, width, window, cx),
            None => header(&theme, session, &folder, motion, now, width, window),
        })
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
                .when(show_list || split_space > 0.0, |row| {
                    let list = thread_list(
                        &theme,
                        session,
                        beside,
                        if beside { list_width } else { width },
                        list_scroll,
                        cached_search_rows,
                        cx,
                    );
                    if beside {
                        row.child(
                            div()
                                .relative()
                                .flex_none()
                                .w(px(split_space))
                                .overflow_hidden()
                                .child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .bottom_0()
                                        .right_0()
                                        .w(px(split_width))
                                        .flex()
                                        .flex_row()
                                        .opacity(motion.reading_list_opacity(now))
                                        .child(list)
                                        .child(splitter(&theme, cx)),
                                )
                                .when(!show_list, |clip| {
                                    clip.child(
                                        div()
                                            .id("closing-thread-list")
                                            .absolute()
                                            .top_0()
                                            .left_0()
                                            .size_full()
                                            .occlude(),
                                    )
                                }),
                        )
                    } else {
                        row.child(entering(list, list_entry))
                    }
                })
                .when(show_conv, |row| {
                    if let Some((revision, preview)) = cached_preview {
                        let pane = div()
                            .id(("cached-search-conversation", revision))
                            .relative()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .flex()
                            .flex_col()
                            .child(preview)
                            .when(
                                !conv.preview_current
                                    && !subject.as_ref().is_none_or(String::is_empty),
                                |pane| {
                                    pane.child(preview_shield(
                                        &theme,
                                        session.search_results_current(),
                                    ))
                                },
                            );
                        return row.child(pane);
                    }
                    row.child(entering(
                        conversation(
                            &theme,
                            session,
                            subject.take().unwrap_or_default(),
                            motion,
                            now,
                            Some(conv_scroll),
                            ReaderColumns::new(
                                width,
                                split_width,
                                motion.reading_target(),
                                reading,
                                scale,
                            )
                            .with_reflow(motion.reading_reflow(now)),
                            Live(conv.preview_current && !session.search_editing()),
                            &conv,
                            window,
                            cx,
                        ),
                        conv_entry,
                    ))
                }),
        )
        .child(status_bar(
            &theme,
            session,
            status,
            reply_check,
            width,
            window,
        ))
        .children(leaving)
        .when(
            session.which_visible && session.overlay == Overlay::None && !session.chord.is_empty(),
            |root| root.child(which_key_popup(&theme, session, motion.which(now))),
        )
        .children(active_overlay)
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

fn folder_path_ends(path: &str) -> Option<(&str, &str)> {
    let (first, rest) = path.split_once('/')?;
    Some((first, rest.rsplit('/').next().unwrap_or(rest)))
}

fn fit_folder_path(
    path: &str,
    fit: impl Fn(&str) -> Option<gpui::ShapedLine>,
) -> Option<gpui::ShapedLine> {
    let full = fit(path);
    if full.as_ref().is_some_and(|line| line.text == path) {
        return full;
    }
    let Some((first, leaf)) = folder_path_ends(path) else {
        return full;
    };
    let prefix = format!("{first}/…/");
    fit(&format!("{prefix}{leaf}"))
        .filter(|line| {
            line.text
                .strip_prefix(&prefix)
                .is_some_and(|leaf| !leaf.is_empty() && leaf != "…")
        })
        .or_else(|| fit(&format!("…/{leaf}")))
}

fn account_header<'a>(name: &'a str, email: &'a str) -> (&'a str, Option<&'a str>) {
    let name = name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case(email) {
        (email, None)
    } else {
        (name, (!email.is_empty()).then_some(email))
    }
}

fn useful_header_email(email: &str, fitted: &str) -> bool {
    if fitted == email {
        return true;
    }
    let kept = fitted.strip_suffix('…').unwrap_or(fitted);
    kept.chars().take(12).count() == 12
        || email.rsplit_once('@').is_some_and(|(local, _)| {
            !local.is_empty() && kept.starts_with(local) && kept.len() > local.len()
        })
}

struct HeaderAccount {
    name: Option<SharedString>,
    email: Option<SharedString>,
    quiet: bool,
}

impl HeaderAccount {
    fn fit(
        session: &Session,
        room: f32,
        gap: f32,
        show_email: bool,
        fit: &impl Fn(&str, f32, FontWeight) -> Option<gpui::ShapedLine>,
    ) -> Self {
        let (name, email) = account_header(&session.account_name, &session.account_email);
        let full = fit(name, room, FontWeight::SEMIBOLD).filter(|line| line.text == name);
        let quiet = full.is_none();
        let name = full.or_else(|| fit(name, room, FontWeight::NORMAL));
        let email = name
            .as_ref()
            .filter(|_| show_email && !quiet)
            .and_then(|line| {
                let email = email?;
                fit(
                    email,
                    room - f32::from(line.width).ceil() - gap,
                    FontWeight::NORMAL,
                )
                .filter(|line| useful_header_email(email, &line.text))
            });
        Self {
            name: name.map(|line| line.text),
            email: email.map(|line| line.text),
            quiet,
        }
    }

    fn render(self, theme: &Theme, gap: f32, [name_opacity, email_opacity]: [f32; 2]) -> gpui::Div {
        div()
            .flex_none()
            .flex()
            .items_baseline()
            .gap(px(gap))
            .children(self.name.map(|name| {
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .when(self.quiet, |label| label.text_color(rgb(theme.dimmed)))
                    .when(!self.quiet, |label| label.font_weight(FontWeight::SEMIBOLD))
                    .opacity(name_opacity)
                    .child(name)
            }))
            .children(self.email.map(|email| {
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_color(rgb(theme.dimmed))
                    .opacity(email_opacity)
                    .child(email)
            }))
    }
}

fn header(
    theme: &Theme,
    session: &Session,
    folder: &str,
    motion: &mut InboxMotion,
    now: Instant,
    width: f32,
    window: &Window,
) -> gpui::Div {
    const PAD: f32 = 16.;
    const FOLDER_GAP: f32 = 24.;
    const COUNT_GAP: f32 = 14.;
    const ACCOUNT_MIN: f32 = 88.;
    const DETAIL_MIN_WIDTH: f32 = 420.;

    let style = conversation_line_style(window, theme, px(theme.ui_px));
    let room = (width - 2. * PAD - FOLDER_GAP).max(0.);
    let fit = |text: &str, room: f32, weight| {
        clip_shaped(window, &style, text, room.floor(), weight, None)
            .filter(|line| f32::from(line.width).ceil() <= room)
    };
    let count_width = |unread| {
        line_px_with_style(
            window,
            &style,
            &format!("{unread} unread"),
            FontWeight::NORMAL,
        )
        .ceil()
    };
    let count = |unread: usize| {
        div()
            .flex_none()
            .whitespace_nowrap()
            .text_color(rgb(if unread > 0 {
                theme.accent
            } else {
                theme.dimmed
            }))
            .child(format!("{unread} unread"))
    };
    let folder_group = |folder: &str, unread, count_fade: Option<(usize, f32)>| {
        let drafts = session.roles.is(FolderRole::Drafts, folder);
        let count_room = count_fade.map_or_else(
            || count_width(unread),
            |(old, shown)| motion::lerp(count_width(old), count_width(unread), shown),
        );
        let count_space = if drafts { 0. } else { COUNT_GAP + count_room };
        let name_room = (room - ACCOUNT_MIN - count_space).max(0.);
        let label = session.roles.label(folder);
        let name = fit_folder_path(&label, |path| fit(path, name_room, FontWeight::SEMIBOLD));
        let name_width = name
            .as_ref()
            .map_or(0., |line| f32::from(line.width).ceil());
        let detail = session
            .folder_detail(folder)
            .and_then(|text| fit(&text, name_room - name_width - 6., FontWeight::NORMAL));
        let detail_space = detail
            .as_ref()
            .map_or(0., |line| 6. + f32::from(line.width).ceil());
        let group = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(COUNT_GAP))
            .child(
                div()
                    .flex_none()
                    .flex()
                    .items_baseline()
                    .gap(px(6.))
                    .children(name.map(|line| {
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(line.text)
                    }))
                    .children(detail.map(|line| {
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .text_color(rgb(theme.dimmed))
                            .child(line.text)
                    })),
            )
            .when(!drafts, |row| {
                row.child(
                    div()
                        .flex_none()
                        .relative()
                        .flex()
                        .justify_end()
                        .w(px(count_room))
                        .child(count(unread).opacity(count_fade.map_or(1., |(_, shown)| shown)))
                        .children(count_fade.map(|(old, shown)| {
                            count(old).absolute().right_0().top_0().opacity(1. - shown)
                        })),
                )
            });
        (group, name_width + detail_space + count_space)
    };

    let (current, current_width) =
        folder_group(folder, session.unread_threads(), motion.unread_count(now));
    let fade = motion.folder_header();
    let leaving = fade
        .as_ref()
        .map(|fade| folder_group(&fade.folder, fade.unread, None));
    let old_width = leaving.as_ref().map_or(current_width, |(_, width)| *width);
    let (outgoing, incoming) = fade
        .as_ref()
        .map_or((0., 1.), |fade| fade.label_opacities(now));
    let fit_account = |folder_width| {
        HeaderAccount::fit(
            session,
            room - folder_width,
            COUNT_GAP,
            width >= DETAIL_MIN_WIDTH,
            &fit,
        )
    };
    let account = fit_account(current_width);
    // Each account fit shares its half of the fade with its folder. This
    // keeps both fits at their own widths and the right edge anchored.
    let (account, account_opacities) = if let Some(fade) = fade {
        let old = fit_account(old_width);
        let name_changes = old.name != account.name || old.quiet != account.quiet;
        let email_changes = name_changes || old.email != account.email;
        let (leaving, opacities) = fade.account_frame([name_changes, email_changes], now);
        (if leaving { old } else { account }, opacities)
    } else {
        (account, [1.; 2])
    };
    let folder_slot = |group: gpui::Div, width: f32| {
        div()
            .flex_none()
            .w(px(width))
            .overflow_hidden()
            .flex()
            .justify_end()
            .child(group)
    };

    div()
        .h(px(44.))
        .px(px(PAD))
        .flex()
        .items_center()
        .gap(px(FOLDER_GAP))
        .border_b(px(1.))
        .border_color(rgb(theme.border))
        .bg(rgb(theme.surface))
        .child(div().flex_1().min_w_0().flex().child(account.render(
            theme,
            COUNT_GAP,
            account_opacities,
        )))
        .child(
            div()
                .flex_none()
                .w(px(current_width.max(old_width)))
                .relative()
                .overflow_hidden()
                .flex()
                .justify_end()
                .child(folder_slot(current, current_width).opacity(incoming))
                .children(leaving.map(|(old, width)| {
                    folder_slot(old, width)
                        .absolute()
                        .right_0()
                        .top_0()
                        .opacity(outgoing)
                })),
        )
}

fn thread_list(
    theme: &Theme,
    session: &Session,
    beside: bool,
    list_width: f32,
    scroll: UniformListScrollHandle,
    cached_results: Option<(u64, gpui::AnyView)>,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
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
    if let Some(search) = &session.search {
        let count = session.threads.len();
        pane = pane.child(search::feedback(theme, search, count, cx));
        return if count == 0 {
            pane.child(empty_state(theme, session))
        } else if let Some((revision, cached)) = cached_results {
            pane.child(
                div()
                    .id(("cached-search-rows", revision))
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .child(cached),
            )
        } else {
            pane.child(search::rows(theme, count, scroll, cx))
        };
    }
    if session.threads.is_empty() {
        return pane.child(empty_state(theme, session));
    }

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
                // The list also asks for row 0 alone to measure a row's
                // height; only the rows it draws count as on screen.
                let drawn = range.len() > 1 || inbox.session.threads.len() == 1;
                if drawn {
                    let ids = inbox.session.threads.iter().map(|row| row.id.as_str());
                    inbox.row_widths.keep_drawn(range.clone(), ids);
                }
                let rows: Vec<_> = range
                    .map(|index| {
                        let row = inbox.session.threads[index].clone();
                        let frame = inbox.row_widths.frame(
                            &row.id,
                            &row.subject,
                            (row.unread, row.starred),
                            |unread| line_px(window, &row.subject, row_weight(unread)),
                            now,
                            drawn,
                        );
                        let previous_labels = if drawn {
                            inbox.row_widths.labels_frame(&row.id, &row.labels, now)
                        } else {
                            None
                        };
                        let selected = highlighted.contains(&row.id);
                        let look = RowLook {
                            selected,
                            fading: inbox
                                .motion
                                .selection_left(&row.id, now)
                                .filter(|_| !selected),
                            ghost: false,
                            bar_dx,
                            frame,
                            previous_labels,
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
                                    frame: still_frame(&gone, window),
                                    previous_labels: None,
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
                    .collect();
                // A row that turned read, unread or starred this frame
                // needs the next one; render keeps asking while it moves.
                if inbox.row_widths.running(now) {
                    window.request_animation_frame();
                }
                rows
            })
        },
    )
    .flex_1()
    .min_h(px(0.))
    .track_scroll(scroll);
    pane.child(rows)
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

/// A row drawn as it is, with nothing moving: a row that left the list,
/// or one shaped ahead of time.
fn still_frame(row: &ThreadRow, window: &Window) -> RowFrame {
    let natural = line_px(window, &row.subject, row_weight(row.unread));
    RowFrame::still(natural, row.unread, row.starred)
}

/// The reader moved the list's cursor, or came back to the list (`Esc`,
/// `h`, a click): given the thread cursor and pane before and after.
fn moved_in_list(before: (Option<&str>, Pane), after: (Option<&str>, Pane)) -> bool {
    before.0 != after.0 || (before.1 != Pane::List && after.1 == Pane::List)
}

/// The cursors, folder and pane before a key ([`Inbox::before_step`]).
struct StepBefore {
    message: usize,
    thread: Option<String>,
    folder: String,
    pane: Pane,
    searching: bool,
    search_generation: u64,
    help_open: bool,
}

/// How a thread row is drawn.
struct RowLook {
    selected: bool,
    /// The selection just left this row: how much of its highlight shows.
    fading: Option<f32>,
    /// A row that left the list, fading where it was. Not clickable.
    ghost: bool,
    /// How far right of its place the unread bar is drawn ([`unread_bar_dx`]).
    bar_dx: f32,
    /// Its weight, star and subject box this frame.
    frame: RowFrame,
    previous_labels: Option<(Vec<String>, f32)>,
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
    let frame = look.frame;
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
                .when(frame.unread > 0.0, |bar| {
                    bar.bg(rgb(theme.accent)).opacity(frame.unread)
                }),
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
                .child(thread_line_one(
                    theme,
                    row,
                    (frame, background),
                    selected,
                    text_width,
                    look.previous_labels,
                    window,
                ))
                .child(thread_line_two(
                    theme,
                    row,
                    (frame, background),
                    selected,
                    text_width,
                    window,
                )),
        )
        .into_any_element()
}

const THREAD_LINE_GAP: f32 = 8.0;
const THREAD_ATTACHMENT_MARK: &str = "⧉";

fn thread_line_one(
    theme: &Theme,
    row: &ThreadRow,
    (frame, background): (RowFrame, u32),
    selected: bool,
    text_width: f32,
    previous_labels: Option<(Vec<String>, f32)>,
    window: &Window,
) -> gpui::Div {
    let dim = if selected {
        theme.on_selection
    } else {
        theme.dimmed
    };
    let count = (row.message_count > 1).then(|| row.message_count.to_string());
    let date = present::relative_date(now_unix(), row.date_unix);
    let size = theme.ui_px - 1.0;
    let style = conversation_line_style(window, theme, px(size));
    let date_width = line_px_with_style(window, &style, &date, FontWeight::NORMAL).ceil();
    let count_width = count.as_deref().map_or(0.0, |count| {
        line_px_with_style(window, &style, count, FontWeight::NORMAL).ceil() + THREAD_LINE_GAP
    });
    let attachment_width = if row.has_attachment {
        line_px(window, THREAD_ATTACHMENT_MARK, FontWeight::NORMAL).ceil() + THREAD_LINE_GAP
    } else {
        0.0
    };
    // The leading group, star, and date need two gaps. Inside the group,
    // optional metadata and labels each need one gap after the previous item.
    let room = (text_width
        - date_width
        - count_width
        - attachment_width
        - STAR_SLOT
        - 2.0 * THREAD_LINE_GAP)
        .max(0.0);
    let labels = if row.labels.is_empty() && previous_labels.is_none() {
        None
    } else {
        thread_label_group(
            theme,
            row,
            background,
            dim,
            (room * 0.40 - THREAD_LINE_GAP).max(0.0),
            previous_labels,
            window,
        )
    };
    let label_space = labels
        .as_ref()
        .map_or(0.0, |(_, width)| width + THREAD_LINE_GAP);
    let sender = names(
        theme,
        row,
        frame,
        selected,
        (room - label_space).floor(),
        window,
    );
    div()
        .h(px(22.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(THREAD_LINE_GAP))
        .min_w(px(0.))
        .overflow_hidden()
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .items_center()
                .gap(px(THREAD_LINE_GAP))
                .child(sender)
                .when_some(count, |line, count| {
                    line.child(
                        div()
                            .flex_none()
                            .text_color(rgb(dim))
                            .text_size(px(size))
                            .child(count),
                    )
                })
                .when(row.has_attachment, |line| {
                    line.child(
                        div()
                            .flex_none()
                            .text_color(rgb(dim))
                            .child(THREAD_ATTACHMENT_MARK),
                    )
                })
                .children(labels.map(|(group, _)| group)),
        )
        // A fixed slot every row keeps, so starring never moves the line.
        // The star fades in as it grows to full size, and out as it shrinks.
        .child(
            div()
                .flex_none()
                .w(px(STAR_SLOT))
                .flex()
                .justify_center()
                .items_center()
                .text_color(rgb(theme.star))
                .when(frame.star > 0.0, |slot| {
                    slot.child(
                        div()
                            .opacity(frame.star)
                            .text_size(px(theme.ui_px * frame.star_scale()))
                            .child("★"),
                    )
                }),
        )
        .child(
            div()
                .flex_none()
                .text_color(rgb(dim))
                .text_size(px(size))
                .child(date),
        )
}

const LABEL_GAP: f32 = 8.0;
const LABEL_CHIP_CHROME: f32 = 14.0;
const LABEL_CHIP_MAX: f32 = 144.0;

fn thread_label_group(
    theme: &Theme,
    row: &ThreadRow,
    background: u32,
    dim: u32,
    room: f32,
    previous: Option<(Vec<String>, f32)>,
    window: &Window,
) -> Option<(gpui::Div, f32)> {
    let style = conversation_line_style(window, theme, px((theme.ui_px - 2.0).max(11.0)));
    let outline = theme.label_outline(background);
    let layout = |labels: &[String]| {
        let mut width = 0.0;
        let mut chips = Vec::new();
        let overflow = |hidden| format!("+{hidden}");
        for (index, label) in labels.iter().take(2).enumerate() {
            let hidden = labels.len() - index - 1;
            let reserve = if hidden > 0 {
                line_px_with_style(window, &style, &overflow(hidden), FontWeight::NORMAL).ceil()
                    + LABEL_CHIP_CHROME
                    + LABEL_GAP
            } else {
                0.0
            };
            let gap = if chips.is_empty() { 0.0 } else { LABEL_GAP };
            let budget = (room - width - gap - reserve).min(LABEL_CHIP_MAX) - LABEL_CHIP_CHROME;
            if budget <= 0.0 {
                break;
            }
            let label = present::display_safe(label);
            let Some(line) = clip_shaped(window, &style, &label, budget, FontWeight::NORMAL, None)
            else {
                break;
            };
            let chip_width = f32::from(line.width).ceil() + LABEL_CHIP_CHROME;
            let too_short = line.text != label
                && line
                    .text
                    .trim_end_matches('…')
                    .chars()
                    .filter(|c| !c.is_whitespace())
                    .take(5)
                    .count()
                    < 5;
            if chip_width > budget + LABEL_CHIP_CHROME || too_short {
                break;
            }
            width += gap + chip_width;
            chips.push(label_chip(
                theme,
                line.text,
                chip_width,
                row.warn && index == 0,
                outline,
                dim,
            ));
        }
        let hidden = labels.len() - chips.len();
        if hidden > 0 {
            let text = overflow(hidden);
            let chip_width = line_px_with_style(window, &style, &text, FontWeight::NORMAL).ceil()
                + LABEL_CHIP_CHROME;
            let gap = if chips.is_empty() { 0.0 } else { LABEL_GAP };
            if width + gap + chip_width <= room {
                width += gap + chip_width;
                chips.push(label_chip(
                    theme,
                    text.into(),
                    chip_width,
                    false,
                    outline,
                    dim,
                ));
            }
        }
        let group = div()
            .flex()
            .flex_row()
            .gap(px(LABEL_GAP))
            .w(px(width))
            .children(chips);
        (width, group)
    };
    let (width, group) = layout(&row.labels);
    if let Some((previous, shown)) = previous {
        let (old_width, old_group) = layout(&previous);
        let transition_width = old_width.max(width);
        if transition_width > 0.0 {
            return Some((
                div()
                    .relative()
                    .flex_none()
                    .w(px(transition_width))
                    .h(px(LABEL_CHIP_PX))
                    .child(group.opacity(shown))
                    .child(
                        old_group
                            .absolute()
                            .left(px(0.0))
                            .top(px(0.0))
                            .opacity(1.0 - shown),
                    ),
                transition_width,
            ));
        }
    }
    (width > 0.0).then(|| (group.flex_none(), width))
}

/// A label on a row: an outline that fits inside the 22 px line, so all
/// four of its edges show. A warning is in the error colour, which holds
/// AA on the selection as well as on the page.
fn label_chip(
    theme: &Theme,
    label: SharedString,
    width: f32,
    warn: bool,
    outline: u32,
    dim: u32,
) -> gpui::Div {
    let size = (theme.ui_px - 2.0).max(11.0);
    div()
        .flex_none()
        .w(px(width))
        .h(px(LABEL_CHIP_PX))
        .overflow_hidden()
        .whitespace_nowrap()
        .flex()
        .items_center()
        .px(px(6.))
        .rounded_md()
        .border_1()
        .border_color(rgb(if warn { theme.error } else { outline }))
        .text_size(px(size))
        .line_height(px(size + 2.0))
        .text_color(rgb(if warn { theme.error } else { dim }))
        .child(label)
}

/// A row chip's height, border included: under the 22 px line it sits on.
const LABEL_CHIP_PX: f32 = 20.;

/// Width of the star slot before the date on every row.
const STAR_SLOT: f32 = 14.0;

/// The weight a row's text takes: bold when unread.
fn row_weight(unread: bool) -> FontWeight {
    if unread {
        FontWeight::BOLD
    } else {
        FontWeight::NORMAL
    }
}

/// The names on line one. The span is as wide as the names in bold
/// whatever the row's weight (a few pixels nobody sees on a read row), so
/// marking a row read or unread never moves what follows them on the line
/// (count, attachment, labels).
fn names(
    theme: &Theme,
    row: &ThreadRow,
    frame: RowFrame,
    selected: bool,
    limit: f32,
    window: &Window,
) -> gpui::Div {
    let dim = if selected {
        theme.on_selection
    } else {
        theme.dimmed
    };
    let spans = &row.spans;
    let extra = row.extra;
    let parts = |bold: bool| -> Vec<(String, FontWeight)> {
        spans
            .iter()
            .enumerate()
            .map(|(index, span)| {
                let text = if index + 1 == spans.len() && extra == 0 {
                    span.text.clone()
                } else {
                    format!("{}, ", span.text)
                };
                let span_weight = if bold {
                    FontWeight::BOLD
                } else if span.emphasis {
                    FontWeight::SEMIBOLD
                } else {
                    FontWeight::NORMAL
                };
                (text, span_weight)
            })
            .collect()
    };
    let extra_text = (extra > 0).then(|| format!("+{extra}"));
    let bold_parts = parts(true);
    let width = (bold_parts
        .iter()
        .map(|(text, span_weight)| line_px(window, text, *span_weight))
        .sum::<f32>()
        + extra_text
            .as_deref()
            .map_or(0.0, |text| line_px(window, text, FontWeight::NORMAL)))
    .ceil();
    let text_color = if selected {
        theme.on_selection
    } else {
        theme.foreground
    };
    let mut line = div().flex().flex_row().whitespace_nowrap();
    let width = if width > limit {
        let names: String = bold_parts
            .iter()
            .map(|(text, _)| text.as_str())
            .chain(extra_text.as_deref())
            .collect();
        let visible = clip_shaped(
            window,
            &window.text_style(),
            &names,
            limit,
            FontWeight::BOLD,
            None,
        )
        .filter(|line| f32::from(line.width).ceil() <= limit);
        let fitted_width = visible
            .as_ref()
            .map_or(0., |line| f32::from(line.width).ceil());
        line = line.child(
            div()
                .flex_none()
                .font_weight(row_weight(frame.bold))
                .text_color(rgb(text_color))
                .children(visible.map(|line| line.text)),
        );
        fitted_width
    } else {
        for (text, span_weight) in parts(frame.bold) {
            line = line.child(
                div()
                    .flex_none()
                    .font_weight(span_weight)
                    .text_color(rgb(text_color))
                    .child(text),
            );
        }
        if let Some(text) = extra_text {
            line = line.child(div().flex_none().text_color(rgb(dim)).child(text));
        }
        width
    };
    div()
        .relative()
        .flex_none()
        .w(px(width))
        .overflow_hidden()
        .child(line)
}

/// Line two: the subject and the snippet. `background` is the row's fill
/// this frame, which the snippet's edge fades into.
fn thread_line_two(
    theme: &Theme,
    row: &ThreadRow,
    (frame, background): (RowFrame, u32),
    selected: bool,
    text_width: f32,
    window: &Window,
) -> gpui::Div {
    let snippet_color = if selected {
        theme.on_selection_dim
    } else {
        theme.dimmed
    };
    let subject_color = if selected {
        theme.on_selection
    } else {
        theme.foreground
    };
    // The box is drawn where it is now (it eases while it grows or
    // relaxes); its text and the snippet are cut where it is going, so
    // they do not change from frame to frame.
    let (box_w, _) = row_widths::line_two(frame.subject_w, text_width);
    let (room, rest_x) = row_widths::line_two(frame.subject_room, text_width);
    let snippet_w = (text_width - rest_x).max(24.0);
    // Shaped once here and drawn from that shaping: a row that comes into
    // view shapes each of its two texts at most twice.
    let style = window.text_style();
    let snippet = clip_shaped(
        window,
        &style,
        &row.snippet,
        snippet_w,
        FontWeight::NORMAL,
        Some(snippet_color),
    );
    let subject = (!row.subject.is_empty()).then(|| {
        let shaped = clip_shaped(
            window,
            &style,
            &row.subject,
            room,
            row_weight(frame.bold),
            Some(subject_color),
        );
        div()
            .relative()
            .flex_none()
            .w(px(box_w))
            .overflow_hidden()
            .children(shaped.map(|line| shaped_text(line, window)))
    });
    // A relaxing box carries the snippet, cut for where it is going, past
    // the end of the line: its last pixels fade into the row instead of
    // showing half a letter.
    let edge = frame.edge_fade();
    let edge_fade = (edge > 0.0).then(|| {
        let solid = gpui::Hsla::from(rgb(background));
        let clear = gpui::Hsla { a: 0.0, ..solid };
        div()
            .absolute()
            .top(px(0.))
            .bottom(px(0.))
            .right(px(0.))
            .w(px(row_widths::EDGE_FADE))
            .opacity(edge)
            .bg(gpui::linear_gradient(
                90.,
                gpui::linear_color_stop(clear, 0.),
                gpui::linear_color_stop(solid, 1.),
            ))
    });
    // Keep line two at a fixed height while the snippet fills in.
    div()
        .relative()
        .w(px(text_width))
        .h(px(22.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(row_widths::SUBJECT_GAP))
        .overflow_hidden()
        .children(subject)
        .children(snippet.map(|line| {
            div()
                .flex_none()
                .w(px((text_width - box_w - row_widths::SUBJECT_GAP).max(24.0)))
                .overflow_hidden()
                .child(shaped_text(line, window))
        }))
        .children(edge_fade)
}

fn line_px(window: &Window, text: &str, weight: FontWeight) -> f32 {
    line_px_with_style(window, &window.text_style(), text, weight)
}

fn line_px_with_style(window: &Window, style: &TextStyle, text: &str, weight: FontWeight) -> f32 {
    shape_ui_line(window, style, text, weight, None).map_or(0.0, |line| f32::from(line.width))
}

/// `text` shaped as one line in the UI font at `weight`, in `color` (the
/// inherited one when `None`).
fn shape_ui_line(
    window: &Window,
    style: &TextStyle,
    text: &str,
    weight: FontWeight,
    color: Option<u32>,
) -> Option<gpui::ShapedLine> {
    if text.is_empty() {
        return None;
    }
    let mut style = style.clone();
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
fn clip_line(
    window: &Window,
    style: &TextStyle,
    text: &str,
    max_px: f32,
    weight: FontWeight,
) -> String {
    clip_shaped(window, style, text, max_px, weight, None)
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
    style: &TextStyle,
    text: &str,
    max_px: f32,
    weight: FontWeight,
    color: Option<u32>,
) -> Option<gpui::ShapedLine> {
    // No glyph of a UI font is narrower than a fifth of an em, so more
    // characters than this can never fit.
    let font_px = f32::from(style.font_size.to_pixels(window.rem_size()));
    let most = (max_px / (font_px * 0.2).max(1.0)).ceil() as usize + 2;
    let probe = match text.char_indices().nth(most) {
        Some((cut, _)) => &text[..cut],
        None => text,
    };
    let mut line = shape_ui_line(window, style, probe, weight, color)?;
    if f32::from(line.width) <= max_px {
        if probe.len() == text.len() {
            return Some(line);
        }
        // Narrower glyphs than any font has: measure the whole line.
        line = shape_ui_line(window, style, text, weight, color)?;
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
    let mut room = max_px - line_px_with_style(window, style, "…", weight);
    // The whole line does not fit: the cut is before its end.
    let mut before = shaped.len();
    loop {
        let kept = text_cut::kept(&shaped[..cuts.fit(room, before)]);
        // No space or punctuation before the ellipsis: "Friday…".
        let cut = shape_ui_line(window, style, &present::ellipsis(kept), weight, color)?;
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
    if let Some(search) = &session.search {
        return search::empty(search);
    }
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
        .children(
            banner
                .filter(|item| !item.hint.is_empty())
                .map(|item| div().text_color(rgb(theme.dimmed)).child(item.hint)),
        )
}

const SPLITTER_PX: f32 = 7.;

fn splitter(theme: &Theme, cx: &mut Context<Inbox>) -> impl IntoElement {
    div()
        .id("split")
        .w(px(SPLITTER_PX))
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

const CONVERSATION_RULE_PX: f32 = 2.;
const CONVERSATION_PAD_PX: f32 = 20.;
const MESSAGE_PAD_PX: f32 = 8.;
const COLLAPSED_GAP_PX: f32 = 12.;
const COLLAPSED_NARROW_GAP_PX: f32 = 8.;
const COLLAPSED_SENDER_PX: f32 = 140.;

fn message_content_width(conv_width: f32) -> f32 {
    (conv_width - CONVERSATION_RULE_PX - 2. * (CONVERSATION_PAD_PX + MESSAGE_PAD_PX)).max(0.)
}

const READING_WIDE_PX: f32 = 1040.;
const READING_GUTTER_PX: f32 = 48.;

#[derive(Clone, Copy)]
struct ReaderColumn {
    width: f32,
    inset: f32,
}

impl ReaderColumn {
    fn place(self, child: impl IntoElement) -> gpui::Div {
        div().ml(px(self.inset)).w(px(self.width)).child(child)
    }
}

/// Column positions are interpolated in window coordinates. Each element
/// uses the difference from the sliding pane's left edge as its inset.
#[derive(Clone, Copy)]
struct ReaderColumns {
    header: ReaderColumn,
    prose: ReaderColumn,
    card: ReaderColumn,
    wide: ReaderColumn,
    split_width: f32,
    reading_prose: f32,
    reading_wide: f32,
    pane_right: f32,
    progress: f32,
    reflow: Option<motion::ReadingReflow>,
}

impl ReaderColumns {
    fn split(width: f32) -> Self {
        Self::new(width, 0.0, false, 0.0, 1.0)
    }

    fn pane_left(split: f32, progress: f32, scale: f32) -> f32 {
        motion::snap(split * (1.0 - progress), scale)
    }

    fn new(window: f32, split_left: f32, reading: bool, progress: f32, scale: f32) -> Self {
        let pane_left = Self::pane_left(split_left, progress, scale);
        let split = message_content_width(window - split_left);
        let full = message_content_width(window);
        let available = message_content_width(window - pane_left);
        let reading_prose = full.min(READING_MEASURE_PX);
        let reading_wide = full
            .min(READING_WIDE_PX)
            .min((window - 2.0 * READING_GUTTER_PX).max(split));
        let centered_inset = |to_width: f32| {
            let to_left = ((full - to_width) * 0.5).round();
            let left = motion::snap(motion::lerp(split_left, to_left, progress), scale);
            (left - pane_left).max(0.0)
        };
        let inset = centered_inset(reading_prose);
        let card = ReaderColumn {
            width: motion::lerp(split, reading_prose, progress)
                .round()
                .min((available - inset).max(0.0).floor()),
            inset,
        };
        Self {
            header: card,
            prose: card,
            card,
            wide: ReaderColumn {
                width: reading_wide,
                inset: centered_inset(reading_wide),
            },
            split_width: split,
            reading_prose,
            reading_wide,
            pane_right: available,
            progress,
            reflow: None,
        }
        .at_measure(reading)
    }

    fn at_measure(mut self, reading: bool) -> Self {
        self.header.width = if reading {
            self.reading_prose
        } else {
            self.split_width
        };
        self.prose.width = self.header.width.min(READING_MEASURE_PX);
        self.wide.width = if reading {
            self.reading_wide
        } else {
            self.split_width
        };
        self
    }

    fn with_reflow(mut self, frame: Option<motion::ReadingReflow>) -> Self {
        self.reflow = frame;
        self
    }

    fn card_right(self) -> f32 {
        self.card.inset + self.card.width
    }

    fn moving_prose(self) -> ReaderColumn {
        ReaderColumn {
            width: self.card.width.min(READING_MEASURE_PX),
            ..self.card
        }
    }

    fn flow(
        self,
        measure: impl Fn(Self) -> ReaderColumn,
        mut layer: impl FnMut(f32, bool) -> gpui::AnyElement,
    ) -> gpui::AnyElement {
        let column = measure(self);
        let place = |column: ReaderColumn, child| {
            disclosure::clip_column(
                column.inset,
                column.width,
                self.card_right(),
                div().child(child),
            )
            .into_any_element()
        };
        if let Some(frame) = self.reflow {
            let split = measure(self.at_measure(false));
            let reading = measure(self.at_measure(true));
            if split.width != reading.width {
                let live = frame.alpha[1] > frame.alpha[0];
                return disclosure::reflow(
                    self.pane_right,
                    frame,
                    [
                        place(split, layer(split.width, !live)),
                        place(reading, layer(reading.width, live)),
                    ],
                );
            }
        }
        place(column, layer(column.width, true))
    }

    fn blocks(self, fill: u32) -> crate::render::BlockColumns {
        let layout = |columns: Self| {
            crate::render::BlockColumn::reading(
                (columns.wide.width, columns.wide.inset),
                (columns.prose.width, columns.prose.inset),
                (columns.reading_wide, columns.progress),
                (columns.card_right(), columns.pane_right),
                fill,
            )
        };
        match self.reflow {
            Some(frame) => crate::render::BlockColumns::reflow(
                layout(self.at_measure(false)),
                layout(self.at_measure(true)),
                frame,
            ),
            None => layout(self).into(),
        }
    }

    fn date_column(self) -> ReaderColumn {
        ReaderColumn {
            width: self.card.width,
            inset: self.card.inset + MESSAGE_PAD_PX,
        }
    }
}

struct CollapsedRows {
    style: TextStyle,
    align_senders: bool,
    gap: f32,
    room: f32,
    sender_ellipsis: f32,
    snippet_ellipsis: f32,
    snippet_preview: f32,
}

impl CollapsedRows {
    fn new(window: &Window, theme: &Theme, content_width: f32) -> Self {
        let style = conversation_line_style(window, theme, px(theme.ui_px));
        let measure = |text, weight| line_px_with_style(window, &style, text, weight).ceil();
        let snippet_preview = measure("mmm…", FontWeight::NORMAL);
        // Use a full date as the width reference for the pane, so a shorter
        // date or an empty preview cannot switch one row's layout.
        let date_width = measure("May 28, 2000", FontWeight::NORMAL);
        let align_senders = content_width
            >= COLLAPSED_SENDER_PX + snippet_preview + date_width + 2. * COLLAPSED_GAP_PX;
        let gap = if align_senders {
            COLLAPSED_GAP_PX
        } else {
            COLLAPSED_NARROW_GAP_PX
        };
        Self {
            align_senders,
            gap,
            room: (content_width - 2. * gap).max(0.).floor(),
            sender_ellipsis: measure("…", FontWeight::MEDIUM),
            snippet_ellipsis: measure("…", FontWeight::NORMAL),
            snippet_preview,
            style,
        }
    }
}

fn collapsed_sender_budget(
    remaining: f32,
    sender_ellipsis: f32,
    snippet_ellipsis: f32,
    snippet_preview: f32,
) -> f32 {
    let sender_ellipsis = sender_ellipsis.ceil();
    let snippet_ellipsis = snippet_ellipsis.ceil();
    let snippet_preview = snippet_preview.ceil();
    let reserve = if remaining >= sender_ellipsis + snippet_preview {
        snippet_preview
    } else if remaining >= sender_ellipsis + snippet_ellipsis {
        snippet_ellipsis
    } else {
        0.
    };
    (remaining - reserve).min(COLLAPSED_SENDER_PX)
}

/// The latest reread and accepted HTML preparations for the open thread.
#[derive(Default)]
struct BodyRefresh {
    messages: Option<Vec<ConvMessage>>,
    rich: HashMap<String, Rich>,
}

impl BodyRefresh {
    fn is_empty(&self) -> bool {
        self.messages.is_none() && self.rich.is_empty()
    }

    fn apply(self, session: &mut Session, motion: &mut InboxMotion, now: Instant) {
        let mut rich = self.rich;
        if let Some(mut messages) = self.messages {
            for message in &mut messages {
                if let Some(ready) = rich.remove(&message.id) {
                    message.rich = ready;
                }
            }
            let changed = present::bodies_filled(&session.messages, &messages)
                .expect("a queued reread keeps the same message order");
            for index in changed {
                motion.body_arrived(&messages[index].id, now);
            }
            session.refresh_same_messages(messages);
        } else {
            for message in &mut session.messages {
                if let Some(ready) = rich.remove(&message.id)
                    && ready != message.rich
                {
                    if message.expanded {
                        motion.body_arrived(&message.id, now);
                    }
                    message.rich = ready;
                }
            }
        }
    }
}

/// One thread's messages as the conversation shows them, with whether
/// each has its body yet. `shown`: the account address, its folder roles
/// and names, and the folder the list shows ([`present::conversation`]).
pub fn read_conversation(
    store: &fell_core::Store,
    account: &AccountId,
    thread: &ThreadId,
    shown: (&str, &FolderRoles, &[String], &str),
) -> Result<Vec<ConvMessage>, fell_core::Error> {
    let (me, roles, names, viewing) = shown;
    let messages = store.thread_messages(account, thread)?;
    let ids: Vec<_> = messages
        .iter()
        .map(|message| message.headers.message_id.clone())
        .collect();
    let states = store.body_states(account, &ids)?;
    let mut view = present::conversation(&messages, me, roles, names, viewing);
    for ((message, state), stored) in view.iter_mut().zip(states).zip(&messages) {
        message.body_state = state;
        if stored.has_attachment {
            let parts = store.message_attachments(account, &stored.headers.message_id)?;
            message.attachments = attachments::visible_parts(parts.unwrap_or_default());
        }
    }
    Ok(view)
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
    columns: ReaderColumns,
    live: Live,
    frame: &ConvFrame,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    // The card under the cursor shows it as much as the pane is focused,
    // so on Enter and Esc its background turns with the focus edge.
    let focus = motion.focus_edge(session.pane, now);
    let arrival = motion.arrival(now);
    let hold = frame.anchor.map(|anchor| (anchor, LayoutSlot::default()));
    let chip_probe = (live.0 && frame.reveal_attachment).then(LayoutSlot::default);
    let collapsed_rows = OnceCell::new();
    let retained = !frame.preview_current && session.search.is_some() && !subject.is_empty();
    // Short subjects keep their one-line measure in both modes.
    let title_width = columns.reflow.map_or(f32::INFINITY, |_| {
        let style = conversation_line_style(window, theme, px(theme.ui_px + 11.0));
        line_px_with_style(window, &style, &subject, FontWeight::BOLD).ceil() + 4.0
    });
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
            let block = if message.expanded {
                let fill = expanded_card_fill(theme, cursor);
                expanded_message(
                    theme,
                    session,
                    message,
                    index,
                    (cursor, fill),
                    columns,
                    live,
                    frame,
                    chip_probe.as_ref(),
                    window,
                    cx,
                )
            } else {
                let body = frame.alphas.get(&message.id).copied().unwrap_or(1.0);
                collapsed_message(
                    theme,
                    message,
                    index,
                    (cursor, body),
                    columns,
                    collapsed_rows
                        .get_or_init(|| CollapsedRows::new(window, theme, columns.card.width)),
                    live,
                    window,
                    cx,
                )
            };
            if let Some((anchor, probe)) = &hold
                && anchor.index == index
            {
                LayoutProbe {
                    inner: block,
                    slot: probe.clone(),
                }
                .into_any_element()
            } else {
                block
            }
        })
        .collect();
    div()
        .id("conversation")
        .relative()
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .border_l(px(CONVERSATION_RULE_PX))
        .border_color(rgb(motion::mix(
            theme.background,
            theme.accent,
            focus * (1.0 - columns.progress),
        )))
        .when(!subject.is_empty(), |pane| {
            pane.child(
                div()
                    .pt(px(22.))
                    .pb(px(14.))
                    .text_size(px(theme.ui_px + 11.0))
                    .font_weight(FontWeight::BOLD)
                    .line_height(px(theme.ui_px + 16.0))
                    .border_b(px(1.))
                    .border_color(rgb(theme.border))
                    .child(
                        div()
                            .px(px(CONVERSATION_PAD_PX + MESSAGE_PAD_PX))
                            .when(arrival < 1.0, |text| text.opacity(arrival))
                            .child(columns.flow(
                                |columns| ReaderColumn {
                                    width: columns.header.width.min(title_width),
                                    ..columns.header
                                },
                                |_, _| div().child(subject.clone()).into_any_element(),
                            )),
                    ),
            )
        })
        .child({
            let body = div()
                .id("conversation-scroll")
                .flex_1()
                .min_h(px(0.))
                .map(|body| match &scroll {
                    Some(scroll) => body.overflow_y_scroll().track_scroll(scroll),
                    None => body.overflow_hidden(),
                })
                .px(px(CONVERSATION_PAD_PX))
                .pb(px(28.))
                .when(arrival < 1.0, |body| body.opacity(arrival))
                .children(messages)
                .into_any_element();
            let landing = chip_probe
                .map(|probe| (None, probe))
                .or_else(|| hold.map(|(anchor, probe)| (Some(anchor.y), probe)));
            match (scroll, landing) {
                (Some(scroll), Some((y, probe))) => LandingHold {
                    inner: body,
                    probe,
                    scroll,
                    y,
                    frames: frame.frames.clone(),
                }
                .into_any_element(),
                _ => body,
            }
        })
        .when(retained, |pane| {
            pane.child(preview_shield(theme, session.search_results_current()))
        })
}

fn expanded_card_fill(theme: &Theme, focus: f32) -> u32 {
    motion::mix(
        theme.background,
        theme.surface,
        motion::lerp(0.4, 1.0, focus),
    )
}

fn preview_shield(theme: &Theme, results_current: bool) -> impl IntoElement {
    div()
        .id("retained-preview")
        .absolute()
        .size_full()
        .top_0()
        .left_0()
        .occlude()
        .when(results_current, |shield| {
            shield.child(
                div()
                    .absolute()
                    .top(px(4.))
                    .right(px(16.))
                    .text_size(px(theme.ui_px - 1.0))
                    .text_color(rgb(theme.dimmed))
                    .bg(rgb(theme.background))
                    .child("Updating preview…"),
            )
        })
}

/// Conversation elements measure during render, before the root's font is active.
fn conversation_line_style(window: &Window, theme: &Theme, size: gpui::Pixels) -> TextStyle {
    let mut style = window.text_style();
    style.font_size = size.into();
    if let Some(family) = &theme.ui_family {
        style.font_family = SharedString::from(family.clone());
    }
    style
}

/// Give the date its own space while the leading fields ellipsize.
fn message_date_row<T: Styled + ParentElement>(
    row: T,
    gap: f32,
    leading: gpui::Div,
    date: Option<gpui::Div>,
) -> T {
    row.flex()
        .items_center()
        .gap(px(gap))
        .child(div().flex_1().min_w_0().overflow_hidden().child(leading))
        .children(date)
}

#[allow(clippy::too_many_arguments)]
fn collapsed_message(
    theme: &Theme,
    message: &ConvMessage,
    index: usize,
    (cursor, body): (f32, f32),
    columns: ReaderColumns,
    rows: &CollapsedRows,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let now = now_unix();
    let mut date = present::relative_date(now, message.date_unix);
    let dimmed = motion::mix(theme.dimmed, theme.on_selection, cursor);
    let missing = present::missing_body_line(message.body_state);
    let snippet = missing.as_deref().unwrap_or(&message.snippet);
    let measure =
        |text: &str| line_px_with_style(window, &rows.style, text, FontWeight::NORMAL).ceil();
    let snippet_preview = if snippet.is_empty() {
        0.
    } else {
        rows.snippet_preview
    };
    let mut date_w = measure(&date);
    let date_limit = if rows.align_senders {
        (rows.room - COLLAPSED_SENDER_PX - snippet_preview).max(0.)
    } else {
        rows.room
    };
    if !rows.align_senders || date_w > date_limit {
        let compact = present::compact_relative_date(now, message.date_unix);
        let width = measure(&compact);
        if width < date_w {
            date = compact;
            date_w = width;
        }
    }
    let date_w = date_w.min(date_limit);
    let remaining = rows.room - date_w;
    let sender_budget = if rows.align_senders {
        COLLAPSED_SENDER_PX
    } else {
        collapsed_sender_budget(
            remaining,
            rows.sender_ellipsis,
            rows.snippet_ellipsis,
            snippet_preview,
        )
    };
    let clip = |text: &str, width, weight| {
        clip_shaped(window, &rows.style, text, width, weight, None)
            // The clipping helper can return a lone ellipsis at any width.
            .filter(|line| line.width <= px(width))
    };
    let sender = message
        .sender
        .contains(['\r', '\n', '\u{2028}', '\u{2029}'])
        .then(|| present::one_line(&message.sender));
    let sender = clip(
        sender.as_deref().unwrap_or(&message.sender),
        sender_budget,
        FontWeight::MEDIUM,
    );
    // Keep the aligned column in wide panes. In narrow ones, let a short
    // sender give its unused pixels to the preview.
    let sender_w = if rows.align_senders {
        sender_budget
    } else {
        sender
            .as_ref()
            .map_or(0., |line| f32::from(line.width).ceil())
    };
    let snippet_w = remaining - sender_w;
    let dates = columns.date_column();
    let row = div()
        .id(gpui::ElementId::named_usize("message", index))
        .h(px(40.))
        .ml(px(dates.inset - MESSAGE_PAD_PX))
        .w(px(dates.width + 2.0 * MESSAGE_PAD_PX))
        .min_w(px(0.))
        .px(px(MESSAGE_PAD_PX))
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
                    if !inbox.search_conversation_live() {
                        return;
                    }
                    window.focus(&inbox.focus);
                    inbox.cancel_pending_keys(cx);
                    inbox.focus_message(index);
                    if let Some(message) = inbox.session.messages.get_mut(index) {
                        message.expanded = true;
                    }
                    cx.notify();
                }),
            )
        });
    let leading = div()
        .flex()
        .items_center()
        .gap(px(rows.gap))
        .child(
            div()
                .w(px(sender_w))
                .flex_none()
                .whitespace_nowrap()
                .font_weight(FontWeight::MEDIUM)
                .children(sender.map(|line| line.text)),
        )
        .child(
            div()
                .w(px(snippet_w))
                .flex_none()
                .whitespace_nowrap()
                .text_color(rgb(dimmed))
                .when(body < 1.0, |line| line.opacity(body))
                .children(clip(snippet, snippet_w, FontWeight::NORMAL).map(|line| line.text)),
        );
    message_date_row(
        row,
        rows.gap,
        leading,
        Some(
            div()
                .w(px(date_w))
                .flex_none()
                .whitespace_nowrap()
                .text_right()
                .text_color(rgb(dimmed))
                .children(clip(&date, date_w, FontWeight::NORMAL).map(|line| line.text)),
        ),
    )
    .into_any_element()
}

const MESSAGE_HEADER_GAP_PX: f32 = 10.0;

fn expanded_header_widths(width: f32, [sender, email, folders, date]: [f32; 4]) -> [f32; 4] {
    // The folder group also acts as the spacer before the date when empty.
    let gaps =
        (1 + usize::from(sender > 0.0) + usize::from(email > 0.0)) as f32 * MESSAGE_HEADER_GAP_PX;
    let date = date.min(width);
    let room = (width - date - gaps).max(0.0);
    // When the identity needs to truncate, give it at least two thirds of
    // the room. A short or absent folder list leaves more for the sender.
    let identity = (room - folders.min(room / 3.0)).min(sender + email);
    let email = email.min(identity - sender.min(identity / 2.0));
    let sender = sender.min(identity - email);
    let folders = folders.min((room - sender - email).max(0.0));
    [sender, email, folders, date]
}

fn expanded_message_header(
    theme: &Theme,
    message: &ConvMessage,
    columns: ReaderColumns,
    window: &Window,
) -> gpui::Div {
    let style = conversation_line_style(window, theme, px(theme.ui_px - 1.));
    let sender_style = conversation_line_style(window, theme, px(theme.ui_px));
    let date = present::relative_date(now_unix(), message.date_unix);
    let date_width = line_px_with_style(window, &sender_style, &date, FontWeight::NORMAL).ceil();
    let sender = present::display_safe(&message.sender);
    let email = present::display_safe(&message.email);
    let folders = message
        .folders
        .iter()
        .map(|folder| present::display_safe(folder))
        .collect::<Vec<_>>();
    let joined = folders.join(" · ");
    let measure = |style: &TextStyle, text: &str, weight, color| {
        shape_ui_line(window, style, text, weight, Some(color))
            .map_or(0.0, |line| f32::from(line.width).ceil())
    };
    let [sender_width, email_width, folder_width, _] = expanded_header_widths(
        columns.card.width,
        [
            measure(
                &sender_style,
                &sender,
                FontWeight::SEMIBOLD,
                theme.foreground,
            ),
            measure(&style, &email, FontWeight::NORMAL, theme.dimmed),
            measure(&style, &joined, FontWeight::NORMAL, theme.dimmed),
            date_width,
        ],
    );
    let fit = |style: &TextStyle, text: &str, width: f32, weight, color| {
        clip_shaped(window, style, text, width.floor(), weight, Some(color))
            .filter(|line| !line.text.is_empty() && f32::from(line.width).ceil() <= width)
    };
    let sender = fit(
        &sender_style,
        &sender,
        sender_width,
        FontWeight::SEMIBOLD,
        theme.foreground,
    );
    let email = fit(
        &style,
        &email,
        email_width,
        FontWeight::NORMAL,
        theme.dimmed,
    );
    let line_width = |line: &Option<gpui::ShapedLine>| {
        line.as_ref()
            .map_or(0., |line| f32::from(line.width).ceil())
    };
    let folder_width =
        folder_width + (sender_width - line_width(&sender)) + (email_width - line_width(&email));
    let fit_folders =
        |text: &str| fit(&style, text, folder_width, FontWeight::NORMAL, theme.dimmed);
    let fit_whole = |text: &str| fit_folders(text).filter(|line| line.text == text);
    let folders = fit_whole(&joined)
        .or_else(|| {
            let shortened = folders
                .iter()
                .map(|path| match path.split_once('/') {
                    Some((first, rest)) if rest.contains('/') => {
                        let leaf = rest.rsplit('/').next().unwrap_or(rest);
                        format!("{first}/…/{leaf}")
                    }
                    _ => path.clone(),
                })
                .collect::<Vec<_>>()
                .join(" · ");
            fit_whole(&shortened)
        })
        .or_else(|| {
            let leaves = folders
                .iter()
                .map(|path| match path.rsplit_once('/') {
                    Some((_, leaf)) => format!("…/{leaf}"),
                    None => path.clone(),
                })
                .collect::<Vec<_>>()
                .join(" · ");
            fit_folders(&leaves)
        });
    let leading = div()
        .flex()
        .items_center()
        .gap(px(MESSAGE_HEADER_GAP_PX))
        .children(
            [sender, email, folders]
                .into_iter()
                .flatten()
                .map(|line| shaped_text(line, window)),
        );
    let date = fit(
        &sender_style,
        &date,
        columns.card.width,
        FontWeight::NORMAL,
        theme.dimmed,
    );
    message_date_row(
        div()
            .w(px(columns.date_column().width))
            // The recipient line belongs to this header, so it sits close.
            .pb(px(if message.recipients.is_some() { 2. } else { 8. })),
        MESSAGE_HEADER_GAP_PX,
        leading,
        date.map(|line| div().flex_none().child(shaped_text(line, window))),
    )
}

#[allow(clippy::too_many_arguments)]
fn expanded_message(
    theme: &Theme,
    session: &Session,
    message: &ConvMessage,
    index: usize,
    (cursor, fill): (f32, u32),
    columns: ReaderColumns,
    live: Live,
    frame: &ConvFrame,
    chip_probe: Option<&LayoutSlot>,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let dim = theme.dimmed;
    div()
        .id(gpui::ElementId::named_usize("message", index))
        .px(px(MESSAGE_PAD_PX))
        .py(px(10.))
        .mb(px(8.))
        .relative()
        .child(
            div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(columns.card.inset))
                .w(px(columns.card.width + 2.0 * MESSAGE_PAD_PX))
                .rounded_md()
                .bg(rgb(fill)),
        )
        .when(live.0, |card| {
            card.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |inbox, _, window, cx| {
                    if !inbox.search_conversation_live() {
                        return;
                    }
                    window.focus(&inbox.focus);
                    inbox.cancel_pending_keys(cx);
                    inbox.focus_message(index);
                    cx.notify();
                }),
            )
        })
        .child(
            columns
                .card
                .place(expanded_message_header(theme, message, columns, window)),
        )
        .children(message.recipients.as_ref().map(|line| {
            div()
                .id(gpui::ElementId::named_usize("recipients", index))
                .child(columns.flow(
                    |columns| ReaderColumn {
                        width: columns.header.width.min(680.),
                        ..columns.header
                    },
                    |width, active| {
                        recipient_lines(
                            theme,
                            line,
                            session.recipients_are_open(index),
                            frame.disclosures.recipients.get(&message.id).copied(),
                            index,
                            width,
                            Live(live.0 && active),
                            window,
                            cx,
                        )
                    },
                ))
        }))
        .child(
            columns.card.place(
                div()
                    .children(
                        live.0
                            .then(|| search::match_reason(theme, session, message))
                            .flatten(),
                    )
                    // What a draft answers, under who it goes to, as in its window.
                    .children(message.context.as_ref().map(|line| {
                        div()
                            .max_w(px(680.))
                            // Close under the recipient line, as one header block.
                            .when(message.recipients.is_some(), |line| line.mt(px(-6.)))
                            .pb(px(10.))
                            .text_size(px(theme.ui_px - 1.0))
                            .text_color(rgb(dim))
                            .truncate()
                            .child(line.clone())
                    })),
            ),
        )
        // `q` opens every collapsed block in the message at once, so only
        // the first fold of the reader's message names the key.
        .child(message_body(
            theme,
            message,
            FoldView {
                open: session.is_open(index),
                columns,
                name_key: index == session.message_cursor,
                focused: cursor,
                fill,
                index,
                live: live.0,
            },
            frame,
            cx,
        ))
        .when(!message.attachments.is_empty(), |card| {
            card.child(div().id("attachment-layout").child(columns.flow(
                |columns| columns.header,
                |width, active| {
                    div()
                        .id(("attachment-measure", width as usize))
                        .child(attachment_chips::row(
                            theme,
                            session,
                            message,
                            index,
                            width,
                            Live(live.0 && active),
                            frame,
                            chip_probe.filter(|_| active),
                            window,
                            cx,
                        ))
                        .into_any_element()
                },
            )))
        })
        .into_any_element()
}

/// How one message's folds paint.
#[derive(Clone, Copy)]
struct FoldView {
    /// `q` has opened them.
    open: bool,
    columns: ReaderColumns,
    /// The first fold names `q`: this is the reader's message.
    name_key: bool,
    /// How much the message shows the cursor in the focused
    /// conversation, 0 to 1: the fold `q` acts on takes the accent with it.
    focused: f32,
    fill: u32,
    index: usize,
    /// Drawn on screen, not as an unseen copy: a click opens the fold.
    live: bool,
}

/// Height a folded line takes, its margin included.
const FOLD_ROW_PX: f32 = 34.0;
const FOLD_GAP_PX: f32 = 6.0;

/// The body under an expanded message's header: prepared HTML, the stored
/// text, or the room kept while the HTML is prepared.
fn message_body(
    theme: &Theme,
    message: &ConvMessage,
    folds: FoldView,
    frame: &ConvFrame,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let alpha = frame.alphas.get(&message.id).copied().unwrap_or(1.0);
    if let Some(line) = present::missing_body_line(message.body_state) {
        return folds.columns.flow(
            |columns| columns.prose,
            |_, _| {
                div()
                    .pb(px(8.))
                    .line_height(px(theme.ui_px * 1.45))
                    .text_color(rgb(theme.dimmed))
                    .opacity(alpha)
                    .child(line.clone())
                    .into_any_element()
            },
        );
    }
    let reveal = frame.disclosures.quotes.get(&message.id).copied();
    if let Rich::Html(body) = &message.rich {
        return div()
            .opacity(alpha)
            .children(html_sections(theme, body, folds, reveal, cx))
            .into_any_element();
    }
    let blocks: Vec<gpui::AnyElement> = match &message.rich {
        Rich::Html(_) => unreachable!(),
        Rich::Pending => vec![div().h(px(PENDING_BODY_PX)).into_any_element()],
        Rich::Plain | Rich::PlainPart(_) | Rich::Missing => {
            // A signature and the quote next to it fold as one, as in HTML.
            // A draft's markdown is painted as its compose window shows it.
            let palette = mail_body::palette(theme);
            let quiet = mail_body::history_palette(theme);
            let mut runs: Vec<(usize, Option<fell_render::Fold>, Vec<&str>)> = Vec::new();
            let mut drafts = HashMap::new();
            let body = match &message.rich {
                Rich::PlainPart(plain) => plain,
                _ => &message.body,
            };
            for (at, block) in body.blocks.iter().enumerate() {
                let (kind, text) = match block {
                    BodyBlock::Text(text) => (None, text.as_str()),
                    BodyBlock::Markdown { doc, agent } => {
                        drafts.insert(at, (doc, *agent));
                        runs.push((at, None, Vec::new()));
                        continue;
                    }
                    BodyBlock::Quote(text) => (Some(fell_render::Fold::Quote), text.as_str()),
                    BodyBlock::Signature(text) => {
                        (Some(fell_render::Fold::Signature), text.as_str())
                    }
                };
                match (runs.last_mut(), kind) {
                    (Some((_, Some(above), texts)), Some(next)) => {
                        *above = joined_fold(*above, next);
                        texts.push(text);
                    }
                    _ => runs.push((at, kind, vec![text])),
                }
            }
            let first_hidden = runs
                .iter()
                .position(|(_, kind, _)| kind.is_some())
                .filter(|_| folds.name_key);
            runs.into_iter()
                .enumerate()
                .map(|(run, (at, kind, texts))| match kind {
                    None => {
                        let text = texts.concat();
                        folds.columns.flow(
                            |columns| columns.prose,
                            |_, _| {
                                div()
                                    .pb(px(8.))
                                    .line_height(px(theme.ui_px * 1.45))
                                    .map(|block| match drafts.get(&at) {
                                        Some((doc, agent)) => block.child(
                                            crate::compose_ui::render_draft_doc(doc, *agent, theme),
                                        ),
                                        None => block.child(crate::render::plain_text(
                                            text.clone(),
                                            palette,
                                        )),
                                    })
                                    .into_any_element()
                            },
                        )
                    }
                    Some(kind) => {
                        let text = texts.join("\n\n");
                        let lines = text.lines().filter(|line| !line.trim().is_empty()).count();
                        let line_height = px(theme.ui_px * 1.4);
                        fold_section(
                            theme,
                            FoldLabel {
                                kind,
                                lines,
                                show_key: Some(run) == first_hidden,
                                rtl: crate::render::is_right_to_left(&text),
                            },
                            folds,
                            (folds.columns.moving_prose(), folds.columns.pane_right),
                            reveal,
                            || {
                                folds.columns.flow(
                                    |columns| columns.prose,
                                    |_, _| {
                                        history_rule(
                                            div(),
                                            crate::render::is_right_to_left(&text),
                                            crate::theme::strong_border(theme),
                                        )
                                        .text_color(rgb(theme.dimmed))
                                        .line_height(line_height)
                                        .child(crate::render::plain_text(text.clone(), quiet))
                                        .into_any_element()
                                    },
                                )
                            },
                            cx,
                        )
                    }
                })
                .collect()
        }
    };
    div().opacity(alpha).children(blocks).into_any_element()
}

/// A prepared HTML body, one section at a time: what the sender wrote is
/// painted; quoted history and the signature fold to one line until `q`.
fn html_sections(
    theme: &Theme,
    body: &HtmlBody,
    folds: FoldView,
    reveal: Option<f32>,
    cx: &mut Context<Inbox>,
) -> Vec<gpui::AnyElement> {
    let palette = mail_body::palette(theme);
    let quiet = mail_body::history_palette(theme);
    let line = px(theme.ui_px * 1.45);
    let folded = &body.folded;
    let columns = folds.columns;
    let first_fold = folded
        .sections
        .iter()
        .position(|section| section.fold.is_some())
        .filter(|_| folds.name_key);
    folded
        .sections
        .iter()
        .enumerate()
        .map(|(at, section)| {
            let rtl = body.rtl.get(at).copied().unwrap_or(false);
            let blocks = folded.blocks(section);
            let layout = columns.blocks(folds.fill);
            match section.fold {
                // A quote the sender answers under it: open, under the
                // history rule, in the dimmed colour.
                // A quote the sender answers sits a step apart from the
                // answers around it, closer than two paragraphs.
                None if section.quoted => div()
                    .pt(px(8.))
                    .pb(px(16.))
                    .child(history_section(
                        blocks,
                        None,
                        &quiet,
                        &body.gallery,
                        (theme, rtl, layout),
                    ))
                    .into_any_element(),
                // A short signature shown in place ("Sent from my iPhone")
                // is the dimmed colour, as history is.
                None => div()
                    .pb(px(8.))
                    // A paragraph's gap above it, and the dimmed colour.
                    .when(section.quiet, |quiet| {
                        quiet.pt(px(16.)).text_color(rgb(theme.dimmed))
                    })
                    .child(
                        crate::render::blocks_column_spaced(
                            blocks,
                            if section.quiet { &quiet } else { &palette },
                            false,
                            &body.gallery,
                            layout,
                            crate::render::BLOCK_GAP,
                        )
                        .line_height(line),
                    )
                    .into_any_element(),
                Some(kind) => {
                    let history = mail_body::history_palette(theme);
                    fold_section(
                        theme,
                        FoldLabel {
                            kind,
                            lines: body.fold_lines.get(at).copied().unwrap_or(0),
                            show_key: Some(at) == first_fold,
                            rtl,
                        },
                        folds,
                        (columns.moving_prose(), columns.pane_right),
                        reveal,
                        || {
                            history_section(
                                blocks,
                                Some(kind),
                                &history,
                                &body.gallery,
                                (theme, rtl, layout),
                            )
                        },
                        cx,
                    )
                }
            }
        })
        .collect()
}

fn history_section(
    blocks: &[fell_render::Block],
    kind: Option<fell_render::Fold>,
    palette: &crate::render::Palette,
    gallery: &crate::render::ImageGallery,
    (theme, rtl, layout): (&Theme, bool, crate::render::BlockColumns),
) -> gpui::AnyElement {
    let (rule, layout) = layout.quote(
        div(),
        HISTORY_INDENT_PX,
        2.0,
        crate::theme::strong_border(theme),
        rtl,
    );
    rule.text_color(rgb(theme.dimmed))
        .child(history_blocks(
            blocks,
            kind,
            palette,
            gallery,
            (theme, rtl, layout),
        ))
        .into_any_element()
}

/// Space between a history rule and the text it marks.
const HISTORY_INDENT_PX: f32 = 12.0;

/// The 2 px rule history hangs off, with its 10 px indent: on the left, or
/// on the right of text written right to left.
fn history_rule(block: gpui::Div, rtl: bool, color: u32) -> gpui::Div {
    let block = block.border_color(rgb(color));
    if rtl {
        block.pr(px(10.)).border_r(px(2.))
    } else {
        block.pl(px(10.)).border_l(px(2.))
    }
}

/// History under its rule. A quote's own rule is the section's, so it is
/// not drawn again, except where a signature shares the fold: there the
/// quote keeps a rule of its own inside, so the sender's signature and the
/// mail it answers read as two things. History reads as a block: lines
/// closer than the sender's own paragraphs.
fn history_blocks(
    blocks: &[fell_render::Block],
    kind: Option<fell_render::Fold>,
    history: &crate::render::Palette,
    gallery: &crate::render::ImageGallery,
    (theme, rtl, layout): (&Theme, bool, crate::render::BlockColumns),
) -> gpui::AnyElement {
    use fell_render::{Block, Fold};
    let nested = matches!(
        kind,
        Some(Fold::SignatureAndQuote | Fold::QuoteAndSignature)
    );
    let line_height = px(theme.ui_px * 1.4);
    let column = |run: &[Block], layout| {
        crate::render::blocks_column_spaced(run, history, false, gallery, layout, 8.)
            .line_height(line_height)
    };
    div()
        .flex()
        .flex_col()
        .gap(px(8.))
        .children(blocks.iter().map(|block| match block {
            Block::Quote { blocks } if nested => {
                let (rule, nested_layout) = layout.quote(
                    div(),
                    HISTORY_INDENT_PX,
                    2.0,
                    crate::theme::strong_border(theme),
                    rtl,
                );
                rule.child(column(blocks, nested_layout)).into_any_element()
            }
            Block::Quote { blocks } => column(blocks, layout).into_any_element(),
            other => column(std::slice::from_ref(other), layout).into_any_element(),
        }))
        .into_any_element()
}

/// The fold a run of two kinds of history makes.
fn joined_fold(above: fell_render::Fold, next: fell_render::Fold) -> fell_render::Fold {
    use fell_render::Fold;
    match (above, next) {
        (Fold::Signature, Fold::Signature) => Fold::Signature,
        (Fold::Quote, Fold::Quote) => Fold::Quote,
        (Fold::Signature | Fold::SignatureAndQuote, _) => Fold::SignatureAndQuote,
        (Fold::Quote | Fold::QuoteAndSignature, _) => Fold::QuoteAndSignature,
    }
}

/// What a folded line says.
struct FoldLabel {
    kind: fell_render::Fold,
    lines: usize,
    show_key: bool,
    /// The history is written right to left: the rule is on the right and
    /// the line is set flush right, as the text it stands for.
    rtl: bool,
}

impl FoldLabel {
    /// `··· Quoted text · 12 lines`, and ` · q` on the first fold of the
    /// reader's message.
    fn text(&self) -> String {
        let name = match self.kind {
            fell_render::Fold::Quote => "Quoted text",
            fell_render::Fold::Signature => "Signature",
            fell_render::Fold::SignatureAndQuote => "Signature and quoted text",
            fell_render::Fold::QuoteAndSignature => "Quoted text and signature",
        };
        let mut text = format!("\u{b7}\u{b7}\u{b7} {name}");
        match self.lines {
            0 => {}
            1 => text.push_str(" \u{b7} 1 line"),
            lines => text.push_str(&format!(" \u{b7} {lines} lines")),
        }
        if self.show_key {
            text.push_str(" \u{b7} q");
        }
        text
    }
}

/// A fold's summary and details share an edge. The disclosure keeps both
/// at their natural heights while it moves the blocks below.
fn fold_section(
    theme: &Theme,
    label: FoldLabel,
    folds: FoldView,
    (summary, width): (ReaderColumn, f32),
    reveal: Option<f32>,
    opened: impl FnOnce() -> gpui::AnyElement,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    disclosure(
        width,
        folds.open,
        reveal,
        || {
            summary
                .place(folded_line(theme, &label, folds, cx))
                .into_any_element()
        },
        || {
            div()
                .min_h(px(FOLD_ROW_PX))
                .pb(px(8.))
                .child(opened())
                .into_any_element()
        },
    )
}

/// The one line a folded quote or signature shows. A click opens it, as
/// `q` does. It hangs off the same rule as the opened history, and the rule
/// and text take the accent under the pointer, or when the message is the
/// one `q` acts on.
fn folded_line(
    theme: &Theme,
    label: &FoldLabel,
    folds: FoldView,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let index = folds.index;
    let rule = if label.show_key {
        motion::mix(theme.border, theme.accent, folds.focused)
    } else {
        theme.border
    };
    let accent = theme.accent;
    history_rule(div(), label.rtl, rule)
        .h(px(FOLD_ROW_PX - FOLD_GAP_PX))
        .mb(px(FOLD_GAP_PX))
        .flex()
        .items_center()
        .truncate()
        .when(label.rtl, |line| {
            line.max_w(px(READING_MEASURE_PX)).justify_end()
        })
        .text_color(rgb(theme.dimmed))
        .child(label.text())
        .when(folds.live, |line| {
            line.cursor_pointer()
                .hover(move |style| style.text_color(rgb(accent)).border_color(rgb(accent)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |inbox, _, window, cx| {
                        if !inbox.search_conversation_live() {
                            return;
                        }
                        window.focus(&inbox.focus);
                        inbox.cancel_pending_keys(cx);
                        inbox.focus_message(index);
                        inbox.session.toggle_quotes(index);
                        cx.notify();
                    }),
                )
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
    reveal: Option<f32>,
    index: usize,
    width: f32,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let toggle = cx.listener(move |inbox, _, window, cx| {
        if !inbox.search_conversation_live() {
            return;
        }
        window.focus(&inbox.focus);
        inbox.cancel_pending_keys(cx);
        inbox.focus_message(index);
        inbox.session.toggle_recipients(index);
        cx.notify();
    });
    let size = px(theme.ui_px - 1.0);
    let details = || {
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(line.groups.iter().map(|group| {
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
                            // One dim note ends the row. The line closes the
                            // way it opened, so it names no key.
                            .children({
                                let note = group.only_you.then_some("only you");
                                note.map(|note| {
                                    div()
                                        .whitespace_nowrap()
                                        .text_color(rgb(theme.dimmed))
                                        .child(note)
                                })
                            })
                    })
            }))
            .into_any_element()
    };
    let summary = || {
        // Whole people only, measured like the thread list, so the count
        // always matches the names on screen; the Bcc and key always show.
        // Only a single name too long for the pane is cut, with an ellipsis.
        // The text takes the box's colour, so hover reaches it.
        let style = conversation_line_style(window, theme, size);
        let measure = |text: &str| line_px_with_style(window, &style, text, FontWeight::NORMAL);
        let (names, tail, _) =
            present::fit_recipients(line, |names, tail| measure(names) + measure(tail) <= width);
        let names = clip_line(
            window,
            &style,
            &names,
            (width - measure(&tail)).max(40.0),
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
            .into_any_element()
    };
    div()
        .max_w(px(680.))
        .pb(px(10.))
        .text_size(size)
        .child(
            div()
                .when(live.0, |body| {
                    body.cursor_pointer()
                        .on_mouse_down(MouseButton::Left, toggle)
                })
                .child(disclosure(width, open, reveal, summary, details)),
        )
        .into_any_element()
}

/// Mode, sync state and one notice. At rest there are no key hints
/// (docs/ux.md, Quiet): the right side shows only what the reader is
/// typing, a count or a chord's prefix, while which-key lists the rest.
fn status_bar(
    theme: &Theme,
    session: &Session,
    status: StatusMotion,
    reply_check: Option<ReplyCheck>,
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
    let (sync_label, sync_color) = match reply_check {
        Some(ReplyCheck::Waiting) => ("Reply-To unverified".into(), theme.dimmed),
        Some(ReplyCheck::Checking) => ("Checking Reply-To…".into(), theme.dimmed),
        Some(ReplyCheck::Unavailable) => {
            let key = keymap::binding_text(&session.keymap, Action::SyncNow);
            let label = if width < present::NARROW_WIDTH {
                format!("Retry Reply-To · {key}")
            } else {
                format!("Reply-To unavailable · {key} to retry")
            };
            (label, theme.error)
        }
        Some(ReplyCheck::Verified) => (
            format!(
                "Reply-To verified · {} to reply",
                keymap::binding_text(&session.keymap, Action::Reply)
            ),
            theme.accent,
        ),
        None => (
            session.sync.status(),
            if session.sync.is_error() {
                theme.error
            } else {
                theme.dimmed
            },
        ),
    };
    // What the bar spends before the notice, measured as drawn: the
    // padding, the mode in mono, sync, the dots, the gaps between the
    // children. No typed keys show beside a notice, so no air after it.
    let mut used = 28.0 + 4.0 * 12.0;
    used += bar_px(window, session.mode_label(), size, mono)
        + bar_px(window, &sync_label, size, ui)
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
            .text_color(rgb(if toast.alert {
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
        .child(div().text_color(rgb(sync_color)).child(sync_label))
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
                .child(div().flex_1().child(item.label))
                .child(
                    div()
                        .w(px(12.))
                        .flex_none()
                        .text_right()
                        .text_color(rgb(theme.dimmed))
                        .children(item.is_group.then_some("›")),
                )
        }))
}

const PALETTE_PX: f32 = 560.0;
const PICKER_PX: f32 = 420.0;
const PALETTE_MAX_PX: f32 = 420.0;
const PICKER_MAX_PX: f32 = 380.0;
const PALETTE_ROW_PX: f32 = 36.0;
const PALETTE_ROW_MARGIN: f32 = 4.0;
const PALETTE_ROW_PAD: f32 = 10.0;
const ABOUT_ROW_PAD_Y: f32 = 7.0;
const ABOUT_ROW_GAP: f32 = 2.0;
const PICKER_ROW_PX: f32 = 34.0;
const PICKER_GROUP_GAP: f32 = 8.0;
/// Where the palette, folder picker and help card sit below the window's top.
const CARD_TOP_PX: f32 = 72.0;

/// How tall the open palette or folder picker should be to hold its items
/// (its border, the 44 px query line and 4 px above and below the rows),
/// up to its maximum and the window. `None` for any other overlay.
fn card_fit(session: &Session, theme: &Theme, window: &Window) -> Option<f32> {
    let (rows_px, max) = match session.overlay {
        Overlay::Palette => {
            let rows_px = session.palette().into_iter().map(|item| match item.action {
                Action::About => about_palette_height(theme, window),
                _ => PALETTE_ROW_PX,
            });
            (rows_px.sum(), PALETTE_MAX_PX)
        }
        Overlay::Folders => {
            let choices = session.folder_choices();
            let dividers = choices
                .windows(2)
                .filter(|pair| separates_folder_views(session, &pair[0], &pair[1]))
                .count();
            (
                choices.len() as f32 * PICKER_ROW_PX + dividers as f32 * (PICKER_GROUP_GAP + 1.),
                PICKER_MAX_PX,
            )
        }
        _ => return None,
    };
    Some(present::card_height(
        rows_px,
        max,
        f32::from(window.viewport_size().height) - CARD_TOP_PX - 16.0,
    ))
}

/// The build stamp wraps at the same width and line height used in the row.
fn about_palette_height(theme: &Theme, window: &Window) -> f32 {
    let mono_px = theme.mono_px.max(12.0);
    let mut style = conversation_line_style(window, theme, px(mono_px));
    if let Some(family) = &theme.mono_family {
        style.font_family = family.clone().into();
    }
    let width = f32::from(window.viewport_size().width).min(PALETTE_PX);
    let text_width = (width - 2.0 - 2.0 * (PALETTE_ROW_MARGIN + PALETTE_ROW_PAD)).max(1.0);
    let stamp = fell_version::VERSION;
    let lines = window.text_system().shape_text(
        stamp.into(),
        px(mono_px),
        &[style.to_run(stamp.len())],
        Some(px(text_width)),
        None,
    );
    let stamp_height = lines
        .ok()
        .and_then(|lines| {
            lines
                .first()
                .map(|line| f32::from(line.size(px(mono_px + 4.0)).height))
        })
        .unwrap_or(mono_px + 4.0);
    theme.ui_px + 4.0 + stamp_height + 2.0 * ABOUT_ROW_PAD_Y + ABOUT_ROW_GAP
}
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
    help: Option<(f32, gpui::Div)>,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let top = help.as_ref().map_or(CARD_TOP_PX, |(top, _)| *top);
    div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .right(px(0.))
        .bottom(px(0.))
        .flex()
        .justify_center()
        .pt(px((top - drop).max(0.)))
        .when(session.overlay == Overlay::About, |layer| {
            layer.items_start()
        })
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
            Overlay::Help => help
                .map_or_else(div, |(_, content)| content)
                .opacity(opacity)
                .into_any_element(),
            Overlay::About => about(theme, cx).opacity(opacity).into_any_element(),
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
        .max_w_full()
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
                    let is_about = item.action == Action::About;
                    let detail = if is_about {
                        fell_version::VERSION
                    } else {
                        item.binding.as_str()
                    };
                    let choice = item.clone();
                    div()
                        .id(gpui::ElementId::named_usize("palette", index))
                        .when(!is_about, |row| row.h(px(PALETTE_ROW_PX)))
                        .mx(px(PALETTE_ROW_MARGIN))
                        .px(px(PALETTE_ROW_PAD))
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
                                inbox.reveal.cancel_pending();
                                let page = inbox.page_rows();
                                let before = inbox.before_step();
                                let step = inbox.session.confirm_palette(choice.clone(), page);
                                inbox.apply_step(step, before, cx);
                            }),
                        )
                        .when(is_about, |row| {
                            row.flex_col()
                                .items_start()
                                .py(px(ABOUT_ROW_PAD_Y))
                                .gap(px(ABOUT_ROW_GAP))
                        })
                        .child(
                            div()
                                .when(!is_about, |label| label.flex_1().min_w_0().truncate())
                                .when(is_about, |label| label.line_height(px(theme.ui_px + 4.0)))
                                .child(item.label.clone()),
                        )
                        .child(
                            div()
                                .font_family_opt(theme.mono_family.clone())
                                .text_size(px(theme.mono_px.max(12.0)))
                                .when(is_about, |stamp| {
                                    stamp
                                        .w_full()
                                        .line_height(px(theme.mono_px.max(12.0) + 4.0))
                                })
                                .text_color(rgb(if index == selected {
                                    theme.on_selection
                                } else {
                                    theme.dimmed
                                }))
                                .child(detail.to_owned()),
                        )
                })),
        )
}

fn about(theme: &Theme, cx: &mut Context<Inbox>) -> gpui::Div {
    div()
        .w(px(420.))
        .mx(px(12.))
        .p(px(16.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .gap(px(8.))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|inbox, _, _, cx| {
                if inbox.session.overlay == Overlay::About {
                    cx.stop_propagation();
                }
            }),
        )
        .child(div().font_weight(FontWeight::SEMIBOLD).child("About Fell"))
        .child(
            div()
                .font_family_opt(theme.mono_family.clone())
                .text_size(px(theme.mono_px.max(12.0)))
                .child(fell_version::VERSION),
        )
}

fn separates_folder_views(session: &Session, first: &str, second: &str) -> bool {
    let view = |folder| {
        matches!(
            session.roles.role_of(folder),
            Some(FolderRole::All | FolderRole::Flagged)
        )
    };
    view(first) != view(second)
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
                    let row = div()
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
                                let before = inbox.before_step();
                                let step = inbox.session.confirm_folder();
                                inbox.apply_step(step, before, cx);
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
                        }));
                    let divider =
                        index > 0 && separates_folder_views(session, &choices[index - 1], &name);
                    div()
                        .flex_none()
                        .when(divider, |group| {
                            group.child(
                                div()
                                    .mx(px(14.))
                                    .h(px(PICKER_GROUP_GAP + 1.))
                                    .flex()
                                    .items_center()
                                    .child(div().w_full().h(px(1.)).bg(rgb(theme.border))),
                            )
                        })
                        .child(row)
                })),
        )
}

/// What `#`, `!`, and a confirmed permanent delete do to the threads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Removal {
    /// To Trash.
    Delete,
    /// To Spam, or from Spam to the inbox.
    Spam,
    /// Out of Trash for good, in Fell only.
    Purge,
}

/// "Moved to Trash · u", or "Moved 3 to Trash · u" for a selection.
fn moved_notice(count: usize, to: &str) -> String {
    match count {
        1 => format!("Moved to {to} · u"),
        n => format!("Moved {n} to {to} · u"),
    }
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
        "left" => Some(KeyId::Left),
        "right" => Some(KeyId::Right),
        "tab" => Some(KeyId::Tab),
        "home" => Some(KeyId::Home),
        "end" => Some(KeyId::End),
        "delete" => Some(KeyId::Delete),
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
                    '3' => '#',
                    '1' => '!',
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
        KeyId::Enter | KeyId::Char('o' | 'r' | 'R' | 'f' | 'a') => pane == Pane::Conversation,
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
        "slash" => Some(KeyId::Char('/')),
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
        LIST_BORDER_PX, MESSAGE_HEADER_GAP_PX, MESSAGE_PAD_PX, READING_MEASURE_PX, ReaderColumns,
        account_header, collapsed_sender_budget, ctrl_letter, expanded_header_widths, key_from,
        message_content_width, moved_in_list, should_follow_visible, unread_bar_dx,
        useful_header_email, write_back_notice,
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
    fn account_header_only_shows_a_distinct_display_name() {
        let email = "reader@example.test";
        assert_eq!(account_header("", email), (email, None));
        assert_eq!(account_header("   ", email), (email, None));
        assert_eq!(account_header(email, email), (email, None));
        assert_eq!(
            account_header(" Reader@example.test ", email),
            (email, None)
        );
        assert_eq!(
            account_header("Demo Reader", email),
            ("Demo Reader", Some(email))
        );
        assert_eq!(account_header("Demo Reader", ""), ("Demo Reader", None));
    }

    #[test]
    fn header_email_keeps_a_readable_address_prefix() {
        let email = "robin@demo.test";
        for stub in ["…", "ro…", "robin…"] {
            assert!(!useful_header_email(email, stub));
        }
        assert!(useful_header_email(email, "robin@…"));
        assert!(useful_header_email(email, email));
        assert!(useful_header_email("r@x.test", "r@x.test"));

        let long = "abcdefghijklmnop@demo.test";
        assert!(!useful_header_email(long, "abcdefghijk…"));
        assert!(useful_header_email(long, "abcdefghijkl…"));

        let unicode = "ééééééééééééé@demo.test";
        assert!(!useful_header_email(unicode, "éééééé…"));
        assert!(useful_header_email("rōbin@demo.test", "rōbin@…"));
    }

    #[test]
    fn collapsed_sender_budget_keeps_room_for_the_preview() {
        assert_eq!(collapsed_sender_budget(379., 11., 11., 44.), 140.);
        assert_eq!(collapsed_sender_budget(185., 18.2, 17.1, 72.), 113.);
        assert_eq!(collapsed_sender_budget(70., 18.2, 17.1, 72.), 52.);
        assert_eq!(collapsed_sender_budget(24., 18.2, 17.1, 72.), 24.);
        assert_eq!(collapsed_sender_budget(0., 18.2, 17.1, 72.), 0.);
    }

    #[test]
    fn an_unfocused_expanded_card_keeps_its_resting_fill() {
        for theme in crate::theme::every_test_theme() {
            let resting = motion::mix(theme.background, theme.surface, 0.4);
            assert_eq!(super::expanded_card_fill(&theme, 0.0), resting);
            assert_eq!(super::expanded_card_fill(&theme, 1.0), theme.surface);
            assert_eq!(
                super::expanded_card_fill(&theme, 0.5),
                motion::mix(theme.background, theme.surface, 0.7)
            );
        }
    }

    #[test]
    fn expanded_sender_truncates_without_losing_the_address_or_date() {
        let fields = [900.0, 135.0, 35.0, 30.0];
        assert_eq!(expanded_header_widths(1_200.0, fields), fields);
        assert_eq!(
            expanded_header_widths(502.0, fields),
            [272.0, 135.0, 35.0, 30.0]
        );

        let width = 150.0;
        let [sender, email, folders, date] =
            expanded_header_widths(width, [300.0, 225.0, 200.0, 55.0]);
        assert_eq!(date, 55.0);
        assert!(sender > 0.0 && email > 0.0 && folders > 0.0);
        assert!(sender + email + folders + date + MESSAGE_HEADER_GAP_PX * 3.0 <= width);

        let [sender, email, folders, date] = expanded_header_widths(
            super::message_content_width(360.0),
            [300.0, 225.0, 0.0, 55.0],
        );
        assert_eq!([sender, email, folders, date], [108.5, 108.5, 0.0, 55.0]);
    }

    #[test]
    fn reading_columns_share_the_header_origin_and_stay_inside_the_window() {
        for (width, split) in [
            (560.0, 0.0),
            (760.0, 0.0),
            (880.0, 310.0),
            (1000.0, 387.0),
            (1500.0, 337.0),
            (1500.0, 487.0),
            (1920.0, 724.0),
            (1920.0, 429.4),
            (2400.0, 520.0),
        ] {
            for reading in [true, false] {
                let end_progress = if reading { 1.0 } else { 0.0 };
                let end = ReaderColumns::new(width, split, reading, end_progress, 2.0);
                let end_pane = ReaderColumns::pane_left(split, end_progress, 2.0);
                let mut previous: Option<[f32; 2]> = None;
                for step in 0..=100 {
                    let progress = if reading { step } else { 100 - step } as f32 / 100.0;
                    let columns = ReaderColumns::new(width, split, reading, progress, 2.0);
                    let pane_left = ReaderColumns::pane_left(split, progress, 2.0);
                    let dates = columns.date_column();
                    let [header, prose, card, wide] =
                        [columns.header, columns.prose, columns.card, columns.wide];
                    assert_eq!(header.inset, prose.inset);
                    assert_eq!(dates.inset, header.inset + MESSAGE_PAD_PX);
                    let card_right = card.inset + card.width + 2.0 * MESSAGE_PAD_PX;
                    assert_eq!(dates.inset + dates.width, card_right - MESSAGE_PAD_PX);
                    assert_eq!(card.width, card.width.round());
                    assert!(
                        pane_left + card.inset + card.width <= message_content_width(width) + 0.01
                    );
                    for (column, destination) in
                        [(header, end.header), (prose, end.prose), (wide, end.wide)]
                    {
                        assert_eq!(column.width, destination.width);
                        if step == 100 {
                            assert!(
                                pane_left + column.inset + column.width
                                    <= message_content_width(width) + 0.51
                            );
                        }
                    }
                    let origins = [pane_left + header.inset, pane_left + wide.inset];
                    if let Some(previous) = previous {
                        for (index, end_inset) in
                            [end.header.inset, end.wide.inset].into_iter().enumerate()
                        {
                            let end = end_pane + end_inset;
                            assert!((origins[index] - end).abs() <= (previous[index] - end).abs());
                        }
                    }
                    previous = Some(origins);
                }
            }
            let reading = ReaderColumns::new(width, split, true, 1.0, 2.0);
            assert_eq!(
                reading.prose.width,
                message_content_width(width).min(READING_MEASURE_PX)
            );
            assert_eq!(reading.card.width, reading.prose.width);
            assert!(reading.wide.width >= reading.prose.width);
        }
    }

    /// After each key, whether a box left wide relaxes: only a key that
    /// moves the list's cursor or comes back to the list.
    #[test]
    fn only_moving_in_the_list_relaxes_a_wide_row() {
        use crate::keymap::Keymap;
        use crate::model::{Session, SyncKind};
        use crate::present::{NameSpan, ThreadRow};
        let row = |id: &str| ThreadRow {
            id: id.into(),
            spans: vec![NameSpan {
                text: id.into(),
                emphasis: true,
            }],
            extra: 0,
            message_count: 1,
            starred: false,
            has_attachment: false,
            date_unix: 0,
            subject: id.into(),
            snippet: "snippet".into(),
            labels: vec![],
            warn: false,
            unread: true,
            draft: None,
        };
        let mut session = Session::new("A", "a@x", SyncKind::FixtureOffline, Keymap::default());
        session.set_threads(vec![row("a"), row("b"), row("c")]);
        let mut press = |key: KeyId| {
            let before = (session.cursor_id.clone(), session.pane);
            session.on_key(key, 10);
            moved_in_list(
                (before.0.as_deref(), before.1),
                (session.cursor_id.as_deref(), session.pane),
            )
        };
        // Opening, `U` and `s` leave the row where the reader is looking.
        assert!(!press(KeyId::Enter));
        assert!(!press(KeyId::Char('U')));
        assert!(!press(KeyId::Char('s')));
        // `j` in the conversation moves between messages, not threads.
        assert!(!press(KeyId::Char('j')));
        // Back to the list, then down it.
        assert!(press(KeyId::Escape));
        assert!(press(KeyId::Char('j')));
        assert!(press(KeyId::Char('k')));
        // Any other move of the list cursor: `G`, `gg`, a page.
        assert!(press(KeyId::Char('G')));
        assert!(!press(KeyId::Char('g')));
        assert!(press(KeyId::Char('g')));
        // Open, then back with `h`.
        assert!(!press(KeyId::Enter));
        assert!(press(KeyId::Char('h')));
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
        assert_eq!(key_from(&stroke("3", "#", true)), Some(KeyId::Char('#')));
        assert_eq!(key_from(&stroke("1", "!", true)), Some(KeyId::Char('!')));
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
        use fell_core::{DropReason, DroppedWrite, OutboxKind};

        let session = Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        let notice = |reason, count| {
            let write = DroppedWrite {
                reason,
                kind: OutboxKind::Move,
                source_folder: "INBOX".into(),
                target_folder: "Archive".into(),
            };
            write_back_notice(&session, &vec![write; count], false)
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
        assert_eq!(
            notice(DropReason::NoMove, 1),
            "This server can't move mail; archive stays in Fell"
        );
        assert_eq!(
            notice(DropReason::Abandoned, 1),
            "1 message left half moved in Inbox on the server"
        );
        for reason in [
            DropReason::FolderReset,
            DropReason::Refused,
            DropReason::NotFound,
            DropReason::NoMove,
            DropReason::Abandoned,
        ] {
            let text = notice(reason, 2);
            assert!(!text.contains('\n') && text.chars().count() <= 64, "{text}");
        }
    }

    #[test]
    fn the_status_notice_covers_both_move_and_flag_failures() {
        use crate::keymap::Keymap;
        use crate::model::{Session, SyncKind};
        use fell_core::{DropReason, DroppedWrite, OutboxKind};

        let session = Session::new(
            "Reader",
            "me@example.test",
            SyncKind::FixtureOffline,
            Keymap::default(),
        );
        let move_failure = DroppedWrite {
            reason: DropReason::NoMove,
            kind: OutboxKind::Move,
            source_folder: "INBOX".into(),
            target_folder: "Archive".into(),
        };
        let flag_failure = DroppedWrite {
            reason: DropReason::FolderReset,
            kind: OutboxKind::Seen,
            source_folder: "INBOX".into(),
            target_folder: String::new(),
        };
        let mut dropped = [move_failure, flag_failure];
        assert_eq!(
            write_back_notice(&session, &dropped, false),
            "Server can't move mail. Read/star sync failed"
        );
        dropped.reverse();
        assert_eq!(
            write_back_notice(&session, &dropped, false),
            "Server can't move mail. Read/star sync failed"
        );
        assert_eq!(
            write_back_notice(&session, &[], true),
            "Read/star sync delayed. Retrying"
        );
    }
}
