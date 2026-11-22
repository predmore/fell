//! Inbox session: folders, selection, chords, palette. No drawing.

pub mod agents;
mod notes;
pub mod search;
pub use notes::{AgentNotes, NoteRequest, SUGGESTIONS_QUERY};

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use std::ops::Range;

use fell_core::compose::ReplyKind;
use fell_core::diagnostics::DiagnosticFile;
use fell_core::onboarding::connection_recovery;
use fell_core::{
    AccountConfig, BackendKind, Error, FlagChange, FolderRole, FolderRoles, FolderSpan, ListKey,
    NotificationMode, SyncFailureKind, ThreadId, classify_sync_error,
};

use crate::attachments::{Request as AttachmentRequest, Target as AttachmentTarget};
use crate::keymap::{self, Action, KeyId, Keymap, PaletteItem, SeqMatch, WhichItem};
use crate::present::{self, ConvMessage, NOTICE_DOT, Pane, ThreadRow, display_safe, merge_folders};
use crate::worker::Worker;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncKind {
    /// No account is set up, so the demo threads were loaded.
    FixtureOffline,
    /// The store already had mail and no account file is set up.
    LocalOffline,
    /// The store has an account but no mail, and no account file is set up.
    FirstSync,
    StoreError,
    /// The account file is set up and the first attempt has not answered yet.
    Connecting,
    /// A pass under way, with the count when the store can determine it.
    Syncing {
        new_messages: Option<usize>,
        /// The first sync of a folder under way, read newest first.
        progress: Option<fell_core::SyncProgress>,
    },
    /// Unix time the last pass finished and whether to refresh its mail.
    Synced {
        at: i64,
        changed: bool,
        /// The final count for this pass, if known.
        new_messages: Option<usize>,
    },
    Failed(SyncFailure),
}

/// `31882` as `31,882`.
fn grouped(count: u32) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncFailure {
    pub kind: SyncFailureKind,
    /// The backend is Proton Mail Bridge, so copy can name it.
    pub bridge: bool,
    pub detail: String,
    pub recovery: Option<&'static str>,
}

impl SyncFailure {
    pub fn connection(config: &AccountConfig, error: &Error) -> Self {
        Self {
            kind: classify_sync_error(error),
            bridge: config.backend == BackendKind::Bridge,
            detail: error.to_string(),
            recovery: connection_recovery(config, error),
        }
    }
}

impl SyncKind {
    pub fn status(&self) -> String {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs() as i64)
            .unwrap_or(0);
        self.status_at(now)
    }

    pub fn status_at(&self, now: i64) -> String {
        match self {
            Self::FixtureOffline => "fixture · offline".into(),
            Self::LocalOffline => "local · offline".into(),
            Self::FirstSync => "no account".into(),
            Self::StoreError => "mailbox error".into(),
            Self::Connecting => "connecting…".into(),
            Self::Syncing {
                progress,
                new_messages,
            } => {
                let mut status = "syncing".to_string();
                if let Some(progress) = progress {
                    status.push_str(&format!(
                        " {} · {} of {}",
                        progress.folder,
                        grouped(progress.done),
                        grouped(progress.total)
                    ));
                }
                if let Some(count) = new_messages.filter(|count| *count > 0) {
                    status.push_str(&format!(" · {count} new"));
                } else if progress.is_none() {
                    status.push('…');
                }
                status
            }
            Self::Synced {
                at, new_messages, ..
            } => {
                let ago = now.saturating_sub(*at).max(0);
                if ago < 60 {
                    match new_messages {
                        Some(count) if *count > 0 => format!("synced just now · {count} new"),
                        _ => "synced just now".into(),
                    }
                } else if ago < 3600 {
                    format!("synced {}m ago", ago / 60)
                } else {
                    format!("synced {}h ago", ago / 3600)
                }
            }
            Self::Failed(failure) => match failure.kind {
                SyncFailureKind::Unreachable if failure.bridge => {
                    "offline · Bridge not running".into()
                }
                SyncFailureKind::Unreachable => "offline · server unreachable".into(),
                SyncFailureKind::LoginRejected => "offline · login rejected".into(),
                SyncFailureKind::Password => "offline · password unavailable".into(),
                SyncFailureKind::Tls => "offline · TLS failed".into(),
                SyncFailureKind::Other => "offline · sync failed".into(),
            },
        }
    }

    /// Paint the status in the error color.
    pub fn is_error(&self) -> bool {
        matches!(self, Self::StoreError | Self::Failed(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlay {
    None,
    Palette,
    Folders,
    Help,
    About,
    Agents,
}

/// What confirming the folder picker does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderIntent {
    /// `gf` switches the list to the chosen folder.
    Go,
    /// `m` moves the open thread out of the folder on screen.
    Move,
    Label,
    LabelList,
}

/// One-line result of the last move or undo.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub alert: bool,
    pub hint: Option<(String, &'static str)>,
    pub label: Option<present::LabelNotice>,
}

impl Toast {
    pub fn with_hint(text: impl Into<String>, key: String, suffix: &'static str) -> Self {
        Self {
            text: text.into(),
            alert: false,
            hint: Some((key, suffix)),
            label: None,
        }
    }
}

#[derive(Clone, Debug)]
struct PurgePrompt {
    ids: Vec<String>,
    folder: String,
    notice: u64,
}

/// Rows the list wants from the store beyond the ones it holds. The view
/// turns this into the next folder read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fetch {
    /// Rows to add above the first loaded row.
    pub before: usize,
    /// Rows to add below the last loaded row.
    pub after: usize,
    /// Replace the loaded rows with the top or the bottom of the folder.
    pub jump: Option<Jump>,
    /// A cursor motion since a jump was asked for: drop that jump, so `G`
    /// then `j` before the bottom rows land does not still go to the end.
    pub drop_jump: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Jump {
    /// The first this many rows.
    Top(usize),
    /// The last this many rows.
    Bottom(usize),
}

impl Fetch {
    pub fn is_empty(&self) -> bool {
        self.before == 0 && self.after == 0 && self.jump.is_none() && !self.drop_jump
    }

    pub fn merge(self, later: Fetch) -> Fetch {
        Fetch {
            before: self.before.max(later.before),
            after: self.after.max(later.after),
            jump: if later.drop_jump {
                later.jump
            } else {
                later.jump.or(self.jump)
            },
            drop_jump: self.drop_jump || later.drop_jump,
        }
    }
}

/// Where the cursor goes once rows that are still loading arrive.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Pending {
    /// A motion ran past the last loaded row. Move this many more.
    Down(usize),
    /// A motion ran past the first loaded row.
    Up(usize),
    /// `G`: the last row of the folder.
    Last,
    /// `gg` or a count: this row, counted from the top of the folder.
    Row(usize),
    /// The selected row was archived as the last loaded one. Nothing is
    /// selected until the row that comes after `key` loads. Then select it
    /// and move `down` rows more, for motions made while it loaded.
    After { key: ListKey, down: usize },
}

#[derive(Clone, Copy, Debug)]
enum Motion {
    Forward(usize),
    Backward(usize),
}

fn step_index(cursor: usize, len: usize, motion: Motion) -> usize {
    if len == 0 {
        return 0;
    }
    let last = len - 1;
    let cursor = cursor.min(last);
    match motion {
        Motion::Forward(steps) => cursor.saturating_add(steps).min(last),
        Motion::Backward(steps) => cursor.saturating_sub(steps),
    }
}

/// One label change on the selected threads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LabelEdit {
    pub ids: Vec<String>,
    pub name: String,
    pub add: bool,
}

impl LabelEdit {
    pub fn include_name(&self, names: &mut Vec<String>) {
        if self.add && !names.contains(&self.name) {
            names.push(self.name.clone());
        }
    }

