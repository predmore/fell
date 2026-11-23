//! Two-pane inbox. The window reads the local store and never talks to IMAP.

mod agent_notes;
mod agents;
mod attachment_actions;
mod attachment_chips;
mod diagnostics;
pub(crate) mod disclosure;
mod help;
mod notification_actions;
mod search;

use std::cell::{Cell, OnceCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use fell_core::{
    AccountConfig, AccountId, CidRepairBatch, CidRepairProgress, DropReason, DroppedWrite,
    FlagChange, FolderRole, FolderRoles, FolderSpan, ListKey, MessageId, OutboxKind, ThreadId,
};
use futures::StreamExt;
use gpui::{
    App, Application, AvailableSpace, Bounds, Context, FocusHandle, FontWeight, HighlightStyle,
    InteractiveElement, IntoElement, KeyDownEvent, Keystroke, MouseButton, MouseMoveEvent,
    MouseUpEvent, ParentElement, Pixels, Render, ScrollHandle, ScrollStrategy, SharedString,
    StatefulInteractiveElement, Styled, StyledText, TextStyle, TitlebarOptions,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions, div, point, prelude::*, px, rgb,
    size, uniform_list,
};
use unicode_segmentation::UnicodeSegmentation;

use fell_core::actions::LabelUndoTarget;
use fell_core::compose::{DraftSummary, ReplyKind};

use crate::attachments::{self, Download, Fetcher, Request as AttachmentRequest};
use crate::compose_ui::ComposeView;
use crate::draft_host::DraftHome;
use crate::frame_log::{FrameLog, SharedFrameLog};
use crate::inbox_motion::{Disclosures, InboxMotion, PaneEntry, StatusMotion};
use crate::keymap::{self, Action, KeyId};
use crate::mail_body::{
    self, BodyAsks, BodyCache, BodyJob, BodyLoad, BodyLoads, HtmlBody, PreparedBody,
};
use crate::mailbox;
use crate::model::{
    Banner, DRAFT_NOT_THREAD, DownloadStatus, Fetch, FlagEdit, FolderIntent, LabelEdit, Overlay,
    PickerChoice, QueryFault, ReadDwell, Session, Step, SyncKind, Toast, grouped, row_key,
};
use crate::motion::{self, Glide, LayerSwap, ListShift, Motion, RowMotion, SwapFrame, Transition};
use crate::onboarding::Onboarding;
use crate::present::{self, ConvMessage, Pane, Rich, ThreadRow};
use crate::render::READING_MEASURE_PX;
use crate::reply_check::{ReplyCheck, ReplyChecks};
use crate::row_widths::{self, RowFrame, RowWidths};
use crate::scroll_anchor::{
    LandingGlide, LandingHold, LayoutProbe, LayoutSlot, MessageProbes, ReaderLanding, Reveal,
    RevealPhase, ScrollAnchor, clamped_offset,
};
use crate::store_actor::StoreClient;
use crate::sync_worker::{self, SyncEvent, SyncHandle};
use crate::text_cut;
use crate::text_render::BodyBlock;
use crate::theme::{self, Theme};
use crate::ui_trace::UiTrace;
use crate::worker::Worker;

use self::agent_notes::NoteVisit;
use self::disclosure::{GrowingMessage, disclosure};

pub fn run(demo: bool, trace_start: Option<Instant>) {
    fell_core::diagnostic!(INFO, "Opening inbox");
    Application::new().run(move |cx: &mut App| {
        let trace = trace_start.and_then(|start| match UiTrace::open(start) {
            Ok(trace) => Some(trace),
            Err(error) => {
                fell_core::diagnostic!(
                    WARN,
                    "Could not open UI trace",
                    error = &error as &dyn std::error::Error
                );
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
        let startup = mailbox::startup();
        let (width, height) = window_size();
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("Fell".into()),
                ..Default::default()
            }),
            app_id: Some("dev.predmore.fell".into()),
            focus: true,
            show: true,
            ..Default::default()
        };
        let view = if startup.account_missing && !demo {
            cx.open_window(options, |window, cx| {
                if let Some(trace) = &trace {
                    trace.window_created(window);
                }
                cx.new(|cx| crate::onboarding::Onboarding::new(window, cx))
            })
            .map(gpui::AnyWindowHandle::from)
        } else {
            crate::draft_host::host_agent_drafts(cx);
            cx.open_window(options, |window, cx| {
                if let Some(trace) = &trace {
                    trace.window_created(window);
                }
                cx.new(|cx| Inbox::focused(startup, demo, window, cx))
            })
            .map(gpui::AnyWindowHandle::from)
        };
        match view {
            Ok(view) => {
                cx.on_window_closed(move |cx| {
                    if cx.windows().contains(&view) {
                        if let Some(inbox) = cx.try_global::<InboxWindow>().copied() {
                            inbox.update(cx, |inbox, cx| {
                                if inbox.motion.has_pending_messages() {
                                    cx.notify();
                                }
                            });
                        }
                    } else if let Some(trace) = &trace {
                        trace.finish();
                    }
                })
                .detach();
            }
            Err(_) => fell_core::diagnostic!(ERROR, "Could not open inbox window"),
        }
        cx.activate(true);
    });
}

#[derive(Clone, Copy)]
struct InboxWindow(gpui::AnyWindowHandle);

impl gpui::Global for InboxWindow {}

impl InboxWindow {
    fn update(self, cx: &mut App, update: impl FnOnce(&mut Inbox, &mut Context<Inbox>)) {
        let _ = self.0.update(cx, |root, _, cx| {
            if let Ok(inbox) = root.downcast::<Inbox>() {
                inbox.update(cx, update);
            }
        });
    }
}

/// Put the committed copy into the inbox before the compose window closes.
pub(crate) fn present_sent_copy(
    cx: &mut App,
    thread: ThreadId,
    message: fell_core::Message,
    rich: Rich,
) {
    let Some(inbox) = cx.try_global::<InboxWindow>().copied() else {
        return;
    };
    inbox.update(cx, |inbox, cx| {
        if message.account_id != inbox.account_id {
            return;
        }
        inbox.show_sent_copy(thread, message, rich);
        inbox.request_threads(
            ReadIntent {
                refresh_open: true,
                ..ReadIntent::default()
            },
            cx,
        );
        cx.notify();
    });
}

fn compose_open(cx: &App) -> bool {
    cx.windows()
        .iter()
        .any(|handle| handle.downcast::<ComposeView>().is_some())
}

pub(crate) fn compose_draft_shown(id: &str, cx: &mut App) {
    if let Some(inbox) = cx.try_global::<InboxWindow>().copied() {
        inbox.update(cx, |inbox, cx| {
            if inbox
                .opening_draft
                .as_ref()
                .and_then(|open| open.ready.as_deref())
                == Some(id)
            {
                inbox.finish_server_draft_open(Some(id), cx);
            }
        });
    }
}

pub(crate) fn compose_draft_closing(id: &str, cx: &mut App) {
    if let Some(inbox) = cx.try_global::<InboxWindow>().copied() {
        inbox.update(cx, |inbox, cx| {
            if !inbox.session.in_drafts() {
                return;
            }
            if inbox
                .opening_draft
                .as_ref()
                .and_then(|open| open.ready.as_deref())
                == Some(id)
            {
                inbox.finish_server_draft_open(Some(id), cx);
            }
            inbox.refresh_closing_draft(id, cx);
        });
    }
}

pub(crate) fn open_compose_diagnostic_log(cx: &mut App) {
    if let Some(inbox) = cx.try_global::<InboxWindow>().copied() {
        inbox.update(cx, |inbox, cx| {
            inbox.open_diagnostic(fell_core::diagnostics::DiagnosticFile::Log, cx);
        });
    }
}

pub(crate) fn compose_draft_window_failed(id: &str, cx: &mut App) {
    if let Some(inbox) = cx.try_global::<InboxWindow>().copied() {
        inbox.update(cx, |inbox, cx| {
            if inbox
                .opening_draft
                .as_ref()
                .and_then(|open| open.ready.as_deref())
                == Some(id)
            {
                inbox.finish_server_draft_open(None, cx);
                inbox.session.show_toast("Couldn't open the draft.", true);
            }
        });
    }
}

pub(crate) fn wake_sent_copy_sync(cx: &mut App) {
    if let Some(inbox) = cx.try_global::<InboxWindow>().copied() {
        inbox.update(cx, |inbox, _| {
            // A failed local insert can still have queued the server copy.
            if let Some(sync) = &inbox.sync {
                sync.write_back_now();
            }
        });
    }
}

pub(crate) fn finish_onboarding(
    setup: gpui::Entity<Onboarding>,
    window: &mut Window,
    cx: &mut App,
) {
    let startup = mailbox::startup();
    crate::draft_host::host_agent_drafts(cx);
    window.replace_root(cx, |window, cx| {
        let mut inbox = Inbox::focused(startup, false, window, cx);
        inbox.first_preview = FirstPreview::Handoff;
        inbox.waiting_for_first_rows = true;
        inbox.waiting_for_inbox_total = true;
        let now = inbox.motion.clock.now();
        inbox
            .motion
            .observe_header(&inbox.list_folder, None, None, now);
        inbox.setup_exit = LayerSwap::start(inbox.motion.motion, now, motion::QUICK, motion::FADE)
            .map(|swap| (setup, swap));
        inbox
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
    /// What the notification opens once its row lands.
    notification_pane: Option<Pane>,
    /// Rows the list asked for beyond the ones it holds.
    fetch: Fetch,
}

impl ReadIntent {
    fn merge(mut self, later: ReadIntent) -> ReadIntent {
        self.switch |= later.switch;
        self.refresh_open |= later.refresh_open;
        self.changed |= later.changed;
        if later.select.is_some() {
            self.select = later.select;
            self.notification_pane = later.notification_pane;
        } else if later.switch && self.notification_pane.is_some() {
            self.select = None;
            self.notification_pane = None;
        }
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

struct OpeningServerDraft {
    thread: ThreadId,
    started: Instant,
    ready: Option<String>,
}

const DRAFT_OPEN_FEEDBACK: Duration = Duration::from_millis(300);

/// One folder read: a window of rows, its edges, the unread count, and the
/// folder names for the picker.
struct ListRead {
    rows: Vec<ThreadRow>,
    more_before: bool,
    more_after: bool,
    unread: usize,
    names: Vec<String>,
    labels: Option<Vec<String>>,
    /// Where the selected thread sits now, when it is still in the folder
    /// but not among `rows`: new mail moved it outside the window.
    followed: Option<ListKey>,
    /// In Drafts, the person's compose drafts, shown above the synced rows
    /// when the window reaches the top of the folder.
    drafts: Vec<DraftSummary>,
    roles: FolderRoles,
}

type ThreadsResult = Result<ListRead, String>;

async fn finish_mailbox_change<T>(
    reply: futures::channel::oneshot::Receiver<fell_core::Result<T>>,
    done: impl FnOnce(fell_core::Result<T>),
) {
    let result = reply.await.unwrap_or_else(|_| {
        Err(fell_core::Error::Action(
            "Could not update this mailbox".into(),
        ))
    });
    done(result);
}

/// Return the visible page's edges, or request a restart for an empty label
/// window. A page hidden by pending removals waits for their follow-up read.
fn prepare_list_rows(
    rows: &mut Vec<ThreadRow>,
    unread: &mut usize,
    pending_removals: Option<&HashSet<String>>,
    edges: (bool, bool),
    label: bool,
) -> Option<(bool, bool)> {
    let had_rows = !rows.is_empty();
    if label && !had_rows && (edges.0 || edges.1) {
        return None;
    }
    if let Some(pending) = pending_removals {
        rows.retain(|row| {
            let hidden = pending.contains(&row.id);
            if hidden && row.unread {
                *unread = unread.saturating_sub(1);
            }
            !hidden
        });
    }
    if label && had_rows && rows.is_empty() {
        // A waiting cursor motion would otherwise request a page again.
        return Some((false, false));
    }
    Some(edges)
}

#[derive(Default)]
struct PendingLabels {
    next: u64,
    edits: VecDeque<PendingLabel>,
}

struct PendingLabel {
    id: u64,
    edit: LabelEdit,
    /// A read started through this generation could predate the commit.
    through: Option<u64>,
}

impl PendingLabels {
    fn start(&mut self, edit: LabelEdit) -> u64 {
        self.next += 1;
        let id = self.next;
        self.edits.push_back(PendingLabel {
            id,
            edit,
            through: None,
        });
        id
    }

    fn finish(&mut self, id: u64, committed_through: Option<u64>) {
        if let Some(through) = committed_through {
            if let Some(pending) = self.edits.iter_mut().find(|edit| edit.id == id) {
                pending.through = Some(through);
            }
        } else {
            self.edits.retain(|edit| edit.id != id);
        }
    }

    fn land(
        &mut self,
        read: u64,
        rows: &mut [ThreadRow],
        mut labels: Option<&mut Vec<String>>,
        scope: Option<&str>,
    ) {
        // Keep later edits behind any still pending earlier edit, since they
        // can toggle the same chip and completion callbacks can land apart.
        while self
            .edits
            .front()
            .is_some_and(|edit| edit.through.is_some_and(|through| read > through))
        {
            self.edits.pop_front();
        }
        for pending in &self.edits {
            if pending.edit.add || scope != Some(&pending.edit.name) {
                pending.edit.apply(rows);
            }
            if let Some(labels) = labels.as_deref_mut() {
                pending.edit.include_name(labels);
            }
        }
    }
}

#[derive(Default)]
struct PendingRowFlags {
    unread: Option<PendingFlag>,
    starred: Option<PendingFlag>,
}

struct PendingFlag {
    write: u64,
    value: bool,
    /// The last page request that could predate this write's reply.
    through: Option<u64>,
}

impl PendingRowFlags {
    fn slot(&mut self, change: FlagChange) -> &mut Option<PendingFlag> {
        match change {
            FlagChange::Read | FlagChange::Unread => &mut self.unread,
            FlagChange::Star | FlagChange::Unstar => &mut self.starred,
        }
    }

    fn land(&mut self, read: u64) -> bool {
        for flag in [&mut self.unread, &mut self.starred] {
            if flag
                .as_ref()
                .and_then(|flag| flag.through)
                .is_some_and(|through| read > through)
            {
                *flag = None;
            }
        }
        self.unread.is_some() || self.starred.is_some()
    }
}

/// Use one notice so a flag failure and a move failure can both be seen.
fn write_back_notice(
    session: &Session,
    dropped: &[DroppedWrite],
    flag_recovery_delayed: bool,
) -> String {
    let Some(first) = dropped.first() else {
        return "Read/star sync delayed. Retrying".to_string();
    };
    let is_label =
        |write: &DroppedWrite| matches!(write.kind, OutboxKind::Keyword | OutboxKind::BridgeLabel);
    if dropped.iter().any(is_label) {
        let other: Vec<_> = dropped
            .iter()
            .filter(|write| !is_label(write))
            .cloned()
            .collect();
        let labels = "Label changes failed to sync";
        if other.is_empty() && !flag_recovery_delayed {
            return labels.into();
        }
        return format!(
            "{labels}. {}",
            write_back_notice(session, &other, flag_recovery_delayed)
        );
    }
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
        DropReason::UnconfirmedAppend => "Sent · server copy unconfirmed".to_string(),
        DropReason::DraftCleanup => "An older draft copy remains on the server".to_string(),
    }
}

/// Height of one thread row. Rows never change height, so the list can
/// place, scroll to and animate any row from its index.
const ROW_PX: f32 = 68.0;
/// A glide further than this (about four rows) eases on the gentler
/// curve, so one late frame cannot carry it half the way ([`Glide::gentle_beyond`]).
const LONG_GLIDE_PX: f32 = 4.0 * ROW_PX;

const FIRST_PREVIEW_WAIT: Duration = Duration::from_millis(1500);

#[derive(Default, PartialEq, Eq)]
enum FirstPreview {
    #[default]
    Off,
    Handoff,
    Waiting(Instant),
    AllowPending,
}

impl FirstPreview {
    fn finish_handoff(&mut self, now: Instant) -> bool {
        if *self == Self::Handoff {
            *self = Self::Waiting(now + FIRST_PREVIEW_WAIT);
            return true;
        }
        false
    }

    fn waits_for_body(&self) -> bool {
        matches!(self, Self::Handoff | Self::Waiting(_))
    }

    fn allow_pending(&mut self) -> bool {
        if self.waits_for_body() {
            *self = Self::AllowPending;
            return true;
        }
        false
    }

    fn sync_changed(&mut self, sync: &SyncKind) -> bool {
        !matches!(sync, SyncKind::Connecting | SyncKind::Syncing { .. }) && self.allow_pending()
    }

    fn expire(&mut self, now: Instant) -> bool {
        matches!(self, Self::Waiting(deadline) if now >= *deadline) && self.allow_pending()
    }

    fn take(&mut self) -> bool {
        std::mem::take(self) != Self::Off
    }
}

struct Inbox {
    focus: FocusHandle,
    notifications: Option<crate::notifications::Queue>,
    pending_activation: Option<crate::notifications::Activation>,
    notification_toast: Option<(String, bool)>,
    window_active: bool,
    /// The mailbox connection's own thread. The window never calls SQLite.
    store: StoreClient,
    startup_crash: Option<std::path::PathBuf>,
    worker_restart_notice: Option<(Worker, u64)>,
    /// Cursor observed before the first inbox read, consumed after it paints.
    cid_repair: Option<futures::channel::oneshot::Receiver<fell_core::Result<CidRepairProgress>>>,
    account_id: AccountId,
    attachment_fetcher: Fetcher,
    attachment_busy: bool,
    attachment_retry: Option<AttachmentRequest>,
    attachment_download: Option<(String, String, Download)>,
    reveal_attachment: bool,
    forwarding_attachments: bool,
    note_view: agent_notes::NoteView,
    conv_note_slot: bool,
    instant_conversation_scroll: bool,
    note_pin: agent_notes::NotePin,
    note_band: Pixels,
    note_visit: Option<NoteVisit>,
    note_in_flight: Option<i64>,
    /// Note snapshots older than a completed local decision cannot paint.
    note_generation: u64,
    session: Session,
    theme: Theme,
    setup_exit: Option<(gpui::Entity<Onboarding>, LayerSwap)>,
    first_preview: FirstPreview,
    waiting_for_first_rows: bool,
    waiting_for_inbox_total: bool,
    status_labels: StatusLabels,
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    // Keep the extra scroll range on the first preview thread after input,
    // so taking over the reader cannot clamp the viewport and jump.
    preview_padding: bool,
    preview_laid_out: bool,
    preview_card_height: Rc<Cell<gpui::Pixels>>,
    palette_scroll: ScrollHandle,
    folder_scroll: ScrollHandle,
    agents_scroll: ScrollHandle,
    agents_motion: agents::AgentMotion,
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
    pending_bodies: PendingBodies,
    body_landing: Option<(gpui::Pixels, Transition)>,
    /// The scroll handle has bounds for this conversation's current messages.
    conv_layout_current: bool,
    painted_reader: Option<ReaderLayout>,
    sent_arrival_anchor: Option<ScrollAnchor>,
    /// Background sync for the account file. `None` for the demo mailbox.
    sync: Option<SyncHandle>,
    /// The folder the rows on screen came from. `session.folder` changes on
    /// the key press; this changes when that folder's read lands. Archive and
    /// move act on this one, the list the reader sees.
    list_folder: String,
    list_label: Option<String>,
    /// Visible label rows as they looked in the last list prepaint.
    painted_label_rows: Vec<LeavingRow>,
    leaving_rows: Option<Arc<[LeavingRow]>>,
    /// Paint-only reader when the last rows leave a label list.
    leaving_reader: Option<Arc<LeavingReader>>,
    /// One folder read at a time is on the store thread.
    read_in_flight: bool,
    /// The read in flight was only a sync refresh, on the background lane.
    refresh_in_flight: bool,
    /// Bumped by every folder read. A read the user superseded is dropped.
    read_gen: u64,
    /// Asked for while a read or undo was in flight. Issued when it lands.
    read_wanted: Option<ReadIntent>,
    /// An in-flight read can already see rows restored on the store thread.
    /// Wait for the undo reply so those rows arrive with their selection.
    undos_in_flight: usize,
    /// Threads taken out of the list whose archive or move has not landed.
    /// A read that started before the write cannot paint them back.
    pending_removals: std::collections::HashSet<String>,
    /// Flag edits on loaded rows, kept until a page requested after the write lands.
    pending_flags: HashMap<String, PendingRowFlags>,
    pending_labels: PendingLabels,
    label_targets_read: u64,
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
    opening_draft: Option<OpeningServerDraft>,
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
    fn focused(
        startup: mailbox::Startup,
        demo: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let inbox = Self::with_startup(startup, demo, Some(window), cx);
        window.focus(&inbox.focus);
        cx.set_global(InboxWindow(window.window_handle()));
        let mut viewport = (window.viewport_size(), window.scale_factor());
        cx.observe_window_bounds(window, move |_, window, cx| {
            let previous = viewport;
            viewport = (window.viewport_size(), window.scale_factor());
            if viewport != previous {
                // Draw at the new bounds for the next compositor frame.
                window.defer(cx, |window, cx| window.draw(cx).clear());
            }
        })
        .detach();
        cx.observe_window_activation(window, |_, window, cx| {
            if window.is_window_active() {
                cx.notify();
            }
        })
        .detach();
        inbox
    }

    fn with_startup(
        startup: mailbox::Startup,
        demo: bool,
        window: Option<&mut Window>,
        cx: &mut Context<Self>,
    ) -> Self {
        let theme = theme::load();
        let config = startup.config.clone();
        let path = startup.path.clone();
        let draft_home = startup.path.clone().ok().map(DraftHome::real);
        let (notices, store_restarts) = futures::channel::mpsc::unbounded();
        let (store, ready) = StoreClient::spawn(
            {
                let config = config.clone();
                move || mailbox::open_store(path.clone(), config.as_ref(), demo)
            },
            notices,
        );
        cx.set_global(crate::compose_ui::RecipientStore(store.clone()));
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
        session.notification_mode = config.as_ref().map(|config| config.notifications);
        if let Some(error) = startup.error {
            session.record_startup_error(error);
        }
        let reader = cx.entity().downgrade();
        let window_active = window
            .as_ref()
            .is_some_and(|window| window.is_window_active());
        let notifications = config.as_ref().zip(window).map(|(config, window)| {
            let (queue, invoked) = crate::notifications::start(
                window_active,
                config.notifications,
                store.clone(),
                config.account().id,
            );
            Self::watch_notifications(window.window_handle(), invoked, cx);
            cx.observe_window_activation(window, Self::notification_window_activation)
                .detach();
            queue
        });
        let mut inbox = Self {
            focus: cx.focus_handle(),
            notifications,
            pending_activation: None,
            notification_toast: None,
            window_active,
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
            note_view: agent_notes::NoteView::default(),
            conv_note_slot: false,
            instant_conversation_scroll: false,
            note_pin: agent_notes::NotePin::default(),
            note_band: px(0.),
            note_visit: None,
            note_in_flight: None,
            note_generation: 0,
            session,
            setup_exit: None,
            first_preview: FirstPreview::Off,
            waiting_for_first_rows: false,
            waiting_for_inbox_total: false,
            status_labels: StatusLabels::default(),
            store,
            startup_crash: None,
            worker_restart_notice: None,
            cid_repair: None,
            theme,
            list_scroll: UniformListScrollHandle::new(),
            conv_scroll: ScrollHandle::new(),
            preview_padding: false,
            preview_laid_out: false,
            preview_card_height: Rc::default(),
            palette_scroll: ScrollHandle::new(),
            folder_scroll: ScrollHandle::new(),
            agents_scroll: ScrollHandle::new(),
            agents_motion: agents::AgentMotion::default(),
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
            pending_bodies: PendingBodies::default(),
            body_landing: None,
            conv_layout_current: false,
            painted_reader: None,
            sent_arrival_anchor: None,
            sync: None,
            list_folder: FolderRole::Inbox.default_name().into(),
            list_label: None,
            painted_label_rows: Vec::new(),
            leaving_rows: None,
            leaving_reader: None,
            read_in_flight: false,
            refresh_in_flight: false,
            read_gen: 0,
            read_wanted: None,
            undos_in_flight: 0,
            pending_removals: std::collections::HashSet::new(),
            pending_flags: std::collections::HashMap::new(),
            pending_labels: PendingLabels::default(),
            label_targets_read: 0,
            flag_gen: 0,
            row_widths: RowWidths::new(motion),
            conv_gen: Arc::new(AtomicU64::new(0)),
            draft_home,
            opening_draft: None,
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
        inbox.show_crash_notice(cx);
        inbox.watch_store_restarts(store_restarts, cx);
        theme::follow(cx, |inbox, next, cx| {
            if inbox.theme != next {
                inbox.theme = next;
                inbox.row_widths.clear();
                cx.notify();
            }
        });
        inbox.watch_reviews(cx);
        inbox.watch_notes(cx);
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
        self.first_preview.sync_changed(&self.session.sync);
        if let Some(warning) = opened.warning {
            self.session.show_toast(warning, false);
            self.dismiss_notice_after(
                self.session.toast_generation(),
                std::time::Duration::from_secs(8),
                cx,
            );
        }
        cx.default_global::<ReplyChecks>()
            .start(self.account_id.clone(), sync_with.is_none() || store_failed);
        if !store_failed {
            self.cid_repair = Some(self.store.call(fell_core::Store::cid_repair_progress));
        }
        // Sync opens its own connection only after this one finished opening
        // and migrating the file, so the two never race for the first lock.
        if !store_failed && let Some((config, path)) = sync_with {
            let write_back = config.write_back;
            let (handle, events) = sync_worker::start(config, path, self.notifications.clone());
            if write_back && let Some(drafts) = cx.try_global::<crate::draft_host::AgentDrafts>() {
                let sync = handle.clone();
                drafts
                    .0
                    .on_server_draft_queued(move || sync.write_back_now());
            }
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
                    fell_core::diagnostic!(
                        WARN,
                        "Could not check cached CID roles",
                        error = &err as &dyn std::error::Error
                    );
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
                            fell_core::diagnostic!(
                                WARN,
                                "Could not repair cached CID roles",
                                error = &err as &dyn std::error::Error
                            );
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
                            SyncEvent::Worker(event) => {
                                inbox.apply_worker_event(event, cx);
                                return;
                            }
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
                                mut dropped,
                                flag_recovery_delayed,
                                sent_append_delayed,
                                draft_append_delayed,
                                draft_append_recovered,
                            } => {
                                let sent_failed = dropped
                                    .iter()
                                    .any(|write| write.kind == OutboxKind::AppendSent);
                                let cleanup_failed = dropped
                                    .iter()
                                    .any(|write| write.reason == DropReason::DraftCleanup);
                                let draft_failed = dropped.iter().any(|write| {
                                    write.kind == OutboxKind::AppendDraft
                                        && write.reason != DropReason::DraftCleanup
                                });
                                dropped.retain(|write| {
                                    !matches!(
                                        write.kind,
                                        OutboxKind::AppendSent
                                            | OutboxKind::AppendDraft
                                            | OutboxKind::DeleteDraft
                                    )
                                });
                                let mut lines = Vec::new();
                                let local_save_failed =
                                    inbox.session.status_notice().is_some_and(|notice| {
                                        notice.text == fell_core::compose::LOCAL_SENT_COPY_FAILED
                                    });
                                if sent_failed {
                                    lines.push("Sent · server copy unconfirmed".to_string());
                                }
                                if cleanup_failed {
                                    lines.push("An older draft copy may still be in Drafts".into());
                                }
                                if !dropped.is_empty() || flag_recovery_delayed {
                                    lines.push(write_back_notice(
                                        &inbox.session,
                                        &dropped,
                                        flag_recovery_delayed,
                                    ));
                                }
                                let draft_status = if draft_failed {
                                    Some(fell_core::compose::DraftSaveStatus::ServerFailed)
                                } else if draft_append_delayed {
                                    Some(fell_core::compose::DraftSaveStatus::ServerRetrying)
                                } else if draft_append_recovered {
                                    Some(fell_core::compose::DraftSaveStatus::Server)
                                } else {
                                    None
                                };
                                if let Some(status) = draft_status {
                                    inbox.apply_draft_write_notice(status, lines, cx);
                                    return;
                                }
                                if !lines.is_empty() {
                                    inbox.session.show_toast(lines.join(" · "), true);
                                } else if sent_append_delayed && !local_save_failed {
                                    inbox
                                        .session
                                        .show_toast("Sent · retrying server copy", false);
                                } else {
                                    return;
                                }
                                cx.notify();
                                return;
                            }
                            // Refresh list rows and the open thread when
                            // stored parts, state or previews change.
                            SyncEvent::BodiesStored
                            | SyncEvent::FlagsChanged
                            | SyncEvent::SnippetsStored => {
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
                        let active =
                            matches!(kind, SyncKind::Connecting | SyncKind::Syncing { .. });
                        if !active
                            || matches!(&kind, SyncKind::Syncing {
                            progress: Some(progress),
                            ..
                        } if progress.folder == inbox.session.roles.label("INBOX"))
                        {
                            inbox.waiting_for_inbox_total = false;
                        }
                        let finishing =
                            !active && matches!(inbox.session.sync, SyncKind::Syncing { .. });
                        if finishing {
                            inbox
                                .pending_bodies
                                .wait_for_read(inbox.conv_gen.load(Ordering::SeqCst));
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
                        let open_pending = inbox.first_preview.sync_changed(&kind);
                        inbox.session.apply_sync_status(kind);
                        if finishing && inbox.session.search.is_some() {
                            inbox.read_selected_messages(true, cx);
                        } else if wrote || finishing {
                            inbox.request_threads(
                                ReadIntent {
                                    refresh_open: true,
                                    ..ReadIntent::default()
                                },
                                cx,
                            );
                        } else if open_pending {
                            inbox.read_selected_messages(true, cx);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    return;
                }
            }
            let _ = this.update(cx, |inbox, cx| {
                if inbox.first_preview.allow_pending() {
                    inbox.read_selected_messages(true, cx);
                }
            });
        })
        .detach();
    }

    /// Reread the folder on screen on the store thread. While one read is in
    /// flight, further requests merge into a single follow-up, so a burst of
    /// sync batches is one read and never a queue.
    fn request_threads(&mut self, mut intent: ReadIntent, cx: &mut Context<Self>) {
        if self.opening_draft.is_some() && self.session.in_drafts() && !intent.switch {
            self.defer_folder_read(intent);
            return;
        }
        if self.session.search.is_some() {
            self.search_view.defer_folder_read(intent);
            return;
        }
        // A sync refresh in flight does not make the reader wait: their read
        // starts now on the interactive lane, and the refresh is dropped
        // when it lands. This read sees everything the refresh would have.
        let supersede = self.read_in_flight && self.refresh_in_flight && !intent.is_refresh();
        if self.undos_in_flight > 0 || (self.read_in_flight && !supersede) {
            self.defer_folder_read(intent);
            return;
        }
        if let Some(earlier) = self.read_wanted.take() {
            intent = earlier.merge(intent);
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
        let label = self.session.label_scope.clone();
        let reading_label = label.clone();
        let reading = folder.clone();
        let refresh_labels = self.session.loading || intent.changed || intent.is_refresh();
        let same_scope = folder == self.list_folder && label == self.list_label;
        // A folder the rows on screen do not come from opens at its top.
        let span = if let Some(key) = intent
            .select
            .as_ref()
            .filter(|_| intent.notification_pane.is_some())
        {
            FolderSpan::Window {
                first: Some(key.clone()),
                last: key.clone(),
                before: self.session.load_rows / 2,
                after: self.session.load_rows / 2,
            }
        } else if !same_scope || self.session.threads.is_empty() {
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
            .filter(|id| same_scope && !self.pending_removals.contains(id));
        let read = move |store: &fell_core::Store| -> ThreadsResult {
            let follow = follow.map(ThreadId);
            let (page, followed) = if let Some(label) = &reading_label {
                store
                    .label_page(&account, label, &span, follow.as_ref())
                    .map_err(|err| err.to_string())?
            } else {
                let page = store
                    .folder_page(&account, &reading, &span)
                    .map_err(|err| err.to_string())?;
                let followed = follow
                    .as_ref()
                    .filter(|id| !page.threads.iter().any(|thread| &thread.thread_id == *id))
                    .and_then(|id| store.folder_key_of(&account, &reading, id).ok().flatten());
                (page, followed)
            };
            let names = store.folder_names(&account).unwrap_or_default();
            let labels = refresh_labels
                .then(|| store.list_labels(&account).ok())
                .flatten();
            let roles = store.folder_roles(&account).unwrap_or_default();
            let mut rows = present::thread_rows(&page.threads, &me);
            let unread = page.unread;
            let drafts = if reading_label.is_none() && roles.is(FolderRole::Drafts, &reading) {
                // Synced drafts name who they are to, and none is unread.
                let ids: Vec<ThreadId> = page
                    .threads
                    .iter()
                    .map(|thread| thread.thread_id.clone())
                    .collect();
                let previews = store
                    .synced_draft_previews(&account, &reading, &ids)
                    .unwrap_or_default();
                // The header shows no count in Drafts; the folder's own
                // count stays, so leaving Drafts never reads "0 unread".
                present::address_draft_rows(&mut rows, &previews, &me);
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
                labels,
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
            let result = reply
                .await
                .unwrap_or_else(|_| Err("Could not read this folder".into()));
            let _ = this.update(cx, |inbox, cx| {
                if inbox.read_gen == generation {
                    let log = inbox.frames.clone();
                    FrameLog::event(log.as_ref(), "rows", || {
                        inbox.read_landed((folder, label), result, intent, cx)
                    });
                }
            });
        })
        .detach();
    }

    fn defer_folder_read(&mut self, intent: ReadIntent) {
        self.read_wanted = Some(match self.read_wanted.take() {
            Some(earlier) => earlier.merge(intent),
            None => intent,
        });
    }

    fn read_landed(
        &mut self,
        scope: (String, Option<String>),
        result: ThreadsResult,
        intent: ReadIntent,
        cx: &mut Context<Self>,
    ) {
        self.read_in_flight = false;
        self.refresh_in_flight = false;
        if self.opening_draft.is_some()
            && self.session.in_drafts()
            && scope.0 == self.list_folder
            && scope.1 == self.list_label
            && !intent.switch
        {
            self.defer_folder_read(intent);
            return;
        }
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
        let stale = self.undos_in_flight > 0
            || scope.0 != self.session.folder
            || scope.1 != self.session.label_scope
            || wanted.as_ref().is_some_and(|next| !next.is_refresh());
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
        self.show_threads(scope, result, intent, follow_up_pending, cx);
        // Only sync wrote while that read ran. Show it first, then read once
        // more, so a fast sync cannot starve the list.
        if let Some(next) = wanted {
            self.request_threads(next, cx);
        }
    }

    fn show_threads(
        &mut self,
        scope: (String, Option<String>),
        result: ThreadsResult,
        intent: ReadIntent,
        follow_up_pending: bool,
        cx: &mut Context<Self>,
    ) {
        let (folder, label) = scope;
        self.session.loading = false;
        let ListRead {
            rows: threads,
            more_before,
            more_after,
            mut unread,
            names,
            mut labels,
            followed,
            drafts,
            roles,
        } = match result {
            Ok(found) => found,
            Err(err) => {
                if intent.switch {
                    self.session.folder = self.list_folder.clone();
                    self.session.label_scope.clone_from(&self.list_label);
                }
                self.session.fail_query(QueryFault::Folder, err);
                self.settle_pending_bodies();
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
        let switched = folder != self.list_folder || label != self.list_label;
        let had_store_rows = !rows.is_empty();
        let edges = prepare_list_rows(
            &mut rows,
            &mut unread,
            (!switched).then_some(&self.pending_removals),
            (more_before, more_after),
            label.is_some(),
        );
        self.pending_flags
            .retain(|_, flags| flags.land(self.read_gen));
        if !switched {
            for row in &mut rows {
                let Some(flags) = self.pending_flags.get(&row.id) else {
                    continue;
                };
                if let Some(flag) = &flags.unread
                    && row.unread != flag.value
                {
                    unread = if flag.value {
                        unread + 1
                    } else {
                        unread.saturating_sub(1)
                    };
                    row.unread = flag.value;
                }
                if let Some(flag) = &flags.starred {
                    row.starred = flag.value;
                }
            }
        }
        let Some((more_before, more_after)) = edges else {
            self.request_threads(
                ReadIntent {
                    fetch: Fetch {
                        jump: Some(crate::model::Jump::Top(self.session.load_rows)),
                        ..Fetch::default()
                    },
                    ..intent
                },
                cx,
            );
            return;
        };
        // New mail moved the selected thread outside the rows read (they do
        // not reach the top of the folder). Read the rows around it, as undo
        // does, instead of selecting whichever row took its place.
        let followed = followed.filter(|key| !self.pending_removals.contains(&key.thread_id.0));
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
        self.pending_labels
            .land(self.read_gen, &mut rows, labels.as_mut(), label.as_deref());
        if let Some(labels) = labels {
            self.session.set_labels(labels);
        }
        self.list_folder = folder;
        self.list_label = label;
        if switched {
            self.session.forget_window();
            self.session
                .picked
                .retain(|id| rows.iter().any(|row| &row.id == id));
            self.painted_label_rows.clear();
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
        let had_rows = !self.session.threads.is_empty();
        let emptied = !switched
            && self.list_label.is_some()
            && had_rows
            && rows.is_empty()
            && !had_store_rows;
        if emptied {
            let full_width = self.motion.list_emptied(now);
            if !full_width && self.motion.motion.enabled() {
                self.leaving_rows = Some(std::mem::take(&mut self.painted_label_rows).into());
            }
            if self.motion.motion.enabled()
                && self.conv_layout_current
                && self.open_thread.is_some()
                && let Some(layout) = self.painted_reader
            {
                let disclosures = self.motion.disclosures(&self.session, self.frame_now);
                self.leaving_reader = Some(Arc::new(LeavingReader {
                    subject: self.shown_subject(),
                    messages: std::mem::take(&mut self.session.messages),
                    offset: f32::from(clamped_offset(&self.conv_scroll)),
                    layout,
                    quotes: self.session.quotes_open.clone(),
                    recipients: self.session.recipients_open.clone(),
                    arrival: self.motion.arrival(self.frame_now),
                    alphas: self.motion.body_alphas(self.frame_now),
                    disclosures,
                }));
            }
            self.session.pane = Pane::List;
            self.session.full_width = false;
            self.motion.list_glide = None;
            self.motion.list = None;
        } else if switched || !rows.is_empty() {
            self.motion.finish_empty_list_entry();
        }
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
        self.waiting_for_first_rows &= rows.is_empty();
        self.session.set_page(rows, more_before, more_after, unread);
        if let Some(key) = &intent.select
            && self.session.select_if_present(key.thread_id.as_str())
            && self.session.cursor_id != before_id
        {
            self.motion.land_selection(self.session.cursor_id.clone());
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
        if !emptied && (switched || had_rows || !self.session.threads.is_empty()) {
            self.start_list_shift(drawn, switched, intent.select.is_some());
        }
        if self.list_label.is_some() {
            self.warm = None;
            self.warmed_folder = None;
        } else if self.warmed_folder.as_deref() != Some(self.list_folder.as_str()) {
            self.warm_bottom(more_after, cx);
        }
        let open_changed = self.open_thread != self.session.cursor_id;
        let grew = open_count.is_some_and(|count| count != self.session.messages.len());
        // A new selection and draft edits load immediately. Give a queued
        // folder refresh the opportunity to resolve the open thread's key
        // before starting another body-only read.
        let new_selection = open_changed && self.session.cursor_id.is_some();
        let refresh_open = open_changed || grew || waiting || intent.refresh_open;
        if emptied
            || new_selection
            || self.session.cursor_draft().is_some()
            || (refresh_open && !follow_up_pending)
        {
            self.read_selected_messages(true, cx);
        }
        // A cursor move that waited for these rows may need more.
        self.issue_fetch(cx);
        if intent.notification_pane == Some(Pane::Conversation)
            && let Some(key) = intent.select
            && self.session.cursor_id.as_deref() == Some(key.thread_id.as_str())
        {
            self.open_notification_selection(cx);
        }
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
                if inbox.list_folder != folder || inbox.list_label.is_some() {
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
                                        label_scope: None,
                                        unread_note: false,
                                    };
                                    thread_row(
                                        &row_theme,
                                        &chunk[index],
                                        index,
                                        &look,
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
        let first = ((-offset / ROW_PX).floor() as usize).saturating_sub(2);
        let last = ((-offset + view) / ROW_PX).ceil() as usize + 2;
        let overscan = self.motion.list.as_ref().map_or(0, |s| s.overscan(ROW_PX));
        let rows = &self.session.threads;
        (first.saturating_sub(overscan)..(last + overscan).min(rows.len()))
            .filter_map(|index| {
                let row = &rows[index];
                let slot = index as f32 * ROW_PX + offset;
                let top = slot + self.motion.row_offset(&row.id, slot, now);
                ((first..last).contains(&index) || (top < view && top + ROW_PX > 0.0)).then(|| {
                    Drawn {
                        top,
                        slot,
                        row: row.clone(),
                    }
                })
            })
            .collect()
    }

    /// Animate the rows from where `drawn` had them to where they are now.
    /// Rows among them that left the list fade out where they were, and
    /// scroll with the list from there. `replace`: the list is another
    /// folder's; it fades in and nothing of the old one lingers.
    fn start_list_shift(&mut self, drawn: Vec<Drawn>, replace: bool, reveal_selection: bool) {
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
        let reach = self.list_viewport().unwrap_or(640.0);
        // Keep the current shift if all drawn rows stayed in place and no
        // row arrived below them in the viewport (undo at the end of a list).
        let end = self.session.threads.len() as f32 * ROW_PX;
        let same_visible_end = drawn.last().is_some_and(|last| {
            let old_end = last.slot + ROW_PX;
            old_end >= reach || end + offset <= old_end + 0.5
        });
        let kept = !replace
            && same_visible_end
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
        // Rows of a list this one replaced that sat below its last row:
        // nothing arrives over them, so they fade out where they were.
        let below = drawn
            .into_iter()
            .map(|old| {
                let top = old.top - offset;
                (old.row, top)
            })
            .filter(|(_, top)| end == 0.0 || *top >= end)
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
        .map(|shift| {
            let shift = shift.fading_below(below);
            if reveal_selection && !replace {
                shift.show_restored_selection()
            } else {
                shift
            }
        });
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

    /// A key goes straight to the destination while outgoing paint fades.
    fn settle_motion(&mut self) {
        self.reveal.cancel_pending();
        self.setup_exit = None;
        let now = self.motion.clock.now();
        let settled = self.motion.settle(now);
        self.row_widths.settle();
        self.agents_motion.settle();
        self.settle_search_match();
        if let Some(y) = settled.list {
            self.set_list_offset(y);
        }
        if let Some(y) = settled.conversation {
            self.instant_conversation_scroll = true;
            let offset = self.conv_scroll.offset();
            self.conv_scroll.set_offset(point(offset.x, px(y)));
        }
    }

    /// An explicit selection can open a conversation while bodies download.
    fn request_messages(&mut self, force: bool, cx: &mut Context<Self>) {
        self.first_preview.take();
        self.read_selected_messages(force, cx);
    }

    /// Read the selected thread on the store thread. A read that is stale
    /// by the time the store thread reaches it is skipped there.
    fn read_selected_messages(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(id) = self.session.cursor_id.clone() else {
            self.conv_gen.fetch_add(1, Ordering::SeqCst);
            self.clear_open_message();
            self.settle_pending_bodies();
            return;
        };
        if self
            .opening_draft
            .as_ref()
            .is_some_and(|open| open.thread.as_str() == id)
            || (!force && self.open_thread.as_deref() == Some(id.as_str()))
        {
            return;
        }
        if !self.session.search_results_current() {
            if self
                .session
                .search
                .as_ref()
                .is_some_and(|search| search.pending)
            {
                self.search_view.refresh_open_after_reply |= self.pending_bodies.after.is_some();
            } else {
                self.settle_pending_bodies();
            }
            return;
        }
        if let Some(draft) = self.session.cursor_draft() {
            self.first_preview.take();
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
            if same {
                if present::same_messages(&self.session.messages, &shown) {
                    if self.session.messages != shown {
                        let generation = self.conv_gen.fetch_add(1, Ordering::SeqCst) + 1;
                        self.queue_conversation_refresh(shown, generation);
                    }
                } else {
                    self.session.refresh_messages(shown, true);
                }
            } else {
                self.invalidate_conversation_layout();
                // Fades in as a synced draft's messages do, so `j` between
                // the two kinds never pops.
                self.reveal = Reveal::Waiting;
                self.motion.show_thread(&mut self.session, shown);
                self.scroll_conversation();
            }
            self.session.clear_query_fault(QueryFault::Message);
            self.mark_open(id, None);
            self.settle_pending_bodies();
            return;
        }
        let opening = self.open_thread.as_deref() != Some(&id);
        let generation = self.conv_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let note_generation = self.note_generation;
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
        let wait_for_body = self.first_preview.waits_for_body();
        let reply = self.store.call(move |store| {
            if current.load(Ordering::SeqCst) != generation {
                return Ok(None);
            }
            let mut view =
                read_conversation(store, &account, &thread, (&me, &roles, &names, &viewing))
                    .map_err(|err| err.to_string())?;
            // The automatic preview after setup waits for the last message.
            // Each sync write refreshes the folder and retries this read.
            if wait_for_body && !present::newest_body_ready(&view) {
                return Ok(None);
            }
            let reads = mail_body::read_bodies(store, &account, &mut view, &cached);
            let matches = query.as_ref().map(|(accepted, query)| {
                (
                    *accepted,
                    store
                        .search_thread_matches(&account, query, &thread)
                        .unwrap_or_default(),
                )
            });
            let latest_note = opening
                .then(|| {
                    store
                        .latest_thread_note_id(&account, &thread)
                        .ok()
                        .flatten()
                })
                .flatten();
            let notes = store.thread_notes(&account, &thread).ok();
            Ok(Some(ConversationRead {
                view,
                reads,
                matches,
                notes,
                latest_note,
                note_generation,
            }))
        });
        cx.spawn(async move |this, cx| {
            let result = reply
                .await
                .unwrap_or_else(|_| Err("Mailbox store stopped".into()));
            let _ = this.update(cx, |inbox, cx| {
                inbox.receive_conversation(id, generation, result, cx)
            });
        })
        .detach();
    }

    fn receive_conversation(
        &mut self,
        id: String,
        generation: u64,
        result: Result<Option<ConversationRead>, String>,
        cx: &mut Context<Self>,
    ) {
        if self.conv_gen.load(Ordering::SeqCst) != generation
            || self.session.cursor_id.as_deref() != Some(id.as_str())
            || self
                .opening_draft
                .as_ref()
                .is_some_and(|open| open.thread.as_str() == id)
        {
            return;
        }
        let inbox = self;
        let log = inbox.frames.clone();
        FrameLog::event(log.as_ref(), "messages", || match result {
            Ok(None) => inbox.finish_body_refresh(generation, true),
            Ok(Some(read)) if read.view.is_empty() && inbox.session.search.is_some() => {
                // A hit may disappear between the two reads. Search
                // has its own list, so a folder read cannot replace it.
                inbox.clear_open_message();
                inbox.settle_pending_bodies();
                inbox.session.cancel_read_delay();
                inbox.session.clear_query_fault(QueryFault::Message);
                if inbox.session.search_results_current() {
                    inbox.refresh_search(cx);
                }
            }
            Ok(Some(read)) if read.view.is_empty() && !inbox.session.messages.is_empty() => {
                // The open thread key can disappear between the folder
                // and message reads. Keep the conversation until a
                // folder read confirms where its messages belong.
                if !inbox.read_in_flight {
                    inbox.request_threads(ReadIntent::default(), cx);
                }
            }
            Ok(Some(ConversationRead {
                mut view,
                reads,
                matches,
                notes,
                latest_note,
                note_generation,
            })) => {
                if let Some(notes) = notes {
                    inbox.update_agent_notes(note_generation, &id, notes);
                }
                let automatic = inbox.first_preview.take();
                inbox.body_loads.accept(reads, &mut inbox.bodies, &mut view);
                inbox.session.clear_query_fault(QueryFault::Message);
                let same = inbox.open_thread.as_deref() == Some(id.as_str());
                // Sync may rekey the thread as its bodies arrive.
                // A read of the same messages still belongs to the reader.
                let in_place = (same || !view.is_empty())
                    && present::same_messages(&inbox.session.messages, &view);
                let follow = if in_place {
                    inbox.keep_shown_text(&mut view);
                    inbox.queue_conversation_refresh(view, generation);
                    false
                } else if same {
                    inbox.keep_shown_text(&mut view);
                    let preview = inbox.reveal == Reveal::Preview;
                    let bottom = preview || inbox.conversation_at_bottom();
                    let padding = inbox.preview_padding;
                    let laid_out = inbox.preview_laid_out;
                    inbox.invalidate_conversation_layout();
                    inbox.preview_padding = padding;
                    inbox.preview_laid_out = laid_out;
                    let follow = inbox.session.refresh_messages(view, bottom);
                    inbox.finish_body_refresh(generation, true);
                    if preview {
                        inbox.land_on_first_preview();
                    }
                    follow && !preview
                } else {
                    inbox.invalidate_conversation_layout();
                    inbox.reveal = Reveal::Waiting;
                    inbox.motion.show_thread(&mut inbox.session, view);
                    inbox.finish_body_refresh(generation, false);
                    // Keep the initial header landing while the
                    // preview's pending bodies fill in.
                    if automatic
                        || (inbox.session.cursor_awaits_body()
                            && inbox.pending_bodies.downloading(&inbox.session.sync))
                    {
                        inbox.land_on_first_preview();
                    } else {
                        inbox.land_on_reader();
                    }
                    false
                };
                let landing = if inbox.quiet_search_preview() {
                    crate::model::search::MatchLanding::KeepReader
                } else if same {
                    crate::model::search::MatchLanding::CurrentMatch
                } else {
                    crate::model::search::MatchLanding::Best
                };
                if let Some((accepted, matches)) = matches
                    && inbox
                        .session
                        .set_search_matches(accepted, &id, matches, landing)
                {
                    inbox.land_on_reader();
                }
                inbox.mark_open(id, latest_note);
                if follow {
                    inbox.scroll_conversation();
                }
            }
            Err(err) => {
                // Never leave another thread's body under this row.
                if inbox.open_thread.as_deref() != Some(id.as_str()) {
                    inbox.clear_open_message();
                }
                inbox.settle_pending_bodies();
                inbox.session.fail_query(QueryFault::Message, err);
            }
        });
        cx.notify();
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

    fn queue_conversation_refresh(&mut self, messages: Vec<ConvMessage>, generation: u64) {
        // Newly downloaded bodies can push the latest message below a short
        // pane. An already scrollable pane keeps the reader where they are.
        let downloaded =
            self.session
                .messages
                .iter()
                .zip(&messages)
                .enumerate()
                .any(|(index, (old, new))| {
                    (index == self.session.message_cursor
                        || matches!(self.reveal, Reveal::Following(_)))
                        && old.body_state == fell_core::BodyState::Pending
                        && old.body_state != new.body_state
                });
        if downloaded && self.reveal != Reveal::Preview && self.body_landing.is_none() {
            self.reveal.body_changed(
                self.conv_layout_current
                    && present::conversation_fits(f32::from(self.conv_scroll.max_offset().height)),
            );
        }
        self.body_refresh.messages = Some((messages, generation));
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
            .as_ref()
            .map(|(messages, _)| messages.as_slice())
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
                            if inbox.session.search.is_some() {
                                inbox.refresh_search_snippet(MessageId(id.clone()), cx);
                                inbox.reread_open_thread(cx);
                            }
                            inbox.request_threads(
                                ReadIntent {
                                    refresh_open: true,
                                    ..ReadIntent::default()
                                },
                                cx,
                            );
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
        done: impl FnOnce(
            &mut Self,
            Result<T, fell_core::Error>,
            &mut Context<Self>,
        ) -> Option<ReadIntent>
        + 'static,
        cx: &mut Context<Self>,
    ) {
        // Retain the current view while the write runs. It will be
        // reread once the store can see the change.
        self.begin_search_refresh(cx);
        let reply = self.store.call(change);
        cx.spawn(async move |this, cx| {
            finish_mailbox_change(reply, |result| {
                let _ = this.update(cx, |inbox, cx| {
                    if result.is_ok()
                        && let Some(queue) = &inbox.notifications
                    {
                        queue.refresh();
                    }
                    let after = done(inbox, result, cx);
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
            .await;
        })
        .detach();
    }

    fn apply_draft_write_notice(
        &mut self,
        status: fell_core::compose::DraftSaveStatus,
        mut lines: Vec<String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(warning) = status.warning() {
            lines.insert(0, warning.into());
        }
        if !lines.is_empty() {
            self.session.show_toast(lines.join(" · "), true);
            cx.notify();
        } else if self
            .session
            .status_notice()
            .is_some_and(|notice| notice.text == fell_core::compose::SERVER_DRAFT_SAVE_RETRYING)
        {
            self.session.show_toast("Draft saved", false);
            cx.notify();
        }
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
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            while events.next().await.is_some() {
                while events.try_recv().is_ok() {}
                let closed = if let Some(notice) = hub.take_close_notices().pop() {
                    let notice = if notice.needs_local_write() {
                        cx.background_executor()
                            .spawn(async move { notice.after_local_write() })
                            .await
                    } else {
                        notice
                    };
                    if notice.server_save().is_some() {
                        store.call(move |store| notice.resolve(store)).await.ok()
                    } else {
                        Some(notice.into_line())
                    }
                } else {
                    None
                };
                let alive = this
                    .update(cx, |inbox, cx| {
                        // A draft changed, was sent, or is new: the drafts
                        // list on screen follows.
                        if inbox.session.in_drafts() {
                            inbox.request_threads(ReadIntent::default(), cx);
                        }
                        if let Some((mut line, warn)) = closed {
                            if inbox.session.in_drafts()
                                && let Some(trimmed) = line.strip_suffix("  ·  gd drafts")
                            {
                                line = trimmed.to_owned();
                            }
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

    fn show_sent_copy(&mut self, thread: ThreadId, mut message: fell_core::Message, rich: Rich) {
        if self.open_thread.as_deref() != Some(thread.as_str())
            || self.session.cursor_id != self.open_thread
            || !self.search_preview_current()
            || self
                .session
                .messages
                .iter()
                .any(|shown| shown.id == message.headers.message_id.as_str())
        {
            return;
        }
        let viewing = self
            .session
            .search_source(thread.as_str())
            .unwrap_or(&self.session.folder);
        let parts = message.parts.take();
        let mut shown = present::conversation(
            std::slice::from_ref(&message),
            &self.session.account_email,
            &self.session.roles,
            &self.session.folders,
            viewing,
        )
        .remove(0);
        shown.rich = rich;
        if let Some(parts) = parts {
            shown.attachments = attachments::visible_parts(parts.mime_parts);
        }
        // A stale read must not replace the just-committed reply. Keep the
        // reader's existing cursor and viewport, and fade in the new body.
        self.conv_gen.fetch_add(1, Ordering::SeqCst);
        let follow = self.conversation_at_bottom()
            && self.session.message_cursor + 1 == self.session.messages.len();
        let anchor = if self.conv_layout_current && self.motion.conv_glide.is_none() {
            ScrollAnchor::capture(&self.conv_scroll, usize::from(self.conv_note_slot))
        } else {
            None
        };
        self.invalidate_conversation_layout();
        self.motion.message_arrived(&shown.id);
        let messages = &mut self.session.messages;
        let at = messages.partition_point(|old| old.date_unix <= shown.date_unix);
        messages.insert(at, shown);
        self.sent_arrival_anchor = anchor.map(|mut anchor| {
            anchor.index += usize::from(at <= anchor.index);
            anchor
        });
        if follow && at + 1 == messages.len() {
            self.session.message_cursor = at;
            self.reveal = Reveal::Following(RevealPhase::Laying);
        } else if at <= self.session.message_cursor && messages.len() > 1 {
            self.session.message_cursor += 1;
        }
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
            move |inbox, result, _| match result {
                Ok(()) => {
                    inbox.pending_removals.remove(&id);
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

    /// Remove the selected threads in this frame and select the row that
    /// takes the cursor's place.
    fn remove_threads(&mut self, ids: Vec<String>, removal: Removal, cx: &mut Context<Self>) {
        let (ids, drafts) = self.session.split_removal_targets(ids);
        if ids.is_empty() && !drafts.is_empty() {
            self.session.show_toast(DRAFT_NOT_THREAD, true);
            return;
        }
        if self.session.search.is_some() {
            self.remove_search_threads(ids, removal, cx);
            return;
        }
        if self.session.folder != self.list_folder {
            self.session
                .show_toast("The folder is still loading", false);
            return;
        }
        if ids.is_empty() {
            self.session.show_toast("No conversation", true);
            return;
        }
        let key = ids.iter().find_map(|id| {
            self.session
                .threads
                .iter()
                .find(|row| &row.id == id)
                .map(row_key)
        });
        let roles = &self.session.roles;
        let folder = self.list_folder.clone();
        let spam_to = if roles.is(FolderRole::Junk, &folder) {
            FolderRole::Inbox
        } else {
            FolderRole::Junk
        };
        let destination = self
            .session
            .folder_text(roles.name(if removal == Removal::Delete {
                FolderRole::Trash
            } else {
                spam_to
            }));
        let account = self.account_id.clone();
        let threads: Vec<ThreadId> = ids.iter().cloned().map(ThreadId).collect();
        self.session.picked.clear();
        self.session.visual = false;
        self.session.anchor_id = None;
        if key.is_some() && !roles.is_view(&folder) {
            self.take_out_of_list(&ids, cx);
        }
        self.change_mailbox(
            move |store| removal.in_folder(store, &account, threads, folder),
            move |inbox, result, _| {
                for id in &ids {
                    inbox.pending_removals.remove(id);
                }
                match result {
                    Ok(count) => {
                        let done_text = match removal {
                            Removal::Archive => archived_notice(count, ids.len()),
                            Removal::Delete | Removal::Spam => moved_notice(count, &destination),
                            Removal::Purge if count == 1 => "Removed from Fell".to_owned(),
                            Removal::Purge => format!("Removed {count} from Fell"),
                        };
                        inbox.session.show_toast(done_text, false);
                        inbox.send_to_server();
                        Some(ReadIntent {
                            changed: true,
                            ..ReadIntent::default()
                        })
                    }
                    Err(err) => {
                        let message = if removal == Removal::Archive {
                            archive_failure(&err)
                        } else {
                            err.to_string()
                        };
                        inbox.session.show_toast(message, true);
                        Some(ReadIntent {
                            changed: true,
                            select: key,
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
        self.start_list_shift(drawn, false, false);
        self.request_messages(true, cx);
        // The removed row was the last one loaded: load the row after it.
        self.issue_fetch(cx);
    }

    /// List position of the selected row, to recover it if a move fails.
    fn selected_key(&self) -> Option<ListKey> {
        let id = self.session.cursor_id.as_ref()?;
        self.session
            .threads
            .iter()
            .find(|row| &row.id == id)
            .map(row_key)
    }

    fn read_label_targets(&mut self, cx: &mut Context<Self>) {
        use futures::future::{Either, select};

        self.label_targets_read += 1;
        self.motion.reset_label_marks();
        let generation = self.label_targets_read;
        let account = self.account_id.clone();
        let ids = self.session.label_target_ids().to_vec();
        let reply = self
            .store
            .call(move |store| store.thread_labels(&account, &ids));
        let deadline = cx.background_executor().timer(Duration::from_millis(50));
        cx.spawn(async move |this, cx| {
            let result = match select(reply, deadline).await {
                Either::Left((result, _)) => result,
                Either::Right((_, reply)) => {
                    let _ = this.update(cx, |inbox, cx| {
                        if inbox.label_targets_read == generation {
                            inbox.session.open_label_picker();
                            cx.notify();
                        }
                    });
                    reply.await
                }
            };
            let result = result
                .map_err(|_| "The mailbox store is unavailable".into())
                .and_then(|result| result.map_err(|err| err.to_string()));
            let _ = this.update(cx, |inbox, cx| {
                if inbox.label_targets_read != generation || !inbox.session.label_picker_active() {
                    return;
                }
                let late = inbox.session.visible_overlay() == Overlay::Folders;
                let before = inbox.before_step();
                let step = inbox.session.load_label_targets(result);
                if late && inbox.session.label_picker_active() {
                    let now = inbox.motion.clock.now();
                    inbox.motion.fade_label_marks(now);
                }
                inbox.apply_step(step, before, cx);
            });
        })
        .detach();
    }

    fn apply_labels(&mut self, edit: LabelEdit, cx: &mut Context<Self>) {
        let now = self.motion.clock.now();
        let leaving_scope = !edit.add
            && self.list_label.as_deref() == Some(&edit.name)
            && self.session.search.is_none();
        self.session.label_rows(&edit, leaving_scope);
        let pending = self.pending_labels.start(edit.clone());
        for row in self
            .session
            .threads
            .iter()
            .filter(|row| edit.ids.contains(&row.id))
        {
            self.row_widths.observe_labels(&row.id, &row.labels, now);
        }
        let account = self.account_id.clone();
        let threads: Vec<_> = edit.ids.into_iter().map(ThreadId).collect();
        let count = threads.len();
        let notice = present::LabelNotice {
            verb: if edit.add { "Applied" } else { "Removed" },
            label: present::display_safe(&edit.name),
            relation: if edit.add { "to" } else { "from" },
            subject: self
                .session
                .label_target_subject()
                .map(present::display_safe),
            count,
            undo: !edit.add,
        };
        let change = if edit.add {
            fell_core::LabelChange::Add(edit.name)
        } else {
            fell_core::LabelChange::Remove(edit.name)
        };
        self.change_mailbox(
            move |store| store.label_threads(&account, &threads, change),
            move |inbox, result, cx| {
                inbox
                    .pending_labels
                    .finish(pending, result.is_ok().then_some(inbox.read_gen));
                match result {
                    Ok(_) => {
                        inbox.session.show_label_toast(notice);
                        inbox.expire_action_notice(cx);
                        inbox.send_to_server();
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

    fn apply_undo(&mut self, cx: &mut Context<Self>) {
        self.undo_mailbox(None, cx);
    }

    fn undo_mailbox(
        &mut self,
        notice: Option<(crate::notifications::Receipt, i64)>,
        cx: &mut Context<Self>,
    ) {
        let account = self.account_id.clone();
        let folder = self.session.folder.clone();
        let reading = folder.clone();
        let expected = notice.as_ref().map(|(_, action_id)| *action_id);
        self.undos_in_flight += 1;
        self.change_mailbox(
            move |store| {
                let (action, note_notice, restored) = match expected {
                    Some(id) => store
                        .undo_notification(&account, id)
                        .map(|(action, restored)| (action, None, restored)),
                    None => store.undo_with_note(&account),
                }?;
                let select = restored_folder_key(store, &account, &reading, &restored);
                Ok((action, note_notice, restored, select))
            },
            move |inbox, result, cx| {
                inbox.undos_in_flight -= 1;
                match result {
                    Ok((action, note_notice, restored, select)) => {
                        if note_notice.is_some() {
                            inbox.note_generation = inbox.note_generation.wrapping_add(1);
                        }
                        if action.kind == fell_core::ActionKind::DiscardDrafts {
                            inbox.send_to_server();
                            inbox.session.show_toast(action.summary, false);
                            return Some(ReadIntent {
                                changed: true,
                                ..ReadIntent::default()
                            });
                        }
                        let text = match action.kind {
                            fell_core::ActionKind::Read => "Marked unread",
                            fell_core::ActionKind::Unread => "Marked read",
                            fell_core::ActionKind::Archive if notice.is_some() => {
                                "Moved back to Inbox"
                            }
                            fell_core::ActionKind::Archive => "Moved back",
                            _ => "Undone",
                        };
                        if let Some((receipt, _)) = notice {
                            inbox.notification_feedback(receipt, text.into(), None, false);
                        } else if let Some(notice) = note_notice {
                            inbox.session.show_toast(notice, false);
                        } else if let Some(target) = action.label_undo_target {
                            let (count, subject) = match target {
                                LabelUndoTarget::Conversation(subject) => {
                                    (1, Some(present::display_safe(&subject)))
                                }
                                LabelUndoTarget::Conversations(count) => (count, None),
                            };
                            let (verb, label, relation) =
                                action.summary.strip_prefix("removed label ").map_or_else(
                                    || {
                                        (
                                            "Removed",
                                            action
                                                .summary
                                                .strip_prefix("added label ")
                                                .unwrap_or(&action.summary),
                                            "from",
                                        )
                                    },
                                    |label| ("Restored", label, "to"),
                                );
                            inbox.session.show_label_toast(present::LabelNotice {
                                verb,
                                label: present::display_safe(label),
                                relation,
                                subject,
                                count,
                                undo: false,
                            });
                        } else {
                            inbox.session.show_toast(text, false);
                        }
                        inbox.expire_action_notice(cx);
                        inbox.send_to_server();
                        if inbox.session.search.is_some() && !restored.is_empty() {
                            inbox.search_view.prefer = restored;
                        }
                        let select = select.filter(|_| inbox.session.folder == folder);
                        Some(ReadIntent {
                            changed: true,
                            select,
                            ..ReadIntent::default()
                        })
                    }
                    Err(err) => {
                        let raw = err.to_string();
                        let (text, alert) = if raw == "nothing to undo" {
                            ("Nothing to undo".into(), false)
                        } else {
                            (raw, true)
                        };
                        if let Some((receipt, _)) = notice {
                            inbox.notification_feedback(receipt, text, None, alert);
                        } else {
                            inbox.session.show_toast(text, alert);
                        }
                        // Resume any folder read held while the undo ran.
                        (inbox.undos_in_flight == 0 && !inbox.read_in_flight)
                            .then(|| inbox.read_wanted.take())
                            .flatten()
                    }
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
            |inbox, result, _| {
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
                |inbox, result, _| {
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
        for id in &edit.ids {
            self.protect_flag(id, edit.change, generation, None);
        }
        let account = self.account_id.clone();
        let folder = if self.list_label.is_some() {
            String::new()
        } else {
            self.list_folder.clone()
        };
        let threads: Vec<ThreadId> = edit.ids.iter().cloned().map(ThreadId).collect();
        let change = edit.change;
        let ids = edit.ids;
        self.change_mailbox(
            move |store| store.flag_threads(&account, &threads, &folder, change),
            move |inbox, result, _| {
                for id in ids {
                    if let Some(flags) = inbox.pending_flags.get_mut(&id) {
                        let slot = flags.slot(change);
                        if let Some(flag) = slot.as_mut().filter(|flag| flag.write == generation) {
                            if result.is_ok() {
                                flag.through = Some(inbox.read_gen);
                            } else {
                                *slot = None;
                            }
                        }
                    }
                }
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

    fn protect_flag(
        &mut self,
        id: &str,
        change: FlagChange,
        write: u64,
        through: Option<u64>,
    ) -> bool {
        let slot = self
            .pending_flags
            .entry(id.into())
            .or_default()
            .slot(change);
        if slot.as_ref().is_some_and(|flag| flag.write > write) {
            return false;
        }
        *slot = Some(PendingFlag {
            write,
            value: matches!(change, FlagChange::Unread | FlagChange::Star),
            through,
        });
        true
    }

    fn toggle_off_page_flags(&mut self, ids: Vec<String>, star: bool, cx: &mut Context<Self>) {
        if self.session.folder != self.list_folder || self.session.label_scope != self.list_label {
            self.session
                .show_toast("The folder is still loading", false);
            return;
        }
        let account = self.account_id.clone();
        let folder = self.list_folder.clone();
        let label = self.list_label.clone();
        let selected = ids.len();
        self.flag_gen += 1;
        let generation = self.flag_gen;
        let targets = ids.clone();
        let scope = (folder.clone(), label.clone());
        self.change_mailbox(
            move |store| {
                use fell_core::store::ListSource;
                let source = label
                    .as_deref()
                    .map_or(ListSource::Folder(&folder), ListSource::Label);
                store.toggle_selected_flags(&account, &ids, source, star)
            },
            move |inbox, result, _| match result {
                Ok((change, count)) => {
                    // The transaction decides the direction using every
                    // selected row, including the ones we cannot see yet.
                    if inbox.session.search.is_none()
                        && inbox.list_folder == scope.0
                        && inbox.list_label == scope.1
                    {
                        let mut loaded: Vec<_> = inbox
                            .session
                            .threads
                            .iter()
                            .filter(|row| targets.contains(&row.id))
                            .map(|row| row.id.clone())
                            .collect();
                        loaded.retain(|id| {
                            inbox.protect_flag(id, change, generation, Some(inbox.read_gen))
                        });
                        inbox.session.mark_rows(loaded, change);
                    }
                    inbox.session.show_flag_notice(change, selected, count);
                    inbox.send_to_server();
                    Some(ReadIntent {
                        changed: true,
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    inbox.session.show_toast(err.to_string(), true);
                    None
                }
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
    fn open_draft(
        &mut self,
        id: Option<String>,
        remote: Option<(ThreadId, fell_core::RemoteId)>,
        cx: &mut Context<Self>,
    ) {
        let Some(hub) = cx
            .try_global::<crate::draft_host::AgentDrafts>()
            .map(|drafts| drafts.0.clone())
        else {
            self.session
                .show_toast("Compose is not available here", true);
            return;
        };
        if let Some(id) = &id
            && crate::draft_host::show_draft(&hub, id)
        {
            self.watch_held_draft(&hub, id, cx);
            return;
        }
        let Some(home) = self.draft_home.clone() else {
            self.session.show_toast(
                "The mailbox did not open, so a draft has nowhere to go · ? details",
                true,
            );
            return;
        };
        let remote_open = remote.is_some();
        if let Some((thread, _)) = &remote {
            if self.opening_draft.is_some() {
                return;
            }
            let started = Instant::now();
            self.opening_draft = Some(OpeningServerDraft {
                thread: thread.clone(),
                started,
                ready: None,
            });
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(DRAFT_OPEN_FEEDBACK).await;
                let _ = this.update(cx, |inbox, cx| {
                    if inbox
                        .opening_draft
                        .as_ref()
                        .is_some_and(|open| open.started == started)
                    {
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        let account = self.account_id.clone();
        let from = fell_core::compose::configured_from(
            &self.session.account_name,
            &self.session.account_email,
        );
        let creating = id.is_none() && remote.is_none();
        // The draft goes live in the hub off this thread; only showing it
        // happens here.
        let task = cx.background_executor().spawn({
            let hub = hub.clone();
            async move {
                match (id, remote) {
                    (Some(id), _) => home
                        .open(&hub, &id, &account)
                        .map(|()| id)
                        .map_err(fell_core::Error::Action),
                    (None, Some((_, source))) => home.open_server_draft(&hub, &account, &source),
                    (None, None) => home
                        .new_draft(&hub, &account, &from)
                        .map_err(fell_core::Error::Action),
                }
            }
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |inbox, cx| {
                match result {
                    Ok(id) => {
                        if remote_open && let Some(open) = &mut inbox.opening_draft {
                            open.ready = Some(id.clone());
                        }
                        inbox.watch_held_draft(&hub, &id, cx);
                        if !crate::draft_host::show_draft(&hub, &id) && remote_open {
                            inbox.finish_server_draft_open(None, cx);
                            inbox.session.show_toast("Couldn't open the draft.", true);
                        }
                    }
                    Err(err) => {
                        if remote_open {
                            inbox.finish_server_draft_open(None, cx);
                        }
                        fell_core::diagnostic!(
                            WARN,
                            "Could not open compose draft",
                            error = &err as &dyn std::error::Error
                        );
                        let text = if remote_open {
                            fell_core::compose::server_draft_open_notice(&err)
                        } else if creating {
                            format!("Couldn't start a draft. {err}")
                        } else {
                            format!("Couldn't open the draft. {err}")
                        };
                        inbox.session.show_toast(text, true);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn finish_server_draft_open(&mut self, local_id: Option<&str>, cx: &mut Context<Self>) {
        let Some(open) = self.opening_draft.take() else {
            return;
        };
        if self.session.in_drafts() {
            if let Some(id) = local_id
                && self.session.cursor_id.as_deref() == Some(open.thread.as_str())
            {
                self.session.cursor_id = Some(present::draft_row_id(id));
            }
            self.request_threads(
                ReadIntent {
                    refresh_open: true,
                    ..ReadIntent::default()
                },
                cx,
            );
        }
        cx.notify();
    }

    fn refresh_closing_draft(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.session.cursor_draft() == Some(id)
            && let Some(index) = self.drafts.iter().position(|draft| draft.id == id)
            && let Some(draft) = self.live_drafts(vec![self.drafts[index].clone()], cx).pop()
        {
            if let Some(row) = self
                .session
                .threads
                .iter_mut()
                .find(|row| row.draft.as_deref() == Some(id))
                && let Some(updated) = present::draft_rows(std::slice::from_ref(&draft)).pop()
            {
                *row = updated;
            }
            self.drafts[index] = draft;
            self.read_selected_messages(true, cx);
        }
        self.request_threads(
            ReadIntent {
                refresh_open: true,
                ..ReadIntent::default()
            },
            cx,
        );
        cx.notify();
    }

    /// `r`, `R`, `f`: start a reply, reply all, or forward of the message
    /// the model picked, and bring the compose window up on it. The parent
    /// is read on the store thread and the draft is written off this one.
    fn start_reply(
        &mut self,
        kind: ReplyKind,
        suggestion: Option<crate::model::NoteRequest>,
        cx: &mut Context<Self>,
    ) {
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
        if let Some(note) = &suggestion {
            self.note_in_flight = Some(note.id);
        }
        self.forwarding_attachments |= forwarding;
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
            let reply_note = suggestion.clone();
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
                        .spawn(async move {
                            match reply_note {
                                Some(note) => {
                                    home.new_suggested_reply(&hub, &account, start, &note)
                                }
                                None => home.new_reply_with_files(&hub, &account, start, files),
                            }
                        })
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
                        if let Some(note) = suggestion {
                            inbox.agent_note_resolved(&note);
                        }
                        crate::draft_host::show_draft(&hub, &id);
                    }
                    Err(err) => {
                        if suggestion.is_some() {
                            inbox.finish_note_decision();
                        }
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

    fn expire_action_notice(&mut self, cx: &mut Context<Self>) {
        self.dismiss_notice_after(self.session.toast_generation(), Duration::from_secs(3), cx);
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
        self.note_pin = agent_notes::NotePin::default();
    }

    /// The conversation now shows thread `id`'s messages.
    fn mark_open(&mut self, id: String, latest_note: Option<i64>) {
        self.conversation_revision = self.conversation_revision.wrapping_add(1);
        if self.open_thread.as_deref() != Some(&id) {
            let notes = &self.session.agent_notes;
            self.note_pin = agent_notes::NotePin::opened(
                notes.current(Some(&id)).map(|note| note.id),
                latest_note.max(notes.newest(&id)),
            );
            self.note_visit = None;
        }
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
        if self.reveal == Reveal::Preview {
            self.reader_took_over(false);
            cx.notify();
        }
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
        let previous_notice = self.session.toast_generation();
        // When the note has scrolled away, the ordinary mail shortcuts
        // belong to the message the reader can see.
        self.session.agent_notes.shortcuts_hidden = !self.note_in_view();
        let step = self.session.on_key(key, page);
        self.session
            .dismiss_key_notice(previous_notice, step.undo || self.undos_in_flight > 0);
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
            label: self.session.label_scope.clone(),
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
            label: label_before,
            pane: pane_before,
            searching,
            search_generation,
            help_open,
        } = before;
        if pane_before != Pane::Conversation && self.session.pane == Pane::Conversation {
            self.reader_entered_conversation();
        }
        if !help_open && self.session.overlay == Overlay::Help {
            self.help_scroll = help::HelpScroll::default();
        }
        if let Some(request) = step.agents.clone() {
            self.request_agent_access(request, cx);
        }
        if self.session.search.is_some()
            || self.session.folder != folder_before
            || self.session.label_scope != label_before
        {
            self.motion.finish_empty_list_entry();
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
        if let Some(note) = step.note {
            self.choose_agent_note(note, cx);
        }
        if step.note_scroll != 0 {
            self.note_view.scroll(step.note_scroll);
        }
        if step.toggle_agent_note {
            self.toggle_pinned_note();
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
        if let Some(edit) = step.labels {
            self.apply_labels(edit, cx);
        }
        if step.read_label_targets {
            self.read_label_targets(cx);
        }
        if step.undo {
            self.apply_undo(cx);
        }
        if let Some(edit) = step.flags.clone() {
            self.apply_flags(edit, cx);
        }
        if let Some((ids, star)) = step.off_page_flags {
            self.toggle_off_page_flags(ids, star, cx);
        }
        if let Some(read) = step.read_after {
            self.read_after(read, cx);
        }
        if step.archive {
            let ids = self.session.action_targets();
            self.remove_threads(ids, Removal::Archive, cx);
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
        if let Some(file) = step.open_diagnostic {
            self.open_diagnostic(file, cx);
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
            self.open_draft(None, None, cx);
        }
        if step.open_draft
            && let Some(row) = self.session.cursor_thread()
        {
            let id = row.draft.clone();
            let server = row
                .server_draft
                .clone()
                .map(|source| (ThreadId(row.id.clone()), source));
            if id.is_some() || server.is_some() {
                self.open_draft(id, server, cx);
            } else {
                self.session
                    .show_toast(fell_core::Error::DraftNoLongerInFolder.to_string(), true);
            }
        }
        if let Some(kind) = step.reply {
            self.start_reply(kind, None, cx);
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
        if let Some(mode) = step.notification_mode {
            self.set_notification_mode(mode, cx);
        }
        // Reads go to the store thread. The list moves on this frame; the
        // conversation follows when its read lands.
        if step.reload_threads
            && (self.session.folder != folder_before || self.session.label_scope != label_before)
        {
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
        } else if (searching && self.session.search.is_none())
            || (self.first_preview != FirstPreview::Off
                && pane_before == Pane::List
                && self.session.pane == Pane::Conversation)
        {
            self.request_messages(false, cx);
        }
        if self.session.search.is_none()
            && let Some(intent) = self.search_view.take_folder_read()
            && self.session.folder == folder_before
            && self.session.label_scope == label_before
        {
            self.request_threads(intent, cx);
        }
        // A move past the loaded rows, or near their end, asks for more.
        self.issue_fetch(cx);
        if step.arm_which {
            self.arm_which(cx);
        }
        let thread_moved = self.session.cursor_id != thread_before;
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
        if self.session.overlay == Overlay::Agents {
            self.agents_scroll
                .scroll_to_item(self.session.agents.selected);
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
            && ScrollAnchor::capture_message(
                &self.conv_scroll,
                self.session.message_cursor,
                usize::from(self.conv_note_slot),
            )
            .is_some()
        {
            return;
        }
        if let Some(anchor) =
            ScrollAnchor::capture(&self.conv_scroll, usize::from(self.conv_note_slot))
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
        if self.session.pane != Pane::Conversation {
            self.reader_entered_conversation();
        }
        self.session.pane = Pane::Conversation;
        self.session.message_cursor = index;
    }

    fn search_conversation_live(&self) -> bool {
        !self.session.search_editing() && self.search_preview_current()
    }

    fn note_in_view(&self) -> bool {
        self.session
            .note()
            .is_some_and(|note| self.note_view.visible(note.id))
    }

    fn search_preview_current(&self) -> bool {
        self.session.search.as_ref().is_none_or(|search| {
            self.open_thread == self.session.cursor_id
                && search.matches_thread(self.session.cursor_id.as_deref())
        })
    }

    fn reader_took_over(&mut self, selected: bool) {
        self.reveal = Reveal::Reading { selected };
        self.note_visit = None;
        self.note_pin.release_reader();
        self.instant_conversation_scroll = false;
        self.release_landing();
    }

    fn message_selected(&self) -> bool {
        self.reveal != (Reveal::Reading { selected: false })
    }

    fn invalidate_conversation_layout(&mut self) {
        self.body_refresh = BodyRefresh::default();
        self.note_visit = None;
        self.instant_conversation_scroll = false;
        self.sent_arrival_anchor = None;
        self.preview_padding = false;
        self.preview_laid_out = false;
        if self.reveal == Reveal::Preview {
            self.reveal = Reveal::Waiting;
        }
        self.release_landing();
        self.conv_layout_current = false;
        self.reveal.cancel_pending();
    }

    fn apply_body_refresh(&mut self, now: Instant) -> Option<ScrollAnchor> {
        // Let a keyboard glide finish in the geometry it was calculated from.
        if self.motion.conv_glide.is_some() {
            return None;
        }
        if self.body_refresh.is_empty() && !self.motion.has_body_growth() {
            return None;
        }
        let refresh = std::mem::take(&mut self.body_refresh);
        if !refresh.is_empty() {
            self.conversation_revision = self.conversation_revision.wrapping_add(1);
        }
        if !refresh.rich.is_empty() && self.body_landing.is_none() {
            self.reveal.body_changed(false);
        }
        if self.reveal.pending() {
            self.release_landing();
        }
        let anchor = if self.conv_layout_current && self.reveal != Reveal::Preview {
            self.reveal.body_anchor(
                &self.conv_scroll,
                self.session.message_cursor,
                usize::from(self.conv_note_slot),
            )
        } else {
            None
        };
        if refresh.is_empty() {
            return anchor;
        }
        let selected = self.session.message_cursor;
        let rich_arrived = self
            .session
            .messages
            .get(selected)
            .is_some_and(|message| refresh.rich.contains_key(&message.id));
        let layout_current = self.conv_layout_current;
        let scroll = &self.conv_scroll;
        let padded_last = self
            .preview_padding
            .then(|| self.session.messages.len().saturating_sub(1));
        let height = |index| {
            if !layout_current {
                return None;
            }
            let height = if padded_last == Some(index) {
                self.preview_card_height.get()
            } else {
                scroll
                    .bounds_for_item(index + usize::from(self.conv_note_slot))?
                    .size
                    .height
            };
            (height > px(0.)).then(|| f32::from(height))
        };
        let generation = refresh.messages.as_ref().map(|(_, generation)| *generation);
        let downloading = self.pending_bodies.downloading(&self.session.sync);
        refresh.apply(
            &mut self.session,
            &mut self.motion,
            now,
            height,
            downloading,
        );
        if let Some(generation) = generation {
            self.finish_body_refresh_at(generation, true, now);
        }
        if self.reveal.pending()
            && self.motion.body_growth_running(now)
            && layout_current
            && let Some(placeholder) = ScrollAnchor::capture_message(
                &self.conv_scroll,
                selected,
                usize::from(self.conv_note_slot),
            )
            && let Some(slide) = self.motion.motion.start(motion::PANE, now)
        {
            self.body_landing = Some((self.conv_scroll.bounds().top() + placeholder.y, slide));
            self.reveal.cancel_pending();
        }
        if rich_arrived && self.session.reveal_matching_quote(selected) {
            self.land_on_reader();
        }
        anchor
    }

    fn finish_body_refresh(&mut self, generation: u64, crossfade: bool) {
        let now = self.motion.clock.now();
        self.finish_body_refresh_at(generation, crossfade, now);
    }

    fn finish_body_refresh_at(&mut self, generation: u64, crossfade: bool, now: Instant) {
        let animate = crossfade && self.has_missing_bodies();
        self.pending_bodies
            .applied(generation, animate, self.motion.motion, now);
    }

    fn settle_pending_bodies(&mut self) {
        let animate = self.has_missing_bodies();
        self.pending_bodies
            .settle(animate, self.motion.motion, self.motion.clock.now());
    }

    fn has_missing_bodies(&self) -> bool {
        !matches!(self.session.sync, SyncKind::Syncing { .. })
            && present::awaiting_bodies(&self.session.messages)
    }

    /// Once the new bodies are laid out, glide to the reader's message
    /// if it is clipped. The first-run preview follows its growing cards
    /// directly through its per-frame landing.
    fn reveal_reader(&mut self, window: &mut Window) {
        match self.reveal {
            Reveal::Waiting
            | Reveal::Preview
            | Reveal::Following(RevealPhase::Idle)
            | Reveal::Reading { .. } => {}
            Reveal::Following(RevealPhase::Laying) => {
                self.reveal = Reveal::Following(RevealPhase::Ready);
                window.request_animation_frame();
            }
            Reveal::Following(RevealPhase::Ready) => {
                self.reveal = Reveal::Following(RevealPhase::Idle);
                let Some(item) = self.conv_scroll.bounds_for_item(
                    self.session.message_cursor + usize::from(self.conv_note_slot),
                ) else {
                    return;
                };
                let view = self.conv_scroll.bounds();
                let offset = f32::from(self.conv_scroll.offset().y);
                let max = f32::from(self.conv_scroll.max_offset().height);
                if let Some(to) = present::offset_to_show_message(
                    (item.top().into(), item.bottom().into()),
                    (view.top().into(), view.bottom().into()),
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

    /// Hold the reader's message using each frame's own layout bounds.
    fn land_on_reader(&mut self) {
        self.body_landing = None;
        self.landing.hold(
            self.session
                .messages
                .get(self.session.message_cursor)
                .map(|message| message.id.as_str()),
        );
    }

    fn land_on_first_preview(&mut self) {
        self.preview_padding = !self.session.messages.is_empty();
        if self.preview_padding {
            self.reveal = Reveal::Preview;
            self.session.message_cursor = self.session.messages.len() - 1;
            self.land_on_reader();
        }
    }

    /// A narrow inbox can receive its first preview while the reader is
    /// hidden. Opening that pane still gets its initial landing after the
    /// key takes over, including the frame that learns its viewport height.
    fn first_preview_layout(&mut self, visible: bool) -> Option<ScrollAnchor> {
        if !visible || !self.preview_padding || self.preview_laid_out {
            return None;
        }
        self.preview_laid_out = self.conv_scroll.bounds().size.height > px(0.);
        Some(ScrollAnchor::landing(&self.session, self.reveal))
    }

    /// Hold this message until the reader scrolls or moves to another one.
    fn hold_landing(&mut self) -> Option<ScrollAnchor> {
        let index = self.session.message_cursor;
        let current = self
            .session
            .messages
            .get(index)
            .map(|message| message.id.as_str());
        self.landing.anchor(
            current,
            ScrollAnchor::landing(&self.session, self.reveal),
            self.motion.conv_glide.is_some(),
        )
    }

    /// The reader took over: stop holding their message in place.
    fn release_landing(&mut self) {
        self.body_landing = None;
        self.landing.hold(None);
    }

    fn growing_body_landing(&mut self, now: Instant) -> Option<ScrollAnchor> {
        let (from, slide) = self.body_landing?;
        let moving = slide.running(now);
        if !moving {
            self.body_landing = None;
        }
        let mut anchor = ScrollAnchor::landing(&self.session, self.reveal);
        anchor.snap_headers = false;
        if moving {
            anchor.body_scroll = Some((from, motion::ease_out_quad(slide.linear(now))));
        }
        Some(anchor)
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
            .then(|| {
                self.conv_scroll
                    .bounds_for_item(index + usize::from(self.conv_note_slot))
            })
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
        let reach = f32::from(self.conv_scroll.bounds().size.height) * motion::GLIDE_MAX_VIEWPORTS;
        self.glide_conversation_for(offset, to, motion::SLIDE, reach);
    }

    fn glide_conversation_for(&mut self, offset: f32, to: f32, duration: Duration, reach: f32) {
        let from = self.motion.glide_from_conversation(offset);
        let x = self.conv_scroll.offset().x;
        self.conv_scroll.set_offset(point(x, px(to)));
        let now = self.motion.clock.now();
        self.motion.conv_glide = Glide::start(&self.motion.motion, from, to, reach, duration, now)
            .map(|glide| glide.gentle_beyond(LONG_GLIDE_PX));
        if let Some(glide) = self.motion.conv_glide {
            self.conv_scroll.set_offset(point(x, px(glide.from)));
        } else {
            self.instant_conversation_scroll = from != to;
        }
    }

    fn nudge_conversation(&mut self, pages: i32, lines: i32) {
        let viewport = f32::from(self.conv_scroll.bounds().size.height);
        let viewport = if viewport < 1.0 { 640.0 } else { viewport };
        let delta = pages as f32 * viewport + lines as f32 * 24.0;
        let measured = self.conv_scroll.offset();
        let scrollport_top = f32::from(self.conv_scroll.bounds().top());
        let mut item_bounds = Vec::new();
        let mut index = usize::from(self.conv_note_slot);
        while let Some(bounds) = self.conv_scroll.bounds_for_item(index) {
            item_bounds.push((f32::from(bounds.top()), f32::from(bounds.bottom())));
            index += 1;
        }
        let max = f32::from(self.conv_scroll.max_offset().height);
        let measured_y = f32::from(measured.y);
        let mut next_y = (measured_y - delta).clamp(-max, 0.0);
        let top = present::message_after_page(&item_bounds, scrollport_top, measured_y, next_y)
            .filter(|index| *index < self.session.messages.len());
        if pages > 0
            && next_y < 0.
            && self.note_band == px(0.)
            && self.note_pin.matches(self.session.note())
            && let Some(index) = top
            && !self.session.messages[index].expanded
        {
            // Put the first page's destination header below the band.
            // A page into an open body keeps its offset.
            next_y = present::offset_to_message_top(item_bounds[index].0, scrollport_top, max);
        }
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
        }
        self.session.cursor_id = Some(id);
        self.session.pane = Pane::List;
        if let Some(search) = self.session.search.as_mut() {
            search.editing = false;
            self.session.remember_search();
        }
        if changed || self.first_preview != FirstPreview::Off {
            self.request_messages(true, cx);
        }
        self.scroll_list_to_cursor();
    }
}

impl Render for Inbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.track_inbox_visibility(window);
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
        let scrolling_to_target = self.motion.conv_glide.is_some()
            || std::mem::take(&mut self.instant_conversation_scroll);
        self.advance_glides(now, window.scale_factor());
        let refreshing_body = !self.body_refresh.is_empty();
        let anchor = self
            .sent_arrival_anchor
            .take()
            .or_else(|| self.apply_body_refresh(now));
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
        self.motion.narrow_status = width < present::NARROW_WIDTH;
        let composing = compose_open(cx);
        let viewport = (window.viewport_size(), window.scale_factor());
        let waiting_for_resize = self.motion.waiting_for_uncover(viewport.0, composing, now);
        let uncovered = panes.1 && !composing && !waiting_for_resize;
        if let Some(frame) = self
            .motion
            .stage_arriving_messages(uncovered.then_some(viewport))
        {
            let this = cx.weak_entity();
            // GPUI runs next-frame callbacks before submitting the scene. Let
            // this callback present the pinned frame and start the clock on
            // its callback, provided no configure replaced that scene.
            window.on_next_frame(move |window, _| {
                window.on_next_frame(move |window, cx| {
                    if !compose_open(cx) {
                        let viewport = (window.viewport_size(), window.scale_factor());
                        let _ = this.update(cx, |inbox, cx| {
                            let now = inbox.motion.clock.now();
                            if inbox.motion.reveal_arriving_messages(viewport, frame, now) {
                                cx.notify();
                            }
                        });
                    }
                });
            });
        }
        self.motion.observe(&self.session, panes, now);
        let reflow = self.motion.reading_reflow(now);
        let list_width = list_width(width, self.split);
        let split_width = if panes.1 && width >= present::NARROW_WIDTH {
            list_width + SPLITTER_PX
        } else {
            0.0
        };
        let reading = self.motion.reading_progress(now);
        let scale = window.scale_factor();
        let reader = ReaderLayout {
            columns: ReaderColumns::new(
                width,
                split_width,
                self.motion.reading_target(),
                reading,
                scale,
            )
            .with_reflow(reflow),
            left: ReaderColumns::pane_left(split_width, reading, scale),
            focus: self.motion.focus_edge(self.session.pane, now),
            message_cursor: self.session.message_cursor,
        };
        self.painted_reader = panes.1.then_some(reader);
        let reveal_attachment = if reflow.is_some() && reveal_attachment {
            self.reveal_attachment = true;
            false
        } else {
            reveal_attachment
        };
        let waiting_for_total = self.waiting_for_inbox_total
            && self.session.roles.is(FolderRole::Inbox, &self.list_folder);
        let unread = (!self.waiting_for_first_rows && !waiting_for_total)
            .then(|| self.session.unread_threads());
        self.motion
            .observe_header(&self.list_folder, self.list_label.as_deref(), unread, now);
        let banner = self
            .session
            .search
            .is_none()
            .then(|| {
                self.session
                    .list_banner(&self.list_folder, self.list_label.as_deref())
            })
            .flatten();
        let banner = self.motion.list_banner(banner, now);
        let exit = self.motion.empty_list_exit(now);
        if exit == 0.0 {
            self.leaving_reader = None;
            self.leaving_rows = None;
        }
        if !panes.0 {
            self.painted_label_rows.clear();
        }
        let visible_overlay = self.session.visible_overlay();
        let picker_choices = if visible_overlay == Overlay::Folders {
            self.session.picker_choices()
        } else {
            Vec::new()
        };
        self.motion.fit_card(
            card_fit(&self.session, &picker_choices, &self.theme, window),
            now,
        );
        if self.body_refresh.is_empty() && !self.motion.body_growth_running(now) {
            self.reveal_reader(window);
        }
        let layout_anchor = || {
            if (previous_reading != self.motion.reading_target() || self.motion.reading_sliding())
                && self.conv_layout_current
                && self.motion.conv_glide.is_none()
            {
                ScrollAnchor::capture(&self.conv_scroll, usize::from(self.conv_note_slot))
            } else {
                None
            }
        };
        let anchor = anchor.or_else(layout_anchor);
        let note = self
            .session
            .agent_notes
            .current(self.open_thread.as_deref());
        let note_moving = self.note_view.observe(
            self.open_thread.as_deref(),
            note,
            self.session.agent_notes.collapsed,
            &self.motion.motion,
            now,
        );
        let offset = clamped_offset(&self.conv_scroll);
        let note_anchor = (note_moving
            && self.conv_layout_current
            && self.motion.conv_glide.is_none()
            && offset < px(-0.5))
        .then(|| ScrollAnchor::capture(&self.conv_scroll, usize::from(self.conv_note_slot)))
        .flatten();
        let selected = (self.note_pin.arriving()
            && self.conv_layout_current
            && !scrolling_to_target
            && self.message_selected())
        .then(|| {
            ScrollAnchor::capture_message(
                &self.conv_scroll,
                self.session.message_cursor,
                usize::from(self.conv_note_slot),
            )
        })
        .flatten();
        let agent_note = self.note_view.frame(now);
        self.conv_note_slot = agent_note.is_some() && !self.session.messages.is_empty();
        let pinnable = note.filter(|_| self.conv_note_slot && self.search_conversation_live());
        let visiting = self
            .note_visit
            .as_ref()
            .is_some_and(|visit| note.is_some_and(|note| note.id == visit.note));
        let target_band = if panes.1 {
            self.note_pin.band(
                pinnable,
                visiting,
                self.conv_layout_current.then_some(offset),
                self.theme.ui_px,
            )
        } else {
            px(0.)
        };
        let pin = self.note_pin.paint(
            target_band,
            if panes.1 { self.note_band } else { px(0.) },
            agent_note.as_ref(),
            &self.motion.motion,
            now,
        );
        let band = pin.map_or(px(0.), |pin| pin.height);
        let band_delta = band - self.note_band;
        self.note_band = band;
        let verify_pin = panes.1 && !self.conv_layout_current && self.note_pin.matches(pinnable);
        if band_delta != px(0.)
            && (band_delta < px(0.) || !scrolling_to_target || self.motion.conv_glide.is_some())
        {
            // Preserve window Y while the scrollport moves under the band.
            let mut offset = self.conv_scroll.offset();
            offset.y = if let Some(glide) = &mut self.motion.conv_glide {
                px(glide.move_origin(now, f32::from(offset.y), f32::from(band_delta)))
            } else {
                offset.y - band_delta
            };
            self.conv_scroll.set_offset(offset);
        }
        let arrival_anchor = self
            .note_pin
            .arrival_anchor(target_band, band, selected, now);
        if arrival_anchor.is_some() || agent_note.as_ref().is_some_and(|frame| frame.leaving) {
            // Passive note motion releases the held alignment. A growing
            // body already compensates for the moving viewport.
            self.landing.hold(None);
        }
        let returning = self.note_return(now);
        let viewing_note = visiting && returning.is_none();
        let preview_padding = self.preview_padding.then(|| PreviewPadding {
            // The previous scroll bounds can still belong to a different
            // thread, or be empty when a narrow reader has never opened.
            // Start with an upper bound and trim the slack after layout.
            viewport: if self.preview_laid_out {
                self.conv_scroll.bounds().size.height
            } else {
                window.viewport_size().height
            },
            newest_height: self.preview_card_height.clone(),
        });
        let first_preview_layout = self.first_preview_layout(panes.1);
        let anchor = if viewing_note {
            // Keep the note's top while message bodies finish loading.
            if self.motion.conv_glide.is_none() {
                self.conv_scroll
                    .set_offset(point(self.conv_scroll.offset().x, px(0.)));
            }
            None
        } else {
            self.growing_body_landing(now)
                .or(arrival_anchor)
                .or_else(|| self.hold_landing())
                .or(first_preview_layout)
                .or_else(|| {
                    anchor.or(note_anchor).map(|mut anchor| {
                        anchor.y -= band_delta;
                        anchor
                    })
                })
        };
        let note_details_only = self.note_visit.as_ref().is_some_and(|visit| {
            agent_note
                .as_ref()
                .is_some_and(|frame| frame.note.id == visit.note)
        });
        let setup_frame = self
            .setup_exit
            .as_ref()
            .and_then(|(_, swap)| swap.frame(now));
        if setup_frame.is_none() {
            self.setup_exit = None;
            if self.first_preview.finish_handoff(real) {
                cx.spawn(async |this, cx| {
                    cx.background_executor().timer(FIRST_PREVIEW_WAIT).await;
                    let _ = this.update(cx, |inbox, cx| {
                        if inbox.first_preview.expire(Instant::now()) {
                            inbox.read_selected_messages(true, cx);
                        }
                    });
                })
                .detach();
            }
        }
        let preview_current = self.search_preview_current();
        let conv = ConvFrame {
            syncing: self.pending_bodies.downloading(&self.session.sync),
            missing_fade: self.pending_bodies.fade(now),
            agent_note,
            pin,
            note_details_only,
            note_expanded_in_band: !self.session.agent_notes.collapsed,
            hide_message_cursor: self.note_pin.eligible() && !self.message_selected(),
            return_glide: returning.map(|(_, glide)| glide),
            card_alphas: self.motion.card_alphas(now),
            alphas: self.motion.body_alphas(now),
            heights: self.motion.body_heights(now),
            outgoing_placeholders: self.motion.body_placeholders(),
            preview_padding,
            disclosures: self.motion.disclosures(&self.session, now),
            anchor: returning.map(|(anchor, _)| anchor).or(anchor),
            frames: self.frames.clone(),
            attachment_download: self.attachment_download.clone(),
            reveal_attachment,
            spin_attachment,
            preview_current,
            quiet_preview: self.quiet_search_preview(),
            match_line: self.search_match_frame(now, preview_current),
            leaving: self.leaving_reader.clone().map(|reader| (reader, exit)),
        };
        self.conv_layout_current = panes.1;
        let agents_animating = self.agents_motion.observe(
            (self.session.overlay == Overlay::Agents).then_some(&self.session.agents),
            &self.motion.motion,
            now,
        );
        let reply_check = cx.try_global::<ReplyChecks>().and_then(|checks| {
            let selected = self.session.reply_target().filter(|_| {
                self.open_thread.is_some() && self.open_thread == self.session.cursor_id
            });
            selected.and_then(|target| checks.for_message(&target.id))
        });
        let status =
            if self.opening_draft.as_ref().is_some_and(|open| {
                real.saturating_duration_since(open.started) >= DRAFT_OPEN_FEEDBACK
            }) {
                StatusLabel::opening_draft(&self.theme)
            } else {
                StatusLabel::new(&self.theme, &self.session, reply_check, width)
            };
        let status_labels = self.status_labels.update(status, self.motion.motion, now);
        let animating = self.motion.tick(now)
            | waiting_for_resize
            | self.note_view.running()
            | self.note_pin.running()
            | self
                .note_visit
                .as_ref()
                .is_some_and(|visit| visit.returning.is_some())
            | verify_pin
            | setup_frame.is_some()
            | status_labels.outgoing.is_some()
            | status_labels.count_fade.is_some()
            | self.body_landing.is_some()
            | conv.missing_fade.is_some()
            | conv.match_line.animating
            | self.row_widths.running(now)
            | spin_attachment
            | agents_animating
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
        let viewport_height = f32::from(window.viewport_size().height);
        let mut overlay_card =
            |session: &Session, motion, live, picker: (&[PickerChoice], Option<String>, f32)| {
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
                    picker,
                    help,
                    (session.overlay == Overlay::Agents).then(|| {
                        agents::card(
                            &theme,
                            &session.agents,
                            live.then_some(&self.agents_scroll),
                            viewport_height - CARD_TOP_PX - 16.0,
                            live.then_some((&self.agents_motion, now)),
                            cx,
                        )
                    }),
                    window,
                    cx,
                )
            };
        if let Some((layer, opacity)) = self.motion.closing_overlay(now) {
            layer.swap(&mut self.session);
            leaving.push(
                overlay_card(
                    &self.session,
                    (opacity, 0.0),
                    false,
                    (
                        &layer.picker_choices,
                        layer.label_target.clone(),
                        layer.label_marks,
                    ),
                )
                .into_any_element(),
            );
            layer.swap(&mut self.session);
        }
        let active_overlay = (visible_overlay != Overlay::None).then(|| {
            overlay_card(
                &self.session,
                self.motion.overlay(now),
                true,
                (
                    &picker_choices,
                    self.session.label_target(),
                    self.motion.label_marks(now),
                ),
            )
        });
        let warm = self.warm_layer(width, panes, animating, window, cx);
        let subject = self.shown_subject();
        let cached_preview = self.search_preview_for(panes, animating || refreshing_body, &subject);
        let cached_search_rows = self.search_rows_for(animating);
        let background = theme.background;
        let leaving_banner = self.motion.leaving_banner(now);
        let recipient_popup = self.search_recipient_popup(window, cx);
        let root = shell(
            theme,
            &self.session,
            &mut self.motion,
            now,
            Shown {
                subject,
                folder: self.list_folder.clone(),
                label: self.list_label.clone(),
                leaving,
                warm,
                active_overlay,
                conv,
                reader,
                cached_preview,
                list: ListContent {
                    banner,
                    leaving_banner,
                    leaving: self.leaving_rows.clone().map(|rows| (rows, exit)),
                    cached_results: cached_search_rows,
                },
                status_labels,
                recipient_popup,
            },
            width,
            (list_width, split_width),
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
        });
        let shell = || div().relative().size_full().bg(rgb(background));
        match (setup_frame, &self.setup_exit) {
            (Some(SwapFrame::Leaving(opacity)), Some((setup, _))) => {
                let layer = setup.update(cx, |setup, cx| setup.leaving_layer(window, cx));
                shell()
                    .child(root.opacity(0.))
                    .child(
                        layer
                            .absolute()
                            .inset_0()
                            .overflow_hidden()
                            .opacity(motion::lerp(motion::ARRIVE_FROM, 1., opacity)),
                    )
                    .into_any_element()
            }
            (Some(SwapFrame::Arriving(progress)), _) => shell()
                .child(root.opacity(motion::lerp(motion::ARRIVE_FROM, 1., progress)))
                .into_any_element(),
            _ => root.into_any_element(),
        }
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
    label: Option<String>,
    /// What the conversation's bodies animate this frame.
    conv: ConvFrame,
    reader: ReaderLayout,
    cached_preview: Option<(u64, gpui::AnyView)>,
    list: ListContent,
    status_labels: StatusLabelsFrame,
    recipient_popup: Option<gpui::Deferred>,
}

struct LeavingReader {
    subject: String,
    messages: Vec<ConvMessage>,
    offset: f32,
    layout: ReaderLayout,
    quotes: HashSet<String>,
    recipients: HashSet<String>,
    arrival: f32,
    alphas: HashMap<String, f32>,
    disclosures: Disclosures,
}

#[derive(Clone, Copy)]
struct ReaderLayout {
    columns: ReaderColumns,
    left: f32,
    focus: f32,
    message_cursor: usize,
}

struct LeavingRow {
    row: ThreadRow,
    index: usize,
    look: RowLook,
    top: f32,
    motion: RowMotion,
}

#[derive(Default)]
struct ListContent {
    banner: Option<(Banner, f32)>,
    leaving_banner: Option<(Banner, f32)>,
    leaving: Option<(Arc<[LeavingRow]>, f32)>,
    cached_results: Option<(u64, gpui::AnyView)>,
}

/// What the conversation's bodies show this frame beyond the session.
#[derive(Default)]
struct ConvFrame {
    agent_note: Option<agent_notes::NoteFrame>,
    pin: Option<agent_notes::PinnedFrame>,
    note_details_only: bool,
    note_expanded_in_band: bool,
    hide_message_cursor: bool,
    return_glide: Option<LandingGlide>,
    syncing: bool,
    missing_fade: Option<f32>,
    /// Bodies fading in, by message id: their opacity.
    alphas: HashMap<String, f32>,
    card_alphas: HashMap<String, f32>,
    heights: HashMap<String, (f32, f32)>,
    outgoing_placeholders: HashMap<String, bool>,
    preview_padding: Option<PreviewPadding>,
    disclosures: Disclosures,
    /// The message held at its previous viewport position, or at the top on open.
    anchor: Option<ScrollAnchor>,
    frames: Option<SharedFrameLog>,
    attachment_download: Option<(String, String, Download)>,
    reveal_attachment: bool,
    spin_attachment: bool,
    preview_current: bool,
    quiet_preview: bool,
    match_line: search::MatchLineFrame,
    leaving: Option<(Arc<LeavingReader>, f32)>,
}

impl ConvFrame {
    fn body_alpha(&self, id: &str) -> f32 {
        self.leaving
            .as_ref()
            .map_or(&self.alphas, |(reader, _)| &reader.alphas)
            .get(id)
            .copied()
            .unwrap_or(1.0)
    }

    fn disclosures(&self) -> &Disclosures {
        self.leaving
            .as_ref()
            .map_or(&self.disclosures, |(reader, _)| &reader.disclosures)
    }
}

struct PreviewPadding {
    viewport: gpui::Pixels,
    newest_height: Rc<Cell<gpui::Pixels>>,
}

impl PreviewPadding {
    fn wrap_newest(&self, card: gpui::AnyElement) -> gpui::AnyElement {
        let height = self.newest_height.clone();
        // The difference between the pane and the card becomes space after
        // the card in the same layout pass, even while the body is growing.
        div()
            .min_h(self.viewport)
            .child(card)
            .on_children_prepainted(move |bounds, _, _| height.set(bounds[0].size.height))
            .into_any_element()
    }
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
                        if inbox.reveal == Reveal::Preview && inbox.session.overlay == Overlay::None
                        {
                            inbox.reader_took_over(false);
                        } else if over_reader && inbox.session.overlay == Overlay::None {
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
    (list_width, split_width): (f32, f32),
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    focus: FocusHandle,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    let (show_list, show_conv) = present::visible_panes(width, session.pane, session.full_width);
    let beside = show_conv && width >= present::NARROW_WIDTH;
    let list_entry = motion.pane_entry(Pane::List, now);
    let conv_entry = motion.pane_entry(Pane::Conversation, now);
    let status = motion.status(session, now);
    let Shown {
        subject,
        folder,
        label,
        leaving,
        warm,
        active_overlay,
        conv,
        reader,
        cached_preview,
        list,
        status_labels,
        recipient_popup,
    } = shown;
    let frames = conv.frames.clone();
    let same_scope = session.search.is_none()
        && session.folder == folder
        && session.label_scope.as_deref() == label.as_deref();
    let empty_list_pane = motion.empty_list_pane(same_scope, now);
    let mut subject = Some(subject);
    let mut conv = conv;
    let departing = conv.leaving.take().map(|(reader, opacity)| {
        let layout = reader.layout;
        let frame = ConvFrame {
            leaving: Some((reader, opacity)),
            ..ConvFrame::default()
        };
        (layout, frame)
    });
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
            Pane::List => thread_list(
                &theme,
                session,
                false,
                width,
                list_scroll.clone(),
                ListContent {
                    banner: list.banner.clone(),
                    ..ListContent::default()
                },
                cx,
            ),
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
            None => header(
                &theme,
                session,
                (&folder, label.as_deref()),
                motion,
                now,
                width,
                window,
            ),
        })
        .children(recipient_popup)
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
                .when(show_list || reader.left > 0.0, |row| {
                    let list = thread_list(
                        &theme,
                        session,
                        beside,
                        if beside { list_width } else { width },
                        list_scroll,
                        list,
                        cx,
                    );
                    if beside {
                        row.child(
                            div()
                                .relative()
                                .flex_none()
                                .w(px(reader.left))
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
                                        .opacity(motion.reading_list_opacity(now) * empty_list_pane)
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
                        row.child(entering(list.opacity(empty_list_pane), list_entry))
                    }
                })
                .when(
                    show_conv && (departing.is_none() || empty_list_pane == 1.0),
                    |row| {
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
                                            session.search_results_current() && !conv.quiet_preview,
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
                                reader.columns,
                                Live(conv.preview_current && !session.search_editing()),
                                &conv,
                                window,
                                cx,
                            ),
                            conv_entry,
                        ))
                    },
                )
                .when_some(departing.as_ref(), |row, (layout, snapshot)| {
                    row.child(
                        div()
                            .id("leaving-reader")
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .right_0()
                            .left(px(layout.left))
                            .flex()
                            .child(conversation(
                                &theme,
                                session,
                                String::new(),
                                motion,
                                now,
                                None,
                                layout.columns,
                                Live(false),
                                snapshot,
                                window,
                                cx,
                            )),
                    )
                }),
        )
        .child(status_bar(
            &theme,
            session,
            status,
            status_labels,
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

#[derive(Clone, Copy)]
enum FolderFit<'a> {
    Header,
    Picker(&'a str),
    Search,
}

fn fit_folder_path<T>(
    path: &str,
    mode: FolderFit<'_>,
    clip: impl Fn(&str) -> Option<T>,
    text: impl Fn(&T) -> &str,
) -> Option<T> {
    let fit = |candidate: &str| clip(candidate).filter(|line| text(line) != "…");
    let full = fit(path);
    if full.as_ref().is_some_and(|line| text(line) == path) {
        return full;
    }
    let parents = path
        .split_once('/')
        .map(|(first, rest)| (first, rest.rsplit('/').next().unwrap_or(rest)));
    let leaf = parents.map_or(path, |(_, leaf)| leaf);
    let exact = |candidate: &str| fit(candidate).filter(|line| text(line) == candidate);
    let query = match mode {
        FolderFit::Picker(query) => query,
        _ => "",
    };
    let target = if folder_match(leaf, query).is_some() {
        leaf
    } else {
        path
    };
    let anchor = folder_match(target, query).map(|matched| folder_match_words(target, matched));
    let anchor_text = anchor
        .as_ref()
        .map(|words| target[words.clone()].to_ascii_lowercase());
    let keeps_match = |line: &T| {
        anchor_text
            .as_deref()
            .is_none_or(|words| folder_match(text(line), words).is_some())
    };
    if let Some((first, _)) = parents {
        let prefix = format!("{first}/…/");
        let shortened = format!("{prefix}{leaf}");
        if matches!(mode, FolderFit::Header) {
            return fit(&shortened)
                .filter(|line| {
                    text(line)
                        .strip_prefix(&prefix)
                        .is_some_and(|leaf| !leaf.is_empty() && leaf != "…")
                })
                .or_else(|| exact(&format!("…/{leaf}")))
                .or_else(|| clip(leaf));
        }
        if let Some(line) = exact(&shortened).filter(&keeps_match) {
            return Some(line);
        }
        let leaf_label = if matches!(mode, FolderFit::Search) {
            leaf.to_owned()
        } else {
            format!("…/{leaf}")
        };
        if let Some(line) = exact(&leaf_label).filter(&keeps_match) {
            return Some(line);
        }
    } else if matches!(mode, FolderFit::Header) {
        return full.or_else(|| clip(leaf));
    }

    let suffix = fit_folder_words("…", leaf, &exact).or_else(|| {
        let starts: Vec<_> = leaf
            .grapheme_indices(true)
            .map(|(index, _)| index)
            .collect();
        fit_folder_tail("…", leaf, &starts, &exact)
    });
    if let Some(line) = suffix.filter(&keeps_match) {
        return Some(line);
    }
    if let Some(words) = anchor {
        // The boundaries stay fixed while the query grows inside a word.
        let tail = &target[words.start..];
        let start = if words.start == 0 { "" } else { "…" };
        if let Some(line) = exact(&format!("{start}{tail}")) {
            return Some(line);
        }
        let after_match = target[words.end..].trim_start();
        let prefix = format!("{start}{} … ", &target[words]);
        if !after_match.is_empty()
            && let Some(line) = fit_folder_words(&prefix, after_match, &exact)
        {
            return Some(line);
        }
        let head = format!("{start}{tail}");
        if let Some(line) = fit(&head).filter(&keeps_match) {
            if let Some(kept) = text(&line).strip_suffix('…')
                && !head[kept.len()..].starts_with(folder_word_separator)
                && let Some(end) = kept.rfind(folder_word_separator)
                && let Some(whole) = exact(&format!("{}…", &kept[..end])).filter(&keeps_match)
            {
                return Some(whole);
            }
            return Some(line);
        }
        // A single matched word can itself be wider than the picker.
        if let Some(matched) = folder_match(tail, query) {
            let start = tail
                .grapheme_indices(true)
                .take_while(|&(index, _)| index <= matched.start)
                .last()
                .map_or(0, |(index, _)| index);
            if let Some(line) = fit(&format!("…{}", &tail[start..]))
                .filter(|line| folder_match(text(line), query).is_some())
            {
                return Some(line);
            }
        }
    }
    match mode {
        FolderFit::Picker(_) => clip(leaf),
        _ => None,
    }
}

fn folder_match(label: &str, query: &str) -> Option<std::ops::Range<usize>> {
    if query.is_empty() {
        return None;
    }
    let start = label.to_ascii_lowercase().find(query)?;
    Some(start..start + query.len())
}

fn folder_match_words(label: &str, matched: std::ops::Range<usize>) -> std::ops::Range<usize> {
    let start = label[..matched.start]
        .rfind(folder_word_separator)
        .map_or(0, |index| {
            index + label[index..].chars().next().unwrap().len_utf8()
        });
    let end = label[matched.end..]
        .find(folder_word_separator)
        .map_or(label.len(), |index| matched.end + index);
    start..end
}

fn folder_word_separator(ch: char) -> bool {
    ch.is_whitespace() || ch == '/'
}

fn fit_folder_words<T>(prefix: &str, tail: &str, exact: &impl Fn(&str) -> Option<T>) -> Option<T> {
    let mut starts = vec![0];
    starts.extend(
        tail.match_indices(char::is_whitespace)
            .filter_map(|(index, space)| {
                let start = index + space.len();
                tail[start..]
                    .chars()
                    .next()
                    .filter(|ch| !ch.is_whitespace())
                    .map(|_| start)
            }),
    );
    fit_folder_tail(prefix, tail, &starts, exact)
}

fn fit_folder_tail<T>(
    prefix: &str,
    tail: &str,
    starts: &[usize],
    exact: &impl Fn(&str) -> Option<T>,
) -> Option<T> {
    let first =
        starts.partition_point(|&start| exact(&format!("{prefix}{}", &tail[start..])).is_none());
    exact(&format!("{prefix}{}", &tail[*starts.get(first)?..]))
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
    scope: (&str, Option<&str>),
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
    let figures = tabular_style(window, theme, theme.ui_px);
    let count_text = |unread| format!("{} unread", grouped(unread));
    let count_width =
        |text: &str| line_px_with_style(window, &figures, text, FontWeight::NORMAL).ceil();
    let count = |unread: usize, room: f32| {
        let text = count_text(unread);
        let required = count_width(&text);
        div()
            .flex_none()
            .whitespace_nowrap()
            .text_color(rgb(if unread > 0 {
                theme.accent
            } else {
                theme.dimmed
            }))
            .font(figures.font())
            .when(required > room, |count| {
                count.text_size(px(theme.ui_px * room / required))
            })
            .child(text)
    };
    let folder_group = |(folder, label): (&str, Option<&str>),
                        unread: Option<usize>,
                        count_fade: Option<(Option<usize>, f32)>| {
        let drafts = label.is_none() && session.roles.is(FolderRole::Drafts, folder);
        let (old, shown) = count_fade.unwrap_or((None, 1.));
        let inbox = label.is_none() && session.roles.is(FolderRole::Inbox, folder);
        let measure = |unread| count_width(&count_text(unread));
        let count_room = if inbox {
            inbox_count_room(count_width)
        } else {
            measure(888)
                .max(unread.map_or(0., measure))
                .max(old.map_or(0., measure))
        };
        let count_space = if drafts { 0. } else { COUNT_GAP + count_room };
        let name_room = (room - ACCOUNT_MIN - count_space).max(0.);
        let title = label.map_or_else(|| session.roles.label(folder), present::display_safe);
        let name = if label.is_some() {
            fit(&title, name_room, FontWeight::SEMIBOLD)
        } else {
            fit_folder_path(
                &title,
                FolderFit::Header,
                |path| {
                    clip_shaped(
                        window,
                        &style,
                        path,
                        name_room.floor(),
                        FontWeight::SEMIBOLD,
                        None,
                    )
                },
                |line| &line.text,
            )
        };
        let name_width = name
            .as_ref()
            .map_or(0., |line| f32::from(line.width).ceil());
        let detail = label
            .map(|_| "Label".into())
            .or_else(|| session.folder_detail(folder))
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
                        #[cfg(test)]
                        let text = line.text.clone();
                        let label = div()
                            .flex_none()
                            .whitespace_nowrap()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(line.text);
                        #[cfg(test)]
                        let label = label.when(inbox, |label| {
                            label.on_children_prepainted(move |bounds, _, _| {
                                tests::INBOX_LABEL_X.with_borrow_mut(|positions| {
                                    positions.push((bounds[0].left(), text.clone()));
                                });
                            })
                        });
                        label
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
                        .when(!inbox, |count| count.justify_end())
                        .w(px(count_room))
                        .children(unread.map(|unread| count(unread, count_room).opacity(shown)))
                        .children(old.map(|old| {
                            count(old, count_room)
                                .absolute()
                                .when(inbox, |count| count.left_0())
                                .when(!inbox, |count| count.right_0())
                                .top_0()
                                .opacity(1. - shown)
                        })),
                )
            });
        (group, name_width + detail_space + count_space)
    };

    let (current, current_width) =
        folder_group(scope, motion.header_unread(), motion.unread_count(now));
    let label_header_opacity = motion.label_header_opacity(now);
    let fade = motion.folder_header();
    let leaving = fade
        .as_ref()
        .map(|fade| folder_group((&fade.folder, None), fade.unread, None));
    let old_width = leaving.as_ref().map_or(current_width, |(_, width)| *width);
    let (outgoing, incoming) = fade
        .as_ref()
        .map_or((0., label_header_opacity), |fade| fade.label_opacities(now));
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

/// Keep the Inbox name fixed. Six digits cover large mailboxes at the
/// normal size, and longer labels fit within the same room.
fn inbox_count_room(measure: impl FnOnce(&str) -> f32) -> f32 {
    measure("888,888 unread")
}

#[allow(clippy::too_many_arguments)]
fn thread_list(
    theme: &Theme,
    session: &Session,
    beside: bool,
    list_width: f32,
    scroll: UniformListScrollHandle,
    content: ListContent,
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
            pane.child(search::empty(search))
        } else if let Some((revision, cached)) = content.cached_results {
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
        return pane
            .relative()
            .children(
                content
                    .banner
                    .map(|(banner, opacity)| empty_state(theme, banner).opacity(opacity)),
            )
            .children(content.leaving_banner.map(|(banner, opacity)| {
                empty_state(theme, banner)
                    .absolute()
                    .inset_0()
                    .opacity(opacity)
            }))
            .when_some(content.leaving, |pane, (rows, opacity)| {
                pane.child(leaving_label_rows(
                    theme.clone(),
                    rows,
                    opacity,
                    row_text_width(list_width),
                    cx,
                ))
            });
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
                let viewport = inbox.list_viewport().unwrap_or(0.0);
                let last = inbox.session.threads.len().saturating_sub(1);
                let bar_dx = unread_bar_dx(inbox.motion.pane_entry(Pane::List, now).dx);
                // The list also asks for row 0 alone to measure a row's
                // height; only the rows it draws count as on screen.
                let drawn = range.len() > 1 || inbox.session.threads.len() == 1;
                let capture = drawn && inbox.list_label.is_some();
                let mut painted = Vec::new();
                if drawn {
                    let ids = inbox.session.threads.iter().map(|row| row.id.as_str());
                    inbox.row_widths.keep_drawn(range.clone(), ids);
                }
                let visible = range.clone();
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
                            label_scope: inbox.list_label.clone(),
                            unread_note: inbox.session.agent_notes.unread.contains(&row.id),
                        };
                        let element =
                            thread_row(&theme_for_rows, &row, index, &look, text_width, window, cx);
                        let top = index as f32 * ROW_PX + offset;
                        let (moving, ghosts, overflow) = if let Some(shift) =
                            inbox.motion.list.as_mut()
                        {
                            let below = inbox.session.threads[index + 1..]
                                .iter()
                                .enumerate()
                                .map(|(i, next)| (next.id.as_str(), top + (i + 1) as f32 * ROW_PX));
                            let mut moving = shift.row(&row.id, top, ROW_PX, below, now);
                            moving.dy = motion::snap(moving.dy, window.scale_factor());
                            let reach = if index == last { f32::INFINITY } else { ROW_PX };
                            // Ghosts are kept in content pixels, so they scroll.
                            let ghosts = shift.ghosts_in(index as f32 * ROW_PX, reach, now);
                            // GPUI only asks for in-view slots. Paint rows still
                            // moving across an edge from the nearest drawn slot.
                            let extra = shift.overscan(ROW_PX);
                            let above = (drawn && index == visible.start)
                                .then(|| visible.start.saturating_sub(extra)..visible.start);
                            let below = (drawn && index + 1 == visible.end)
                                .then(|| visible.end..(visible.end + extra).min(last + 1));
                            let overflow = above
                                .into_iter()
                                .chain(below)
                                .flatten()
                                .filter_map(|outside| {
                                    let row = &inbox.session.threads[outside];
                                    let slot = outside as f32 * ROW_PX + offset;
                                    let painted = shift
                                        .visible_overflow(&row.id, slot, ROW_PX, viewport, now)?;
                                    let dy = motion::snap(painted - top, window.scale_factor());
                                    let look = RowLook {
                                        selected: highlighted.contains(&row.id),
                                        fading: None,
                                        ghost: true,
                                        bar_dx,
                                        frame: still_frame(row, window),
                                        previous_labels: None,
                                        label_scope: inbox.list_label.clone(),
                                        unread_note: inbox
                                            .session
                                            .agent_notes
                                            .unread
                                            .contains(&row.id),
                                    };
                                    Some(slot_layer(dy, 1.0).child(thread_row(
                                        &theme_for_rows,
                                        row,
                                        outside,
                                        &look,
                                        text_width,
                                        window,
                                        cx,
                                    )))
                                })
                                .collect::<Vec<_>>();
                            (moving, ghosts, overflow)
                        } else {
                            (RowMotion::STILL, Vec::new(), Vec::new())
                        };
                        if capture {
                            painted.push(LeavingRow {
                                row,
                                index,
                                look,
                                top,
                                motion: moving,
                            });
                        }
                        if moving == RowMotion::STILL && ghosts.is_empty() && overflow.is_empty() {
                            return element;
                        }
                        let ghosts: Vec<_> = ghosts
                            .into_iter()
                            .map(|ghost| {
                                ghost_row(
                                    &theme_for_rows,
                                    ghost,
                                    index,
                                    text_width,
                                    (bar_dx, inbox.list_label.clone()),
                                    window,
                                    cx,
                                )
                            })
                            .collect();
                        let layer = moving_row_layer(
                            0.0,
                            moving,
                            selected.then_some(theme_for_rows.selection),
                            element,
                            window,
                        );
                        div()
                            .id(gpui::ElementId::named_usize("thread-slot", index))
                            .relative()
                            .h(px(ROW_PX))
                            .w_full()
                            .children(ghosts)
                            .child(layer)
                            .children(overflow)
                            .into_any_element()
                    })
                    .collect();
                if capture {
                    inbox.painted_label_rows = painted;
                }
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

/// Shape the leaving rows during list prepaint, where the same font and
/// text size are inherited as in the uniform list.
fn leaving_label_rows(
    theme: Theme,
    rows: Arc<[LeavingRow]>,
    opacity: f32,
    text_width: f32,
    cx: &mut Context<Inbox>,
) -> impl IntoElement {
    let entity = cx.entity();
    gpui::canvas(
        move |bounds, window, app| {
            let mut layer = entity.update(app, |_, cx| {
                div()
                    .size_full()
                    .relative()
                    .opacity(opacity)
                    .children(rows.iter().map(|frame| {
                        let look = RowLook {
                            ghost: true,
                            ..frame.look.clone()
                        };
                        let element = thread_row(
                            &theme,
                            &frame.row,
                            frame.index,
                            &look,
                            text_width,
                            window,
                            cx,
                        );
                        moving_row_layer(
                            frame.top,
                            frame.motion,
                            look.selected.then_some(theme.selection),
                            element,
                            window,
                        )
                    }))
                    .into_any_element()
            });
            layer.layout_as_root(
                size(
                    AvailableSpace::Definite(bounds.size.width),
                    AvailableSpace::Definite(bounds.size.height),
                ),
                window,
                app,
            );
            layer.prepaint_at(bounds.origin, window, app);
            layer
        },
        |_, mut layer, window, app| layer.paint(window, app),
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
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

fn moving_row_layer(
    top: f32,
    motion: RowMotion,
    selection: Option<u32>,
    element: gpui::AnyElement,
    window: &Window,
) -> gpui::Div {
    if motion.reveal.is_none() {
        return slot_layer(top + motion.dy, motion.opacity).child(element);
    }
    // The selected slot is highlighted as soon as it opens. Its content
    // fades in on top while the rows below slide down.
    let shown = motion.visible_height(ROW_PX, window.scale_factor());
    slot_layer(top + motion.dy, 1.0)
        .h(px(shown))
        .overflow_hidden()
        .when_some(selection, |layer, fill| layer.bg(rgb(fill)))
        .child(slot_layer(0.0, motion.opacity).child(element))
}

/// Paint a row that left, at its last position and with no input handlers.
fn ghost_row(
    theme: &Theme,
    (row, top, opacity): (ThreadRow, f32, f32),
    index: usize,
    text_width: f32,
    (bar_dx, label_scope): (f32, Option<String>),
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let look = RowLook {
        selected: false,
        fading: None,
        ghost: true,
        bar_dx,
        frame: still_frame(&row, window),
        previous_labels: None,
        label_scope,
        unread_note: false,
    };
    slot_layer(motion::snap(top, window.scale_factor()), opacity)
        .child(thread_row(
            theme, &row, index, &look, text_width, window, cx,
        ))
        .into_any_element()
}

/// A row drawn as it is, with nothing moving: a row that left the list,
/// or one shaped ahead of time.
fn still_frame(row: &ThreadRow, window: &Window) -> RowFrame {
    let natural = line_px(window, &row.subject, row_weight(row.unread));
    RowFrame::still(natural, row.unread, row.starred)
}

/// The cursors, folder and search state before a key ([`Inbox::before_step`]).
struct StepBefore {
    message: usize,
    thread: Option<String>,
    folder: String,
    label: Option<String>,
    pane: Pane,
    searching: bool,
    search_generation: u64,
    help_open: bool,
}

/// How a thread row is drawn.
#[derive(Clone)]
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
    label_scope: Option<String>,
    unread_note: bool,
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
    look: &RowLook,
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
                    look,
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
    look: &RowLook,
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
        - if look.unread_note {
            agent_notes::MARKER_SLOT
        } else {
            0.0
        }
        - 2.0 * THREAD_LINE_GAP)
        .max(0.0);
    let labels = if row.labels.is_empty() && look.previous_labels.is_none() {
        None
    } else {
        thread_label_group(
            theme,
            row,
            background,
            dim,
            // Leave enough for the sender without stranding space on rows
            // whose names already fit in a short span.
            (room - (room * 0.4).min(80.0) - THREAD_LINE_GAP).max(0.0),
            Some(look),
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
        .when(look.unread_note, |line| {
            line.child(agent_notes::marker(theme, selected).mr(px(-THREAD_LINE_GAP)))
        })
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
    look: Option<&RowLook>,
    window: &Window,
) -> Option<(gpui::Div, f32)> {
    let label_scope = look.and_then(|look| look.label_scope.as_deref());
    let style = conversation_line_style(window, theme, px((theme.ui_px - 2.0).max(11.0)));
    let outline = theme.label_outline(background);
    let layout = |labels: &[String]| {
        let labels: Vec<_> = labels
            .iter()
            .filter(|label| Some(label.as_str()) != label_scope)
            .collect();
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
    if let Some((previous, shown)) = look.and_then(|look| look.previous_labels.as_ref()) {
        let (old_width, old_group) = layout(previous);
        let transition_width = old_width.max(width);
        if transition_width > 0.0 {
            return Some((
                div()
                    .relative()
                    .flex_none()
                    .w(px(transition_width))
                    .h(px(LABEL_CHIP_PX))
                    .child(group.opacity(*shown))
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

/// Line two: the subject and the snippet. Clipped edges fade into the row fill.
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
    let max_subject = row_widths::SUBJECT_MAX.min(text_width);
    let box_w = frame.subject_w.min(max_subject);
    let room = frame.subject_room.min(max_subject);
    // Shape at the new weight once and reveal it through the moving box.
    let style = window.text_style();
    let subject = clip_shaped(
        window,
        &style,
        &row.subject,
        room,
        row_weight(frame.bold),
        Some(subject_color),
    );
    let shaped_w = subject.as_ref().map_or(0.0, |line| f32::from(line.width));
    let (subject_x, visible_w, snippet_x) = row_widths::line_two(box_w, room, shaped_w);
    let snippet_w = (text_width - room.min(shaped_w) - row_widths::SUBJECT_GAP).max(24.0);
    let snippet_visible_w = (text_width - snippet_x).max(0.0);
    let snippet = clip_shaped(
        window,
        &style,
        &row.snippet,
        snippet_w,
        FontWeight::NORMAL,
        Some(snippet_color),
    )
    .map(|line| {
        // The final ellipsis can slide past the edge while the subject box
        // shrinks. Fade that edge until the shaped snippet fits again.
        let clipped = (f32::from(line.width) - snippet_visible_w).clamp(0.0, 1.0);
        div()
            .absolute()
            .top_0()
            .bottom_0()
            .left(px(snippet_x))
            .w(px(snippet_visible_w))
            .flex()
            .items_center()
            .overflow_hidden()
            .child(shaped_text(line, window))
            .when(!frame.bold && clipped > 0.0, |snippet| {
                snippet.child(row_text_edge_fade(background, snippet_visible_w, clipped))
            })
    });
    let subject = subject.map(|line| {
        let clipped = (shaped_w.min(room) - visible_w).clamp(0.0, 1.0);
        div()
            .relative()
            .left(px(subject_x))
            .flex_none()
            .w(px(visible_w))
            .overflow_hidden()
            .child(shaped_text(line, window))
            .when(clipped > 0.0, |subject| {
                subject.child(row_text_edge_fade(background, visible_w, clipped))
            })
    });
    // Keep line two at a fixed height while the snippet fills in.
    div()
        .relative()
        .w(px(text_width))
        .h(px(22.))
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .children(subject)
        .children(snippet)
}

fn row_text_edge_fade(background: u32, width: f32, opacity: f32) -> gpui::Div {
    let solid = gpui::Hsla::from(rgb(background));
    let clear = gpui::Hsla { a: 0.0, ..solid };
    div()
        .absolute()
        .top_0()
        .bottom_0()
        .right_0()
        .w(px(row_widths::SUBJECT_FADE.min(width)))
        .opacity(opacity)
        .bg(gpui::linear_gradient(
            90.,
            gpui::linear_color_stop(clear, 0.),
            gpui::linear_color_stop(solid, 1.),
        ))
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
    let font_px = f32::from(style.font_size.to_pixels(window.rem_size()));
    fit_shaped_line(text, max_px, font_px, |text| {
        shape_ui_line(window, style, text, weight, color)
    })
}

fn fit_shaped_line(
    text: &str,
    max_px: f32,
    font_px: f32,
    shape: impl Fn(&str) -> Option<gpui::ShapedLine>,
) -> Option<gpui::ShapedLine> {
    // No glyph of a UI font is narrower than a fifth of an em, so more
    // characters than this can never fit.
    let most = (max_px / (font_px * 0.2).max(1.0)).ceil() as usize + 2;
    let probe = match text.char_indices().nth(most) {
        Some((cut, _)) => &text[..cut],
        None => text,
    };
    let mut line = shape(probe)?;
    if f32::from(line.width) <= max_px {
        if probe.len() == text.len() {
            return Some(line);
        }
        // Narrower glyphs than any font has: measure the whole line.
        line = shape(text)?;
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
    let mut room = max_px - f32::from(shape("…")?.width);
    // The whole line does not fit: the cut is before its end.
    let mut before = shaped.len();
    loop {
        let kept = text_cut::kept(&shaped[..cuts.fit(room, before)]);
        // No space or punctuation before the ellipsis: "Friday…".
        let cut = shape(&present::ellipsis(kept))?;
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
    paint_shaped_text(line, window, false)
}

fn paint_shaped_text(
    line: gpui::ShapedLine,
    window: &Window,
    background: bool,
) -> gpui::Canvas<()> {
    let height = window.text_style().line_height_in_pixels(window.rem_size());
    let width = line.width;
    gpui::canvas(
        |_, _, _| {},
        move |bounds, _, window, cx| {
            if background {
                let _ = line.paint_background(bounds.origin, height, window, cx);
            }
            let _ = line.paint(bounds.origin, height, window, cx);
        },
    )
    .w(width)
    .h(height)
    .flex_none()
}

fn tabular_style(window: &Window, theme: &Theme, size: f32) -> TextStyle {
    static FEATURES: OnceLock<gpui::FontFeatures> = OnceLock::new();
    let mut style = conversation_line_style(window, theme, px(size));
    style.font_features = FEATURES
        .get_or_init(|| gpui::FontFeatures(Arc::new(vec![("tnum".into(), 1)])))
        .clone();
    style
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

fn empty_state(theme: &Theme, banner: Banner) -> gpui::Div {
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .px(px(24.))
        .child(div().text_color(rgb(theme.foreground)).child(banner.title))
        .when(!banner.hint.is_empty(), |pane| {
            pane.child(div().text_color(rgb(theme.dimmed)).child(banner.hint))
        })
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
const CONVERSATION_BOTTOM_PX: f32 = 28.;
const MESSAGE_GAP_PX: f32 = 8.;
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
    messages: Option<(Vec<ConvMessage>, u64)>,
    rich: HashMap<String, Rich>,
}

struct ConversationRead {
    notes: Option<Vec<fell_core::notes::ThreadNote>>,
    latest_note: Option<i64>,
    note_generation: u64,
    view: Vec<ConvMessage>,
    reads: mail_body::BodyReads,
    matches: Option<(u64, Vec<fell_core::SearchMatch>)>,
}

#[derive(Default)]
struct PendingBodies {
    /// A read already in flight at the end of a pass can still be stale.
    after: Option<u64>,
    missing: Option<Transition>,
}

impl PendingBodies {
    fn downloading(&self, sync: &SyncKind) -> bool {
        matches!(sync, SyncKind::Syncing { .. }) || self.after.is_some()
    }

    fn wait_for_read(&mut self, generation: u64) {
        self.after = Some(generation);
        self.missing = None;
    }

    fn applied(&mut self, generation: u64, animate: bool, motion: Motion, now: Instant) {
        if self.after.is_some_and(|after| generation > after) {
            self.settle(animate, motion, now);
        }
    }

    fn settle(&mut self, animate: bool, motion: Motion, now: Instant) {
        if self.after.take().is_some() && animate {
            self.missing = motion.start(motion::FADE, now);
        }
    }

    fn fade(&self, now: Instant) -> Option<f32> {
        self.missing
            .filter(|transition| transition.running(now))
            .map(|transition| transition.progress(now))
    }
}

impl BodyRefresh {
    fn is_empty(&self) -> bool {
        self.messages.is_none() && self.rich.is_empty()
    }

    fn apply(
        self,
        session: &mut Session,
        motion: &mut InboxMotion,
        now: Instant,
        height: impl Fn(usize) -> Option<f32>,
        downloading: bool,
    ) {
        let mut rich = self.rich;
        if let Some((mut messages, _)) = self.messages {
            for message in &mut messages {
                if let Some(ready) = rich.remove(&message.id) {
                    message.rich = ready;
                }
            }
            let changed = present::bodies_filled(&session.messages, &messages)
                .expect("a queued reread keeps the same message order");
            for index in changed {
                let old_height = session.messages[index]
                    .expanded
                    .then(|| height(index))
                    .flatten();
                let placeholder = (session.messages[index].body_state
                    == fell_core::BodyState::Pending
                    && messages[index].body_state != fell_core::BodyState::Pending)
                    .then_some(downloading);
                motion.body_arrived(&messages[index].id, old_height, placeholder, now);
            }
            session.refresh_same_messages(messages);
        } else {
            for (index, message) in session.messages.iter_mut().enumerate() {
                if let Some(ready) = rich.remove(&message.id)
                    && ready != message.rich
                {
                    if message.expanded {
                        motion.body_arrived(&message.id, height(index), None, now);
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
        if state == fell_core::BodyState::Stored
            && roles.is(FolderRole::Drafts, viewing)
            && stored
                .folders
                .iter()
                .any(|folder| roles.is(FolderRole::Drafts, folder))
            && let Some(body) = store.editable_draft_body(
                account,
                stored.headers.message_id.as_str(),
                stored.body_text.as_deref(),
            )?
            && let Some(preview) = present::draft_reader_body(&body, &stored.headers.subject, false)
        {
            message.body = preview;
            message.snippet = present::markdown_snippet(&body, &stored.headers.subject);
        }
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
    let leaving = frame.leaving.as_ref();
    let (columns, focus, message_cursor) = leaving.map_or_else(
        || {
            (
                columns,
                motion.focus_edge(session.pane, now),
                session.message_cursor,
            )
        },
        |(reader, _)| {
            (
                reader.layout.columns,
                reader.layout.focus,
                reader.layout.message_cursor,
            )
        },
    );
    let (subject, shown, opacity) = match leaving {
        Some((reader, exit)) => (reader.subject.clone(), reader.messages.as_slice(), *exit),
        None => (subject, session.messages.as_slice(), 1.0),
    };
    let live = Live(live.0 && leaving.is_none());
    let arrival = leaving.map_or_else(|| motion.arrival(now), |(reader, _)| reader.arrival);
    let chip_probe = (live.0 && frame.reveal_attachment).then(LayoutSlot::default);
    let hold = frame
        .anchor
        .filter(|_| leaving.is_none() && scroll.is_some() && chip_probe.is_none())
        .map(|anchor| MessageProbes::new(anchor, shown.len(), frame.return_glide));
    let collapsed_rows = OnceCell::new();
    let retained = !frame.preview_current && session.search.is_some() && !subject.is_empty();
    // Short subjects keep their one-line measure in both modes.
    let title_width = columns.reflow.map_or(f32::INFINITY, |_| {
        let style = conversation_line_style(window, theme, px(theme.ui_px + 11.0));
        line_px_with_style(window, &style, &subject, FontWeight::BOLD).ceil() + 4.0
    });
    let padding = frame.preview_padding.as_ref().filter(|_| scroll.is_some());
    let messages: Vec<_> = shown
        .iter()
        .enumerate()
        .map(|(index, message)| {
            let cursor = if !frame.hide_message_cursor && message_cursor == index {
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
                collapsed_message(
                    theme,
                    message,
                    index,
                    cursor,
                    columns,
                    collapsed_rows
                        .get_or_init(|| CollapsedRows::new(window, theme, columns.card.width)),
                    frame,
                    live,
                    window,
                    cx,
                )
            };
            let block = if let Some(alpha) = frame.card_alphas.get(&message.id) {
                div().opacity(*alpha).child(block).into_any_element()
            } else {
                block
            };
            let block = match frame.heights.get(&message.id) {
                Some(&(from, progress)) if progress < 1.0 && message.expanded => GrowingMessage {
                    inner: block,
                    width: px(columns.pane_right + 2. * MESSAGE_PAD_PX),
                    from,
                    progress,
                    gap: px(MESSAGE_GAP_PX),
                }
                .into_any_element(),
                _ => block,
            };
            let block = if let Some(padding) = padding.filter(|_| index + 1 == shown.len()) {
                padding.wrap_newest(block)
            } else {
                block
            };
            match &hold {
                Some(probes) => probes.wrap(index, block),
                None => block,
            }
        })
        .collect();
    div()
        .id("conversation")
        .relative()
        .when(opacity < 1.0, |pane| pane.opacity(opacity))
        .when(leaving.is_some(), |pane| pane.bg(rgb(theme.background)))
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
        .children(
            frame
                .pin
                .zip(frame.agent_note.as_ref())
                .map(|(mut pin, note)| {
                    pin.alpha *= arrival;
                    pin.divider *= arrival;
                    agent_notes::pinned_header(
                        theme,
                        session,
                        columns,
                        note,
                        frame.note_expanded_in_band,
                        pin,
                        live,
                        cx,
                    )
                }),
        )
        .child(match leaving {
            Some((reader, _)) => div()
                .flex_1()
                .min_h_0()
                .overflow_hidden()
                .child(
                    div()
                        .relative()
                        .top(px(reader.offset))
                        .px(px(CONVERSATION_PAD_PX))
                        .opacity(arrival)
                        .children(messages),
                )
                .into_any_element(),
            None => {
                let note = frame
                    .agent_note
                    .as_ref()
                    .filter(|_| !session.messages.is_empty())
                    .map(|note| {
                        agent_notes::slot(
                            theme,
                            session,
                            columns,
                            note,
                            !frame.note_details_only,
                            frame.pin.is_some(),
                            live,
                            window,
                            cx,
                        )
                    })
                    .map(IntoElement::into_any_element);
                conversation_body(
                    note.into_iter().chain(messages),
                    scroll,
                    hold,
                    chip_probe,
                    padding.map(|padding| padding.viewport),
                    arrival,
                    frame.frames.clone(),
                )
            }
        })
        .when(retained, |pane| {
            pane.child(preview_shield(
                theme,
                leaving.is_none() && session.search_results_current() && !frame.quiet_preview,
            ))
        })
}

fn expanded_card_fill(theme: &Theme, focus: f32) -> u32 {
    motion::mix(
        theme.background,
        theme.surface,
        motion::lerp(0.4, 1.0, focus),
    )
}

fn conversation_body(
    children: impl IntoIterator<Item = gpui::AnyElement>,
    scroll: Option<ScrollHandle>,
    hold: Option<MessageProbes>,
    chip_probe: Option<LayoutSlot>,
    preview_viewport: Option<gpui::Pixels>,
    arrival: f32,
    frames: Option<SharedFrameLog>,
) -> gpui::AnyElement {
    let bottom = px(if preview_viewport.is_some() {
        0.
    } else {
        CONVERSATION_BOTTOM_PX
    });
    let resize = preview_viewport.zip(scroll.clone());
    let body = div()
        .when_some(resize, |body, (height, scroll)| {
            body.on_children_prepainted(move |_, window, _| {
                if scroll.bounds().size.height != height {
                    window.request_animation_frame();
                }
            })
        })
        .id("conversation-scroll")
        .flex_1()
        .min_h(px(0.))
        .map(|body| match &scroll {
            Some(scroll) => body.overflow_y_scroll().track_scroll(scroll),
            None => body.overflow_hidden(),
        })
        .px(px(CONVERSATION_PAD_PX))
        .pb(bottom)
        .when(arrival < 1.0, |body| body.opacity(arrival))
        .children(children)
        .into_any_element();
    match (scroll, hold, chip_probe) {
        (Some(scroll), Some(hold), _) => hold.hold(body, scroll, bottom, frames).into_any_element(),
        (Some(scroll), _, Some(probe)) => {
            LandingHold::reveal(body, scroll, probe, frames).into_any_element()
        }
        _ => body,
    }
}

fn preview_shield(theme: &Theme, show_feedback: bool) -> impl IntoElement {
    div()
        .id("retained-preview")
        .absolute()
        .size_full()
        .top_0()
        .left_0()
        .occlude()
        .when(show_feedback, |shield| {
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
    cursor: f32,
    columns: ReaderColumns,
    rows: &CollapsedRows,
    frame: &ConvFrame,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let now = now_unix();
    let mut date = present::relative_date(now, message.date_unix);
    let dimmed = motion::mix(theme.dimmed, theme.on_selection, cursor);
    let (body, outgoing) = frame.body_labels(message);
    let missing = present::missing_body_line(message.body_state, frame.syncing);
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
    let leading =
        div()
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
                    .relative()
                    .whitespace_nowrap()
                    .text_color(rgb(dimmed))
                    .child(div().opacity(body).children(
                        clip(snippet, snippet_w, FontWeight::NORMAL).map(|line| line.text),
                    ))
                    .children(
                        outgoing
                            .and_then(|line| clip(&line, snippet_w, FontWeight::NORMAL))
                            .map(|line| {
                                div()
                                    .absolute()
                                    .left_0()
                                    .top_0()
                                    .opacity(1. - body)
                                    .child(line.text)
                            }),
                    ),
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

fn expanded_header_gaps([sender, email, ..]: [f32; 4]) -> f32 {
    // The folder group also acts as the spacer before the date when empty.
    (1 + usize::from(sender > 0.0) + usize::from(email > 0.0)) as f32 * MESSAGE_HEADER_GAP_PX
}

fn expanded_header_widths(width: f32, fields: [f32; 4]) -> [f32; 4] {
    let [sender, email, folders, date] = fields;
    let date = date.min(width);
    let room = (width - date - expanded_header_gaps(fields)).max(0.0);
    // When the identity needs to truncate, give it at least two thirds of
    // the room. A short or absent folder list leaves more for the sender.
    let identity = (room - folders.min(room / 3.0)).min(sender + email);
    let email = email.min(identity - sender.min(identity / 2.0));
    let sender = sender.min(identity - email);
    let folders = folders.min((room - sender - email).max(0.0));
    [sender, email, folders, date]
}

fn expanded_header_flow(
    columns: ReaderColumns,
    fields: [f32; 4],
    mut layer: impl FnMut([f32; 4]) -> gpui::AnyElement,
) -> gpui::AnyElement {
    // A header that fits both modes keeps one layer, just like a short subject.
    let full_width = fields.iter().sum::<f32>() + expanded_header_gaps(fields);
    columns.flow(
        |columns| ReaderColumn {
            width: columns.header.width.min(full_width),
            ..columns.header
        },
        |width, _| layer(expanded_header_widths(width, fields)),
    )
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
    let widths = [
        measure(
            &sender_style,
            &sender,
            FontWeight::SEMIBOLD,
            theme.foreground,
        ),
        measure(&style, &email, FontWeight::NORMAL, theme.dimmed),
        measure(&style, &joined, FontWeight::NORMAL, theme.dimmed),
        date_width,
    ];
    let fit = |style: &TextStyle, text: &str, width: f32, weight, color| {
        clip_shaped(window, style, text, width.floor(), weight, Some(color))
            .filter(|line| !line.text.is_empty() && f32::from(line.width).ceil() <= width)
    };
    let leading = expanded_header_flow(
        columns,
        widths,
        |[sender_width, email_width, folder_width, _]| {
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
            let folder_width = folder_width
                + (sender_width - line_width(&sender))
                + (email_width - line_width(&email));
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
            div()
                .flex()
                .items_center()
                .gap(px(MESSAGE_HEADER_GAP_PX))
                .children(
                    [sender, email, folders]
                        .into_iter()
                        .flatten()
                        .map(|line| shaped_text(line, window)),
                )
                .into_any_element()
        },
    );
    let date = fit(
        &sender_style,
        &date,
        columns.header.width,
        FontWeight::NORMAL,
        theme.dimmed,
    );
    // Only the leading clip changes width. The date sits at the padded
    // card edge throughout the slide, independently of either header measure.
    message_date_row(
        div()
            .w(px(columns.card_right()))
            // The recipient line belongs to this header, so it sits close.
            .pb(px(if message.recipients.is_some() { 2. } else { 8. })),
        MESSAGE_HEADER_GAP_PX,
        div().child(leading),
        date.map(|line| {
            div()
                .flex_none()
                .max_w(px(columns.card.width))
                .flex()
                .justify_end()
                .overflow_hidden()
                .child(shaped_text(line, window))
        }),
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
        .mb(px(MESSAGE_GAP_PX))
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
        .child(expanded_message_header(theme, message, columns, window))
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
                            frame.leaving.as_ref().map_or_else(
                                || session.recipients_are_open(&message.id),
                                |(reader, _)| reader.recipients.contains(&message.id),
                            ),
                            frame.disclosures().recipients.get(&message.id).copied(),
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
                    .children(frame.match_line.render(theme, message))
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
                open: frame.leaving.as_ref().map_or_else(
                    || session.is_open(&message.id),
                    |(reader, _)| reader.quotes.contains(&message.id),
                ),
                columns,
                name_key: index
                    == frame
                        .leaving
                        .as_ref()
                        .map_or(session.message_cursor, |(reader, _)| {
                            reader.layout.message_cursor
                        }),
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
    let (alpha, outgoing) = frame.body_labels(message);
    let body = match present::missing_body_line(message.body_state, frame.syncing) {
        Some(line) => missing_message_body(theme, folds.columns, &line, alpha),
        None => stored_message_body(theme, message, folds, frame, alpha, cx),
    };
    let Some(line) = outgoing else {
        return body;
    };
    div()
        .relative()
        .child(body)
        .child(div().absolute().inset_0().child(missing_message_body(
            theme,
            folds.columns,
            &line,
            1.0 - alpha,
        )))
        .into_any_element()
}

impl ConvFrame {
    fn body_labels(&self, message: &ConvMessage) -> (f32, Option<String>) {
        use fell_core::BodyState::Pending;

        let (alpha, was_downloading) = if message.body_state == Pending
            && !self.syncing
            && let Some(alpha) = self.missing_fade
        {
            (alpha, Some(true))
        } else {
            (
                self.body_alpha(&message.id),
                self.outgoing_placeholders.get(&message.id).copied(),
            )
        };
        let outgoing = was_downloading
            .filter(|_| alpha < 1.)
            .and_then(|downloading| present::missing_body_line(Pending, downloading));
        (alpha, outgoing)
    }
}

fn missing_message_body(
    theme: &Theme,
    columns: ReaderColumns,
    line: &str,
    alpha: f32,
) -> gpui::AnyElement {
    columns.flow(
        |columns| columns.prose,
        |_, _| {
            div()
                .pb(px(8.))
                .line_height(px(theme.ui_px * 1.45))
                .text_color(rgb(theme.dimmed))
                .opacity(alpha)
                .child(line.to_owned())
                .into_any_element()
        },
    )
}

fn stored_message_body(
    theme: &Theme,
    message: &ConvMessage,
    folds: FoldView,
    frame: &ConvFrame,
    alpha: f32,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let reveal = frame.disclosures().quotes.get(&message.id).copied();
    if let Rich::Html(body) = &message.rich {
        return div()
            .opacity(alpha)
            .children(html_sections(theme, body, folds, reveal, cx))
            .into_any_element();
    }
    let blocks: Vec<gpui::AnyElement> = match &message.rich {
        Rich::Html(_) => unreachable!(),
        Rich::Pending if message.body.blocks.is_empty() => {
            return missing_message_body(theme, folds.columns, "Preparing message…", alpha);
        }
        Rich::Pending | Rich::Plain | Rich::PlainPart(_) | Rich::Missing => {
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
    let style = conversation_line_style(window, theme, size);
    let space = line_px_with_style(window, &style, " ", FontWeight::NORMAL);
    let details = || {
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .children(line.groups.iter().map(|group| {
                div()
                    .flex()
                    .gap(px(space))
                    .child(
                        div()
                            .flex_shrink_0()
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum StatusPhase {
    OpeningDraft,
    Sync(std::mem::Discriminant<SyncKind>),
    Download,
    Reply(ReplyCheck),
}

#[derive(Clone)]
struct StatusLabel {
    phase: StatusPhase,
    text: String,
    download: Option<DownloadStatus>,
    color: u32,
    steady_sync: bool,
}

impl StatusLabel {
    fn opening_draft(theme: &Theme) -> Self {
        Self {
            phase: StatusPhase::OpeningDraft,
            text: "Opening…".into(),
            download: None,
            color: theme.dimmed,
            steady_sync: false,
        }
    }

    fn new(theme: &Theme, session: &Session, reply_check: Option<ReplyCheck>, width: f32) -> Self {
        let download = reply_check
            .is_none()
            .then(|| session.sync.download_status())
            .flatten();
        let phase = if let Some(check) = reply_check {
            StatusPhase::Reply(check)
        } else if download.is_some() {
            StatusPhase::Download
        } else {
            StatusPhase::Sync(std::mem::discriminant(&session.sync))
        };
        let (text, color) = match reply_check {
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
                if download.is_some() {
                    String::new()
                } else {
                    session.sync.status()
                },
                if session.sync.is_error() {
                    theme.error
                } else {
                    theme.dimmed
                },
            ),
        };
        Self {
            phase,
            text,
            download,
            color,
            steady_sync: reply_check.is_none()
                && matches!(
                    session.sync,
                    SyncKind::Connecting
                        | SyncKind::Syncing { progress: None, .. }
                        | SyncKind::Synced { .. }
                ),
        }
    }

    fn draw(
        &self,
        theme: &Theme,
        size: f32,
        room: f32,
        window: &Window,
        fading_count: Option<(&str, f32)>,
    ) -> (gpui::Div, f32) {
        let ui = theme.ui_family.as_ref();
        let label = div()
            .flex_none()
            .whitespace_nowrap()
            .text_size(px(size))
            .text_color(rgb(self.color));
        let Some(DownloadStatus {
            prefix,
            done,
            total,
            suffix,
        }) = &self.download
        else {
            let width = bar_px(window, &self.text, size, ui).ceil();
            return (label.child(self.text.clone()), width);
        };
        let figures = tabular_style(window, theme, size);
        let figure_width =
            |text| line_px_with_style(window, &figures, text, FontWeight::NORMAL).ceil();
        let count_room = figure_width(total)
            .max(figure_width(done))
            .max(fading_count.map_or(0., |(old, _)| figure_width(old)));
        let suffix_width = bar_px(window, suffix, size, ui).ceil();
        let prefix_room = room - count_room - suffix_width;
        let short;
        let prefix = if bar_px(window, prefix, size, ui).ceil() > prefix_room {
            short = format!("Downloading {total} · ");
            &short
        } else {
            prefix
        };
        let width = bar_px(window, prefix, size, ui).ceil() + count_room + suffix_width;
        let shown = fading_count.map_or(1.0, |(_, progress)| progress);
        let label = label
            .flex()
            .items_baseline()
            .child(div().flex_none().child(prefix.clone()))
            .child(
                div()
                    .relative()
                    .w(px(count_room))
                    .flex_none()
                    .font(figures.font())
                    .child(div().text_right().opacity(shown).child(done.clone()))
                    .children(fading_count.map(|(old, progress)| {
                        div()
                            .absolute()
                            .right_0()
                            .top_0()
                            .opacity(1.0 - progress)
                            .child(old.to_owned())
                    })),
            )
            .child(div().flex_none().child(suffix.clone()));
        (label, width)
    }
}

#[derive(Default)]
struct StatusLabels {
    current: Option<StatusLabel>,
    outgoing: Option<(StatusLabel, Transition)>,
    count_fade: motion::NumberFade<String>,
}

struct StatusLabelsFrame {
    current: StatusLabel,
    outgoing: Option<(StatusLabel, f32)>,
    count_fade: Option<(String, f32)>,
}

impl StatusLabels {
    fn update(&mut self, next: StatusLabel, motion: Motion, now: Instant) -> StatusLabelsFrame {
        let old = self.current.replace(next.clone());
        if self
            .outgoing
            .as_ref()
            .is_some_and(|(_, fade)| !fade.running(now))
        {
            self.outgoing = None;
        }
        if let Some(old) = old {
            if old.phase == StatusPhase::OpeningDraft && old.phase != next.phase {
                self.outgoing = None;
                self.count_fade = motion::NumberFade::default();
            } else if old.phase != next.phase {
                self.outgoing = motion.start(motion::FADE, now).map(|fade| (old, fade));
                self.count_fade = motion::NumberFade::default();
            } else if let (Some(old_download), Some(next_download)) =
                (&old.download, &next.download)
                && old_download.done != next_download.done
                && self.outgoing.is_none()
            {
                self.count_fade
                    .change(old_download.done.clone(), motion, now);
            }
        }
        StatusLabelsFrame {
            current: next,
            count_fade: self
                .count_fade
                .frame(now)
                .map(|(old, shown)| (old.clone(), shown)),
            outgoing: self
                .outgoing
                .as_ref()
                .map(|(prior, fade)| (prior.clone(), fade.linear(now))),
        }
    }
}

impl StatusLabelsFrame {
    fn fading_count(&self) -> Option<(&str, f32)> {
        self.count_fade
            .as_ref()
            .map(|(old, shown)| (old.as_str(), *shown))
    }

    fn draw(&self, theme: &Theme, size: f32, room: f32, window: &Window) -> (gpui::Div, f32) {
        let Some((old, elapsed)) = &self.outgoing else {
            return self
                .current
                .draw(theme, size, room, window, self.fading_count());
        };

        // Keep both measures while the phase changes. Only a brief, dim
        // overlap passes through the middle of the fade.
        let (current, width) = self.current.draw(theme, size, room, window, None);
        let (old, old_width) = old.draw(theme, size, room, window, None);
        let width = width.max(old_width);
        let incoming = motion::ease_out((*elapsed - 0.45) / 0.55);
        let outgoing = motion::fade_out(*elapsed / 0.6);
        (
            div()
                .flex_none()
                .relative()
                .w(px(width))
                .child(current.opacity(incoming))
                .child(old.absolute().top_0().left_0().opacity(outgoing)),
            width,
        )
    }
}

/// Mode, sync state and one notice. At rest there are no key hints
/// (docs/ux.md, Quiet): the right side shows only what the reader is
/// typing, a count or a chord's prefix, while which-key lists the rest.
fn status_bar(
    theme: &Theme,
    session: &Session,
    status: StatusMotion,
    labels: StatusLabelsFrame,
    width: f32,
    window: &Window,
) -> gpui::Div {
    // A notice takes the room of the keys being typed (a count, a chord's
    // prefix), entering or leaving, so its text never re-flows as it fades.
    // Those keys fade under it at the right.
    let noticed = status.notice.is_some() || status.leaving.is_some();
    let narrow = width < present::NARROW_WIDTH;
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
    let (padding, gap) = if width < 420. { (8., 6.) } else { (14., 12.) };
    let mode_width = bar_px(window, session.mode_label(), size, mono);
    let dot_width = bar_px(window, "·", size, ui);
    let outage = session.worker_outage();
    let details = outage.is_none() && session.detail_text().is_some();
    let details_width = if details {
        gap + bar_px(window, "· ? details", size, ui)
    } else {
        0.
    };
    let trailing = if noticed {
        gap + dot_width
    } else {
        16. + if hint.is_empty() && count.is_empty() {
            0.
        } else {
            gap + bar_px(window, &format!("{count}{hint}"), size, mono)
        }
    };
    let sync_room =
        width - 2. * padding - 3. * gap - mode_width - dot_width - details_width - trailing;
    // What the bar spends before the notice, measured as drawn: the
    // padding, the mode in mono, sync, the dots, the gaps between the
    // children. No typed keys show beside a notice, so no air after it.
    let draw_sync = |size| {
        if let Some(worker) = outage {
            let key = keymap::binding_text(&session.keymap, Action::OpenDiagnosticLog);
            let text = worker.outage_notice();
            let width = bar_px(window, &format!("{text} · {key} for log"), size, ui);
            let label = div()
                .text_size(px(size))
                .whitespace_nowrap()
                .text_color(rgb(theme.error))
                .child(status_text(
                    text.to_owned(),
                    Some((&key, " for log")),
                    theme,
                ));
            (label, width)
        } else if !noticed
            && !narrow
            && labels.current.steady_sync
            && labels
                .outgoing
                .as_ref()
                .is_none_or(|(old, _)| old.steady_sync)
        {
            // Keep the routine sync slot fixed even while its text fades.
            let width = bar_px(window, "synced just now · 0000 new", size, ui).ceil();
            let (label, _) = labels.draw(theme, size, sync_room, window);
            (label.w(px(width)).truncate(), width)
        } else {
            labels.draw(theme, size, sync_room, window)
        }
    };
    let (label, sync_width) = draw_sync(size);
    // The count slot is measured for every digit of the total. Tight
    // windows give the full label room even when both numbers are long.
    let (label, sync_width) = if sync_width > sync_room {
        let fitted_size = (size * (sync_room - 3.).max(0.) / sync_width).max(1.);
        draw_sync(fitted_size)
    } else {
        (label, sync_width)
    };
    let shared_width = 2. * padding + 2. * gap + mode_width + dot_width + details_width;
    let used = shared_width
        + if narrow {
            0.
        } else {
            sync_width + 2. * gap + dot_width
        };
    let scale = window.scale_factor();
    let notice_text = |toast: &Toast, opacity: f32, dy: f32| {
        let hint = toast
            .hint
            .as_ref()
            .map(|(key, suffix)| (key.as_str(), *suffix));
        let tail_width = hint.map_or(0.0, |(key, suffix)| {
            bar_px(window, &format!(" · {key}{suffix}"), size, ui)
        });
        let room = (width - used - tail_width).max(0.0);
        let notice_size = if narrow && toast.alert && toast.label.is_none() && hint.is_none() {
            let natural = bar_px(window, &toast.text, size, ui);
            if natural > room {
                (size * (room - 2.).max(0.) / natural).max(11.)
            } else {
                size
            }
        } else {
            size
        };
        let measure = |text: &str| bar_px(window, text, notice_size, ui);
        let text = toast.label.as_ref().map_or_else(
            || present::fit_notice(&toast.text, room, measure),
            |label| label.fit(room, measure),
        );
        div()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(notice_size))
            .text_color(rgb(if toast.alert {
                theme.error
            } else {
                theme.foreground
            }))
            .when(opacity < 1.0, |text| text.opacity(opacity))
            .when(dy.abs() >= 0.01, |text| {
                text.relative().top(px(motion::snap(dy, scale)))
            })
            .child(status_text(text, hint, theme))
    };
    let label = label.opacity(status.sync_opacity);
    let (wide_sync, narrow_sync) = if narrow {
        (None, Some(label))
    } else {
        (Some(label), None)
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
    let text = status
        .leaving
        .filter(|(_, opacity)| *opacity > 0.0)
        .map(|(toast, opacity)| (toast, opacity, leaving_dy))
        .or_else(|| {
            status
                .notice
                .filter(|(_, opacity)| *opacity > 0.0)
                .map(|(toast, opacity)| (toast, opacity, notice_dy))
        });
    div()
        .relative()
        .h(px(30.))
        .px(px(padding))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(gap))
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
        .when(!narrow, |bar| {
            bar.child(div().flex_none().text_color(rgb(theme.dimmed)).child("·"))
                .child(wide_sync.unwrap().flex_none())
        })
        .when(details, |bar| {
            bar.child(div().text_color(rgb(theme.error)).child("· ? details"))
        })
        // In a narrow window sync and the notice share a separator and cell.
        .when(narrow || noticed, |bar| {
            bar.child(
                div()
                    .flex_none()
                    .text_color(rgb(theme.dimmed))
                    .when(!narrow && notice_opacity < 1.0, |dot| {
                        dot.opacity(notice_opacity)
                    })
                    .child("·"),
            )
        })
        .child(
            div()
                .relative()
                .flex_1()
                .h_full()
                .min_w(px(0.))
                .flex()
                .items_center()
                // Air before the typed keys, so a long message never runs into them.
                .when(!noticed, |cell| cell.pr(px(16.)))
                .overflow_hidden()
                .when(narrow && status.sync > 0.0, |cell| {
                    cell.child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .opacity(status.sync)
                            .child(narrow_sync.unwrap().min_w_0().truncate()),
                    )
                })
                .when_some(text, |cell, (toast, opacity, dy)| {
                    cell.child(notice_text(&toast, opacity, dy))
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
                    .right(px(padding))
                    .flex()
                    .items_center()
                    .child(hints),
            ),
            _ => bar,
        })
}

fn status_text(mut text: String, hint: Option<(&str, &str)>, theme: &Theme) -> StyledText {
    let highlight = hint.map(|(key, suffix)| {
        text.push_str(" · ");
        let start = text.len();
        text.push_str(key);
        let end = text.len();
        text.push_str(suffix);
        (
            start..end,
            HighlightStyle {
                color: Some(rgb(theme.accent).into()),
                ..HighlightStyle::default()
            },
        )
    });
    StyledText::new(text).with_highlights(highlight)
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
const PICKER_ROW_MARGIN: f32 = 4.0;
const PICKER_ROW_PAD: f32 = 10.0;
/// Where the palette, folder picker and help card sit below the window's top.
const CARD_TOP_PX: f32 = 72.0;

/// How tall the open palette or folder picker should be to hold its items
/// (its border, the 44 px query line and 4 px above and below the rows),
/// up to its maximum and the window. `None` for any other overlay.
fn card_fit(
    session: &Session,
    choices: &[PickerChoice],
    theme: &Theme,
    window: &Window,
) -> Option<f32> {
    let room = f32::from(window.viewport_size().height) - CARD_TOP_PX - 16.0;
    match session.visible_overlay() {
        Overlay::Palette => {
            let rows_px = session.palette().into_iter().map(|item| match item.action {
                Action::About => about_palette_height(theme, window),
                _ => PALETTE_ROW_PX,
            });
            Some(present::card_height(rows_px.sum(), PALETTE_MAX_PX, room))
        }
        Overlay::Folders => {
            let empty_label = choices.is_empty()
                && matches!(
                    session.folder_intent,
                    FolderIntent::Label | FolderIntent::LabelList
                );
            Some(picker_card_height(
                choices
                    .iter()
                    .map(|choice| choice.divider)
                    .chain(empty_label.then_some(false)),
                room,
            ))
        }
        _ => None,
    }
}

fn picker_card_height(dividers: impl DoubleEndedIterator<Item = bool>, room: f32) -> f32 {
    let capacity = room.min(PICKER_MAX_PX) - present::CARD_CHROME_PX;
    // Measure from the last row so maximum scroll lands on a row boundary.
    // Only dividers between visible rows count toward the scroll viewport.
    let mut rows = 0.0;
    let mut divider = false;
    for before in dividers.rev() {
        let next = rows + PICKER_ROW_PX + if divider { PICKER_GROUP_GAP + 1.0 } else { 0.0 };
        if next > capacity {
            rows = rows.max(PICKER_ROW_PX);
            break;
        }
        rows = next;
        divider = before;
    }
    present::card_height(rows, PICKER_MAX_PX, room)
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
    picker: (&[PickerChoice], Option<String>, f32),
    help: Option<(f32, gpui::Div)>,
    agents: Option<gpui::Div>,
    window: &Window,
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
        .when(
            matches!(session.overlay, Overlay::About | Overlay::Agents),
            |layer| layer.items_start(),
        )
        .when(live, |layer| {
            layer.occlude().on_mouse_down(
                MouseButton::Left,
                cx.listener(|inbox, _, _, cx| {
                    inbox.session.dismiss_overlay();
                    cx.notify();
                }),
            )
        })
        .child(match session.overlay {
            Overlay::Palette => palette(theme, session, palette_scroll, cx)
                .when_some(card, |card, height| card.h(px(height)))
                .opacity(opacity)
                .into_any_element(),
            Overlay::Folders => folder_picker(theme, session, folder_scroll, picker, window, cx)
                .when_some(card, |card, height| card.h(px(height)))
                .opacity(opacity)
                .into_any_element(),
            Overlay::Help => help
                .map_or_else(div, |(_, content)| content)
                .opacity(opacity)
                .into_any_element(),
            Overlay::About => about(theme, cx).opacity(opacity).into_any_element(),
            Overlay::Agents => agents
                .unwrap_or_else(div)
                .opacity(opacity)
                .into_any_element(),
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
                    let detail = match item.action {
                        Action::About => fell_version::VERSION,
                        Action::Notifications(mode) if session.notification_mode == Some(mode) => {
                            "✓"
                        }
                        _ => item.binding.as_str(),
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
                                .when(detail != "✓", |slot| {
                                    slot.font_family_opt(theme.mono_family.clone())
                                })
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

fn folder_picker(
    theme: &Theme,
    session: &Session,
    scroll: &ScrollHandle,
    (choices, label_target, mark_opacity): (&[PickerChoice], Option<String>, f32),
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    const MARGIN: f32 = 12.0;
    const PAD: f32 = 14.0;
    const GAP: f32 = 8.0;
    const CARET: f32 = 2.0;
    const CARET_GAP: f32 = 2.0;
    let query_gap = if session.folder_intent == FolderIntent::Label {
        8.0
    } else {
        0.0
    };
    let labeling = matches!(
        session.folder_intent,
        FolderIntent::Label | FolderIntent::LabelList
    );
    let selected = session.folder_index;
    let match_query = session.folder_query.trim().to_ascii_lowercase();
    let inset = if labeling { 2.0 * MARGIN } else { 0.0 };
    let width = PICKER_PX.min((f32::from(window.viewport_size().width) - inset).max(0.0));
    let label_room = (width - 2.0 - 2.0 * (PICKER_ROW_MARGIN + PICKER_ROW_PAD)).max(0.0);
    let style = conversation_line_style(window, theme, px(theme.ui_px));
    let prompt_dim = theme::ensure_all(theme.dimmed, &[theme.surface], 4.8);
    let prompt = session.folder_prompt();
    let query = present::display_safe(&session.folder_query);
    let target = label_target.and_then(|target| {
        let measure = |text| line_px_with_style(window, &style, text, FontWeight::NORMAL).ceil();
        let query_width = measure(&query) + CARET + if query.is_empty() { 0.0 } else { CARET_GAP };
        let room = width
            - 2.0 * (PAD + 1.0)
            - 3.0 * GAP
            - query_gap
            - measure(prompt)
            - measure("·")
            - query_width;
        let target = present::display_safe(&target);
        clip_shaped(
            window,
            &style,
            &target,
            room.floor(),
            FontWeight::NORMAL,
            None,
        )
        .filter(|line| f32::from(line.width).ceil() <= room)
        .map(|line| line.text)
    });
    div()
        .w(px(width))
        .max_w_full()
        .when(labeling, |card| card.mx(px(MARGIN)))
        .min_w_0()
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
                if inbox.session.visible_overlay() == Overlay::Folders {
                    cx.stop_propagation();
                }
            }),
        )
        .child(
            div()
                .flex_none()
                .h(px(present::CARD_QUERY_PX))
                .px(px(PAD))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(GAP))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(if session.folder_intent == FolderIntent::Move {
                            theme.accent
                        } else {
                            prompt_dim
                        }))
                        .child(prompt),
                )
                .when_some(target, |prompt, target| {
                    prompt
                        .child(div().flex_none().text_color(rgb(theme.dimmed)).child("·"))
                        .child(
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .text_color(rgb(theme.dimmed))
                                .child(target),
                        )
                })
                .child(
                    div()
                        .flex()
                        .items_center()
                        .ml(px(query_gap))
                        .gap(px(CARET_GAP))
                        .min_w_0()
                        .when(!query.is_empty(), |field| {
                            field.child(div().min_w_0().truncate().child(query))
                        })
                        .child(
                            div()
                                .flex_none()
                                .w(px(CARET))
                                .h(px(16.))
                                .bg(rgb(theme.accent)),
                        ),
                ),
        )
        .child(
            div()
                .id("folder-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .my(px(present::CARD_ROW_INSET_PX))
                .children((labeling && choices.is_empty()).then(|| {
                    div()
                        .h(px(PICKER_ROW_PX))
                        .flex_none()
                        .flex()
                        .items_center()
                        .px(px(14.))
                        .text_color(rgb(theme.dimmed))
                        .child(if session.folder_query.is_empty() {
                            "No labels yet"
                        } else {
                            "No matching labels"
                        })
                }))
                .children(choices.iter().enumerate().map(|(index, choice)| {
                    let label = present::display_safe(&choice.title);
                    let room = label_room
                        - if session.folder_intent == crate::model::FolderIntent::Label {
                            16.0 + 8.0
                        } else {
                            0.0
                        };
                    let clip = |text: &str| {
                        clip_shaped(window, &style, text, room, FontWeight::NORMAL, None)
                    };
                    let label = if labeling {
                        clip(&label)
                    } else {
                        fit_folder_path(&label, FolderFit::Picker(&match_query), clip, |line| {
                            &line.text
                        })
                    };
                    let detail = choice.detail.clone();
                    let detail_color = if index == selected {
                        theme.on_selection_dim
                    } else {
                        theme.dimmed
                    };
                    let row = div()
                        .id(gpui::ElementId::named_usize("folder", index))
                        .h(px(PICKER_ROW_PX))
                        .mx(px(PICKER_ROW_MARGIN))
                        .px(px(PICKER_ROW_PAD))
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
                                if inbox.session.visible_overlay() != Overlay::Folders {
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
                        .when(session.folder_intent == FolderIntent::Label, |row| {
                            row.child(
                                div()
                                    .w(px(16.))
                                    .flex_none()
                                    .opacity(if choice.mark == Some("+") {
                                        1.0
                                    } else {
                                        mark_opacity
                                    })
                                    .text_color(rgb(if index == selected {
                                        theme.on_selection
                                    } else {
                                        theme.accent
                                    }))
                                    .children(choice.mark),
                            )
                        })
                        .children(label.map(|line| {
                            let matched = folder_match(&line.text, &match_query);
                            let highlight = HighlightStyle {
                                color: Some(
                                    rgb(if index == selected {
                                        theme.on_selection_accent
                                    } else {
                                        theme.accent
                                    })
                                    .into(),
                                ),
                                ..HighlightStyle::default()
                            };
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .font_weight(FontWeight::NORMAL)
                                .when(matched.is_some(), |label| {
                                    label.text_color(rgb(detail_color))
                                })
                                .child(
                                    StyledText::new(line.text)
                                        .with_highlights(matched.map(|range| (range, highlight))),
                                )
                        }))
                        .children(detail.map(|name| {
                            div()
                                .min_w(px(0.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_color(rgb(detail_color))
                                .child(name)
                        }));
                    let divider = choice.divider;
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

/// Land on the first restored row in the current folder's order. Use the
/// store's current keys so undo can land outside the loaded window too.
fn restored_folder_key(
    store: &fell_core::Store,
    account: &fell_core::AccountId,
    folder: &str,
    threads: &[ThreadId],
) -> Option<ListKey> {
    threads
        .iter()
        .filter_map(|id| store.folder_key_of(account, folder, id).ok().flatten())
        .min()
}

/// How a triage action takes threads out of the current folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Removal {
    /// To Archive.
    Archive,
    /// To Trash.
    Delete,
    /// To Spam, or from Spam to the inbox.
    Spam,
    /// Out of Trash for good, in Fell only.
    Purge,
}

impl Removal {
    fn in_folder(
        self,
        store: &fell_core::Store,
        account: &AccountId,
        threads: Vec<ThreadId>,
        folder: String,
    ) -> fell_core::Result<usize> {
        if self == Self::Purge {
            return store
                .purge_threads_counted(account, &threads, &folder)
                .map(|(_, count)| count);
        }
        let scopes: Vec<_> = threads.into_iter().map(|id| (id, folder.clone())).collect();
        match self {
            Self::Archive => store.archive_scoped_threads(account, &scopes),
            Self::Delete => store.delete_scoped_threads(account, &scopes),
            Self::Spam => store.spam_scoped_threads(account, &scopes),
            Self::Purge => unreachable!(),
        }
        .map(|(_, count)| count)
    }
}

/// "Moved to Trash · u", or "Moved 3 to Trash · u" for a selection.
fn moved_notice(count: usize, to: &str) -> String {
    match count {
        1 => format!("Moved to {to} · u"),
        n => format!("Moved {n} to {to} · u"),
    }
}

fn archived_notice(count: usize, selected: usize) -> String {
    if count == 1 && selected == 1 {
        "Archived · u".into()
    } else {
        format!("Archived {count} · u")
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
    thread_local! {
        pub(super) static INBOX_LABEL_X: std::cell::RefCell<Vec<(gpui::Pixels, gpui::SharedString)>> = const {
            std::cell::RefCell::new(Vec::new())
        };
    }
    use super::{
        FIRST_PREVIEW_WAIT, FirstPreview, FolderFit, LIST_BORDER_PX, MESSAGE_HEADER_GAP_PX,
        MESSAGE_PAD_PX, READING_MEASURE_PX, ReaderColumns, account_header, collapsed_sender_budget,
        ctrl_letter, expanded_header_flow, expanded_header_widths, fit_folder_path, key_from,
        message_content_width, picker_card_height, should_follow_visible, unread_bar_dx,
        useful_header_email, write_back_notice,
    };
    use crate::keymap::KeyId;
    use crate::model::{Overlay, SyncKind};
    use crate::motion;
    use crate::present::Pane;
    use gpui::{IntoElement, Keystroke, Modifiers, div};
    use std::time::{Duration, Instant};

    #[test]
    fn a_dropped_mailbox_reply_still_runs_failure_completion() {
        let (sender, reply) = futures::channel::oneshot::channel::<fell_core::Result<()>>();
        drop(sender);

        let mut outcome = None;
        futures::executor::block_on(super::finish_mailbox_change(reply, |result| {
            outcome = Some(result);
        }));

        assert!(matches!(
            outcome,
            Some(Err(fell_core::Error::Action(message))) if message == "Could not update this mailbox"
        ));
    }

    #[test]
    fn page_reads_retire_only_the_flags_they_could_have_seen() {
        let mut pending = super::PendingRowFlags {
            unread: Some(super::PendingFlag {
                write: 1,
                value: false,
                through: None,
            }),
            starred: Some(super::PendingFlag {
                write: 2,
                value: true,
                through: Some(12),
            }),
        };
        assert!(pending.land(12));
        assert!(pending.starred.as_ref().unwrap().value);
        assert!(pending.land(13));
        assert!(pending.starred.is_none());
        assert!(!pending.unread.as_ref().unwrap().value);
        pending.unread.as_mut().unwrap().through = Some(13);
        assert!(!pending.land(14));
    }

    #[test]
    fn folder_labels_keep_the_leaf_and_filter_match_in_view() {
        let fit = |path, capacity, mode| {
            fit_folder_path(
                path,
                mode,
                |candidate| {
                    let len = candidate.chars().count();
                    Some(if len <= capacity {
                        candidate.to_owned()
                    } else {
                        let head: String = candidate.chars().take(capacity - 1).collect();
                        crate::present::ellipsis(&head)
                    })
                },
                String::as_str,
            )
            .unwrap()
        };
        let path = "Projects/2026/Field survey";
        let picker = FolderFit::Picker("");
        assert_eq!(fit(path, 40, picker), path);
        assert_eq!(fit(path, 23, picker), "Projects/…/Field survey");
        assert_eq!(fit(path, 17, picker), "…/Field survey");
        assert_eq!(fit(path, 11, picker), "…survey");
        assert_eq!(fit(path, 10, picker), "…survey");
        assert_eq!(fit(path, 10, FolderFit::Picker("field")), "Field…");
        assert_eq!(fit(path, 10, FolderFit::Picker("2026")), "…2026…");
        let long = "Projects/2026/Field survey equipment provisioning for remote area planning and regional coordination";
        assert_eq!(
            fit(long, 41, picker),
            "…area planning and regional coordination"
        );
        let matched = fit(long, 45, FolderFit::Picker("equipment provisioning"));
        assert_eq!(matched, "…equipment provisioning … coordination");
        for query in ["equ", "equipme", "equipment"] {
            assert_eq!(
                fit(long, 45, FolderFit::Picker(query)),
                "…equipment … and regional coordination"
            );
        }
        assert_eq!(fit(path, 10, FolderFit::Search), "…survey");
        assert_eq!(fit(long, 16, FolderFit::Search), "…coordination");
        let sibling = long.replace("coordination", "training");
        assert_eq!(fit(&sibling, 16, FolderFit::Search), "…training");
        assert_eq!(fit(path, 17, FolderFit::Header), "Projects/…/Field…");
        assert_eq!(fit("Field survey", 8, FolderFit::Search), "…survey");
        for mode in [FolderFit::Picker("2026"), FolderFit::Header] {
            assert_eq!(fit(path, 5, mode), "Fiel…");
            assert_eq!(fit(path, 1, mode), "…");
        }
        assert!(fit_folder_path(path, FolderFit::Search, |_| Some("…"), |line| line).is_none());
    }

    #[test]
    fn folder_picker_fits_complete_rows_at_the_end() {
        assert_eq!(picker_card_height([false; 20].into_iter(), 692.), 360.);
        assert_eq!(picker_card_height([false; 20].into_iter(), 331.), 326.);
        let mut dividers = [false; 20];
        dividers[16] = true;
        assert_eq!(picker_card_height(dividers.into_iter(), 692.), 369.);
        assert_eq!(picker_card_height([false, true].into_iter(), 692.), 131.);
    }

    fn row(id: &str) -> super::ThreadRow {
        use crate::present::NameSpan;
        super::ThreadRow {
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
            server_draft: None,
        }
    }

    #[test]
    fn optimistic_label_edits_survive_stale_pages_until_the_commit_is_read() {
        use super::{LabelEdit, PendingLabels};

        let mut pending = PendingLabels::default();
        let add = LabelEdit {
            ids: vec!["a".into()],
            name: "field notes".into(),
            add: true,
        };
        let first = pending.start(add.clone());
        let mut rows = [row("a"), row("b")];
        let mut names = Vec::new();
        pending.land(7, &mut rows, Some(&mut names), None);
        assert_eq!(rows[0].labels, ["field notes"]);
        assert!(rows[1].labels.is_empty());
        assert_eq!(names, ["field notes"]);

        let second = pending.start(LabelEdit { add: false, ..add });
        pending.finish(second, Some(8));
        // A callback for the later edit may arrive first. Its removal
        // still wins over the older optimistic add on the same row.
        pending.land(9, &mut rows, None, None);
        assert!(rows[0].labels.is_empty());
        pending.finish(first, Some(9));

        let mut lagged = [row("a")];
        lagged[0].labels.push("field notes".into());
        pending.land(9, &mut lagged, None, Some("field notes"));
        assert_eq!(lagged[0].labels, ["field notes"]);
        pending.land(9, &mut lagged, None, None);
        assert!(lagged[0].labels.is_empty());

        let mut current = [row("a")];
        let mut current_names = Vec::new();
        pending.land(10, &mut current, Some(&mut current_names), None);
        assert!(pending.edits.is_empty());
        assert!(current[0].labels.is_empty());
        assert!(current_names.is_empty());
    }

    #[test]
    fn a_page_hidden_by_pending_removals_does_not_request_another_read() {
        use super::{HashSet, prepare_list_rows};
        use crate::keymap::Keymap;
        use crate::model::{Session, SyncKind};

        let mut session = Session::new("A", "a@x", SyncKind::FixtureOffline, Keymap::default());
        session.set_page(vec![row("a"), row("b")], false, true, 3);
        session.remove_thread("a");
        session.remove_thread("b");
        assert!(session.take_fetch().is_some(), "the initial lookahead");
        let removals = HashSet::from(["a".to_owned(), "b".to_owned()]);
        for _ in 0..2 {
            let mut rows = vec![row("a"), row("b")];
            let mut unread = 3;
            let (before, after) =
                prepare_list_rows(&mut rows, &mut unread, Some(&removals), (false, true), true)
                    .expect("the store page still had rows");
            assert!(rows.is_empty());
            assert_eq!(unread, 1);
            session.set_page(rows, before, after, unread);
            assert!(!session.has_fetch(), "wait for the removal's own read");
        }
        assert_eq!(
            prepare_list_rows(
                &mut Vec::new(),
                &mut 1,
                Some(&removals),
                (true, false),
                true,
            ),
            None
        );
    }

    #[test]
    fn a_second_of_frame_rate_download_ticks_keeps_the_latest_count_visible() {
        use super::{Motion, StatusLabel, StatusLabels, StatusPhase};

        let mut labels = StatusLabels::default();
        let start = Instant::now();
        let next = |done| StatusLabel {
            phase: StatusPhase::Download,
            text: String::new(),
            download: SyncKind::Syncing {
                new_messages: None,
                progress: Some(fell_core::SyncProgress {
                    folder: "Inbox".into(),
                    total: 85,
                    done,
                }),
            }
            .download_status(),
            color: 0,
            steady_sync: false,
        };
        for count in 0..=60 {
            let now = start + Duration::from_secs_f64(f64::from(count) / 60.);
            let frame = labels.update(next(count), Motion::new(true), now);
            let current = &frame.current.download.as_ref().unwrap().done;
            let fading = frame.fading_count();
            let dominant = match fading {
                Some((old, shown)) if shown < 0.5 => old,
                _ => current,
            };
            assert_eq!(current, &count.to_string());
            if count >= 3 {
                assert_eq!(dominant, current, "frame {count}");
            }
            if count >= 9 {
                assert!(fading.is_none(), "a busy stream never restarts the fade");
            }
        }
        let next_burst = start + Duration::from_secs(2);
        let frame = labels.update(next(61), Motion::new(true), next_burst);
        assert_eq!(frame.fading_count(), Some(("60", 0.)));
    }

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
    fn archive_undo_selects_each_restored_row_and_the_first_of_a_batch() {
        use super::{ListShift, Motion, ROW_PX, restored_folder_key};
        use crate::fixtures::{fixture_account, seed_if_empty};
        use crate::keymap::{KeyId, Keymap};
        use crate::model::{Session, SyncKind};
        use crate::present::{draft_row_id, thread_rows};
        use fell_core::{FolderSpan, ListKey, Store};

        let store = Store::open_in_memory().unwrap();
        let account = fixture_account();
        store.upsert_account(&account).unwrap();
        seed_if_empty(&store, &account).unwrap();
        let folder = "INBOX";
        let list = || {
            store
                .folder_page(&account.id, folder, &FolderSpan::Top { rows: 100 })
                .unwrap()
                .threads
        };
        let original = list();
        let mut session = Session::new(
            "Demo",
            &account.email,
            SyncKind::FixtureOffline,
            Keymap::default(),
        );
        session.set_threads(thread_rows(&original, &account.email));
        let keys: Vec<_> = original.iter().skip(2).take(3).map(ListKey::of).collect();
        session.select_if_present(keys[0].thread_id.as_str());
        for key in &keys {
            assert_eq!(session.cursor_id.as_deref(), Some(key.thread_id.as_str()));
            store
                .archive_thread(&account.id, &key.thread_id, folder)
                .unwrap();
            session.remove_thread(key.thread_id.as_str());
        }

        let restore = |session: &mut Session, expected: &ListKey| {
            let before = session
                .threads
                .iter()
                .enumerate()
                .map(|(index, row)| (row.id.clone(), index as f32 * ROW_PX))
                .collect();
            assert!(session.on_key(KeyId::Char('u'), 10).undo);
            let (_, restored) = store.undo_with_threads(&account.id).unwrap();
            let selected = restored_folder_key(&store, &account.id, folder, &restored);
            assert_eq!(selected.as_ref(), Some(expected));
            let page = list();
            session.set_threads(thread_rows(&page, &account.email));
            assert!(session.select_if_present(selected.unwrap().thread_id.as_str()));
            assert_eq!(
                session.cursor_id.as_deref(),
                Some(expected.thread_id.as_str())
            );
            let cursor = session.cursor_index();
            let cursor_id = session.cursor_id.as_deref().unwrap();
            assert!(session.highlighted().contains(cursor_id));
            let top = cursor as f32 * ROW_PX;
            let below = session
                .threads
                .iter()
                .enumerate()
                .skip(cursor + 1)
                .map(|(index, row)| (row.id.as_str(), index as f32 * ROW_PX));
            let first = std::time::Instant::now();
            let mut shift = ListShift::<()>::start(
                &Motion::new(true),
                before,
                vec![],
                vec![],
                640.0,
                false,
                first,
            )
            .unwrap()
            .show_restored_selection();
            let row = shift.row(cursor_id, top, ROW_PX, below, first);
            assert!(
                row.reveal.is_some(),
                "an adjacent survivor makes room for the row"
            );
            let highlight = row.visible_height(ROW_PX, 1.0);
            assert!(highlight > 0.0, "first-frame highlight: {highlight}");
            restored
        };
        for key in keys.iter().rev() {
            assert_eq!(
                restore(&mut session, key),
                std::slice::from_ref(&key.thread_id)
            );
        }
        let mut draft = session.threads[0].clone();
        draft.id = draft_row_id("unsent");
        draft.draft = Some("unsent".into());
        let selected = vec![
            draft.id.clone(),
            keys[2].thread_id.0.clone(),
            keys[1].thread_id.0.clone(),
            keys[0].thread_id.0.clone(),
        ];
        session.threads.insert(0, draft);
        let (batch, skipped) = session.split_removal_targets(selected);
        assert_eq!(skipped, ["unsent"]);
        let batch: Vec<_> = batch.into_iter().map(fell_core::ThreadId).collect();
        store.archive_threads(&account.id, &batch, folder).unwrap();
        session.set_threads(thread_rows(&list(), &account.email));
        let restored = restore(&mut session, &keys[0]);
        assert_eq!(restored.len(), batch.len());
        assert!(batch.iter().all(|id| restored.contains(id)));
        assert!(matches!(
            store.undo_with_threads(&account.id),
            Err(fell_core::Error::Action(message)) if message == "nothing to undo"
        ));
        assert_eq!(
            list().iter().map(ListKey::of).collect::<Vec<_>>(),
            original.iter().map(ListKey::of).collect::<Vec<_>>()
        );
    }

    #[test]
    fn first_preview_releases_pending_bodies_when_sync_stops() {
        use crate::model::SyncFailure;
        use fell_core::SyncFailureKind;

        let active = SyncKind::Syncing {
            new_messages: None,
            progress: None,
        };
        for terminal in [
            SyncKind::LocalOffline,
            SyncKind::StoreError,
            SyncKind::Synced {
                at: 0,
                changed: false,
                new_messages: None,
            },
            SyncKind::Failed(SyncFailure {
                kind: SyncFailureKind::Unreachable,
                bridge: false,
                detail: "fixture stopped".into(),
                recovery: None,
            }),
        ] {
            let mut preview = FirstPreview::Handoff;
            assert!(!preview.sync_changed(&SyncKind::Connecting));
            assert!(!preview.sync_changed(&active));
            assert!(preview.sync_changed(&terminal));
            assert!(!preview.waits_for_body());
            assert!(
                preview.take(),
                "the fallback still gets the automatic landing"
            );
            assert!(!preview.take());
        }
    }

    #[test]
    fn first_preview_times_out_after_the_setup_exit_finishes() {
        let mut preview = FirstPreview::Handoff;
        let exit = Instant::now() + Duration::from_secs(3);
        assert!(!preview.expire(exit));
        assert!(preview.finish_handoff(exit));
        assert!(!preview.finish_handoff(exit));
        assert!(!preview.expire(exit + Duration::from_millis(1499)));
        assert!(preview.waits_for_body());
        assert!(preview.expire(exit + FIRST_PREVIEW_WAIT));
        assert!(!preview.waits_for_body());
        assert!(preview.take());
    }

    pub(super) fn with_pending_inbox(
        test: impl FnOnce(&mut super::Inbox, &mut gpui::Context<super::Inbox>),
    ) {
        use gpui::TestAppContext;

        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        store.upsert_account(&account).unwrap();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let page = store
            .folder_page(
                &account.id,
                "INBOX",
                &fell_core::FolderSpan::Top { rows: 1 },
            )
            .unwrap();
        let mut rows = crate::present::thread_rows(&page.threads, &account.email);
        let row = rows.remove(0);
        let mut messages = super::read_conversation(
            &store,
            &account.id,
            &fell_core::ThreadId(row.id.clone()),
            (
                &account.email,
                &fell_core::FolderRoles::default(),
                &[],
                "INBOX",
            ),
        )
        .unwrap();
        messages[0].body_state = fell_core::BodyState::Pending;

        let mut app = TestAppContext::single();
        let inbox = app.add_window(|window, cx| {
            super::Inbox::with_startup(
                crate::mailbox::Startup {
                    keymap: Default::default(),
                    split: 0.4,
                    read_dwell_ms: 0,
                    error: None,
                    account_missing: true,
                    config: None,
                    path: Err("in-memory test".into()),
                },
                false,
                Some(window),
                cx,
            )
        });
        inbox
            .update(&mut app, |inbox, _, cx| {
                inbox.motion = super::InboxMotion::new(motion::Motion::new(true));
                inbox.session.set_messages(messages);
                inbox.session.cursor_id = Some(row.id.clone());
                inbox.mark_open(row.id.clone(), None);
                inbox.session.set_threads(vec![row]);
                inbox.pending_bodies.wait_for_read(0);
                inbox.conv_gen.store(1, std::sync::atomic::Ordering::SeqCst);
                test(inbox, cx);
                assert!(!inbox.pending_bodies.downloading(&inbox.session.sync));
            })
            .unwrap();
    }

    #[test]
    fn pending_bodies_settle_without_a_cursor() {
        with_pending_inbox(|inbox, cx| {
            inbox.session.cursor_id = None;
            inbox.read_selected_messages(true, cx);
            assert!(inbox.session.messages.is_empty());
            assert!(
                inbox
                    .pending_bodies
                    .fade(inbox.motion.clock.now())
                    .is_none()
            );
        });
    }

    #[test]
    fn pending_bodies_settle_when_switching_to_a_draft() {
        with_pending_inbox(|inbox, cx| {
            inbox.session.threads[0].draft = Some("local-draft".into());
            inbox.read_selected_messages(true, cx);
            assert!(inbox.session.messages.is_empty());
            assert!(
                inbox
                    .pending_bodies
                    .fade(inbox.motion.clock.now())
                    .is_none()
            );
        });
    }

    #[test]
    fn pending_bodies_fade_when_the_conversation_read_fails() {
        with_pending_inbox(|inbox, cx| {
            let id = inbox.open_thread.clone().unwrap();
            inbox.receive_conversation(id, 1, Err("failed to read thread".into()), cx);
            assert!(
                inbox
                    .pending_bodies
                    .fade(inbox.motion.clock.now())
                    .is_some()
            );
        });
    }

    #[test]
    fn pending_bodies_wait_on_the_folder_read_after_the_open_thread_reads_empty() {
        with_pending_inbox(|inbox, cx| {
            let id = inbox.open_thread.clone().unwrap();
            inbox.read_in_flight = true;
            let reply = super::ConversationRead {
                notes: None,
                latest_note: None,
                note_generation: 0,
                view: Vec::new(),
                reads: Default::default(),
                matches: None,
            };
            inbox.receive_conversation(id, 1, Ok(Some(reply)), cx);
            assert!(inbox.pending_bodies.downloading(&inbox.session.sync));
            assert!(
                inbox
                    .pending_bodies
                    .fade(inbox.motion.clock.now())
                    .is_none()
            );
            let id = inbox.open_thread.clone().unwrap();
            let reply = super::ConversationRead {
                notes: None,
                latest_note: None,
                note_generation: 0,
                view: inbox.session.messages.clone(),
                reads: Default::default(),
                matches: None,
            };
            inbox.receive_conversation(id, 1, Ok(Some(reply)), cx);
            let now = inbox.motion.clock.now();
            inbox.apply_body_refresh(now);
        });
    }

    #[test]
    fn pending_bodies_fade_when_the_folder_read_fails() {
        with_pending_inbox(|inbox, cx| {
            inbox.show_threads(
                (inbox.session.folder.clone(), None),
                Err("failed to read folder".into()),
                super::ReadIntent::default(),
                false,
                cx,
            );
            assert!(
                inbox
                    .pending_bodies
                    .fade(inbox.motion.clock.now())
                    .is_some()
            );
        });
    }

    #[test]
    fn pending_bodies_settle_during_stale_search_and_reread_the_same_preview() {
        use crate::model::search::{Hit, SearchReply};
        use std::sync::atomic::Ordering;

        with_pending_inbox(|inbox, cx| {
            let row = inbox.session.threads[0].clone();
            let reply = || SearchReply {
                rows: vec![row.clone()],
                hits: [(
                    row.id.clone(),
                    Hit {
                        message: fell_core::MessageId("fixture-message".into()),
                        related: false,
                        preview: None,
                        folder: Some("INBOX".into()),
                    },
                )]
                .into(),
                correction: None,
                error: None,
            };
            inbox.session.on_key(KeyId::Char('/'), 10);
            inbox.session.type_search("field");
            assert!(
                inbox
                    .session
                    .apply_search_reply(inbox.session.search_generation, &mut reply())
            );
            inbox.session.type_search(" notes");
            assert!(!inbox.session.search_results_current());

            inbox.read_selected_messages(true, cx);
            assert_eq!(
                crate::present::missing_body_line(
                    fell_core::BodyState::Pending,
                    inbox.pending_bodies.downloading(&inbox.session.sync),
                )
                .as_deref(),
                Some("Downloading…")
            );
            assert!(
                inbox
                    .pending_bodies
                    .fade(inbox.motion.clock.now())
                    .is_none()
            );
            let before = inbox.conv_gen.load(Ordering::SeqCst);

            inbox.show_search_reply(inbox.session.search_generation, reply(), cx);
            assert_eq!(inbox.conv_gen.load(Ordering::SeqCst), before + 1);
            assert_eq!(inbox.open_thread.as_deref(), Some(row.id.as_str()));
            assert!(inbox.pending_bodies.downloading(&inbox.session.sync));

            let mut view = inbox.session.messages.clone();
            view[0].body_state = fell_core::BodyState::Stored;
            inbox.receive_conversation(
                row.id,
                before + 1,
                Ok(Some(super::ConversationRead {
                    notes: None,
                    latest_note: None,
                    note_generation: 0,
                    view,
                    reads: Default::default(),
                    matches: None,
                })),
                cx,
            );
            let now = inbox.motion.clock.now();
            inbox.apply_body_refresh(now);
            assert!(
                inbox
                    .pending_bodies
                    .fade(inbox.motion.clock.now())
                    .is_none()
            );
        });
    }

    #[test]
    fn pending_bodies_settle_when_the_search_reply_fails() {
        with_pending_inbox(|inbox, cx| {
            inbox.session.on_key(KeyId::Char('/'), 10);
            inbox.session.type_search("field");
            inbox.session.cursor_id.clone_from(&inbox.open_thread);
            inbox.read_selected_messages(true, cx);
            assert!(inbox.search_view.refresh_open_after_reply);
            assert!(inbox.pending_bodies.downloading(&inbox.session.sync));

            inbox.show_search_reply(
                inbox.session.search_generation,
                crate::model::search::SearchReply {
                    rows: Vec::new(),
                    hits: Default::default(),
                    correction: None,
                    error: Some("failed to search".into()),
                },
                cx,
            );
            assert!(!inbox.search_view.refresh_open_after_reply);
            assert!(
                inbox
                    .pending_bodies
                    .fade(inbox.motion.clock.now())
                    .is_some()
            );
        });
    }

    #[test]
    fn open_search_match_holds_its_height_through_the_post_sync_reread() {
        use crate::model::search::{Hit, MatchLanding, SearchReply};
        use fell_core::{MessageId, SearchMatch, SearchPreview};

        with_pending_inbox(|inbox, cx| {
            let row = inbox.session.threads[0].clone();
            let message = MessageId(inbox.session.messages[0].id.clone());
            let reply = || SearchReply {
                rows: vec![row.clone()],
                hits: [(
                    row.id.clone(),
                    Hit {
                        message: message.clone(),
                        related: false,
                        preview: None,
                        folder: Some("INBOX".into()),
                    },
                )]
                .into(),
                correction: None,
                error: None,
            };
            let hit = |text: &str| SearchMatch {
                message: message.clone(),
                preview: Some(SearchPreview {
                    text: format!("Harbor {text}"),
                    highlights: std::iter::once(0..6).collect(),
                }),
            };
            let now = Instant::now();
            inbox.session.on_key(KeyId::Char('/'), 10);
            inbox.session.type_search("Harbor");
            let generation = inbox.session.search_generation;
            assert!(inbox.session.apply_search_reply(generation, &mut reply()));
            inbox.session.search.as_mut().unwrap().editing = false;
            inbox.session.pane = super::Pane::Conversation;
            inbox.session.set_search_matches(
                generation,
                &row.id,
                vec![hit("checkpoint")],
                MatchLanding::Best,
            );
            let line = |inbox: &mut super::Inbox, at| {
                let frame = inbox.search_match_frame(at, inbox.search_preview_current());
                (frame.reserved_height(&inbox.theme), frame.animating)
            };
            let (height, animating) = line(inbox, now);
            assert!(height > 0.);
            assert!(!animating);

            let mut previous_height = height;
            for (step, (matches, target_height, changes)) in [
                (vec![hit("checkpoint")], height, false),
                (vec![hit("checkpoint revised")], height, true),
                (Vec::new(), 0., true),
                (vec![hit("checkpoint")], height, true),
            ]
            .into_iter()
            .enumerate()
            {
                let now = now + Duration::from_secs(step as u64 + 1);
                if step == 0 {
                    inbox.session.search.as_mut().unwrap().editing = true;
                    inbox.session.type_search(" checkpoint");
                    inbox.session.search.as_mut().unwrap().editing = false;
                    inbox.read_selected_messages(true, cx);
                    assert!(inbox.search_view.refresh_open_after_reply);
                } else {
                    assert!(inbox.session.refresh_search(now));
                }
                assert_eq!(line(inbox, now), (previous_height, false));
                let generation = inbox.session.search_generation;
                inbox.show_search_reply(generation, reply(), cx);
                assert!(!inbox.search_preview_current());
                assert!(inbox.quiet_search_preview());
                assert_eq!(line(inbox, now), (previous_height, false));

                let read = inbox.conv_gen.load(std::sync::atomic::Ordering::SeqCst);
                inbox.receive_conversation(
                    row.id.clone(),
                    read,
                    Ok(Some(super::ConversationRead {
                        notes: None,
                        latest_note: None,
                        note_generation: 0,
                        view: inbox.session.messages.clone(),
                        reads: Default::default(),
                        matches: Some((generation, matches)),
                    })),
                    cx,
                );
                inbox.apply_body_refresh(now);
                assert!(inbox.search_preview_current());
                assert_eq!(line(inbox, now), (previous_height, changes));
                let (halfway, _) = line(inbox, now + motion::FADE / 2);
                if previous_height != target_height {
                    assert!(halfway > 0. && halfway < height);
                } else {
                    assert_eq!(halfway, height);
                }
                assert_eq!(line(inbox, now + motion::FADE), (target_height, false));
                previous_height = target_height;
            }
        });
    }

    #[test]
    fn final_body_refresh_crossfades_only_bodies_still_missing() {
        use super::{BodyRefresh, ConvFrame, InboxMotion, PendingBodies, Session};
        use crate::{fixtures, keymap::Keymap, present};
        use fell_core::{BodyState, FolderRoles, Store};

        let store = Store::open_in_memory().unwrap();
        let account = fixtures::fixture_account();
        store.upsert_account(&account).unwrap();
        fixtures::seed_if_empty(&store, &account).unwrap();
        let id = &store.list_threads(&account.id).unwrap()[0].thread_id;
        let mut stored = super::read_conversation(
            &store,
            &account.id,
            id,
            (&account.email, &FolderRoles::default(), &[], "INBOX"),
        )
        .unwrap()
        .remove(0);
        stored.expanded = true;
        let mut missing = stored.clone();
        missing.id = "missing@demo.test".into();
        missing.body_state = BodyState::Pending;
        let mut was_pending = stored.clone();
        was_pending.body_state = BodyState::Pending;
        let mut session = Session::new(
            String::new(),
            account.email,
            SyncKind::LocalOffline,
            Keymap::default(),
        );
        session.set_messages(vec![missing.clone(), was_pending]);

        let now = Instant::now();
        let mut motion = InboxMotion::new(motion::Motion::new(true));
        let mut pending = PendingBodies::default();
        pending.wait_for_read(7);
        pending.applied(7, true, motion.motion, now);
        assert!(
            pending.downloading(&session.sync),
            "the old read cannot settle the pass"
        );
        assert_eq!(
            present::missing_body_line(missing.body_state, pending.downloading(&session.sync))
                .as_deref(),
            Some("Downloading…")
        );
        assert!(pending.fade(now).is_none());

        BodyRefresh {
            messages: Some((vec![missing, stored], 8)),
            ..BodyRefresh::default()
        }
        .apply(
            &mut session,
            &mut motion,
            now,
            |_| Some(100.),
            pending.downloading(&SyncKind::LocalOffline),
        );
        pending.applied(
            8,
            present::awaiting_bodies(&session.messages),
            motion.motion,
            now,
        );
        let frames = |at| ConvFrame {
            syncing: pending.downloading(&session.sync),
            missing_fade: pending.fade(at),
            alphas: motion.body_alphas(at),
            heights: motion.body_heights(at),
            outgoing_placeholders: motion.body_placeholders(),
            ..ConvFrame::default()
        };
        let first = frames(now);
        let [missing, ready] = &session.messages[..] else {
            panic!("two messages")
        };
        assert_eq!(
            first.body_labels(missing),
            (0., Some("Downloading…".into()))
        );
        assert_eq!(first.body_labels(ready), (0., Some("Downloading…".into())));
        assert_eq!(first.heights[&ready.id], (100., 0.));
        let next = frames(now + Duration::from_millis(16));
        assert_eq!(next.body_labels(ready).0, next.heights[&ready.id].1);
        assert!(next.body_labels(ready).0 > 0.);
        assert!(next.body_labels(missing).0 > 0.);
        assert!(!next.heights.contains_key(&missing.id));
        let settled = frames(now + motion::FADE);
        assert_eq!(settled.body_labels(missing), (1., None));
        assert_eq!(
            present::missing_body_line(missing.body_state, settled.syncing).as_deref(),
            Some("Not downloaded yet")
        );
        assert_eq!(
            present::missing_body_line(ready.body_state, settled.syncing),
            None
        );
    }

    #[test]
    #[ignore = "requires an isolated nested Wayland compositor"]
    fn inbox_label_stays_put_with_late_total_and_growing_counts() {
        use crate::{keymap::Keymap, model::Session};
        use gpui::{App, Context, Render, Window, prelude::*, px};

        require_nested_wayland();

        struct Headers(Session, crate::theme::Theme);
        impl Render for Headers {
            fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let now = Instant::now();
                div().flex().flex_col().children([360., 980.].map(|width| {
                    let mut motion = super::InboxMotion::new(motion::Motion::new(true));
                    div().w(px(width)).children(
                        [
                            None,
                            Some(61),
                            Some(1061),
                            Some(7727),
                            Some(u32::MAX as usize),
                        ]
                        .into_iter()
                        .enumerate()
                        .map(|(step, unread)| {
                            let at = now + Duration::from_millis(step as u64 * 140);
                            motion.observe_header("INBOX", None, unread, at);
                            super::header(
                                &self.1,
                                &self.0,
                                ("INBOX", None),
                                &mut motion,
                                at + Duration::from_millis(60),
                                width,
                                window,
                            )
                        }),
                    )
                }))
            }
        }

        gpui::Application::new().run(|cx: &mut App| {
            let window = cx
                .open_window(gpui::WindowOptions::default(), |_, cx| {
                    cx.new(|_| {
                        Headers(
                            Session::new(
                                "Robin Ellis",
                                "robin@demo.test",
                                SyncKind::LocalOffline,
                                Keymap::default(),
                            ),
                            crate::theme::load(),
                        )
                    })
                })
                .unwrap();
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                cx.update(|cx| {
                    window
                        .update(cx, |_, window, cx| {
                            window.defer(cx, |window, cx| {
                                INBOX_LABEL_X.with_borrow_mut(Vec::clear);
                                window.draw(cx).clear();
                                INBOX_LABEL_X.with_borrow(|positions| {
                                    assert_eq!(positions.len(), 10);
                                    for width in positions.as_chunks::<5>().0 {
                                        assert!(width[0].0 > px(0.));
                                        assert!(
                                            width.iter().all(|(x, text)| *x == width[0].0
                                                && text.as_ref() == "Inbox"),
                                            "{width:?}"
                                        );
                                    }
                                });
                                cx.quit();
                            });
                        })
                        .unwrap();
                })
                .unwrap();
            })
            .detach();
        });
    }

    fn require_nested_wayland() {
        use std::os::unix::fs::FileTypeExt;
        use std::path::PathBuf;
        let display = PathBuf::from(std::env::var_os("WAYLAND_DISPLAY").unwrap());
        let runtime = PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR").unwrap());
        assert_eq!(
            std::env::var_os("FELL_TEST_NESTED_WAYLAND").as_deref(),
            Some(display.as_os_str())
        );
        assert!(display.is_absolute() && display.parent() == Some(runtime.as_path()));
        assert!(display.metadata().unwrap().file_type().is_socket());
        assert!(std::env::var_os("DISPLAY").is_none());
    }

    #[test]
    #[ignore = "requires an isolated nested Wayland compositor"]
    fn opening_landings_use_the_real_conversation_end_probe() {
        use super::{
            MessageProbes, PreviewPadding, ReaderLanding, ScrollAnchor, conversation_body,
        };
        use gpui::{App, Context, IntoElement, Render, ScrollHandle, Window, div, prelude::*, px};
        use std::cell::Cell;
        use std::rc::Rc;

        require_nested_wayland();

        struct Case {
            heights: &'static [f32],
            automatic: bool,
            pending: bool,
            expected_header: Option<usize>,
            newest_height: Rc<Cell<gpui::Pixels>>,
            scroll: ScrollHandle,
        }

        struct Landings(Vec<Case>);

        impl Render for Landings {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .flex()
                    .children(self.0.iter().enumerate().map(|(index, case)| {
                        let mut landing = ReaderLanding::default();
                        landing.hold(Some("newest"));
                        let padding = case.automatic.then(|| PreviewPadding {
                            viewport: px(340.),
                            newest_height: case.newest_height.clone(),
                        });
                        let anchor = landing
                            .anchor(
                                Some("newest"),
                                ScrollAnchor {
                                    index: case.heights.len() - 1,
                                    y: px(0.),
                                    snap_headers: case.pending,
                                    body_scroll: None,
                                },
                                false,
                            )
                            .unwrap();
                        let probes = MessageProbes::new(anchor, case.heights.len(), None);
                        let cards: Vec<_> = case
                            .heights
                            .iter()
                            .enumerate()
                            .map(|(index, height)| {
                                let card = div().h(px(*height)).mb(px(8.)).into_any_element();
                                let card = match &padding {
                                    Some(padding) if index + 1 == case.heights.len() => {
                                        padding.wrap_newest(card)
                                    }
                                    _ => card,
                                };
                                probes.wrap(index, card)
                            })
                            .collect();
                        div()
                            .id(("case", index))
                            .flex()
                            .flex_col()
                            .w(px(125.))
                            .h(px(300.))
                            .child(conversation_body(
                                cards,
                                Some(case.scroll.clone()),
                                Some(probes),
                                None,
                                padding.map(|padding| padding.viewport),
                                1.,
                                None,
                            ))
                    }))
            }
        }

        let checked = Rc::new(Cell::new(false));
        let completed = checked.clone();
        gpui::Application::new().run(move |cx: &mut App| {
            let handle = cx
                .open_window(gpui::WindowOptions::default(), |_, cx| {
                    cx.new(|_| {
                        Landings(
                            [
                                (&[160., 90., 200., 80.][..], true, false, Some(3)),
                                (&[160., 90., 200., 68.][..], true, false, Some(3)),
                                (&[390., 430., 640., 230.][..], true, false, Some(3)),
                                (&[60., 80.][..], true, false, Some(1)),
                                (&[160., 400.][..], true, false, Some(1)),
                                (&[60., 280., 40.][..], false, true, None),
                            ]
                            .into_iter()
                            .map(|(heights, automatic, pending, expected_header)| Case {
                                heights,
                                automatic,
                                pending,
                                expected_header,
                                scroll: ScrollHandle::new(),
                                newest_height: Rc::default(),
                            })
                            .collect(),
                        )
                    })
                })
                .unwrap();
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(40))
                    .await;
                cx.update(|cx| {
                    let entity = handle.entity(cx).unwrap();
                    handle
                        .update(cx, |_, window, cx| {
                            window.defer(cx, move |window, cx| {
                                window.draw(cx).clear();
                                for case in &entity.read(cx).0 {
                                    let view = case.scroll.bounds();
                                    let newest = case
                                        .scroll
                                        .bounds_for_item(case.heights.len() - 1)
                                        .unwrap();
                                    let offset = case.scroll.offset().y;
                                    if let Some(header) = case.expected_header {
                                        let header = case.scroll.bounds_for_item(header).unwrap();
                                        assert_eq!(header.top() + offset, view.top());
                                    } else {
                                        assert_eq!(offset, -case.scroll.max_offset().height);
                                        assert_eq!(offset, px(-124.));
                                    }
                                    let height = *case.heights.last().unwrap();
                                    if case.automatic {
                                        assert_eq!(case.newest_height.get(), px(height));
                                    }
                                    if px(height) <= view.size.height {
                                        assert!(
                                            newest.top() + offset + px(height) <= view.bottom()
                                        );
                                    }
                                    assert!(newest.top() + offset >= view.top());
                                    assert!(offset >= -case.scroll.max_offset().height);
                                }
                                completed.set(true);
                                cx.quit();
                            });
                        })
                        .unwrap();
                })
                .unwrap();
            })
            .detach();
        });
        assert!(checked.get());
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
    fn expanded_header_field_widths_hold_during_the_reading_slide() {
        let long = [900.0, 450.0, 250.0, 55.0];
        let short = [80.0, 120.0, 60.0, 55.0];
        for (width, split) in [(760.0, 0.0), (1000.0, 387.0), (1920.0, 724.0)] {
            let columns_at =
                |reading, progress| ReaderColumns::new(width, split, reading, progress, 2.0);
            let measure = |columns, fields| {
                let mut layers = Vec::new();
                expanded_header_flow(columns, fields, |widths| {
                    layers.push(widths);
                    div().into_any_element()
                });
                layers
            };
            let [split_layer, reading_layer] = [
                measure(columns_at(false, 0.0), long)[0],
                measure(columns_at(true, 1.0), long)[0],
            ];
            assert_ne!(split_layer, reading_layer);
            for reading in [false, true] {
                for step in 0..=100 {
                    let progress = if reading { step } else { 100 - step } as f32 / 100.0;
                    let columns =
                        columns_at(reading, progress).with_reflow(Some(motion::ReadingReflow {
                            alpha: [1.0 - progress, progress],
                            height: progress,
                        }));
                    assert_eq!(measure(columns, long), [split_layer, reading_layer]);
                    assert_eq!(measure(columns, short), [short]);
                    let card_right = columns.card.inset + columns.card.width + 2.0 * MESSAGE_PAD_PX;
                    assert_eq!(
                        MESSAGE_PAD_PX + columns.card_right(),
                        card_right - MESSAGE_PAD_PX
                    );
                }
            }
        }
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

    #[test]
    fn delete_trashes_exactly_the_marks_across_pages() {
        use super::{Removal, Session, SyncKind};
        use crate::keymap::Keymap;
        use crate::present::thread_rows;

        for mark_on_second_page in [false, true] {
            let store = fell_core::Store::open_in_memory().unwrap();
            let account = crate::fixtures::fixture_account();
            crate::fixtures::seed_if_empty(&store, &account).unwrap();
            let inbox = store.folder_threads(&account.id, "INBOX").unwrap();
            let [first, second, cursor, ..] = inbox.as_slice() else {
                panic!("three fixture rows");
            };
            let ids = |rows: &[fell_core::store::FolderThread]| thread_rows(rows, &account.email);
            let mut session = Session::new(
                "Demo",
                &account.email,
                SyncKind::FixtureOffline,
                Keymap::default(),
            );
            session.set_page(ids(&inbox[..1]), false, true, 0);
            session.on_key(KeyId::Char('x'), 10);
            session.set_page(ids(&inbox[1..3]), true, false, 0);
            if mark_on_second_page {
                session.on_key(KeyId::Char('x'), 10);
            }
            session.on_key(KeyId::Char('j'), 10);
            assert_eq!(
                session.cursor_id.as_deref(),
                Some(cursor.thread_id.as_str())
            );
            assert!(session.on_key(KeyId::Char('#'), 10).delete);
            let (ids, drafts) = session.split_removal_targets(session.action_targets());
            assert!(drafts.is_empty());
            let selected = if mark_on_second_page {
                vec![second.thread_id.0.clone(), first.thread_id.0.clone()]
            } else {
                vec![first.thread_id.0.clone()]
            };
            assert_eq!(ids, selected);
            let mut threads: Vec<_> = ids.into_iter().map(fell_core::ThreadId).collect();
            threads.push(fell_core::ThreadId("missing".into()));
            let count = Removal::Delete
                .in_folder(&store, &account.id, threads, "INBOX".into())
                .unwrap();
            assert_eq!(count, selected.len());

            let trash = store.folder_threads(&account.id, "Trash").unwrap();
            for row in [first, second, cursor] {
                let trashed = trash.iter().any(|item| item.thread_id == row.thread_id);
                assert_eq!(trashed, selected.contains(&row.thread_id.0));
            }
            assert!(
                store
                    .folder_key_of(&account.id, "INBOX", &cursor.thread_id)
                    .unwrap()
                    .is_some()
            );
        }
    }
}
