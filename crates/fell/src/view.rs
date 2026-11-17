//! Two-pane inbox. The window reads the local store and never talks to IMAP.

use std::time::{SystemTime, UNIX_EPOCH};

use fell_core::{AccountId, Store, ThreadId};
use gpui::{
    App, Application, Bounds, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, Keystroke, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Render,
    ScrollHandle, ScrollStrategy, StatefulInteractiveElement, Styled, TitlebarOptions,
    UniformListScrollHandle, Window, WindowBounds, WindowOptions, div, point, prelude::*, px, rgb,
    size, uniform_list,
};

use crate::keymap::KeyId;
use crate::mailbox;
use crate::model::{Overlay, QueryFault, Session, Step};
use crate::present::{self, ConvMessage, NameSpan, Pane, ThreadRow};
use crate::text_render::BodyBlock;
use crate::theme::{self, Theme};

pub fn run() {
    Application::new().run(|cx: &mut App| {
        let (width, height) = window_size();
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let view = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Fell".into()),
                    ..Default::default()
                }),
                app_id: Some("dev.predmore.fell".into()),
                focus: true,
                show: true,
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(Inbox::new);
                let focus = view.read(cx).focus.clone();
                window.focus(&focus);
                view
            },
        );
        if view.is_err() {
            eprintln!("fell: could not open a window");
        }
        cx.activate(true);
    });
}

struct Inbox {
    focus: FocusHandle,
    store: Store,
    account_id: AccountId,
    session: Session,
    theme: Theme,
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    palette_scroll: ScrollHandle,
    folder_scroll: ScrollHandle,
    split: f32,
    dragging: bool,
    open_thread: Option<String>,
}

impl Inbox {
    fn new(cx: &mut Context<Self>) -> Self {
        let mailbox = mailbox::open();
        let theme = theme::load();
        let mut inbox = Self {
            focus: cx.focus_handle(),
            account_id: mailbox.account.id.clone(),
            session: Session::new(
                mailbox.account.display_name.clone(),
                mailbox.account.email.clone(),
                mailbox.sync,
                mailbox.keymap,
            ),
            store: mailbox.store,
            theme,
            list_scroll: UniformListScrollHandle::new(),
            conv_scroll: ScrollHandle::new(),
            palette_scroll: ScrollHandle::new(),
            folder_scroll: ScrollHandle::new(),
            split: mailbox.split,
            dragging: false,
            open_thread: None,
        };
        if let Some(error) = mailbox.error {
            inbox.session.record_startup_error(error);
        }
        if inbox.reload_threads() && !inbox.reload_messages(true) {
            inbox.clear_open_message();
        }
        inbox.scroll_conversation();
        inbox.watch_theme(cx);
        inbox.watch_dates(cx);
        inbox
    }