    pub fn apply(&self, rows: &mut [ThreadRow]) {
        for row in rows.iter_mut().filter(|row| self.ids.contains(&row.id)) {
            if self.add && !row.labels.contains(&self.name) {
                row.labels.push(self.name.clone());
            } else if !self.add {
                row.labels.retain(|label| label != &self.name);
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PickerChoice {
    pub name: String,
    pub title: String,
    pub detail: Option<String>,
    pub mark: Option<&'static str>,
    pub divider: bool,
}

#[derive(Clone, Debug, Default)]
struct LabelSelection {
    ids: Vec<String>,
    subject: Option<String>,
    counts: Option<HashMap<String, usize>>,
    waiting_to_open: bool,
    pending_choice: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlagEdit {
    /// Thread ids, in list order.
    pub ids: Vec<String>,
    pub change: FlagChange,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadDwell {
    id: String,
    folder: String,
    generation: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub note: Option<NoteRequest>,
    pub note_scroll: i8,
    pub toggle_agent_note: bool,

    pub action: Option<Action>,
    pub reload_threads: bool,
    pub reload_messages: bool,
    pub arm_which: bool,
    /// Conversation pages, in viewport heights. Positive moves down.
    pub scroll_pages: i32,
    /// Conversation lines. Positive moves down.
    pub scroll_lines: i32,
    /// Go to the selected message's header even if its index is unchanged.
    pub jump_to_message: bool,
    /// The reader expanded or folded a message, its quotes, or recipients.
    pub message_disclosure_changed: bool,
    /// The view moves the thread to [`Session::pending_move`].
    pub apply_move: bool,
    pub read_label_targets: bool,
    pub labels: Option<LabelEdit>,
    /// Resolve a flag toggle against marked conversations outside the loaded page.
    pub off_page_flags: Option<(Vec<String>, bool)>,
    /// `u` restores the latest local action.
    pub undo: bool,
    /// `e` archives the open thread out of the folder on screen.
    pub archive: bool,
    /// `#` outside Trash: move [`Session::action_targets`] to Trash.
    pub delete: bool,
    /// `#` on selected local drafts: discard them as one undoable action.
    pub discard: Vec<String>,
    /// `!`: move [`Session::action_targets`] to Spam, or out of it.
    pub spam: bool,
    /// `y` answered the permanent-delete question: delete
    /// [`Session::take_purge`] from Trash for good.
    pub purge: bool,
    /// Start a sync pass now instead of waiting for the next one.
    pub sync_now: bool,
    pub notification_mode: Option<NotificationMode>,
    /// Bring back the agent review window that closed last.
    pub reopen_review: bool,
    pub open_diagnostic: Option<DiagnosticFile>,
    /// `c`: start a new draft and open it in the compose editor.
    pub compose: bool,
    /// Open the selected row's draft in the compose editor.
    pub open_draft: bool,
    /// `r`, `R`, `f`: answer [`Session::reply_target`] in a new draft.
    pub reply: Option<ReplyKind>,
    pub attachment: Option<AttachmentRequest>,
    pub confirmed_attachment: Option<AttachmentTarget>,
    pub retry_attachment: bool,
    pub reveal_attachment: bool,
    /// Opening a thread, `U`, or `s` changed these rows.
    pub flags: Option<FlagEdit>,
    /// Opened an unread thread with a read delay: mark it read once it has
    /// stayed open that long (see [`Session::read_after_dwell`]).
    pub read_after: Option<ReadDwell>,
    pub search_request: bool,
    pub search_completion: bool,
    pub reveal_search_hit: bool,
    pub agents: Option<agents::Request>,
}

#[derive(Clone, Debug)]
pub struct Banner {
    pub title: String,
    pub hint: String,
}

/// Which read produced `Session::error`. A later success clears only its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryFault {
    Folder,
    Message,
}

#[derive(Clone, Debug)]
pub struct Session {
    pub account_name: String,
    pub account_email: String,
    pub folder: String,
    pub folders: Vec<String>,
    pub label_scope: Option<String>,
    pub labels: Vec<String>,
    /// Which folder is Sent, Drafts, Archive and the rest on this account.
    pub roles: FolderRoles,
    /// The loaded part of the folder list: a run of rows in list order.
    pub threads: Vec<ThreadRow>,
    /// The folder has rows above `threads` that are not loaded.
    pub more_before: bool,
    /// The folder has rows below `threads` that are not loaded.
    pub more_after: bool,
    /// Unread threads in the whole folder, counted by the store.
    pub unread_count: usize,
    /// Rows to read first and to add at a time.
    pub load_rows: usize,
    /// Load more once fewer than this many rows are left past the cursor or
    /// the viewport.
    pub margin: usize,
    /// Rows a read keeps loaded beyond the cursor, the visual anchor, and
    /// the viewport. Rows further away are dropped.
    pub keep_rows: usize,
    fetch: Option<Fetch>,
    pending: Option<Pending>,
    pub cursor_id: Option<String>,
    pub anchor_id: Option<String>,
    pub picked: HashSet<String>,
    pub visual: bool,
    pub pane: Pane,
    pub full_width: bool,
    reading_return_pane: Pane,
    pub messages: Vec<ConvMessage>,
    pub agent_notes: AgentNotes,
    pub message_cursor: usize,
    attachment_focus: Option<AttachmentTarget>,
    confirm_attachment: Option<(AttachmentTarget, u64)>,
    /// Messages whose collapsed parts (quotes, signature, every recipient)
    /// are open, by Message-ID, so a reread that shifts rows keeps them.
    pub quotes_open: HashSet<String>,
    /// Messages whose full recipient list is open, by Message-ID. A click
    /// on the recipient line opens only this; `q` opens everything.
    pub recipients_open: HashSet<String>,
    pub overlay: Overlay,
    pub agents: agents::Agents,
    pub palette_from_agents: bool,
    pub chord: Vec<KeyId>,
    pub chord_gen: u64,
    pub which_visible: bool,
    pub count: String,
    pub keymap: Keymap,
    pub sync: SyncKind,
    pub worker_outages: HashSet<Worker>,
    /// Mailbox or keymap failure from startup. Folder and message reads do not clear it.
    pub startup_error: Option<String>,
    /// Failure from the latest folder or message read.
    pub error: Option<String>,
    pub error_fault: Option<QueryFault>,
    /// Details for the last attachment failure, until another attempt starts.
    pub attachment_error: Option<String>,
    pub recent: Vec<Action>,
    pub search: Option<search::Search>,
    pub search_generation: u64,
    folder_list: Option<std::sync::Arc<search::FolderList>>,
    recent_searches: Vec<String>,
    pub notification_mode: Option<NotificationMode>,
    pub palette_query: String,
    pub palette_index: usize,
    pub folder_query: String,
    pub folder_index: usize,
    pub folder_intent: FolderIntent,
    label_selection: LabelSelection,
    pub toast: Option<Toast>,
    pub startup_crash_notice: Option<Toast>,
    /// Counts notices shown; see [`Session::toast_generation`].
    toast_generation: u64,
    /// The manual sync acknowledgement, until the worker sends status.
    sync_notice: Option<u64>,
    /// Destination chosen by `m`, consumed when the view applies the move.
    pub pending_move: Option<String>,
    /// `#` in Trash asked to remove these threads from Fell, in the
    /// notice of this generation. The next key answers: `y` deletes them,
    /// anything else lets it go. A notice that replaced the question
    /// withdraws it, so `y` never answers a question no longer on screen.
    confirm_purge: Option<PurgePrompt>,
    /// The threads `y` confirmed, consumed when the view deletes them.
    purge_ready: Option<Vec<String>>,
    /// The folder on screen has not been read from the store yet.
    pub loading: bool,
    /// How long a thread stays open before it is marked read. 0 marks it on
    /// the key press.
    pub read_dwell_ms: u64,
    /// Bumped whenever the open thread, the pane, or the folder changes, so
    /// a read delay started by an earlier open cannot fire for this one.
    pub read_gen: u64,
    /// A dwell that ended while the open search hit was being refreshed.
    deferred_read: Option<ReadDwell>,
    /// The list rows on screen, set by the view before each key. `None`:
    /// the list is not showing, or its layout is not known yet.
    pub rows_in_view: Option<Range<usize>>,
}

impl Session {
    pub fn new(
        account_name: impl Into<String>,
        account_email: impl Into<String>,
        sync: SyncKind,
        keymap: Keymap,
    ) -> Self {
        Self {
            account_name: account_name.into(),
            account_email: account_email.into(),
            folder: FolderRole::Inbox.default_name().into(),
            folders: Vec::new(),
            roles: FolderRoles::default(),
            threads: Vec::new(),
            more_before: false,
            more_after: false,
            unread_count: 0,
            load_rows: 100,
            margin: 40,
            keep_rows: 200,
            fetch: None,
            pending: None,
            cursor_id: None,
            anchor_id: None,
            picked: HashSet::new(),
            visual: false,
            pane: Pane::List,
            full_width: false,
            reading_return_pane: Pane::List,
            messages: Vec::new(),
            agent_notes: AgentNotes::default(),
            message_cursor: 0,
            attachment_focus: None,
            confirm_attachment: None,
            quotes_open: HashSet::new(),
            recipients_open: HashSet::new(),
            overlay: Overlay::None,
            agents: agents::Agents::default(),
            palette_from_agents: false,
            chord: Vec::new(),
            chord_gen: 0,
            which_visible: false,
            count: String::new(),
            keymap,
            sync,
            startup_error: None,
            error: None,
            error_fault: None,
            attachment_error: None,
            recent: Vec::new(),
            search: None,
            search_generation: 0,
            folder_list: None,
            recent_searches: Vec::new(),
            notification_mode: None,
            palette_query: String::new(),
            palette_index: 0,
            label_scope: None,
            labels: Vec::new(),
            folder_query: String::new(),
            folder_index: 0,
            folder_intent: FolderIntent::Go,
            label_selection: LabelSelection::default(),
            toast: None,
            startup_crash_notice: None,
            worker_outages: HashSet::new(),
            toast_generation: 0,
            sync_notice: None,
            pending_move: None,
            confirm_purge: None,
            purge_ready: None,
            loading: false,
            read_dwell_ms: 0,
            read_gen: 0,
            deferred_read: None,
            rows_in_view: None,
        }
    }

    pub fn show_toast(&mut self, text: impl Into<String>, alert: bool) {
        self.set_toast(Toast {
            text: text.into(),
            alert,
            hint: None,
            label: None,
        });
    }

    pub fn show_label_toast(&mut self, notice: present::LabelNotice) {
        self.set_toast(Toast {
            text: notice.fit(f32::INFINITY, |_| 0.0),
            alert: false,
            hint: None,
            label: Some(notice),
        });
    }

    pub fn set_toast(&mut self, toast: Toast) {
        self.toast_generation = self.toast_generation.wrapping_add(1);
        self.toast = Some(toast);
    }

    pub fn status_notice(&self) -> Option<&Toast> {
        self.toast
            .as_ref()
            .or_else(|| self.search.as_ref()?.error.as_ref())
            .or(self.startup_crash_notice.as_ref())
    }

    pub fn clear_toast(&mut self) {
        self.toast = None;
    }

    pub fn dismiss_key_notice(&mut self, generation: u64, undo_pending: bool) {
        if !undo_pending
            && self
                .toast
                .as_ref()
                .is_some_and(|toast| toast.label.is_none())
        {
            self.clear_toast_if(generation);
        }
    }

    pub fn show_sync_requested(&mut self) {
        self.show_toast("Syncing…", false);
        self.sync_notice = Some(self.toast_generation);
    }

    pub fn apply_sync_status(&mut self, status: SyncKind) {
        if let Some(notice) = self.sync_notice.take() {
            self.clear_toast_if(notice);
        }
        self.sync = status;
    }

    pub fn worker_outage(&self) -> Option<Worker> {
        [Worker::Store, Worker::BackgroundStore, Worker::Sync]
            .into_iter()
            .find(|worker| self.worker_outages.contains(worker))
    }

    /// Which notice is on screen: every `show_toast` makes a new one, even
    /// with the same words.
    pub fn toast_generation(&self) -> u64 {
        self.toast_generation
    }

    /// Clear the notice only if it is still `generation`, so a timer for an
    /// earlier notice never takes down a newer one.
    pub fn clear_toast_if(&mut self, generation: u64) -> bool {
        if self.toast.is_some() && self.toast_generation == generation {
            self.toast = None;
            self.confirm_purge = None;
            self.confirm_attachment = None;
            return true;
        }
        false
    }

    pub fn folder_prompt(&self) -> &'static str {
        match self.folder_intent {
            FolderIntent::Move => "Move to",
            FolderIntent::Go => "Folder",
            FolderIntent::Label => "Label",
            FolderIntent::LabelList => "Go to label",
        }
    }

    pub fn label_target(&self) -> Option<String> {
        if self.overlay != Overlay::Folders || self.folder_intent != FolderIntent::Label {
            return None;
        }
        match self.label_selection.ids.as_slice() {
            [] => None,
            [_] => self.label_selection.subject.clone(),
            ids => Some(format!("{} conversations", ids.len())),
        }
    }

    pub fn label_target_ids(&self) -> &[String] {
        &self.label_selection.ids
    }

    pub fn label_target_subject(&self) -> Option<&str> {
        self.label_selection.subject.as_deref()
    }

    pub fn visible_overlay(&self) -> Overlay {
        if self.label_picker_active() && self.label_selection.waiting_to_open {
            Overlay::None
        } else {
            self.overlay.clone()
        }
    }

    pub fn label_picker_active(&self) -> bool {
        self.overlay == Overlay::Folders && self.folder_intent == FolderIntent::Label
    }

    pub fn open_label_picker(&mut self) {
        if self.label_picker_active() {
            self.label_selection.waiting_to_open = false;
        }
    }

    pub fn load_label_targets(
        &mut self,
        result: Result<HashMap<String, fell_core::store::ThreadLabels>, String>,
    ) -> Step {
        if !self.label_picker_active() {
            return Step::default();
        }
        let targets = match result {
            Ok(targets) => targets,
            Err(error) => {
                self.overlay = Overlay::None;
                self.folder_intent = FolderIntent::Go;
                self.show_toast(format!("Could not read labels · {error}"), true);
                return Step::default();
            }
        };
        let selection = &mut self.label_selection;
        selection.waiting_to_open = false;
        selection.ids.retain(|id| targets.contains_key(id));
        if selection.ids.is_empty() {
            self.overlay = Overlay::None;
            self.folder_intent = FolderIntent::Go;
            self.show_toast("A selected conversation is no longer available", true);
            return Step::default();
        }
        if let [id] = selection.ids.as_slice() {
            selection.subject = Some(targets[id].subject.clone());
        }
        let mut counts = HashMap::new();
        for id in &selection.ids {
            for label in &targets[id].labels {
                *counts.entry(label.clone()).or_insert(0) += 1;
            }
        }
        selection.counts = Some(counts);
        if let Some(name) = selection.pending_choice.take() {
            self.apply_picker_choice(name)
        } else {
            Step::default()
        }
    }

    pub fn record_startup_error(&mut self, error: String) {
        self.startup_error = Some(error);
    }

    pub fn fail_query(&mut self, fault: QueryFault, error: String) {
        self.error = Some(error);
        self.error_fault = Some(fault);
    }

    /// Drop a query error only when this read is the one that recorded it.
    pub fn clear_query_fault(&mut self, fault: QueryFault) {
        if self.error_fault == Some(fault) {
            self.error = None;
            self.error_fault = None;
        }
    }

    pub fn detail_text(&self) -> Option<String> {
        let sync = match &self.sync {
            SyncKind::Failed(failure) => Some(failure.detail.as_str()),
            _ => None,
        };
        let lines: Vec<&str> = [
            self.startup_error.as_deref(),
            self.error.as_deref(),
            sync,
            self.attachment_error.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        if lines.is_empty() {
            None
        } else {
            Some(lines.join("\n"))
        }
    }

    pub fn folder_title(&self) -> String {
        self.label_scope
            .clone()
            .unwrap_or_else(|| self.roles.label(&self.folder))
    }

    /// `folder`'s own name, shown dimmed beside its title in the header,
    /// when its label alone would read like another of
    /// the account's folders: Gmail's `[Gmail]/Sent Mail` beside a label
    /// called `Sent`. `None` for every folder in the usual case.
    pub fn folder_detail(&self, folder: &str) -> Option<String> {
        self.roles
            .label_detail(folder, &self.folders)
            .map(str::to_string)
    }

    /// `folder` as one line of text, told apart from a folder that reads
    /// the same: `Sent ([Gmail]/Sent Mail)`.
    pub fn folder_text(&self, folder: &str) -> String {
        self.roles.label_among(folder, &self.folders)
    }

    /// The folder on screen is the account's drafts folder.
    pub fn in_drafts(&self) -> bool {
        self.label_scope.is_none() && self.roles.is(FolderRole::Drafts, &self.folder)
    }

    /// The compose draft the selected row stands for, in the drafts list.
    pub fn cursor_thread(&self) -> Option<&ThreadRow> {
        let id = self.cursor_id.as_ref()?;
        self.threads.iter().find(|thread| &thread.id == id)
    }

    /// The message `r`, `R`, and `f` answer: the one under the cursor in
    /// the conversation, or the latest when the list has focus. None on a
    /// draft row or before the conversation is read.
    pub fn reply_target(&self) -> Option<&ConvMessage> {
        if self.cursor_draft().is_some() || self.cursor_thread().is_none() {
            return None;
        }
        match self.pane {
            Pane::Conversation => self.messages.get(self.message_cursor),
            Pane::List => self.messages.last(),
        }
    }

    pub fn cursor_draft(&self) -> Option<&str> {
        let id = self.cursor_id.as_ref()?;
        self.threads
            .iter()
            .find(|thread| &thread.id == id)?
            .draft
            .as_deref()
    }

    /// Unread threads in the folder, including rows that are not loaded.
    pub fn unread_threads(&self) -> usize {
        self.unread_count
    }

    pub fn cursor_index(&self) -> usize {
        self.selected_index().unwrap_or(0)
    }

    /// The selected row among the loaded ones, if any is selected.
    pub fn selected_index(&self) -> Option<usize> {
        self.threads
            .iter()
            .position(|thread| Some(&thread.id) == self.cursor_id.as_ref())
    }

    /// Where the cursor stands while nothing is selected because the
    /// selected row was archived as the last loaded one: the first loaded
    /// row after it, or one past the end. Also the rows still to move once
    /// that row loads.
    fn gap(&self) -> Option<(usize, usize)> {
        if self.cursor_id.is_some() {
            return None;
        }
        let Some(Pending::After { key, down }) = &self.pending else {
            return None;
        };
        let lead = self.draft_rows();
        let place = lead + self.threads[lead..].partition_point(|row| row_key(row) <= *key);
        Some((place, *down))
    }

    /// Compose drafts at the top of the Drafts list. They come before the
    /// folder's first row and are not part of its order, so paging skips
    /// them.
    fn draft_rows(&self) -> usize {
        self.threads
            .iter()
            .take_while(|row| row.draft.is_some())
            .count()
    }

    /// Take the cursor move waiting for rows. Dropping a `G` or `gg` that
    /// asked for a jump also drops the jump.
    fn take_pending(&mut self) -> Option<Pending> {
        let pending = self.pending.take();
        if matches!(pending, Some(Pending::Last | Pending::Row(_))) {
            self.want(Fetch {
                drop_jump: true,
                ..Fetch::default()
            });
        }
        pending
    }

    pub fn mode_label(&self) -> &'static str {
        if self.search.as_ref().is_some_and(|search| search.editing)
            && self.overlay == Overlay::None
        {
            "insert"
        } else if self.focused_attachment().is_some() {
            "attachments"
        } else if self.visual {
            "visual"
        } else {
            "normal"
        }
    }

    pub fn prefix_text(&self) -> String {
        keymap::prefix_text(&self.chord, &self.keymap.leader_name)
    }

    pub fn which_items(&self) -> Vec<WhichItem> {
        keymap::which_key(&self.keymap, &self.chord)
    }

    pub fn palette(&self) -> Vec<PaletteItem> {
        if self.palette_from_agents {
            return self.agents.palette(&self.palette_query);
        }
        keymap::recent_search_items(&self.palette_query, &self.recent_searches)
            .chain(keymap::palette_items(
                &self.keymap,
                &self.palette_query,
                &self.recent,
            ))
            .collect()
    }

    pub fn search_editing(&self) -> bool {
        self.overlay == Overlay::None && self.search.as_ref().is_some_and(|search| search.editing)
    }

    pub fn folder_choices(&self) -> Vec<String> {
        let all = merge_folders(&self.roles, &self.folders);
        let query = self.folder_query.trim();
        let from = self
            .cursor_id
            .as_deref()
            .and_then(|id| self.search_source(id))
            .unwrap_or(&self.folder);
        all.into_iter()
            .filter(|name| {
                if self.folder_intent == FolderIntent::Move
                    && (name.eq_ignore_ascii_case(from) || self.roles.is_view(name))
                {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                self.roles
                    .label(name)
                    .to_ascii_lowercase()
                    .contains(&query.to_ascii_lowercase())
                    || name
                        .to_ascii_lowercase()
                        .contains(&query.to_ascii_lowercase())
            })
            .collect()
    }

    pub fn picker_choices(&self) -> Vec<PickerChoice> {
        if matches!(
            self.folder_intent,
            FolderIntent::Label | FolderIntent::LabelList
        ) {
            return self.label_choices();
        }
        let mut last_view = None;
        self.folder_choices()
            .into_iter()
            .map(|name| {
                let is_view = matches!(
                    self.roles.role_of(&name),
                    Some(FolderRole::All | FolderRole::Flagged)
                );
                let divider = last_view
                    .replace(is_view)
                    .is_some_and(|last| last != is_view);
                PickerChoice {
                    title: self.roles.label(&name),
                    detail: self.folder_detail(&name),
                    mark: None,
                    name,
                    divider,
                }
            })
            .collect()
    }

    fn label_choices(&self) -> Vec<PickerChoice> {
        let query = self.folder_query.trim();
        let applying = self.folder_intent == FolderIntent::Label;
        let folded = keymap::fuzzy_query(query);
        let mut names: Vec<_> = self
            .labels
            .iter()
            .filter_map(|name| Some((keymap::fuzzy_score(&folded, name)?, name)))
            .collect();
        names.sort_by(|(a, name_a), (b, name_b)| b.cmp(a).then_with(|| name_a.cmp(name_b)));
        let mut choices: Vec<_> = names
            .into_iter()
            .map(|(_, name)| {
                let count = self
                    .label_selection
                    .counts
                    .as_ref()
                    .filter(|_| applying)
                    .and_then(|counts| counts.get(name))
                    .copied()
                    .unwrap_or(0);
                PickerChoice {
                    name: name.clone(),
                    title: name.clone(),
                    detail: None,
                    mark: match count {
                        0 => None,
                        n if n == self.label_selection.ids.len() => Some("✓"),
                        _ => Some("−"),
                    },
                    divider: false,
                }
            })
            .collect();
        if applying
            && !query.is_empty()
            && !self
                .labels
                .iter()
                .any(|name| name.eq_ignore_ascii_case(query))
        {
            choices.push(PickerChoice {
                name: query.to_string(),
                title: format!("Create label “{query}”"),
                detail: None,
                mark: Some("+"),
                divider: !choices.is_empty(),
            });
        }
        choices
    }

    pub fn set_labels(&mut self, labels: Vec<String>) {
        if self.labels == labels {
            return;
        }
        let picker_open = self.overlay == Overlay::Folders
            && matches!(
                self.folder_intent,
                FolderIntent::Label | FolderIntent::LabelList
            );
        let selected = picker_open
            .then(|| {
                self.label_choices()
                    .get(self.folder_index)
                    .map(|choice| choice.name.clone())
            })
            .flatten();
        self.labels = labels;
        if picker_open {
            self.folder_index = selected
                .and_then(|name| {
                    self.label_choices()
                        .iter()
                        .position(|choice| choice.name == name)
                })
                .unwrap_or(0);
        }
    }

    pub fn label_rows(&mut self, edit: &LabelEdit, leaving_scope: bool) {
        edit.include_name(&mut self.labels);
        if !leaving_scope {
            edit.apply(&mut self.threads);
        }
    }

    pub fn highlighted(&self) -> HashSet<String> {
        if let Some(prompt) = &self.confirm_purge
            && prompt.notice == self.toast_generation
        {
            return prompt.ids.iter().cloned().collect();
        }
        if let Some(rows) = self.visual_rows() {
            return rows.iter().map(|row| row.id.clone()).collect();
        }
        let mut ids = self.picked.clone();
        if let Some(id) = &self.cursor_id
            && self.threads.iter().any(|thread| &thread.id == id)
        {
            ids.insert(id.clone());
        }
        ids
    }

    fn visual_rows(&self) -> Option<&[ThreadRow]> {
        if !self.visual {
            return None;
        }
        let anchor = self
            .threads
            .iter()
            .position(|thread| Some(&thread.id) == self.anchor_id.as_ref())?;
        let cursor = self
            .threads
            .iter()
            .position(|thread| Some(&thread.id) == self.cursor_id.as_ref())?;
        Some(&self.threads[anchor.min(cursor)..=anchor.max(cursor)])
    }

    /// A visual range takes precedence over marks. Marks keep their IDs across
    /// pages. With neither, the cursor supplies the single-row target.
    pub fn action_targets(&self) -> Vec<String> {
        if self.visual {
            return self
                .visual_rows()
                .unwrap_or_default()
                .iter()
                .map(|row| row.id.clone())
                .collect();
        }
        if self.picked.is_empty() {
            return self
                .threads
                .iter()
                .find(|thread| Some(&thread.id) == self.cursor_id.as_ref())
                .map(|thread| thread.id.clone())
                .into_iter()
                .collect();
        }
        let mut targets: Vec<_> = self
            .threads
            .iter()
            .filter(|thread| self.picked.contains(&thread.id))
            .map(|thread| thread.id.clone())
            .collect();
        let loaded: HashSet<_> = targets.iter().collect();
        let mut off_page: Vec<_> = self
            .picked
            .iter()
            .filter(|id| !loaded.contains(id))
            .cloned()
            .collect();
        off_page.sort_unstable();
        targets.extend(off_page);
        targets
    }

    /// The selected conversation row IDs and local draft IDs, in order.
    /// Conversation actions skip local drafts. In Drafts, a draft-only
    /// delete discards those drafts through the editor's path.
    pub fn split_removal_targets(&self, mut ids: Vec<String>) -> (Vec<String>, Vec<String>) {
        let mut drafts = Vec::new();
        ids.retain(|id| {
            let loaded_draft = self
                .threads
                .iter()
                .find(|row| &row.id == id)
                .and_then(|row| row.draft.as_deref());
            if let Some(draft) = loaded_draft.or_else(|| present::draft_id_from_row(id)) {
                drafts.push(draft.to_owned());
                return false;
            }
            true
        });
        (ids, drafts)
    }

    /// The threads `y` confirmed for a permanent delete, once.
    pub fn take_purge(&mut self) -> Option<Vec<String>> {
        self.purge_ready.take()
    }

    fn withdraw_purge(&mut self) {
        if let Some(prompt) = self.confirm_purge.take() {
            self.clear_toast_if(prompt.notice);
        }
    }

    /// The folder on screen is the account's Trash.
    pub fn in_trash(&self) -> bool {
        self.roles.is(FolderRole::Trash, &self.folder)
    }

    /// Select `id` when that thread is in the current list.
    ///
    /// Undo uses this so the thread that just came back is the one on screen.
    pub fn select_if_present(&mut self, id: &str) -> bool {
        if self.threads.iter().any(|thread| thread.id == id) {
            self.cursor_id = Some(id.to_string());
            true
        } else {
            false
        }
    }

    /// Keep the same thread selected when the list is replaced.
    pub fn set_threads(&mut self, threads: Vec<ThreadRow>) -> bool {
        let previous = self.cursor_id.clone();
        let old_ids: HashSet<String> = self
            .threads
            .iter()
            .map(|thread| thread.id.clone())
            .collect();
        let previous_index = previous
            .as_ref()
            .and_then(|id| self.threads.iter().position(|thread| &thread.id == id));
        self.threads = threads;
        if self.confirm_purge.as_ref().is_some_and(|prompt| {
            prompt.folder != self.folder
                || ((!self.more_before && !self.more_after)
                    && !prompt
                        .ids
                        .iter()
                        .all(|id| self.threads.iter().any(|row| &row.id == id)))
        }) {
            self.withdraw_purge();
        }
        if let Some(id) = &previous {
            if self.threads.iter().any(|thread| &thread.id == id) {
                self.cursor_id = Some(id.clone());
            } else {
                // The thread left this folder or merged into another. Stay at
                // the same place in the list instead of jumping to the top.
                let index = previous_index
                    .unwrap_or(0)
                    .min(self.threads.len().saturating_sub(1));
                self.cursor_id = self.threads.get(index).map(|thread| thread.id.clone());
            }
        } else if !matches!(self.pending, Some(Pending::After { .. })) {
            // Unless the row after an archived one is still to load: it is
            // selected when it does (see [`Session::settle`]).
            self.cursor_id = self.threads.first().map(|thread| thread.id.clone());
        }
        if !self.more_before && !self.more_after {
            self.picked
                .retain(|id| self.threads.iter().any(|thread| &thread.id == id));
        }
        if self.visual && !self.visual_span_survived(&old_ids, previous.as_deref()) {
            self.visual = false;
            self.anchor_id = None;
        }
        self.cursor_id != previous
    }

    /// The visual span survives only when both ends are still present and every
    /// row now between them was already in the previous list.
    fn visual_span_survived(
        &self,
        old_ids: &HashSet<String>,
        previous_cursor: Option<&str>,
    ) -> bool {
        let Some(anchor) = self
            .threads
            .iter()
            .position(|thread| Some(thread.id.as_str()) == self.anchor_id.as_deref())
        else {
            return false;
        };
        if !previous_cursor.is_some_and(|id| {
            old_ids.contains(id) && self.threads.iter().any(|thread| thread.id == id)
        }) {
            return false;
        }
        let Some(cursor) = self
            .threads
            .iter()
            .position(|thread| Some(thread.id.as_str()) == previous_cursor)
        else {
            return false;
        };
        let (lo, hi) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        self.threads[lo..=hi]
            .iter()
            .all(|thread| old_ids.contains(&thread.id))
    }

    /// `q`: open or close every collapsed part of the message at `index`,
    /// quoted text and recipients together.
    pub fn toggle_open(&mut self, index: usize) {
        let Some(id) = self.messages.get(index).map(|message| message.id.clone()) else {
            return;
        };
        let open = !(self.quotes_open.contains(&id) || self.recipients_open.contains(&id));
        for set in [&mut self.quotes_open, &mut self.recipients_open] {
            if open {
                set.insert(id.clone());
            } else {
                set.remove(&id);
            }
        }
    }

    /// Take the rows a folder read returned, and whether the folder goes on
    /// past them. The same thread stays selected (see [`Session::set_threads`]).
    /// A cursor move that was waiting for these rows then finishes, or asks
    /// for more.
    pub fn set_page(
        &mut self,
        rows: Vec<ThreadRow>,
        more_before: bool,
        more_after: bool,
        unread: usize,
    ) {
        self.more_before = more_before;
        self.more_after = more_after;
        self.unread_count = unread;
        self.set_threads(rows);
        self.settle();
    }

    /// Where to follow the selected thread before taking `rows`: its key
    /// now (`found`, looked up by id) when the rows read leave it out though
    /// it is still in the folder. New mail moved it above rows that do not
    /// reach the top of the folder. Without this the row that took its
    /// place would be selected, and opened.
    /// A `G` or `gg` waiting for these rows moves the selection anyway, so
    /// there is nothing to follow.
    pub fn moved_selection(&self, rows: &[ThreadRow], found: Option<ListKey>) -> Option<ListKey> {
        if matches!(self.pending, Some(Pending::Last | Pending::Row(_))) {
            return None;
        }
        let key = found?;
        let id = self.cursor_id.as_deref()?;
        (key.thread_id.0 == id && !rows.iter().any(|row| row.id == id)).then_some(key)
    }

    /// Rows to read next, if the list asked for any.
    pub fn take_fetch(&mut self) -> Option<Fetch> {
        self.fetch.take()
    }

    pub fn has_fetch(&self) -> bool {
        self.fetch.is_some()
    }

    fn want(&mut self, fetch: Fetch) {
        self.fetch = Some(match self.fetch {
            Some(earlier) => earlier.merge(fetch),
            None => fetch,
        });
    }

    /// Forget rows and cursor moves that belong to the folder on screen.
    /// A folder switch calls this.
    pub fn forget_window(&mut self) {
        self.withdraw_purge();
        self.more_before = false;
        self.more_after = false;
        self.fetch = None;
        self.pending = None;
    }

    /// Ask for more rows when the cursor or the rows on screen are within
    /// `margin` of an end of the loaded rows and the folder goes on.
    pub fn need_rows(&mut self, visible: Option<Range<usize>>) {
        let Some((lo, hi)) = self.focus(visible) else {
            return;
        };
        let len = self.threads.len();
        let mut fetch = Fetch::default();
        if self.more_after && len - 1 - hi < self.margin {
            fetch.after = self.load_rows;
        }
        if self.more_before && lo < self.margin {
            fetch.before = self.load_rows;
        }
        if !fetch.is_empty() {
            self.want(fetch);
        }
    }

    /// The loaded rows the reader is at: the cursor (or the place of a row
    /// just removed), the visual anchor, and the rows on screen.
    fn focus(&self, visible: Option<Range<usize>>) -> Option<(usize, usize)> {
        let len = self.threads.len();
        if len == 0 {
            return None;
        }
        let cursor = match self.gap() {
            Some((place, _)) => place.min(len - 1),
            None => self.cursor_index(),
        };
        let (mut lo, mut hi) = (cursor, cursor);
        if let Some(range) = visible.filter(|range| !range.is_empty()) {
            lo = lo.min(range.start);
            hi = hi.max(range.end - 1);
        }
        if self.visual
            && let Some(anchor) = self
                .threads
                .iter()
                .position(|thread| Some(&thread.id) == self.anchor_id.as_ref())
        {
            lo = lo.min(anchor);
            hi = hi.max(anchor);
        }
        let hi = hi.min(len - 1);
        Some((lo.min(hi), hi))
    }

    /// What the next read of this folder asks the store for: the loaded rows
    /// again (so rows sync added or changed among them show up) plus `fetch`.
    /// Rows more than `keep_rows` past the reader are dropped, unless the
    /// fetch is growing that end. `select` is a thread the read must include,
    /// such as the one undo brings back.
    pub fn span(
        &self,
        fetch: Fetch,
        select: Option<&ListKey>,
        visible: Option<Range<usize>>,
    ) -> FolderSpan {
        match fetch.jump {
            Some(Jump::Top(rows)) => return FolderSpan::Top { rows },
            Some(Jump::Bottom(rows)) => return FolderSpan::Bottom { rows },
            None => {}
        }
        // Positions below are among the folder's rows, after any drafts.
        let lead = self.draft_rows();
        let rows = &self.threads[lead..];
        let (Some(top), Some(bottom), Some((lo, hi))) =
            (rows.first(), rows.last(), self.focus(visible))
        else {
            return FolderSpan::Top {
                rows: self.load_rows.max(fetch.after),
            };
        };
        let (lo, hi) = (lo.saturating_sub(lead), hi.saturating_sub(lead));
        if let Some(key) = select {
            let above = self.more_before && *key < row_key(top);
            let below = self.more_after && *key > row_key(bottom);
            if above || below {
                return FolderSpan::Window {
                    first: Some(key.clone()),
                    last: key.clone(),
                    before: self.margin,
                    after: self.load_rows,
                };
            }
        }
        let len = rows.len();
        let mut start = if fetch.before > 0 {
            0
        } else {
            lo.saturating_sub(self.keep_rows)
        };
        let mut end = if fetch.after > 0 {
            len - 1
        } else {
            hi.saturating_add(self.keep_rows).min(len - 1)
        };
        // A thread to select within the loaded bounds is read even when it
        // moved among rows that would be dropped.
        if let Some(key) = select {
            let at = rows.partition_point(|row| row_key(row) < *key).min(len - 1);
            start = start.min(at);
            end = end.max(at);
        }
        // From the top when the top is loaded, so rows sync adds above the
        // first one are read too.
        let first = (start > 0 || self.more_before).then(|| row_key(&rows[start]));
        let after = if end + 1 < len {
            0
        } else if self.more_after {
            fetch.after
        } else {
            // The whole end is loaded: also read rows sync adds below it.
            fetch.after.max(self.margin)
        };
        FolderSpan::Window {
            first,
            last: row_key(&rows[end]),
            before: if start == 0 { fetch.before } else { 0 },
            after,
        }
    }

    /// Finish a cursor move that was waiting for rows, now that a read
    /// landed. Asks for more when they are still not loaded.
    fn settle(&mut self) {
        let Some(pending) = self.pending.take() else {
            return;
        };
        match pending {
            Pending::Down(steps) => {
                self.move_list(Motion::Forward(steps));
            }
            Pending::Up(steps) => {
                self.move_list(Motion::Backward(steps));
            }
            Pending::Last => {
                self.list_last();
            }
            Pending::Row(index) => {
                self.list_row(index);
            }
            // Something got selected meanwhile (a click, undo): keep it.
            Pending::After { .. } if self.cursor_id.is_some() => {}
            Pending::After { key, down } => {
                let lead = self.draft_rows();
                let next = self.threads[lead..].iter().find(|row| row_key(row) > key);
                match next {
                    Some(row) => {
                        self.cursor_id = Some(row.id.clone());
                        if down > 0 {
                            self.move_list(Motion::Forward(down));
                        }
                    }
                    None if self.more_after => {
                        self.pending = Some(Pending::After { key, down });
                        self.want(Fetch {
                            after: down.saturating_add(self.load_rows),
                            ..Fetch::default()
                        });
                    }
                    None => self.cursor_id = self.threads.last().map(|row| row.id.clone()),
                }
            }
        }
    }

    /// A click on a quote or signature block.
    pub fn toggle_quotes(&mut self, index: usize) {
        if let Some(id) = self.messages.get(index).map(|message| message.id.clone())
            && !self.quotes_open.remove(&id)
        {
            self.quotes_open.insert(id);
        }
    }

    /// A click on the recipient line.
    pub fn toggle_recipients(&mut self, index: usize) {
        if let Some(id) = self.messages.get(index).map(|message| message.id.clone())
            && !self.recipients_open.remove(&id)
        {
            self.recipients_open.insert(id);
        }
    }

    pub fn is_open(&self, message_id: &str) -> bool {
        self.quotes_open.contains(message_id)
    }

    pub fn recipients_are_open(&self, message_id: &str) -> bool {
        self.recipients_open.contains(message_id)
    }

    fn select_index(&mut self, index: usize) -> bool {
        let Some(row) = self.threads.get(index) else {
            return false;
        };
        let changed = self.cursor_id.as_deref() != Some(row.id.as_str());
        self.cursor_id = Some(row.id.clone());
        changed
    }

    /// Move the list cursor. A move past the loaded rows goes as far as it
    /// can now and finishes when the rest loads.
    fn move_list(&mut self, motion: Motion) -> bool {
        let len = self.threads.len();
        if len == 0 {
            return false;
        }
        if let Some((place, down)) = self.gap() {
            return self.move_from_gap(place, down, motion);
        }
        // Keys pressed while the rows were loading add up.
        let owed = match (&self.pending, motion) {
            (Some(Pending::Down(steps)), Motion::Forward(_))
            | (Some(Pending::Up(steps)), Motion::Backward(_)) => *steps,
            _ => 0,
        };
        self.take_pending();
        let cursor = self.cursor_index();
        let (next, short) = match motion {
            Motion::Forward(steps) => {
                let target = cursor.saturating_add(steps).saturating_add(owed);
                (target.min(len - 1), target.saturating_sub(len - 1))
            }
            Motion::Backward(steps) => {
                let steps = steps.saturating_add(owed);
                (cursor.saturating_sub(steps), steps.saturating_sub(cursor))
            }
        };
        let changed = self.select_index(next);
        if short > 0 {
            match motion {
                Motion::Forward(_) if self.more_after => {
                    self.pending = Some(Pending::Down(short));
                    self.want(Fetch {
                        after: short.saturating_add(self.margin),
                        ..Fetch::default()
                    });
                }
                Motion::Backward(_) if self.more_before => {
                    self.pending = Some(Pending::Up(short));
                    self.want(Fetch {
                        before: short.saturating_add(self.margin),
                        ..Fetch::default()
                    });
                }
                _ => {}
            }
        }
        self.need_rows(None);
        changed
    }

    /// A motion while nothing is selected after an archive. The cursor
    /// stands at `place`, `down` rows short of where earlier motions took
    /// it. It selects a row only once the target is loaded, so the next key
    /// cannot act on the row above the archived one, or on the top row.
    fn move_from_gap(&mut self, place: usize, down: usize, motion: Motion) -> bool {
        let len = self.threads.len();
        let at = place.saturating_add(down);
        let target = match motion {
            Motion::Forward(steps) => at.saturating_add(steps),
            Motion::Backward(steps) => at.saturating_sub(steps),
        };
        if target < len {
            self.pending = None;
            let changed = self.select_index(target);
            self.need_rows(None);
            return changed;
        }
        if !self.more_after {
            self.pending = None;
            return self.select_index(len - 1);
        }
        if let Some(Pending::After { down, .. }) = &mut self.pending {
            *down = target - place;
        }
        self.want(Fetch {
            after: (target - len + 1).saturating_add(self.margin),
            ..Fetch::default()
        });
        false
    }

    /// `G`: the last row of the folder.
    fn list_last(&mut self) -> bool {
        self.take_pending();
        if !self.more_after {
            return self.select_index(self.threads.len().saturating_sub(1));
        }
        self.pending = Some(Pending::Last);
        self.want(if self.visual {
            // Keep the anchor: load every row down to the end.
            Fetch {
                after: usize::MAX,
                ..Fetch::default()
            }
        } else {
            Fetch {
                jump: Some(Jump::Bottom(self.load_rows)),
                ..Fetch::default()
            }
        });
        false
    }

    /// `gg` or a count: row `index` from the top of the folder.
    fn list_row(&mut self, index: usize) -> bool {
        self.take_pending();
        let len = self.threads.len();
        if !self.more_before && (index < len || !self.more_after) {
            let changed = self.select_index(index.min(len.saturating_sub(1)));
            self.need_rows(None);
            return changed;
        }
        self.pending = Some(Pending::Row(index));
        self.want(if !self.more_before {
            Fetch {
                after: (index + 1 - len).saturating_add(self.margin),
                ..Fetch::default()
            }
        } else if self.visual {
            Fetch {
                before: usize::MAX,
                ..Fetch::default()
            }
        } else {
            Fetch {
                jump: Some(Jump::Top(
                    index.saturating_add(1 + self.margin).max(self.load_rows),
                )),
                ..Fetch::default()
            }
        });
        false
    }

    pub fn set_messages(&mut self, messages: Vec<ConvMessage>) {
        self.messages = messages;
        self.leave_attachments();
        self.quotes_open.clear();
        self.recipients_open.clear();
        self.message_cursor = self.messages.len().saturating_sub(1);
    }

    pub fn focused_attachment(&self) -> Option<&AttachmentTarget> {
        let target = self.attachment_focus.as_ref()?;
        let message = self.messages.get(self.message_cursor)?;
        (self.pane == Pane::Conversation
            && message.expanded
            && message.id == target.message
            && message.attachments.contains(&target.part))
        .then_some(target)
    }

    pub fn focus_attachment(&mut self, index: usize, part: fell_core::MimePart) {
        self.leave_attachments();
        self.pane = Pane::Conversation;
        self.message_cursor = index;
        self.attachment_focus = Some(AttachmentTarget {
            message: self.messages[index].id.clone(),
            part,
        });
    }

    pub fn leave_attachments(&mut self) {
        self.attachment_focus = None;
        if let Some((_, notice)) = self.confirm_attachment.take() {
            self.clear_toast_if(notice);
        }
    }

    pub fn ask_attachment_confirmation(&mut self, target: AttachmentTarget) {
        if self.focused_attachment() != Some(&target) {
            self.show_toast("Focus the executable attachment to confirm opening", true);
            return;
        }
        self.show_toast(
            format!(
                "Open executable{NOTICE_DOT}“{}”{NOTICE_DOT}y confirm",
                target.name()
            ),
            true,
        );
        self.confirm_attachment = Some((target, self.toast_generation));
    }

    /// Reread of the same messages in the same order. Folder and header
    /// changes land along with bodies. The reader's open state stays put.
    pub fn refresh_same_messages(&mut self, mut messages: Vec<ConvMessage>) {
        for (new, old) in messages.iter_mut().zip(&self.messages) {
            new.expanded = old.expanded;
        }
        self.messages = messages;
        if self.focused_attachment().is_none() {
            self.leave_attachments();
        }
    }

    /// Reread of the thread already open. A reader at the end follows when
    /// a new message arrives there. Returns whether the view should land
    /// on that new message.
    ///
    /// `viewport_at_end` is whether the conversation is scrolled to its
    /// bottom: a reader who scrolled up with the wheel is not following.
    /// Messages already on screen keep their open or closed state.
    pub fn refresh_messages(
        &mut self,
        mut messages: Vec<ConvMessage>,
        viewport_at_end: bool,
    ) -> bool {
        let at_end = viewport_at_end && self.message_cursor + 1 >= self.messages.len();
        for new in &mut messages {
            if let Some(old) = self.messages.iter().find(|old| old.id == new.id) {
                new.expanded = old.expanded;
            }
        }
        if at_end
            && messages
                .last()
                .is_some_and(|last| self.messages.iter().all(|old| old.id != last.id))
        {
            self.set_messages(messages);
            return true;
        }
        let cursor = self
            .messages
            .get(self.message_cursor)
            .and_then(|current| messages.iter().position(|message| message.id == current.id))
            .unwrap_or(self.message_cursor);
        let len = messages.len();
        for set in [&mut self.quotes_open, &mut self.recipients_open] {
            set.retain(|id| messages.iter().any(|message| &message.id == id));
        }
        self.messages = messages;
        self.message_cursor = cursor.min(len.saturating_sub(1));
        if self.focused_attachment().is_none() {
            self.leave_attachments();
        }
        false
    }

    /// Take `id` out of the list before the store write that removes it
    /// lands, and select the row that slides into its place (the next row,
    /// or the previous one at the end). The next key acts on that row.
    pub fn remove_thread(&mut self, id: &str) {
        let Some(index) = self.threads.iter().position(|thread| thread.id == id) else {
            return;
        };
        // A visual range loses an end: end the range instead of letting the
        // next row join it.
        if self.visual
            && (self.anchor_id.as_deref() == Some(id) || self.cursor_id.as_deref() == Some(id))
        {
            self.visual = false;
            self.anchor_id = None;
        }
        let removed = self.threads.remove(index);
        if removed.unread {
            self.unread_count = self.unread_count.saturating_sub(1);
        }
        self.picked.remove(id);
        if self.cursor_id.as_deref() == Some(id) {
            if index == self.threads.len() && self.more_after {
                // The row that takes its place is not loaded yet. Select
                // nothing until it is, so the next key cannot act on the row
                // above by mistake.
                self.cursor_id = None;
                self.pending = Some(Pending::After {
                    key: row_key(&removed),
                    down: 0,
                });
                self.want(Fetch {
                    after: self.load_rows,
                    ..Fetch::default()
                });
            } else {
                let next = index.min(self.threads.len().saturating_sub(1));
                self.cursor_id = self.threads.get(next).map(|thread| thread.id.clone());
            }
        }
        if self.anchor_id.as_deref() == Some(id) {
            self.visual = false;
            self.anchor_id = None;
        }
    }

    pub fn on_key(&mut self, key: KeyId, page_rows: usize) -> Step {
        if key == KeyId::Escape {
            self.startup_crash_notice = None;
        }
        self.apply_input(|session| session.handle_key(key, page_rows))
    }

    fn apply_input(&mut self, apply: impl FnOnce(&mut Self) -> Step) -> Step {
        let before = (self.pane, self.cursor_id.clone(), self.folder.clone());
        let mut step = apply(self);
        if before != (self.pane, self.cursor_id.clone(), self.folder.clone()) {
            self.cancel_read_delay();
        }
        if self.attachment_focus.is_some() && self.focused_attachment().is_none() {
            self.leave_attachments();
        }
        if let Some(read) = &mut step.read_after {
            read.generation = self.read_gen;
        }
        step
    }

    /// Invalidate the read delay after navigation or an explicit read edit.
    pub fn cancel_read_delay(&mut self) {
        self.read_gen = self.read_gen.wrapping_add(1);
        self.deferred_read = None;
    }

    fn handle_key(&mut self, key: KeyId, page_rows: usize) -> Step {
        if let Some((target, notice)) = self.confirm_attachment.take()
            && notice == self.toast_generation
            && self.focused_attachment() == Some(&target)
        {
            self.clear_toast();
            if key == KeyId::Char('y') {
                self.count.clear();
                return Step {
                    confirmed_attachment: Some(target),
                    ..Step::default()
                };
            }
            if matches!(key, KeyId::Escape | KeyId::Char('n')) {
                self.count.clear();
                return Step::default();
            }
        }
        if self.overlay != Overlay::None {
            return self.overlay_key(key, page_rows);
        }
        if self.search_editing() && key != KeyId::CtrlShiftP {
            return self.search_key(key);
        }
        if key == KeyId::Tab && self.search.is_some() {
            return self.accept_search_suggestion();
        }
        // The question is asked once: any key answers it. `y` deletes;
        // Escape and `n` only let it go; any other key does what it does.
        if let Some(prompt) = self.confirm_purge.take()
            && prompt.notice == self.toast_generation
            && prompt.folder == self.folder
            && self.search_results_current()
        {
            match key {
                KeyId::Char('y') => {
                    self.count.clear();
                    self.purge_ready = Some(prompt.ids);
                    return Step {
                        purge: true,
                        ..Step::default()
                    };
                }
                KeyId::Escape | KeyId::Char('n') => {
                    self.count.clear();
                    return Step::default();
                }
                _ => {}
            }
        }
        let mut step = Step::default();
        if self.chord.is_empty() && self.pane == Pane::Conversation {
            if self.focused_attachment().is_some() && key != self.keymap.leader {
                if key == KeyId::Escape {
                    self.leave_attachments();
                    self.count.clear();
                    return step;
                }
                if let Some(action) = self.keymap.attachment_action(key) {
                    return Step {
                        action: Some(action),
                        ..self.run(action, page_rows)
                    };
                }
            }
            if self.focused_attachment().is_none()
                && let Some(note) = self.accept_note_key(key)
            {
                return Step {
                    note: Some(note),
                    ..Step::default()
                };
            }
            match key {
                KeyId::Up => {
                    step.scroll_lines = -signed_count(self.take_count());
                    return step;
                }
                KeyId::Down => {
                    step.scroll_lines = signed_count(self.take_count());
                    return step;
                }
                _ => {}
            }
        }
        if key == KeyId::Escape && !self.chord.is_empty() {
            self.clear_chord();
            step.arm_which = true;
            return step;
        }
        if key == KeyId::Backspace && !self.chord.is_empty() {
            self.chord.pop();
            self.note_chord();
            step.arm_which = true;
            return step;
        }
        if key == KeyId::Escape && self.end_search() {
            self.count.clear();
            return step;
        }
        if self.chord.is_empty()
            && let KeyId::Char(ch) = key
            && ch.is_ascii_digit()
            && (ch != '0' || !self.count.is_empty())
        {
            // Longer than usize::MAX on this machine. Further digits stay saturated.
            if self.count.len() < 20 {
                self.count.push(ch);
            }
            return step;
        }
        let key = if key == self.keymap.leader {
            KeyId::Leader
        } else {
            key
        };
        let mut seq = self.chord.clone();
        seq.push(key);
        match keymap::match_seq(&self.keymap, &seq) {
            SeqMatch::Action(action) => {
                self.clear_chord();
                // Everything the action asked for, so a new Step field cannot
                // be dropped here (sync_now was, so `space s` did nothing).
                step = Step {
                    action: Some(action),
                    arm_which: true,
                    ..self.run(action, page_rows)
                };
            }
            SeqMatch::Prefix => {
                self.chord = seq;
                self.note_chord();
                step.arm_which = true;
            }
            SeqMatch::None => {
                if !self.chord.is_empty() {
                    self.clear_chord();
                    step.arm_which = true;
                }
                self.count.clear();
            }
        }
        step
    }

    fn overlay_key(&mut self, key: KeyId, page_rows: usize) -> Step {
        match self.overlay {
            Overlay::Palette => self.palette_key(key, page_rows),
            Overlay::Folders => self.folder_key(key),
            Overlay::Agents => self.agents_key(key, page_rows),
            Overlay::Help => {
                if matches!(key, KeyId::Escape | KeyId::Enter | KeyId::Char('?')) {
                    self.overlay = Overlay::None;
                }
                Step::default()
            }
            Overlay::About => {
                if matches!(key, KeyId::Escape | KeyId::Enter) {
                    self.overlay = Overlay::None;
                }
                Step::default()
            }
            Overlay::None => Step::default(),
        }
    }

    fn agents_key(&mut self, key: KeyId, page_rows: usize) -> Step {
        match key {
            KeyId::Escape if self.agents.confirm.is_none() || self.agents.pending => {
                self.dismiss_overlay();
                Step::default()
            }
            KeyId::Char(':') | KeyId::CtrlShiftP if self.agents.confirm.is_none() => {
                self.run(Action::Palette, page_rows)
            }
            _ => Step {
                agents: self.agents.key(key),
                ..Step::default()
            },
        }
    }

    pub fn dismiss_overlay(&mut self) {
        self.overlay = if std::mem::take(&mut self.palette_from_agents) {
            Overlay::Agents
        } else {
            self.agents.confirm_revoke(false);
            Overlay::None
        };
    }

    fn palette_key(&mut self, key: KeyId, page_rows: usize) -> Step {
        let len = self.palette().len();
        match key {
            KeyId::Escape => {
                self.dismiss_overlay();
                Step::default()
            }
            KeyId::Enter => {
                let Some(item) = self.palette().get(self.palette_index).cloned() else {
                    self.dismiss_overlay();
                    return Step::default();
                };
                self.confirm_palette(item, page_rows)
            }
            KeyId::Backspace => {
                self.palette_query.pop();
                self.palette_index = 0;
                Step::default()
            }
            KeyId::Up | KeyId::Ctrl('p') | KeyId::Ctrl('k') => {
                if len > 0 {
                    self.palette_index = if self.palette_index == 0 {
                        len - 1
                    } else {
                        self.palette_index - 1
                    };
                }
                Step::default()
            }
            KeyId::Down | KeyId::Ctrl('n') | KeyId::Ctrl('j') => {
                if len > 0 {
                    self.palette_index = (self.palette_index + 1) % len;
                }
                Step::default()
            }
            KeyId::Ctrl('u') => {
                self.palette_query.clear();
                self.palette_index = 0;
                Step::default()
            }
            KeyId::Char(ch) => {
                self.palette_query.push(ch);
                self.palette_index = 0;
                Step::default()
            }
            KeyId::Space => {
                self.palette_query.push(' ');
                self.palette_index = 0;
                Step::default()
            }
            _ => Step::default(),
        }
    }

    pub fn confirm_palette(&mut self, item: PaletteItem, page_rows: usize) -> Step {
        self.dismiss_overlay();
        let step = match item.search {
            Some(query) => self.apply_input(|session| {
                let step = session.start_search(Some(query));
                session.search.as_mut().unwrap().editing = false;
                session.remember_search();
                step
            }),
            None => self.perform(item.action, page_rows),
        };
        Step {
            action: Some(item.action),
            ..step
        }
    }

    fn folder_key(&mut self, key: KeyId) -> Step {
        match key {
            KeyId::Escape => {
                self.overlay = Overlay::None;
                self.folder_intent = FolderIntent::Go;
                Step::default()
            }
            KeyId::Enter => self.confirm_folder(),
            KeyId::Backspace => {
                self.folder_query.pop();
                self.folder_index = 0;
                Step::default()
            }
            KeyId::Up | KeyId::Ctrl('p') | KeyId::Ctrl('k') => {
                let len = self.picker_choices().len();
                if len > 0 {
                    self.folder_index = if self.folder_index == 0 {
                        len - 1
                    } else {
                        self.folder_index - 1
                    };
                }
                Step::default()
            }
            KeyId::Down | KeyId::Ctrl('n') | KeyId::Ctrl('j') => {
                let len = self.picker_choices().len();
                if len > 0 {
                    self.folder_index = (self.folder_index + 1) % len;
                }
                Step::default()
            }
            KeyId::Char(ch) => {
                self.folder_query.push(ch);
                self.folder_index = 0;
                Step::default()
            }
            KeyId::Space => {
                self.folder_query.push(' ');
                self.folder_index = 0;
                Step::default()
            }
            _ => Step::default(),
        }
    }

    /// Enter or a click on the highlighted folder.
    pub fn confirm_folder(&mut self) -> Step {
        let Some(chosen) = self.picker_choices().into_iter().nth(self.folder_index) else {
            if matches!(
                self.folder_intent,
                FolderIntent::Label | FolderIntent::LabelList
            ) {
                return Step::default();
            }
            self.overlay = Overlay::None;
            self.folder_query.clear();
            self.folder_intent = FolderIntent::Go;
            return Step::default();
        };
        if self.label_picker_active() && self.label_selection.counts.is_none() {
            self.label_selection.pending_choice = Some(chosen.name);
            return Step::default();
        }
        self.apply_picker_choice(chosen.name)
    }

    fn apply_picker_choice(&mut self, name: String) -> Step {
        let intent = self.folder_intent;
        self.overlay = Overlay::None;
        self.folder_query.clear();
        self.folder_intent = FolderIntent::Go;
        match intent {
            FolderIntent::Move => {
                self.pending_move = Some(name);
                Step {
                    apply_move: true,
                    ..Step::default()
                }
            }
            FolderIntent::Label => {
                let labeled = self
                    .label_selection
                    .counts
                    .as_ref()
                    .and_then(|counts| counts.get(&name))
                    .copied();
                let edit = LabelEdit {
                    ids: self.label_selection.ids.clone(),
                    name,
                    add: labeled != Some(self.label_selection.ids.len()),
                };
                if edit.add || self.label_scope.as_deref() != Some(&edit.name) {
                    self.visual = false;
                    self.anchor_id = None;
                }
                // When rows leave their own label list, keep its visual
                // range until the new page lands so it fades with the rows.
                Step {
                    labels: Some(edit),
                    ..Step::default()
                }
            }
            FolderIntent::LabelList => Step {
                reload_threads: self.go_label(&name),
                ..Step::default()
            },
            FolderIntent::Go => Step {
                reload_threads: self.go_folder(&name),
                ..Step::default()
            },
        }
    }

    pub fn perform(&mut self, action: Action, page_rows: usize) -> Step {
        self.apply_input(|session| session.run(action, page_rows))
    }

    fn run(&mut self, action: Action, page_rows: usize) -> Step {
        if self.overlay == Overlay::Agents {
            let agents = match action {
                Action::AgentControl(control) => {
                    self.agents.activate(self.agents.selected, control)
                }
                Action::UndoAgentRevoke => self.agents.undo(),
                _ => None,
            };
            if matches!(action, Action::AgentControl(_) | Action::UndoAgentRevoke) {
                return Step {
                    agents,
                    ..Step::default()
                };
            }
        }
        if !matches!(action, Action::Palette | Action::Help) {
            self.recent.retain(|item| *item != action);
            self.recent.insert(0, action);
            self.recent.truncate(8);
        }
        let mut step = Step::default();
        if !self.search_results_current()
            && !matches!(
                action,
                Action::Search
                    | Action::Back
                    | Action::Inbox
                    | Action::Sent
                    | Action::Drafts
                    | Action::FolderPicker
                    | Action::LabelList
                    | Action::Palette
                    | Action::Help
                    | Action::About
                    | Action::Compose
                    | Action::SyncNow
                    | Action::Notifications(_)
                    | Action::Undo
                    | Action::ReopenReview
                    | Action::Agents
                    | Action::OpenDiagnosticLog
                    | Action::OpenCrashReport
                    | Action::ThreadsWithSuggestions
            )
        {
            self.count.clear();
            return step;
        }
        if self.pane == Pane::Conversation
            && self
                .search
                .as_ref()
                .is_some_and(|search| !search.matches_thread(self.cursor_id.as_deref()))
            && matches!(
                action,
                Action::Open
                    | Action::Next
                    | Action::Prev
                    | Action::First
                    | Action::Last
                    | Action::FocusAttachments
                    | Action::OpenAttachment
                    | Action::SaveAttachment
                    | Action::SaveAllAttachments
                    | Action::RetryAttachment
                    | Action::ToggleQuotes
                    | Action::Reply
                    | Action::ReplyAll
                    | Action::Forward
            )
        {
            self.count.clear();
            return step;
        }
        if self.label_scope.is_some()
            && matches!(
                action,
                Action::Move | Action::Archive | Action::Delete | Action::Spam
            )
        {
            self.count.clear();
            self.show_toast("Choose a folder to move mail", false);
            return step;
        }
        match action {
            Action::ThreadsWithSuggestions => {
                let step = self.start_search(Some(SUGGESTIONS_QUERY.into()));
                self.search.as_mut().expect("search started").editing = false;
                return step;
            }
            Action::ToggleAgentNote
            | Action::DismissAgentNote
            | Action::NextAgentNote
            | Action::ScrollAgentNoteDown
            | Action::ScrollAgentNoteUp
            | Action::AcceptAgentSuggestion => {
                self.count.clear();
                return self.note_action(action);
            }
            Action::Search => return self.start_search(None),
            Action::NextHit | Action::PreviousHit => {
                let count = self.take_count();
                return self.move_search_hit(action == Action::NextHit, count);
            }
            Action::Next => {
                let count = self.take_count();
                step.reload_messages = self.move_cursor(Motion::Forward(count));
            }
            Action::Prev => {
                let count = self.take_count();
                step.reload_messages = self.move_cursor(Motion::Backward(count));
            }
            Action::First | Action::Last => {
                step.reload_messages = if action == Action::First {
                    self.jump_first()
                } else {
                    self.jump_last()
                };
                step.jump_to_message = self.pane == Pane::Conversation && !self.messages.is_empty();
            }
            Action::Open => {
                self.count.clear();
                if self.cursor_draft().is_some() {
                    // A draft row opens in the editor, not the reading pane.
                    step.open_draft = true;
                } else if self.pane == Pane::List {
                    if self.cursor_id.is_some() {
                        self.pane = Pane::Conversation;
                        self.opened(&mut step);
                    }
                } else if let Some(message) = self.messages.get_mut(self.message_cursor) {
                    message.expanded = !message.expanded;
                    step.message_disclosure_changed = true;
                }
            }
            Action::Back => {
                self.count.clear();
                if self.focused_attachment().is_some() {
                    self.leave_attachments();
                    return step;
                }
                self.visual = false;
                self.picked.clear();
                self.pane = Pane::List;
            }
            Action::PageDown => {
                if self.pane == Pane::Conversation {
                    step.scroll_pages = signed_count(self.take_count());
                } else {
                    let steps = self.page_steps(page_rows);
                    step.reload_messages = self.move_cursor(Motion::Forward(steps));
                }
            }
            Action::PageUp => {
                if self.pane == Pane::Conversation {
                    step.scroll_pages = -signed_count(self.take_count());
                } else {
                    let steps = self.page_steps(page_rows);
                    step.reload_messages = self.move_cursor(Motion::Backward(steps));
                }
            }
            Action::Inbox => step.reload_threads = self.go_role(FolderRole::Inbox),
            Action::Sent => step.reload_threads = self.go_role(FolderRole::Sent),
            Action::Drafts => step.reload_threads = self.go_role(FolderRole::Drafts),
            Action::FolderPicker => {
                self.count.clear();
                self.folder_intent = FolderIntent::Go;
                self.overlay = Overlay::Folders;
                self.folder_query.clear();
                self.folder_index = self
                    .folder_choices()
                    .iter()
                    .position(|name| name.eq_ignore_ascii_case(&self.folder))
                    .unwrap_or(0);
            }
            Action::Label | Action::LabelList => {
                self.count.clear();
                if action == Action::Label {
                    let targets = self.action_targets();
                    if targets.is_empty() {
                        self.show_toast("No conversation", true);
                        return step;
                    }
                    let (targets, _) = self.split_removal_targets(targets);
                    if targets.is_empty() {
                        self.show_toast(DRAFT_NOT_THREAD, true);
                        return step;
                    }
                    let subject = (targets.len() == 1)
                        .then(|| self.threads.iter().find(|row| row.id == targets[0]))
                        .flatten()
                        .map(|row| row.subject.clone());
                    self.label_selection = LabelSelection {
                        ids: targets,
                        subject,
                        waiting_to_open: true,
                        ..LabelSelection::default()
                    };
                    step.read_label_targets = true;
                }
                self.folder_intent = if action == Action::Label {
                    FolderIntent::Label
                } else {
                    FolderIntent::LabelList
                };
                self.overlay = Overlay::Folders;
                self.folder_query.clear();
                self.folder_index = 0;
            }
            Action::Move => {
                self.count.clear();
                if self.cursor_id.is_none() {
                    self.show_toast("No conversation", true);
                    return Step::default();
                }
                if self.cursor_draft().is_some() {
                    // Said now, not after a folder is picked for nothing.
                    self.show_toast(DRAFT_NOT_THREAD, true);
                    return Step::default();
                }
                if self
                    .cursor_id
                    .as_deref()
                    .and_then(|id| self.search_source(id))
                    .is_none()
                {
                    self.show_toast("No folder for this match", true);
                    return step;
                }
                self.folder_intent = FolderIntent::Move;
                self.overlay = Overlay::Folders;
                self.folder_query.clear();
                self.folder_index = 0;
            }
            Action::Archive => {
                self.count.clear();
                if self.action_targets().is_empty() {
                    self.show_toast("No conversation", true);
                    return Step::default();
                }
                return Step {
                    archive: true,
                    ..Step::default()
                };
            }
            Action::ToggleUnread => {
                self.count.clear();
                return self.toggle_flag(false);
            }
            Action::ToggleStar => {
                self.count.clear();
                return self.toggle_flag(true);
            }
            Action::Delete | Action::Spam => {
                self.count.clear();
                let (targets, drafts) = self.split_removal_targets(self.action_targets());
                if targets.is_empty() {
                    if drafts.is_empty() {
                        self.show_toast("No conversation", true);
                    } else if action == Action::Delete && self.in_drafts() {
                        return Step {
                            discard: drafts,
                            ..Step::default()
                        };
                    } else {
                        self.show_toast(DRAFT_NOT_THREAD, true);
                    }
                    return Step::default();
                }
                if action == Action::Spam {
                    return Step {
                        spam: true,
                        ..Step::default()
                    };
                }
                let trash = if self.search.is_some() {
                    let Some(scopes) = self.search_scopes(&targets) else {
                        self.show_toast("No folder for a selected match", true);
                        return step;
                    };
                    let trash = scopes
                        .iter()
                        .all(|(_, folder)| self.roles.is(FolderRole::Trash, folder));
                    if trash && scopes.len() != targets.len() {
                        self.show_toast("No folder for a selected match", true);
                        return step;
                    }
                    trash
                } else {
                    self.in_trash()
                };
                if !trash {
                    return Step {
                        delete: true,
                        ..Step::default()
                    };
                }
                // Destructive and not undoable: ask, and show the answer key.
                let target = match targets.as_slice() {
                    [id] => self.threads.iter().find(|row| &row.id == id).map_or_else(
                        || "1 marked".into(),
                        |row| {
                            let subject = display_safe(&row.subject).replace(['“', '”'], "'");
                            if subject.trim().is_empty() {
                                "“(no subject)”".to_string()
                            } else {
                                format!("“{subject}”")
                            }
                        },
                    ),
                    ids => format!("{} marked", ids.len()),
                };
                self.show_toast(
                    format!(
                        "Remove from Fell only?{NOTICE_DOT}{target}{NOTICE_DOT}No undo{NOTICE_DOT}y"
                    ),
                    true,
                );
                self.confirm_purge = Some(PurgePrompt {
                    ids: targets,
                    folder: self.folder.clone(),
                    notice: self.toast_generation,
                });
            }
            Action::Undo => {
                self.count.clear();
                self.cancel_read_delay();
                return Step {
                    undo: true,
                    ..Step::default()
                };
            }
            Action::SyncNow => {
                self.count.clear();
                return Step {
                    sync_now: true,
                    ..Step::default()
                };
            }
            Action::Notifications(mode) => {
                self.count.clear();
                step.notification_mode = Some(mode);
            }
            Action::ToggleSelect => {
                self.count.clear();
                if let Some(id) = self.cursor_id.clone()
                    && !self.picked.remove(&id)
                {
                    self.picked.insert(id);
                }
            }
            Action::Visual => {
                self.count.clear();
                if self.visual {
                    self.visual = false;
                } else {
                    self.visual = true;
                    self.anchor_id = self.cursor_id.clone();
                }
            }
            Action::FullWidth => {
                self.count.clear();
                self.full_width = !self.full_width;
                if self.full_width {
                    self.reading_return_pane = self.pane;
                    if self.pane == Pane::List {
                        self.pane = Pane::Conversation;
                        self.opened(&mut step);
                    }
                } else if self.pane == Pane::Conversation {
                    self.pane = self.reading_return_pane;
                }
            }
            Action::ToggleQuotes => {
                self.count.clear();
                if self.messages.get(self.message_cursor).is_some() {
                    self.toggle_open(self.message_cursor);
                    step.message_disclosure_changed = true;
                }
            }
            Action::Palette => {
                self.count.clear();
                self.palette_from_agents = self.overlay == Overlay::Agents;
                self.overlay = Overlay::Palette;
                self.palette_query.clear();
                self.palette_index = 0;
            }
            Action::Help => {
                self.count.clear();
                self.overlay = Overlay::Help;
            }
            Action::About => {
                self.count.clear();
                self.overlay = Overlay::About;
            }
            Action::Agents => {
                self.count.clear();
                self.overlay = Overlay::Agents;
                step.agents = self.agents.open();
            }
            Action::AgentControl(_) | Action::UndoAgentRevoke => {}
            Action::OpenDiagnosticLog | Action::OpenCrashReport => {
                self.count.clear();
                step.open_diagnostic = Some(match action {
                    Action::OpenDiagnosticLog => DiagnosticFile::Log,
                    _ => DiagnosticFile::Crash,
                });
            }
            Action::ReopenReview => {
                self.count.clear();
                return Step {
                    reopen_review: true,
                    ..Step::default()
                };
            }
            Action::Compose => {
                self.count.clear();
                return Step {
                    compose: true,
                    ..Step::default()
                };
            }
            Action::FocusAttachments => {
                self.count.clear();
                let index = if self.pane == Pane::List {
                    self.messages.len().saturating_sub(1)
                } else {
                    self.message_cursor
                };
                if self.focused_attachment().is_some() {
                    step.reveal_attachment = true;
                } else if let Some(part) = self
                    .messages
                    .get(index)
                    .and_then(|message| message.attachments.first())
                    .cloned()
                {
                    let was_list = self.pane == Pane::List;
                    step.message_disclosure_changed = !self.messages[index].expanded;
                    self.messages[index].expanded = true;
                    self.focus_attachment(index, part);
                    if was_list {
                        self.opened(&mut step);
                    }
                    step.reveal_attachment = true;
                } else {
                    self.show_toast("No attachments on this message", false);
                }
            }
            Action::PreviousAttachment | Action::NextAttachment => {
                let count = self.take_count();
                if let Some(target) = self.focused_attachment() {
                    let parts = &self.messages[self.message_cursor].attachments;
                    let index = parts.iter().position(|part| *part == target.part).unwrap();
                    let next = if action == Action::PreviousAttachment {
                        index.saturating_sub(count)
                    } else {
                        index.saturating_add(count).min(parts.len() - 1)
                    };
                    self.focus_attachment(self.message_cursor, parts[next].clone());
                    step.reveal_attachment = true;
                }
            }
            Action::OpenAttachment | Action::SaveAttachment | Action::SaveAllAttachments => {
                self.count.clear();
                step.attachment = match action {
                    Action::OpenAttachment => self
                        .focused_attachment()
                        .cloned()
                        .map(AttachmentRequest::Open),
                    Action::SaveAttachment => self
                        .focused_attachment()
                        .cloned()
                        .map(|target| AttachmentRequest::Save(vec![target])),
                    _ => self
                        .reply_target()
                        .filter(|message| !message.attachments.is_empty())
                        .map(|message| {
                            AttachmentRequest::Save(
                                message
                                    .attachments
                                    .iter()
                                    .cloned()
                                    .map(|part| AttachmentTarget {
                                        message: message.id.clone(),
                                        part,
                                    })
                                    .collect(),
                            )
                        }),
                };
                if step.attachment.is_none() {
                    let text = if action == Action::SaveAllAttachments {
                        "No attachments to save"
                    } else {
                        "Focus an attachment first"
                    };
                    self.show_toast(text, false);
                }
            }
            Action::RetryAttachment => {
                self.count.clear();
                step.retry_attachment = true;
            }
            Action::Reply | Action::ReplyAll | Action::Forward => {
                self.count.clear();
                let kind = match action {
                    Action::Reply => ReplyKind::Reply,
                    Action::ReplyAll => ReplyKind::ReplyAll,
                    _ => ReplyKind::Forward,
                };
                if self.reply_target().is_none() {
                    let what = if kind == ReplyKind::Forward {
                        "forward"
                    } else {
                        "reply to"
                    };
                    self.show_toast(format!("No message to {what} here"), false);
                    return Step::default();
                }
                return Step {
                    reply: Some(kind),
                    ..Step::default()
                };
            }
        }
        step
    }

    /// Open the account's folder for `role`, whatever the server calls it.
    fn go_role(&mut self, role: FolderRole) -> bool {
        let folder = self.roles.name(role).to_string();
        self.go_folder(&folder)
    }

    /// The conversation pane just opened on the selected thread: an unread
    /// one is marked read now, or after the read delay.
    fn opened(&mut self, step: &mut Step) {
        let Some(row) = self.cursor_thread() else {
            return;
        };
        if !row.unread || row.draft.is_some() {
            return;
        }
        let Some(folder) = self.search_source(&row.id) else {
            return;
        };
        let id = row.id.clone();
        if self.read_dwell_ms == 0 {
            step.flags = self.mark_rows(vec![id], FlagChange::Read);
        } else {
            // The open it belongs to is filled in by `on_key`.
            step.read_after = Some(ReadDwell {
                id,
                folder: folder.to_owned(),
                generation: 0,
            });
        }
    }

    /// The read delay ran out. Mark it read if it is still the
    /// thread open in the conversation pane, from the same open
    /// and folder. Wait for a pending search refresh to confirm the scope.
    pub fn read_after_dwell(&mut self, read: ReadDwell) -> Option<FlagEdit> {
        let open = read.generation == self.read_gen
            && self.pane == Pane::Conversation
            && self.cursor_id.as_deref() == Some(read.id.as_str());
        if !open || !self.cursor_thread().is_some_and(|row| row.unread) {
            return None;
        }
        if self.search.as_ref().is_some_and(|search| search.pending) {
            self.deferred_read = Some(read);
            return None;
        }
        if self.search_source(&read.id) != Some(read.folder.as_str()) {
            return None;
        }
        self.mark_rows(vec![read.id], FlagChange::Read)
    }

    pub fn resume_read_after_search(&mut self) -> Option<FlagEdit> {
        let read = self.deferred_read.take()?;
        self.read_after_dwell(read)
    }

    /// `U` (`star` false) or `s` on the targets. All already unread (or
    /// starred) turns it off; otherwise every target gets it.
    fn toggle_flag(&mut self, star: bool) -> Step {
        let targets = self.action_targets();
        let multiple_selected = targets.len() > 1;
        let (mut ids, drafts) = self.split_removal_targets(targets);
        if ids.is_empty() {
            let text = if !drafts.is_empty() {
                DRAFT_NOT_THREAD
            } else {
                "No conversation"
            };
            self.show_toast(text, true);
            return Step::default();
        }
        if self.search.is_some() {
            ids.retain(|id| self.search_source(id).is_some());
            if ids.is_empty() {
                self.show_toast("No folder for a selected match", true);
                return Step::default();
            }
        }
        let rows: Vec<&ThreadRow> = self
            .threads
            .iter()
            .filter(|row| ids.contains(&row.id))
            .collect();
        let off_page = rows.len() != ids.len();
        let change = if star {
            if rows.iter().all(|row| row.starred) {
                FlagChange::Unstar
            } else {
                FlagChange::Star
            }
        } else if rows.iter().all(|row| row.unread) {
            FlagChange::Read
        } else {
            FlagChange::Unread
        };
        // The selection was the operand: it is used up, as in vim.
        if multiple_selected {
            self.visual = false;
            self.anchor_id = None;
            self.picked.clear();
        }
        if !star {
            self.cancel_read_delay();
        }
        if off_page {
            return Step {
                off_page_flags: Some((ids, star)),
                ..Step::default()
            };
        }
        let count = ids.len();
        // Quiet: a single row on screen with the list focused shows the
        // change itself. Say it only when the reader cannot see it there.
        let toast = count > 1 || !self.flag_targets_in_view(&ids);
        let flags = self.mark_rows(ids, change);
        if toast {
            // A selection counts the rows that changed, not the ones that
            // already had it.
            let changed = flags.as_ref().map_or(0, |edit| edit.ids.len());
            self.show_flag_notice(change, count, changed);
        }
        Step {
            flags,
            ..Step::default()
        }
    }

    pub fn show_flag_notice(&mut self, change: FlagChange, selected: usize, changed: usize) {
        let what = match change {
            FlagChange::Read => "marked read",
            FlagChange::Unread => "marked unread",
            FlagChange::Star => "starred",
            FlagChange::Unstar => "unstarred",
        };
        let text = if selected == 1 {
            let mut text = what.to_string();
            text[..1].make_ascii_uppercase();
            text
        } else {
            format!("{changed} {what}")
        };
        self.show_toast(text, false);
    }

    /// The list has focus and every row in `ids` is on screen.
    fn flag_targets_in_view(&self, ids: &[String]) -> bool {
        let Some(range) = &self.rows_in_view else {
            return false;
        };
        self.pane == Pane::List
            && ids.iter().all(|id| {
                self.threads
                    .iter()
                    .position(|row| &row.id == id)
                    .is_some_and(|index| range.contains(&index))
            })
    }

    /// Show `change` on the loaded rows `ids` now, in this frame, and
    /// return the store write for the view. `None` when no row changes.
    pub(crate) fn mark_rows(&mut self, ids: Vec<String>, change: FlagChange) -> Option<FlagEdit> {
        let mut changed = Vec::new();
        for row in self.threads.iter_mut().filter(|row| ids.contains(&row.id)) {
            let (unread, starred) = match change {
                FlagChange::Read => (false, row.starred),
                FlagChange::Unread => (true, row.starred),
                FlagChange::Star => (row.unread, true),
                FlagChange::Unstar => (row.unread, false),
            };
            if self.search.is_none() && unread != row.unread {
                if unread {
                    self.unread_count += 1;
                } else {
                    self.unread_count = self.unread_count.saturating_sub(1);
                }
            }
            if unread != row.unread || starred != row.starred {
                row.unread = unread;
                row.starred = starred;
                changed.push(row.id.clone());
            }
        }
        (!changed.is_empty()).then_some(FlagEdit {
            ids: changed,
            change,
        })
    }

    fn go_folder(&mut self, folder: &str) -> bool {
        self.count.clear();
        self.end_search();
        let was_label = self.label_scope.take().is_some();
        if self.folder == folder && !was_label {
            return false;
        }
        self.folder = folder.to_string();
        self.forget_window();
        true
    }

    fn go_label(&mut self, label: &str) -> bool {
        self.end_search();
        if self.label_scope.as_deref() == Some(label) {
            return false;
        }
        self.label_scope = Some(label.into());
        self.forget_window();
        true
    }

    fn page_steps(&mut self, page_rows: usize) -> usize {
        self.take_count().saturating_mul(page_rows.max(1))
    }

    fn jump_first(&mut self) -> bool {
        let count = self.take_count();
        self.jump_index(count.saturating_sub(1))
    }

    fn jump_last(&mut self) -> bool {
        if self.count.is_empty() {
            if self.pane == Pane::List {
                return self.list_last();
            }
            self.jump_index(self.messages.len().saturating_sub(1))
        } else {
            let count = self.take_count();
            self.jump_index(count.saturating_sub(1))
        }
    }

    fn jump_index(&mut self, index: usize) -> bool {
        match self.pane {
            Pane::List => {
                if self.threads.is_empty() {
                    return false;
                }
                self.list_row(index)
            }
            Pane::Conversation => {
                if self.messages.is_empty() {
                    return false;
                }
                let index = index.min(self.messages.len() - 1);
                self.message_cursor = index;
                false
            }
        }
    }

    fn move_cursor(&mut self, motion: Motion) -> bool {
        match self.pane {
            Pane::List => self.move_list(motion),
            Pane::Conversation => {
                let len = self.messages.len();
                if len == 0 {
                    return false;
                }
                self.message_cursor = step_index(self.message_cursor, len, motion);
                false
            }
        }
    }

    fn take_count(&mut self) -> usize {
        let n = if self.count.is_empty() {
            1
        } else {
            // A prefix that does not fit in usize still means "as far as possible".
            self.count.parse::<usize>().unwrap_or(usize::MAX).max(1)
        };
        self.count.clear();
        n
    }

    /// Drop a typed count, an unfinished chord, and a cursor move waiting
    /// for rows. Mouse selection calls this so the next key does not finish
    /// the prefix, and a late page does not move the row just clicked.
    /// A permanent-delete question waiting for `y` is withdrawn too.
    ///
    /// The place of a row just archived is kept while nothing is selected:
    /// it is where the cursor stands, not a key. Selecting a row ends it.
    pub fn cancel_pending_keys(&mut self) {
        if self.gap().is_some() {
            if let Some(Pending::After { down, .. }) = &mut self.pending {
                *down = 0;
            }
        } else {
            self.take_pending();
        }
        self.count.clear();
        self.chord.clear();
        self.note_chord();
        // A click withdraws the permanent-delete question, and it leaves.
        self.withdraw_purge();
    }

    fn clear_chord(&mut self) {
        if self.chord.is_empty() && !self.which_visible {
            return;
        }
        self.chord.clear();
        self.note_chord();
    }

    fn note_chord(&mut self) {
        self.chord_gen = self.chord_gen.wrapping_add(1);
        self.which_visible = self.keymap.which_key_ms == 0 && !self.chord.is_empty();
    }

    pub fn list_banner(&self, folder: &str, label: Option<&str>) -> Option<Banner> {
        if !self.threads.is_empty() {
            return None;
        }
        if let Some(error) = &self.error {
            return Some(Banner {
                title: error.clone(),
                hint: "The folder could not be read.".into(),
            });
        }
        if self.loading {
            return Some(Banner {
                title: format!("Opening {}…", self.folder_title()),
                hint: String::new(),
            });
        }
        let title = || {
            label
                .map(str::to_owned)
                .unwrap_or_else(|| self.roles.label(folder))
        };
        let retry = keymap::binding_text(&self.keymap, Action::SyncNow);
        Some(match &self.sync {
            SyncKind::FirstSync => Banner {
                title: "No account is set up".into(),
                hint: "Run fell account setup in a terminal, then reopen Fell.".into(),
            },
            SyncKind::StoreError => Banner {
                title: self
                    .startup_error
                    .clone()
                    .unwrap_or_else(|| "Couldn't open the mailbox".into()),
                hint: "? details".into(),
            },
            SyncKind::Connecting | SyncKind::Syncing { .. } => Banner {
                title: format!("Syncing {}", title()),
                hint: match &self.sync {
                    SyncKind::Syncing {
                        new_messages: Some(count),
                        ..
                    } if *count > 0 => {
                        format!("{count} messages stored · conversations appear as they arrive")
                    }
                    _ => "Conversations appear as they arrive".into(),
                },
            },
            SyncKind::Failed(failure) => {
                let title = match failure.kind {
                    SyncFailureKind::Unreachable if failure.bridge => {
                        "Can't reach Proton Mail Bridge"
                    }
                    SyncFailureKind::Unreachable => "Can't reach the mail server",
                    SyncFailureKind::LoginRejected if failure.bridge => {
                        "Bridge didn't accept the login"
                    }
                    SyncFailureKind::LoginRejected => "The server didn't accept the login",
                    SyncFailureKind::Password => "Account password unavailable",
                    SyncFailureKind::Tls => "Couldn't establish a secure connection",
                    SyncFailureKind::Other => "Sync failed",
                };
                let hint = failure
                    .recovery
                    .map_or_else(|| format!("? details · {retry} retry"), str::to_owned);
                Banner {
                    title: title.into(),
                    hint,
                }
            }
            SyncKind::FixtureOffline | SyncKind::LocalOffline | SyncKind::Synced { .. }
                if label.is_none() && self.roles.is(FolderRole::Drafts, folder) =>
            {
                Banner {
                    title: "No drafts".into(),
                    hint: format!(
                        "{} compose · gi inbox",
                        keymap::binding_text(&self.keymap, Action::Compose)
                    ),
                }
            }
            SyncKind::FixtureOffline | SyncKind::LocalOffline | SyncKind::Synced { .. } => Banner {
                title: format!("No conversations in {}", title()),
                hint: String::new(),
            },
        })
    }
}

/// Archive, move, delete, and spam act on threads. A draft row is not one.
pub const DRAFT_NOT_THREAD: &str = "That is a draft · enter opens it in the editor";

/// Where `row` sits in its folder list.
pub fn row_key(row: &ThreadRow) -> ListKey {
    ListKey {
        date_unix: row.date_unix,
        thread_id: ThreadId(row.id.clone()),
    }
}

fn signed_count(count: usize) -> i32 {
    i32::try_from(count).unwrap_or(i32::MAX).clamp(1, 10_000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::keymap_from_text;
    use crate::present::NameSpan;

    #[test]
    fn a_first_sync_names_its_folder_and_how_far_it_is() {
        let progress = fell_core::SyncProgress {
            folder: "Inbox".into(),
            done: 4_210,
            total: 31_882,
        };
        for (new_messages, expected) in [
            (None, "syncing Inbox · 4,210 of 31,882"),
            (Some(2), "syncing Inbox · 4,210 of 31,882 · 2 new"),
        ] {
            let syncing = SyncKind::Syncing {
                new_messages,
                progress: Some(progress.clone()),
            };
            assert_eq!(syncing.status_at(0), expected);
            assert!(!syncing.is_error());
        }
        for (count, text) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (1_234_567, "1,234,567"),
        ] {
            assert_eq!(grouped(count), text);
        }
    }

    #[test]
    fn catch_up_labels_only_confirmed_inserts_as_new() {
        for (new_messages, expected) in [
            (Some(3), "syncing · 3 new"),
            (Some(0), "syncing…"),
            (None, "syncing…"),
        ] {
            let status = SyncKind::Syncing {
                new_messages,
                progress: None,
            };
            assert_eq!(status.status_at(0), expected);
        }
    }

    #[test]
    fn catch_up_keeps_the_final_total_even_if_the_last_batch_was_throttled() {
        let mut session = session();
        for (count, label) in [(0, "syncing…"), (1, "syncing · 1 new")] {
            session.apply_sync_status(SyncKind::Syncing {
                new_messages: Some(count),
                progress: None,
            });
            assert_eq!(session.sync.status_at(100), label);
        }

        session.apply_sync_status(SyncKind::Synced {
            at: 100,
            changed: true,
            new_messages: Some(3),
        });
        assert_eq!(session.sync.status_at(100), "synced just now · 3 new");
        assert_eq!(session.sync.status_at(159), "synced just now · 3 new");
        assert_eq!(session.sync.status_at(160), "synced 1m ago");

        for new_messages in [Some(0), None] {
            session.apply_sync_status(SyncKind::Synced {
                at: 101,
                changed: false,
                new_messages,
            });
            assert_eq!(session.sync.status_at(101), "synced just now");
        }
    }

    #[test]
    fn worker_status_replaces_only_the_manual_sync_acknowledgement() {
        let mut session = session();
        session.show_sync_requested();
        assert_eq!(session.toast.as_ref().unwrap().text, "Syncing…");
        session.apply_sync_status(SyncKind::Connecting);
        assert!(session.toast.is_none());

        session.show_sync_requested();
        session.show_toast("Read/star sync delayed. Retrying", true);
        session.apply_sync_status(SyncKind::Failed(SyncFailure {
            kind: SyncFailureKind::Unreachable,
            bridge: false,
            detail: "test timeout".into(),
            recovery: None,
        }));
        assert_eq!(session.sync.status_at(0), "offline · server unreachable");
        let notice = session.toast.unwrap();
        assert_eq!(notice.text, "Read/star sync delayed. Retrying");
        assert!(notice.alert);
    }

    #[test]
    fn connection_failure_banner_shows_the_recovery_hint() {
        let config = AccountConfig::parse(
            "backend = \"bridge\"\nemail = \"me@example.test\"\npassword_source = \"keyring\"",
        )
        .unwrap();
        let mut session = session();
        session.set_threads(Vec::new());

        for (error, title, hint) in [
            (
                Error::Keyring(fell_core::credentials::KeyringError::Locked),
                "Account password unavailable",
                "Unlock your desktop keyring and try again.",
            ),
            (
                Error::Tls("test certificate failure".into()),
                "Couldn't establish a secure connection",
                "Check the server host, port, TLS mode, and certificate, then try again.",
            ),
        ] {
            session.apply_sync_status(SyncKind::Failed(SyncFailure::connection(&config, &error)));
            let banner = session
                .list_banner(&session.folder, session.label_scope.as_deref())
                .unwrap();
            assert_eq!(banner.title, title);
            assert_eq!(banner.hint, hint);
        }
    }

    fn row(id: &str) -> ThreadRow {
        ThreadRow {
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
            unread: false,
            draft: None,
        }
    }

    #[test]
    fn c_composes_and_enter_opens_a_draft_row_in_the_editor() {
        let mut session = session();
        let step = session.on_key(KeyId::Char('c'), 10);
        assert!(step.compose && step.arm_which);
        let mut draft = row("draft-1");
        draft.draft = Some("c1".into());
        session.set_threads(vec![draft, row("a")]);
        session.cursor_id = Some("draft-1".into());
        assert_eq!(session.cursor_draft(), Some("c1"));
        let step = session.on_key(KeyId::Enter, 10);
        assert!(step.open_draft);
        assert_eq!(session.pane, Pane::List, "a draft opens in the editor");
        session.cursor_id = Some("a".into());
        let step = session.on_key(KeyId::Enter, 10);
        assert!(!step.open_draft);
        assert_eq!(session.pane, Pane::Conversation);
    }

    #[test]
    fn hash_discards_only_the_selected_local_drafts() {
        let mut session = session();
        session.folder = session.roles.name(FolderRole::Drafts).into();
        let mut first = row("draft-1");
        first.draft = Some("c1".into());
        let mut second = row("draft-2");
        second.draft = Some("r2".into());
        session.set_threads(vec![first, second, row("synced")]);
        let hash = KeyId::Char('#');
        session.cursor_id = Some("draft-1".into());
        assert_eq!(session.on_key(hash, 10).discard, ["c1"]);
        session.on_key(KeyId::Char('x'), 10);
        session.on_key(KeyId::Char('j'), 10);
        session.on_key(KeyId::Char('x'), 10);
        assert_eq!(session.on_key(hash, 10).discard, ["c1", "r2"]);

        session.picked.clear();
        session.cursor_id = Some("draft-1".into());
        session.on_key(KeyId::Char('V'), 10);
        session.on_key(KeyId::Char('j'), 10);
        assert_eq!(session.on_key(hash, 10).discard, ["c1", "r2"]);
        session.on_key(KeyId::Char('j'), 10);
        let mixed = session.on_key(hash, 10);
        assert!(mixed.discard.is_empty() && mixed.delete);
        let (threads, _) = session.split_removal_targets(session.action_targets());
        assert_eq!(threads, ["synced"]);
        session.visual = false;
        assert!(session.on_key(hash, 10).delete);
    }

    #[test]
    fn a_body_filled_in_keeps_the_reader_where_they_are() {
        let mut session = session();
        let mut older = empty_message(true);
        older.body_state = fell_core::BodyState::Pending;
        let mut newest = empty_message(true);
        newest.body_state = fell_core::BodyState::Pending;
        session.set_messages(vec![older.clone(), newest.clone()]);
        // The reader went up to the older message, closed the newest and
        // opened the older one's quotes.
        session.message_cursor = 0;
        session.messages[1].expanded = false;
        session.quotes_open.insert(older.id.clone());

        // Sync fills in the older message's body (not the latest's).
        let mut read = vec![older.clone(), newest.clone()];
        read[0].body_state = fell_core::BodyState::Stored;
        read[0].body = crate::text_render::render_stored_body(Some("it came"), "it came", "");
        read[1].expanded = true;
        let changed = crate::present::bodies_filled(&session.messages, &read);
        assert_eq!(changed, Some(vec![0]), "an in-place fill, of message 0");
        session.refresh_same_messages(read);
        assert_eq!(session.message_cursor, 0, "the cursor stays");
        assert!(
            !session.messages[1].expanded,
            "what was closed stays closed"
        );
        assert!(session.quotes_open.contains(&older.id));
        assert_eq!(session.messages[0].body_state, fell_core::BodyState::Stored);
    }

    #[test]
    fn a_metadata_reread_updates_a_pending_message_without_landing() {
        let mut session = session();
        let mut pending = empty_message(true);
        pending.body_state = fell_core::BodyState::Pending;
        session.set_messages(vec![pending.clone()]);
        session.toggle_open(0);

        let mut read = pending;
        read.folders = vec!["Inbox".into(), "Archive".into()];
        assert_eq!(
            crate::present::bodies_filled(&session.messages, std::slice::from_ref(&read)),
            Some(vec![])
        );
        session.refresh_same_messages(vec![read]);
        assert_eq!(session.messages[0].folders, ["Inbox", "Archive"]);
        assert_eq!(
            session.messages[0].body_state,
            fell_core::BodyState::Pending
        );
        assert!(session.is_open(&session.messages[0].id));
        assert!(!session.refresh_messages(session.messages.clone(), true));
        assert!(session.is_open(&session.messages[0].id));
    }

    #[test]
    fn r_shift_r_and_f_answer_the_message_in_view_or_the_latest() {
        let mut session = session();
        session.cursor_id = Some("a".into());
        // Nothing read yet: nothing to answer, and the person is told.
        let step = session.on_key(KeyId::Char('r'), 10);
        assert_eq!(step.reply, None);
        assert!(session.toast.is_some());
        let first = empty_message(false);
        let second = empty_message(true);
        session.set_messages(vec![first.clone(), second.clone()]);
        // From the list, the latest message.
        let step = session.on_key(KeyId::Char('r'), 10);
        assert_eq!(step.reply, Some(ReplyKind::Reply));
        assert!(step.arm_which);
        assert_eq!(session.reply_target().map(|m| &m.id), Some(&second.id));
        // In the conversation, the one under the cursor.
        session.pane = Pane::Conversation;
        session.message_cursor = 0;
        let step = session.on_key(KeyId::Char('R'), 10);
        assert_eq!(step.reply, Some(ReplyKind::ReplyAll));
        assert_eq!(session.reply_target().map(|m| &m.id), Some(&first.id));
        let step = session.on_key(KeyId::Char('f'), 10);
        assert_eq!(step.reply, Some(ReplyKind::Forward));
        // A count does not repeat it.
        session.on_key(KeyId::Char('3'), 10);
        let step = session.on_key(KeyId::Char('r'), 10);
        assert_eq!(step.reply, Some(ReplyKind::Reply));
        assert!(session.count.is_empty());
        // A draft row is not a message to answer.
        let mut draft = row("draft-1");
        draft.draft = Some("c1".into());
        session.set_threads(vec![draft, row("a")]);
        session.cursor_id = Some("draft-1".into());
        let step = session.on_key(KeyId::Char('f'), 10);
        assert_eq!(step.reply, None);
        assert_eq!(
            session.toast.as_ref().map(|toast| toast.text.as_str()),
            Some("No message to forward here")
        );
    }

    #[test]
    fn m_on_a_draft_row_explains_without_opening_the_picker() {
        let mut session = session();
        let mut draft = row("draft-1");
        draft.draft = Some("c1".into());
        session.set_threads(vec![draft, row("a")]);
        session.cursor_id = Some("draft-1".into());
        let step = session.on_key(KeyId::Char('m'), 10);
        assert!(!step.apply_move);
        assert_eq!(session.overlay, Overlay::None);
        let toast = session.toast.as_ref().expect("m says why");
        assert_eq!(toast.text, DRAFT_NOT_THREAD);
    }

    fn session() -> Session {
        let mut session =
            Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        session.set_threads(vec![row("a"), row("b"), row("c"), row("d"), row("e")]);
        session
    }

    fn type_picker(session: &mut Session, text: &str) {
        for ch in text.chars() {
            let key = if ch == ' ' {
                KeyId::Space
            } else {
                KeyId::Char(ch)
            };
            session.on_key(key, 10);
        }
    }

    fn load_test_label_targets(session: &mut Session) -> Step {
        let ids = session.label_target_ids();
        session.load_label_targets(Ok(session
            .threads
            .iter()
            .filter(|row| ids.contains(&row.id))
            .map(|row| {
                (
                    row.id.clone(),
                    fell_core::store::ThreadLabels {
                        subject: row.subject.clone(),
                        labels: row.labels.iter().cloned().collect(),
                    },
                )
            })
            .collect()))
    }

    #[test]
    fn label_picker_opens_with_checks_if_counts_arrive_before_the_deadline() {
        let mut session = session();
        session.labels = vec!["Work".into()];
        session.threads[0].labels = vec!["Work".into()];
        assert!(session.on_key(KeyId::Char('l'), 10).read_label_targets);
        assert_eq!(session.visible_overlay(), Overlay::None);

        assert!(load_test_label_targets(&mut session).labels.is_none());
        assert_eq!(session.visible_overlay(), Overlay::Folders);
        assert_eq!(session.picker_choices()[0].mark, Some("✓"));
    }

    #[test]
    fn the_label_picker_can_open_before_late_counts_arrive() {
        let mut session = session();
        session.labels = vec!["Work".into()];
        session.threads[0].labels = vec!["Work".into()];
        session.on_key(KeyId::Char('l'), 10);
        session.open_label_picker();
        assert_eq!(session.visible_overlay(), Overlay::Folders);
        assert_eq!(session.picker_choices()[0].mark, None);

        assert!(load_test_label_targets(&mut session).labels.is_none());
        assert_eq!(session.picker_choices()[0].mark, Some("✓"));
    }

    #[test]
    fn enter_queued_during_label_lookup_uses_the_choice_at_the_time_of_the_key() {
        for timed_out in [false, true] {
            let mut session = session();
            session.labels = vec!["Travel".into(), "Work".into()];
            session.threads[0].labels = vec!["Work".into()];
            session.on_key(KeyId::Char('l'), 10);
            if timed_out {
                session.open_label_picker();
            }
            type_picker(&mut session, "work");
            assert!(session.on_key(KeyId::Enter, 10).labels.is_none());
            session.on_key(KeyId::Backspace, 10);

            let edit = load_test_label_targets(&mut session).labels.unwrap();
            assert_eq!(edit.name, "Work");
            assert_eq!(edit.ids, ["a"]);
            assert!(!edit.add);
            assert_eq!(session.visible_overlay(), Overlay::None);
        }
    }

    #[test]
    fn failing_label_lookup_drops_the_queued_enter_and_explains_why() {
        let mut session = session();
        session.labels = vec!["Work".into()];
        session.on_key(KeyId::Char('l'), 10);
        session.open_label_picker();
        session.on_key(KeyId::Enter, 10);

        let step = session.load_label_targets(Err("Store unavailable".into()));
        assert!(step.labels.is_none());
        assert_eq!(session.visible_overlay(), Overlay::None);
        let toast = session.toast.as_ref().unwrap();
        assert_eq!(toast.text, "Could not read labels · Store unavailable");
        assert!(toast.alert);
        assert!(load_test_label_targets(&mut session).labels.is_none());
    }

    #[test]
    fn label_picker_creates_adds_and_removes_through_the_palette_and_keys() {
        let mut session = session();
        session.labels = vec!["Design review".into(), "Travel".into()];
        let item = keymap::palette_items(&session.keymap, "apply label", &[])
            .into_iter()
            .find(|item| item.action == Action::Label)
            .unwrap();
        session.confirm_palette(item, 10);
        load_test_label_targets(&mut session);
        type_picker(&mut session, "drev");
        assert_eq!(session.picker_choices()[0].name, "Design review");
        let add = session.on_key(KeyId::Enter, 10).labels.unwrap();
        assert_eq!(
            add,
            LabelEdit {
                ids: vec!["a".into()],
                name: "Design review".into(),
                add: true
            }
        );
        session.label_rows(&add, false);

        session.on_key(KeyId::Char('l'), 10);
        load_test_label_targets(&mut session);
        assert_eq!(session.picker_choices()[0].mark, Some("✓"));
        let remove = session.on_key(KeyId::Enter, 10).labels.unwrap();
        assert_eq!(remove, LabelEdit { add: false, ..add });
        session.label_rows(&remove, false);
        assert!(session.threads[0].labels.is_empty());

        session.on_key(KeyId::Char('l'), 10);
        load_test_label_targets(&mut session);
        type_picker(&mut session, "Client notes");
        assert_eq!(
            session.picker_choices()[0].title,
            "Create label “Client notes”"
        );
        let create = session.on_key(KeyId::Enter, 10).labels.unwrap();
        assert!(create.add);
        session.label_rows(&create, false);
        assert_eq!(session.threads[0].labels, ["Client notes"]);

        session.on_key(KeyId::Char('g'), 10);
        assert!(session.which_items().iter().any(|item| item.key == "l"));
        session.on_key(KeyId::Char('l'), 10);
        assert!(session.on_key(KeyId::Enter, 10).reload_threads);
        assert_eq!(session.label_scope.as_deref(), Some("Client notes"));
        assert!(session.perform(Action::Inbox, 10).reload_threads);
        assert_eq!(session.label_scope, None);
    }

    #[test]
    fn label_picker_fills_a_mixed_visual_range_and_removes_from_marked_rows() {
        let mut session = session();
        session.labels = vec!["review".into()];
        session.threads[0].labels.push("review".into());
        for key in ['V', 'j', 'j', 'l'] {
            session.on_key(KeyId::Char(key), 10);
        }
        load_test_label_targets(&mut session);
        assert_eq!(session.picker_choices()[0].mark, Some("−"));
        let add = session.on_key(KeyId::Enter, 10).labels.unwrap();
        assert_eq!(add.ids, ["a", "b", "c"]);
        assert!(add.add);
        assert!(!session.visual);
        session.label_rows(&add, false);
        for row in &session.threads[..3] {
            assert_eq!(row.labels, ["review"]);
        }

        session.on_key(KeyId::Escape, 10);
        for key in ['x', 'k', 'k', 'x', 'j', 'l'] {
            session.on_key(KeyId::Char(key), 10);
        }
        load_test_label_targets(&mut session);
        assert_eq!(session.picker_choices()[0].mark, Some("✓"));
        let remove = session.on_key(KeyId::Enter, 10).labels.unwrap();
        assert_eq!(remove.ids, ["a", "c"]);
        assert!(!remove.add);
        session.label_rows(&remove, false);
        assert!(session.threads[0].labels.is_empty());
        assert_eq!(session.threads[1].labels, ["review"]);
        assert!(session.threads[2].labels.is_empty());
        assert!(session.on_key(KeyId::Char('u'), 10).undo);
    }

    #[test]
    fn label_picker_toggles_a_selection_across_loaded_pages() {
        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let threads = store.list_threads(&account.id).unwrap();
        let first = &threads[0].thread_id;
        let last = &threads.last().unwrap().thread_id;
        store
            .label_thread(
                &account.id,
                first,
                fell_core::LabelChange::Add("Review".into()),
            )
            .unwrap();
        let mut session = session();
        session.labels = vec!["Review".into()];
        session.set_page(vec![row(first.as_str())], false, true, 2);
        session.on_key(KeyId::Char('x'), 10);
        session.set_page(vec![row(last.as_str())], true, false, 2);
        session.on_key(KeyId::Char('x'), 10);

        for (mark, add) in [(Some("−"), true), (Some("✓"), false)] {
            assert!(session.on_key(KeyId::Char('l'), 10).read_label_targets);
            let ids = session.label_target_ids();
            assert_eq!(ids, [last.as_str(), first.as_str()]);
            session.load_label_targets(Ok(store.thread_labels(&account.id, ids).unwrap()));
            assert_eq!(session.picker_choices()[0].mark, mark);
            let edit = session.on_key(KeyId::Enter, 10).labels.unwrap();
            assert_eq!(edit.ids, [last.as_str(), first.as_str()]);
            assert_eq!(edit.add, add);
            let change = if add {
                fell_core::LabelChange::Add(edit.name)
            } else {
                fell_core::LabelChange::Remove(edit.name)
            };
            store
                .label_threads(&account.id, &[first.clone(), last.clone()], change)
                .unwrap();
        }
        assert!(
            store
                .thread_labels(&account.id, session.label_target_ids())
                .unwrap()
                .values()
                .all(|thread| !thread.labels.contains("Review"))
        );
    }

    #[test]
    fn label_picker_skips_an_off_page_draft_in_a_mixed_selection() {
        let mut session = session();
        let draft_id = present::draft_row_id("local");
        let mut draft = row(&draft_id);
        draft.draft = Some("local".into());
        session.set_page(vec![draft], false, true, 0);
        assert!(!session.on_key(KeyId::Char('l'), 10).read_label_targets);
        assert_eq!(session.toast.as_ref().unwrap().text, DRAFT_NOT_THREAD);
        session.on_key(KeyId::Char('x'), 10);
        session.set_page(vec![row("a")], true, false, 0);
        assert!(session.picked.contains(&draft_id));
        assert!(!session.on_key(KeyId::Char('l'), 10).read_label_targets);
        assert_eq!(session.toast.as_ref().unwrap().text, DRAFT_NOT_THREAD);

        session.on_key(KeyId::Char('x'), 10);
        session.labels = vec!["Review".into()];
        assert!(session.on_key(KeyId::Char('l'), 10).read_label_targets);
        assert_eq!(session.label_target_ids(), ["a"]);
        load_test_label_targets(&mut session);
        assert_eq!(session.on_key(KeyId::Enter, 10).labels.unwrap().ids, ["a"]);
    }

    #[test]
    fn label_picker_uses_the_remaining_target_when_a_marked_thread_is_gone() {
        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let threads = store.list_threads(&account.id).unwrap();
        let remaining = &threads[0];
        let mut session = session();
        session.labels = vec!["Review".into()];
        session.set_page(vec![row("purged")], false, true, 0);
        session.on_key(KeyId::Char('x'), 10);
        session.set_page(vec![row(remaining.thread_id.as_str())], true, false, 0);
        session.on_key(KeyId::Char('x'), 10);
        assert!(session.on_key(KeyId::Char('l'), 10).read_label_targets);
        let targets = store
            .thread_labels(&account.id, session.label_target_ids())
            .unwrap();
        let subject = targets[remaining.thread_id.as_str()].subject.clone();
        assert_eq!(targets.len(), 1);
        session.load_label_targets(Ok(targets));
        assert_eq!(session.label_target_subject(), Some(subject.as_str()));
        let edit = session.on_key(KeyId::Enter, 10).labels.unwrap();
        assert_eq!(edit.ids, [remaining.thread_id.as_str()]);
        assert!(edit.add);
    }

    #[test]
    fn a_closing_label_picker_keeps_its_checked_row() {
        let mut motion = crate::inbox_motion::InboxMotion::new(crate::motion::Motion::new(true));
        let mut session = session();
        session.labels.push("work".into());
        session.threads[0].labels.push("work".into());
        session.perform(Action::Label, 10);
        load_test_label_targets(&mut session);
        type_picker(&mut session, "work");
        let now = std::time::Instant::now();
        motion.observe(&session, (true, true), now);
        assert!(!session.confirm_folder().labels.unwrap().add);
        session.set_labels(Vec::new());
        motion.observe(&session, (true, true), now);

        let (layer, _) = motion.closing_overlay(now).unwrap();
        layer.swap(&mut session);
        assert_eq!(session.folder_query, "work");
        assert_eq!(session.folder_index, 0);
        assert_eq!(layer.picker_choices.len(), 1);
        assert_eq!(layer.picker_choices[0].title, "work");
        assert_eq!(layer.picker_choices[0].mark, Some("✓"));
    }

    #[test]
    fn label_picker_filters_five_hundred_names() {
        let mut session = session();
        session.labels = (0..500).map(|i| format!("Project {i:03}")).collect();
        session.on_key(KeyId::Char('l'), 10);
        load_test_label_targets(&mut session);
        let mut slowest = std::time::Duration::ZERO;
        for key in "p427".chars() {
            let start = std::time::Instant::now();
            session.on_key(KeyId::Char(key), 10);
            std::hint::black_box(session.picker_choices());
            slowest = slowest.max(start.elapsed());
        }
        eprintln!("500 labels, slowest model keystroke: {slowest:?}");
        assert_eq!(session.picker_choices()[0].name, "Project 427");
    }

    #[test]
    fn note_shortcuts_accept_only_the_shown_note_and_only_from_the_conversation() {
        use fell_core::notes::{NoteDecision, PendingSuggestion, Suggestion, ThreadNote};

        let mut session = session();
        let shown = ThreadNote {
            id: 41,
            actor: "review".into(),
            created_unix: 10,
            summary: "A summary".into(),
            seen: false,
            suggestions: vec![
                PendingSuggestion {
                    position: 0,
                    action: Suggestion::Label("Work".into()),
                },
                PendingSuggestion {
                    position: 1,
                    action: Suggestion::Label("Follow up".into()),
                },
                PendingSuggestion {
                    position: 2,
                    action: Suggestion::Reply,
                },
            ],
        };
        let thread = session.cursor_id.clone().unwrap();
        session
            .agent_notes
            .update(Some(thread.clone()), vec![shown.clone()]);
        let picker = session.on_key(KeyId::Char('l'), 10);
        assert!(picker.note.is_none());
        assert!(picker.read_label_targets);
        session.on_key(KeyId::Escape, 10);
        session.pane = Pane::Conversation;
        let first = session.on_key(KeyId::Char('l'), 10).note.unwrap();
        assert_eq!(
            (first.id, first.decision, first.reply),
            (41, NoteDecision::Accept(0), false)
        );
        let second = session.on_key(KeyId::Char('L'), 10).note.unwrap();
        assert_eq!(second.decision, NoteDecision::Accept(1));
        let reply = session.on_key(KeyId::Char('r'), 10).note.unwrap();
        assert!(reply.reply);
        session.agent_notes.resolved(&first);
        assert_eq!(session.note().unwrap().suggestions.len(), 2);
        session.perform(Action::ToggleAgentNote, 10);
        let picker = session.on_key(KeyId::Char('l'), 10);
        assert!(picker.note.is_none());
        assert!(picker.read_label_targets);
        session.on_key(KeyId::Escape, 10);
        let dismiss = session.perform(Action::DismissAgentNote, 10).note.unwrap();
        assert_eq!(dismiss.decision, NoteDecision::Dismiss);
        session.agent_notes.resolved(&dismiss);
        assert!(session.note().is_none());
        session.agent_notes.update(Some(thread), vec![shown]);
        session.cursor_id = Some("b".into());
        assert!(session.perform(Action::DismissAgentNote, 10).note.is_none());
    }

    fn attachment_message(expanded: bool) -> ConvMessage {
        let mut message = empty_message(expanded);
        message.attachments = (2..5)
            .map(|section| fell_core::MimePart {
                section: section.to_string(),
                role: fell_core::PartRole::Attachment,
                mime_type: "application/pdf".into(),
                filename: Some(format!("report-{section}.pdf")),
                size: 42,
                encoded_size: 42,
                encoding: fell_core::TransferEncoding::Identity,
                content_id: None,
                disposition: fell_core::Disposition::Attachment,
                nested: false,
            })
            .collect();
        message
    }

    #[test]
    fn attachment_keys_focus_expand_move_open_and_save() {
        let mut session = session();
        session.set_messages(vec![attachment_message(false), empty_message(true)]);
        session.pane = Pane::Conversation;
        session.message_cursor = 0;

        let step = session.on_key(KeyId::Char('a'), 10);
        assert!(step.reveal_attachment && step.message_disclosure_changed);
        assert!(session.messages[0].expanded);
        assert_eq!(session.mode_label(), "attachments");
        assert_eq!(session.focused_attachment().unwrap().part.section, "2");
        session.on_key(KeyId::Char('l'), 10);
        let middle = session.focused_attachment().unwrap().clone();
        assert_eq!(middle.part.section, "3");
        for key in [KeyId::Enter, KeyId::Char('o')] {
            assert_eq!(
                session.on_key(key, 10).attachment,
                Some(AttachmentRequest::Open(middle.clone())),
            );
        }
        assert_eq!(
            session.on_key(KeyId::Char('S'), 10).attachment,
            Some(AttachmentRequest::Save(vec![middle])),
        );
        for key in ['h', 'h'] {
            session.on_key(KeyId::Char(key), 10);
        }
        assert_eq!(session.focused_attachment().unwrap().part.section, "2");
        for key in ['9', 'l'] {
            session.on_key(KeyId::Char(key), 10);
        }
        assert_eq!(session.focused_attachment().unwrap().part.section, "4");

        session.on_key(KeyId::Space, 10);
        session.on_key(KeyId::Char('a'), 10);
        let Some(AttachmentRequest::Save(targets)) =
            session.on_key(KeyId::Char('s'), 10).attachment
        else {
            panic!("save-all targets the current message");
        };
        assert_eq!(targets.len(), 3);
        assert!(
            targets
                .iter()
                .all(|target| target.message == session.messages[0].id)
        );
        assert_eq!(targets[0].part.section, "2");
        session.on_key(KeyId::Space, 10);
        session.on_key(KeyId::Char('a'), 10);
        assert!(session.on_key(KeyId::Char('R'), 10).retry_attachment);

        session.on_key(KeyId::Escape, 10);
        assert_eq!(session.pane, Pane::Conversation);
        assert!(session.focused_attachment().is_none());
        assert!(session.on_key(KeyId::Enter, 10).message_disclosure_changed);
        session.on_key(KeyId::Escape, 10);
        assert_eq!(session.pane, Pane::List);
    }

    #[test]
    fn reading_mode_returns_focus_to_the_pane_it_opened_from() {
        for pane in [Pane::List, Pane::Conversation] {
            let mut session = session();
            session.pane = pane;
            session.on_key(KeyId::Char('z'), 10);
            assert!(session.full_width);
            assert_eq!(session.pane, Pane::Conversation);
            session.on_key(KeyId::Char('z'), 10);
            assert!(!session.full_width);
            assert_eq!(session.pane, pane);
        }
    }

    #[test]
    fn executable_confirmation_is_bound_to_the_focused_file_and_notice() {
        let mut session = session();
        session.set_messages(vec![attachment_message(true)]);
        session.on_key(KeyId::Char('a'), 10);
        let target = session.focused_attachment().unwrap().clone();
        session.ask_attachment_confirmation(target.clone());
        assert!(session.toast.as_ref().unwrap().text.contains("y confirm"));
        assert!(
            session
                .on_key(KeyId::Char('n'), 10)
                .confirmed_attachment
                .is_none()
        );
        assert!(
            session
                .on_key(KeyId::Char('y'), 10)
                .confirmed_attachment
                .is_none()
        );

        session.ask_attachment_confirmation(target.clone());
        session.clear_toast(); // The window clears notices before dispatching a key.
        assert_eq!(
            session.on_key(KeyId::Char('y'), 10).confirmed_attachment,
            Some(target.clone()),
        );
        assert!(
            session
                .on_key(KeyId::Char('y'), 10)
                .confirmed_attachment
                .is_none()
        );
        session.ask_attachment_confirmation(target.clone());
        let mut updated = session.messages.clone();
        updated[0].attachments[0].filename = Some("different.pdf".into());
        session.refresh_same_messages(updated);
        assert!(
            session
                .on_key(KeyId::Char('y'), 10)
                .confirmed_attachment
                .is_none()
        );
    }

    #[test]
    fn gi_gs_gd_and_leader_chords_switch_folders() {
        let mut session = session();
        session.folder = "Sent".into();
        let step = session.on_key(KeyId::Char('g'), 4);
        assert!(step.arm_which);
        assert!(!step.reload_threads);
        let step = session.on_key(KeyId::Char('i'), 4);
        assert!(step.reload_threads);
        assert_eq!(session.folder, "INBOX");

        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('s'), 4);
        assert_eq!(session.folder, "Sent");
        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('d'), 4);
        assert_eq!(session.folder, "Drafts");

        session.on_key(KeyId::Space, 4);
        session.on_key(KeyId::Char('f'), 4);
        let step = session.on_key(KeyId::Char('i'), 4);
        assert!(step.reload_threads);
        assert_eq!(session.folder, "INBOX");
        assert!(session.chord.is_empty());
    }

    #[test]
    fn gs_and_gd_open_the_folders_the_server_marked() {
        let mut session = session();
        let folder = |role, name: &str| fell_core::RoleFolder {
            role,
            name: name.into(),
            raw: name.into(),
            source: fell_core::RoleSource::Server,
        };
        session.roles = FolderRoles::new(vec![
            folder(FolderRole::Inbox, "INBOX"),
            folder(FolderRole::Sent, "Sent Items"),
            folder(FolderRole::Drafts, "[Gmail]/Drafts"),
            folder(FolderRole::Junk, "Junk"),
        ]);
        session.folders = vec!["INBOX".into(), "Sent Items".into(), "Sent".into()];
        session.on_key(KeyId::Char('g'), 4);
        assert!(session.on_key(KeyId::Char('s'), 4).reload_threads);
        assert_eq!(session.folder, "Sent Items");
        assert_eq!(session.folder_title(), "Sent");
        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('d'), 4);
        assert_eq!(session.folder, "[Gmail]/Drafts");
        assert_eq!(session.folder_title(), "Drafts");
        assert!(session.in_drafts());

        // The picker leads with the role folders under their labels, and a
        // plain folder that only shares a role's usual name keeps its own.
        session.folder_query.clear();
        let choices = session.folder_choices();
        assert_eq!(
            choices,
            vec![
                "INBOX",
                "Sent Items",
                "[Gmail]/Drafts",
                "Archive",
                "Junk",
                "Trash",
                "Sent"
            ]
        );
        session.folder_query = "spam".into();
        assert_eq!(session.folder_choices(), vec!["Junk"]);
    }

    #[test]
    fn a_label_named_sent_does_not_read_like_the_sent_folder() {
        let mut session = session();
        let folder = |role, name: &str| fell_core::RoleFolder {
            role,
            name: name.into(),
            raw: name.into(),
            source: fell_core::RoleSource::Server,
        };
        session.roles = FolderRoles::new(vec![
            folder(FolderRole::Inbox, "INBOX"),
            folder(FolderRole::Sent, "[Gmail]/Sent Mail"),
            folder(FolderRole::Junk, "[Gmail]/Spam"),
        ]);
        session.folders = vec![
            "INBOX".into(),
            "[Gmail]/Sent Mail".into(),
            "[Gmail]/Spam".into(),
            "Sent".into(),
        ];
        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('f'), 4);
        let choices = session.folder_choices();
        let sent: Vec<(String, Option<String>)> = choices
            .iter()
            .filter(|name| session.roles.label(name) == "Sent")
            .map(|name| (session.roles.label(name), session.folder_detail(name)))
            .collect();
        assert_eq!(
            sent,
            vec![
                ("Sent".to_string(), Some("[Gmail]/Sent Mail".to_string())),
                ("Sent".to_string(), None),
            ]
        );
        // Spam has no twin, so it stays a plain label.
        assert_eq!(session.folder_detail("[Gmail]/Spam"), None);

        // Filtering by the server's name finds the role folder.
        session.folder_query = "sent mail".into();
        assert_eq!(session.folder_choices(), vec!["[Gmail]/Sent Mail"]);

        session.folder_query.clear();
        session.on_key(KeyId::Escape, 4);
        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('s'), 4);
        assert_eq!(session.folder, "[Gmail]/Sent Mail");
        assert_eq!(session.folder_title(), "Sent");
        assert_eq!(
            session.folder_detail(&session.folder).as_deref(),
            Some("[Gmail]/Sent Mail")
        );
        assert_eq!(
            session.folder_text("[Gmail]/Sent Mail"),
            "Sent ([Gmail]/Sent Mail)"
        );
        assert_eq!(session.folder_text("Sent"), "Sent");
    }

