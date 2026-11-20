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
    arrival: Option<motion::Transition>,
}

#[derive(PartialEq)]
struct PreviewKey {
    conversation: u64,
    subject: String,
    theme: Theme,
    offset: gpui::Pixels,
    split: f32,
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
                    ..ConvFrame::default()
                };
                conversation(
                    &inbox.theme,
                    &inbox.session,
                    inbox.shown_subject(),
                    &inbox.motion,
                    inbox.frame_now,
                    Some(inbox.conv_scroll.clone()),
                    width,
                    Live(true),
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
    pub(super) fn search_preview_for(
        &mut self,
        panes: (bool, bool),
        animating: bool,
        subject: &str,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyView> {
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
            self.search_preview.update(cx, |_, cx| cx.notify());
        }
        let mut box_style = div().flex_1().min_h_0().min_w_0();
        Some(gpui::AnyView::from(self.search_preview.clone()).cached(box_style.style().clone()))
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
            self.search_view
                .current
                .store(generation, Ordering::Relaxed);
        }
        if step.search_request && self.search_view.ready {
            self.request_search(cx);
        }
        if step.search_completion && self.search_view.ready {
            self.request_search_completion(cx);
        }
        switched || cleared
    }

    pub(super) fn refresh_search_after_write(&mut self, cx: &mut Context<Self>) {
        if self.session.refresh_search() {
            self.cancel_search_work();
            self.request_search(cx);
        }
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
        self.session.picked.clear();
        self.session.visual = false;
        self.session.anchor_id = None;
        self.change_mailbox(
            move |store| match removal {
                Removal::Delete => store.delete_scoped_threads(&account, &scopes),
                Removal::Spam => store.spam_scoped_threads(&account, &scopes),
                Removal::Purge => {
                    let threads: Vec<_> = scopes.iter().map(|(thread, _)| thread.clone()).collect();
                    store.purge_threads(&account, &threads, &scopes[0].1)
                }
            },
            move |inbox, result| match result {
                Ok(_) => {
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
            if store
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
            }
            Ok::<_, fell_core::Error>((rows, page, select))
        });
        cx.spawn(async move |this, cx| {
            let Ok(Ok((rows, page, select))) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, _| {
                if inbox.search_view.folder_revision == revision {
                    inbox
                        .session
                        .refresh_saved_folder(rows, &page, select.as_deref());
                }
            });
        })
        .detach();
    }

    pub(super) fn request_search(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &self.session.search else {
            return;
        };
        let query = search.request().to_owned();
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
            let response = store.call(move |store| {
                if current.load(Ordering::Relaxed) != generation {
                    return None;
                }
                let start = Instant::now();
                let results = store.search(&account, &query);
                let end = Instant::now();
                let reply = (current.load(Ordering::Relaxed) == generation).then(|| {
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
                    SearchReply::new(results, initial, locations)
                });
                Some((start, end, reply))
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
                        )),
                    )
                }
            };
            let _ = this.update(cx, |inbox, cx| {
                if let Some(log) = &inbox.frames {
                    log.borrow_mut().search_work(response.0, response.1);
                }
                let Some(mut reply) = response.2 else {
                    return;
                };
                let log = inbox.frames.clone();
                FrameLog::event(log.as_ref(), "search_results", || {
                    if inbox.session.search_generation != generation {
                        return;
                    }
                    let old_id = inbox.session.cursor_id.clone();
                    let old_scope = old_id
                        .as_ref()
                        .and_then(|id| inbox.session.search.as_ref()?.hits.get(id)?.folder.clone());
                    let mailbox_changed = inbox
                        .session
                        .search
                        .as_ref()
                        .is_some_and(Search::mailbox_changed);
                    let old_index = inbox.session.selected_index();
                    let offset = inbox.list_offset();
                    if inbox.session.apply_search_reply(generation, &mut reply) {
                        if let Some(preferred) = inbox.search_view.prefer.take() {
                            inbox.session.select_if_present(&preferred);
                        }
                        let search = inbox.session.search.as_ref().unwrap();
                        inbox
                            .search_view
                            .accepted
                            .store(generation, Ordering::Relaxed);
                        inbox.search_view.preview_asked.clear();
                        inbox.search_view.preview_asked.extend(
                            inbox
                                .session
                                .threads
                                .iter()
                                .take(PREVIEW_BATCH)
                                .filter_map(|row| search.hits.get(&row.id))
                                .map(|hit| hit.message.clone()),
                        );
                        if old_id == inbox.session.cursor_id {
                            if let Some((old, new)) = old_index.zip(inbox.session.selected_index())
                            {
                                inbox.set_list_offset(offset + (old as f32 - new as f32) * ROW_PX);
                            }
                        } else if old_id.is_none() {
                            inbox.set_list_offset(0.0);
                            inbox.search_view.arrival = inbox
                                .motion
                                .motion
                                .start(motion::QUICK, inbox.motion.clock.now());
                        } else {
                            inbox.scroll_list_to_cursor();
                        }
                        let same_preview = !mailbox_changed
                            && old_id == inbox.session.cursor_id
                            && inbox.session.cursor_id == inbox.open_thread
                            && inbox
                                .session
                                .cursor_id
                                .as_deref()
                                .and_then(|id| inbox.session.search_source(id))
                                == old_scope.as_deref();
                        if same_preview && inbox.open_thread.is_some() {
                            inbox.refresh_search_matches(cx);
                        } else {
                            inbox.request_messages(true, cx);
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
            });
        })
        .detach();
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
                {
                    if inbox
                        .session
                        .set_search_matches(accepted, &id, matches, false)
                    {
                        inbox.land_on_reader();
                    }
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
        // This lane does not hold up ranked results or conversation reads.
        let reply = self.store.call_maintenance(move |store| {
            if latest.load(Ordering::Relaxed) == revision {
                search_suggestions(store, &account, field, &prefix)
            } else {
                Vec::new()
            }
        });
        cx.spawn(async move |this, cx| {
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

    pub(super) fn queue_search_previews(
        &mut self,
        range: Range<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(search) = self
            .session
            .search
            .as_ref()
            .filter(|search| !search.pending)
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
                    if search.pending {
                        for (_, id) in batch {
                            inbox.search_view.preview_asked.remove(&id);
                        }
                        return;
                    }
                    for (thread, id) in batch {
                        if let Some(hit) = search.hits.get_mut(&thread) {
                            hit.preview = previews.remove(&id);
                        }
                    }
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
        .filter(|_| search.editing && (search.pending || search.correction.is_none()));
    let correction = search.correction.as_ref().filter(|_| !search.pending);
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

pub(super) fn empty(theme: &Theme, search: &Search) -> gpui::Div {
    let text = if search.pending
        || search.query.trim().is_empty()
        || (search.editing && !search.suggestions.is_empty())
    {
        ""
    } else if search.error.is_some() {
        "Check your search"
    } else {
        "No mail matches · Esc to go back"
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
        .children(
            search
                .error
                .as_ref()
                .filter(|_| text == "Check your search")
                .map(|error| div().text_color(rgb(theme.dimmed)).child(error.clone())),
        )
}

pub(super) fn row(
    theme: &Theme,
    inbox: &Inbox,
    index: usize,
    highlighted: &HashSet<String>,
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
    let date = present::relative_date(now_unix(), row.date_unix);
    let detail = match &hit.folder {
        Some(folder) if hit.related => format!("Related · {}", inbox.session.roles.label(folder)),
        None if hit.related => "Related".to_owned(),
        Some(folder) => format!("{} · {date}", inbox.session.roles.label(folder)),
        None => date,
    };
    let excerpt = match &hit.preview {
        Some(preview) if !preview.highlights.is_empty() => {
            let (text, highlights) = visible_excerpt(preview);
            let highlight = HighlightStyle {
                color: Some(rgb(if selected { foreground } else { theme.accent }).into()),
                font_weight: Some(FontWeight::SEMIBOLD),
                background_color: Some(
                    rgb(motion::mix(
                        background,
                        if selected { foreground } else { theme.accent },
                        0.14,
                    ))
                    .into(),
                ),
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
        .px(px(14.))
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
                    !search.pending && search.accepted_generation() == generation
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
                .gap(px(8.))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(if row.subject.is_empty() {
                            "(No subject)".to_owned()
                        } else {
                            row.subject.clone()
                        }),
                )
                .when(row.starred, |line| {
                    line.child(div().flex_none().text_color(rgb(theme.star)).child("★"))
                })
                .child(
                    div()
                        .flex_none()
                        .max_w_1_2()
                        .truncate()
                        .text_color(rgb(dim))
                        .child(detail),
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
