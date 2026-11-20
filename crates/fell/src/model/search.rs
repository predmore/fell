//! Search input and navigation, independent of the window and store actor.

use std::collections::{HashMap, HashSet};
use std::ops::Range;

use fell_core::{MessageId, SearchField, SearchHit, SearchPreview};

use super::{Fetch, Pending, Session, Step};
use crate::keymap::KeyId;
use crate::present::{NameSpan, Pane, ThreadRow, one_line};

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
}

#[derive(Clone, Debug, Default)]
pub struct Search {
    pub query: String,
    pub caret: usize,
    pub editing: bool,
    pub pending: bool,
    pub error: Option<String>,
    pub correction: Option<String>,
    pub suggestions: Vec<String>,
    pub suggestion_index: usize,
    pub hits: HashMap<String, Hit>,
    pub matches: Vec<MessageId>,
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub message: MessageId,
    pub related: bool,
    /// An answered preview can still have no highlighted text.
    pub preview: Option<SearchPreview>,
}

pub struct SearchReply {
    pub rows: Vec<ThreadRow>,
    pub hits: HashMap<String, Hit>,
    pub correction: Option<String>,
    pub suggestions: Vec<String>,
    pub error: Option<String>,
}

impl SearchReply {
    pub fn new(
        results: fell_core::Result<fell_core::SearchResults>,
        suggestions: Vec<String>,
        mut previews: HashMap<MessageId, SearchPreview>,
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
                    rows.push(ThreadRow {
                        id: thread_id.0.clone(),
                        spans: vec![NameSpan {
                            text: one_line(&sender_email),
                            emphasis: true,
                        }],
                        extra: 0,
                        message_count: 1,
                        starred: false,
                        has_attachment: false,
                        date_unix,
                        subject: one_line(&subject),
                        snippet: one_line(&snippet),
                        labels: Vec::new(),
                        warn: false,
                        unread: false,
                        draft: None,
                    });
                    hits.insert(
                        thread_id.0,
                        Hit {
                            preview: previews.remove(&message_id),
                            message: message_id,
                            related: semantic_only,
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
            suggestions,
            error,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum CompletionField {
    Mailbox(SearchField),
    Flag(&'static str),
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
    pub fn effective_query(&self) -> &str {
        self.correction.as_deref().unwrap_or(&self.query)
    }
}

impl Session {
    pub(super) fn start_search(&mut self, query: Option<String>) -> Step {
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
        self.pane = Pane::List;
        true
    }

    pub fn type_search(&mut self, text: &str) -> Step {
        let Some(search) = self.search.as_mut().filter(|search| search.editing) else {
            return Step::default();
        };
        let line: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let typed = if text.chars().any(|ch| ch.is_control()) {
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

    pub(super) fn search_key(&mut self, key: KeyId) -> Step {
        let search = self.search.as_mut().unwrap();
        match key {
            KeyId::Escape => {
                self.end_search();
                return Step::default();
            }
            KeyId::Enter => {
                search.editing = false;
                // Remember a submitted query, including one whose response
                // is still in flight. Tabbed corrections replace it first.
                self.remember_search();
                return Step::default();
            }
            KeyId::Tab => return self.accept_search_suggestion(),
            KeyId::Up | KeyId::Down if !search.suggestions.is_empty() => {
                let len = search.suggestions.len();
                let step = if key == KeyId::Down { 1 } else { len - 1 };
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
        self.search_generation = self.search_generation.wrapping_add(1);
        let search = self.search.as_mut().unwrap();
        search.pending = !search.query.trim().is_empty();
        search.error = None;
        search.correction = None;
        search.suggestions.clear();
        search.suggestion_index = 0;
        search.hits.clear();
        search.matches.clear();
        self.threads.clear();
        self.cursor_id = None;
        self.picked.clear();
        self.anchor_id = None;
        self.visual = false;
        Step {
            search_request: search.pending,
            ..Step::default()
        }
    }

    fn update_search_completion(&mut self) -> Step {
        let Some(search) = self.search.as_mut() else {
            return Step::default();
        };
        search.suggestions.clear();
        search.suggestion_index = 0;
        Step {
            search_completion: true,
            ..Step::default()
        }
    }

    pub fn accept_search_suggestion(&mut self) -> Step {
        let Some(search) = self.search.as_mut().filter(|search| search.editing) else {
            return Step::default();
        };
        if let Some(corrected) = search.correction.take() {
            search.query = corrected;
            search.caret = search.query.len();
        } else if let Some(suggestion) = search.suggestions.get(search.suggestion_index)
            && let Some(completion) = completion(&search.query, search.caret)
        {
            let value = if suggestion
                .chars()
                .any(|ch| ch.is_ascii_whitespace() || ch == '"')
            {
                format!("\"{}\"", suggestion.replace('"', "\"\""))
            } else {
                suggestion.clone()
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
        self.search_changed()
    }

    pub fn apply_search_reply(
        &mut self,
        generation: u64,
        caret: usize,
        reply: SearchReply,
    ) -> bool {
        let Some(search) = self
            .search
            .as_mut()
            .filter(|_| self.search_generation == generation)
        else {
            return false;
        };
        self.threads = reply.rows;
        self.cursor_id = self.threads.first().map(|row| row.id.clone());
        search.pending = false;
        search.error = reply.error;
        search.correction = reply.correction;
        search.hits = reply.hits;
        if search.caret == caret {
            search.suggestions = reply.suggestions;
            search.suggestion_index = 0;
        }
        true
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

    pub fn set_search_matches(&mut self, thread: &str, matches: Vec<MessageId>) {
        let Some(search) = self.search.as_mut() else {
            return;
        };
        let Some(hit) = search.hits.get(thread) else {
            return;
        };
        search.matches = if matches.is_empty() {
            vec![hit.message.clone()]
        } else {
            matches
        };
        if let Some(index) = self
            .messages
            .iter()
            .position(|message| message.id == hit.message.0)
        {
            self.message_cursor = index;
            self.messages[index].expanded = true;
        }
    }

    pub(super) fn move_search_hit(&mut self, forward: bool, count: usize) -> Step {
        let Some(search) = self
            .search
            .as_ref()
            .filter(|_| self.pane == Pane::Conversation)
        else {
            return Step::default();
        };
        let hits: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| search.matches.iter().any(|hit| hit.0 == message.id))
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
        Step {
            message_disclosure_changed: changed,
            reveal_search_hit: true,
            ..Step::default()
        }
    }
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
    use crate::model::SyncKind;

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

    fn reply(
        hits: Vec<SearchHit>,
        correction: Option<&str>,
        suggestions: Vec<&str>,
    ) -> SearchReply {
        SearchReply::new(
            Ok(fell_core::SearchResults {
                hits,
                did_you_mean: correction.map(str::to_owned),
            }),
            suggestions.into_iter().map(str::to_owned).collect(),
            HashMap::new(),
        )
    }

    fn land(session: &mut Session, reply: SearchReply) {
        let caret = session.search.as_ref().unwrap().caret;
        assert!(session.apply_search_reply(session.search_generation, caret, reply));
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
            reply(
                vec![row_hit("one", "m1"), row_hit("two", "m2")],
                None,
                vec![],
            ),
        );
        assert_eq!(session.pane, Pane::List);
        assert_eq!(session.cursor_id.as_deref(), Some("one"));
        session.on_key(KeyId::Char('j'), 10);
        assert_eq!(session.cursor_id.as_deref(), Some("two"));
        session.on_key(KeyId::Enter, 10);
        assert_eq!(session.pane, Pane::Conversation);
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
        land(
            &mut session,
            reply(vec![], None, vec!["Archive", "Archive 2025"]),
        );
        session.on_key(KeyId::Down, 10);
        assert!(session.on_key(KeyId::Tab, 10).search_request);
        assert_eq!(
            session.search.as_ref().unwrap().query,
            r#"invoice in:"Archive 2025" "#
        );
        assert!(completion(r#""note from:ada""#, 13).is_none());
    }

    #[test]
    fn correction_previews_hits_and_tab_submits_the_corrected_query() {
        let mut session = session();
        session.on_key(KeyId::Char('/'), 10);
        session.type_search("invioce");
        land(
            &mut session,
            reply(vec![row_hit("one", "m1")], Some("invoice"), vec![]),
        );
        assert_eq!(session.threads.len(), 1);
        assert_eq!(
            session.search.as_ref().unwrap().effective_query(),
            "invoice"
        );
        assert!(session.on_key(KeyId::Tab, 10).search_request);
        assert!(session.threads.is_empty());
        assert_eq!(session.search.as_ref().unwrap().query, "invoice");
        session.on_key(KeyId::Enter, 10);
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
        let initial = reply(vec![row_hit("a", "m1"), row_hit("b", "m2")], None, vec![]);
        session.set_threads(initial.rows);
        session.select_if_present("b");
        session.more_after = true;
        session.picked.insert("a".into());
        session.perform(Action::Search, 10);
        session.type_search("old");
        let old = session.search_generation;
        session.type_search("er");
        let recent = session.search_generation;
        land(
            &mut session,
            reply(vec![row_hit("new", "m3")], None, vec![]),
        );
        assert!(!session.apply_search_reply(
            old,
            3,
            reply(vec![row_hit("stale", "m4")], None, vec![])
        ));
        assert_eq!(session.cursor_id.as_deref(), Some("new"));
        session.on_key(KeyId::Escape, 10);
        assert!(!session.apply_search_reply(recent, 5, reply(vec![], None, vec![])));
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
        let thread = hit.thread_id.0.clone();
        land(&mut session, reply(vec![hit], None, vec![]));
        session.on_key(KeyId::Enter, 10);
        session.set_messages(crate::present::conversation(
            &messages,
            &account.email,
            &session.roles,
            &[],
            "Inbox",
        ));
        assert!(matches.len() > 1);
        session.set_search_matches(&thread, matches.clone());
        let representative = session.message_cursor;
        session.on_key(KeyId::Char('n'), 10);
        assert_ne!(session.message_cursor, representative);
        assert!(
            matches
                .iter()
                .any(|id| id.0 == session.messages[session.message_cursor].id)
        );
        assert!(session.messages[session.message_cursor].expanded);
        session.on_key(KeyId::Char('N'), 10);
        assert_eq!(session.message_cursor, representative);
    }
}
