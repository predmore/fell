use std::time::{Duration, Instant};

use fell_core::notes::{NoteDecision, Suggestion, ThreadNote};
use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, div, point, px, rgb,
};

use gpui::prelude::FluentBuilder;

use super::{
    CONVERSATION_PAD_PX, Inbox, Live, MESSAGE_PAD_PX, ReadIntent, ReaderColumns, disclosure,
};
use crate::keymap::{self, Action};
use crate::model::{NoteRequest, SUGGESTIONS_QUERY, Session};
use crate::motion::{self, Motion, Transition};
use crate::present::{self, Pane};
use crate::theme::Theme;

pub(super) const MARKER_SLOT: f32 = 12.0;

pub(super) fn marker(theme: &Theme, selected: bool, unread: bool) -> gpui::Div {
    let color = if selected {
        theme.on_selection
    } else {
        theme.accent
    };
    div()
        .w(px(MARKER_SLOT))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .when(unread, |slot| {
            slot.child(
                div()
                    .w(px(7.))
                    .h(px(7.))
                    .rounded_sm()
                    .border_1()
                    .border_color(rgb(color)),
            )
        })
}

#[derive(Default)]
pub(super) struct NoteView {
    thread: Option<String>,
    current: Option<ThreadNote>,
    closed: bool,
    entrance: Option<Transition>,
    exit: Option<(ThreadNote, bool, Transition)>,
    fold: Option<Transition>,
    scroll: ScrollHandle,
}

pub(super) struct NoteFrame {
    note: ThreadNote,
    closed: bool,
    reveal: Option<f32>,
    alpha: f32,
    leaving: bool,
    scroll: ScrollHandle,
}

impl NoteView {
    pub(super) fn observe(
        &mut self,
        thread: Option<&str>,
        note: Option<&ThreadNote>,
        closed: bool,
        motion: &Motion,
        now: Instant,
    ) {
        if self.thread.as_deref() != thread {
            self.thread = thread.map(str::to_owned);
            self.current = None;
            self.exit = None;
        }
        if self.current.as_ref().map(|note| note.id) != note.map(|note| note.id) {
            self.exit = if note.is_none() {
                self.current
                    .take()
                    .zip(motion.start(motion::FADE, now))
                    .map(|(note, exit)| (note, self.closed, exit))
            } else {
                None
            };
            self.closed = closed;
            self.fold = None;
            self.entrance = note.and_then(|_| motion.start(motion::FADE, now));
            self.scroll = ScrollHandle::new();
        } else if self.closed != closed {
            self.closed = closed;
            self.fold = motion.start(motion::FADE, now);
        }
        self.current = note.cloned();
        if self
            .exit
            .as_ref()
            .is_some_and(|(_, _, exit)| !exit.running(now))
        {
            self.exit = None;
        }
    }

    pub(super) fn frame(&self, now: Instant) -> Option<NoteFrame> {
        if let Some(note) = &self.current {
            Some(NoteFrame {
                note: note.clone(),
                closed: self.closed,
                reveal: self.fold.filter(|fold| fold.running(now)).map(|fold| {
                    let shown = fold.progress(now);
                    if self.closed { 1.0 - shown } else { shown }
                }),
                alpha: motion::progress(self.entrance.as_ref(), now),
                leaving: false,
                scroll: self.scroll.clone(),
            })
        } else {
            self.exit.as_ref().map(|(note, closed, exit)| NoteFrame {
                note: note.clone(),
                closed: *closed,
                reveal: None,
                alpha: motion::fade_out(exit.linear(now)),
                leaving: true,
                scroll: self.scroll.clone(),
            })
        }
    }

    pub(super) fn running(&self, now: Instant) -> bool {
        self.entrance
            .iter()
            .chain(self.fold.iter())
            .any(|animation| animation.running(now))
            || self
                .exit
                .as_ref()
                .is_some_and(|(_, _, exit)| exit.running(now))
    }

    pub(super) fn settle(&mut self) {
        self.entrance = None;
        self.fold = None;
    }

    pub(super) fn scroll(&self, direction: i8) {
        let offset = self.scroll.offset();
        let y = (offset.y - px(f32::from(direction) * 30.))
            .clamp(-self.scroll.max_offset().height, px(0.));
        self.scroll.set_offset(point(offset.x, y));
    }
}

