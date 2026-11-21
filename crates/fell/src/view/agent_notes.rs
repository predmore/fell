use std::time::{Duration, Instant};

use fell_core::notes::{NoteDecision, Suggestion, ThreadNote, display_safe};
use gpui::prelude::FluentBuilder;
use gpui::{
    Context, FontWeight, InteractiveElement, IntoElement, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, Window, div, point, px, rgb,
};

use super::{
    Inbox, Live, MESSAGE_PAD_PX, ReadIntent, ReaderColumns, conversation_line_style, disclosure,
};
use crate::keymap::{self, Action};
use crate::model::{NoteRequest, SUGGESTIONS_QUERY, Session};
use crate::motion::{self, Motion, Transition};
use crate::present::{self, Pane};
use crate::theme::{self, Theme};

pub(super) const MARKER_SLOT: f32 = 12.0;

pub(super) fn marker(theme: &Theme, selected: bool) -> gpui::Div {
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
        .child(
            div()
                .w(px(7.))
                .h(px(7.))
                .rounded_sm()
                .border_1()
                .border_color(rgb(color)),
        )
}

const ARRIVAL: Duration = Duration::from_millis(140);
const DISMISSAL: Duration = Duration::from_millis(240);

#[derive(Clone, Copy)]
enum VisibilityChange {
    Arrive,
    Dismiss,
    Restore,
}

#[derive(Clone, Copy)]
struct Visibility {
    change: VisibilityChange,
    from: (f32, f32),
    time: Transition,
}

impl Visibility {
    fn sample(self, now: Instant) -> (f32, f32) {
        let t = self.time.linear(now);
        let grow = |progress| motion::lerp(self.from.0, 1.0, motion::ease_out(progress));
        let fade_in = || motion::lerp(self.from.1, 1.0, motion::ease_out(2.0 * t - 1.0));
        match self.change {
            VisibilityChange::Arrive => (grow(t), fade_in()),
            VisibilityChange::Dismiss => (
                self.from.0 * (1.0 - motion::ease_out(2.0 * t - 1.0)),
                self.from.1 * motion::fade_out(2.0 * t),
            ),
            VisibilityChange::Restore if self.from.0 >= 1.0 => {
                (1.0, motion::lerp(self.from.1, 1.0, self.time.progress(now)))
            }
            VisibilityChange::Restore => (grow(2.0 * t), fade_in()),
        }
    }
}

#[derive(Default)]
pub(super) struct NoteView {
    thread: Option<String>,
    painted: Option<ThreadNote>,
    dismissed: Option<i64>,
    shown: bool,
    closed: bool,
    visibility: Option<Visibility>,
    fold: Option<(f32, Transition)>,
    previous_suggestions: Option<(ThreadNote, Transition)>,
    scroll: ScrollHandle,
}