    #[test]
    fn e_asks_to_archive_without_leaving_the_folder() {
        let mut session = session();
        let folder = session.folder.clone();
        let step = session.on_key(KeyId::Char('e'), 4);
        assert!(step.archive);
        assert!(!step.undo);
        assert!(!step.apply_move);
        assert_eq!(session.folder, folder);
        assert_eq!(session.cursor_id.as_deref(), Some("a"));
    }

    fn unread_rows(session: &Session) -> Vec<&str> {
        session
            .threads
            .iter()
            .filter(|row| row.unread)
            .map(|row| row.id.as_str())
            .collect()
    }

    #[test]
    fn opening_an_unread_thread_marks_it_read_in_the_same_frame() {
        let mut session = session();
        session.threads[0].unread = true;
        session.threads[1].unread = true;
        session.unread_count = 2;
        session.cursor_id = Some("a".into());
        let step = session.on_key(KeyId::Enter, 10);
        assert_eq!(session.pane, Pane::Conversation);
        assert_eq!(
            step.flags,
            Some(FlagEdit {
                ids: vec!["a".into()],
                change: FlagChange::Read,
            })
        );
        assert_eq!(unread_rows(&session), vec!["b"]);
        assert_eq!(session.unread_count, 1);
        assert!(session.toast.is_none(), "reading is not announced");
        // Enter again expands a message; nothing more to mark.
        assert_eq!(session.on_key(KeyId::Enter, 10).flags, None);
        // A read thread opens without a write.
        session.on_key(KeyId::Escape, 10);
        session.on_key(KeyId::Char('j'), 10);
        session.on_key(KeyId::Char('j'), 10);
        let step = session.on_key(KeyId::Char('o'), 10);
        assert_eq!(step.flags, None);
    }

