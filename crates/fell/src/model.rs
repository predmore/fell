//! Inbox session: folders, selection, chords, palette. No drawing.

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use std::ops::Range;

use fell_core::{FolderSpan, ListKey, SyncFailureKind, ThreadId};

use crate::keymap::{self, Action, KeyId, Keymap, PaletteItem, SeqMatch, WhichItem};
use crate::present::{ConvMessage, Pane, ThreadRow, folder_label, merge_folders};

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
    /// Messages written so far in this pass.
    Syncing {
        stored: usize,
    },
    /// Unix time the last pass finished, and how many messages it wrote.
    Synced {
        at: i64,
        stored: usize,
    },
    Failed(SyncFailure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SyncFailure {
    pub kind: SyncFailureKind,
    /// The backend is Proton Mail Bridge, so copy can name it.
    pub bridge: bool,
    pub detail: String,
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
            Self::Syncing { stored: 0 } => "syncing…".into(),
            Self::Syncing { stored } => format!("syncing · {stored} new"),
            Self::Synced { at, .. } => {
                let ago = now.saturating_sub(*at).max(0);
                if ago < 60 {
                    "synced just now".into()
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
                SyncFailureKind::Password => "offline · password_command failed".into(),
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
}

/// What confirming the folder picker does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderIntent {
    /// `gf` switches the list to the chosen folder.
    Go,
    /// `m` moves the open thread out of the folder on screen.
    Move,
}

/// One-line result of the last move or undo. The next key clears it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    pub text: String,
    pub error: bool,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub reload_threads: bool,
    pub reload_messages: bool,
    pub arm_which: bool,
    /// Conversation pages, in viewport heights. Positive moves down.
    pub scroll_pages: i32,
    /// Conversation lines. Positive moves down.
    pub scroll_lines: i32,
    /// The view moves the thread to [`Session::pending_move`].
    pub apply_move: bool,
    /// `u` restores the latest local action.
    pub undo: bool,
    /// `e` archives the open thread out of the folder on screen.
    pub archive: bool,
    /// Start a sync pass now instead of waiting for the next one.
    pub sync_now: bool,
    /// Bring back the agent review window that closed last.
    pub reopen_review: bool,
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
    pub messages: Vec<ConvMessage>,
    pub message_cursor: usize,
    /// Messages whose collapsed parts (quotes, signature, every recipient)
    /// are open, by Message-ID, so a reread that shifts rows keeps them.
    pub quotes_open: HashSet<String>,
    /// Messages whose full recipient list is open, by Message-ID. A click
    /// on the recipient line opens only this; `q` opens everything.
    pub recipients_open: HashSet<String>,
    pub overlay: Overlay,
    pub chord: Vec<KeyId>,
    pub chord_gen: u64,
    pub which_visible: bool,
    pub count: String,
    pub keymap: Keymap,
    pub sync: SyncKind,
    /// Mailbox or keymap failure from startup. Folder and message reads do not clear it.
    pub startup_error: Option<String>,
    /// Failure from the latest folder or message read.
    pub error: Option<String>,
    pub error_fault: Option<QueryFault>,
    pub recent: Vec<Action>,
    pub palette_query: String,
    pub palette_index: usize,
    pub folder_query: String,
    pub folder_index: usize,
    pub folder_intent: FolderIntent,
    pub toast: Option<Toast>,
    /// Destination chosen by `m`, consumed when the view applies the move.
    pub pending_move: Option<String>,
    /// The folder on screen has not been read from the store yet.
    pub loading: bool,
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
            folder: "INBOX".into(),
            folders: Vec::new(),
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
            messages: Vec::new(),
            message_cursor: 0,
            quotes_open: HashSet::new(),
            recipients_open: HashSet::new(),
            overlay: Overlay::None,
            chord: Vec::new(),
            chord_gen: 0,
            which_visible: false,
            count: String::new(),
            keymap,
            sync,
            startup_error: None,
            error: None,
            error_fault: None,
            recent: Vec::new(),
            palette_query: String::new(),
            palette_index: 0,
            folder_query: String::new(),
            folder_index: 0,
            folder_intent: FolderIntent::Go,
            toast: None,
            pending_move: None,
            loading: false,
        }
    }

    pub fn show_toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            error,
        });
    }

    pub fn clear_toast(&mut self) {
        self.toast = None;
    }

    pub fn folder_prompt(&self) -> &'static str {
        match self.folder_intent {
            FolderIntent::Move => "Move to",
            FolderIntent::Go => "Folder",
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
        let lines: Vec<&str> = [self.startup_error.as_deref(), self.error.as_deref(), sync]
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
        folder_label(&self.folder)
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
        let place = self.threads.partition_point(|row| row_key(row) <= *key);
        Some((place, *down))
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
        if self.visual { "visual" } else { "normal" }
    }

    pub fn prefix_text(&self) -> String {
        keymap::prefix_text(&self.chord, &self.keymap.leader_name)
    }

    pub fn which_items(&self) -> Vec<WhichItem> {
        keymap::which_key(&self.keymap, &self.chord)
    }

    pub fn palette(&self) -> Vec<PaletteItem> {
        keymap::palette_items(&self.keymap, &self.palette_query, &self.recent)
    }

    pub fn folder_choices(&self) -> Vec<String> {
        let all = merge_folders(&self.folders);
        let query = self.folder_query.trim();
        all.into_iter()
            .filter(|name| {
                if self.folder_intent == FolderIntent::Move
                    && name.eq_ignore_ascii_case(&self.folder)
                {
                    return false;
                }
                if query.is_empty() {
                    return true;
                }
                folder_label(name)
                    .to_ascii_lowercase()
                    .contains(&query.to_ascii_lowercase())
                    || name
                        .to_ascii_lowercase()
                        .contains(&query.to_ascii_lowercase())
            })
            .collect()
    }

    pub fn highlighted(&self) -> HashSet<String> {
        let mut ids = self.picked.clone();
        if let Some(id) = &self.cursor_id
            && self.threads.iter().any(|thread| &thread.id == id)
        {
            ids.insert(id.clone());
        }
        if !self.visual || self.threads.is_empty() {
            return ids;
        }
        let Some(anchor) = self
            .threads
            .iter()
            .position(|thread| Some(&thread.id) == self.anchor_id.as_ref())
        else {
            return ids;
        };
        let cursor = self.cursor_index().min(self.threads.len() - 1);
        let (lo, hi) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        for thread in &self.threads[lo..=hi] {
            ids.insert(thread.id.clone());
        }
        ids
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
        self.picked
            .retain(|id| self.threads.iter().any(|thread| &thread.id == id));
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
    pub fn moved_selection(&self, rows: &[ThreadRow], found: Option<ListKey>) -> Option<ListKey> {
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
        let (Some(top), Some(bottom), Some((lo, hi))) = (
            self.threads.first(),
            self.threads.last(),
            self.focus(visible),
        ) else {
            return FolderSpan::Top {
                rows: self.load_rows.max(fetch.after),
            };
        };
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
        let len = self.threads.len();
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
            let at = self
                .threads
                .partition_point(|row| row_key(row) < *key)
                .min(len - 1);
            start = start.min(at);
            end = end.max(at);
        }
        // From the top when the top is loaded, so rows sync adds above the
        // first one are read too.
        let first = (start > 0 || self.more_before).then(|| row_key(&self.threads[start]));
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
            last: row_key(&self.threads[end]),
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
                let next = self.threads.iter().find(|row| row_key(row) > key);
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

    pub fn is_open(&self, index: usize) -> bool {
        self.messages
            .get(index)
            .is_some_and(|message| self.quotes_open.contains(&message.id))
    }

    pub fn recipients_are_open(&self, index: usize) -> bool {
        self.messages
            .get(index)
            .is_some_and(|message| self.recipients_open.contains(&message.id))
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
        self.quotes_open.clear();
        self.recipients_open.clear();
        self.message_cursor = self.messages.len().saturating_sub(1);
    }

    /// Reread of the thread already open (sync added or dropped a message).
    /// A reader on the last message follows to the new last one. A reader
    /// further up keeps their message and opened quotes. Returns whether the
    /// cursor moved to the end.
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
        if at_end {
            self.set_messages(messages);
            return true;
        }
        for new in &mut messages {
            if let Some(old) = self.messages.iter().find(|old| old.id == new.id) {
                new.expanded = old.expanded;
            }
        }
        let cursor = self.message_cursor;
        let len = messages.len();
        for set in [&mut self.quotes_open, &mut self.recipients_open] {
            set.retain(|id| messages.iter().any(|message| &message.id == id));
        }
        self.messages = messages;
        self.message_cursor = cursor.min(len.saturating_sub(1));
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
        if self.overlay != Overlay::None {
            return self.overlay_key(key, page_rows);
        }
        let mut step = Step::default();
        if self.chord.is_empty() && self.pane == Pane::Conversation {
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
            Overlay::Help => {
                if matches!(key, KeyId::Escape | KeyId::Enter | KeyId::Char('?')) {
                    self.overlay = Overlay::None;
                }
                Step::default()
            }
            Overlay::None => Step::default(),
        }
    }

    fn palette_key(&mut self, key: KeyId, page_rows: usize) -> Step {
        let len = self.palette().len();
        match key {
            KeyId::Escape => {
                self.overlay = Overlay::None;
                Step::default()
            }
            KeyId::Enter => {
                let Some(item) = self.palette().get(self.palette_index).cloned() else {
                    self.overlay = Overlay::None;
                    return Step::default();
                };
                self.overlay = Overlay::None;
                self.run(item.action, page_rows)
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

    fn folder_key(&mut self, key: KeyId) -> Step {
        let len = self.folder_choices().len();
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
        let choices = self.folder_choices();
        let Some(chosen) = choices.get(self.folder_index).cloned() else {
            self.overlay = Overlay::None;
            self.folder_query.clear();
            self.folder_intent = FolderIntent::Go;
            return Step::default();
        };
        let intent = self.folder_intent;
        self.overlay = Overlay::None;
        self.folder_query.clear();
        self.folder_intent = FolderIntent::Go;
        match intent {
            FolderIntent::Move => {
                self.pending_move = Some(chosen);
                Step {
                    apply_move: true,
                    ..Step::default()
                }
            }
            FolderIntent::Go => {
                if self.folder != chosen {
                    self.folder = chosen;
                    self.forget_window();
                    Step {
                        reload_threads: true,
                        ..Step::default()
                    }
                } else {
                    Step::default()
                }
            }
        }
    }

    pub fn perform(&mut self, action: Action, page_rows: usize) -> Step {
        self.run(action, page_rows)
    }

    fn run(&mut self, action: Action, page_rows: usize) -> Step {
        if !matches!(action, Action::Palette | Action::Help) {
            self.recent.retain(|item| *item != action);
            self.recent.insert(0, action);
            self.recent.truncate(8);
        }
        let mut step = Step::default();
        match action {
            Action::Next => {
                let count = self.take_count();
                step.reload_messages = self.move_cursor(Motion::Forward(count));
            }
            Action::Prev => {
                let count = self.take_count();
                step.reload_messages = self.move_cursor(Motion::Backward(count));
            }
            Action::First => step.reload_messages = self.jump_first(),
            Action::Last => step.reload_messages = self.jump_last(),
            Action::Open => {
                self.count.clear();
                if self.pane == Pane::List {
                    self.pane = Pane::Conversation;
                } else if let Some(message) = self.messages.get_mut(self.message_cursor) {
                    message.expanded = !message.expanded;
                }
            }
            Action::Back => {
                self.count.clear();
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
            Action::Inbox => step.reload_threads = self.go_folder("INBOX"),
            Action::Sent => step.reload_threads = self.go_folder("Sent"),
            Action::Drafts => step.reload_threads = self.go_folder("Drafts"),
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
            Action::Move => {
                self.count.clear();
                if self.cursor_id.is_none() {
                    self.show_toast("No conversation", true);
                    return Step::default();
                }
                self.folder_intent = FolderIntent::Move;
                self.overlay = Overlay::Folders;
                self.folder_query.clear();
                self.folder_index = 0;
            }
            Action::Archive => {
                self.count.clear();
                if self.cursor_id.is_none() {
                    self.show_toast("No conversation", true);
                    return Step::default();
                }
                return Step {
                    archive: true,
                    ..Step::default()
                };
            }
            Action::Undo => {
                self.count.clear();
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
                    self.pane = Pane::Conversation;
                }
            }
            Action::ToggleQuotes => {
                self.count.clear();
                self.toggle_open(self.message_cursor);
            }
            Action::Palette => {
                self.count.clear();
                self.overlay = Overlay::Palette;
                self.palette_query.clear();
                self.palette_index = 0;
            }
            Action::Help => {
                self.count.clear();
                self.overlay = Overlay::Help;
            }
            Action::ReopenReview => {
                self.count.clear();
                return Step {
                    reopen_review: true,
                    ..Step::default()
                };
            }
        }
        step
    }

    fn go_folder(&mut self, folder: &str) -> bool {
        self.count.clear();
        if self.folder == folder {
            return false;
        }
        self.folder = folder.to_string();
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

    pub fn list_banner(&self) -> Option<Banner> {
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
                title: format!("Syncing {}", self.folder_title()),
                hint: match &self.sync {
                    SyncKind::Syncing { stored } if *stored > 0 => {
                        format!("{stored} messages stored · conversations appear as they arrive")
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
                    SyncFailureKind::Password => "password_command failed",
                    SyncFailureKind::Other => "Sync failed",
                };
                let fix = match failure.kind {
                    SyncFailureKind::Unreachable if failure.bridge => {
                        format!("Start Bridge, then {retry} to retry")
                    }
                    SyncFailureKind::LoginRejected => format!(
                        "Use the IMAP username and password Bridge shows · fell account setup · {retry} retry"
                    ),
                    _ => format!("? details · {retry} retry"),
                };
                Banner {
                    title: title.into(),
                    hint: fix,
                }
            }
            SyncKind::FixtureOffline | SyncKind::LocalOffline | SyncKind::Synced { .. } => Banner {
                title: format!("No conversations in {}", self.folder_title()),
                hint: "gi inbox · gf folders".into(),
            },
        })
    }
}

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
            unread: false,
        }
    }

    fn session() -> Session {
        let mut session =
            Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        session.set_threads(vec![row("a"), row("b"), row("c"), row("d"), row("e")]);
        session
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
        session.folder = "Drafts".into();
        let banner = session.list_banner().unwrap();
        assert!(banner.title.contains("Drafts"));
        assert!(banner.hint.contains("gi"));
        session.sync = SyncKind::FirstSync;
        let banner = session.list_banner().unwrap();
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
    fn failed_read_is_not_an_empty_folder() {
        let mut session = session();
        session.threads.clear();
        session.sync = SyncKind::LocalOffline;
        session.fail_query(QueryFault::Folder, "database is locked".into());
        let banner = session.list_banner().unwrap();
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
    fn open_parts_stay_with_their_message_when_a_reread_shifts_rows() {
        let mut session =
            Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        let first = empty_message(true);
        let second = empty_message(true);
        session.set_messages(vec![first.clone(), second.clone()]);
        session.message_cursor = 0;
        session.toggle_open(0);
        assert!(session.is_open(0));
        // Sync puts a new message before the one that was opened.
        let newcomer = empty_message(true);
        session.refresh_messages(vec![newcomer, first.clone(), second], false);
        assert!(!session.is_open(0));
        assert!(session.is_open(1));
        // The opened message leaves the thread: nothing else opens in its place.
        session.refresh_messages(vec![empty_message(true)], false);
        assert!(!session.is_open(0));
    }

    #[test]
    fn a_click_on_recipients_opens_only_them_and_q_opens_everything() {
        let mut session =
            Session::new("Caleb", "me@x", SyncKind::FixtureOffline, Keymap::default());
        session.set_messages(vec![empty_message(true)]);
        session.toggle_recipients(0);
        assert!(session.recipients_are_open(0));
        assert!(!session.is_open(0), "quoted text stays closed");
        session.toggle_recipients(0);
        session.toggle_open(0);
        assert!(session.recipients_are_open(0) && session.is_open(0));
        session.toggle_open(0);
        assert!(!session.recipients_are_open(0) && !session.is_open(0));
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

    fn empty_message(expanded: bool) -> ConvMessage {
        ConvMessage {
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
            folders: "Inbox".into(),
            date_unix: 0,
            snippet: "snip".into(),
            body: crate::text_render::render_stored_body(Some("body"), "snip"),
            expanded,
            recipients: None,
        }
    }
}
