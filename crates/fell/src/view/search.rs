//! Store dispatch and drawing for the inbox's search mode.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fell_core::{MessageId, SearchPreview, Store};
use gpui::{HighlightStyle, StyledText};

use super::*;
use crate::model::search::{
    CompletionField, MatchLanding, REFRESH_FEEDBACK_DELAY, Search, SearchReply, Suggestion,
};
use crate::recipient_menu::{self, Menu};

const PREVIEW_BATCH: usize = 32;
const SUGGESTION_LIMIT: usize = 4;
const QUERY_DELAY: Duration = Duration::from_millis(30);
const SEARCH_SETTLE_DELAY: Duration = Duration::from_millis(220);

#[derive(Default)]
pub(super) struct SearchView {
    ready: bool,
    current: Arc<AtomicU64>,
    folder_scroll: Option<UniformListScrollHandle>,
    folder_read: Option<ReadIntent>,
    preview_asked: HashSet<MessageId>,
    accepted: Arc<AtomicU64>,
    completion_revision: Arc<AtomicU64>,
    correspondent_menu: Menu,
    correspondent_field: Option<CompletionField>,
    completion_pending: bool,
    accept_pending: bool,
    pub(super) folder_revision: u64,
    pub(super) prefer: Vec<ThreadId>,
    preview_key: Option<PreviewKey>,
    preview_revision: u64,
    rows_key: Option<RowsKey>,
    rows_revision: u64,
    rows_previews: u64,
    rows_were_current: bool,
    arrival: Option<motion::Transition>,
    settling_reply: Option<SearchReply>,
    pub(super) refresh_open_after_reply: bool,
    quiet_preview: Option<u64>,
    match_line: MatchLineMotion,
    match_frame: MatchLineFrame,
}

#[derive(PartialEq)]
struct PreviewKey {
    conversation: u64,
    subject: String,
    theme: Theme,
    offset: gpui::Pixels,
    split: f32,
    syncing: bool,
}

#[derive(PartialEq)]
struct RowsKey {
    generation: u64,
    previews: u64,
    selected: HashSet<String>,
    theme: Theme,
    offset: f32,
    minute: i64,
}

/// The results stay in place as the field and feedback change.
pub(super) struct CachedRows(pub(super) gpui::WeakEntity<Inbox>);

impl Render for CachedRows {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.0
            .update(cx, |inbox, cx| {
                div()
                    .size_full()
                    .flex()
                    .flex_col()
                    .child(rows(
                        &inbox.theme,
                        inbox.session.threads.len(),
                        inbox.list_scroll.clone(),
                        cx,
                    ))
                    .into_any_element()
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

/// During field edits, a steady conversation can reuse its laid-out scene.
/// Input guards and the pending shield remain in the outer view.
pub(super) struct CachedPreview(gpui::WeakEntity<Inbox>);

impl CachedPreview {
    pub(super) fn new(inbox: gpui::WeakEntity<Inbox>) -> Self {
        Self(inbox)
    }

    fn frame(inbox: &mut Inbox) -> ConvFrame {
        ConvFrame {
            agent_note: inbox.note_view.frame(inbox.frame_now),
            anchor: inbox.hold_landing(),
            frames: inbox.frames.clone(),
            syncing: inbox.pending_bodies.downloading(&inbox.session.sync),
            // The outer pane owns the shield when the preview is stale.
            preview_current: true,
            match_line: inbox.search_view.match_frame.clone(),
            ..ConvFrame::default()
        }
    }
}

impl Render for CachedPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.0
            .update(cx, |inbox, cx| {
                let width = f32::from(window.viewport_size().width);
                let width = width - list_width(width, inbox.split) - SPLITTER_PX;
                let frame = Self::frame(inbox);
                conversation(
                    &inbox.theme,
                    &inbox.session,
                    inbox.shown_subject(),
                    &inbox.motion,
                    inbox.frame_now,
                    Some(inbox.conv_scroll.clone()),
                    ReaderColumns::split(width),
                    Live(false),
                    &frame,
                    window,
                    cx,
                )
                .into_any_element()
            })
            .unwrap_or_else(|_| div().into_any_element())
    }
}

impl SearchView {
    pub(super) fn invalidate_row_previews(&mut self) {
        self.rows_previews = self.rows_previews.wrapping_add(1);
    }

    fn correspondent_rows_current(&self, shown: u64) -> bool {
        self.correspondent_menu.is_open()
            && !self.completion_pending
            && self.completion_revision.load(Ordering::Relaxed) == shown
    }

    pub(super) fn animating(&self, now: Instant) -> bool {
        self.arrival
            .as_ref()
            .is_some_and(|arrival| arrival.running(now))
    }

    pub(super) fn defer_folder_read(&mut self, intent: ReadIntent) {
        self.folder_read = Some(match self.folder_read.take() {
            Some(earlier) => earlier.merge(intent),
            None => intent,
        });
    }

    pub(super) fn take_folder_read(&mut self) -> Option<ReadIntent> {
        self.folder_read.take()
    }
}

impl Inbox {
    pub(super) fn quiet_search_preview(&self) -> bool {
        self.session.search.is_some()
            && self.open_thread.is_some()
            && self.open_thread == self.session.cursor_id
            && self.search_view.quiet_preview == Some(self.session.search_generation)
    }

    pub(super) fn search_match_frame(
        &mut self,
        now: Instant,
        preview_current: bool,
    ) -> MatchLineFrame {
        let message = (self.session.search.is_some()
            && (self.session.pane == Pane::Conversation || self.session.search_editing()))
        .then(|| self.session.messages.get(self.session.message_cursor))
        .flatten();
        let retain = !preview_current
            && self.open_thread == self.session.cursor_id
            && (self.quiet_search_preview()
                || self.search_view.refresh_open_after_reply
                || self
                    .session
                    .search
                    .as_ref()
                    .is_some_and(|search| search.refresh_started.is_some()));
        let line = message
            .filter(|_| preview_current)
            .and_then(|message| MatchLine::for_message(&self.session, message));
        let frame = self.search_view.match_line.update(
            message.map(|message| message.id.as_str()),
            line,
            retain,
            self.motion.motion,
            now,
        );
        self.search_view.match_frame = frame.clone();
        frame
    }

    pub(super) fn settle_search_match(&mut self) {
        self.search_view.match_line.transition = None;
    }

    pub(super) fn search_rows_for(&mut self, animating: bool) -> Option<(u64, gpui::AnyView)> {
        let search = self.session.search.as_ref().filter(|search| {
            search.editing
                && !animating
                && !self.session.threads.is_empty()
                && self
                    .list_scroll
                    .0
                    .borrow()
                    .deferred_scroll_to_item
                    .is_none()
        });
        let Some((search, generation)) =
            search.and_then(|search| Some((search, search.accepted_generation()?)))
        else {
            self.search_view.rows_key = None;
            return None;
        };
        let key = RowsKey {
            generation,
            previews: self.search_view.rows_previews,
            selected: self.session.highlighted(),
            theme: self.theme.clone(),
            offset: self.list_offset(),
            minute: now_unix() / 60,
        };
        let current = search.results_current();
        // When results become current again, let the list schedule previews
        // for any rows the reader scrolled to while results were stale.
        if self.search_view.rows_key.as_ref() != Some(&key)
            || (current && !self.search_view.rows_were_current)
        {
            self.search_view.rows_key = Some(key);
            self.search_view.rows_revision = self.search_view.rows_revision.wrapping_add(1);
            self.search_view.rows_were_current = current;
        }
        let mut style = div().flex_1().min_h_0();
        Some((
            self.search_view.rows_revision,
            gpui::AnyView::from(self.search_rows.clone()).cached(style.style().clone()),
        ))
    }

    pub(super) fn search_preview_for(
        &mut self,
        panes: (bool, bool),
        animating: bool,
        subject: &str,
    ) -> Option<(u64, gpui::AnyView)> {
        if panes != (true, true)
            || animating
            || self.session.pane != Pane::List
            || !self.session.search_editing()
            || self.session.messages.is_empty()
        {
            self.search_view.preview_key = None;
            return None;
        }
        let key = PreviewKey {
            conversation: self.conversation_revision,
            subject: subject.to_owned(),
            theme: self.theme.clone(),
            offset: self.conv_scroll.offset().y,
            split: self.split,
            syncing: self.pending_bodies.downloading(&self.session.sync),
        };
        if self.search_view.preview_key.as_ref() != Some(&key) {
            self.search_view.preview_key = Some(key);
            // GPUI reuses a cached scene by element identity during paint.
            // Change that identity in the frame that takes the new state.
            self.search_view.preview_revision = self.search_view.preview_revision.wrapping_add(1);
        }
        let mut box_style = div().flex_1().min_h_0().min_w_0();
        Some((
            self.search_view.preview_revision,
            gpui::AnyView::from(self.search_preview.clone()).cached(box_style.style().clone()),
        ))
    }