    #[test]
    fn a_read_delay_marks_the_thread_only_if_it_stays_open() {
        let mut session = session();
        session.read_dwell_ms = 800;
        session.threads[0].unread = true;
        session.unread_count = 1;
        session.cursor_id = Some("a".into());
        let step = session.on_key(KeyId::Enter, 10);
        assert_eq!(step.flags, None);
        let first = step.read_after.unwrap();
        assert_eq!(first.id, "a");
        assert_eq!(unread_rows(&session), vec!["a"]);
        // Left before the delay ran out: still unread.
        session.on_key(KeyId::Escape, 10);
        assert_eq!(session.read_after_dwell(first.clone()), None);
        // Opened again: the first open's delay is stale even though the
        // same thread is open now, so only the second one marks it read.
        let second = session.on_key(KeyId::Enter, 10).read_after.unwrap();
        assert_ne!(first, second);
        assert_eq!(session.read_after_dwell(first), None);
        assert_eq!(unread_rows(&session), vec!["a"]);
        let edit = session.read_after_dwell(second).unwrap();
        assert_eq!(edit.change, FlagChange::Read);
        assert!(unread_rows(&session).is_empty());
        assert_eq!(session.unread_count, 0);
    }

    #[test]
    fn a_read_delay_is_cancelled_by_moving_or_changing_folder() {
        let mut session = session();
        session.read_dwell_ms = 800;
        session.threads[0].unread = true;
        session.cursor_id = Some("a".into());
        let read = session.on_key(KeyId::Enter, 10).read_after.unwrap();
        // The pointer picks another thread and then this one again.
        session.cancel_read_delay();
        assert_eq!(session.read_after_dwell(read), None);
        let read = {
            session.on_key(KeyId::Escape, 10);
            session.on_key(KeyId::Enter, 10).read_after.unwrap()
        };
        session.on_key(KeyId::Char('g'), 10);
        session.on_key(KeyId::Char('s'), 10);
        assert_eq!(session.read_after_dwell(read), None);
    }