pub(super) fn slot(
    theme: &Theme,
    session: &Session,
    columns: ReaderColumns,
    frame: Option<&NoteFrame>,
    live: Live,
    cx: &mut Context<Inbox>,
) -> impl IntoElement {
    let column = columns.card;
    let line = theme.ui_px + 3.0;
    // The note follows the reading column, without changing the message viewport
    // height as the column slides between its two measures.
    let details_height = if columns.split_width.min(columns.reading_prose) < 460.0 {
        4.0 * line
    } else {
        3.0 * line
    };
    let slot = div()
        .h(px(details_height + line + 21.0))
        .flex_none()
        .px(px(CONVERSATION_PAD_PX + MESSAGE_PAD_PX))
        .pt(px(8.0))
        .overflow_hidden();
    let Some(frame) = frame else { return slot };
    let active = live.0
        && !frame.leaving
        && session.pane == Pane::Conversation
        && session.note().is_some_and(|note| note.id == frame.note.id);
    let (position, total) = session.agent_notes.position();
    let header = |collapsed| {
        let row = div()
            .flex()
            .items_center()
            .gap(px(7.0))
            .h(px(line + 6.0))
            .child(div().flex_none().child(if collapsed { "▸" } else { "▾" }))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!(
                        "{} · {}",
                        frame.note.actor,
                        if frame.note.summary.is_empty() {
                            "suggestions"
                        } else {
                            "summary"
                        },
                    )),
            )
            .when(total > 1 && !frame.leaving, |row| {
                row.child(div().flex_none().child(format!("{position}/{total}")))
            });
        if active {
            row.id("toggle-agent-note")
                .cursor_pointer()
                .on_click(cx.listener(|inbox, _, _, cx| {
                    let before = inbox.before_step();
                    let step = inbox.session.perform(Action::ToggleAgentNote, 1);
                    inbox.apply_step(step, before, cx);
                }))
                .into_any_element()
        } else {
            row.into_any_element()
        }
    };
    let closed_header = header(true);
    let open_header = header(false);
    let summary = div().when(!frame.note.summary.is_empty(), |text| {
        text.child(frame.note.summary.clone())
    });
    let note_id = frame.note.id;
    let clickable = active && !frame.closed;
    let hints: Vec<_> = if live.0
        && session.pane == Pane::Conversation
        && (!frame.closed || frame.reveal.is_some())
    {
        let keys = session.note_shortcuts(&frame.note);
        frame
            .note
            .suggestions
            .iter()
            .zip(keys)
            .map(|(item, (_, key))| {
                let label = match &item.action {
                    Suggestion::Snooze(until) => {
                        format!("Snooze {}", present::note_snooze_date(*until))
                    }
                    action => action.title(),
                };
                let key = key.map(|key| keymap::key_label(key, &session.keymap.leader_name));
                let position = item.position;
                div()
                    .id(("agent-suggestion", position as usize))
                    .flex()
                    .items_center()
                    .gap(px(4.0))
                    .max_w_full()
                    .min_w_0()
                    .text_color(rgb(theme.accent))
                    .when_some(key, |chip, key| chip.child(keycap(theme, key)))
                    .child(div().min_w_0().truncate().child(label))
                    .when(clickable, |chip| {
                        chip.cursor_pointer()
                            .on_click(cx.listener(move |inbox, _, _, cx| {
                                if let Some(request) =
                                    inbox.session.note_request(NoteDecision::Accept(position))
                                    && request.id == note_id
                                {
                                    inbox.choose_agent_note(request, cx);
                                }
                            }))
                    })
                    .into_any_element()
            })
            .collect()
    } else {
        Vec::new()
    };
    let dismiss = (!hints.is_empty()).then(|| {
        div()
            .id("dismiss-agent-note")
            .flex()
            .items_center()
            .gap(px(4.0))
            .child(keycap(
                theme,
                keymap::binding_text(&session.keymap, Action::DismissAgentNote),
            ))
            .child("Dismiss")
            .when(clickable, |chip| {
                chip.cursor_pointer()
                    .on_click(cx.listener(move |inbox, _, _, cx| {
                        if let Some(request) = inbox.session.note_request(NoteDecision::Dismiss)
                            && request.id == note_id
                        {
                            inbox.choose_agent_note(request, cx);
                        }
                    }))
            })
    });
    let details = div()
        .flex()
        .flex_col()
        .child(open_header)
        .child(
            div()
                .id("agent-note-details")
                .max_h(px(details_height))
                .overflow_y_scroll()
                .track_scroll(&frame.scroll)
                .child(summary)
                .when(!hints.is_empty(), |body| {
                    body.child(
                        div()
                            .flex()
                            .flex_wrap()
                            .gap_x(px(12.0))
                            .gap_y(px(2.0))
                            .mt(px(3.0))
                            .children(hints)
                            .children(dismiss),
                    )
                }),
        )
        .into_any_element();
    slot.child(
        column.place(
            div()
                .w_full()
                .max_h_full()
                .overflow_hidden()
                .opacity(frame.alpha)
                .border_l_1()
                .border_color(rgb(theme.border))
                .bg(rgb(theme.surface))
                .pl(px(10.0))
                .pr(px(8.0))
                .pb(px(3.0))
                .text_color(rgb(theme.dimmed))
                .text_size(px(theme.ui_px - 1.0))
                .line_height(px(line))
                .child(disclosure(
                    (column.width - 19.).max(0.),
                    !frame.closed,
                    frame.reveal,
                    || closed_header,
                    || details,
                )),
        ),
    )
}

