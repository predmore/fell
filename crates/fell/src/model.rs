//! Inbox session: folders, selection, chords, palette. No drawing.

use std::collections::HashSet;

use crate::keymap::{self, Action, KeyId, Keymap, PaletteItem, SeqMatch, WhichItem};
use crate::present::{ConvMessage, Pane, ThreadRow, folder_label, merge_folders};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SyncKind {
    /// Fixtures were loaded because read-only IMAP is not on this build.
    FixtureOffline,
    /// The store already had mail. Sync is still not connected.
    LocalOffline,
    FirstSync,
    StoreError,
}

impl SyncKind {
    pub fn status(&self) -> &'static str {
        match self {
            Self::FixtureOffline => "fixture · offline",
            Self::LocalOffline => "local · offline",
            Self::FirstSync => "waiting for first sync",
            Self::StoreError => "mailbox error",
        }
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
    pub threads: Vec<ThreadRow>,
    pub cursor_id: Option<String>,
    pub anchor_id: Option<String>,
    pub picked: HashSet<String>,
    pub visual: bool,
    pub pane: Pane,
    pub full_width: bool,
    pub messages: Vec<ConvMessage>,
    pub message_cursor: usize,
    pub quotes_open: HashSet<usize>,
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
            cursor_id: None,
            anchor_id: None,
            picked: HashSet::new(),
            visual: false,
            pane: Pane::List,
            full_width: false,
            messages: Vec::new(),
            message_cursor: 0,
            quotes_open: HashSet::new(),
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
        match (&self.startup_error, &self.error) {
            (Some(startup), Some(query)) => Some(format!("{startup}\n{query}")),
            (Some(startup), None) => Some(startup.clone()),
            (None, Some(query)) => Some(query.clone()),
            (None, None) => None,
        }
    }

    pub fn folder_title(&self) -> String {
        folder_label(&self.folder)
    }

    pub fn unread_threads(&self) -> usize {
        self.threads.iter().filter(|thread| thread.unread).count()
    }

    pub fn cursor_index(&self) -> usize {
        self.threads
            .iter()
            .position(|thread| Some(&thread.id) == self.cursor_id.as_ref())
            .unwrap_or(0)
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
        self.threads = threads;
        if let Some(id) = &previous {
            if self.threads.iter().any(|thread| &thread.id == id) {
                self.cursor_id = Some(id.clone());
            } else {
                self.cursor_id = self.threads.first().map(|thread| thread.id.clone());
            }
        } else {
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

    pub fn set_messages(&mut self, messages: Vec<ConvMessage>) {
        self.messages = messages;
        self.quotes_open.clear();
        self.message_cursor = self.messages.len().saturating_sub(1);
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
                step.arm_which = true;
                let ran = self.run(action, page_rows);
                step.reload_threads |= ran.reload_threads;
                step.reload_messages |= ran.reload_messages;
                step.scroll_pages = ran.scroll_pages;
                step.scroll_lines = ran.scroll_lines;
                step.apply_move |= ran.apply_move;
                step.undo |= ran.undo;
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
            Action::Undo => {
                self.count.clear();
                return Step {
                    undo: true,
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
                if !self.quotes_open.remove(&self.message_cursor) {
                    self.quotes_open.insert(self.message_cursor);
                }
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
        }
        step
    }

    fn go_folder(&mut self, folder: &str) -> bool {
        self.count.clear();
        if self.folder == folder {
            return false;
        }
        self.folder = folder.to_string();
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
            let end = match self.pane {
                Pane::List => self.threads.len().saturating_sub(1),
                Pane::Conversation => self.messages.len().saturating_sub(1),
            };
            self.jump_index(end)
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
                let index = index.min(self.threads.len() - 1);
                let id = self.threads[index].id.clone();
                let changed = self.cursor_id.as_deref() != Some(id.as_str());
                self.cursor_id = Some(id);
                changed
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
            Pane::List => {
                let len = self.threads.len();
                if len == 0 {
                    return false;
                }
                let next = step_index(self.cursor_index(), len, motion);
                let id = self.threads[next].id.clone();
                let changed = self.cursor_id.as_deref() != Some(id.as_str());
                self.cursor_id = Some(id);
                changed
            }
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

    /// Drop a typed count and an unfinished chord. Mouse selection calls this
    /// so the next key does not finish the prefix.
    pub fn cancel_pending_keys(&mut self) {
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
        Some(match self.sync {
            SyncKind::FirstSync => Banner {
                title: "Waiting for the first sync".into(),
                hint: "The mailbox is on disk. Sync is not connected in this build.".into(),
            },
            SyncKind::StoreError => Banner {
                title: self
                    .startup_error
                    .clone()
                    .unwrap_or_else(|| "Couldn't open the mailbox".into()),
                hint: "? details".into(),
            },
            SyncKind::FixtureOffline | SyncKind::LocalOffline => Banner {
                title: format!("No conversations in {}", self.folder_title()),
                hint: "gi inbox · gf folders".into(),
            },
        })
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
        assert!(banner.title.contains("first sync"));
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

    fn empty_message(expanded: bool) -> ConvMessage {
        ConvMessage {
            sender: "Jon".into(),
            email: "jon@x".into(),
            folders: "Inbox".into(),
            date_unix: 0,
            snippet: "snip".into(),
            body: crate::text_render::render_stored_body(Some("body"), "snip"),
            expanded,
        }
    }
}
