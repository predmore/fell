use std::collections::HashSet;

use fell_core::notes::{NoteDecision, Suggestion, ThreadNote};

use super::{Session, Step};
use crate::keymap::{Action, KeyId, SeqMatch, match_seq};
use crate::present::Pane;

pub const SUGGESTIONS_QUERY: &str = "has:suggestions";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoteRequest {
    pub thread: String,
    pub id: i64,
    pub decision: NoteDecision,
    pub reply: bool,
}

#[derive(Clone, Debug, Default)]
pub struct AgentNotes {
    pub unread: HashSet<String>,
    pub pending_threads: HashSet<String>,
    thread: Option<String>,
    notes: Vec<ThreadNote>,
    selected: usize,
    pub collapsed: bool,
    pub shortcuts_hidden: bool,
    dismissed: Option<(i64, bool)>,
}

impl AgentNotes {
    pub fn current(&self, thread: Option<&str>) -> Option<&ThreadNote> {
        (thread.is_some() && thread == self.thread.as_deref())
            .then(|| self.notes.get(self.selected))
            .flatten()
    }

    pub fn position(&self) -> (usize, usize) {
        (self.selected + 1, self.notes.len())
    }

    pub fn update(&mut self, thread: Option<String>, notes: Vec<ThreadNote>) -> bool {
        if self.thread == thread && self.notes == notes {
            return false;
        }
        let previous = (self.thread == thread)
            .then(|| self.notes.get(self.selected).map(|note| note.id))
            .flatten();
        if self.thread != thread {
            self.dismissed = None;
        }
        self.thread = thread;
        self.notes = notes;
        self.selected = previous
            .and_then(|id| self.notes.iter().position(|note| note.id == id))
            .unwrap_or(0);
        let current = self.notes.get(self.selected).map(|note| note.id);
        if current != previous {
            self.collapsed = self
                .dismissed
                .is_some_and(|(id, collapsed)| current == Some(id) && collapsed);
        }
        true
    }

    pub fn resolved(&mut self, request: &NoteRequest) {
        if self.thread.as_deref() != Some(&request.thread) {
            return;
        }
        match request.decision {
            NoteDecision::Dismiss => {
                self.dismissed = Some((request.id, self.collapsed));
                let mut notes = self.notes.clone();
                notes.retain(|note| note.id != request.id);
                self.update(self.thread.clone(), notes);
            }
            NoteDecision::Accept(position) => {
                if let Some(note) = self.notes.iter_mut().find(|note| note.id == request.id) {
                    note.suggestions.retain(|item| item.position != position);
                }
            }
        }
    }
}

impl Session {
    pub fn note(&self) -> Option<&ThreadNote> {
        self.agent_notes.current(self.cursor_id.as_deref())
    }

    fn can_act_on_note(&self) -> bool {
        self.pane == Pane::Conversation
            && self.search_results_current()
            && self
                .search
                .as_ref()
                .is_none_or(|search| search.matches_thread(self.cursor_id.as_deref()))
    }

    pub(super) fn accept_note_key(&self, key: KeyId) -> Option<NoteRequest> {
        if !self.can_act_on_note()
            || self.agent_notes.collapsed
            || self.agent_notes.shortcuts_hidden
            || !self.count.is_empty()
        {
            return None;
        }
        let note = self.note()?;
        let (position, _) = self
            .note_shortcuts(note)
            .into_iter()
            .find(|(_, shortcut)| *shortcut == Some(key))?;
        self.note_request(NoteDecision::Accept(position))
    }

    /// Keep mnemonic keys when possible and give additional labels or snoozes
    /// distinct shortcuts. Leave configured actions and chord prefixes intact.
    pub fn note_shortcuts(&self, note: &ThreadNote) -> Vec<(i64, Option<KeyId>)> {
        let mut used = HashSet::new();
        let mut keys: Vec<_> = note
            .suggestions
            .iter()
            .map(|item| {
                let preferred = match item.action {
                    Suggestion::Archive => self.single_key(Action::Archive),
                    Suggestion::Reply => self.single_key(Action::Reply),
                    Suggestion::MarkRead => self.single_key(Action::ToggleUnread),
                    Suggestion::Star => self.single_key(Action::ToggleStar),
                    Suggestion::Label(_) => self.free_key('l'),
                    Suggestion::Snooze(_) => self.free_key('Z'),
                }
                .filter(|key| used.insert(*key));
                (item.position, preferred)
            })
            .collect();
        for (_, key) in &mut keys {
            if key.is_none() {
                *key = "LbBwWDYHpPTI"
                    .chars()
                    .filter_map(|ch| self.free_key(ch))
                    .find(|candidate| used.insert(*candidate));
            }
        }
        keys
    }

    fn single_key(&self, action: Action) -> Option<KeyId> {
        self.keymap
            .bindings
            .iter()
            .find_map(|binding| match &binding.keys[..] {
                [key @ KeyId::Char(ch)]
                    if *key != self.keymap.leader
                        && !ch.is_ascii_digit()
                        && binding.action == action =>
                {
                    Some(*key)
                }
                _ => None,
            })
    }

    fn free_key(&self, ch: char) -> Option<KeyId> {
        let key = KeyId::Char(ch);
        (key != self.keymap.leader && matches!(match_seq(&self.keymap, &[key]), SeqMatch::None))
            .then_some(key)
    }

    pub(super) fn note_action(&mut self, action: Action) -> Step {
        if !self.can_act_on_note() || self.note().is_none() {
            self.show_toast("No agent note in this conversation", false);
            return Step::default();
        }
        let mut step = Step::default();
        match action {
            Action::ToggleAgentNote => self.agent_notes.collapsed = !self.agent_notes.collapsed,
            Action::NextAgentNote => {
                self.agent_notes.selected =
                    (self.agent_notes.selected + 1) % self.agent_notes.notes.len();
                self.agent_notes.collapsed = false;
            }
            Action::ScrollAgentNoteDown => step.note_scroll = 1,
            Action::ScrollAgentNoteUp => step.note_scroll = -1,
            Action::DismissAgentNote => step.note = self.note_request(NoteDecision::Dismiss),
            Action::AcceptAgentSuggestion => {
                if let Some(suggestion) = self.note().and_then(|note| note.suggestions.first()) {
                    step.note = self.note_request(NoteDecision::Accept(suggestion.position));
                }
            }
            _ => unreachable!("only note actions reach this handler"),
        }
        step
    }

    pub fn note_request(&self, decision: NoteDecision) -> Option<NoteRequest> {
        let note = self.note()?;
        Some(NoteRequest {
            thread: self.cursor_id.clone()?,
            id: note.id,
            decision,
            reply: note.suggestions.iter().any(|item| {
                decision == NoteDecision::Accept(item.position) && item.action == Suggestion::Reply
            }),
        })
    }
}