    fn watch_theme(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(300))
                    .await;
                let next = theme::load();
                let alive = this
                    .update(cx, |inbox, cx| {
                        if inbox.theme != next {
                            inbox.theme = next;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn watch_dates(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(15))
                    .await;
                let alive = this.update(cx, |_, cx| cx.notify()).is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn reload_threads(&mut self) -> bool {
        match self
            .store
            .folder_threads(&self.account_id, &self.session.folder)
        {
            Ok(threads) => {
                self.session.clear_query_fault(QueryFault::Folder);
                if let Ok(names) = self.store.folder_names(&self.account_id) {
                    self.session.folders = names;
                }
                let rows = present::thread_rows(&threads, &self.session.account_email);
                self.session.set_threads(rows);
                true
            }
            Err(err) => {
                self.session.fail_query(QueryFault::Folder, err.to_string());
                false
            }
        }
    }

    fn reload_messages(&mut self, force: bool) -> bool {
        let Some(id) = self.session.cursor_id.clone() else {
            self.clear_open_message();
            return true;
        };
        if !force && self.open_thread.as_deref() == Some(id.as_str()) {
            return true;
        }
        match self
            .store
            .thread_messages(&self.account_id, &ThreadId(id.clone()))
        {
            Ok(messages) => {
                self.session.clear_query_fault(QueryFault::Message);
                let view = present::conversation(&messages, &self.session.account_email);
                self.session.set_messages(view);
                self.open_thread = Some(id);
                true
            }
            Err(err) => {
                self.session
                    .fail_query(QueryFault::Message, err.to_string());
                false
            }
        }
    }

    fn clear_open_message(&mut self) {
        self.session.messages.clear();
        self.open_thread = None;
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = key_from(&event.keystroke) else {
            return;
        };
        window.focus(&self.focus);
        let page = self.page_rows();
        let message_before = self.session.message_cursor;
        let thread_before = self.session.cursor_id.clone();
        let folder_before = self.session.folder.clone();
        let step = self.session.on_key(key, page);
        self.apply_step(step, message_before, thread_before, folder_before, cx);
        cx.stop_propagation();
    }

    fn apply_step(
        &mut self,
        step: Step,
        message_before: usize,
        thread_before: Option<String>,
        folder_before: String,
        cx: &mut Context<Self>,
    ) {
        let mut loaded_messages = false;
        let mut loaded_threads = false;
        if step.reload_threads {
            if !self.reload_threads() {
                self.session.folder = folder_before;
            } else {
                loaded_threads = true;
                if !self.reload_messages(true) {
                    self.clear_open_message();
                }
                loaded_messages = true;
            }
        } else if step.reload_messages {
            if !self.reload_messages(true) {
                self.session.cursor_id = thread_before.clone();
            } else {
                loaded_messages = true;
            }
        }
        if step.arm_which {
            self.arm_which(cx);
        }
        let thread_moved = self.session.cursor_id != thread_before;
        if loaded_threads || thread_moved {
            self.scroll_list_to_cursor();
        }
        // A tall message aligns to its top when scrolled into view. Skip that
        // unless the reader moved to another message, or a new thread loaded.
        // Paging already moved the viewport by pixels, so it must not jump
        // the current message back to the top.
        if step.scroll_pages != 0 || step.scroll_lines != 0 {
            self.nudge_conversation(step.scroll_pages, step.scroll_lines);
        } else if loaded_messages || self.session.message_cursor != message_before {
            self.scroll_conversation();
        }
        if self.session.overlay == Overlay::Palette {
            self.palette_scroll
                .scroll_to_item(self.session.palette_index);
        }
        if self.session.overlay == Overlay::Folders {
            self.folder_scroll.scroll_to_item(self.session.folder_index);
        }
        cx.notify();
    }

    fn scroll_conversation(&mut self) {
        if !self.session.messages.is_empty() {
            self.conv_scroll.scroll_to_item(self.session.message_cursor);
        }
    }

    fn nudge_conversation(&mut self, pages: i32, lines: i32) {
        let viewport = f32::from(self.conv_scroll.bounds().size.height);
        let viewport = if viewport < 1.0 { 640.0 } else { viewport };
        let delta = pages as f32 * viewport + lines as f32 * 24.0;
        let measured = self.conv_scroll.offset();
        let scrollport_top = f32::from(self.conv_scroll.bounds().top());
        let mut item_bounds = Vec::new();
        let mut index = 0;
        while let Some(bounds) = self.conv_scroll.bounds_for_item(index) {
            item_bounds.push((f32::from(bounds.top()), f32::from(bounds.bottom())));
            index += 1;
        }
        let max = f32::from(self.conv_scroll.max_offset().height);
        let next_y = (f32::from(measured.y) - delta).clamp(-max, 0.0);
        if let Some(ix) =
            present::message_after_page(&item_bounds, scrollport_top, f32::from(measured.y), next_y)
            && ix < self.session.messages.len()
        {
            self.session.message_cursor = ix;
        }
        self.conv_scroll.set_offset(point(measured.x, px(next_y)));
    }

    fn arm_which(&mut self, cx: &mut Context<Self>) {
        let generation = self.session.chord_gen;
        let delay = self.session.keymap.which_key_ms;
        if self.session.chord.is_empty() {
            self.session.which_visible = false;
            return;
        }
        if delay == 0 {
            self.session.which_visible = true;
            return;
        }
        self.session.which_visible = false;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay))
                .await;
            let _ = this.update(cx, |inbox, cx| {
                if inbox.session.chord_gen == generation && !inbox.session.chord.is_empty() {
                    inbox.session.which_visible = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn page_rows(&self) -> usize {
        let height = match self.session.pane {
            Pane::List => f32::from(self.list_scroll.0.borrow().base_handle.bounds().size.height),
            Pane::Conversation => f32::from(self.conv_scroll.bounds().size.height),
        };
        let height = if height < 1.0 { 640.0 } else { height };
        let row = match self.session.pane {
            Pane::List => 68.0,
            Pane::Conversation => 160.0,
        };
        ((height / row).floor() as usize).max(1)
    }

    fn scroll_list_to_cursor(&self) {
        self.list_scroll
            .scroll_to_item(self.session.cursor_index(), ScrollStrategy::Top);
    }

    fn select_thread(&mut self, id: String) {
        self.session.cancel_pending_keys();
        let changed = self.session.cursor_id.as_deref() != Some(id.as_str());
        let previous = self.session.cursor_id.clone();
        self.session.cursor_id = Some(id);
        self.session.pane = Pane::List;
        if changed {
            if !self.reload_messages(true) {
                self.session.cursor_id = previous;
            } else {
                self.scroll_conversation();
            }
        }
        self.scroll_list_to_cursor();
    }
}

impl Render for Inbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = f32::from(window.viewport_size().width);
        let theme = self.theme.clone();
        let session = &self.session;
        let list_scroll = self.list_scroll.clone();
        let conv_scroll = self.conv_scroll.clone();
        let palette_scroll = self.palette_scroll.clone();
        let folder_scroll = self.folder_scroll.clone();
        let focus = self.focus.clone();
        let split = self.split;
        shell(
            theme,
            session,
            width,
            split,
            list_scroll,
            conv_scroll,
            palette_scroll,
            folder_scroll,
            focus,
            cx,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn shell(
    theme: Theme,
    session: &Session,
    width: f32,
    split: f32,
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    palette_scroll: ScrollHandle,
    folder_scroll: ScrollHandle,
    focus: FocusHandle,
    cx: &mut Context<Inbox>,
) -> impl IntoElement {
    let (show_list, show_conv) = present::visible_panes(width, session.pane, session.full_width);
    let list_width = (width * split).clamp(280.0, (width - 400.0).max(280.0));
    div()
        .id("fell-root")
        .track_focus(&focus)
        .on_key_down(cx.listener(|inbox, event: &KeyDownEvent, window, cx| {
            inbox.on_key_down(event, window, cx);
        }))
        .on_mouse_move(cx.listener(|inbox, event: &MouseMoveEvent, window, cx| {
            if !inbox.dragging {
                return;
            }
            let width = f32::from(window.viewport_size().width).max(1.0);
            let x = f32::from(event.position.x);
            inbox.split = (x / width).clamp(0.22, 0.52);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|inbox, _: &MouseUpEvent, _, _| {
                if inbox.dragging {
                    inbox.dragging = false;
                    mailbox::save_split(inbox.split);
                }
            }),
        )
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme.background))
        .text_color(rgb(theme.foreground))
        .text_size(px(theme.ui_px))
        .font_family_opt(theme.ui_family.clone())
        .overflow_hidden()
        .child(header(&theme, session))
        .child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_row()
                .overflow_hidden()
                .when(show_list, |row| {
                    row.child(thread_list(
                        &theme,
                        session,
                        show_conv,
                        list_width,
                        list_scroll,
                        cx,
                    ))
                })
                .when(show_list && show_conv, |row| {
                    row.child(splitter(&theme, cx))
                })
                .when(show_conv, |row| {
                    row.child(conversation(&theme, session, conv_scroll, cx))
                }),
        )
        .child(status_bar(&theme, session))
        .when(
            session.which_visible && session.overlay == Overlay::None && !session.chord.is_empty(),
            |root| root.child(which_key_popup(&theme, session)),
        )
        .when(session.overlay != Overlay::None, |root| {
            root.child(overlay(
                &theme,
                session,
                &palette_scroll,
                &folder_scroll,
                cx,
            ))
        })
}

fn header(theme: &Theme, session: &Session) -> gpui::Div {
    let unread = session.unread_threads();
    div()
        .h(px(44.))
        .px(px(16.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.))
        .border_b(px(1.))
        .border_color(rgb(theme.border))
        .bg(rgb(theme.surface))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(session.account_name.clone()),
        )
        .child(
            div()
                .text_color(rgb(theme.dimmed))
                .child(session.account_email.clone()),
        )
        .child(div().flex_1())
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(session.folder_title()),
        )
        .child(
            div()
                .text_color(rgb(if unread > 0 {
                    theme.accent
                } else {
                    theme.dimmed
                }))
                .child(if unread == 1 {
                    "1 unread".to_string()
                } else {
                    format!("{unread} unread")
                }),
        )
}

fn thread_list(
    theme: &Theme,
    session: &Session,
    beside: bool,
    list_width: f32,
    scroll: UniformListScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    let entity = cx.entity();
    let theme_for_rows = theme.clone();
    let rows = uniform_list(
        "thread-rows",
        session.threads.len(),
        move |range, _window, app| {
            entity.update(app, |inbox, cx| {
                let highlighted = inbox.session.highlighted();
                range
                    .map(|index| {
                        let row = inbox.session.threads[index].clone();
                        thread_row(
                            &theme_for_rows,
                            &inbox.session,
                            &row,
                            index,
                            &highlighted,
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            })
        },
    )
    .flex_1()
    .min_h(px(0.))
    .track_scroll(scroll);
    let mut pane = div()
        .id("thread-list")
        .flex()
        .flex_col()
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_hidden()
        .border_l(px(2.))
        .border_color(rgb(if session.pane == Pane::List {
            theme.accent
        } else {
            theme.background
        }));
    pane = if beside {
        pane.w(px(list_width)).flex_none()
    } else {
        pane.flex_1()
    };
    if session.threads.is_empty() {
        pane.child(empty_state(theme, session))
    } else {
        pane.child(rows)
    }
}

fn thread_row(
    theme: &Theme,
    session: &Session,
    row: &ThreadRow,
    index: usize,
    highlighted: &std::collections::HashSet<String>,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let cursor = session.cursor_id.as_deref() == Some(row.id.as_str());
    let selected = highlighted.contains(&row.id);
    let id = row.id.clone();
    let weight = if row.unread {
        FontWeight::BOLD
    } else {
        FontWeight::NORMAL
    };
    div()
        .id(gpui::ElementId::named_usize("thread", index))
        .h(px(68.))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .bg(rgb(if selected {
            theme.selection
        } else {
            theme.background
        }))
        .text_color(rgb(if selected {
            theme.on_selection
        } else {
            theme.foreground
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |inbox, _, window, cx| {
                window.focus(&inbox.focus);
                inbox.select_thread(id.clone());
                cx.notify();
            }),
        )
        .child(div().w(px(3.)).h_full().bg(rgb(if row.unread || cursor {
            theme.accent
        } else {
            theme.background
        })))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .px(px(12.))
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(3.))
                .child(thread_line_one(theme, row, weight, selected))
                .child(thread_line_two(theme, row, weight, selected)),
        )
        .into_any_element()
}

fn thread_line_one(
    theme: &Theme,
    row: &ThreadRow,
    weight: FontWeight,
    selected: bool,
) -> gpui::Div {
    let dim = if selected {
        theme.on_selection
    } else {
        theme.dimmed
    };
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .min_w(px(0.))
        .child(names(theme, &row.spans, row.extra, weight, selected))
        .when(row.message_count > 1, |line| {
            line.child(
                div()
                    .text_color(rgb(dim))
                    .text_size(px(theme.ui_px - 1.0))
                    .child(row.message_count.to_string()),
            )
        })
        .when(row.starred, |line| {
            line.child(div().text_color(rgb(theme.star)).child("★"))
        })
        .when(row.has_attachment, |line| {
            line.child(div().text_color(rgb(dim)).child("⧉"))
        })
        .children(row.labels.iter().take(2).map(|label| {
            div()
                .px(px(6.))
                .py(px(1.))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.border))
                .text_size(px((theme.ui_px - 2.0).max(11.0)))
                .text_color(rgb(dim))
                .child(label.clone())
        }))
        .child(div().flex_1())
        .child(
            div()
                .text_color(rgb(dim))
                .text_size(px(theme.ui_px - 1.0))
                .child(present::relative_date(now_unix(), row.date_unix)),
        )
}

fn names(
    theme: &Theme,
    spans: &[NameSpan],
    extra: usize,
    weight: FontWeight,
    selected: bool,
) -> gpui::Div {
    let dim = if selected {
        theme.on_selection
    } else {
        theme.dimmed
    };
    let mut line = div()
        .flex()
        .flex_row()
        .gap(px(6.))
        .min_w(px(0.))
        .overflow_hidden();
    for (index, span) in spans.iter().enumerate() {
        let span_weight = if weight == FontWeight::BOLD {
            FontWeight::BOLD
        } else if span.emphasis {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };
        let last = index + 1 == spans.len() && extra == 0;
        let text = if last {
            span.text.clone()
        } else {
            format!("{},", span.text)
        };
        line = line.child(
            div()
                .font_weight(span_weight)
                .text_color(rgb(if selected {
                    theme.on_selection
                } else {
                    theme.foreground
                }))
                .child(text),
        );
    }
    if extra > 0 {
        line = line.child(div().text_color(rgb(dim)).child(format!("+{extra}")));
    }
    line
}

fn thread_line_two(
    theme: &Theme,
    row: &ThreadRow,
    weight: FontWeight,
    selected: bool,
) -> gpui::Div {
    let snippet = if selected {
        theme.on_selection_dim
    } else {
        theme.dimmed
    };
    div()
        .flex()
        .flex_row()
        .gap(px(8.))
        .min_w(px(0.))
        .overflow_hidden()
        .child(
            div()
                .font_weight(weight)
                .flex_shrink_0()
                .max_w(px(280.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(rgb(if selected {
                    theme.on_selection
                } else {
                    theme.foreground
                }))
                .child(row.subject.clone()),
        )
        .when(!row.snippet.is_empty(), |line| {
            line.child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(snippet))
                    .child(row.snippet.clone()),
            )
        })
}

fn empty_state(theme: &Theme, session: &Session) -> gpui::Div {
    let banner = session.list_banner();
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .px(px(24.))
        .child(
            div().text_color(rgb(theme.foreground)).child(
                banner
                    .as_ref()
                    .map(|item| item.title.clone())
                    .unwrap_or_default(),
            ),
        )
        .child(
            div().text_color(rgb(theme.dimmed)).child(
                banner
                    .as_ref()
                    .map(|item| item.hint.clone())
                    .unwrap_or_default(),
            ),
        )
}

fn splitter(theme: &Theme, cx: &mut Context<Inbox>) -> impl IntoElement {
    div()
        .id("split")
        .w(px(7.))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_col_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|inbox, _, _, cx| {
                inbox.dragging = true;
                cx.notify();
            }),
        )
        .child(div().w(px(1.)).h_full().bg(rgb(theme.border)))
}