pub(super) struct NoteFrame {
    note: ThreadNote,
    closed: bool,
    details: f32,
    height: f32,
    alpha: f32,
    leaving: bool,
    suggestions: Option<(ThreadNote, f32)>,
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
    ) -> bool {
        let was_moving =
            self.visibility.is_some() || self.fold.is_some() || self.previous_suggestions.is_some();
        if self.thread.as_deref() != thread {
            *self = Self {
                thread: thread.map(str::to_owned),
                painted: note.cloned(),
                shown: note.is_some(),
                closed,
                ..Self::default()
            };
            return false;
        }
        let changed = self.shown != note.is_some()
            || note
                .is_some_and(|note| self.painted.as_ref() != Some(note) || self.closed != closed);
        if let Some(note) = note {
            if !self.shown {
                let restoring = self.dismissed == Some(note.id);
                let from = self.visibility.map_or((0.0, 0.0), |v| v.sample(now));
                let duration = if restoring && from.0 < 1.0 {
                    DISMISSAL
                } else if restoring {
                    motion::FADE
                } else {
                    ARRIVAL
                };
                let change = if restoring {
                    VisibilityChange::Restore
                } else {
                    VisibilityChange::Arrive
                };
                self.visibility =
                    motion
                        .start(duration, now)
                        .map(|time| Visibility { change, from, time });
                // A folded note comes back folded.
                self.closed = closed;
                self.fold = None;
                self.previous_suggestions = None;
                self.scroll = ScrollHandle::new();
            } else if let Some(old) = &self.painted {
                if old.id != note.id {
                    self.visibility = None;
                    self.fold = None;
                    self.closed = closed;
                    self.previous_suggestions = None;
                    self.scroll = ScrollHandle::new();
                } else if old.suggestions != note.suggestions {
                    self.previous_suggestions = motion
                        .start(motion::SLIDE, now)
                        .map(|time| (old.clone(), time));
                }
            }
            if self.closed != closed {
                let from = self.details(now);
                self.fold = motion.start(motion::SLIDE, now).map(|time| (from, time));
                self.closed = closed;
            }
            self.shown = true;
            self.painted = Some(note.clone());
        } else if self.shown {
            let from = self.visibility.map_or((1.0, 1.0), |v| v.sample(now));
            self.dismissed = self.painted.as_ref().map(|note| note.id);
            self.visibility = motion.start(DISMISSAL, now).map(|time| Visibility {
                change: VisibilityChange::Dismiss,
                from,
                time,
            });
            self.shown = false;
        }
        self.visibility = self.visibility.filter(|v| v.time.running(now));
        self.fold = self.fold.filter(|(_, t)| t.running(now));
        if self
            .previous_suggestions
            .as_ref()
            .is_some_and(|(_, t)| !t.running(now))
        {
            self.previous_suggestions = None;
        }
        if !self.shown && self.visibility.is_none() {
            self.painted = None;
        }
        was_moving || changed
    }

    fn details(&self, now: Instant) -> f32 {
        let to = if self.closed { 0.0 } else { 1.0 };
        self.fold.map_or(to, |(from, time)| {
            motion::lerp(from, to, time.progress(now))
        })
    }

    pub(super) fn frame(&self, now: Instant) -> Option<NoteFrame> {
        let (height, alpha) = self.visibility.map_or((1.0, 1.0), |v| v.sample(now));
        Some(NoteFrame {
            note: self.painted.clone()?,
            closed: self.closed,
            details: self.details(now),
            height,
            alpha,
            leaving: !self.shown,
            suggestions: self
                .previous_suggestions
                .as_ref()
                .map(|(note, t)| (note.clone(), t.linear(now))),
            scroll: self.scroll.clone(),
        })
    }

    pub(super) fn running(&self) -> bool {
        self.visibility.is_some() || self.fold.is_some() || self.previous_suggestions.is_some()
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
    frame: &NoteFrame,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let column = columns.card;
    let line = theme.ui_px + 3.0;
    let header_height = line + 14.0;
    let width = (column.width - 21.0).max(0.0);
    let active = live.0
        && !frame.leaving
        && frame.alpha > 0.0
        && session.pane == Pane::Conversation
        && session.note().is_some_and(|note| note.id == frame.note.id);
    let (position, total) = session.agent_notes.position();
    let actor = display_safe(&frame.note.actor, false);
    let summary = display_safe(&frame.note.summary, true);
    let dimmed = rgb(theme::note_text(theme.dimmed, theme.background));
    let kind = if summary.is_empty() {
        "Agent suggestions"
    } else {
        "Agent summary"
    };
    let age = present::compact_relative_date(super::now_unix(), frame.note.created_unix);
    let header = div()
        .id("toggle-agent-note")
        .flex()
        .items_center()
        .gap(px(7.0))
        .h(px(header_height))
        .child(
            div()
                .flex_none()
                .text_color(dimmed)
                .child(if frame.closed { "▸" } else { "▾" }),
        )
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::SEMIBOLD)
                .child(kind),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .truncate()
                .text_color(dimmed)
                .child(if actor.trim().is_empty() {
                    String::new()
                } else {
                    format!("· {actor}")
                }),
        )
        .when(total > 1 && !frame.leaving, |row| {
            row.child(div().flex_none().child(format!("{position}/{total}")))
        })
        .child(div().flex_none().text_color(dimmed).child(age))
        .when(active, |row| {
            row.cursor_pointer()
                .on_click(cx.listener(|inbox, _, _, cx| {
                    let before = inbox.before_step();
                    let step = inbox.session.perform(Action::ToggleAgentNote, 1);
                    inbox.apply_step(step, before, cx);
                }))
        });
    let detail = (frame.details > 0.0).then(|| {
        disclosure::reveal(
            width,
            frame.details,
            1.0,
            div()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .pb(px(8.0))
                .when(!summary.is_empty(), |body| {
                    body.child(
                        div()
                            .id("agent-note-summary")
                            .max_h(px(4.0 * line))
                            .overflow_y_scroll()
                            .track_scroll(&frame.scroll)
                            .child(summary),
                    )
                })
                .child(suggestions(
                    theme,
                    session,
                    frame,
                    active && !frame.closed,
                    width,
                    window,
                    cx,
                ))
                .into_any_element(),
        )
    });
    let content = div()
        .w_full()
        .border_l(px(2.0))
        .border_color(rgb(motion::mix(theme.background, theme.accent, 0.4)))
        .pl(px(11.0))
        .pr(px(8.0))
        .text_color(rgb(theme.foreground))
        .text_size(px(theme.ui_px - 1.0))
        .line_height(px(line))
        .child(header)
        .children(detail);
    div()
        .flex_none()
        .pl(px(MESSAGE_PAD_PX))
        .child(column.place(disclosure::reveal(
            column.width,
            frame.height,
            frame.alpha,
            div().py(px(4.0)).child(content).into_any_element(),
        )))
}

