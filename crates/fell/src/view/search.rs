//! Store dispatch and drawing for the inbox's search mode.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fell_core::{MessageId, SearchPreview, Store};
use gpui::{HighlightStyle, StyledText};

use super::*;
use crate::model::search::{CompletionField, Search, SearchReply};

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
    pub(super) folder_revision: u64,
    pub(super) prefer: Option<String>,
    preview_key: Option<PreviewKey>,
    preview_revision: u64,
    rows_key: Option<RowsKey>,
    rows_revision: u64,
    rows_previews: u64,
    rows_were_current: bool,
    arrival: Option<motion::Transition>,
    settling_reply: Option<SearchReply>,
}

#[derive(PartialEq)]
struct PreviewKey {
    conversation: u64,
    subject: String,
    theme: Theme,
    offset: gpui::Pixels,
    split: f32,
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
}

impl Render for CachedPreview {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.0
            .update(cx, |inbox, cx| {
                let width = f32::from(window.viewport_size().width);
                let width = width - list_width(width, inbox.split) - SPLITTER_PX;
                let frame = ConvFrame {
                    anchor: inbox.hold_landing(),
                    frames: inbox.frames.clone(),
                    // The outer pane owns the shield when the preview is stale.
                    preview_current: true,
                    ..ConvFrame::default()
                };
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
        if self
            .session
            .search
            .as_ref()
            .is_some_and(|search| search.pending)
        {
            self.request_search(cx);
        }
        if self.session.search.is_some() {
            self.request_search_completion(cx);
        }
    }

    pub(super) fn search_edit_key(
        &mut self,
        event: &KeyDownEvent,
        started: Instant,
        cx: &mut Context<Self>,
    ) -> bool {
        let stroke = &event.keystroke;
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
            self.search_view.prefer = None;
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
        if step.search_completion && self.search_view.ready {
            self.request_search_completion(cx);
        }
        switched || cleared
    }

    pub(super) fn refresh_search(&mut self, cx: &mut Context<Self>) {
        if self.session.refresh_search() {
            self.cancel_search_work();
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
                None
            } else {
                store
                    .search_previews(&account, &query, std::slice::from_ref(&id))
                    .ok()
                    .and_then(|mut previews| previews.remove(&id))
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
                } else if inbox
                    .session
                    .set_search_snippet(generation, &thread, &message, &snippet, preview)
                {
                    inbox.search_view.rows_previews =
                        inbox.search_view.rows_previews.wrapping_add(1);
                    cx.notify();
                }
            });
        })
        .detach();
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
            .filter(|scopes| !scopes.is_empty())
        else {
            self.session
                .show_toast("No folder for a selected match", true);
            return;
        };
        let key = self.selected_key();
        let account = self.account_id.clone();
        let done = match removal {
            Removal::Delete => format!(
                "Moved to {}",
                self.session
                    .folder_text(self.session.roles.name(FolderRole::Trash))
            ),
            Removal::Spam => "Spam status updated".to_owned(),
            Removal::Purge => "Removed from Fell".to_owned(),
        };
        let accepted = self
            .session
            .search
            .as_ref()
            .and_then(Search::accepted_generation);
        self.change_mailbox(
            move |store| match removal {
                Removal::Delete => store.delete_scoped_threads(&account, &scopes),
                Removal::Spam => store.spam_scoped_threads(&account, &scopes),
                Removal::Purge => store.purge_scoped_threads(&account, &scopes),
            },
            move |inbox, result| match result {
                Ok(_) => {
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
                    inbox.moved_thread = if removal == Removal::Purge { None } else { key };
                    inbox.session.show_toast(done, false);
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
        let reply = self.store.call_background(move |store| {
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
                let recipients = store.folder_recipients(&account, &folder, &ids)?;
                present::address_draft_rows(&mut rows, &recipients, &me);
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

    fn show_search_reply(
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
            let old_index = self.session.selected_index();
            let offset = self.list_offset();
            if self.session.apply_search_reply(generation, &mut reply) {
                if !self.session.search_results_current() {
                    cx.notify();
                    return;
                }
                if let Some(preferred) = self.search_view.prefer.take() {
                    self.session.select_if_present(&preferred);
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
                let same_preview = !mailbox_changed
                    && old_id == self.session.cursor_id
                    && self.session.cursor_id == self.open_thread
                    && self
                        .session
                        .cursor_id
                        .as_deref()
                        .and_then(|id| self.session.search_source(id))
                        == old_scope.as_deref();
                if same_preview && self.open_thread.is_some() {
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
                    if inbox
                        .session
                        .set_search_matches(accepted, &id, matches, false)
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
        let latest = Arc::clone(&self.search_view.completion_revision);
        let revision = latest.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        let Some((field, prefix)) = self
            .session
            .search
            .as_ref()
            .and_then(|search| search.completion_key())
            .cloned()
        else {
            return;
        };
        let account = self.account_id.clone();
        let store = self.store.clone();
        cx.spawn(async move |this, cx| {
            let reply = store.call_search_completion(move |store| {
                if latest.load(Ordering::Relaxed) == revision {
                    search_suggestions(store, &account, field, &prefix)
                } else {
                    Vec::new()
                }
            });
            let Ok(suggestions) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if inbox
                    .search_view
                    .completion_revision
                    .load(Ordering::Relaxed)
                    == revision
                    && let Some(search) = inbox.session.search.as_mut()
                {
                    search.suggestions = suggestions;
                    search.suggestion_index = 0;
                    cx.notify();
                }
            });
        })
        .detach();
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
                (!hit.related
                    && hit.preview.is_none()
                    && self.search_view.preview_asked.insert(hit.message.clone()))
                .then(|| (row.id.clone(), hit.message.clone()))
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
        batch: Vec<(String, MessageId)>,
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
                let ids: Vec<_> = chunk.iter().map(|(_, id)| id.clone()).collect();
                previews.extend(
                    store
                        .search_previews(&account, &query, &ids)
                        .unwrap_or_default(),
                );
            }
            Some((batch, previews))
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some((batch, mut previews))) = reply.await else {
                return;
            };
            if previews.is_empty() {
                return;
            }
            let _ = this.update(cx, |inbox, cx| {
                if let Some(search) = inbox
                    .session
                    .search
                    .as_mut()
                    .filter(|search| search.accepted_generation() == Some(generation))
                {
                    if !search.results_current() {
                        for (_, id) in batch {
                            inbox.search_view.preview_asked.remove(&id);
                        }
                        // The cached list must get a chance to ask again.
                        inbox.search_view.rows_were_current = false;
                        return;
                    }
                    for (thread, id) in batch {
                        if let Some(hit) = search.hits.get_mut(&thread)
                            && hit.preview.is_none()
                        {
                            hit.preview = previews.remove(&id);
                        }
                    }
                    inbox.search_view.rows_previews =
                        inbox.search_view.rows_previews.wrapping_add(1);
                    cx.notify();
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
) -> Vec<String> {
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
        .filter(|_| search.editing && (!search.results_current() || search.correction.is_none()));
    let correction = search
        .correction
        .as_ref()
        .filter(|_| search.results_current());
    let line = if let Some(correction) = correction {
        Some((format!("Did you mean {correction}?"), true))
    } else if let Some(value) = suggestion {
        Some((format!("Complete with {value}"), true))
    } else if search.pending {
        Some((
            (if count > 0 {
                "Searching… · showing previous results"
            } else {
                "Searching…"
            })
            .to_owned(),
            false,
        ))
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
                .when(search.pending && suggestion.is_some(), |row| {
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

fn detail_budget(row_room: f32, room: f32, subject: f32, minimum: f32) -> f32 {
    (room - subject.max(MIN_SUBJECT))
        .max(minimum)
        .min(row_room * 0.45)
        .min((room - MIN_SUBJECT).max(0.0))
        .floor()
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
    const PAD: f32 = 14.0;
    const SENDER_GAP: f32 = 5.0;
    let size = px(theme.ui_px - 2.0);
    let style = conversation_line_style(window, theme, size);
    let subject_style = conversation_line_style(window, theme, px(theme.ui_px));
    let measure = |style: &TextStyle, text: &str, weight| {
        line_px_with_style(window, style, text, weight).ceil()
    };
    let row_room = (list_width - LIST_BORDER_PX - 2.0 * PAD).max(0.0);
    let sender = &row.spans[0].text;
    let count = (row.message_count > 1).then(|| format!("· {}", row.message_count));
    let sender_width = (measure(&style, sender, FontWeight::NORMAL)
        + count.as_deref().map_or(0.0, |count| {
            SENDER_GAP + measure(&style, count, FontWeight::NORMAL)
        }))
    .min((row_room * 0.30).floor());
    let marks = [
        row.starred.then_some(("★", theme.star)),
        row.has_attachment.then_some((THREAD_ATTACHMENT_MARK, dim)),
    ];
    let marks = marks.into_iter().flatten().map(|(text, color)| {
        (
            text,
            color,
            measure(&subject_style, text, FontWeight::NORMAL),
        )
    });
    let marks_width: f32 = marks.clone().map(|(_, _, width)| width + ROW_GAP).sum();
    let room = (row_room - sender_width - marks_width - 2.0 * ROW_GAP).max(0.0);
    let subject = if row.subject.is_empty() {
        "(No subject)"
    } else {
        &row.subject
    };
    // Reserve the timestamp and enough of the final folder word to
    // distinguish sibling folders when the subject needs clipping.
    let date = (!hit.related)
        .then(|| present::relative_date(now_unix(), row.date_unix))
        .filter(|date| !date.is_empty());
    let shape_detail =
        |text: &str| shape_ui_line(window, &style, text, FontWeight::NORMAL, Some(dim));
    let date_text = date.as_deref().and_then(shape_detail);
    let dated_location = date
        .as_ref()
        .filter(|_| hit.folder.is_some())
        .and_then(|date| shape_detail(&format!(" · {date}")));
    let related_prefix = if hit.related { "Related · " } else { "" };
    let folder = hit
        .folder
        .as_ref()
        .map(|folder| inbox.session.roles.label(folder));
    let line_width = |line: &gpui::ShapedLine| f32::from(line.width).ceil();
    let date_width = dated_location
        .as_ref()
        .or(date_text.as_ref())
        .map_or(0.0, line_width);
    let related = if hit.related {
        shape_detail("Related")
    } else {
        None
    };
    let location_min = folder.as_deref().map_or_else(
        || related.as_ref().map_or(0.0, line_width),
        |folder| {
            let leaf = folder.rsplit('/').next().unwrap_or(folder);
            let last = leaf.split_whitespace().next_back().unwrap_or(leaf);
            let suffix = if last == leaf { "" } else { "…" };
            measure(
                &style,
                &format!("{related_prefix}{suffix}{last}"),
                FontWeight::NORMAL,
            )
            .min(120.0)
        },
    );
    let detail_room = detail_budget(
        row_room,
        room,
        measure(&subject_style, subject, FontWeight::SEMIBOLD),
        date_width + location_min,
    );
    let location_room = detail_room - date_width;
    let location = folder
        .as_deref()
        .and_then(|folder| {
            fit_folder_path(
                folder,
                FolderFit::Search,
                |path| {
                    let text = format!("{related_prefix}{path}");
                    clip_shaped(
                        window,
                        &style,
                        &text,
                        location_room,
                        FontWeight::NORMAL,
                        Some(dim),
                    )
                    .filter(|line| line_width(line) <= location_room)
                },
                |line| line.text.strip_prefix(related_prefix).unwrap_or_default(),
            )
        })
        .or_else(|| related.filter(|line| line_width(line) <= detail_room));
    let date = if location.is_some() {
        dated_location
    } else {
        date_text
    };
    let detail = location.into_iter().chain(date);
    let subject_room = room - detail.clone().map(|line| line_width(&line)).sum::<f32>();
    let subject = clip_shaped(
        window,
        &subject_style,
        subject,
        subject_room,
        FontWeight::SEMIBOLD,
        Some(foreground),
    );
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
            StyledText::new(text)
                .with_highlights(highlights.into_iter().map(|range| (range, highlight)))
        }
        _ => StyledText::new(row.snippet.clone()),
    };
    div()
        .id(gpui::ElementId::named_usize("search-row", index))
        .relative()
        .h(px(ROW_PX))
        .w_full()
        .px(px(PAD))
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
                        .child(div().min_w_0().truncate().child(sender.clone()))
                        .children(count.map(|count| div().flex_none().child(count))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .children(subject.map(|line| shaped_text(line, window))),
                )
                .children(marks.map(|(text, color, width)| {
                    div()
                        .flex_none()
                        .w(px(width))
                        .text_color(rgb(color))
                        .child(text)
                }))
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .children(detail.map(|line| {
                            let width = line_width(&line);
                            shaped_text(line, window).w(px(width))
                        })),
                ),
        )
        .child(div().truncate().text_color(rgb(dim)).child(excerpt))
        .into_any_element()
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

pub(super) fn match_reason(
    theme: &Theme,
    session: &Session,
    message: &ConvMessage,
) -> Option<gpui::Div> {
    let search = session
        .search
        .as_ref()
        .filter(|_| session.pane == Pane::Conversation)?;
    if session.messages.get(session.message_cursor)?.id != message.id
        || search.hits.get(session.cursor_id.as_deref()?)?.related
    {
        return None;
    }
    let position = session.messages[..session.message_cursor]
        .iter()
        .filter(|message| search.matches.iter().any(|hit| hit.message.0 == message.id))
        .count()
        + 1;
    let hit = search
        .matches
        .iter()
        .find(|hit| hit.message.0 == message.id)?;
    let line = div()
        .flex()
        .items_center()
        .gap(px(9.))
        .pb(px(6.))
        .min_w_0()
        .child(
            div()
                .flex_none()
                .text_color(rgb(theme.accent))
                .child(format!("Match {position} of {}", search.matches.len())),
        );
    Some(
        line.children(
            hit.preview
                .as_ref()
                .filter(|preview| !preview.highlights.is_empty())
                .map(|preview| {
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
                }),
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::detail_budget;

    #[test]
    fn search_details_use_spare_width_and_reserve_room_for_the_subject() {
        assert_eq!(detail_budget(400., 300., 110., 100.), 180.);
        assert_eq!(detail_budget(400., 300., 155., 100.), 145.);
        assert_eq!(detail_budget(400., 300., 270., 100.), 100.);
        assert_eq!(detail_budget(260., 150., 270., 100.), 78.);
    }
}