fn conversation(
    theme: &Theme,
    session: &Session,
    scroll: ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    let subject = session
        .threads
        .iter()
        .find(|thread| Some(&thread.id) == session.cursor_id.as_ref())
        .map(|thread| thread.subject.clone())
        .unwrap_or_default();
    let messages: Vec<_> = session
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| message_block(theme, session, message, index, cx))
        .collect();
    div()
        .id("conversation")
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .border_l(px(2.))
        .border_color(rgb(if session.pane == Pane::Conversation {
            theme.accent
        } else {
            theme.background
        }))
        .when(!subject.is_empty(), |pane| {
            pane.child(
                div()
                    .px(px(28.))
                    .pt(px(22.))
                    .pb(px(14.))
                    .text_size(px(theme.ui_px + 11.0))
                    .font_weight(FontWeight::BOLD)
                    .line_height(px(theme.ui_px + 16.0))
                    .border_b(px(1.))
                    .border_color(rgb(theme.border))
                    .child(subject),
            )
        })
        .child(
            div()
                .id("conversation-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(&scroll)
                .px(px(20.))
                .pb(px(28.))
                .children(messages),
        )
}

fn message_block(
    theme: &Theme,
    session: &Session,
    message: &ConvMessage,
    index: usize,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let cursor = session.message_cursor == index && session.pane == Pane::Conversation;
    if message.expanded {
        expanded_message(theme, session, message, index, cursor, cx)
    } else {
        collapsed_message(theme, message, index, cursor, cx)
    }
}