struct Chip {
    decision: NoteDecision,
    key: Option<String>,
    label: String,
    width: f32,
    x: f32,
    y: f32,
}

fn chip_layout(
    session: &Session,
    note: &ThreadNote,
    width: f32,
    line: f32,
    measure: impl Fn(&str) -> f32,
) -> Vec<Chip> {
    let keys = session.note_shortcuts(note);
    let mut chips: Vec<_> = note
        .suggestions
        .iter()
        .zip(keys)
        .map(|(item, (_, key))| {
            let label = match &item.action {
                Suggestion::Snooze(until) => {
                    format!("Snooze {}", present::note_snooze_date(*until))
                }
                action => display_safe(&action.title(), false),
            };
            (
                NoteDecision::Accept(item.position),
                key.map(|key| keymap::key_label(key, &session.keymap.leader_name)),
                label,
            )
        })
        .collect();
    if !chips.is_empty() {
        chips.push((
            NoteDecision::Dismiss,
            Some(keymap::binding_text(
                &session.keymap,
                Action::DismissAgentNote,
            )),
            "Dismiss".into(),
        ));
    }
    let (mut x, mut y) = (0.0, 0.0);
    chips
        .into_iter()
        .map(|(decision, key, label)| {
            let chip_width =
                (measure(&label) + key.as_ref().map_or(0.0, |key| measure(key) + 14.0)).min(width);
            if x > 0.0 && x + chip_width > width {
                x = 0.0;
                y += line + 4.0;
            }
            let chip = Chip {
                decision,
                key,
                label,
                width: chip_width,
                x,
                y,
            };
            x += chip_width + 12.0;
            chip
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn suggestions(
    theme: &Theme,
    session: &Session,
    frame: &NoteFrame,
    clickable: bool,
    width: f32,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let line = theme.ui_px + 5.0;
    let style = conversation_line_style(window, theme, px(theme.ui_px - 1.0));
    let measure = |label: &str| {
        super::line_px_with_style(window, &style, label, FontWeight::NORMAL).ceil() + 1.0
    };
    let layout = |note| chip_layout(session, note, width, line, measure);
    let chips = layout(&frame.note);
    let (previous, t) = frame
        .suggestions
        .as_ref()
        .map_or((Vec::new(), 1.0), |(note, t)| (layout(note), *t));
    let progress = motion::ease_out(t);
    let height = |chips: &[Chip]| chips.last().map_or(0.0, |chip| chip.y + line);
    let current_height = height(&chips);
    let previous_height = if frame.suggestions.is_some() {
        height(&previous)
    } else {
        current_height
    };
    let note_id = frame.note.id;
    let mut row = div()
        .relative()
        .h(px(motion::lerp(previous_height, current_height, progress)));
    for old in &previous {
        if !chips.iter().any(|chip| chip.decision == old.decision) {
            row = row.child(draw_chip(theme, old, line).opacity(motion::fade_out(t)));
        }
    }
    for mut chip in chips {
        let old = previous.iter().find(|old| old.decision == chip.decision);
        if let Some(old) = old {
            chip.x = motion::lerp(old.x, chip.x, progress);
            chip.y = motion::lerp(old.y, chip.y, progress);
        }
        let decision = chip.decision;
        let id = match decision {
            NoteDecision::Dismiss => 0,
            NoteDecision::Accept(position) => position as usize + 1,
        };
        let element = draw_chip(theme, &chip, line)
            .id(("agent-suggestion", id))
            .when(clickable, |chip| {
                chip.cursor_pointer()
                    .on_click(cx.listener(move |inbox, _, _, cx| {
                        if let Some(request) = inbox.session.note_request(decision)
                            && request.id == note_id
                        {
                            inbox.choose_agent_note(request, cx);
                        }
                    }))
            });
        row = row.child(if old.is_none() && frame.suggestions.is_some() {
            element.opacity(progress)
        } else {
            element
        });
    }
    row
}

fn draw_chip(theme: &Theme, chip: &Chip, line: f32) -> gpui::Div {
    let text = if chip.decision == NoteDecision::Dismiss {
        theme.dimmed
    } else {
        theme.accent
    };
    div()
        .absolute()
        .left(px(chip.x))
        .top(px(chip.y))
        .w(px(chip.width))
        .h(px(line))
        .flex()
        .items_center()
        .gap(px(4.0))
        .text_color(rgb(theme::note_text(text, theme.background)))
        .when_some(chip.key.as_ref(), |chip, key| {
            chip.child(keycap(theme, key))
        })
        .child(div().min_w_0().truncate().child(chip.label.clone()))
}

fn keycap(theme: &Theme, key: &str) -> gpui::Div {
    div()
        .flex_none()
        .rounded_sm()
        .px(px(4.0))
        .border_1()
        .border_color(rgb(theme::strong_border(theme)))
        .bg(rgb(theme.selection))
        .text_color(rgb(theme.on_selection))
        .child(key.to_owned())
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
                        && !inbox.session.agent_notes.collapsed
                        && inbox.note_in_view())
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
        let notice = match decision {
            NoteDecision::Dismiss => "Note dismissed",
            NoteDecision::Accept(position) => {
                let Some(suggestion) = self.session.note().and_then(|note| {
                    note.suggestions
                        .iter()
                        .find(|item| item.position == position)
                }) else {
                    return;
                };
                match suggestion.action {
                    Suggestion::Archive => "Archived",
                    Suggestion::Label(_) => "Label added",
                    Suggestion::Reply => "Reply draft opened",
                    Suggestion::Snooze(_) => "Snoozed",
                    Suggestion::MarkRead => "Marked read",
                    Suggestion::Star => "Starred",
                }
            }
        };
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
                        inbox.session.show_toast(notice, false);
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