fn keycap(theme: &Theme, key: String) -> gpui::Div {
    div()
        .flex_none()
        .rounded_sm()
        .px(px(4.0))
        .bg(rgb(theme.background))
        .text_color(rgb(theme.foreground))
        .child(key)
}

impl Inbox {
    pub(super) fn watch_notes(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut seen = None;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(450))
                    .await;
                let Ok(Some((store, account, open, to_see))) = this.update(cx, |inbox, _| {
                    if inbox.account_id.as_str().is_empty() {
                        return None;
                    }
                    let open = inbox
                        .open_thread
                        .clone()
                        .filter(|id| Some(id) == inbox.session.cursor_id.as_ref());
                    let to_see = (inbox.session.pane == Pane::Conversation
                        && !inbox.session.agent_notes.collapsed)
                        .then(|| inbox.session.note().map(|note| note.id))
                        .flatten();
                    Some((inbox.store.clone(), inbox.account_id.clone(), open, to_see))
                }) else {
                    if this.upgrade().is_none() {
                        break;
                    }
                    continue;
                };
                if let Some(note) = to_see.filter(|note| seen != Some(*note)) {
                    let account = account.clone();
                    if let Ok(Ok(())) = store
                        .call(move |store| store.see_thread_note(&account, note))
                        .await
                    {
                        seen = Some(note);
                    }
                }
                let query = open.clone();
                let result = store
                    .call_search(move |store| {
                        let unread = store.unread_note_threads(&account)?;
                        let pending = store.pending_note_threads(&account)?;
                        let notes = query
                            .map(|id| store.thread_notes(&account, &fell_core::ThreadId(id)))
                            .transpose()?;
                        Ok::<_, fell_core::Error>((unread, pending, notes))
                    })
                    .await;
                let Ok(Ok((unread, pending, notes))) = result else {
                    continue;
                };
                if this
                    .update(cx, |inbox, cx| {
                        let state = &mut inbox.session.agent_notes;
                        let marker_changed = state.unread != unread;
                        let filter_changed = state.pending_threads != pending;
                        state.unread = unread;
                        state.pending_threads = pending;
                        let shown = notes.filter(|_| {
                            open == inbox.open_thread && open == inbox.session.cursor_id
                        });
                        let note_changed = shown.is_some_and(|notes| state.update(open, notes));
                        if filter_changed
                            && inbox.session.search.as_ref().is_some_and(|search| {
                                search.effective_query().contains(SUGGESTIONS_QUERY)
                            })
                        {
                            inbox.refresh_search(cx);
                        }
                        if marker_changed {
                            inbox.search_rows.update(cx, |_, cx| cx.notify());
                        }
                        if marker_changed || note_changed {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(super) fn choose_agent_note(&mut self, note: NoteRequest, cx: &mut Context<Self>) {
        if self.open_thread.as_deref() != Some(&note.thread)
            || self.session.pane != Pane::Conversation
            || self.note_in_flight.is_some()
            || !self.session.note().is_some_and(|shown| {
                shown.id == note.id
                    && match note.decision {
                        NoteDecision::Accept(position) => shown
                            .suggestions
                            .iter()
                            .any(|item| item.position == position),
                        NoteDecision::Dismiss => true,
                    }
            })
        {
            return;
        }
        if note.reply {
            self.start_reply(fell_core::compose::ReplyKind::Reply, Some(note), cx);
        } else {
            self.save_note_decision(note, cx);
        }
    }

    pub(super) fn save_note_decision(&mut self, note: NoteRequest, cx: &mut Context<Self>) {
        let scoped = self.session.search.is_some();
        let folder = if note.decision == NoteDecision::Dismiss {
            self.list_folder.clone()
        } else {
            let Some(folder) = self.action_folder(&note.thread) else {
                return;
            };
            folder
        };
        let account = self.account_id.clone();
        let thread = fell_core::ThreadId(note.thread.clone());
        let id = note.id;
        let decision = note.decision;
        self.note_in_flight = Some(id);
        self.change_mailbox(
            move |store| {
                store.resolve_thread_note(&account, &thread, &folder, id, decision, scoped)
            },
            move |inbox, result| {
                inbox.note_in_flight = None;
                match result {
                    Ok(_) => {
                        inbox.session.agent_notes.resolved(&note);
                        inbox.send_to_server();
                        inbox.session.show_toast(
                            if decision == NoteDecision::Dismiss {
                                "Agent note dismissed"
                            } else {
                                "Suggestion accepted"
                            },
                            false,
                        );
                        Some(ReadIntent {
                            changed: true,
                            ..ReadIntent::default()
                        })
                    }
                    Err(err) => {
                        inbox.session.show_toast(err.to_string(), true);
                        None
                    }
                }
            },
            cx,
        );
    }
}
