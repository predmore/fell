//! Store dispatch and drawing for the inbox's search mode.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use fell_core::{MessageId, SearchPreview, Store};
use gpui::{HighlightStyle, StyledText};

use super::*;
use crate::model::search::{Completion, CompletionField, Search, SearchReply, completion};

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
    arrival: Option<motion::Transition>,
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
    }

    pub(super) fn search_text_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
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
        let Some(text) = text else {
            return false;
        };
        if let Some(log) = &self.frames {
            log.borrow_mut().key(Instant::now(), stroke.unparse());
        }
        self.settle_motion();
        let before = self.before_step();
        let step = self.session.type_search(&text);
        self.apply_step(step, before, cx);
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
        let changed = searching != was_searching || previous != generation;
        self.search_view.arrival = None;
        if changed {
            if searching && !was_searching {
                self.search_view.folder_scroll = Some(std::mem::replace(
                    &mut self.list_scroll,
                    UniformListScrollHandle::new(),
                ));
            } else if !searching && let Some(scroll) = self.search_view.folder_scroll.take() {
                self.list_scroll = scroll;
            }
            self.motion.list = None;
            self.motion.list_glide = None;
            self.motion.conv_glide = None;
            self.search_view
                .current
                .store(generation, Ordering::Relaxed);
            self.search_view.preview_asked.clear();
            if searching {
                self.set_list_offset(0.0);
            }
            self.conv_gen.fetch_add(1, Ordering::SeqCst);
            self.clear_open_message();
        }
        if step.search_request && self.search_view.ready {
            self.request_search(cx);
        } else if step.search_completion {
            self.request_search_completion(cx);
        }
        changed
    }

    pub(super) fn request_search(&mut self, cx: &mut Context<Self>) {
        let Some(search) = &self.session.search else {
            return;
        };
        let query = search.query.clone();
        let caret = search.caret;
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
                    let suggestions =
                        search_suggestions(store, &account, completion(&query, caret));
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
                    SearchReply::new(results, suggestions, initial)
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
                            Vec::new(),
                            HashMap::new(),
                        )),
                    )
                }
            };
            let _ = this.update(cx, |inbox, cx| {
                if let Some(log) = &inbox.frames {
                    log.borrow_mut().search_work(response.0, response.1);
                }
                let Some(reply) = response.2 else {
                    return;
                };
                let log = inbox.frames.clone();
                FrameLog::event(log.as_ref(), "search_results", || {
                    if inbox.session.apply_search_reply(generation, caret, reply) {
                        let search = inbox.session.search.as_ref().unwrap();
                        inbox.search_view.preview_asked.extend(
                            inbox
                                .session
                                .threads
                                .iter()
                                .take(PREVIEW_BATCH)
                                .filter_map(|row| search.hits.get(&row.id))
                                .map(|hit| hit.message.clone()),
                        );
                        let now = inbox.motion.clock.now();
                        inbox.search_view.arrival = inbox.motion.motion.start(motion::QUICK, now);
                        // A split-width inbox previews the selected conversation.
                        inbox.request_messages(false, cx);
                        cx.notify();
                    }
                });
            });
        })
        .detach();
    }

    fn request_search_completion(&mut self, cx: &mut Context<Self>) {
        let search = self.session.search.as_ref().unwrap();
        if completion(&search.query, search.caret).is_none() {
            return;
        }
        let query = search.query.clone();
        let caret = search.caret;
        let generation = self.session.search_generation;
        let account = self.account_id.clone();
        let current = Arc::clone(&self.search_view.current);
        let reply = self.store.call_background(move |store| {
            if current.load(Ordering::Relaxed) == generation {
                search_suggestions(store, &account, completion(&query, caret))
            } else {
                Vec::new()
            }
        });
        cx.spawn(async move |this, cx| {
            let Ok(suggestions) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if inbox.session.search_generation == generation
                    && let Some(search) = inbox.session.search.as_mut()
                    && search.caret == caret
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
        let search = self.session.search.as_ref().unwrap();
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
            let generation = self.session.search_generation;
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
        if self.session.search_generation != generation {
            return;
        }
        let Some(search) = self.session.search.as_ref() else {
            return;
        };
        let query = search.effective_query().to_owned();
        let account = self.account_id.clone();
        let current = Arc::clone(&self.search_view.current);
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
                if inbox.session.search_generation == generation
                    && let Some(search) = inbox.session.search.as_mut()
                {
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
    active: Option<Completion<'_>>,
) -> Vec<String> {
    let Some(Completion { field, prefix, .. }) = active else {
        return Vec::new();
    };
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
        .filter(|_| search.editing && search.correction.is_none());
    let line = if let Some(correction) = &search.correction {
        Some((format!("Did you mean {correction}?"), search.editing))
    } else if let Some(value) = suggestion {
        Some((format!("Complete with {value}"), true))
    } else if count > 0 && !search.pending {
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
                .text_color(rgb(if accept || search.correction.is_some() {
                    theme.accent
                } else {
                    theme.dimmed
                }))
                .child(div().flex_1().min_w_0().truncate().child(label))
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
    let detail = if hit.related {
        "Related".to_owned()
    } else {
        present::relative_date(now_unix(), row.date_unix)
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
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |inbox, _, window, cx| {
                if inbox.session.search.is_some() {
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
                .child(div().flex_none().text_color(rgb(dim)).child(detail)),
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