fn collapsed_message(
    theme: &Theme,
    message: &ConvMessage,
    index: usize,
    cursor: bool,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    div()
        .id(gpui::ElementId::named_usize("message", index))
        .h(px(40.))
        .px(px(8.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .rounded_md()
        .bg(rgb(if cursor {
            theme.selection
        } else {
            theme.background
        }))
        .text_color(rgb(if cursor {
            theme.on_selection
        } else {
            theme.foreground
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |inbox, _, window, cx| {
                window.focus(&inbox.focus);
                inbox.session.cancel_pending_keys();
                inbox.session.pane = Pane::Conversation;
                inbox.session.message_cursor = index;
                if let Some(message) = inbox.session.messages.get_mut(index) {
                    message.expanded = true;
                }
                cx.notify();
            }),
        )
        .child(
            div()
                .w(px(140.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .font_weight(FontWeight::MEDIUM)
                .child(message.sender.clone()),
        )
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(rgb(if cursor {
                    theme.on_selection
                } else {
                    theme.dimmed
                }))
                .child(message.snippet.clone()),
        )
        .child(
            div()
                .text_color(rgb(if cursor {
                    theme.on_selection
                } else {
                    theme.dimmed
                }))
                .child(present::relative_date(now_unix(), message.date_unix)),
        )
        .into_any_element()
}

fn expanded_message(
    theme: &Theme,
    session: &Session,
    message: &ConvMessage,
    index: usize,
    cursor: bool,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let quotes = session.quotes_open.contains(&index);
    let dim = theme.dimmed;
    div()
        .id(gpui::ElementId::named_usize("message", index))
        .px(px(8.))
        .py(px(10.))
        .mb(px(8.))
        .rounded_md()
        .bg(rgb(if cursor {
            theme.surface
        } else {
            theme.background
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |inbox, _, window, cx| {
                window.focus(&inbox.focus);
                inbox.session.cancel_pending_keys();
                inbox.session.pane = Pane::Conversation;
                inbox.session.message_cursor = index;
                cx.notify();
            }),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .pb(px(8.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(message.sender.clone()),
                )
                .child(
                    div()
                        .text_color(rgb(dim))
                        .text_size(px(theme.ui_px - 1.0))
                        .child(message.email.clone()),
                )
                .when(!message.folders.is_empty(), |row| {
                    row.child(
                        div()
                            .text_color(rgb(dim))
                            .text_size(px(theme.ui_px - 1.0))
                            .child(message.folders.clone()),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(rgb(dim))
                        .child(present::relative_date(now_unix(), message.date_unix)),
                ),
        )
        .children(
            message
                .body
                .blocks
                .iter()
                .map(|block| body_block(theme, block, quotes, index, cx)),
        )
        .into_any_element()
}

fn body_block(
    theme: &Theme,
    block: &BodyBlock,
    quotes_open: bool,
    index: usize,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    match block {
        BodyBlock::Text(text) => div()
            .max_w(px(680.))
            .pb(px(8.))
            .line_height(px(theme.ui_px * 1.45))
            .child(text.clone())
            .into_any_element(),
        BodyBlock::Quote(text) | BodyBlock::Signature(text) => {
            let label = if matches!(block, BodyBlock::Quote(_)) {
                "Quoted text"
            } else {
                "Signature"
            };
            if quotes_open {
                div()
                    .max_w(px(680.))
                    .pl(px(10.))
                    .mb(px(8.))
                    .border_l(px(2.))
                    .border_color(rgb(theme.border))
                    .text_color(rgb(theme.dimmed))
                    .line_height(px(theme.ui_px * 1.4))
                    .child(text.clone())
                    .into_any_element()
            } else {
                div()
                    .h(px(28.))
                    .mb(px(6.))
                    .flex()
                    .items_center()
                    .text_color(rgb(theme.dimmed))
                    .child(format!("{label} · q"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |inbox, _, window, cx| {
                            window.focus(&inbox.focus);
                            inbox.session.cancel_pending_keys();
                            inbox.session.pane = Pane::Conversation;
                            inbox.session.message_cursor = index;
                            if !inbox.session.quotes_open.remove(&index) {
                                inbox.session.quotes_open.insert(index);
                            }
                            cx.notify();
                        }),
                    )
                    .into_any_element()
            }
        }
    }
}

fn status_bar(theme: &Theme, session: &Session) -> gpui::Div {
    let hint = if !session.chord.is_empty() {
        session.prefix_text()
    } else if session.pane == Pane::Conversation {
        "j k  ·  esc  ·  ctrl-d  ·  space  ·  :".to_string()
    } else {
        "j k  ·  enter  ·  space  ·  :".to_string()
    };
    let count = if session.count.is_empty() {
        String::new()
    } else {
        format!("{} ", session.count)
    };
    div()
        .h(px(30.))
        .px(px(14.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .border_t(px(1.))
        .border_color(rgb(theme.border))
        .bg(rgb(theme.surface))
        .text_size(px((theme.ui_px - 1.0).max(12.0)))
        .child(
            div()
                .font_family_opt(theme.mono_family.clone())
                .text_color(rgb(if session.visual {
                    theme.accent
                } else {
                    theme.dimmed
                }))
                .child(session.mode_label().to_string()),
        )
        .child(div().text_color(rgb(theme.dimmed)).child("·"))
        .child(
            div()
                .text_color(rgb(
                    if matches!(session.sync, crate::model::SyncKind::StoreError) {
                        theme.error
                    } else {
                        theme.dimmed
                    },
                ))
                .child(session.sync.status().to_string()),
        )
        .when(session.detail_text().is_some(), |bar| {
            bar.child(div().text_color(rgb(theme.error)).child("· ? details"))
        })
        .child(div().flex_1())
        .child(
            div()
                .font_family_opt(theme.mono_family.clone())
                .text_color(rgb(if session.chord.is_empty() {
                    theme.dimmed
                } else {
                    theme.accent
                }))
                .child(format!("{count}{hint}")),
        )
}

fn which_key_popup(theme: &Theme, session: &Session) -> gpui::Div {
    let items = session.which_items();
    div()
        .absolute()
        .bottom(px(40.))
        .left(px(16.))
        .min_w(px(220.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .px(px(12.))
        .py(px(10.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .pb(px(4.))
                .text_color(rgb(theme.dimmed))
                .font_family_opt(theme.mono_family.clone())
                .child(session.prefix_text()),
        )
        .children(items.into_iter().map(|item| {
            div()
                .flex()
                .flex_row()
                .gap(px(12.))
                .child(
                    div()
                        .w(px(72.))
                        .font_family_opt(theme.mono_family.clone())
                        .text_color(rgb(theme.accent))
                        .child(item.key),
                )
                .child(div().child(item.label))
        }))
}

fn overlay(
    theme: &Theme,
    session: &Session,
    palette_scroll: &ScrollHandle,
    folder_scroll: &ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .right(px(0.))
        .bottom(px(0.))
        .flex()
        .justify_center()
        .pt(px(72.))
        .occlude()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|inbox, _, _, cx| {
                inbox.session.overlay = Overlay::None;
                cx.notify();
            }),
        )
        .child(match session.overlay {
            Overlay::Palette => palette(theme, session, palette_scroll, cx).into_any_element(),
            Overlay::Folders => folder_picker(theme, session, folder_scroll, cx).into_any_element(),
            Overlay::Help => help(theme, session, cx).into_any_element(),
            Overlay::None => div().into_any_element(),
        })
}

fn palette(
    theme: &Theme,
    session: &Session,
    scroll: &ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let items = session.palette();
    let selected = session.palette_index;
    div()
        .w(px(560.))
        .max_h(px(420.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .h(px(44.))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .child(div().text_color(rgb(theme.accent)).child(":"))
                .child(div().child(session.palette_query.clone()))
                .child(div().w(px(2.)).h(px(16.)).bg(rgb(theme.accent))),
        )
        .child(
            div()
                .id("palette-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .py(px(4.))
                .children(items.iter().enumerate().map(|(index, item)| {
                    let action = item.action;
                    div()
                        .id(gpui::ElementId::named_usize("palette", index))
                        .h(px(36.))
                        .mx(px(4.))
                        .px(px(10.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .rounded_md()
                        .bg(rgb(if index == selected {
                            theme.selection
                        } else {
                            theme.surface
                        }))
                        .text_color(rgb(if index == selected {
                            theme.on_selection
                        } else {
                            theme.foreground
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |inbox, _, window, cx| {
                                cx.stop_propagation();
                                window.focus(&inbox.focus);
                                inbox.session.overlay = Overlay::None;
                                let page = inbox.page_rows();
                                let message_before = inbox.session.message_cursor;
                                let thread_before = inbox.session.cursor_id.clone();
                                let folder_before = inbox.session.folder.clone();
                                let step = inbox.session.perform(action, page);
                                inbox.apply_step(
                                    step,
                                    message_before,
                                    thread_before,
                                    folder_before,
                                    cx,
                                );
                            }),
                        )
                        .child(div().flex_1().child(item.label.clone()))
                        .child(
                            div()
                                .font_family_opt(theme.mono_family.clone())
                                .text_size(px((theme.mono_px).max(12.0)))
                                .text_color(rgb(if index == selected {
                                    theme.on_selection
                                } else {
                                    theme.dimmed
                                }))
                                .child(item.binding.clone()),
                        )
                })),
        )
}

fn folder_picker(
    theme: &Theme,
    session: &Session,
    scroll: &ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let choices = session.folder_choices();
    let selected = session.folder_index;
    div()
        .w(px(420.))
        .max_h(px(380.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .h(px(44.))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .child(div().text_color(rgb(theme.dimmed)).child("Folder"))
                .child(div().child(session.folder_query.clone()))
                .child(div().w(px(2.)).h(px(16.)).bg(rgb(theme.accent))),
        )
        .child(
            div()
                .id("folder-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .py(px(4.))
                .children(choices.iter().enumerate().map(|(index, name)| {
                    let name = name.clone();
                    let label = present::folder_label(&name);
                    div()
                        .id(gpui::ElementId::named_usize("folder", index))
                        .h(px(34.))
                        .mx(px(4.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .bg(rgb(if index == selected {
                            theme.selection
                        } else {
                            theme.surface
                        }))
                        .text_color(rgb(if index == selected {
                            theme.on_selection
                        } else {
                            theme.foreground
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |inbox, _, window, cx| {
                                cx.stop_propagation();
                                window.focus(&inbox.focus);
                                inbox.session.overlay = Overlay::None;
                                if inbox.session.folder != name {
                                    let previous = inbox.session.folder.clone();
                                    inbox.session.folder = name.clone();
                                    if !inbox.reload_threads() {
                                        inbox.session.folder = previous;
                                    } else {
                                        if !inbox.reload_messages(true) {
                                            inbox.clear_open_message();
                                        }
                                        inbox.scroll_list_to_cursor();
                                        inbox.scroll_conversation();
                                    }
                                }
                                cx.notify();
                            }),
                        )
                        .child(label)
                })),
        )
}

fn help(theme: &Theme, session: &Session, cx: &mut Context<Inbox>) -> gpui::Div {
    let lines: Vec<_> = crate::keymap::Action::all()
        .iter()
        .copied()
        .map(|action| {
            (
                action.label().to_string(),
                crate::keymap::binding_text(&session.keymap, action),
            )
        })
        .collect();
    div()
        .w(px(520.))
        .max_h(px(460.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .px(px(14.))
                .py(px(10.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Keys"),
        )
        .when_some(session.detail_text(), |card, error| {
            card.child(
                div()
                    .px(px(14.))
                    .py(px(8.))
                    .text_color(rgb(theme.error))
                    .child(error),
            )
        })
        .child(
            div()
                .id("help-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .px(px(14.))
                .py(px(8.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .children(lines.into_iter().map(|(label, binding)| {
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(12.))
                        .child(div().flex_1().child(label))
                        .child(
                            div()
                                .font_family_opt(theme.mono_family.clone())
                                .text_color(rgb(theme.dimmed))
                                .child(binding),
                        )
                })),
        )
}

fn key_from(stroke: &Keystroke) -> Option<KeyId> {
    let bare = stroke.key.to_ascii_lowercase();
    if stroke.modifiers.control && stroke.modifiers.shift && bare == "p" {
        return Some(KeyId::CtrlShiftP);
    }
    if stroke.modifiers.control && !stroke.modifiers.alt && !stroke.modifiers.platform {
        let ch = bare.chars().next()?;
        if ch.is_ascii_alphabetic() {
            return Some(KeyId::Ctrl(ch));
        }
    }
    if stroke.modifiers.control || stroke.modifiers.alt || stroke.modifiers.platform {
        return None;
    }
    if let Some(ch) = physical_letter(&stroke.key) {
        return Some(KeyId::Char(if stroke.modifiers.shift {
            ch.to_ascii_uppercase()
        } else {
            ch.to_ascii_lowercase()
        }));
    }
    let typed = stroke
        .key_char
        .clone()
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| stroke.key.clone());
    match typed.as_str() {
        "escape" | "esc" => Some(KeyId::Escape),
        "backspace" => Some(KeyId::Backspace),
        "enter" | "return" => Some(KeyId::Enter),
        "space" | " " => Some(KeyId::Space),
        "up" => Some(KeyId::Up),
        "down" => Some(KeyId::Down),
        "left" | "right" | "tab" => None,
        other => {
            let mut chars = other.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return named_key(&stroke.key, stroke.modifiers.shift);
            }
            if stroke.modifiers.shift {
                Some(KeyId::Char(match ch {
                    '/' => '?',
                    ';' => ':',
                    other => other.to_ascii_uppercase(),
                }))
            } else {
                Some(KeyId::Char(ch))
            }
        }
    }
}

fn physical_letter(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || !ch.is_ascii_alphabetic() {
        return None;
    }
    Some(ch)
}

fn named_key(key: &str, shift: bool) -> Option<KeyId> {
    match key {
        "escape" => Some(KeyId::Escape),
        "backspace" => Some(KeyId::Backspace),
        "enter" | "return" => Some(KeyId::Enter),
        "space" => Some(KeyId::Space),
        "up" => Some(KeyId::Up),
        "down" => Some(KeyId::Down),
        "g" if shift => Some(KeyId::Char('G')),
        "v" if shift => Some(KeyId::Char('V')),
        "slash" if shift => Some(KeyId::Char('?')),
        "semicolon" if shift => Some(KeyId::Char(':')),
        other if other.chars().count() == 1 => {
            let ch = other.chars().next().unwrap();
            Some(KeyId::Char(if shift {
                ch.to_ascii_uppercase()
            } else {
                ch
            }))
        }
        _ => None,
    }
}

fn window_size() -> (f32, f32) {
    if let Ok(raw) = std::env::var("FELL_WINDOW")
        && let Some((width, height)) = raw.split_once('x')
        && let (Ok(width), Ok(height)) = (width.parse::<f32>(), height.parse::<f32>())
    {
        return (width, height);
    }
    (1180.0, 760.0)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

trait FontOpt: Styled {
    fn font_family_opt(self, family: Option<String>) -> Self;
}

impl<T: Styled> FontOpt for T {
    fn font_family_opt(self, family: Option<String>) -> Self {
        if let Some(family) = family {
            self.font_family(family)
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::key_from;
    use crate::keymap::KeyId;
    use gpui::{Keystroke, Modifiers};

    fn stroke(key: &str, key_char: &str, shift: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                shift,
                ..Modifiers::default()
            },
            key: key.into(),
            key_char: Some(key_char.into()),
        }
    }

    #[test]
    fn caps_lock_uses_the_physical_letter() {
        assert_eq!(key_from(&stroke("j", "J", false)), Some(KeyId::Char('j')));
        assert_eq!(key_from(&stroke("k", "K", false)), Some(KeyId::Char('k')));
        assert_eq!(key_from(&stroke("g", "G", false)), Some(KeyId::Char('g')));
        assert_eq!(key_from(&stroke("g", "G", true)), Some(KeyId::Char('G')));
        assert_eq!(
            key_from(&stroke("slash", "?", true)),
            Some(KeyId::Char('?'))
        );
    }
}