    #[test]
    fn explicit_read_edits_cancel_the_dwell_of_an_open_conversation() {
        let mut session = session();
        session.read_dwell_ms = 800;
        session.threads[0].unread = true;
        session.cursor_id = Some("a".into());
        let read = session.on_key(KeyId::Enter, 10).read_after.unwrap();
        session.on_key(KeyId::Char('U'), 10);
        session.on_key(KeyId::Char('U'), 10);
        assert_eq!(session.read_after_dwell(read), None);
        assert!(session.threads[0].unread);

        session.on_key(KeyId::Escape, 10);
        let read = session.on_key(KeyId::Enter, 10).read_after.unwrap();
        assert!(session.on_key(KeyId::Char('u'), 10).undo);
        assert_eq!(session.read_after_dwell(read), None);
    }

    #[test]
    fn opening_from_the_palette_stamps_the_read_delay() {
        let mut session = session();
        session.read_dwell_ms = 800;
        session.threads[0].unread = true;
        session.cursor_id = Some("a".into());
        session.cancel_read_delay();
        let read = session.perform(Action::Open, 10).read_after.unwrap();
        assert_ne!(read.generation, 0);
        assert_eq!(
            session.read_after_dwell(read).unwrap().change,
            FlagChange::Read
        );
    }

    #[test]
    fn a_single_row_on_screen_changes_without_a_toast() {
        let mut session = session();
        session.cursor_id = Some("b".into());
        session.rows_in_view = Some(0..5);
        session.on_key(KeyId::Char('U'), 10);
        assert!(session.threads[1].unread);
        assert!(session.toast.is_none(), "the row shows it");
        session.on_key(KeyId::Char('s'), 10);
        assert!(session.toast.is_none());
        // Off screen: the row cannot show it, so the toast does.
        session.rows_in_view = Some(2..5);
        session.on_key(KeyId::Char('s'), 10);
        assert_eq!(session.toast.as_ref().unwrap().text, "Unstarred");
        // From the conversation pane the toast says it too.
        session.rows_in_view = Some(0..5);
        session.on_key(KeyId::Enter, 10);
        session.clear_toast();
        session.on_key(KeyId::Char('U'), 10);
        assert_eq!(session.toast.as_ref().unwrap().text, "Marked unread");
        // A selection says how many, even on screen.
        session.on_key(KeyId::Escape, 10);
        session.clear_toast();
        session.on_key(KeyId::Char('V'), 10);
        session.on_key(KeyId::Char('j'), 10);
        session.on_key(KeyId::Char('s'), 10);
        assert_eq!(session.toast.as_ref().unwrap().text, "2 starred");
    }