    pub(super) fn search_store_ready(&mut self, cx: &mut Context<Self>) {
        self.search_view.ready = true;
        if self.session.search_editing() {
            let accept = self.search_view.accept_pending;
            self.request_search_completion(cx);
            self.search_view.accept_pending = accept && self.search_view.completion_pending;
        }
        if self
            .session
            .search
            .as_ref()
            .is_some_and(|search| search.pending)
        {
            self.request_search(cx);
        }
    }

    pub(super) fn search_edit_key(
        &mut self,
        event: &KeyDownEvent,
        started: Instant,
        cx: &mut Context<Self>,
    ) -> bool {
        let stroke = &event.keystroke;
        if self.session.overlay == Overlay::None
            && let Some(key) = key_from(stroke)
            && self.correspondent_key(key)
        {
            if let Some(log) = &self.frames {
                log.borrow_mut().key(started, stroke.unparse());
            }
            cx.notify();
            return true;
        }
        let text = if key_from(stroke) == Some(KeyId::Ctrl('v')) {
            cx.read_from_clipboard().and_then(|item| item.text())
        } else if !stroke.modifiers.control && !stroke.modifiers.platform {
            stroke
                .key_char
                .clone()
                .filter(|text| !text.chars().any(char::is_control))
        } else {
            None
        };
        let edit_key = key_from(stroke).filter(|key| {
            matches!(
                key,
                KeyId::Backspace
                    | KeyId::Delete
                    | KeyId::Left
                    | KeyId::Right
                    | KeyId::Home
                    | KeyId::End
                    | KeyId::Up
                    | KeyId::Down
                    | KeyId::Space
                    | KeyId::Ctrl('u')
            )
        });
        if text.is_none() && edit_key.is_none() {
            return false;
        }
        if let Some(log) = &self.frames {
            log.borrow_mut().key(started, stroke.unparse());
        }
        let previous = self.session.search_generation;
        let step = match text {
            Some(text) => self.session.type_search(&text),
            None => self.session.search_key(edit_key.unwrap()),
        };
        // A field edit does not run list navigation, focus, or pane motion.
        self.after_search_step(&step, true, previous, cx);
        cx.notify();
        true
    }

    pub(super) fn after_search_step(
        &mut self,
        step: &Step,
        was_searching: bool,
        previous: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        let searching = self.session.search.is_some();
        let generation = self.session.search_generation;
        let switched = searching != was_searching;
        let cleared = previous != generation
            && self
                .session
                .search
                .as_ref()
                .is_some_and(|search| search.request().is_empty());
        if switched {
            self.search_view.folder_revision = self.search_view.folder_revision.wrapping_add(1);
            self.search_view
                .completion_revision
                .fetch_add(1, Ordering::Relaxed);
            self.search_view.prefer.clear();
            if searching {
                self.search_view.folder_scroll = Some(std::mem::replace(
                    &mut self.list_scroll,
                    UniformListScrollHandle::new(),
                ));
            } else if let Some(scroll) = self.search_view.folder_scroll.take() {
                self.list_scroll = scroll;
            }
        }
        if switched || cleared {
            self.search_view.arrival = None;
            self.search_view.refresh_open_after_reply = false;
            self.motion.list = None;
            self.motion.list_glide = None;
            self.motion.conv_glide = None;
            self.search_view.preview_asked.clear();
            self.search_view.accepted.store(0, Ordering::Relaxed);
            if searching {
                self.set_list_offset(0.0);
            }
            self.conv_gen.fetch_add(1, Ordering::SeqCst);
            self.clear_open_message();
            if cleared {
                self.settle_pending_bodies();
            }
        }
        if switched || previous != generation {
            self.search_view.settling_reply = None;
            self.search_view
                .current
                .store(generation, Ordering::Relaxed);
        }
        if self
            .session
            .search
            .as_ref()
            .is_some_and(|search| !search.editing)
            && let Some(reply) = self.search_view.settling_reply.take()
        {
            self.show_search_reply(generation, reply, cx);
        }
        if step.search_request && self.search_view.ready {
            self.request_search(cx);
        }
        if previous != generation
            && self.session.search_results_current()
            && std::mem::take(&mut self.search_view.refresh_open_after_reply)
        {
            self.request_messages(true, cx);
        }
        if step.search_completion {
            self.request_search_completion(cx);
        }
        switched || cleared
    }

    pub(super) fn refresh_search(&mut self, cx: &mut Context<Self>) {
        if self.begin_search_refresh(cx) {
            self.request_search(cx);
        }
    }

