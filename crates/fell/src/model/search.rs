//! Search input and navigation, independent of the window and store actor.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use fell_core::{
    Address, MessageId, SearchField, SearchHit, SearchLocation, SearchMatch, SearchPreview,
};

use super::{Fetch, Pending, Session, Step, Toast};
use crate::keymap::KeyId;
use crate::present::{ConvMessage, NameSpan, Pane, Rich, ThreadRow, display_name, one_line};
use crate::text_render::BodyBlock;

pub const REFRESH_FEEDBACK_DELAY: Duration = Duration::from_millis(300);

#[derive(Clone, Debug)]
pub(super) struct FolderList {
    rows: Vec<ThreadRow>,
    cursor: Option<String>,
    picked: HashSet<String>,
    anchor: Option<String>,
    visual: bool,
    full_width: bool,
    fetch: Option<Fetch>,
    pending: Option<Pending>,
    more_before: bool,
    more_after: bool,
    unread: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Search {
    pub query: String,
    pub caret: usize,
    pub editing: bool,
    pub pending: bool,
    pub error: Option<Toast>,
    pub correction: Option<String>,
    pub suggestions: Vec<Suggestion>,
    pub suggestion_index: usize,
    completion_key: Option<(CompletionField, String)>,
    completion_exclusions: HashSet<String>,
    pub hits: HashMap<String, Hit>,
    requested: String,
    accepted: Option<(u64, String)>,
    dirty: bool,
    pub refresh_started: Option<Instant>,
    /// The notice on screen when the pending query was sent. An error reply
    /// replaces only that notice, never one shown while the query ran.
    toast_at_request: Option<u64>,
    pub matches: Vec<SearchMatch>,
    matches_for: Option<(u64, String)>,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub message: MessageId,
    pub related: bool,
    /// An answered preview can still have no highlighted text.
    pub preview: Option<SearchPreview>,
    pub folder: Option<String>,
}

pub struct SearchReply {
    pub rows: Vec<ThreadRow>,
    pub hits: HashMap<String, Hit>,
    pub correction: Option<String>,
    pub error: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum MatchLanding {
    Best,
    CurrentMatch,
    KeepReader,
}

impl SearchReply {
    pub fn new(
        results: fell_core::Result<fell_core::SearchResults>,
        mut previews: HashMap<MessageId, SearchPreview>,
        mut locations: HashMap<MessageId, SearchLocation>,
        me: &str,
    ) -> Self {
        let (mut rows, mut hits, mut correction, mut error) =
            (Vec::new(), HashMap::new(), None, None);
        match results {
            Ok(results) => {
                rows.reserve(results.hits.len());
                hits.reserve(results.hits.len());
                for SearchHit {
                    thread_id,
                    message_id,
                    subject,
                    sender_email,
                    date_unix,
                    snippet,
                    semantic_only,
                } in results.hits
                {
                    let mut location = locations.remove(&message_id);
                    let sender_name = location.as_mut().and_then(|hit| hit.sender_name.take());
                    rows.push(ThreadRow {
                        id: thread_id.0.clone(),
                        spans: vec![NameSpan {
                            text: one_line(&display_name(&sender_name, &sender_email, me)),
                            emphasis: true,
                        }],
                        extra: 0,
                        message_count: location.as_ref().map_or(1, |hit| hit.message_count),
                        starred: location.as_ref().is_some_and(|hit| hit.starred),
                        has_attachment: location.as_ref().is_some_and(|hit| hit.has_attachment),
                        date_unix,
                        subject: one_line(&subject),
                        snippet: one_line(&snippet),
                        labels: location.as_ref().map_or_else(Vec::new, |hit| {
                            let mut labels = hit.labels.clone();
                            labels.sort();
                            labels.dedup();
                            labels
                        }),
                        warn: false,
                        unread: location.as_ref().is_some_and(|hit| hit.unread),
                        draft: None,
                        server_draft: None,
                    });
                    hits.insert(
                        thread_id.0,
                        Hit {
                            preview: previews.remove(&message_id),
                            message: message_id,
                            related: semantic_only,
                            folder: location.and_then(|hit| hit.folder),
                        },
                    );
                }
                correction = results.did_you_mean;
            }
            Err(fell_core::Error::Search(message)) => error = Some(message),
            Err(_) => error = Some("Could not search this mailbox".into()),
        }
        Self {
            rows,
            hits,
            correction,
            error,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionField {
    Mailbox(SearchField),
    Flag(&'static str),
}

impl CompletionField {
    pub fn is_address(self) -> bool {
        matches!(
            self,
            Self::Mailbox(SearchField::Sender | SearchField::Recipient)
        )
    }
}

#[derive(Clone, Debug)]
pub enum Suggestion {
    Value(String),
    Correspondent(Address),
}

impl Suggestion {
    fn value(&self) -> &str {
        match self {
            Self::Value(value) => value,
            Self::Correspondent(person) => &person.email,
        }
    }
}

pub struct Completion<'a> {
    pub field: CompletionField,
    pub prefix: &'a str,
    value: Range<usize>,
}

/// The operator value around the caret. Quotes can contain spaces, and a
/// colon inside quoted text is ordinary text.
pub fn completion(query: &str, caret: usize) -> Option<Completion<'_>> {
    let mut start = 0;
    let mut quoted = false;
    let mut end = query.len();
    for (index, ch) in query.char_indices() {
        if ch == '"' {
            quoted = !quoted;
        } else if ch.is_ascii_whitespace() && !quoted {
            if index >= caret {
                end = index;
                break;
            }
            start = index + ch.len_utf8();
        }
    }
    let (key, _) = query.get(start..end)?.split_once(':')?;
    let field = match key.to_ascii_lowercase().as_str() {
        "in" => CompletionField::Mailbox(SearchField::Folder),
        "label" => CompletionField::Mailbox(SearchField::Label),
        "from" => CompletionField::Mailbox(SearchField::Sender),
        "to" => CompletionField::Mailbox(SearchField::Recipient),
        "has" => CompletionField::Flag("attachment"),
        "is" => CompletionField::Flag("unread"),
        _ => return None,
    };
    let value_start = start + key.len() + 1;
    let prefix = query.get(value_start..caret)?.trim_matches('"');
    Some(Completion {
        field,
        prefix,
        value: value_start..end,
    })
}

impl Search {
    pub fn request(&self) -> &str {
        &self.requested
    }

    pub fn accepted_generation(&self) -> Option<u64> {
        self.accepted.as_ref().map(|(generation, _)| *generation)
    }

    pub fn mailbox_changed(&self) -> bool {
        self.dirty
    }

    pub fn results_current(&self) -> bool {
        !self.pending && self.error.is_none()
    }

    pub fn show_pending_feedback(&self, now: Instant) -> bool {
        self.pending
            && self
                .refresh_started
                .is_none_or(|start| now.duration_since(start) >= REFRESH_FEEDBACK_DELAY)
    }

    pub fn effective_query(&self) -> &str {
        self.correction
            .as_deref()
            .or_else(|| self.accepted.as_ref().map(|(_, query)| query.as_str()))
            .unwrap_or("")
    }

    pub fn matches_thread(&self, thread: Option<&str>) -> bool {
        self.results_current()
            && self.matches_for.as_ref().is_some_and(|(generation, id)| {
                self.accepted_generation() == Some(*generation) && Some(id.as_str()) == thread
            })
    }

    pub fn completion_key(&self) -> Option<&(CompletionField, String)> {
        self.completion_key.as_ref()
    }

    pub fn completion_exclusions(&self) -> &HashSet<String> {
        &self.completion_exclusions
    }
}

fn address_filters(query: &str, operator: &str) -> HashSet<String> {
    let mut quoted = false;
    query
        .split(|ch: char| {
            if ch == '"' {
                quoted = !quoted;
            }
            !quoted && ch.is_ascii_whitespace()
        })
        .filter(|token| !token.is_empty())
        .take(crate::recipient_menu::EXCLUSION_LIMIT)
        .filter_map(|token| {
            let (field, value) = token.split_once(':')?;
            if !field.eq_ignore_ascii_case(operator) {
                return None;
            }
            fell_core::compose::email_of(value.trim_matches('"')).map(|email| email.to_lowercase())
        })
        .collect()
}

/// Whitespace outside quoted phrases separates search tokens. Keep bytes
/// inside quotes, including escaped quotes, exactly as the reader typed them.
fn query_key(input: &str) -> String {
    let mut key = String::with_capacity(input.len());
    let mut quoted = false;
    let mut separator = false;
    for ch in input.trim().chars() {
        if !quoted && ch.is_ascii_whitespace() {
            separator = true;
            continue;
        }
        if std::mem::take(&mut separator) {
            key.push(' ');
        }
        key.push(ch);
        if ch == '"' {
            quoted = !quoted;
        }
    }
    key
}

impl Session {
    pub(super) fn start_search(&mut self, query: Option<String>) -> Step {
        self.withdraw_purge();
        if self.search.is_none() {
            self.folder_list = Some(std::sync::Arc::new(FolderList {
                rows: std::mem::take(&mut self.threads),
                cursor: self.cursor_id.take(),
                picked: std::mem::take(&mut self.picked),
                anchor: self.anchor_id.take(),
                visual: self.visual,
                full_width: self.full_width,
                fetch: self.fetch.take(),
                pending: self.pending.take(),
                more_before: self.more_before,
                more_after: self.more_after,
                unread: self.unread_count,
            }));
            self.visual = false;
            self.more_before = false;
            self.more_after = false;
            self.search = Some(Search::default());
        }
        self.count.clear();
        self.pane = Pane::List;
        self.full_width = false;
        let search = self.search.as_mut().unwrap();
        search.editing = true;
        if let Some(query) = query {
            search.caret = query.len();
            search.query = query;
            self.search_changed()
        } else {
            Step::default()
        }
    }

    pub(super) fn end_search(&mut self) -> bool {
        let Some(folder) = self.folder_list.take() else {
            return false;
        };
        let folder = std::sync::Arc::unwrap_or_clone(folder);
        self.search = None;
        self.search_generation = self.search_generation.wrapping_add(1);
        self.threads = folder.rows;
        self.cursor_id = folder.cursor;
        self.picked = folder.picked;
        self.anchor_id = folder.anchor;
        self.visual = folder.visual;
        self.full_width = folder.full_width;
        self.fetch = folder.fetch;
        self.pending = folder.pending;
        self.more_before = folder.more_before;
        self.more_after = folder.more_after;
        self.unread_count = folder.unread;
        self.pane = Pane::List;
        true
    }

    pub fn type_search(&mut self, text: &str) -> Step {
        let Some(search) = self.search.as_mut().filter(|search| search.editing) else {
            return Step::default();
        };
        let line;
        let typed = if text.chars().any(char::is_control) {
            line = text.split_whitespace().collect::<Vec<_>>().join(" ");
            &line
        } else {
            text
        };
        if typed.is_empty() {
            return Step::default();
        }
        search.query.insert_str(search.caret, typed);
        search.caret += typed.len();
        self.search_changed()
    }

    pub(crate) fn search_key(&mut self, key: KeyId) -> Step {
        let search = self.search.as_mut().unwrap();
        match key {
            KeyId::Escape => {
                self.end_search();
                return Step::default();
            }
            KeyId::Enter
                if matches!(
                    search.suggestions.get(search.suggestion_index),
                    Some(Suggestion::Correspondent(_))
                ) =>
            {
                return self.accept_search_suggestion();
            }
            KeyId::Enter => {
                search.editing = false;
                // Remember a submitted query, including one whose response
                // is still in flight. Tabbed corrections replace it first.
                self.remember_search();
                return Step::default();
            }
            KeyId::Tab => return self.accept_search_suggestion(),
            KeyId::Up | KeyId::Down | KeyId::Ctrl('n' | 'p')
                if !search.suggestions.is_empty()
                    && (matches!(key, KeyId::Up | KeyId::Down)
                        || matches!(
                            search.suggestions.first(),
                            Some(Suggestion::Correspondent(_))
                        )) =>
            {
                let len = search.suggestions.len();
                let step = if matches!(key, KeyId::Down | KeyId::Ctrl('n')) {
                    1
                } else {
                    len - 1
                };
                search.suggestion_index = (search.suggestion_index + step) % len;
                return Step::default();
            }
            KeyId::Left | KeyId::Right | KeyId::Home | KeyId::End => {
                search.caret = match key {
                    KeyId::Left => previous(&search.query, search.caret),
                    KeyId::Right => next(&search.query, search.caret),
                    KeyId::Home => 0,
                    _ => search.query.len(),
                };
                return self.update_search_completion();
            }
            KeyId::Backspace if search.caret > 0 => {
                let start = previous(&search.query, search.caret);
                search.query.replace_range(start..search.caret, "");
                search.caret = start;
            }
            KeyId::Delete if search.caret < search.query.len() => {
                let end = next(&search.query, search.caret);
                search.query.replace_range(search.caret..end, "");
            }
            KeyId::Ctrl('u') => {
                search.query.clear();
                search.caret = 0;
            }
            KeyId::Char(ch) => return self.type_search(ch.encode_utf8(&mut [0; 4])),
            KeyId::Space => return self.type_search(" "),
            _ => return Step::default(),
        }
        self.search_changed()
    }

    fn search_changed(&mut self) -> Step {
        let search = self.search.as_mut().unwrap();
        let query = query_key(&search.query);
        if query != search.requested {
            self.search_generation = self.search_generation.wrapping_add(1);
            search.toast_at_request = Some(self.toast_generation);
            search.requested = query;
            search.error = None;
            search.refresh_started = None;
            search.pending = !search.requested.is_empty()
                && (search.dirty
                    || search
                        .accepted
                        .as_ref()
                        .is_none_or(|(_, accepted)| *accepted != search.requested));
            if search.requested.is_empty() {
                search.dirty = false;
                search.correction = None;
                search.accepted = None;
                search.hits.clear();
                search.matches.clear();
                search.matches_for = None;
                self.threads.clear();
                self.cursor_id = None;
                self.picked.clear();
                self.anchor_id = None;
                self.visual = false;
            }
            let mut step = self.update_search_completion();
            step.search_request = self.search.as_ref().unwrap().pending;
            return step;
        }
        self.update_search_completion()
    }

    pub fn refresh_search(&mut self, now: Instant) -> bool {
        let Some(search) = self
            .search
            .as_mut()
            .filter(|search| !search.requested.is_empty())
        else {
            return false;
        };
        self.search_generation = self.search_generation.wrapping_add(1);
        search.toast_at_request = Some(self.toast_generation);
        // Keep the count for a short same-query refresh. A second request
        // after a mailbox write inherits the time spent waiting for it.
        if search.error.is_none()
            && !search.hits.is_empty()
            && search
                .accepted
                .as_ref()
                .is_some_and(|(_, accepted)| *accepted == search.requested)
        {
            search.refresh_started.get_or_insert(now);
        }
        search.pending = true;
        search.dirty = true;
        true
    }

    pub fn search_results_current(&self) -> bool {
        self.search.as_ref().is_none_or(Search::results_current)
    }

    pub fn search_source(&self, id: &str) -> Option<&str> {
        match &self.search {
            Some(search) if search.results_current() => search.hits.get(id)?.folder.as_deref(),
            Some(_) => None,
            None => Some(&self.folder),
        }
    }

    pub fn search_scopes(&self, ids: &[String]) -> Option<Vec<(fell_core::ThreadId, String)>> {
        let scopes: Vec<_> = ids
            .iter()
            .filter_map(|id| {
                Some((
                    fell_core::ThreadId(id.clone()),
                    self.search_source(id)?.to_owned(),
                ))
            })
            .collect();
        (!scopes.is_empty()).then_some(scopes)
    }

    pub fn saved_folder_span(&self) -> Option<fell_core::FolderSpan> {
        let saved = self.folder_list.as_ref()?;
        let mut rows = saved.rows.iter().filter(|row| row.draft.is_none());
        let first = rows.next();
        let last = rows.next_back().or(first);
        Some(match last {
            Some(last) => fell_core::FolderSpan::Window {
                first: first.filter(|_| saved.more_before).map(super::row_key),
                last: super::row_key(last),
                before: 0,
                after: self.margin,
            },
            None => fell_core::FolderSpan::Top {
                rows: self.load_rows,
            },
        })
    }

    pub fn refresh_saved_folder(
        &mut self,
        threads: Vec<ThreadRow>,
        page: &fell_core::FolderPage,
        select: Option<&str>,
    ) {
        let Some(saved) = self.folder_list.as_mut() else {
            return;
        };
        let saved = std::sync::Arc::make_mut(saved);
        let old_index = saved
            .rows
            .iter()
            .position(|row| Some(&row.id) == saved.cursor.as_ref())
            .unwrap_or(0);
        saved.rows = threads;
        let present = |id: &str| saved.rows.iter().any(|row| row.id == id);
        saved.cursor = select
            .filter(|id| present(id))
            .map(str::to_owned)
            .or_else(|| saved.cursor.take().filter(|id| present(id)))
            .or_else(|| {
                saved
                    .rows
                    .get(old_index.min(saved.rows.len().saturating_sub(1)))
                    .map(|row| row.id.clone())
            });
        saved.picked.retain(|id| present(id));
        if saved.anchor.as_ref().is_some_and(|id| !present(id)) {
            saved.anchor = None;
            saved.visual = false;
        }
        saved.more_before = page.more_before;
        saved.more_after = page.more_after;
        saved.unread = page.unread;
    }

    fn update_search_completion(&mut self) -> Step {
        let search = self.search.as_mut().unwrap();
        let key = completion(&search.query, search.caret).map(|c| (c.field, c.prefix.to_owned()));
        let address_operator = match key.as_ref().map(|(field, _)| field) {
            Some(CompletionField::Mailbox(SearchField::Sender)) => Some("from"),
            Some(CompletionField::Mailbox(SearchField::Recipient)) => Some("to"),
            _ => None,
        };
        let excluded = if let Some((_, prefix)) = &key
            && let Some(operator) = address_operator
        {
            let mut excluded = address_filters(&search.query, operator);
            if prefix.contains('@') {
                excluded.insert(prefix.to_lowercase());
            }
            excluded
        } else {
            HashSet::new()
        };
        if key == search.completion_key && excluded == search.completion_exclusions {
            return Step::default();
        }
        search.completion_key = key;
        search.completion_exclusions = excluded;
        search.suggestions.clear();
        Step {
            search_completion: true,
            ..Step::default()
        }
    }

    pub fn accept_search_suggestion(&mut self) -> Step {
        let Some(search) = self.search.as_mut() else {
            return Step::default();
        };
        if !matches!(search.suggestions.get(search.suggestion_index),
            Some(Suggestion::Correspondent(_)) if search.editing)
            && search.results_current()
            && let Some(corrected) = search.correction.take()
        {
            // The store has already run this correction and supplied its hits.
            if let Some((_, accepted)) = search.accepted.as_mut() {
                *accepted = query_key(&corrected);
            }
            search.query = corrected;
            search.caret = search.query.len();
        } else if search.editing
            && let Some(suggestion) = search.suggestions.get(search.suggestion_index)
            && let Some(completion) = completion(&search.query, search.caret)
        {
            let suggestion = suggestion.value();
            let value = if suggestion
                .chars()
                .any(|ch| ch.is_ascii_whitespace() || ch == '"')
            {
                format!("\"{}\"", suggestion.replace('"', "\"\""))
            } else {
                suggestion.to_owned()
            };
            let end = completion.value.start + value.len();
            search.query.replace_range(completion.value, &value);
            if end == search.query.len() {
                search.query.push(' ');
                search.caret = end + 1;
            } else {
                search.caret = end;
            }
        } else {
            return Step::default();
        }
        let submitted = !search.editing;
        let step = self.search_changed();
        if submitted {
            self.remember_search();
        }
        step
    }

    /// Swap out the old data so the view can release large result sets off
    /// the UI thread. A rejected reply is left intact for the same reason.
    pub fn apply_search_reply(&mut self, generation: u64, reply: &mut SearchReply) -> bool {
        if self.search_generation != generation
            || self.search.as_ref().is_none_or(|search| !search.pending)
        {
            return false;
        }
        if let Some(error) = reply.error.take() {
            // An invalid query has no result set of its own. Keep the last
            // valid rows, cursor and reader while presenting the error. It
            // replaces the notice that was up when the query was sent; a
            // newer one (an undo, a sync) stays until its own timer ends.
            if let Some(generation) = self.search.as_ref().unwrap().toast_at_request {
                self.clear_toast_if(generation);
            }
            let search = self.search.as_mut().unwrap();
            self.deferred_read = None;
            search.pending = false;
            search.refresh_started = None;
            search.error = Some(Toast {
                text: one_line(&error),
                alert: true,
                hint: None,
                label: None,
            });
            return true;
        }
        if self.visual {
            self.set_threads(std::mem::take(&mut reply.rows));
        } else {
            std::mem::swap(&mut self.threads, &mut reply.rows);
            if self
                .cursor_id
                .as_ref()
                .is_none_or(|id| !reply.hits.contains_key(id))
            {
                self.cursor_id = self.threads.first().map(|row| row.id.clone());
            }
            self.picked.retain(|id| reply.hits.contains_key(id));
        }
        if self.threads.is_empty() {
            self.pane = Pane::List;
            self.full_width = false;
        }
        let search = self.search.as_mut().unwrap();
        search.pending = false;
        search.refresh_started = None;
        search.dirty = false;
        search.error = None;
        search.correction = reply.correction.take();
        std::mem::swap(&mut search.hits, &mut reply.hits);
        search.accepted = Some((generation, search.requested.clone()));
        true
    }

    pub fn defer_search_reply(&self, generation: u64, reply: &SearchReply) -> bool {
        self.search_generation == generation
            && self
                .search
                .as_ref()
                .is_some_and(|search| search.pending && search.editing)
            && reply.rows.is_empty()
    }

    pub fn remember_search(&mut self) {
        if let Some(query) = self
            .search
            .as_ref()
            .map(|s| s.query.trim())
            .filter(|query| !query.is_empty())
        {
            self.recent_searches.retain(|old| old != query);
            self.recent_searches.insert(0, query.to_string());
            self.recent_searches.truncate(6);
        }
    }

    pub fn set_search_snippet(
        &mut self,
        generation: u64,
        thread: &str,
        message: &MessageId,
        snippet: &str,
        preview: fell_core::Result<Option<SearchPreview>>,
    ) -> bool {
        let Some(hit) = self
            .search
            .as_mut()
            .filter(|search| search.accepted_generation() == Some(generation))
            .and_then(|search| search.hits.get_mut(thread))
            .filter(|hit| &hit.message == message)
        else {
            return false;
        };
        let Some(row) = self.threads.iter_mut().find(|row| row.id == thread) else {
            return false;
        };
        let snippet = one_line(snippet);
        let mut changed = row.snippet != snippet;
        row.snippet = snippet;
        if let Ok(preview) = preview {
            changed |= hit.preview != preview;
            hit.preview = preview;
        }
        changed
    }

    pub fn set_search_matches(
        &mut self,
        generation: u64,
        thread: &str,
        matches: Vec<SearchMatch>,
        landing: MatchLanding,
    ) -> bool {
        let Some(search) = self
            .search
            .as_mut()
            .filter(|s| s.accepted_generation() == Some(generation))
        else {
            return false;
        };
        let Some(hit) = search.hits.get(thread) else {
            return false;
        };
        search.matches_for = Some((generation, thread.to_owned()));
        search.matches =
            if matches.is_empty() && (landing != MatchLanding::KeepReader || hit.related) {
                vec![SearchMatch {
                    message: hit.message.clone(),
                    preview: None,
                }]
            } else {
                matches
            };
        let current_matches = self
            .messages
            .get(self.message_cursor)
            .is_some_and(|message| search.matches.iter().any(|hit| hit.message.0 == message.id));
        if landing == MatchLanding::KeepReader
            || (landing == MatchLanding::CurrentMatch && current_matches)
        {
            return self.reveal_matching_quote(self.message_cursor);
        }
        if let Some(index) = self
            .messages
            .iter()
            .position(|message| message.id == hit.message.0)
        {
            self.message_cursor = index;
            self.messages[index].expanded = true;
            self.reveal_matching_quote(index);
            return true;
        }
        false
    }

    /// The selected result may live in quoted history rather than the reply.
    pub fn reveal_matching_quote(&mut self, index: usize) -> bool {
        let Some(search) = self
            .search
            .as_ref()
            .filter(|s| s.matches_thread(self.cursor_id.as_deref()))
        else {
            return false;
        };
        let Some(message) = self.messages.get(index) else {
            return false;
        };
        if self.quotes_open.contains(&message.id) {
            return false;
        }
        let preview = search
            .matches
            .iter()
            .find(|hit| hit.message.0 == message.id)
            .and_then(|hit| hit.preview.as_ref());
        preview.is_some_and(|preview| {
            folded_match(message, preview) && self.quotes_open.insert(message.id.clone())
        })
    }

    pub(super) fn move_search_hit(&mut self, forward: bool, count: usize) -> Step {
        let Some(search) = self.search.as_ref().filter(|search| {
            self.pane == Pane::Conversation && search.matches_thread(self.cursor_id.as_deref())
        }) else {
            return Step::default();
        };
        let hits: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| search.matches.iter().any(|hit| hit.message.0 == message.id))
            .map(|(index, _)| index)
            .collect();
        let len = hits.len();
        if len == 0 {
            return Step::default();
        }
        let index = if forward {
            (hits.partition_point(|index| *index <= self.message_cursor) % len + (count - 1) % len)
                % len
        } else {
            (hits.partition_point(|index| *index < self.message_cursor) % len + len
                - (count - 1) % len
                - 1)
                % len
        };
        self.leave_attachments();
        self.message_cursor = hits[index];
        let changed = !self.messages[self.message_cursor].expanded;
        self.messages[self.message_cursor].expanded = true;
        let revealed = self.reveal_matching_quote(self.message_cursor);
        Step {
            message_disclosure_changed: changed || revealed,
            reveal_search_hit: true,
            ..Step::default()
        }
    }
}

fn folded_match(message: &ConvMessage, preview: &SearchPreview) -> bool {
    if preview.highlights.is_empty() {
        return false;
    }
    let mut visible = String::new();
    let mut folded = String::new();
    let mut include = |hidden: bool, text: &str| {
        let target = if hidden { &mut folded } else { &mut visible };
        target.push(' ');
        target.push_str(&one_line(text).to_lowercase());
    };
    match &message.rich {
        Rich::Html(html) => {
            for section in &html.folded.sections {
                include(
                    section.fold.is_some(),
                    &fell_render::StyledDocument::blocks_text(html.folded.blocks(section)),
                );
            }
        }
        rich => {
            let body = if let Rich::PlainPart(body) = rich {
                body
            } else {
                &message.body
            };
            for block in &body.blocks {
                match block {
                    BodyBlock::Text(text) => include(false, text),
                    BodyBlock::List(blocks) => {
                        include(false, &fell_render::StyledDocument::blocks_text(blocks))
                    }
                    BodyBlock::Quote(text) | BodyBlock::Signature(text) => include(true, text),
                    BodyBlock::Markdown { .. } => {}
                }
            }
        }
    }
    preview.highlights.iter().any(|range| {
        let word = preview.text[range.clone()].to_lowercase();
        let contains_word = |text: &str| {
            text.match_indices(&word).any(|(at, found)| {
                !text[..at]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_alphanumeric)
                    && !text[at + found.len()..]
                        .chars()
                        .next()
                        .is_some_and(char::is_alphanumeric)
            })
        };
        contains_word(&folded) && !contains_word(&visible)
    })
}

fn previous(query: &str, caret: usize) -> usize {
    query[..caret]
        .char_indices()
        .next_back()
        .map_or(0, |(index, _)| index)
}

fn next(query: &str, caret: usize) -> usize {
    caret + query[caret..].chars().next().map_or(0, char::len_utf8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{Action, Keymap};
    use crate::model::{FlagEdit, SyncKind};

    fn session() -> Session {
        Session::new(
            "Fixture",
            "reader@example.test",
            SyncKind::FixtureOffline,
            Keymap::default(),
        )
    }

    fn row_hit(thread: &str, message: &str) -> SearchHit {
        SearchHit {
            thread_id: fell_core::ThreadId(thread.into()),
            message_id: MessageId(message.into()),
            subject: "Project invoice".into(),
            snippet: "The invoice is ready.".into(),
            sender_email: "ada@example.test".into(),
            date_unix: 1,
            semantic_only: false,
        }
    }

    fn reply(hits: Vec<SearchHit>, correction: Option<&str>) -> SearchReply {
        let locations = hits
            .iter()
            .map(|hit| {
                let folder = if hit.thread_id.0 == "two" {
                    "Sent"
                } else {
                    "INBOX"
                };
                (
                    hit.message_id.clone(),
                    SearchLocation {
                        folder: Some(folder.into()),
                        unread: false,
                        starred: false,
                        sender_name: Some("Ada".into()),
                        message_count: 3,
                        labels: vec![],
                        has_attachment: true,
                    },
                )
            })
            .collect();
        SearchReply::new(
            Ok(fell_core::SearchResults {
                hits,
                did_you_mean: correction.map(str::to_owned),
            }),
            HashMap::new(),
            locations,
            "reader@example.test",
        )
    }

    fn land(session: &mut Session, mut reply: SearchReply) {
        assert!(session.apply_search_reply(session.search_generation, &mut reply));
    }

    #[test]
    fn slash_types_and_enter_focuses_results_even_while_pending() {
        let mut session = session();
        session.on_key(KeyId::Char('/'), 10);
        assert_eq!(session.mode_label(), "insert");
        for ch in "invoice".chars() {
            assert!(session.on_key(KeyId::Char(ch), 10).search_request);
        }
        assert!(session.search.as_ref().unwrap().pending);
        session.on_key(KeyId::Enter, 10);
        assert_eq!(session.mode_label(), "normal");
        land(
            &mut session,
            reply(vec![row_hit("one", "m1"), row_hit("two", "m2")], None),
        );
        assert_eq!(session.pane, Pane::List);
        assert_eq!(session.cursor_id.as_deref(), Some("one"));
        assert_eq!(session.threads[0].spans[0].text, "Ada");
        assert_eq!(session.threads[0].message_count, 3);
        assert!(session.threads[0].has_attachment);
        session.on_key(KeyId::Char('j'), 10);
        assert_eq!(session.cursor_id.as_deref(), Some("two"));
        session.on_key(KeyId::Enter, 10);
        assert_eq!(session.pane, Pane::Conversation);
    }

    #[test]
    fn fetched_html_updates_only_its_search_row() {
        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let source = &store.search(&account.id, "hall").unwrap().hits[0].message_id;
        let mut mail = store.message(&account.id, source).unwrap().unwrap();
        let id = MessageId::new("dispatch@fell.test");
        mail.headers.message_id = id.clone();
        mail.headers.proton_conversation_id = None;
        mail.headers.in_reply_to = None;
        mail.headers.references.clear();
        mail.headers.subject = "Field dispatch".into();
        mail.remote_id = fell_core::RemoteId("fixture:dispatch".into());
        mail.thread_id = fell_core::ThreadId::from_message_id(&id);
        mail.snippet = "HeadingThe invoice is ready.".into();
        mail.body_text = None;
        mail.parts = None;
        store
            .apply_delta(
                &account.id,
                &fell_core::MailboxDelta {
                    messages: vec![mail],
                    ..Default::default()
                },
            )
            .unwrap();
        let hit = store
            .search(&account.id, "invoice")
            .unwrap()
            .hits
            .into_iter()
            .find(|hit| hit.message_id == id)
            .unwrap();
        let thread = hit.thread_id.0.clone();
        let read_preview = || {
            store
                .search_previews(&account.id, "invoice", std::slice::from_ref(&id))
                .unwrap()
                .remove(&id)
        };

        let mut session = session();
        session.perform(Action::Search, 10);
        session.type_search("invoice");
        let mut initial = reply(vec![row_hit("other", "m2"), hit], None);
        initial.hits.get_mut(&thread).unwrap().preview = read_preview();
        land(&mut session, initial);
        session.select_if_present(&thread);
        session.picked.insert("other".into());
        let generation = session.search_generation;
        let other = session.threads[0].clone();
        assert_eq!(session.threads[1].snippet, "HeadingThe invoice is ready.");

        store
            .store_fetched_parts(
                &account.id,
                &[(
                    id.clone(),
                    Some(fell_core::MessageParts {
                        html: Some("<h1>Heading</h1><p>The invoice is ready.</p>".into()),
                        ..Default::default()
                    }),
                )],
            )
            .unwrap();
        let snippet = store.message_snippet(&account.id, &id).unwrap().unwrap();
        assert!(session.set_search_snippet(
            generation,
            &thread,
            &id,
            &snippet,
            Err(fell_core::Error::Search("preview unavailable".into())),
        ));
        assert_eq!(session.threads[1].snippet, "Heading The invoice is ready.");
        assert_eq!(
            session.search.as_ref().unwrap().hits[&thread]
                .preview
                .as_ref()
                .unwrap()
                .text,
            "HeadingThe invoice is ready."
        );
        assert!(session.set_search_snippet(generation, &thread, &id, &snippet, Ok(read_preview())));
        assert_eq!(session.threads[0], other);
        assert_eq!(session.threads[1].snippet, "Heading The invoice is ready.");
        let search = session.search.as_ref().unwrap();
        assert_eq!(search.query, "invoice");
        assert_eq!(search.accepted_generation(), Some(generation));
        assert_eq!(
            search.hits[&thread].preview.as_ref().unwrap().text,
            "Heading The invoice is ready."
        );
        assert_eq!(session.cursor_id.as_deref(), Some(thread.as_str()));
        assert!(session.picked.contains("other"));
        assert!(!session.set_search_snippet(
            generation.wrapping_sub(1),
            &thread,
            &id,
            "stale",
            Ok(None)
        ));
    }

    #[test]
    fn empty_prefix_stays_pending_until_settled_or_submitted() {
        let mut session = session();
        session.on_key(KeyId::Char('/'), 10);
        session.type_search("hall");
        land(&mut session, reply(vec![row_hit("one", "m1")], None));

        session.type_search(" v");
        let prefix = session.search_generation;
        let mut empty = reply(Vec::new(), None);
        assert!(session.defer_search_reply(prefix, &empty));
        assert_eq!(session.cursor_id.as_deref(), Some("one"));
        assert!(!session.search_results_current());

        session.type_search("enue");
        assert!(!session.apply_search_reply(prefix, &mut empty));
        assert_eq!(session.threads.len(), 1);
        assert!(session.defer_search_reply(session.search_generation, &empty));

        session.on_key(KeyId::Enter, 10);
        assert!(!session.defer_search_reply(session.search_generation, &empty));
        land(&mut session, empty);
        assert!(session.threads.is_empty());
        assert!(session.search_results_current());
    }

    #[test]
    fn a_same_query_refresh_keeps_the_count_for_300_ms() {
        let mut session = session();
        session.on_key(KeyId::Char('/'), 10);
        session.type_search("hall");
        let start = Instant::now();
        let pending_feedback = |session: &Session, ms| {
            session
                .search
                .as_ref()
                .unwrap()
                .show_pending_feedback(start + Duration::from_millis(ms))
        };
        assert!(pending_feedback(&session, 0));
        land(&mut session, reply(vec![row_hit("one", "m1")], None));

        assert!(session.refresh_search(start));
        assert!(!session.search_results_current());
        assert!(!pending_feedback(&session, 0));
        assert!(session.refresh_search(start + Duration::from_millis(100)));
        assert!(!pending_feedback(&session, 299));
        assert!(pending_feedback(&session, 300));

        session.type_search(" venue");
        assert!(pending_feedback(&session, 150));
        land(&mut session, reply(vec![row_hit("two", "m2")], None));
        assert!(!pending_feedback(&session, 300));
    }

    #[test]
    fn read_dwell_waits_for_search_to_confirm_the_open_folder() {
        for (folder, mark_read) in [("INBOX", true), ("Archive", false)] {
            let mut session = session();
            session.read_dwell_ms = 800;
            session.on_key(KeyId::Char('/'), 10);
            session.type_search("invoice");
            let unread_hit = || {
                let mut reply = reply(vec![row_hit("one", "m1")], None);
                reply.rows[0].unread = true;
                reply
            };
            land(&mut session, unread_hit());
            session.on_key(KeyId::Enter, 10);
            let dwell = session.on_key(KeyId::Enter, 10).read_after.unwrap();
            assert!(session.on_key(KeyId::Char('s'), 10).flags.is_some());
            assert!(session.refresh_search(Instant::now()));

            assert_eq!(session.read_after_dwell(dwell), None);
            assert!(session.threads[0].unread);

            let mut updated = unread_hit();
            updated.hits.get_mut("one").unwrap().folder = Some(folder.into());
            land(&mut session, updated);
            assert_eq!(
                session.resume_read_after_search(),
                mark_read.then(|| FlagEdit {
                    ids: vec!["one".into()],
                    change: fell_core::FlagChange::Read,
                })
            );
            assert_eq!(session.threads[0].unread, !mark_read);
            assert!(session.resume_read_after_search().is_none());
        }
    }

    #[test]
    fn incomplete_syntax_keeps_previous_results_until_the_query_settles() {
        let mut session = session();
        session.on_key(KeyId::Char('/'), 10);
        session.type_search("hall");
        land(&mut session, reply(vec![row_hit("one", "m1")], None));

        session.type_search(" after:2026");
        let prefix = session.search_generation;
        let invalid_reply = |message: &str| {
            SearchReply::new(
                Err(fell_core::Error::Search(message.into())),
                HashMap::new(),
                HashMap::new(),
                "reader@example.test",
            )
        };
        let invalid = invalid_reply("expected YYYY-MM-DD");
        assert!(session.defer_search_reply(prefix, &invalid));
        assert_eq!(session.cursor_id.as_deref(), Some("one"));
        assert!(session.search.as_ref().unwrap().error.is_none());
        assert!(!session.search_results_current());

        land(&mut session, invalid);
        assert_eq!(session.threads.len(), 1);
        assert_eq!(session.cursor_id.as_deref(), Some("one"));
        assert_eq!(
            session.status_notice().map(|notice| notice.text.as_str()),
            Some("expected YYYY-MM-DD")
        );
        assert!(!session.search_results_current());
        assert_eq!(session.perform(Action::Archive, 10), Step::default());

        session.type_search("-11-11");
        let mut invalid = invalid_reply("expected YYYY-MM-DD");
        assert!(!session.apply_search_reply(prefix, &mut invalid));
        assert_eq!(session.threads.len(), 1);
        assert!(session.status_notice().is_none());
        land(&mut session, reply(vec![row_hit("one", "m1")], None));
        assert_eq!(session.cursor_id.as_deref(), Some("one"));

        session.type_search(" \"unfinished");
        let invalid = invalid_reply("unterminated quote");
        session.on_key(KeyId::Enter, 10);
        assert!(!session.defer_search_reply(session.search_generation, &invalid));
        land(&mut session, invalid);
        assert_eq!(
            session.status_notice().map(|notice| notice.text.as_str()),
            Some("unterminated quote")
        );
        assert_eq!(session.threads.len(), 1);
        assert!(!session.search_results_current());
        session.on_key(KeyId::Escape, 10);
        assert!(session.status_notice().is_none());
    }

    #[test]
    fn a_search_error_replaces_a_label_notice_that_survived_typing() {
        let mut session = session();
        session.show_label_toast(crate::present::LabelNotice {
            verb: "Removed",
            label: "travel".into(),
            relation: "from",
            subject: None,
            count: 1,
            undo: true,
        });
        let removal = session.toast_generation();

        session.on_key(KeyId::Char('/'), 10);
        session.dismiss_key_notice(removal, false);
        session.type_search("after:not-a-date");
        session.on_key(KeyId::Enter, 10);
        session.dismiss_key_notice(removal, false);
        assert!(session.status_notice().unwrap().label.is_some());

        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        let result = store.search(&account.id, "after:not-a-date");
        land(
            &mut session,
            SearchReply::new(
                result,
                HashMap::new(),
                HashMap::new(),
                "reader@example.test",
            ),
        );
        let notice = session.status_notice().unwrap();
        assert_eq!(notice.text, "expected YYYY-MM-DD, got not-a-date");
        assert!(notice.alert);
        session.on_key(KeyId::Escape, 10);
        assert!(session.status_notice().is_none());
    }

    #[test]
    fn a_search_error_keeps_a_notice_shown_while_the_query_ran() {
        let mut session = session();
        session.on_key(KeyId::Char('/'), 10);
        session.type_search("after:not-a-date");
        session.on_key(KeyId::Enter, 10);
        // An undo lands while the query is still pending.
        session.show_toast("Undone", false);
        let undone = session.toast_generation();

        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        let result = store.search(&account.id, "after:not-a-date");
        land(
            &mut session,
            SearchReply::new(
                result,
                HashMap::new(),
                HashMap::new(),
                "reader@example.test",
            ),
        );
        assert_eq!(session.status_notice().unwrap().text, "Undone");

        assert!(session.clear_toast_if(undone));
        assert_eq!(
            session.status_notice().unwrap().text,
            "expected YYYY-MM-DD, got not-a-date"
        );
    }

    #[test]
    fn tab_replaces_the_active_operator_value_and_quotes_spaces() {
        let mut session = session();
        session.on_key(KeyId::Char('/'), 10);
        session.type_search("invoice in:Ar");
        let active = session.search.as_ref().unwrap();
        let value = completion(&active.query, active.caret).unwrap();
        assert!(matches!(
            value.field,
            CompletionField::Mailbox(SearchField::Folder)
        ));
        assert_eq!(value.prefix, "Ar");
        land(&mut session, reply(vec![], None));
        session.search.as_mut().unwrap().suggestions = vec![
            Suggestion::Value("Archive".into()),
            Suggestion::Value("Archive 2025".into()),
        ];
        session.on_key(KeyId::Down, 10);
        assert!(session.on_key(KeyId::Tab, 10).search_request);
        assert_eq!(
            session.search.as_ref().unwrap().query,
            r#"invoice in:"Archive 2025" "#
        );
        assert!(completion(r#""note from:ada""#, 13).is_none());
    }

    #[test]
    fn address_completion_tracks_filters_elsewhere_in_the_query() {
        let mut session = session();
        session.on_key(KeyId::Char('/'), 10);
        session.type_search(
            r#"FROM:"RUBEN@EXAMPLE.TEST" subject:"to:ignored@example.test" TO:"RUTH@EXAMPLE.TEST" to:ru"#,
        );
        let search = session.search.as_ref().unwrap();
        assert_eq!(
            search.completion_exclusions(),
            &HashSet::from(["ruth@example.test".into()])
        );
        let search = session.search.as_mut().unwrap();
        search.query.push_str(" from:rose@example.test");
        assert!(!session.update_search_completion().search_completion);
        let search = session.search.as_mut().unwrap();
        search.query.push_str(" to:rose@example.test");
        assert!(session.update_search_completion().search_completion);
        assert_eq!(
            session.search.as_ref().unwrap().completion_exclusions(),
            &HashSet::from(["rose@example.test".into(), "ruth@example.test".into()])
        );
    }

    #[test]
    fn address_filter_exclusions_stop_after_the_token_budget() {
        let query: String = (0..5_000)
            .map(|i| format!("from:alex{i}@example.test "))
            .collect();
        let excluded = address_filters(&query, "from");
        assert_eq!(excluded.len(), crate::recipient_menu::EXCLUSION_LIMIT);
        assert!(excluded.contains("alex199@example.test"));
        assert!(!excluded.contains("alex200@example.test"));
    }

    #[test]
    fn correction_previews_hits_and_tab_submits_the_corrected_query() {
        let mut session = session();
        session.on_key(KeyId::Char('/'), 10);
        session.type_search("invioce");
        land(
            &mut session,
            reply(vec![row_hit("one", "m1")], Some("invoice")),
        );
        assert_eq!(session.threads.len(), 1);
        assert_eq!(
            session.search.as_ref().unwrap().effective_query(),
            "invoice"
        );
        session.on_key(KeyId::Enter, 10);
        assert!(!session.on_key(KeyId::Tab, 10).search_request);
        assert_eq!(session.threads.len(), 1);
        assert_eq!(session.search.as_ref().unwrap().query, "invoice");
        assert!(!session.search_editing());
        session.on_key(KeyId::Escape, 10);
        session.on_key(KeyId::Char(':'), 10);
        assert_eq!(session.palette()[0].search.as_deref(), Some("invoice"));
        assert!(session.on_key(KeyId::Enter, 10).search_request);
        assert_eq!(session.search.as_ref().unwrap().query, "invoice");
        assert!(!session.search_editing());
    }

    #[test]
    fn escape_restores_the_folder_window_after_stale_results() {
        let mut session = session();
        session.folder = "Archive".into();
        let initial = reply(vec![row_hit("a", "m1"), row_hit("b", "m2")], None);
        session.set_threads(initial.rows);
        session.select_if_present("b");
        session.more_after = true;
        session.picked.insert("a".into());
        session.perform(Action::Search, 10);
        session.type_search("old");
        let old = session.search_generation;
        session.type_search("er");
        let recent = session.search_generation;
        land(&mut session, reply(vec![row_hit("new", "m3")], None));
        assert!(!session.apply_search_reply(old, &mut reply(vec![row_hit("stale", "m4")], None)));
        assert_eq!(session.cursor_id.as_deref(), Some("new"));
        session.on_key(KeyId::Escape, 10);
        assert!(!session.apply_search_reply(recent, &mut reply(vec![], None)));
        assert_eq!(session.folder, "Archive");
        assert_eq!(session.cursor_id.as_deref(), Some("b"));
        assert!(session.more_after);
        assert!(session.picked.contains("a"));
    }

    #[test]
    fn next_and_previous_hit_skip_nonmatching_messages_and_wrap() {
        let mut session = session();
        session.perform(Action::Search, 10);
        session.type_search("hall");
        session.on_key(KeyId::Enter, 10);
        let fixtures = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        crate::fixtures::seed_if_empty(&fixtures, &account).unwrap();
        let threads = fixtures.search(&account.id, "hall").unwrap();
        let hit = threads
            .hits
            .iter()
            .find(|hit| hit.subject.contains("Venue"))
            .unwrap()
            .clone();
        let matches = fixtures
            .search_thread_matches(&account.id, "hall", &hit.thread_id)
            .unwrap();
        let messages = fixtures
            .thread_messages(&account.id, &hit.thread_id)
            .unwrap();
        let locations = fixtures
            .search_locations(&account.id, "hall", "Inbox", std::slice::from_ref(&hit))
            .unwrap();
        let matched = messages
            .iter()
            .find(|message| message.headers.message_id == hit.message_id)
            .unwrap();
        assert_eq!(
            locations[&hit.message_id].sender_name,
            matched.headers.from.name
        );
        assert_eq!(locations[&hit.message_id].message_count, messages.len());
        let thread = hit.thread_id.0.clone();
        land(&mut session, reply(vec![hit], None));
        session.on_key(KeyId::Enter, 10);
        session.set_messages(crate::present::conversation(
            &messages,
            &account.email,
            &session.roles,
            &[],
            "Inbox",
        ));
        assert!(matches.len() > 1);
        session.set_search_matches(
            session.search_generation,
            &thread,
            matches.clone(),
            MatchLanding::Best,
        );
        let representative = session.message_cursor;
        session.on_key(KeyId::Char('n'), 10);
        assert_ne!(session.message_cursor, representative);
        assert!(
            matches
                .iter()
                .any(|hit| hit.message.0 == session.messages[session.message_cursor].id)
        );
        assert!(session.messages[session.message_cursor].expanded);
        assert!(
            session.messages[session.message_cursor]
                .id
                .contains("venue-2@fell.test")
        );
        assert!(
            session.is_open(&session.messages[session.message_cursor].id),
            "the only hall match here is folded"
        );
        session.on_key(KeyId::Char('N'), 10);
        assert_eq!(session.message_cursor, representative);
    }

    #[test]
    fn edits_retain_accepted_rows_and_preview_and_do_not_requery_equivalent_text() {
        let mut session = session();
        session.perform(Action::Search, 10);
        session.type_search("hall");
        let accepted = session.search_generation;
        land(
            &mut session,
            reply(vec![row_hit("one", "m1"), row_hit("two", "m2")], None),
        );
        session.select_if_present("two");
        session.picked.insert("one".into());
        let mut messages = Vec::new();
        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let thread = store
            .search(&account.id, "hall")
            .unwrap()
            .hits
            .remove(0)
            .thread_id;
        messages.extend(crate::present::conversation(
            &store.thread_messages(&account.id, &thread).unwrap(),
            &account.email,
            &session.roles,
            &[],
            "Inbox",
        ));
        session.set_messages(messages.clone());
        assert!(!session.type_search(" ").search_request);
        assert_eq!(session.search_generation, accepted);
        assert_eq!(session.cursor_id.as_deref(), Some("two"));
        assert!(session.type_search("lane").search_request);
        let stale = session.search_generation;
        assert!(session.search.as_ref().unwrap().pending);
        assert_eq!(session.threads.len(), 2);
        assert_eq!(session.messages, messages);
        assert_eq!(session.perform(Action::Archive, 10), Step::default());
        assert_eq!(session.perform(Action::Next, 10), Step::default());
        for _ in 0..4 {
            session.on_key(KeyId::Backspace, 10);
        }
        let search = session.search.as_ref().unwrap();
        assert!(!search.pending);
        assert_eq!(search.accepted_generation(), Some(accepted));
        assert!(!session.apply_search_reply(stale, &mut reply(vec![], None)));
        assert_eq!(session.cursor_id.as_deref(), Some("two"));
        assert!(session.picked.contains("one"));
        assert_eq!(
            query_key(r#"subject:"a  b"   hall  "#),
            r#"subject:"a  b" hall"#
        );
    }

    #[test]
    fn repeated_folder_refresh_replaces_local_and_synced_draft_rows() {
        let mut session = session();
        session.folder = "Drafts".into();
        let rows = reply(
            vec![row_hit("draft:local", "local"), row_hit("synced", "remote")],
            None,
        )
        .rows;
        let [mut local, synced] = <[ThreadRow; 2]>::try_from(rows).unwrap();
        local.draft = Some("local".into());
        session.set_threads(vec![local.clone(), synced.clone()]);
        assert_eq!(session.cursor_id.as_deref(), Some("draft:local"));
        session.perform(Action::Search, 10);

        let mut restored = local;
        restored.subject = "Restored draft text".into();
        let refreshed = vec![restored, synced];
        let page = fell_core::FolderPage {
            threads: vec![],
            more_before: false,
            more_after: false,
            unread: 0,
        };
        for _ in 0..3 {
            session.refresh_saved_folder(refreshed.clone(), &page, None);
        }
        session.on_key(KeyId::Escape, 10);
        assert_eq!(session.threads, refreshed);
        assert_eq!(session.cursor_id.as_deref(), Some("draft:local"));
    }

    #[test]
    fn confirmed_results_use_the_hit_folder_and_restore_the_refreshed_folder() {
        let mut session = session();
        session.set_threads(
            reply(
                vec![row_hit("removed", "old"), row_hit("still", "same")],
                None,
            )
            .rows,
        );
        session.perform(Action::Search, 10);
        session.type_search("hall");
        land(
            &mut session,
            reply(vec![row_hit("one", "m1"), row_hit("two", "m2")], None),
        );
        session.on_key(KeyId::Enter, 10);
        session.picked.insert("one".into());
        session.picked.insert("two".into());
        let targets = session.action_targets();
        assert_eq!(
            session
                .search_scopes(&targets)
                .unwrap()
                .iter()
                .map(|(_, folder)| folder.as_str())
                .collect::<Vec<_>>(),
            ["INBOX", "Sent"]
        );
        assert!(session.perform(Action::Delete, 10).delete);
        session.picked.clear();
        session.select_if_present("two");
        assert!(session.perform(Action::Archive, 10).archive);
        assert_eq!(session.search_source("two"), Some("Sent"));
        assert!(session.perform(Action::ToggleStar, 10).flags.is_some());
        assert!(session.perform(Action::Undo, 10).undo);

        let page = fell_core::FolderPage {
            threads: vec![],
            more_before: false,
            more_after: false,
            unread: 2,
        };
        session.refresh_saved_folder(
            reply(vec![row_hit("still", "same")], None).rows,
            &page,
            None,
        );
        session.on_key(KeyId::Escape, 10);
        assert_eq!(session.cursor_id.as_deref(), Some("still"));
        assert_eq!(session.unread_count, 2);
        assert_eq!(session.threads.len(), 1);
    }

    #[test]
    fn mixed_search_selection_skips_matches_without_a_folder_and_deletes_only_to_trash() {
        let mut session = session();
        session.perform(Action::Search, 10);
        session.type_search("hall");
        let mut hits = reply(
            vec![
                row_hit("inbox", "m1"),
                row_hit("trash", "m2"),
                row_hit("missing", "m3"),
            ],
            None,
        );
        hits.hits.get_mut("trash").unwrap().folder = Some("Trash".into());
        hits.hits.get_mut("missing").unwrap().folder = None;
        land(&mut session, hits);
        session.on_key(KeyId::Enter, 10);
        session
            .picked
            .extend(["inbox".into(), "trash".into(), "missing".into()]);
        let targets = session.action_targets();
        assert_eq!(
            session.search_scopes(&targets).unwrap(),
            [
                (fell_core::ThreadId("inbox".into()), "INBOX".into()),
                (fell_core::ThreadId("trash".into()), "Trash".into()),
            ]
        );
        assert!(session.search_scopes(&["missing".into()]).is_none());
        let step = session.perform(Action::Delete, 10);
        assert!(step.delete && !step.purge);
        session.picked.remove("inbox");
        session.select_if_present("trash");
        let step = session.perform(Action::Delete, 10);
        assert!(!step.delete && !step.purge);
        assert!(!session.on_key(KeyId::Char('y'), 11).purge);
    }
}