    #[test]
    fn a_range_toast_counts_only_the_rows_that_changed() {
        let mut session = session();
        session.threads[1].unread = true;
        session.threads[2].unread = true;
        session.unread_count = 2;
        session.rows_in_view = Some(0..5);
        session.cursor_id = Some("a".into());
        session.on_key(KeyId::Char('V'), 10);
        session.on_key(KeyId::Char('j'), 10);
        session.on_key(KeyId::Char('j'), 10);
        let edit = session.on_key(KeyId::Char('U'), 10).flags.unwrap();
        assert_eq!(edit.ids, vec!["a"]);
        assert_eq!(session.toast.as_ref().unwrap().text, "1 marked unread");
    }

    #[test]
    fn shift_u_toggles_unread_and_s_toggles_the_star() {
        let mut session = session();
        session.cursor_id = Some("b".into());
        let step = session.on_key(KeyId::Char('U'), 10);
        assert_eq!(
            step.flags,
            Some(FlagEdit {
                ids: vec!["b".into()],
                change: FlagChange::Unread,
            })
        );
        assert_eq!(unread_rows(&session), vec!["b"]);
        assert_eq!(session.unread_count, 1);
        assert_eq!(session.toast.as_ref().unwrap().text, "Marked unread");
        let step = session.on_key(KeyId::Char('U'), 10);
        assert_eq!(step.flags.unwrap().change, FlagChange::Read);
        assert!(unread_rows(&session).is_empty());

        let step = session.on_key(KeyId::Char('s'), 10);
        assert_eq!(step.flags.unwrap().change, FlagChange::Star);
        assert!(session.threads[1].starred);
        assert_eq!(session.toast.as_ref().unwrap().text, "Starred");
        let step = session.on_key(KeyId::Char('s'), 10);
        assert_eq!(step.flags.unwrap().change, FlagChange::Unstar);
        assert!(!session.threads[1].starred);
        // Both are in the palette with their keys.
        let palette = session.palette();
        for (label, key) in [("Mark unread or read", "U"), ("Star or unstar", "s")] {
            let item = palette.iter().find(|item| item.label == label).unwrap();
            assert_eq!(item.binding, key);
        }
    }

    #[test]
    fn visual_range_shift_u_marks_five_threads_unread() {
        let mut session = session();
        session.threads[2].unread = true;
        session.unread_count = 1;
        session.cursor_id = Some("a".into());
        session.on_key(KeyId::Char('V'), 10);
        for _ in 0..4 {
            session.on_key(KeyId::Char('j'), 10);
        }
        let step = session.on_key(KeyId::Char('U'), 10);
        let edit = step.flags.unwrap();
        assert_eq!(edit.change, FlagChange::Unread, "mixed rows all go unread");
        assert_eq!(edit.ids, vec!["a", "b", "d", "e"], "c already was");
        assert_eq!(unread_rows(&session).len(), 5);
        assert_eq!(session.unread_count, 5);
        assert_eq!(
            session.toast.as_ref().unwrap().text,
            "4 marked unread",
            "c already was"
        );
        assert!(!session.visual, "the range was the operand");
        // x picks work the same way, and a draft row is never a target.
        let mut draft = row("draft-1");
        draft.draft = Some("c1".into());
        let mut rows = vec![draft];
        rows.extend(session.threads.clone());
        session.set_threads(rows);
        session.picked.insert("draft-1".into());
        session.picked.insert("a".into());
        session.cursor_id = Some("e".into());
        let edit = session.on_key(KeyId::Char('s'), 10).flags.unwrap();
        assert_eq!(edit.ids, vec!["a"]);
        assert!(session.picked.is_empty());
    }

    #[test]
    fn flags_follow_marks_out_of_the_loaded_page_and_skip_missing_threads() {
        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let inbox = store.folder_threads(&account.id, "INBOX").unwrap();
        let marked = inbox
            .iter()
            .find(|row| !row.unread && !row.starred)
            .unwrap();
        let other = inbox
            .iter()
            .find(|row| row.thread_id != marked.thread_id)
            .unwrap();
        let rows = |row: &fell_core::store::FolderThread| {
            present::thread_rows(std::slice::from_ref(row), &account.email)
        };
        let mut session = session();
        session.set_page(rows(marked), false, true, 0);
        session.on_key(KeyId::Char('x'), 10);
        session.set_page(rows(other), true, false, 0);
        for (key, expected) in [
            ('U', FlagChange::Unread),
            ('U', FlagChange::Read),
            ('s', FlagChange::Star),
            ('s', FlagChange::Unstar),
        ] {
            let step = session.on_key(KeyId::Char(key), 10);
            assert!(step.flags.is_none());
            let (mut ids, star) = step.off_page_flags.unwrap();
            assert_eq!(ids, [marked.thread_id.as_str()]);
            ids.push("missing".into());
            let actual = store
                .toggle_selected_flags(
                    &account.id,
                    &ids,
                    fell_core::store::ListSource::Folder("INBOX"),
                    star,
                )
                .unwrap();
            assert_eq!(actual, (expected, 1));
            let refreshed = store.folder_threads(&account.id, "INBOX").unwrap();
            let unchanged = refreshed
                .iter()
                .find(|row| row.thread_id == other.thread_id)
                .unwrap();
            assert_eq!(
                (unchanged.unread, unchanged.starred),
                (other.unread, other.starred)
            );
        }
    }

    #[test]
    fn flag_keys_on_a_draft_row_say_why_nothing_happened() {
        let mut session = session();
        let mut draft = row("draft-1");
        draft.draft = Some("c1".into());
        session.set_threads(vec![draft]);
        session.cursor_id = Some("draft-1".into());
        assert_eq!(session.on_key(KeyId::Char('U'), 10).flags, None);
        assert_eq!(session.toast.as_ref().unwrap().text, DRAFT_NOT_THREAD);
    }

    /// The edits the model makes, written to a real store and undone there,
    /// come back to the rows the list rereads.
    #[test]
    fn open_shift_u_and_s_round_trip_through_the_store_and_undo() {
        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let reread = |session: &mut Session| {
            let page = store
                .folder_page(&account.id, "INBOX", &FolderSpan::Top { rows: 100 })
                .unwrap();
            let rows = crate::present::thread_rows(&page.threads, &account.email);
            session.set_page(rows, page.more_before, page.more_after, page.unread);
        };
        let write = |edit: FlagEdit| {
            let threads: Vec<ThreadId> = edit.ids.into_iter().map(ThreadId).collect();
            store
                .flag_threads(&account.id, &threads, "INBOX", edit.change)
                .unwrap()
                .unwrap();
        };
        let mut session = Session::new(
            "Me",
            &account.email,
            SyncKind::FixtureOffline,
            Keymap::default(),
        );
        reread(&mut session);
        let unread = session
            .threads
            .iter()
            .find(|row| row.unread)
            .unwrap()
            .id
            .clone();
        let before = session.unread_count;
        session.cursor_id = Some(unread.clone());

        // Open: read, on screen and in the store; undo: unread again.
        write(session.on_key(KeyId::Enter, 10).flags.unwrap());
        reread(&mut session);
        assert!(!session.cursor_thread().unwrap().unread);
        assert_eq!(session.unread_count, before - 1);
        store.undo(&account.id).unwrap();
        reread(&mut session);
        assert!(session.cursor_thread().unwrap().unread);
        assert_eq!(session.unread_count, before);

        // U on a read thread, then undo.
        session.on_key(KeyId::Escape, 10);
        let read = session
            .threads
            .iter()
            .find(|row| !row.unread)
            .unwrap()
            .id
            .clone();
        session.cursor_id = Some(read);
        write(session.on_key(KeyId::Char('U'), 10).flags.unwrap());
        reread(&mut session);
        assert!(session.cursor_thread().unwrap().unread);
        store.undo(&account.id).unwrap();
        reread(&mut session);
        assert!(!session.cursor_thread().unwrap().unread);

        // s, then undo.
        let was = session.cursor_thread().unwrap().starred;
        write(session.on_key(KeyId::Char('s'), 10).flags.unwrap());
        reread(&mut session);
        assert_eq!(session.cursor_thread().unwrap().starred, !was);
        store.undo(&account.id).unwrap();
        reread(&mut session);
        assert_eq!(session.cursor_thread().unwrap().starred, was);
    }

    #[test]
    fn mail_that_arrives_after_reading_keeps_the_open_messages_open() {
        let mut session = session();
        let first = empty_message(true);
        let second = empty_message(true);
        session.set_messages(vec![first.clone(), second.clone()]);
        // Reread after the thread was marked read: the store now says both
        // are read, so they would come back closed, plus a new message.
        let mut reread_first = first.clone();
        reread_first.expanded = false;
        let mut reread_second = second.clone();
        reread_second.expanded = false;
        let newcomer = empty_message(true);
        assert!(session.refresh_messages(vec![reread_first, reread_second, newcomer], true));
        assert!(session.messages.iter().all(|message| message.expanded));
        assert_eq!(session.message_cursor, 2);
    }

    #[test]
    fn first_and_last_message_commands_reveal_an_already_selected_header() {
        let mut session = session();
        session.set_messages(vec![empty_message(true), empty_message(true)]);
        session.pane = Pane::Conversation;
        assert_eq!(session.message_cursor, 1);
        let last = session.on_key(KeyId::Char('G'), 10);
        assert!(last.jump_to_message);
        assert_eq!(session.message_cursor, 1);

        for _ in 0..2 {
            assert!(!session.on_key(KeyId::Char('g'), 10).jump_to_message);
            assert!(session.on_key(KeyId::Char('g'), 10).jump_to_message);
            assert_eq!(session.message_cursor, 0);
        }
        assert!(!session.on_key(KeyId::Char('k'), 10).jump_to_message);
    }

    #[test]
    fn hash_deletes_and_bang_marks_spam_on_the_selected_row() {
        let mut session = session();
        session.cursor_id = Some("b".into());
        let step = session.on_key(KeyId::Char('#'), 4);
        assert!(step.delete && !step.purge && !step.spam);
        assert_eq!(session.action_targets(), vec!["b".to_string()]);
        assert!(session.confirm_purge.is_none());
        let step = session.on_key(KeyId::Char('!'), 4);
        assert!(step.spam && !step.delete);
        // In Spam, `!` is "not spam": the same step, the view picks the inbox.
        session.folder = "Spam".into();
        assert!(session.on_key(KeyId::Char('!'), 4).spam);
    }

    #[test]
    fn delete_and_spam_take_the_picked_rows_or_the_visual_range() {
        let mut picked = session();
        picked.cursor_id = Some("a".into());
        picked.on_key(KeyId::Char('x'), 4);
        picked.cursor_id = Some("c".into());
        picked.on_key(KeyId::Char('x'), 4);
        picked.cursor_id = Some("e".into());
        assert_eq!(picked.action_targets(), vec!["a", "c"]);
        assert!(picked.on_key(KeyId::Char('#'), 4).delete);

        let mut session = session();
        session.picked.insert("a".into());
        session.cursor_id = Some("b".into());
        session.on_key(KeyId::Char('V'), 4);
        session.on_key(KeyId::Char('j'), 4);
        session.on_key(KeyId::Char('j'), 4);
        assert_eq!(session.action_targets(), vec!["b", "c", "d"]);
        assert!(session.on_key(KeyId::Char('!'), 4).spam);

        // The rows leave together; the row after them is selected.
        for id in session.action_targets() {
            session.remove_thread(&id);
        }
        assert_eq!(session.cursor_id.as_deref(), Some("e"));
        assert!(!session.visual);
    }