    pub(super) fn refresh_search_snippet(&mut self, message: MessageId, cx: &mut Context<Self>) {
        let Some(search) = &self.session.search else {
            return;
        };
        let Some(generation) = search.accepted_generation() else {
            return;
        };
        let Some((thread, hit)) = search.hits.iter().find(|(_, hit)| hit.message == message) else {
            return;
        };
        let thread = thread.clone();
        let query = search.effective_query().to_owned();
        let related = hit.related;
        let account = self.account_id.clone();
        let id = message.clone();
        let reply = self.store.call_search(move |store| {
            let snippet = store.message_snippet(&account, &id).ok().flatten()?;
            let preview = if related {
                Ok(None)
            } else {
                store
                    .search_previews(&account, &query, std::slice::from_ref(&id))
                    .map(|mut previews| previews.remove(&id))
            };
            Some((snippet, preview))
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some((snippet, preview))) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                let current = inbox
                    .session
                    .search
                    .as_ref()
                    .and_then(Search::accepted_generation);
                if current != Some(generation) {
                    inbox.refresh_search_snippet(message, cx);
                } else {
                    let retry = preview.is_err();
                    if retry {
                        inbox.search_view.preview_asked.remove(&message);
                    } else {
                        inbox.search_view.preview_asked.insert(message.clone());
                    }
                    let changed = inbox
                        .session
                        .set_search_snippet(generation, &thread, &message, &snippet, preview);
                    if changed || retry {
                        inbox.search_view.rows_previews =
                            inbox.search_view.rows_previews.wrapping_add(1);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    pub(super) fn begin_search_refresh(&mut self, cx: &mut Context<Self>) -> bool {
        let now = Instant::now();
        if !self.session.refresh_search(now) {
            return false;
        }
        self.cancel_search_work();
        if self.session.search.as_ref().unwrap().refresh_started == Some(now) {
            let timer = cx.background_executor().timer(REFRESH_FEEDBACK_DELAY);
            cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |inbox, cx| {
                    if inbox
                        .session
                        .search
                        .as_ref()
                        .is_some_and(|search| search.refresh_started == Some(now))
                    {
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        true
    }

    pub(super) fn cancel_search_work(&self) {
        self.search_view
            .current
            .store(self.session.search_generation, Ordering::Relaxed);
    }

    pub(super) fn action_folder(&mut self, id: &str) -> Option<String> {
        if self.session.search.is_none() {
            return Some(self.list_folder.clone());
        }
        let folder = self.session.search_source(id).map(str::to_owned);
        if folder.is_none() && self.session.search_results_current() {
            self.session.show_toast("No folder for this match", true);
        }
        folder
    }

    pub(super) fn remove_search_threads(
        &mut self,
        ids: Vec<String>,
        removal: Removal,
        cx: &mut Context<Self>,
    ) {
        let Some(scopes) = self
            .session
            .search_scopes(&ids)
            .filter(|scopes| removal != Removal::Purge || scopes.len() == ids.len())
        else {
            self.session
                .show_toast("No folder for a selected match", true);
            return;
        };
        let account = self.account_id.clone();
        let accepted = self
            .session
            .search
            .as_ref()
            .and_then(Search::accepted_generation);
        self.change_mailbox(
            move |store| match removal {
                Removal::Archive => store.archive_scoped_threads(&account, &scopes),
                Removal::Delete => store.delete_scoped_threads(&account, &scopes),
                Removal::Spam => store.spam_scoped_threads(&account, &scopes),
                Removal::Purge => store
                    .purge_scoped_threads(&account, &scopes)
                    .map(|action| (action, scopes.len())),
            },
            move |inbox, result, _| match result {
                Ok((action, count)) => {
                    if inbox
                        .session
                        .search
                        .as_ref()
                        .is_some_and(|search| search.accepted_generation() == accepted)
                    {
                        inbox.session.picked.clear();
                        inbox.session.visual = false;
                        inbox.session.anchor_id = None;
                    }
                    let role_label =
                        |role| inbox.session.folder_text(inbox.session.roles.name(role));
                    let done = match removal {
                        Removal::Archive => archived_notice(count, ids.len()),
                        Removal::Delete => moved_notice(count, &role_label(FolderRole::Trash)),
                        Removal::Spam if action.kind == fell_core::ActionKind::NotSpam => {
                            moved_notice(count, &role_label(FolderRole::Inbox))
                        }
                        Removal::Spam if count == 1 => "Spam status updated".to_owned(),
                        Removal::Spam => format!("Updated spam status for {count} conversations"),
                        Removal::Purge => "Removed from Fell".to_owned(),
                    };
                    inbox.session.show_toast(done, false);
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
                    None
                }
            },
            cx,
        );
    }

    pub(super) fn refresh_saved_search_folder(
        &mut self,
        select: Option<String>,
        cx: &mut Context<Self>,
    ) {
        let Some(span) = self.session.saved_folder_span() else {
            return;
        };
        self.search_view.folder_revision = self.search_view.folder_revision.wrapping_add(1);
        let revision = self.search_view.folder_revision;
        let account = self.account_id.clone();
        let folder = self.session.folder.clone();
        let me = self.session.account_email.clone();
        let label_scope = self.session.label_scope.clone();
        let reply = self.store.call_background(move |store| {
            if let Some(label) = label_scope {
                let (page, _) = store.label_page(&account, &label, &span, None)?;
                let rows = present::thread_rows(&page.threads, &me);
                return Ok((rows, page, Vec::new(), select));
            }
            let span = select
                .as_ref()
                .and_then(|id| {
                    store
                        .folder_key_of(&account, &folder, &ThreadId(id.clone()))
                        .ok()
                        .flatten()
                })
                .map_or(span, |key| FolderSpan::Window {
                    first: Some(key.clone()),
                    last: key,
                    before: 100,
                    after: 100,
                });
            let page = store.folder_page(&account, &folder, &span)?;
            let mut rows = present::thread_rows(&page.threads, &me);
            let drafts = if store
                .folder_roles(&account)?
                .is(FolderRole::Drafts, &folder)
            {
                let ids: Vec<_> = page
                    .threads
                    .iter()
                    .map(|row| row.thread_id.clone())
                    .collect();
                let previews = store.synced_draft_previews(&account, &folder, &ids)?;
                present::address_draft_rows(&mut rows, &previews, &me);
                store.unsent_compose_drafts(&account)?
            } else {
                Vec::new()
            };
            Ok::<_, fell_core::Error>((rows, page, drafts, select))
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok((threads, page, drafts, select))) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if inbox.search_view.folder_revision == revision {
                    inbox.drafts = inbox.live_drafts(drafts, cx);
                    let mut rows = if page.more_before {
                        Vec::new()
                    } else {
                        present::draft_rows(&inbox.drafts)
                    };
                    rows.extend(threads);
                    inbox
                        .session
                        .refresh_saved_folder(rows, &page, select.as_deref());
                }
            });
        })
        .detach();
    }

    pub(super) fn request_search(&mut self, cx: &mut Context<Self>) {
        self.search_view.settling_reply = None;
        let Some(search) = &self.session.search else {
            return;
        };
        let query = search.request().to_owned();
        let me = self.session.account_email.clone();
        let folder = self.session.folder.clone();
        let store = self.store.clone();
        let account = self.account_id.clone();
        let generation = self.session.search_generation;
        let current = Arc::clone(&self.search_view.current);
        let timer = cx.background_executor().timer(QUERY_DELAY);
        cx.spawn(async move |this, cx| {
            timer.await;
            if current.load(Ordering::Relaxed) != generation {
                return;
            }
            let response = store.call_search(move |store| {
                if current.load(Ordering::Relaxed) != generation {
                    return None;
                }
                let start = Instant::now();
                let results = store.search(&account, &query);
                let reply = (current.load(Ordering::Relaxed) == generation).then_some(results);
                let reply = reply.and_then(|results| {
                    let initial = results
                        .as_ref()
                        .ok()
                        .map(|results| {
                            let messages: Vec<_> = results
                                .hits
                                .iter()
                                .take(PREVIEW_BATCH)
                                .filter(|hit| !hit.semantic_only)
                                .map(|hit| hit.message_id.clone())
                                .collect();
                            store
                                .search_previews(
                                    &account,
                                    results.did_you_mean.as_deref().unwrap_or(&query),
                                    &messages,
                                )
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    if current.load(Ordering::Relaxed) != generation {
                        return None;
                    }
                    let locations = results
                        .as_ref()
                        .ok()
                        .map(|results| {
                            store
                                .search_locations(
                                    &account,
                                    results.did_you_mean.as_deref().unwrap_or(&query),
                                    &folder,
                                    &results.hits,
                                )
                                .unwrap_or_default()
                        })
                        .unwrap_or_default();
                    (current.load(Ordering::Relaxed) == generation)
                        .then(|| SearchReply::new(results, initial, locations, &me))
                });
                Some((start, Instant::now(), reply))
            });
            let response = match response.await {
                Ok(Some(response)) => response,
                Ok(None) => return,
                Err(_) => {
                    let now = Instant::now();
                    (
                        now,
                        now,
                        Some(SearchReply::new(
                            Err(fell_core::Error::Search(
                                "Could not search this mailbox".into(),
                            )),
                            HashMap::new(),
                            HashMap::new(),
                            "",
                        )),
                    )
                }
            };
            let _ = this.update(cx, |inbox, cx| {
                if let Some(log) = &inbox.frames {
                    log.borrow_mut().search_work(response.0, response.1);
                }
                if let Some(reply) = response.2 {
                    inbox.receive_search_reply(generation, reply, cx);
                }
            });
        })
        .detach();
    }

    fn receive_search_reply(
        &mut self,
        generation: u64,
        reply: SearchReply,
        cx: &mut Context<Self>,
    ) {
        if !self.session.defer_search_reply(generation, &reply) {
            self.show_search_reply(generation, reply, cx);
            return;
        }
        // Positive results land immediately. Give an unfinished query time
        // to resolve a transient error or a zero-match prefix.
        self.search_view.settling_reply = Some(reply);
        let timer = cx.background_executor().timer(SEARCH_SETTLE_DELAY);
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |inbox, cx| {
                if inbox.session.search_generation == generation
                    && let Some(reply) = inbox.search_view.settling_reply.take()
                {
                    inbox.show_search_reply(generation, reply, cx);
                }
            });
        })
        .detach();
    }

    pub(super) fn show_search_reply(
        &mut self,
        generation: u64,
        mut reply: SearchReply,
        cx: &mut Context<Self>,
    ) {
        let log = self.frames.clone();
        FrameLog::event(log.as_ref(), "search_results", || {
            if self.session.search_generation != generation {
                return;
            }
            let old_id = self.session.cursor_id.clone();
            let old_scope = old_id
                .as_ref()
                .and_then(|id| self.session.search.as_ref()?.hits.get(id)?.folder.clone());
            let mailbox_changed = self
                .session
                .search
                .as_ref()
                .is_some_and(Search::mailbox_changed);
            let background_refresh = self
                .session
                .search
                .as_ref()
                .is_some_and(|search| search.refresh_started.is_some());
            let old_index = self.session.selected_index();
            let offset = self.list_offset();
            if self.session.apply_search_reply(generation, &mut reply) {
                if !self.session.search_results_current() {
                    self.search_view.refresh_open_after_reply = false;
                    self.settle_pending_bodies();
                    cx.notify();
                    return;
                }
                let preferred = std::mem::take(&mut self.search_view.prefer);
                if !preferred.is_empty()
                    && let Some(row) = self
                        .session
                        .threads
                        .iter()
                        .find(|row| preferred.iter().any(|id| id.as_str() == row.id))
                {
                    self.session.cursor_id = Some(row.id.clone());
                }
                let search = self.session.search.as_ref().unwrap();
                self.search_view
                    .accepted
                    .store(generation, Ordering::Relaxed);
                self.search_view.preview_asked.clear();
                self.search_view.preview_asked.extend(
                    self.session
                        .threads
                        .iter()
                        .take(PREVIEW_BATCH)
                        .filter_map(|row| search.hits.get(&row.id))
                        .map(|hit| hit.message.clone()),
                );
                if old_id == self.session.cursor_id {
                    if let Some((old, new)) = old_index.zip(self.session.selected_index()) {
                        self.set_list_offset(offset + (old as f32 - new as f32) * ROW_PX);
                    }
                } else if old_id.is_none() {
                    self.set_list_offset(0.0);
                    self.search_view.arrival = self
                        .motion
                        .motion
                        .start(motion::QUICK, self.motion.clock.now());
                } else {
                    self.scroll_list_to_cursor();
                }
                let same_scope = old_id == self.session.cursor_id
                    && self.session.cursor_id == self.open_thread
                    && self
                        .session
                        .cursor_id
                        .as_deref()
                        .and_then(|id| self.session.search_source(id))
                        == old_scope.as_deref();
                let same_preview = !mailbox_changed && same_scope;
                let refresh_open = std::mem::take(&mut self.search_view.refresh_open_after_reply)
                    || self.pending_bodies.after.is_some();
                self.search_view.quiet_preview = (same_scope
                    && (background_refresh || (same_preview && refresh_open)))
                    .then_some(generation);
                if same_preview && self.open_thread.is_some() && !refresh_open {
                    self.refresh_search_matches(cx);
                } else {
                    self.request_messages(true, cx);
                }
                if let Some(edit) = self.session.resume_read_after_search() {
                    self.apply_flags(edit, cx);
                }
                cx.notify();
            }
        });
        // Dropping thousands of previous rows need not delay input.
        if reply.rows.len() > PREVIEW_BATCH || reply.hits.len() > PREVIEW_BATCH {
            cx.background_executor()
                .spawn(async move { drop(reply) })
                .detach();
        }
    }

    fn refresh_search_matches(&mut self, cx: &mut Context<Self>) {
        let Some((search, id)) = self.session.search.as_ref().zip(self.open_thread.clone()) else {
            return;
        };
        let Some(accepted) = search.accepted_generation() else {
            return;
        };
        let query = search.effective_query().to_owned();
        let thread = ThreadId(id.clone());
        let account = self.account_id.clone();
        let current = Arc::clone(&self.search_view.accepted);
        let reply = self.store.call(move |store| {
            (current.load(Ordering::Relaxed) == accepted).then(|| {
                store
                    .search_thread_matches(&account, &query, &thread)
                    .unwrap_or_default()
            })
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some(matches)) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if inbox.open_thread.as_deref() == Some(&id)
                    && inbox.session.cursor_id.as_deref() == Some(&id)
                    && inbox
                        .session
                        .search
                        .as_ref()
                        .is_some_and(|search| search.accepted_generation() == Some(accepted))
                {
                    let landing = if inbox.quiet_search_preview() {
                        MatchLanding::KeepReader
                    } else {
                        MatchLanding::CurrentMatch
                    };
                    if inbox
                        .session
                        .set_search_matches(accepted, &id, matches, landing)
                    {
                        inbox.land_on_reader();
                    }
                    // The cached scene also depends on matches and quote
                    // folds, even when the thread and scroll have not moved.
                    inbox.conversation_revision = inbox.conversation_revision.wrapping_add(1);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn request_search_completion(&mut self, cx: &mut Context<Self>) {
        let view = &mut self.search_view;
        let latest = Arc::clone(&view.completion_revision);
        let revision = latest.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let next = self
            .session
            .search
            .as_ref()
            .and_then(|search| search.completion_key())
            .cloned();
        let field = next.as_ref().and_then(|(field, prefix)| {
            (field.is_address() && !prefix.is_empty()).then_some(*field)
        });
        let now = Instant::now();
        let motion = self.motion.motion;
        if view.correspondent_field != field {
            view.correspondent_menu.dismiss(motion, now);
            view.correspondent_field = field;
            if let Some(search) = self.session.search.as_mut() {
                search.suggestion_index = 0;
            }
        }
        view.completion_pending = field.is_some();
        view.accept_pending = false;
        let Some((field, prefix)) = next
            .filter(|(field, prefix)| view.ready && (!field.is_address() || !prefix.is_empty()))
        else {
            return;
        };
        let account = self.account_id.clone();
        let store = self.store.clone();
        let excluded = self
            .session
            .search
            .as_ref()
            .map(|search| search.completion_exclusions().clone())
            .unwrap_or_default();
        cx.spawn(async move |this, cx| {
            let address = field.is_address();
            if address {
                cx.background_executor()
                    .timer(recipient_menu::QUERY_DELAY)
                    .await;
                if latest.load(Ordering::Relaxed) != revision {
                    return;
                }
            }
            let current = Arc::clone(&latest);
            let job = move |store: &Store| {
                if current.load(Ordering::Relaxed) == revision {
                    search_suggestions(store, &account, field, &prefix, &excluded)
                } else {
                    Vec::new()
                }
            };
            let mut suggestions = if address {
                let reply = store.call(job);
                let timer = cx
                    .background_executor()
                    .timer(recipient_menu::LOADING_DELAY);
                recipient_menu::wait_for_lookup(reply, timer, || {
                    let _ = this.update(cx, |inbox, cx| {
                        if latest.load(Ordering::Relaxed) == revision
                            && let Some(search) =
                                inbox.session.search.as_ref().filter(|s| s.editing)
                        {
                            inbox.search_view.correspondent_menu.loading(
                                search.suggestion_index,
                                inbox.motion.motion,
                                Instant::now(),
                            );
                            cx.notify();
                        }
                    });
                })
                .await
            } else {
                store.call_search_completion(job).await
            }
            .unwrap_or_default();
            let _ = this.update(cx, |inbox, cx| {
                if inbox
                    .search_view
                    .completion_revision
                    .load(Ordering::Relaxed)
                    == revision
                    && let Some(search) = inbox
                        .session
                        .search
                        .as_mut()
                        .filter(|search| search.editing)
                {
                    let mut accept = false;
                    if address {
                        let rows = suggestions
                            .into_iter()
                            .filter_map(|suggestion| match suggestion {
                                Suggestion::Correspondent(person) => Some(person),
                                Suggestion::Value(_) => None,
                            })
                            .collect();
                        let view = &mut inbox.search_view;
                        let menu = &mut view.correspondent_menu;
                        search.suggestion_index = menu.show(
                            rows,
                            search.suggestion_index,
                            inbox.motion.motion,
                            Instant::now(),
                        );
                        suggestions = menu
                            .rows()
                            .iter()
                            .cloned()
                            .map(Suggestion::Correspondent)
                            .collect();
                        view.completion_pending = false;
                        accept =
                            std::mem::take(&mut view.accept_pending) && !suggestions.is_empty();
                    } else {
                        search.suggestion_index = 0;
                    }
                    search.suggestions = suggestions;
                    if accept {
                        let before = inbox.before_step();
                        let step = inbox.session.accept_search_suggestion();
                        inbox.apply_step(step, before, cx);
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn correspondent_key(&mut self, key: KeyId) -> bool {
        let view = &mut self.search_view;
        let now = Instant::now();
        let motion = self.motion.motion;
        if !view.correspondent_menu.is_open() && !view.completion_pending {
            return false;
        }
        match key {
            KeyId::Escape => {
                view.correspondent_menu.dismiss(motion, now);
                view.completion_revision.fetch_add(1, Ordering::Relaxed);
                view.completion_pending = false;
                view.accept_pending = false;
                if let Some(search) = self.session.search.as_mut() {
                    search.suggestions.clear();
                }
            }
            KeyId::Tab | KeyId::Enter if view.completion_pending => view.accept_pending = true,
            KeyId::Tab if view.correspondent_menu.rows().is_empty() => {}
            KeyId::Up | KeyId::Down | KeyId::Ctrl('n' | 'p')
                if !view.completion_pending && !view.correspondent_menu.rows().is_empty() =>
            {
                let from = self.session.search.as_ref().unwrap().suggestion_index;
                self.session.search_key(key);
                let to = self.session.search.as_ref().unwrap().suggestion_index;
                view.correspondent_menu
                    .move_selection(from, to, motion, now);
            }
            _ => return false,
        }
        true
    }

    pub(super) fn search_recipient_popup(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Deferred> {
        let active = self
            .session
            .search
            .as_ref()
            .filter(|search| search.editing)
            .filter(|search| {
                search
                    .completion_key()
                    .is_some_and(|(field, prefix)| field.is_address() && !prefix.is_empty())
            });
        if active.is_none() {
            self.search_view
                .correspondent_menu
                .dismiss(self.motion.motion, Instant::now());
        }
        if self.session.overlay != Overlay::None {
            return None;
        }
        let selected = self
            .session
            .search
            .as_ref()
            .map_or(0, |search| search.suggestion_index);
        let revision = self.search_view.completion_revision.load(Ordering::Relaxed);
        let interactive = active.is_some() && self.search_view.correspondent_rows_current(revision);
        let theme = &self.theme;
        let width = (window.viewport_size().width - px(28.)).min(px(460.));
        let popup = self.search_view.correspondent_menu.popup(
            window,
            selected,
            interactive,
            theme,
            |index, row| {
                row.id(("search-recipient", index))
                    .cursor_pointer()
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |inbox, _, _, cx| {
                            if !inbox.search_view.correspondent_rows_current(revision) {
                                return;
                            }
                            let before = inbox.before_step();
                            let Some(search) = inbox.session.search.as_mut().filter(|search| {
                                search.editing
                                    && matches!(
                                        search.suggestions.get(index),
                                        Some(Suggestion::Correspondent(_))
                                    )
                            }) else {
                                return;
                            };
                            search.suggestion_index = index;
                            let step = inbox.session.accept_search_suggestion();
                            inbox.apply_step(step, before, cx);
                        }),
                    )
                    .into_any_element()
            },
        )?;
        Some(gpui::deferred(
            div()
                .absolute()
                .left(px(14.))
                .top(px(38.))
                .w(width)
                .child(popup),
        ))
    }

    fn queue_search_previews(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self
            .session
            .search
            .as_ref()
            .filter(|search| search.results_current())
        else {
            return;
        };
        let Some(generation) = search.accepted_generation() else {
            return;
        };
        let batch: Vec<_> = range
            .filter_map(|index| {
                let row = self.session.threads.get(index)?;
                let hit = search.hits.get(&row.id)?;
                (!hit.related && self.search_view.preview_asked.insert(hit.message.clone()))
                    .then(|| (row.id.clone(), hit.message.clone(), hit.preview.clone()))
            })
            .collect();
        if !batch.is_empty() {
            cx.defer_in(window, move |inbox, _, cx| {
                inbox.request_search_previews(generation, batch, cx)
            });
        }
    }

    fn request_search_previews(
        &mut self,
        generation: u64,
        batch: Vec<(String, MessageId, Option<SearchPreview>)>,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self
            .session
            .search
            .as_ref()
            .filter(|search| search.accepted_generation() == Some(generation))
        else {
            return;
        };
        let query = search.effective_query().to_owned();
        let account = self.account_id.clone();
        let current = Arc::clone(&self.search_view.accepted);
        let reply = self.store.call_background(move |store| {
            let mut previews = HashMap::new();
            for chunk in batch.chunks(PREVIEW_BATCH) {
                if current.load(Ordering::Relaxed) != generation {
                    return None;
                }
                let ids: Vec<_> = chunk.iter().map(|(_, id, _)| id.clone()).collect();
                if let Ok(mut loaded) = store.search_previews(&account, &query, &ids) {
                    previews.extend(ids.into_iter().map(|id| {
                        let preview = loaded.remove(&id);
                        (id, preview)
                    }));
                }
            }
            Some((batch, previews))
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some((batch, mut previews))) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if let Some(search) = inbox
                    .session
                    .search
                    .as_mut()
                    .filter(|search| search.accepted_generation() == Some(generation))
                {
                    if !search.results_current() {
                        for (_, id, _) in batch {
                            inbox.search_view.preview_asked.remove(&id);
                        }
                        // The cached list must get a chance to ask again.
                        inbox.search_view.rows_were_current = false;
                        return;
                    }
                    let mut changed = false;
                    for (thread, id, previous) in batch {
                        if let Some(hit) = search.hits.get_mut(&thread)
                            && hit.message == id
                            // A repair may already have delivered a newer preview.
                            && hit.preview == previous
                        {
                            match previews.remove(&id) {
                                Some(preview) if hit.preview != preview => {
                                    hit.preview = preview;
                                    changed = true;
                                }
                                Some(_) => {}
                                None => {
                                    // Retry on a later render without invalidating the row again.
                                    inbox.search_view.preview_asked.remove(&id);
                                }
                            }
                        }
                    }
                    if changed {
                        inbox.search_view.rows_previews =
                            inbox.search_view.rows_previews.wrapping_add(1);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }
}

fn search_suggestions(
    store: &Store,
    account: &AccountId,
    field: CompletionField,
    prefix: &str,
    excluded: &HashSet<String>,
) -> Vec<Suggestion> {
    if field.is_address() {
        return store
            .correspondent_suggestions_excluding(account, prefix, excluded)
            .unwrap_or_default()
            .into_iter()
            .map(Suggestion::Correspondent)
            .collect();
    }
    match field {
        CompletionField::Mailbox(field) => store
            .search_suggestions(account, field, prefix, SUGGESTION_LIMIT)
            .unwrap_or_default(),
        CompletionField::Flag(flag) if flag.starts_with(&prefix.to_ascii_lowercase()) => {
            vec![flag.to_owned()]
        }
        CompletionField::Flag(_) => Vec::new(),
    }
    .into_iter()
    .filter(|value| !value.eq_ignore_ascii_case(prefix))
    .map(Suggestion::Value)
    .collect()
}

pub(super) fn header(
    theme: &Theme,
    search: &Search,
    width: f32,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let font = conversation_line_style(window, theme, px(theme.ui_px));
    let field_width = (width - 94.).max(32.);
    let line = shape_ui_line(
        window,
        &font,
        &search.query,
        FontWeight::NORMAL,
        Some(theme.foreground),
    );
    let caret_x = line
        .as_ref()
        .map_or(px(0.), |line| line.x_for_index(search.caret));
    let left = (f32::from(caret_x) - field_width + 7.).max(0.);
    let field = div()
        .relative()
        .flex_1()
        .min_w_0()
        .h(px(23.))
        .overflow_hidden()
        .children(line.map(|line| {
            div()
                .absolute()
                .left(px(-left))
                .child(shaped_text(line, window))
        }))
        .when(search.query.is_empty(), |field| {
            field.child(
                div()
                    .ml(px(7.))
                    .text_color(rgb(theme.dimmed))
                    .child("Search mail"),
            )
        })
        .when(search.editing, |field| {
            field.child(
                div()
                    .absolute()
                    .left(caret_x - px(left))
                    .top(px(3.))
                    .w(px(2.))
                    .h(px(16.))
                    .bg(rgb(theme.accent)),
            )
        });
    div()
        .h(px(44.))
        .px(px(14.))
        .bg(rgb(theme.surface))
        .border_b(px(1.))
        .border_color(rgb(theme.border))
        .flex()
        .items_center()
        .child(
            div()
                .id("search-field")
                .h(px(32.))
                .w_full()
                .px(px(10.))
                .gap(px(10.))
                .rounded_md()
                .flex()
                .items_center()
                .border_1()
                .border_color(rgb(if search.editing {
                    theme.accent
                } else {
                    theme.border
                }))
                .bg(rgb(theme.background))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|inbox, _, window, cx| {
                        window.focus(&inbox.focus);
                        let before = inbox.before_step();
                        let step = inbox.session.perform(Action::Search, inbox.page_rows());
                        inbox.apply_step(step, before, cx);
                    }),
                )
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(rgb(theme.accent))
                        .child("/"),
                )
                .child(field),
        )
}

pub(super) fn feedback(
    theme: &Theme,
    search: &Search,
    count: usize,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let suggestion = search
        .suggestions
        .get(search.suggestion_index)
        .and_then(|suggestion| match suggestion {
            Suggestion::Value(value) => Some(value),
            Suggestion::Correspondent(_) => None,
        })
        .filter(|_| search.editing && (!search.results_current() || search.correction.is_none()));
    let correction = search.correction.as_ref().filter(|_| {
        search.results_current()
            && !matches!(
                search.suggestions.first(),
                Some(Suggestion::Correspondent(_))
            )
    });
    let show_pending = search.show_pending_feedback(Instant::now());
    let line = if let Some(correction) = correction {
        Some((format!("Did you mean {correction}?"), true))
    } else if let Some(value) = suggestion {
        Some((format!("Complete with {value}"), true))
    } else if show_pending {
        Some(("Searching…".to_owned(), false))
    } else if search.error.is_some() && count > 0 {
        Some(("Showing previous results".to_owned(), false))
    } else if count > 0 {
        Some((
            format!(
                "{count} {}",
                if count == 1 {
                    "conversation"
                } else {
                    "conversations"
                }
            ),
            false,
        ))
    } else {
        None
    };
    div()
        .flex_none()
        .h(px(34.))
        .when(line.is_some(), |row| {
            row.border_b(px(1.)).border_color(rgb(theme.border))
        })
        .children(line.map(|(label, accept)| {
            div()
                .id("search-feedback")
                .h_full()
                .px(px(14.))
                .gap(px(8.))
                .flex()
                .items_center()
                .text_color(rgb(if accept { theme.accent } else { theme.dimmed }))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .when(show_pending && suggestion.is_some(), |row| {
                    row.child(
                        div()
                            .flex_none()
                            .text_color(rgb(theme.dimmed))
                            .child("Searching…"),
                    )
                })
                .when(accept, |row| {
                    row.cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|inbox, _, _, cx| {
                                let before = inbox.before_step();
                                let step = inbox.session.accept_search_suggestion();
                                inbox.apply_step(step, before, cx);
                            }),
                        )
                        .children(
                            suggestion
                                .filter(|_| search.suggestions.len() > 1)
                                .map(|_| {
                                    div()
                                        .flex_none()
                                        .text_color(rgb(theme.dimmed))
                                        .child(format!(
                                            "{} / {}",
                                            search.suggestion_index + 1,
                                            search.suggestions.len()
                                        ))
                                }),
                        )
                        .child(div().flex_none().text_color(rgb(theme.dimmed)).child("Tab"))
                })
        }))
}

pub(super) fn empty(search: &Search) -> gpui::Div {
    let text = if !search.results_current()
        || search.query.trim().is_empty()
        || (search.editing && !search.suggestions.is_empty())
    {
        ""
    } else {
        "No mail matches"
    };
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .px(px(18.))
        .text_center()
        .child(div().child(text))
}

pub(super) fn rows(
    theme: &Theme,
    count: usize,
    scroll: UniformListScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::UniformList {
    let entity = cx.entity();
    let theme = theme.clone();
    uniform_list("search-rows", count, move |range, window, app| {
        entity.update(app, |inbox, cx| {
            inbox.queue_search_previews(range.clone(), window, cx);
            let highlighted = inbox.session.highlighted();
            let width = f32::from(window.viewport_size().width);
            let (_, beside) =
                present::visible_panes(width, inbox.session.pane, inbox.session.full_width);
            let width = if beside {
                list_width(width, inbox.split)
            } else {
                width
            };
            range
                .map(|index| row(&theme, inbox, index, &highlighted, width, window, cx))
                .collect()
        })
    })
    .flex_1()
    .min_h_0()
    .track_scroll(scroll)
}

const ROW_GAP: f32 = 8.0;
const MIN_SUBJECT: f32 = 72.0;
const ROW_PAD: f32 = 14.0;
const SENDER_GAP: f32 = 5.0;
const STAR: &str = "★";

fn row_text_width(list_width: f32) -> f32 {
    (list_width - LIST_BORDER_PX - 2.0 * ROW_PAD).max(0.0)
}

fn detail_budget(row_room: f32, room: f32, subject: f32, minimum: f32) -> f32 {
    (room - subject.max(MIN_SUBJECT))
        .max(minimum)
        .min(row_room * 0.45)
        .min((room - MIN_SUBJECT).max(0.0))
        .floor()
}

#[derive(Clone, Copy)]
enum RowText {
    Detail,
    Subject,
    Mark,
}

struct RowLine {
    sender_width: f32,
    sender: Option<gpui::ShapedLine>,
    count: Option<String>,
    star: Option<f32>,
    attachment: Option<f32>,
    subject: Option<gpui::ShapedLine>,
    detail: [Option<gpui::ShapedLine>; 2],
}

fn fit_row_line(
    row: &present::ThreadRow,
    list_width: f32,
    folder: Option<&str>,
    related: bool,
    dates: [&str; 2],
    shape: impl Fn(&str, RowText, Option<f32>) -> Option<gpui::ShapedLine>,
) -> RowLine {
    let line_width = |line: &gpui::ShapedLine| f32::from(line.width).ceil();
    let measure = |text: &str, kind| shape(text, kind, None).as_ref().map_or(0.0, line_width);
    let row_room = row_text_width(list_width);
    let count = (row.message_count > 1).then(|| format!("· {}", row.message_count));
    let count_width = count
        .as_deref()
        .map_or(0.0, |count| SENDER_GAP + measure(count, RowText::Detail));
    let sender_text = &row.spans[0].text;
    let sender_width =
        (measure(sender_text, RowText::Detail) + count_width).min((row_room * 0.30).floor());
    let sender_room = (sender_width - count_width).max(0.0);
    let sender = shape(sender_text, RowText::Detail, Some(sender_room))
        .filter(|line| line_width(line) <= sender_room);
    let star = row.starred.then(|| measure(STAR, RowText::Mark));
    let attachment = row
        .has_attachment
        .then(|| measure(THREAD_ATTACHMENT_MARK, RowText::Mark));
    let marks_width: f32 = star
        .into_iter()
        .chain(attachment)
        .map(|width| width + ROW_GAP)
        .sum();
    let room = (row_room - sender_width - marks_width - 2.0 * ROW_GAP).max(0.0);
    let subject = if row.subject.is_empty() {
        "(No subject)"
    } else {
        &row.subject
    };

    // Reserve the timestamp and enough of the final folder word to
    // distinguish sibling folders when the subject needs clipping.
    let shape_detail = |text: &str| shape(text, RowText::Detail, None);
    let full_date = shape_detail(dates[0]);
    let date_with_location = |date: &str| shape_detail(&format!(" · {date}"));
    let full_date_with_location = full_date
        .as_ref()
        .filter(|_| folder.is_some())
        .and_then(|date| date_with_location(&date.text));
    let related_prefix = if related { "Related · " } else { "" };
    let related = if related {
        shape_detail("Related")
    } else {
        None
    };
    let location_min = folder.map_or_else(
        || related.as_ref().map_or(0.0, line_width),
        |folder| {
            let leaf = folder.rsplit('/').next().unwrap_or(folder);
            let last = leaf.split_whitespace().next_back().unwrap_or(leaf);
            let suffix = if last == leaf { "" } else { "…" };
            measure(&format!("{related_prefix}{suffix}{last}"), RowText::Detail).min(120.0)
        },
    );
    let detail_room = detail_budget(
        row_room,
        room,
        measure(subject, RowText::Subject),
        full_date_with_location
            .as_ref()
            .or(full_date.as_ref())
            .map_or(0.0, line_width)
            + location_min,
    );
    let fits_date = |line: &gpui::ShapedLine| line_width(line) <= detail_room;
    let date = full_date.filter(fits_date).or_else(|| {
        if dates[0] == dates[1] {
            None
        } else {
            shape_detail(dates[1]).filter(fits_date)
        }
    });
    let dated_location = date
        .as_ref()
        .and_then(|date| date_with_location(&date.text));
    let location_room = detail_room - dated_location.as_ref().map_or(0.0, line_width);
    let location = folder
        .filter(|_| location_room > 0.0)
        .and_then(|folder| {
            fit_folder_path(
                folder,
                FolderFit::Search,
                |path| {
                    let text = format!("{related_prefix}{path}");
                    shape(&text, RowText::Detail, Some(location_room))
                        .filter(|line| line_width(line) <= location_room)
                },
                |line| line.text.strip_prefix(related_prefix).unwrap_or_default(),
            )
        })
        .or_else(|| related.filter(|line| line_width(line) <= location_room));
    let date = if location.is_some() {
        dated_location
    } else {
        date
    };
    let detail = [location, date];
    let subject_room = room - detail.iter().flatten().map(line_width).sum::<f32>();
    let subject = shape(subject, RowText::Subject, Some(subject_room));
    RowLine {
        sender_width,
        sender,
        count,
        star,
        attachment,
        subject,
        detail,
    }
}

fn row(
    theme: &Theme,
    inbox: &Inbox,
    index: usize,
    highlighted: &HashSet<String>,
    list_width: f32,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let row = &inbox.session.threads[index];
    let search = inbox.session.search.as_ref().unwrap();
    let hit = &search.hits[&row.id];
    let generation = search.accepted_generation();
    let selected = highlighted.contains(&row.id);
    let id = row.id.clone();
    let dim = if selected {
        theme.on_selection_dim
    } else {
        theme.dimmed
    };
    let background = if selected {
        theme.selection
    } else {
        theme.background
    };
    let foreground = if selected {
        theme.on_selection
    } else {
        theme.foreground
    };
    let opacity = motion::lerp(
        motion::ARRIVE_FROM,
        1.,
        motion::progress(inbox.search_view.arrival.as_ref(), inbox.frame_now),
    );
    let size = px(theme.ui_px - 2.0);
    let style = conversation_line_style(window, theme, size);
    let subject_style = conversation_line_style(window, theme, px(theme.ui_px));
    let dates = if hit.related {
        [String::new(), String::new()]
    } else {
        let now = now_unix();
        [
            present::relative_date(now, row.date_unix),
            present::compact_relative_date(now, row.date_unix),
        ]
    };
    let folder = hit
        .folder
        .as_ref()
        .map(|folder| inbox.session.roles.label(folder));
    let unread_note = inbox.session.agent_notes.unread.contains(&row.id);
    let marker_room = if unread_note {
        super::agent_notes::MARKER_SLOT + ROW_GAP
    } else {
        0.0
    };
    let RowLine {
        sender_width,
        sender,
        count,
        star,
        attachment,
        subject,
        detail,
    } = fit_row_line(
        row,
        list_width - marker_room,
        folder.as_deref(),
        hit.related,
        [&dates[0], &dates[1]],
        |text, kind, limit| {
            let (style, weight, color) = match kind {
                RowText::Detail => (&style, FontWeight::NORMAL, Some(dim)),
                RowText::Subject => (&subject_style, FontWeight::SEMIBOLD, Some(foreground)),
                RowText::Mark => (&subject_style, FontWeight::NORMAL, None),
            };
            match limit {
                Some(limit) => clip_shaped(window, style, text, limit, weight, color),
                None => shape_ui_line(window, style, text, weight, color),
            }
        },
    );
    let marks = [
        star.map(|width| (STAR, theme.star, width)),
        attachment.map(|width| (THREAD_ATTACHMENT_MARK, dim, width)),
    ];
    let row_room = row_text_width(list_width);
    let labels = if row.labels.is_empty() {
        None
    } else {
        thread_label_group(theme, row, background, dim, row_room * 0.45, None, window)
    };
    let snippet_room = row_room - labels.as_ref().map_or(0.0, |(_, width)| width + ROW_GAP);
    let excerpt = match &hit.preview {
        Some(preview) if !preview.highlights.is_empty() => {
            let (text, highlights) = visible_excerpt(preview);
            let (text_color, fill) = if selected {
                theme.selected_search_match
            } else {
                theme.search_match
            };
            let highlight = HighlightStyle {
                color: Some(rgb(text_color).into()),
                font_weight: Some(FontWeight::SEMIBOLD),
                background_color: Some(rgb(fill).into()),
                ..HighlightStyle::default()
            };
            let mut style = subject_style.clone();
            style.color = rgb(dim).into();
            fit_highlighted_excerpt(window, &style, &text, &highlights, highlight, snippet_room)
        }
        _ => clip_shaped(
            window,
            &subject_style,
            &row.snippet,
            snippet_room,
            FontWeight::NORMAL,
            Some(dim),
        ),
    };
    let excerpt = excerpt.filter(|line| line.width <= px(snippet_room));
    div()
        .id(gpui::ElementId::named_usize("search-row", index))
        .relative()
        .h(px(ROW_PX))
        .w_full()
        .px(px(ROW_PAD))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(3.))
        .bg(rgb(background))
        .text_color(rgb(foreground))
        .opacity(opacity)
        .when(row.unread, |row| {
            row.child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(12.))
                    .bottom(px(12.))
                    .w(px(3.))
                    .rounded_r_sm()
                    .bg(rgb(theme.accent)),
            )
        })
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |inbox, _, window, cx| {
                if inbox.session.search.as_ref().is_some_and(|search| {
                    search.results_current() && search.accepted_generation() == generation
                }) {
                    window.focus(&inbox.focus);
                    inbox.select_thread(id.clone(), cx);
                    cx.notify();
                }
            }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(ROW_GAP))
                .child(
                    div()
                        .flex()
                        .flex_none()
                        .w(px(sender_width))
                        .min_w_0()
                        .items_center()
                        .gap(px(SENDER_GAP))
                        .text_color(rgb(dim))
                        .text_size(size)
                        .children(sender.map(|line| shaped_text(line, window)))
                        .children(count.map(|count| div().flex_none().child(count))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .children(subject.map(|line| shaped_text(line, window))),
                )
                .children(marks.into_iter().flatten().map(|(text, color, width)| {
                    div()
                        .flex_none()
                        .w(px(width))
                        .text_color(rgb(color))
                        .child(text)
                }))
                .child(div().flex_none().flex().items_center().children(
                    detail.into_iter().flatten().map(|line| {
                        let width = line.width.ceil();
                        shaped_text(line, window).w(width)
                    }),
                ))
                .when(unread_note, |line| {
                    line.child(super::agent_notes::marker(theme, selected))
                }),
        )
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap(px(ROW_GAP))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .children(excerpt.map(|line| {
                            let height =
                                window.text_style().line_height_in_pixels(window.rem_size());
                            paint_shaped_text(line, height, true)
                        })),
                )
                .children(labels.map(|(group, _)| group)),
        )
        .into_any_element()
}

fn fit_highlighted_excerpt(
    window: &Window,
    style: &TextStyle,
    text: &str,
    highlights: &[Range<usize>],
    highlight: HighlightStyle,
    room: f32,
) -> Option<gpui::ShapedLine> {
    let size = style.font_size.to_pixels(window.rem_size());
    let matched = style.clone().highlight(highlight);
    // Measure the actual runs, including the heavier matches, against the inner row.
    fit_shaped_line(text, room, f32::from(size), |candidate| {
        let end = if text.starts_with(candidate) {
            candidate.len()
        } else {
            candidate.len() - '…'.len_utf8()
        };
        let mut runs = Vec::new();
        let mut offset = 0;
        for range in highlights.iter().take_while(|range| range.start < end) {
            if range.start > offset {
                runs.push(style.to_run(range.start - offset));
            }
            offset = range.end.min(end);
            runs.push(matched.to_run(offset - range.start));
        }
        if offset < candidate.len() {
            runs.push(style.to_run(candidate.len() - offset));
        }
        Some(
            window
                .text_system()
                .shape_line(candidate.to_owned().into(), size, &runs, None),
        )
    })
}

fn visible_excerpt(preview: &SearchPreview) -> (String, Vec<Range<usize>>) {
    let first = preview.highlights[0].start;
    let Some((start, _)) = preview.text[..first]
        .char_indices()
        .rev()
        .nth(20)
        .filter(|(i, _)| *i > 0)
    else {
        return (preview.text.clone(), preview.highlights.clone());
    };
    let tail = preview.text[start..].trim_start();
    let skipped = preview.text.len() - tail.len();
    let prefix = "…";
    (
        format!("{prefix}{tail}"),
        preview
            .highlights
            .iter()
            .map(|range| range.start - skipped + prefix.len()..range.end - skipped + prefix.len())
            .collect(),
    )
}

#[derive(Clone, PartialEq, Eq)]
struct MatchLine {
    position: usize,
    total: usize,
    preview: Option<SearchPreview>,
}

impl MatchLine {
    fn for_message(session: &Session, message: &ConvMessage) -> Option<Self> {
        let search = session.search.as_ref()?;
        if search.hits.get(session.cursor_id.as_deref()?)?.related {
            return None;
        }
        let hit = search
            .matches
            .iter()
            .find(|hit| hit.message.0 == message.id)?;
        let position = session.messages[..session.message_cursor]
            .iter()
            .filter(|message| search.matches.iter().any(|hit| hit.message.0 == message.id))
            .count()
            + 1;
        Some(Self {
            position,
            total: search.matches.len(),
            preview: hit
                .preview
                .clone()
                .filter(|preview| !preview.highlights.is_empty()),
        })
    }

    fn render(&self, theme: &Theme, line_height: f32) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .gap(px(9.))
            .line_height(px(line_height))
            .min_w_0()
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(theme.accent))
                    .child(format!("Match {} of {}", self.position, self.total)),
            )
            .children(self.preview.as_ref().map(|preview| {
                let (text, ranges) = visible_excerpt(preview);
                let style = HighlightStyle {
                    color: Some(rgb(theme.accent).into()),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    ..HighlightStyle::default()
                };
                div()
                    .min_w_0()
                    .truncate()
                    .text_color(rgb(theme.dimmed))
                    .child(
                        StyledText::new(text)
                            .with_highlights(ranges.into_iter().map(|range| (range, style))),
                    )
            }))
    }
}

#[derive(Default)]
struct MatchLineMotion {
    message: Option<String>,
    current: Option<MatchLine>,
    outgoing: Option<MatchLine>,
    transition: Option<Transition>,
    from_height: f32,
}

#[derive(Clone, Default)]
pub(super) struct MatchLineFrame {
    message: Option<String>,
    current: Option<MatchLine>,
    outgoing: Option<MatchLine>,
    progress: f32,
    height: f32,
    pub(super) animating: bool,
}

impl MatchLineMotion {
    fn height(&self, progress: f32) -> f32 {
        motion::lerp(
            self.from_height,
            f32::from(self.current.is_some()),
            progress,
        )
    }

    fn update(
        &mut self,
        message: Option<&str>,
        line: Option<MatchLine>,
        retain: bool,
        motion: Motion,
        now: Instant,
    ) -> MatchLineFrame {
        if self.message.as_deref() != message {
            *self = Self {
                message: message.map(str::to_owned),
                current: line,
                ..Self::default()
            };
        } else if !retain && self.current != line {
            self.from_height = self.height(motion::progress(self.transition.as_ref(), now));
            self.outgoing = std::mem::replace(&mut self.current, line);
            self.transition = motion.start(motion::FADE, now);
        }
        let progress = motion::progress(self.transition.as_ref(), now);
        if progress >= 1. {
            self.outgoing = None;
            self.transition = None;
        }
        MatchLineFrame {
            message: self.message.clone(),
            current: self.current.clone(),
            outgoing: self.outgoing.clone(),
            height: self.height(progress),
            progress,
            animating: self.transition.is_some(),
        }
    }
}

impl MatchLineFrame {
    const GAP: f32 = 6.;

    fn line_height(theme: &Theme) -> f32 {
        theme.ui_px + 8.
    }

    pub(super) fn reserved_height(&self, theme: &Theme) -> f32 {
        self.height * (Self::line_height(theme) + Self::GAP)
    }

    pub(super) fn render(&self, theme: &Theme, message: &ConvMessage) -> Option<gpui::Div> {
        if self.message.as_deref() != Some(&message.id) || self.height == 0. {
            return None;
        }
        let line_height = Self::line_height(theme);
        let layer = |line: &MatchLine, alpha| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .opacity(alpha)
                .child(line.render(theme, line_height))
        };
        Some(
            div()
                .relative()
                .overflow_hidden()
                .h(px(self.reserved_height(theme)))
                .children(
                    self.outgoing
                        .as_ref()
                        .map(|line| layer(line, 1. - self.progress)),
                )
                .children(self.current.as_ref().map(|line| layer(line, self.progress))),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_search_preview_keeps_the_match_line_after_the_fade() {
        super::super::tests::with_pending_inbox(|inbox, _| {
            inbox.settle_pending_bodies();
            let thread = inbox.session.cursor_id.clone().unwrap();
            let message = MessageId::new(&inbox.session.messages[inbox.session.message_cursor].id);
            inbox.session.pane = Pane::List;
            let search = inbox.session.search.insert(Search::default());
            search.editing = true;
            search.hits.insert(
                thread,
                crate::model::search::Hit {
                    message: message.clone(),
                    related: false,
                    preview: None,
                    folder: Some("INBOX".into()),
                },
            );
            search.matches.push(fell_core::SearchMatch {
                message,
                preview: None,
            });

            let now = Instant::now();
            assert!(inbox.search_match_frame(now, false).current.is_none());
            assert!(inbox.search_match_frame(now, true).animating);
            assert!(!inbox.search_match_frame(now + motion::FADE, true).animating);
            assert!(
                inbox
                    .search_preview_for((true, true), false, "Subject")
                    .is_some()
            );

            let frame = CachedPreview::frame(inbox).match_line;
            assert_eq!(
                frame.current.map(|line| (line.position, line.total)),
                Some((1, 1))
            );
            assert_eq!(frame.height, 1.);
            assert!(frame.outgoing.is_none());
        });
    }

    #[test]
    fn search_details_use_spare_width_and_reserve_room_for_the_subject() {
        assert_eq!(detail_budget(400., 300., 110., 100.), 180.);
        assert_eq!(detail_budget(400., 300., 155., 100.), 145.);
        assert_eq!(detail_budget(400., 300., 270., 100.), 100.);
        assert_eq!(detail_budget(260., 150., 270., 100.), 78.);
    }

    #[test]
    fn narrow_search_rows_fit_compact_dates_without_crowding_the_subject() {
        let mut row = present::ThreadRow {
            muted: false,
            wake_at: None,
            id: "field-report".into(),
            spans: vec![present::NameSpan {
                text: "Regional Field Survey Coordinator".into(),
                emphasis: true,
            }],
            extra: 0,
            message_count: 1,
            starred: false,
            has_attachment: false,
            date_unix: 0,
            subject: "Field survey notes and equipment list for the west ridge".into(),
            snippet: String::new(),
            labels: Vec::new(),
            warn: false,
            unread: false,
            draft: None,
            server_draft: None,
        };
        // Deterministic glyph widths keep this row test independent of local fonts.
        let shape = |text: &str, kind, room: Option<f32>| {
            if text.is_empty() {
                return None;
            }
            let glyph = match kind {
                RowText::Detail => 7.5,
                RowText::Subject => 8.0,
                RowText::Mark => 14.0,
            };
            let text = match room {
                Some(room) if text.chars().count() as f32 * glyph > room => {
                    let keep = (room / glyph).floor() as usize;
                    let head: String = text.chars().take(keep.saturating_sub(1)).collect();
                    present::ellipsis(&head)
                }
                _ => text.to_owned(),
            };
            let mut line = gpui::ShapedLine::default();
            Arc::get_mut(&mut *line).unwrap().width = px(text.chars().count() as f32 * glyph);
            line.text = text.into();
            Some(line)
        };
        let folder = Some("Projects/Field Operations/West Ridge Coordination");
        for (width, dates, marked, expected) in [
            (280.0, ["Nov 21, 2025", "11/21/25"], false, Some("11/21/25")),
            (280.0, ["Yesterday", "Yest."], true, Some("Yest.")),
            (245.0, ["Yesterday", "Yest."], true, None),
            (380.0, ["Yesterday", "Yest."], false, Some("Yesterday")),
        ] {
            row.starred = marked;
            row.has_attachment = marked;
            row.message_count = if marked { 3 } else { 1 };
            let layout = fit_row_line(&row, width, folder, false, dates, shape);
            assert_eq!(layout.star.is_some(), marked);
            assert_eq!(layout.attachment.is_some(), marked);
            assert_eq!(
                layout.detail[1]
                    .as_ref()
                    .map(|line| line.text.trim_start_matches(" · ")),
                expected,
            );
            let sender = layout.sender.unwrap();
            assert!(sender.text.ends_with('…'));
            let count_width = layout.count.as_ref().map_or(0.0, |count| {
                f32::from(shape(count, RowText::Detail, None).unwrap().width).ceil() + SENDER_GAP
            });
            assert!(f32::from(sender.width).ceil() + count_width <= layout.sender_width);
            let row_room = row_text_width(width);
            let room = row_room
                - layout.sender_width
                - if marked { 2.0 * (14.0 + ROW_GAP) } else { 0.0 }
                - 2.0 * ROW_GAP;
            let detail_width: f32 = layout
                .detail
                .iter()
                .flatten()
                .map(|line| f32::from(line.width).ceil())
                .sum();
            let subject = layout.subject.unwrap();
            assert!(detail_width <= detail_budget(row_room, room, 0.0, 0.0));
            assert!(subject.text.starts_with("Field"));
            assert!(subject.text.ends_with('…'));
            assert!(f32::from(subject.width) >= MIN_SUBJECT);
            assert!(f32::from(subject.width) + detail_width <= room);
        }
    }
}
#[test]
fn old_correspondent_rows_cannot_accept_during_or_after_a_new_query() {
    let mut view = SearchView::default();
    let motion = motion::Motion::new(false);
    let now = Instant::now();
    view.correspondent_menu.show(
        vec![fell_core::Address {
            name: None,
            email: "demo@example.test".into(),
        }],
        0,
        motion,
        now,
    );
    let shown = view.completion_revision.load(Ordering::Relaxed);
    assert!(view.correspondent_rows_current(shown));
    view.completion_pending = true;
    let latest = view.completion_revision.fetch_add(1, Ordering::Relaxed) + 1;
    assert!(!view.correspondent_rows_current(shown));
    assert!(!view.correspondent_rows_current(latest));
    view.completion_pending = false;
    assert!(!view.correspondent_rows_current(shown));
    assert!(view.correspondent_rows_current(latest));
}

#[test]
fn tab_before_the_store_is_ready_accepts_the_first_correspondent_reply() {
    let Some(root) = fell_core::test_support::in_isolated_process() else {
        return;
    };
    let mut app = gpui::TestAppContext::single();
    let view = app.new(|cx| {
        let startup = crate::mailbox::Startup {
            keymap: Default::default(),
            split: 0.42,
            read_dwell_ms: 0,
            error: None,
            account_missing: true,
            config: None,
            path: Ok(root.join("mail.db")),
        };
        let mut inbox = Inbox::with_startup(startup, true, None, cx);
        inbox.session.on_key(KeyId::Char('/'), 10);
        assert!(inbox.session.type_search("from:ru").search_completion);
        inbox.request_search_completion(cx);
        assert!(inbox.correspondent_key(KeyId::Tab));
        assert!(inbox.search_view.accept_pending);
        inbox
    });
    // The store opens on a real background thread, so this waits on wall
    // time. The deadline only guards against a hang: a loaded machine can
    // take seconds to open the store.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        app.run_until_parked();
        app.executor().advance_clock(recipient_menu::QUERY_DELAY);
        app.run_until_parked();
        let query = view.read_with(&app, |inbox, _| {
            inbox.session.search.as_ref().unwrap().query.clone()
        });
        if query == "from:ruben@harbor.test " {
            break;
        }
        assert!(Instant::now() < deadline, "lookup ended with {query:?}");
        std::thread::sleep(Duration::from_millis(1));
    }
}