    #[test]
    fn mixed_triage_selections_take_the_threads_and_leave_local_drafts() {
        for (key, flags) in [
            ('e', (true, false, false)),
            ('#', (false, true, false)),
            ('!', (false, false, true)),
        ] {
            for visual in [false, true] {
                let mut session = session();
                session.folder = "Drafts".into();
                let mut draft = row("draft-1");
                draft.draft = Some("c1".into());
                session.set_threads(vec![row("a"), draft, row("b")]);
                session.select_if_present("a");
                if visual {
                    for key in ['V', 'j', 'j'] {
                        session.on_key(KeyId::Char(key), 4);
                    }
                } else {
                    for id in ["a", "draft-1", "b"] {
                        session.select_if_present(id);
                        session.on_key(KeyId::Char('x'), 4);
                    }
                }

                let step = session.on_key(KeyId::Char(key), 4);
                assert_eq!((step.archive, step.delete, step.spam), flags);
                assert!(step.discard.is_empty());
                assert!(session.toast.is_none());
                let (targets, drafts) = session.split_removal_targets(session.action_targets());
                assert_eq!(targets, ["a", "b"]);
                assert_eq!(drafts, ["c1"]);
            }
        }
    }

    #[test]
    fn delete_and_spam_explain_a_draft_row_and_an_empty_list() {
        let mut session = session();
        let mut draft = row("draft-1");
        draft.draft = Some("c1".into());
        session.set_threads(vec![draft, row("a")]);
        session.cursor_id = Some("draft-1".into());
        for key in ['#', '!'] {
            let step = session.on_key(KeyId::Char(key), 4);
            assert!(!step.delete && !step.spam);
            assert_eq!(
                session.toast.as_ref().map(|toast| toast.text.as_str()),
                Some(DRAFT_NOT_THREAD)
            );
        }
        session.set_threads(Vec::new());
        session.cursor_id = None;
        assert!(!session.on_key(KeyId::Char('#'), 4).delete);
        assert_eq!(
            session.toast.as_ref().map(|toast| toast.text.as_str()),
            Some("No conversation")
        );
    }

    #[test]
    fn hash_in_trash_asks_once_and_y_confirms() {
        let mut session = session();
        session.folder = "Trash".into();
        session.cursor_id = Some("a".into());
        session.on_key(KeyId::Char('x'), 4);
        session.cursor_id = Some("b".into());
        let step = session.on_key(KeyId::Char('#'), 4);
        assert!(!step.delete && !step.purge, "nothing happens before y");
        assert_eq!(
            session.toast.as_ref().map(|toast| toast.text.as_str()),
            Some("Remove from Fell only?  ·  “a”  ·  No undo  ·  y")
        );
        assert!(session.toast.as_ref().unwrap().alert);
        assert_eq!(session.highlighted(), HashSet::from(["a".to_string()]));
        // The view clears the notice before every key, as it does here.
        session.clear_toast();
        let step = session.on_key(KeyId::Char('y'), 4);
        assert!(step.purge && !step.delete);
        assert_eq!(session.take_purge(), Some(vec!["a".into()]));
        assert_eq!(session.take_purge(), None);
        // `y` again answers nothing.
        assert!(!session.on_key(KeyId::Char('y'), 4).purge);

        // Another picked row changes the count. An unpicked cursor does not.
        session.picked.insert("c".into());
        session.on_key(KeyId::Char('#'), 4);
        assert_eq!(
            session.toast.as_ref().map(|toast| toast.text.as_str()),
            Some("Remove from Fell only?  ·  2 marked  ·  No undo  ·  y")
        );
    }

    #[test]
    fn replacing_the_confirmed_page_or_expiring_the_prompt_prevents_purge() {
        let mut session = session();
        session.folder = "Trash".into();
        session.picked.extend(["a".to_string(), "b".to_string()]);
        session.on_key(KeyId::Char('#'), 4);
        session.set_threads(vec![row("b"), row("c")]);
        assert!(session.toast.is_none());
        assert!(!session.on_key(KeyId::Char('y'), 4).purge);

        session.on_key(KeyId::Char('#'), 4);
        assert!(session.clear_toast_if(session.toast_generation()));
        assert!(!session.on_key(KeyId::Char('y'), 4).purge);
    }

    #[test]
    fn any_other_answer_lets_the_permanent_delete_go() {
        let mut session = session();
        session.folder = "Trash".into();
        session.cursor_id = Some("a".into());

        // Escape and n only withdraw it.
        for key in [KeyId::Escape, KeyId::Char('n')] {
            session.on_key(KeyId::Char('#'), 4);
            session.clear_toast();
            let step = session.on_key(key, 4);
            assert_eq!(step, Step::default());
            assert!(!session.on_key(KeyId::Char('y'), 4).purge);
        }

        // Another key withdraws it and still does what it does.
        session.on_key(KeyId::Char('#'), 4);
        session.clear_toast();
        session.on_key(KeyId::Char('j'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("b"));
        assert!(!session.on_key(KeyId::Char('y'), 4).purge);

        // A notice that replaced the question withdraws it.
        session.on_key(KeyId::Char('#'), 4);
        session.show_toast("Synced", false);
        session.clear_toast();
        assert!(!session.on_key(KeyId::Char('y'), 4).purge);

        // So does a click, and the question leaves with it.
        session.on_key(KeyId::Char('#'), 4);
        session.cancel_pending_keys();
        assert!(session.toast.is_none());
        assert!(!session.on_key(KeyId::Char('y'), 4).purge);
    }

    #[test]
    fn palette_enter_reports_the_action_it_runs() {
        let mut session = session();
        session.cursor_id = Some("a".into());
        session.on_key(KeyId::Char(':'), 4);
        for ch in "archive".chars() {
            session.on_key(KeyId::Char(ch), 4);
        }
        let step = session.on_key(KeyId::Enter, 4);
        assert_eq!(step.action, Some(Action::Archive));
        assert!(step.archive);
    }

    #[test]
    fn the_palette_runs_delete_and_spam() {
        let mut session = session();
        session.cursor_id = Some("a".into());
        session.on_key(KeyId::Char(':'), 4);
        for ch in "spam".chars() {
            session.on_key(KeyId::Char(ch), 4);
        }
        assert!(session.on_key(KeyId::Enter, 4).spam);
        session.on_key(KeyId::Char(':'), 4);
        for ch in "delete".chars() {
            session.on_key(KeyId::Char(ch), 4);
        }
        assert!(session.on_key(KeyId::Enter, 4).delete);
    }

    #[test]
    fn undo_selects_the_thread_that_came_back() {
        let mut session = session();
        session.cursor_id = Some("b".into());
        assert!(session.select_if_present("a"));
        assert_eq!(session.cursor_id.as_deref(), Some("a"));
        assert!(!session.select_if_present("missing"));
        assert_eq!(session.cursor_id.as_deref(), Some("a"));
    }

    #[test]
    fn m_opens_the_move_picker_and_paging_is_ctrl() {
        let mut session = session();
        let before = session.folder.clone();
        let step = session.on_key(KeyId::Char('m'), 4);
        assert!(!step.apply_move);
        assert!(!step.undo);
        assert_eq!(session.overlay, Overlay::Folders);
        assert_eq!(session.folder_intent, FolderIntent::Move);
        assert_eq!(session.folder_prompt(), "Move to");
        assert!(
            session
                .folder_choices()
                .iter()
                .all(|name| !name.eq_ignore_ascii_case("INBOX"))
        );
        let step = session.on_key(KeyId::Enter, 4);
        assert!(step.apply_move);
        assert_eq!(session.pending_move.as_deref(), Some("Sent"));
        assert_eq!(session.folder, before);
        assert_eq!(session.overlay, Overlay::None);
        let step = session.on_key(KeyId::Char('u'), 4);
        assert!(step.undo);
        assert!(session.chord.is_empty());
        session.pane = Pane::Conversation;
        session.messages = vec![
            empty_message(false),
            empty_message(false),
            empty_message(true),
        ];
        session.message_cursor = 0;
        let step = session.on_key(KeyId::Ctrl('d'), 2);
        assert_eq!(step.scroll_pages, 1);
        assert_eq!(step.scroll_lines, 0);
        assert_eq!(session.message_cursor, 0);
        let step = session.on_key(KeyId::Ctrl('u'), 2);
        assert_eq!(step.scroll_pages, -1);
        assert_eq!(session.message_cursor, 0);
        let step = session.on_key(KeyId::Down, 2);
        assert_eq!(step.scroll_lines, 1);
        let step = session.on_key(KeyId::Up, 2);
        assert_eq!(step.scroll_lines, -1);
        assert_eq!(session.message_cursor, 0);
        session.on_key(KeyId::Space, 2);
        assert_eq!(session.message_cursor, 0);
        assert_eq!(session.chord, vec![KeyId::Leader]);
    }

    #[test]
    fn palette_opens_on_colon_and_ctrl_shift_p() {
        let mut session = session();
        session.on_key(KeyId::Char(':'), 4);
        assert_eq!(session.overlay, Overlay::Palette);
        let items = session.palette();
        assert!(items.iter().any(|item| item.action == Action::Inbox));
        assert!(
            items
                .iter()
                .all(|item| item.action != Action::Inbox || item.binding.contains("g i"))
        );
        session.overlay = Overlay::None;
        session.on_key(KeyId::CtrlShiftP, 4);
        assert_eq!(session.overlay, Overlay::Palette);
    }

    #[test]
    fn about_matches_version_and_returns_in_recent_actions() {
        let mut session = session();
        session.on_key(KeyId::Char(':'), 4);
        for ch in "version".chars() {
            session.on_key(KeyId::Char(ch), 4);
        }
        assert_eq!(session.palette()[0].action, Action::About);
        session.on_key(KeyId::Enter, 4);
        assert_eq!(session.overlay, Overlay::About);
        session.on_key(KeyId::Escape, 4);
        assert_eq!(session.overlay, Overlay::None);
        session.on_key(KeyId::Char(':'), 4);
        assert_eq!(session.palette()[0].action, Action::About);
    }

    #[test]
    fn count_and_visual_range_and_stable_selection() {
        let mut session = session();
        session.on_key(KeyId::Char('4'), 4);
        session.on_key(KeyId::Char('j'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("e"));
        session.on_key(KeyId::Char('V'), 4);
        session.on_key(KeyId::Char('k'), 4);
        session.on_key(KeyId::Char('k'), 4);
        let highlighted = session.highlighted();
        assert!(highlighted.contains("c"));
        assert!(highlighted.contains("d"));
        assert!(highlighted.contains("e"));
        let changed = session.set_threads(vec![row("z"), row("c"), row("e")]);
        assert!(!changed);
        assert_eq!(session.cursor_id.as_deref(), Some("c"));
    }

    #[test]
    fn configurable_leader_and_which_key_delay_zero() {
        let keymap = keymap_from_text("leader = \",\"\nwhich_key_ms = 0\n").unwrap();
        let mut session = Session::new("Caleb", "me@x", SyncKind::FixtureOffline, keymap);
        session.set_threads(vec![row("a")]);
        let step = session.on_key(KeyId::Char(','), 4);
        assert!(step.arm_which);
        assert!(session.which_visible);
        assert_eq!(session.prefix_text(), ",");
        session.on_key(KeyId::Backspace, 4);
        assert!(session.chord.is_empty());
        assert!(!session.which_visible);
    }

    #[test]
    fn empty_visual_range_does_not_panic() {
        let mut session = session();
        session.on_key(KeyId::Char('V'), 1);
        assert!(session.visual);
        session.set_threads(vec![]);
        assert!(!session.visual);
        assert!(session.anchor_id.is_none());
        let _ = session.highlighted();
    }

    #[test]
    fn missing_anchor_drops_visual_mode() {
        let mut session = session();
        session.on_key(KeyId::Char('G'), 1);
        session.on_key(KeyId::Char('V'), 1);
        session.on_key(KeyId::Char('k'), 1);
        assert!(session.visual);
        let changed = session.set_threads(vec![row("a"), row("b")]);
        assert!(changed);
        assert!(!session.visual);
        let highlighted = session.highlighted();
        assert!(!highlighted.contains("a") || session.cursor_id.as_deref() == Some("a"));
        assert_eq!(highlighted.len(), usize::from(session.cursor_id.is_some()));
    }

    #[test]
    fn visual_mode_ends_when_the_cursor_leaves_the_folder() {
        let mut session = session();
        session.visual = true;
        session.anchor_id = Some("c".into());
        session.cursor_id = Some("e".into());
        session.set_threads(vec![row("a"), row("c"), row("d")]);
        assert!(!session.visual);
        assert!(session.anchor_id.is_none());
        let highlighted = session.highlighted();
        assert_eq!(highlighted.len(), 1);
    }

    #[test]
    fn visual_mode_ends_when_a_new_row_appears_inside_the_range() {
        let mut session = session();
        session.visual = true;
        session.anchor_id = Some("a".into());
        session.cursor_id = Some("c".into());
        session.set_threads(vec![row("a"), row("new"), row("c")]);
        assert!(!session.visual);
        assert!(session.anchor_id.is_none());
        assert!(!session.highlighted().contains("new"));
    }

    #[test]
    fn huge_count_clamps_instead_of_overflowing() {
        let mut session = session();
        session.on_key(KeyId::Char('G'), 1);
        for ch in "2147483648".chars() {
            session.on_key(KeyId::Char(ch), 1);
        }
        session.on_key(KeyId::Char('k'), 1);
        assert_eq!(session.cursor_id.as_deref(), Some("a"));

        for ch in "2147483647".chars() {
            session.on_key(KeyId::Char(ch), 1);
        }
        session.on_key(KeyId::Char('j'), 1);
        assert_eq!(session.cursor_id.as_deref(), Some("e"));

        session.pane = Pane::Conversation;
        session.messages = vec![empty_message(false), empty_message(false)];
        session.message_cursor = 0;
        for ch in "2147483648".chars() {
            session.on_key(KeyId::Char(ch), 1);
        }
        let step = session.on_key(KeyId::Ctrl('d'), 4);
        assert_eq!(step.scroll_pages, 10_000);
        assert_eq!(session.message_cursor, 0);
        for ch in "2147483648".chars() {
            session.on_key(KeyId::Char(ch), 1);
        }
        let step = session.on_key(KeyId::Ctrl('u'), 4);
        assert_eq!(step.scroll_pages, -10_000);
        assert_eq!(session.message_cursor, 0);
    }

    #[test]
    fn empty_folder_and_first_sync_copy() {
        let mut session = session();
        session.threads.clear();
        session.folder = "Archive".into();
        let banner = session
            .list_banner(&session.folder, session.label_scope.as_deref())
            .unwrap();
        assert!(banner.title.contains("Archive"));
        assert!(banner.hint.is_empty());
        // A navigation key can change the requested scope before its page lands.
        assert_eq!(
            session
                .list_banner("INBOX", Some("field notes"))
                .unwrap()
                .title,
            "No conversations in field notes"
        );

        // An empty drafts list says how to start one.
        session.folder = "Drafts".into();
        let banner = session
            .list_banner(&session.folder, session.label_scope.as_deref())
            .unwrap();
        assert_eq!(banner.title, "No drafts");
        assert!(banner.hint.starts_with("c compose"), "{}", banner.hint);
        assert!(banner.hint.contains("gi"));
        session.sync = SyncKind::FirstSync;
        let banner = session
            .list_banner(&session.folder, session.label_scope.as_deref())
            .unwrap();
        assert!(banner.hint.contains("fell account setup"));
    }

    #[test]
    fn removing_the_selected_thread_selects_the_row_in_its_place() {
        let mut session = session();
        let ids: Vec<String> = session.threads.iter().map(|row| row.id.clone()).collect();
        assert!(ids.len() >= 3, "fixture needs three threads");
        session.cursor_id = Some(ids[1].clone());
        session.remove_thread(&ids[1]);
        assert_eq!(session.cursor_id.as_deref(), Some(ids[2].as_str()));
        let last = session.threads.last().unwrap().id.clone();
        let before_last = session.threads[session.threads.len() - 2].id.clone();
        session.cursor_id = Some(last.clone());
        session.remove_thread(&last);
        assert_eq!(session.cursor_id.as_deref(), Some(before_last.as_str()));
    }

    #[test]
    fn removing_the_cursor_end_of_a_visual_range_ends_it() {
        let mut session = session();
        let ids: Vec<String> = session.threads.iter().map(|row| row.id.clone()).collect();
        session.cursor_id = Some(ids[0].clone());
        session.on_key(KeyId::Char('V'), 4);
        session.on_key(KeyId::Char('j'), 4);
        assert!(session.visual);
        session.remove_thread(&ids[1]);
        assert!(!session.visual);
        assert_eq!(session.highlighted().len(), 1);
    }

    #[test]
    fn missing_selection_stays_at_its_place_in_the_list() {
        let mut session = session();
        let rows = session.threads.clone();
        assert!(rows.len() >= 3, "fixture needs three threads");
        session.cursor_id = Some(rows[1].id.clone());
        let mut without = rows.clone();
        without.remove(1);
        session.set_threads(without);
        assert_eq!(session.cursor_id.as_deref(), Some(rows[2].id.as_str()));
    }

    #[test]
    fn attachment_error_details_survive_opening_help() {
        let mut session = session();
        let cause = "Couldn't open attachment\nIMAP server refused part 2";
        session.attachment_error = Some(cause.into());
        session.show_toast("Couldn't open attachment · space a R retry", true);

        session.on_key(KeyId::Char('?'), 10);
        assert_eq!(session.overlay, Overlay::Help);
        assert_eq!(session.detail_text().as_deref(), Some(cause));
        session.on_key(KeyId::Escape, 10);
        assert_eq!(session.overlay, Overlay::None);
        assert_eq!(session.detail_text().as_deref(), Some(cause));
    }

    #[test]
    fn failed_read_is_not_an_empty_folder() {
        let mut session = session();
        session.threads.clear();
        session.sync = SyncKind::LocalOffline;
        session.fail_query(QueryFault::Folder, "database is locked".into());
        let banner = session
            .list_banner(&session.folder, session.label_scope.as_deref())
            .unwrap();
        assert_eq!(banner.title, "database is locked");
        assert!(!banner.title.contains("No conversations"));
    }

    #[test]
    fn startup_error_survives_a_folder_read() {
        let mut session = session();
        session.record_startup_error("keymap: which_key_ms = nope".into());
        session.fail_query(QueryFault::Folder, "database is locked".into());
        session.clear_query_fault(QueryFault::Message);
        assert_eq!(session.error.as_deref(), Some("database is locked"));
        session.clear_query_fault(QueryFault::Folder);
        assert!(session.error.is_none());
        assert_eq!(
            session.detail_text().as_deref(),
            Some("keymap: which_key_ms = nope")
        );
    }

    #[test]
    fn empty_folder_query_leaves_the_current_folder() {
        let mut session = session();
        session.folder = "Sent".into();
        session.overlay = Overlay::Folders;
        session.folder_query = "zzzz-no-such".into();
        let step = session.on_key(KeyId::Enter, 4);
        assert!(!step.reload_threads);
        assert_eq!(session.folder, "Sent");
        assert_eq!(session.overlay, Overlay::None);
    }

    #[test]
    fn click_cancels_a_folder_prefix() {
        let mut session = session();
        session.folder = "Sent".into();
        session.on_key(KeyId::Char('g'), 4);
        session.count = "3".into();
        session.cancel_pending_keys();
        assert!(session.chord.is_empty());
        assert!(session.count.is_empty());
        assert!(!session.which_visible);
        let step = session.on_key(KeyId::Char('i'), 4);
        assert!(!step.reload_threads);
        assert_eq!(session.folder, "Sent");
    }

    #[test]
    fn a_reread_at_the_bottom_lands_only_when_a_new_last_message_arrives() {
        let mut session = session();
        let first = empty_message(true);
        let last = empty_message(true);
        session.set_messages(vec![first.clone(), last.clone()]);
        session.toggle_open(1);
        session.messages[0].expanded = false;

        // An account-wide sync reread must not drive a landing or reset
        // disclosures while the reader is at the bottom of the same message.
        assert!(!session.refresh_messages(vec![first.clone(), last.clone()], true));
        assert_eq!(session.message_cursor, 1);
        assert!(session.is_open(&session.messages[1].id));
        assert!(session.recipients_are_open(&session.messages[1].id));
        assert!(!session.messages[0].expanded);

        let reply = empty_message(true);
        assert!(session.refresh_messages(vec![first.clone(), last, reply.clone()], true));
        assert_eq!(session.message_cursor, 2);
        assert_eq!(session.messages[2].id, reply.id);
        assert!(!session.refresh_messages(vec![first, reply.clone()], true));

        // The previous message is visible at the bottom of the list but
        // the reader scrolled up, so another reply does not drive a landing.
        let newer = empty_message(true);
        assert!(!session.refresh_messages(vec![reply, newer], false));
    }

    #[test]
    fn open_parts_stay_with_their_message_when_a_reread_shifts_rows() {
        let mut session =
            Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        let first = empty_message(true);
        let second = empty_message(true);
        session.set_messages(vec![first.clone(), second.clone()]);
        session.message_cursor = 0;
        session.toggle_open(0);
        assert!(session.is_open(&session.messages[0].id));
        // Sync puts a new message before the one that was opened.
        let newcomer = empty_message(true);
        session.refresh_messages(vec![newcomer, first.clone(), second], false);
        assert!(!session.is_open(&session.messages[0].id));
        assert!(session.is_open(&session.messages[1].id));
        // The opened message leaves the thread: nothing else opens in its place.
        session.refresh_messages(vec![empty_message(true)], false);
        assert!(!session.is_open(&session.messages[0].id));
    }

    #[test]
    fn a_click_on_recipients_opens_only_them_and_q_opens_everything() {
        let mut session =
            Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        session.set_messages(vec![empty_message(true)]);
        session.toggle_recipients(0);
        assert!(session.recipients_are_open(&session.messages[0].id));
        assert!(
            !session.is_open(&session.messages[0].id),
            "quoted text stays closed"
        );
        session.toggle_recipients(0);
        session.toggle_open(0);
        assert!(
            session.recipients_are_open(&session.messages[0].id)
                && session.is_open(&session.messages[0].id)
        );
        session.toggle_open(0);
        assert!(
            !session.recipients_are_open(&session.messages[0].id)
                && !session.is_open(&session.messages[0].id)
        );
    }

    #[test]
    fn space_a_r_asks_to_reopen_the_agent_review() {
        let mut session =
            Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        session.on_key(KeyId::Space, 4);
        session.on_key(KeyId::Char('a'), 4);
        let step = session.on_key(KeyId::Char('r'), 4);
        assert!(step.reopen_review);
        // `space s` reaches the view too.
        session.on_key(KeyId::Space, 4);
        assert!(session.on_key(KeyId::Char('s'), 4).sync_now);
    }

    /// A folder the way the store holds it: every row, in list order.
    struct Folder(Vec<ThreadRow>);

    fn dated(id: &str, date_unix: i64) -> ThreadRow {
        ThreadRow {
            date_unix,
            ..row(id)
        }
    }

    impl Folder {
        /// Rows `t00`, `t01`, … from newest to oldest, a minute apart.
        fn new(rows: usize) -> Self {
            Self(
                (0..rows)
                    .map(|index| dated(&format!("t{index:02}"), 1_000_000 - index as i64 * 60))
                    .collect(),
            )
        }

        fn insert(&mut self, row: ThreadRow) {
            self.0.push(row);
            self.0.sort_by_key(row_key);
        }

        fn remove(&mut self, id: &str) {
            self.0.retain(|row| row.id != id);
        }

        /// What `Store::folder_page` returns for `span`.
        fn read(&self, span: &FolderSpan) -> (Vec<ThreadRow>, bool, bool) {
            let keys: Vec<ListKey> = self.0.iter().map(row_key).collect();
            let len = keys.len();
            let (start, end) = match span {
                FolderSpan::Top { rows } => (0, (*rows).min(len)),
                FolderSpan::Bottom { rows } => (len.saturating_sub(*rows), len),
                FolderSpan::Window {
                    first,
                    last,
                    before,
                    after,
                } => {
                    let low = first
                        .as_ref()
                        .map_or(0, |first| keys.iter().filter(|key| *key < first).count());
                    let high = keys.iter().filter(|key| *key <= last).count();
                    (
                        low.saturating_sub(*before),
                        high.saturating_add(*after).min(len).max(low),
                    )
                }
            };
            (self.0[start..end].to_vec(), start > 0, end < len)
        }
    }

    fn paged(rows: usize, load: usize, margin: usize) -> (Session, Folder) {
        let folder = Folder::new(rows);
        let mut session =
            Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        session.load_rows = load;
        session.margin = margin;
        session.keep_rows = 1_000;
        let (rows, before, after) = folder.read(&FolderSpan::Top { rows: load });
        session.set_page(rows, before, after, 0);
        (session, folder)
    }

    /// Answer the reads the list asks for, the way the view does, until it
    /// asks for no more. Returns how many reads ran.
    fn land(session: &mut Session, folder: &Folder) -> usize {
        let mut reads = 0;
        while let Some(fetch) = session.take_fetch() {
            reads += 1;
            assert!(reads < 20, "the list keeps asking for rows");
            refresh_with(session, folder, fetch, None);
        }
        reads
    }

    fn refresh_with(
        session: &mut Session,
        folder: &Folder,
        fetch: Fetch,
        select: Option<&ListKey>,
    ) {
        let span = session.span(fetch, select, None);
        let (rows, before, after) = folder.read(&span);
        session.set_page(rows, before, after, 0);
    }

    fn ids(session: &Session) -> Vec<String> {
        session.threads.iter().map(|row| row.id.clone()).collect()
    }

    /// What the view shows for `span` in Drafts: the compose drafts above
    /// the rows read, when those reach the top of the folder.
    fn read_with_drafts(
        session: &mut Session,
        folder: &Folder,
        drafts: &[ThreadRow],
        span: FolderSpan,
    ) {
        let (rows, before, after) = folder.read(&span);
        let mut shown = if before { Vec::new() } else { drafts.to_vec() };
        shown.extend(rows);
        session.set_page(shown, before, after, 0);
    }

    fn land_with_drafts(session: &mut Session, folder: &Folder, drafts: &[ThreadRow]) {
        let mut reads = 0;
        while let Some(fetch) = session.take_fetch() {
            reads += 1;
            assert!(reads < 20, "the list keeps asking for rows");
            let span = session.span(fetch, None, None);
            read_with_drafts(session, folder, drafts, span);
        }
    }

    #[test]
    fn drafts_above_the_folder_do_not_move_its_window() {
        let (mut session, folder) = paged(30, 10, 2);
        // One draft older than every thread, one newer: neither sorts with
        // the folder, and paging must not read from either.
        let drafts: Vec<ThreadRow> = [("draft-old", 1), ("draft-new", 2_000_000)]
            .into_iter()
            .map(|(id, date)| ThreadRow {
                draft: Some(id.into()),
                ..dated(id, date)
            })
            .collect();
        read_with_drafts(&mut session, &folder, &drafts, FolderSpan::Top { rows: 10 });
        assert_eq!(&ids(&session)[..3], ["draft-old", "draft-new", "t00"]);
        session.cursor_id = Some("draft-old".into());
        let FolderSpan::Window { first, last, .. } = session.span(Fetch::default(), None, None)
        else {
            panic!("a refresh rereads the loaded rows");
        };
        assert_eq!(first, None, "from the folder's top");
        assert_eq!(last.thread_id.0, "t09");
        // Keeping few rows past the reader puts the window's end at the
        // cursor. On a draft that is the folder's first row, not the draft,
        // whose date would end the read above every thread.
        session.keep_rows = 0;
        session.cursor_id = Some("draft-new".into());
        let span = session.span(Fetch::default(), None, None);
        let FolderSpan::Window { first, last, .. } = &span else {
            panic!("a refresh rereads the loaded rows");
        };
        assert_eq!(*first, None);
        assert_eq!(last.thread_id.0, "t00");
        read_with_drafts(&mut session, &folder, &drafts, span);
        assert_eq!(&ids(&session)[..3], ["draft-old", "draft-new", "t00"]);
        session.keep_rows = 1_000;
        read_with_drafts(&mut session, &folder, &drafts, FolderSpan::Top { rows: 10 });
        session.cursor_id = Some("draft-old".into());
        // Down past the loaded rows: more load below, in order.
        session.on_key(KeyId::Char('1'), 4);
        session.on_key(KeyId::Char('1'), 4);
        session.on_key(KeyId::Char('j'), 4);
        land_with_drafts(&mut session, &folder, &drafts);
        let loaded = ids(&session);
        let threads: Vec<&String> = loaded.iter().skip(2).collect();
        let expected: Vec<String> = (0..threads.len())
            .map(|index| format!("t{index:02}"))
            .collect();
        assert_eq!(threads, expected.iter().collect::<Vec<_>>());
        assert_eq!(session.cursor_id.as_deref(), Some("t09"));
        // The bottom does not reach the top, so the drafts are not shown.
        session.on_key(KeyId::Char('G'), 4);
        land_with_drafts(&mut session, &folder, &drafts);
        assert_eq!(session.cursor_id.as_deref(), Some("t29"));
        assert!(session.threads.iter().all(|row| row.draft.is_none()));
        // And back at the top, they are again.
        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('g'), 4);
        land_with_drafts(&mut session, &folder, &drafts);
        assert_eq!(session.cursor_id.as_deref(), Some("draft-old"));
    }

    #[test]
    fn j_past_the_last_loaded_row_moves_on_once_the_next_page_lands() {
        // No margin, so nothing loads early and the edge is reached.
        let (mut session, folder) = paged(30, 10, 0);
        session.on_key(KeyId::Char('9'), 4);
        session.on_key(KeyId::Char('j'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("t09"));
        assert!(!session.has_fetch());
        // One more row down: it is not loaded, so the cursor waits.
        session.on_key(KeyId::Char('j'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("t09"));
        // A second j while the page loads still counts.
        session.on_key(KeyId::Char('j'), 4);
        assert_eq!(land(&mut session, &folder), 1);
        assert_eq!(session.cursor_id.as_deref(), Some("t11"));
        assert!(session.more_after);
        // A count that runs past several pages.
        session.on_key(KeyId::Char('1'), 4);
        session.on_key(KeyId::Char('5'), 4);
        session.on_key(KeyId::Char('j'), 4);
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t26"));
        // And off the end of the folder: stops at the last row.
        session.on_key(KeyId::Char('9'), 4);
        session.on_key(KeyId::Char('j'), 4);
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t29"));
        assert!(!session.more_after);
    }

    #[test]
    fn moving_near_the_end_of_the_loaded_rows_loads_more_first() {
        let (mut session, folder) = paged(30, 10, 3);
        for _ in 0..6 {
            session.on_key(KeyId::Char('j'), 4);
            assert!(!session.has_fetch());
        }
        // Three rows left below t06: load before the reader gets there.
        session.on_key(KeyId::Char('j'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("t07"));
        assert_eq!(land(&mut session, &folder), 1);
        assert_eq!(session.threads.len(), 20);
        assert_eq!(session.cursor_id.as_deref(), Some("t07"));
    }

    #[test]
    fn g_and_gg_reach_the_ends_of_rows_that_are_not_loaded() {
        let (mut session, folder) = paged(30, 10, 2);
        session.on_key(KeyId::Char('G'), 4);
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t29"));
        assert!(session.more_before);
        assert!(!session.more_after);
        assert!(
            session.threads.len() <= 10,
            "G loads the end, not the folder"
        );
        // k past the first loaded row loads the rows above.
        session.on_key(KeyId::Char('1'), 4);
        session.on_key(KeyId::Char('2'), 4);
        session.on_key(KeyId::Char('k'), 4);
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t17"));
        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('g'), 4);
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t00"));
        assert!(!session.more_before);
        // A count counts from the top of the folder, loaded or not.
        session.on_key(KeyId::Char('G'), 4);
        land(&mut session, &folder);
        session.on_key(KeyId::Char('5'), 4);
        session.on_key(KeyId::Char('G'), 4);
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t04"));
        session.on_key(KeyId::Char('2'), 4);
        session.on_key(KeyId::Char('5'), 4);
        session.on_key(KeyId::Char('G'), 4);
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t24"));
    }

    #[test]
    fn a_visual_range_keeps_its_anchor_across_loaded_pages() {
        let (mut session, folder) = paged(30, 10, 2);
        session.on_key(KeyId::Char('j'), 4);
        session.on_key(KeyId::Char('j'), 4);
        land(&mut session, &folder);
        session.on_key(KeyId::Char('V'), 4);
        session.on_key(KeyId::Char('G'), 4);
        land(&mut session, &folder);
        assert!(session.visual);
        assert_eq!(session.cursor_id.as_deref(), Some("t29"));
        let highlighted = session.highlighted();
        assert_eq!(highlighted.len(), 28);
        assert!(highlighted.contains("t02") && highlighted.contains("t15"));
        // Back up over the anchor and to the top.
        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('g'), 4);
        land(&mut session, &folder);
        assert!(session.visual);
        assert_eq!(session.highlighted().len(), 3);
    }

    #[test]
    fn rows_sync_adds_above_the_selection_keep_it_selected() {
        let (mut session, mut folder) = paged(30, 10, 2);
        for _ in 0..5 {
            session.on_key(KeyId::Char('j'), 4);
        }
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t05"));
        let loaded = session.threads.len();
        for (index, id) in ["n0", "n1", "n2"].iter().enumerate() {
            folder.insert(dated(id, 2_000_000 - index as i64));
        }
        // Between two rows that are loaded, too.
        folder.insert(dated("mid", 1_000_000 - 2 * 60 - 1));
        refresh_with(&mut session, &folder, Fetch::default(), None);
        assert_eq!(session.cursor_id.as_deref(), Some("t05"));
        assert_eq!(session.cursor_index(), 9);
        assert_eq!(&ids(&session)[..4], ["n0", "n1", "n2", "t00"]);
        assert_eq!(session.threads.len(), loaded + 4);

        // Further down, with the top not loaded: rows added above the
        // loaded ones stay out, and the selection does not move.
        session.on_key(KeyId::Char('G'), 4);
        land(&mut session, &folder);
        session.on_key(KeyId::Char('k'), 4);
        let before = ids(&session);
        assert!(session.more_before);
        folder.insert(dated("n3", 3_000_000));
        refresh_with(&mut session, &folder, Fetch::default(), None);
        assert_eq!(ids(&session), before);
        assert_eq!(session.cursor_id.as_deref(), Some("t28"));
    }

    #[test]
    fn archiving_the_last_loaded_row_selects_the_next_one_once_it_loads() {
        let (mut session, mut folder) = paged(30, 10, 0);
        session.on_key(KeyId::Char('9'), 4);
        session.on_key(KeyId::Char('j'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("t09"));
        session.remove_thread("t09");
        folder.remove("t09");
        // Nothing is selected while the row in its place loads, so a second
        // `e` cannot archive t08.
        assert_eq!(session.cursor_id, None);
        assert_eq!(land(&mut session, &folder), 1);
        assert_eq!(session.cursor_id.as_deref(), Some("t10"));
        // At the true end of the folder the row above takes its place.
        session.on_key(KeyId::Char('G'), 4);
        land(&mut session, &folder);
        session.remove_thread("t29");
        assert_eq!(session.cursor_id.as_deref(), Some("t28"));
        assert!(!session.has_fetch());
    }

    #[test]
    fn archiving_the_last_loaded_row_then_moving_never_acts_on_the_top_row() {
        let (mut session, mut folder) = paged(30, 10, 0);
        session.on_key(KeyId::Char('9'), 4);
        session.on_key(KeyId::Char('j'), 4);
        session.remove_thread("t09");
        folder.remove("t09");
        assert_eq!(session.cursor_id, None);
        assert_eq!(session.selected_index(), None, "the list does not scroll");
        // `j` while the next page loads: still nothing selected, and not
        // the top row. The motion is kept.
        let step = session.on_key(KeyId::Char('j'), 4);
        assert!(!step.archive);
        assert_eq!(session.cursor_id, None);
        // A sync refresh that brings no rows past the gap keeps it.
        refresh_with(&mut session, &folder, Fetch::default(), None);
        assert_eq!(session.threads.len(), 9);
        assert_eq!(
            session.cursor_id, None,
            "set_threads must not pick the top row"
        );
        land(&mut session, &folder);
        // t10 took t09's place; `j` moved one past it.
        assert_eq!(session.cursor_id.as_deref(), Some("t11"));
        // So the next `e` archives t11, not t00.
        assert!(session.on_key(KeyId::Char('e'), 4).archive);

        // A count and Ctrl-d add up the same way.
        let (mut session, mut folder) = paged(60, 10, 0);
        session.on_key(KeyId::Char('9'), 4);
        session.on_key(KeyId::Char('j'), 4);
        session.remove_thread("t09");
        folder.remove("t09");
        session.on_key(KeyId::Char('3'), 4);
        session.on_key(KeyId::Char('j'), 4);
        session.on_key(KeyId::Ctrl('d'), 4);
        assert_eq!(session.cursor_id, None);
        land(&mut session, &folder);
        // t10, then 3 rows, then a page of 4.
        assert_eq!(session.cursor_id.as_deref(), Some("t17"));

        // `k` from the gap goes to the row above the archived one.
        let (mut session, mut folder) = paged(30, 10, 0);
        session.on_key(KeyId::Char('9'), 4);
        session.on_key(KeyId::Char('j'), 4);
        session.remove_thread("t09");
        folder.remove("t09");
        session.on_key(KeyId::Char('k'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("t08"));
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t08"));
    }

    #[test]
    fn a_click_or_cancel_after_archiving_the_last_loaded_row_keeps_its_place() {
        // cancel_pending_keys (a click in the conversation) keeps the place.
        let (mut session, mut folder) = paged(30, 10, 0);
        session.on_key(KeyId::Char('9'), 4);
        session.on_key(KeyId::Char('j'), 4);
        session.remove_thread("t09");
        folder.remove("t09");
        session.on_key(KeyId::Char('j'), 4);
        session.cancel_pending_keys();
        assert_eq!(session.cursor_id, None);
        session.on_key(KeyId::Char('j'), 4);
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t11"));
        assert!(session.on_key(KeyId::Char('e'), 4).archive);

        // A click on a row selects it, and the late page does not move it.
        let (mut session, mut folder) = paged(30, 10, 0);
        session.on_key(KeyId::Char('9'), 4);
        session.on_key(KeyId::Char('j'), 4);
        session.remove_thread("t09");
        folder.remove("t09");
        session.cancel_pending_keys();
        session.cursor_id = Some("t04".into());
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t04"));
        session.on_key(KeyId::Char('j'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("t05"));
    }

    #[test]
    fn a_jump_past_the_loaded_rows_glides_when_it_lands() {
        use crate::motion::{GLIDE_MAX_VIEWPORTS, Glide, Motion, SLIDE};
        use crate::present::{Landing, landing, list_scroll_target};
        // 150 threads, 100 loaded: `G` and `gg` wait for rows.
        let (mut session, folder) = paged(150, 100, 40);
        let (row, view) = (68.0, 680.0);
        for (keys, expected) in [(&['G'][..], "t149"), (&['g', 'g'][..], "t00")] {
            for key in keys {
                session.on_key(KeyId::Char(*key), 10);
            }
            let before = session.cursor_id.clone();
            let fetch = session.take_fetch().expect("the jump asks for rows");
            // The old selection is not in the rows the jump reads, but the
            // view must not read again to follow it: the jump moves it.
            let key = session.cursor_thread().map(row_key);
            let span = session.span(fetch, None, None);
            let (rows, _, _) = folder.read(&span);
            assert!(!rows.iter().any(|row| Some(&row.id) == before.as_ref()));
            assert_eq!(session.moved_selection(&rows, key), None);
            // The read the view sends for it holds the viewport: no switch,
            // no change, nothing to select.
            refresh_with(&mut session, &folder, fetch, None);
            land(&mut session, &folder);
            assert_eq!(session.cursor_id.as_deref(), Some(expected));
            let same = session.cursor_id == before;
            assert_eq!(landing(false, true, same), Landing::Glide, "{keys:?}");
            // The glide covers at most one viewport, however far it went.
            let index = session.selected_index().unwrap();
            let target = list_scroll_target(0.0, index, None, session.threads.len(), row, view);
            let glide = Glide::start(
                &Motion::new(true),
                -60.0 * row,
                target,
                view * GLIDE_MAX_VIEWPORTS,
                SLIDE,
                std::time::Instant::now(),
            )
            .unwrap();
            assert!((glide.from - glide.to).abs() <= view);
        }
        // A folder switch, or the reader's own change, still jumps.
        assert_eq!(landing(true, false, false), Landing::Jump);
        assert_eq!(landing(false, false, false), Landing::Jump);
        assert_eq!(landing(false, true, true), Landing::Hold);
    }

    #[test]
    fn a_motion_after_g_cancels_the_jump() {
        let (mut session, folder) = paged(30, 10, 0);
        session.on_key(KeyId::Char('G'), 4);
        let jump = session.take_fetch().unwrap();
        assert_eq!(jump.jump, Some(Jump::Bottom(10)));
        // `j` before the bottom rows land.
        session.on_key(KeyId::Char('j'), 4);
        let cancel = session.take_fetch().unwrap();
        assert!(cancel.drop_jump);
        // The view merges the two reads into one: no jump left.
        let merged = jump.merge(cancel);
        assert_eq!(merged.jump, None);
        refresh_with(&mut session, &folder, merged, None);
        assert_eq!(session.cursor_id.as_deref(), Some("t01"));
        assert!(!session.more_before);

        // The same for `gg` from the bottom, and a jump not yet taken.
        session.on_key(KeyId::Char('G'), 4);
        land(&mut session, &folder);
        assert!(session.more_before);
        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('g'), 4);
        session.on_key(KeyId::Char('k'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("t28"));
        land(&mut session, &folder);
        assert_eq!(session.cursor_id.as_deref(), Some("t28"));
        assert!(session.more_before);

        // A later jump after a motion still jumps.
        let merged = Fetch {
            drop_jump: true,
            ..Fetch::default()
        }
        .merge(Fetch {
            jump: Some(Jump::Top(5)),
            ..Fetch::default()
        });
        assert_eq!(merged.jump, Some(Jump::Top(5)));
    }

    #[test]
    fn new_mail_on_the_selected_thread_outside_the_loaded_rows_keeps_it_open() {
        let (mut session, mut folder) = paged(30, 10, 2);
        session.on_key(KeyId::Char('G'), 4);
        land(&mut session, &folder);
        session.on_key(KeyId::Char('k'), 4);
        assert_eq!(session.cursor_id.as_deref(), Some("t28"));
        assert!(session.more_before);
        // New mail on t28 moves it to the top of the folder, above the
        // loaded rows.
        folder.remove("t28");
        folder.insert(dated("t28", 5_000_000));
        let span = session.span(Fetch::default(), None, None);
        let (rows, before, after) = folder.read(&span);
        assert!(!rows.iter().any(|row| row.id == "t28"));
        // The view looks the thread up by id and reads the rows around it.
        let found = Some(row_key(&folder.0[0]));
        let key = session.moved_selection(&rows, found.clone()).unwrap();
        refresh_with(&mut session, &folder, Fetch::default(), Some(&key));
        assert!(session.select_if_present("t28"));
        assert_eq!(session.cursor_id.as_deref(), Some("t28"));
        // Rows that include it, or another selected thread, need nothing.
        assert_eq!(
            session.moved_selection(&ids_rows(&session), found.clone()),
            None
        );
        session.cursor_id = Some("t01".into());
        assert_eq!(session.moved_selection(&rows, found), None);
        // Without the follow the list would have taken the old place.
        let mut plain = session.clone();
        plain.cursor_id = Some("t28".into());
        plain.set_page(rows, before, after, 0);
        assert_ne!(plain.cursor_id.as_deref(), Some("t28"));
    }

    fn ids_rows(session: &Session) -> Vec<ThreadRow> {
        session.threads.clone()
    }

    #[test]
    fn an_optimistic_archive_lowers_the_unread_count() {
        let (mut session, _folder) = paged(30, 10, 2);
        session.threads[3].unread = true;
        session.unread_count = 5;
        session.remove_thread("t03");
        assert_eq!(session.unread_threads(), 4);
        session.remove_thread("t04");
        assert_eq!(session.unread_threads(), 4, "a read row leaves it alone");
    }

    #[test]
    fn undo_loads_the_rows_around_a_thread_outside_the_loaded_ones() {
        let (mut session, folder) = paged(40, 10, 2);
        let back = row_key(&folder.0[20]);
        let span = session.span(Fetch::default(), Some(&back), None);
        assert!(matches!(span, FolderSpan::Window { first: Some(_), .. }));
        refresh_with(&mut session, &folder, Fetch::default(), Some(&back));
        assert!(session.select_if_present("t20"));
        assert!(session.more_before && session.more_after);
        // Inside the loaded rows, undo just rereads them.
        let near = row_key(&folder.0[22]);
        assert!(matches!(
            session.span(Fetch::default(), Some(&near), None),
            FolderSpan::Window { before: 0, .. }
        ));
    }

    #[test]
    fn a_long_walk_down_keeps_only_rows_near_the_reader() {
        let (mut session, folder) = paged(400, 20, 5);
        session.keep_rows = 30;
        for _ in 0..300 {
            session.on_key(KeyId::Char('j'), 4);
            land(&mut session, &folder);
        }
        assert_eq!(
            session.cursor_id.as_deref(),
            Some(folder.0[300].id.as_str())
        );
        refresh_with(&mut session, &folder, Fetch::default(), None);
        assert!(
            session.threads.len() <= 2 * 30 + 1,
            "{}",
            session.threads.len()
        );
        assert!(session.more_before && session.more_after);
        assert_eq!(
            session.cursor_id.as_deref(),
            Some(folder.0[300].id.as_str())
        );
    }

    #[test]
    fn the_unread_count_covers_rows_that_are_not_loaded() {
        let (mut session, folder) = paged(30, 10, 2);
        let (rows, before, after) = folder.read(&FolderSpan::Top { rows: 10 });
        session.set_page(rows, before, after, 17);
        assert_eq!(session.unread_threads(), 17);
    }

    #[test]
    fn an_old_notice_timer_leaves_a_newer_notice_alone() {
        // #99: two drafts opened within the settle delay; the first timer
        // cleared the second "opening draft" notice, which had other words.
        let mut session =
            Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        session.show_toast("opening draft \u{201c}A\u{201d}", false);
        let first = session.toast_generation();
        session.show_toast("opening draft \u{201c}A\u{201d}", false);
        let second = session.toast_generation();
        assert!(!session.clear_toast_if(first), "same words, newer notice");
        assert!(session.toast.is_some());
        assert!(session.clear_toast_if(second));
        assert!(session.toast.is_none());
        assert!(!session.clear_toast_if(second), "nothing left to clear");
    }

    #[test]
    fn the_startup_crash_notice_survives_navigation_and_ordinary_toasts() {
        let (mut session, _) = paged(30, 10, 2);
        let crash = Toast::with_hint("Fell crashed last time", "space ? c".into(), " to view");
        session.startup_crash_notice = Some(crash.clone());
        session.show_toast("Other notice", false);
        // The inbox clears its ordinary toast before handling a key.
        session.clear_toast();
        session.on_key(KeyId::Char('j'), 10);
        assert_eq!(session.status_notice(), Some(&crash));
        session.on_key(KeyId::Escape, 10);
        assert!(session.status_notice().is_none());
    }

    #[test]
    fn label_notices_survive_keys_and_undo_keeps_the_notice_it_replaces() {
        let mut session = session();
        session.show_label_toast(present::LabelNotice {
            verb: "Removed",
            label: "travel".into(),
            relation: "from",
            subject: None,
            count: 1,
            undo: true,
        });
        let removal = session.toast_generation();
        for key in [KeyId::Char('/'), KeyId::Enter] {
            let step = session.on_key(key, 10);
            session.dismiss_key_notice(removal, step.undo);
            assert_eq!(
                session.status_notice().unwrap().text,
                "Removed “travel” from 1 conversation · u"
            );
        }
        assert!(session.clear_toast_if(removal));

        session.end_search();
        session.show_toast("Archived · u", false);
        let archive = session.toast_generation();
        let step = session.on_key(KeyId::Char('u'), 10);
        assert!(step.undo);
        session.dismiss_key_notice(archive, step.undo);
        assert_eq!(session.status_notice().unwrap().text, "Archived · u");
    }

    fn empty_message(expanded: bool) -> ConvMessage {
        ConvMessage {
            mail_headers: Vec::new(),
            id: {
                static NEXT: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                format!(
                    "m{}",
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                )
            },
            sender: "Jon".into(),
            email: "jon@x".into(),
            folders: vec!["Inbox".into()],
            date_unix: 0,
            snippet: "snip".into(),
            body: crate::text_render::render_stored_body(Some("body"), "snip", ""),
            rich: crate::present::Rich::Plain,
            expanded,
            attachments: Vec::new(),
            recipients: None,
            context: None,
            body_state: fell_core::BodyState::Stored,
        }
    }
}
