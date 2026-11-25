use std::time::{Duration, Instant};
use std::{cell::Cell, rc::Rc};

use fell_core::notes::{NoteDecision, Suggestion, ThreadNote, display_safe};
use gpui::prelude::FluentBuilder;
use gpui::{
    AnyElement, Context, FontWeight, InteractiveElement, IntoElement, ParentElement, Pixels,
    ScrollHandle, StatefulInteractiveElement, Styled, Window, canvas, div, point, px, rgb,
};

use super::{
    Inbox, Live, MESSAGE_PAD_PX, ReadIntent, ReaderColumns, conversation_line_style, disclosure,
};
use crate::keymap::{self, Action};
use crate::model::{NoteRequest, SUGGESTIONS_QUERY, Session};
use crate::motion::{self, Motion, Transition};
use crate::present::{self, Pane};
use crate::scroll_anchor::{LandingGlide, ScrollAnchor, clamped_offset};
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
const DETAIL_FADE: Duration = Duration::from_millis(60);

fn fade_then_slide(t: f32) -> (f32, f32) {
    let split = DETAIL_FADE.as_secs_f32() / motion::PANE.as_secs_f32();
    (
        motion::fade_out(t / split),
        motion::ease_out((t - split) / (1.0 - split)),
    )
}

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
    fold: Option<((f32, f32), Transition)>,
    previous_suggestions: Option<(ThreadNote, Transition)>,
    scroll: ScrollHandle,
    visible: Rc<Cell<Option<i64>>>,
}

pub(super) struct NoteFrame {
    pub(super) note: ThreadNote,
    closed: bool,
    details: (f32, f32),
    height: f32,
    alpha: f32,
    pub(super) leaving: bool,
    suggestions: Option<(ThreadNote, f32)>,
    scroll: ScrollHandle,
    visible: Rc<Cell<Option<i64>>>,
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
                        .start(motion::PANE, now)
                        .map(|time| (old.clone(), time));
                }
            }
            if self.closed != closed {
                let from = self.details(now);
                let duration = if closed { motion::PANE } else { motion::SLIDE };
                self.fold = motion.start(duration, now).map(|time| (from, time));
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

    fn details(&self, now: Instant) -> (f32, f32) {
        let to = if self.closed { (0.0, 0.0) } else { (1.0, 1.0) };
        self.fold.map_or(to, |(from, time)| {
            if self.closed {
                let (alpha, movement) = fade_then_slide(time.linear(now));
                (from.0 * (1.0 - movement), from.1 * alpha)
            } else {
                let progress = time.progress(now);
                (
                    motion::lerp(from.0, to.0, progress),
                    motion::lerp(from.1, to.1, progress),
                )
            }
        })
    }

    pub(super) fn frame(&self, now: Instant) -> Option<NoteFrame> {
        self.visible.set(None);
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
            visible: self.visible.clone(),
        })
    }

    pub(super) fn visible(&self, note: i64) -> bool {
        self.visible.get() == Some(note)
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

impl NoteFrame {
    /// Use this frame's clip, including any disclosure and scrollport.
    fn visibility_probe(&self) -> impl IntoElement + use<> {
        let visible = self.visible.clone();
        let id = self.note.id;
        canvas(
            move |bounds, window, _| {
                let clip = window.content_mask().bounds;
                let height = bounds.bottom().min(clip.bottom()) - bounds.top().max(clip.top());
                if bounds.size.height > px(0.)
                    && clip.size.width > px(0.)
                    && bounds.intersects(&clip)
                    && height >= bounds.size.height / 2.0
                {
                    visible.set(Some(id));
                }
            },
            |_, (), _, _| {},
        )
        .absolute()
        .inset_0()
    }
}

const PINNED_HEADER_GAP: f32 = 8.0;
const PINNED_BORDER: f32 = 1.0;

fn header_height(ui_px: f32) -> Pixels {
    px(ui_px + 12.0)
}

pub(super) fn pinned_header_height(ui_px: f32) -> Pixels {
    header_height(ui_px) + px(PINNED_BORDER + PINNED_HEADER_GAP)
}

/// Pin the note carried into this reader or a new, unseen arrival in it.
#[derive(Default)]
pub(super) struct NotePin {
    id: Option<i64>,
    newest: Option<i64>,
    arrival: Option<Transition>,
    new_arrival: bool,
    arrival_band: Pixels,
    reader: Option<ScrollAnchor>,
    divider: Option<Transition>,
}

#[derive(Clone, Copy)]
pub(super) struct PinnedFrame {
    pub height: Pixels,
    pub alpha: f32,
    pub divider: f32,
}

impl NotePin {
    pub(super) fn opened(note: Option<i64>, newest: Option<i64>) -> Self {
        Self {
            id: note,
            newest,
            ..Self::default()
        }
    }

    fn observe(
        &mut self,
        note: Option<&ThreadNote>,
        newest: Option<i64>,
        scrolled: bool,
        motion: &Motion,
        now: Instant,
    ) -> bool {
        let arriving = scrolled
            && newest > self.newest
            && note.is_some_and(|note| !note.seen && Some(note.id) == newest);
        if arriving {
            self.id = newest;
            self.arrival = motion.start(motion::FADE, now);
            self.new_arrival = true;
            self.reader = None;
        }
        self.newest = self.newest.max(newest);
        arriving
    }

    pub(super) fn matches(&self, note: Option<&ThreadNote>) -> bool {
        self.id.is_some() && self.id == note.map(|note| note.id)
    }

    pub(super) fn eligible(&self) -> bool {
        self.id.is_some()
    }

    pub(super) fn arriving(&self) -> bool {
        self.new_arrival
    }

    pub(super) fn band(
        &self,
        note: Option<&ThreadNote>,
        visiting: bool,
        offset: Option<Pixels>,
        ui_px: f32,
    ) -> Pixels {
        if self.matches(note) && (visiting || offset.is_some_and(scrolled_from_top)) {
            pinned_header_height(ui_px)
        } else {
            px(0.)
        }
    }

    pub(super) fn paint(
        &mut self,
        target: Pixels,
        previous: Pixels,
        note: Option<&NoteFrame>,
        motion: &Motion,
        now: Instant,
    ) -> Option<PinnedFrame> {
        if self.new_arrival {
            self.arrival_band = previous;
            self.divider = None;
        } else if target > px(0.) && previous == px(0.) && self.arrival.is_none() {
            self.divider = motion.start(motion::QUICK, now);
        }
        self.divider = self.divider.filter(|fade| fade.running(now));
        let progress = motion::progress(self.arrival.as_ref(), now);
        if target > px(0.) {
            return Some(PinnedFrame {
                height: self.arrival_band + (target - self.arrival_band) * progress,
                alpha: progress,
                divider: progress * motion::progress(self.divider.as_ref(), now),
            });
        }
        // Keep the last height until the inline dismissal finishes fading.
        let leaving = note.filter(|note| {
            previous > px(0.) && note.leaving && self.id == Some(note.note.id) && note.alpha > 0.
        })?;
        let alpha = leaving.alpha * progress;
        Some(PinnedFrame {
            height: previous,
            alpha,
            divider: alpha * motion::progress(self.divider.as_ref(), now),
        })
    }

    pub(super) fn arrival_anchor(
        &mut self,
        target: Pixels,
        band: Pixels,
        selected: Option<ScrollAnchor>,
        now: Instant,
    ) -> Option<ScrollAnchor> {
        if std::mem::take(&mut self.new_arrival) && target > px(0.) {
            self.reader = selected
                .map(|mut anchor| {
                    anchor.y += self.arrival_band;
                    anchor
                })
                .filter(|anchor| anchor.y < target);
        }
        let anchor = self.reader.map(|mut anchor| {
            // Keep the window position until the band's edge reaches it.
            anchor.y = (anchor.y - band).max(px(0.));
            anchor
        });
        if self.arrival.is_none_or(|fade| !fade.running(now)) {
            self.arrival = None;
            self.reader = None;
        }
        anchor
    }

    pub(super) fn release_reader(&mut self) {
        self.reader = None;
    }

    pub(super) fn running(&self) -> bool {
        self.arrival.is_some() || self.divider.is_some()
    }
}

fn scrolled_from_top(offset: Pixels) -> bool {
    offset < px(0.)
}

fn active(session: &Session, frame: &NoteFrame, live: Live) -> bool {
    live.0
        && !frame.leaving
        && frame.alpha > 0.0
        && session.pane == Pane::Conversation
        && session.note().is_some_and(|note| note.id == frame.note.id)
}

fn note_header(
    theme: &Theme,
    session: &Session,
    frame: &NoteFrame,
    closed: bool,
    active: bool,
    id: &'static str,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    let (position, total) = session.agent_notes.position();
    let actor = &frame.note.actor;
    let dimmed = rgb(theme::note_text(theme.dimmed, theme.background));
    let kind = if frame.note.summary.is_empty() {
        "Agent suggestions"
    } else {
        "Agent summary"
    };
    let age = present::compact_relative_date(super::now_unix(), frame.note.created_unix);
    div()
        .id(id)
        .relative()
        .flex()
        .items_center()
        .gap(px(7.0))
        .h(header_height(theme.ui_px))
        .child(
            div()
                .flex_none()
                .text_color(dimmed)
                .child(if closed { "▸" } else { "▾" }),
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
                .child(
                    if actor.trim().is_empty() || display_safe(actor, false) != *actor {
                        String::new()
                    } else {
                        format!("· {actor}")
                    },
                ),
        )
        .when(total > 1 && !frame.leaving, |row| {
            row.child(div().flex_none().child(format!("{position}/{total}")))
        })
        .child(div().flex_none().text_color(dimmed).child(age))
        .when(active, |row| {
            row.when(id == "toggle-agent-note", |row| {
                row.child(frame.visibility_probe())
            })
            .cursor_pointer()
            .on_click(cx.listener(|inbox, _, _, cx| {
                let before = inbox.before_step();
                let step = inbox.session.perform(Action::ToggleAgentNote, 1);
                inbox.apply_step(step, before, cx);
            }))
        })
}

fn note_column(
    theme: &Theme,
    columns: ReaderColumns,
    stripe: bool,
    content: impl IntoElement,
) -> gpui::Div {
    div().flex_none().pl(px(MESSAGE_PAD_PX)).child(
        columns.card.place(
            div()
                .w_full()
                .when(stripe, |column| {
                    column
                        .border_l(px(2.0))
                        .border_color(rgb(theme.note_rule()))
                })
                .pl(px(if stripe { 11.0 } else { 13.0 }))
                .text_color(rgb(theme.foreground))
                .text_size(px(theme.ui_px - 1.0))
                .line_height(px(theme.ui_px + 3.0))
                .child(content)
                .into_any_element(),
        ),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn slot(
    theme: &Theme,
    session: &Session,
    columns: ReaderColumns,
    frame: &NoteFrame,
    inline_header: bool,
    pinned: bool,
    live: Live,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let line = theme.ui_px + 3.0;
    let width = (columns.card.width - 21.0).max(0.0);
    let active = active(session, frame, live);
    let header = inline_header.then(|| {
        note_header(
            theme,
            session,
            frame,
            frame.closed,
            active && !pinned,
            "toggle-agent-note",
            cx,
        )
        .opacity(if pinned { 0. } else { 1. })
    });
    let summary = display_safe(&frame.note.summary, true);
    let detail = (frame.details.0 > 0.0).then(|| {
        div().pr(px(8.0)).child(disclosure::reveal(
            width,
            frame.details.0,
            frame.details.1,
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
        ))
    });
    div().flex_none().child(disclosure::reveal(
        columns.card.inset + columns.card.width + MESSAGE_PAD_PX,
        frame.height,
        frame.alpha,
        note_column(
            theme,
            columns,
            true,
            div().children(header).children(detail),
        )
        .into_any_element(),
    ))
}

#[allow(clippy::too_many_arguments)]
pub(super) fn pinned_header(
    theme: &Theme,
    session: &Session,
    columns: ReaderColumns,
    frame: &NoteFrame,
    expanded: bool,
    paint: PinnedFrame,
    live: Live,
    cx: &mut Context<Inbox>,
) -> AnyElement {
    let header = note_header(
        theme,
        session,
        frame,
        !expanded,
        active(session, frame, live) && paint.alpha > 0.0,
        "toggle-pinned-note",
        cx,
    );
    let gap = px(PINNED_HEADER_GAP) * (paint.height / pinned_header_height(theme.ui_px));
    div()
        .id("pinned-agent-note-band")
        .flex_none()
        .h(paint.height)
        .overflow_hidden()
        .when(live.0, |band| {
            band.on_scroll_wheel(cx.listener(
                |inbox, event: &gpui::ScrollWheelEvent, window, cx| {
                    if inbox.session.overlay != super::Overlay::None {
                        return;
                    }
                    let scroll = &inbox.conv_scroll;
                    let old = clamped_offset(scroll);
                    let delta = event.delta.pixel_delta(window.line_height());
                    let y = (old + delta.y).clamp(-scroll.max_offset().height, px(0.));
                    if y != old {
                        scroll.set_offset(point(scroll.offset().x, y));
                        inbox.reader_took_over(false);
                        inbox.motion.cancel_glides();
                        cx.notify();
                    }
                },
            ))
        })
        .child(
            div()
                .h((paint.height - gap - px(PINNED_BORDER)).max(px(0.)))
                .overflow_hidden()
                .px(px(super::CONVERSATION_PAD_PX))
                .child(note_column(theme, columns, false, header))
                .opacity(paint.alpha),
        )
        .child(
            div()
                .h(px(PINNED_BORDER))
                .bg(rgb(theme.border))
                .opacity(paint.divider),
        )
        .into_any_element()
}

struct ChipMotion {
    removed: f32,
    slide: f32,
    restored: f32,
    reflow: f32,
}

impl ChipMotion {
    fn sample(t: f32, accepting: bool) -> Self {
        let fade = DETAIL_FADE.as_secs_f32() / motion::PANE.as_secs_f32();
        let slide = 1.0 - fade;
        if accepting {
            let (removed, movement) = fade_then_slide(t);
            Self {
                removed,
                slide: movement,
                restored: 1.0,
                reflow: motion::ease_out((t - fade) / fade),
            }
        } else {
            Self {
                removed: 0.0,
                slide: motion::ease_out(t / slide),
                restored: motion::ease_out((t - slide) / fade),
                reflow: motion::ease_out(t / fade),
            }
        }
    }
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
                    format!("Snooze {}", present::absolute_date(*until))
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
    let removed = |old: &Chip| !chips.iter().any(|chip| chip.decision == old.decision);
    let motion = ChipMotion::sample(t, previous.iter().any(removed));
    let height = |chips: &[Chip]| chips.last().map_or(0.0, |chip| chip.y + line);
    let current_height = height(&chips);
    let previous_height = if frame.suggestions.is_some() {
        height(&previous)
    } else {
        current_height
    };
    // A row-changing chip fades at each of its two positions. Keep the
    // lower row in the disclosure until its departing chips have faded.
    let height_progress = if current_height < previous_height {
        motion.slide * motion.reflow
    } else {
        motion.reflow
    };
    let note_id = frame.note.id;
    let mut row = div().relative().h(px(crate::motion::lerp(
        previous_height,
        current_height,
        height_progress,
    )));
    for old in previous
        .iter()
        .filter(|old| motion.removed > 0.0 && removed(old))
    {
        row = row.child(draw_chip(theme, old, line).opacity(motion.removed));
    }
    for mut chip in chips {
        let old = previous.iter().find(|old| old.decision == chip.decision);
        let alpha = match old {
            Some(old) if old.y != chip.y => {
                if motion.reflow < 1.0 {
                    row = row.child(draw_chip(theme, old, line).opacity(1.0 - motion.reflow));
                }
                motion.reflow
            }
            Some(old) => {
                chip.x = crate::motion::lerp(old.x, chip.x, motion.slide);
                1.0
            }
            None => motion.restored,
        };
        let decision = chip.decision;
        let id = match decision {
            NoteDecision::Dismiss => 0,
            NoteDecision::Accept(position) => position as usize + 1,
        };
        let element = draw_chip(theme, &chip, line)
            .id(("agent-suggestion", id))
            .when(clickable && alpha > 0.0, |chip| {
                chip.child(frame.visibility_probe())
                    .cursor_pointer()
                    .on_click(cx.listener(move |inbox, _, _, cx| {
                        if let Some(request) = inbox.session.note_request(decision)
                            && request.id == note_id
                        {
                            inbox.choose_agent_note(request, cx);
                        }
                    }))
            });
        row = row.child(element.opacity(alpha));
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

pub(super) struct NoteVisit {
    pub note: i64,
    message: String,
    y: Pixels,
    clipped_selection: bool,
    pub returning: Option<NoteReturn>,
}

pub(super) struct NoteReturn {
    from: Pixels,
    band: Pixels,
    time: Option<Transition>,
}

impl Inbox {
    pub(super) fn reader_entered_conversation(&mut self) {
        if self.open_thread == self.session.cursor_id
            && let Some(note) = self.session.note()
            && !self.note_pin.matches(Some(note))
        {
            // The list preview may have received the note before the reader opened.
            self.note_pin = NotePin::opened(Some(note.id), self.note_pin.newest);
        }
    }

    pub(super) fn toggle_pinned_note(&mut self) {
        if self.open_thread != self.session.cursor_id {
            return;
        }
        let Some(note) = self.session.note().map(|note| note.id) else {
            return;
        };
        if self.note_band == px(0.) {
            return;
        }
        self.release_landing();
        self.note_pin.release_reader();
        if self.session.agent_notes.collapsed {
            self.return_from_note(note);
            return;
        }
        let scroll = &self.conv_scroll;
        let offset = f32::from(clamped_offset(scroll));
        if let Some(visit) = &mut self.note_visit
            && visit.note == note
        {
            if visit.returning.take().is_some() {
                self.glide_conversation_for(offset, 0., motion::PANE, 0.);
            }
            return;
        }
        let first_message = usize::from(self.conv_note_slot);
        let selected = self.message_selected();
        let clipped = selected
            .then(|| {
                ScrollAnchor::capture_message(scroll, self.session.message_cursor, first_message)
            })
            .flatten()
            .filter(|anchor| anchor.y < px(0.));
        let anchor = clipped
            .map(|anchor| ScrollAnchor {
                y: px(0.),
                ..anchor
            })
            .or_else(|| ScrollAnchor::capture_header(scroll, first_message))
            // A single tall message may cover the entire viewport.
            .or_else(|| ScrollAnchor::capture(scroll, first_message));
        let Some(anchor) = anchor else { return };
        let Some(message) = self.session.messages.get(anchor.index) else {
            return;
        };
        let message = message.id.clone();
        let offset = self.motion.glide_from_conversation(offset);
        // The pinned band now supplies the inline header. Account for its
        // removal from the scroll content before the opening glide.
        let offset = (offset + f32::from(header_height(self.theme.ui_px))).min(0.);
        self.reveal = super::Reveal::Reading { selected };
        self.note_visit = Some(NoteVisit {
            note,
            message,
            y: anchor.y + self.note_band,
            clipped_selection: clipped.is_some(),
            returning: None,
        });
        self.glide_conversation_for(offset, 0., motion::PANE, 0.);
    }

    pub(super) fn agent_note_resolved(&mut self, request: &NoteRequest) {
        self.finish_note_decision();
        if self.open_thread.as_deref() == Some(&request.thread)
            && self.session.cursor_id == self.open_thread
        {
            self.return_from_note(request.id);
        }
        self.session.agent_notes.resolved(request);
    }

    pub(super) fn finish_note_decision(&mut self) {
        self.note_in_flight = None;
        self.note_generation = self.note_generation.wrapping_add(1);
    }

    fn note_read_current(&self, generation: u64) -> bool {
        generation == self.note_generation
            && self.note_in_flight.is_none()
            && self.undos_in_flight == 0
    }

    fn return_from_note(&mut self, note: i64) {
        let Some(visit) = &mut self.note_visit else {
            return;
        };
        if visit.note != note || visit.returning.is_some() {
            return;
        }
        let offset = f32::from(clamped_offset(&self.conv_scroll));
        let from = self.motion.glide_from_conversation(offset);
        self.motion.conv_glide = None;
        let duration = if visit.clipped_selection {
            motion::SLIDE
        } else {
            motion::PANE
        };
        visit.returning = Some(NoteReturn {
            from: px(from),
            band: self.note_band,
            time: self.motion.motion.start(duration, self.motion.clock.now()),
        });
    }

    pub(super) fn note_return(&mut self, now: Instant) -> Option<(ScrollAnchor, LandingGlide)> {
        let visit = self.note_visit.as_ref()?;
        let returning = visit.returning.as_ref()?;
        let index = self
            .session
            .messages
            .iter()
            .position(|message| message.id == visit.message);
        let y = visit.y - self.note_band;
        let glide = LandingGlide {
            from: returning.from + returning.band - self.note_band,
            progress: motion::progress(returning.time.as_ref(), now),
        };
        if glide.progress >= 1.0 || index.is_none() {
            self.note_visit = None;
        }
        index.map(|index| {
            (
                ScrollAnchor {
                    index,
                    y,
                    snap_headers: false,
                    body_scroll: None,
                },
                glide,
            )
        })
    }

    pub(super) fn update_agent_notes(
        &mut self,
        generation: u64,
        thread: &str,
        notes: Vec<ThreadNote>,
    ) -> bool {
        if !self.note_read_current(generation) {
            return false;
        }
        let scrolled = self.open_thread.as_deref() == Some(thread)
            && self.conv_layout_current
            && self.session.pane == Pane::Conversation
            && self.note_visit.is_none()
            && scrolled_from_top(clamped_offset(&self.conv_scroll));
        let newest = notes.first().map(|note| note.id);
        let state = &mut self.session.agent_notes;
        let changed = state.update(Some(thread.to_owned()), notes);
        if self.open_thread.as_deref() == Some(thread) {
            let current = state.current(Some(thread));
            if self.note_visit.as_ref().is_some_and(|visit| {
                visit.returning.is_none() && current.is_none_or(|note| note.id != visit.note)
            }) {
                self.note_visit = None;
            }
            let arriving = self.note_pin.observe(
                current,
                newest,
                scrolled,
                &self.motion.motion,
                self.motion.clock.now(),
            );
            if arriving
                && self
                    .session
                    .toast
                    .as_ref()
                    .is_some_and(|toast| toast.text == "Note dismissed")
            {
                self.session.clear_toast();
            }
        }
        changed
    }

    pub(super) fn watch_notes(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let mut seen = None;
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(450))
                    .await;
                let Ok(Some((store, account, open, to_see, generation))) =
                    this.update(cx, |inbox, _| {
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
                        Some((
                            inbox.store.clone(),
                            inbox.account_id.clone(),
                            open,
                            to_see,
                            inbox.note_generation,
                        ))
                    })
                else {
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
                        if !inbox.note_read_current(generation) {
                            return;
                        }
                        let state = &mut inbox.session.agent_notes;
                        let marker_changed = state.unread != unread;
                        let filter_changed = state.pending_threads != pending;
                        state.unread = unread;
                        state.pending_threads = pending;
                        let shown = notes.filter(|_| {
                            open == inbox.open_thread && open == inbox.session.cursor_id
                        });
                        let note_changed = open.zip(shown).is_some_and(|(thread, notes)| {
                            inbox.update_agent_notes(generation, &thread, notes)
                        });
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
            move |inbox, result, _| match result {
                Ok(_) => {
                    inbox.agent_note_resolved(&note);
                    inbox.send_to_server();
                    inbox.session.show_toast(notice, false);
                    Some(ReadIntent {
                        changed: true,
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    inbox.finish_note_decision();
                    inbox.session.show_toast(err.to_string(), true);
                    None
                }
            },
            cx,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AppContext, TestAppContext};

    fn note(id: i64, seen: bool) -> ThreadNote {
        ThreadNote {
            id,
            seen,
            actor: "Survey assistant".into(),
            created_unix: 0,
            summary: "Survey update".into(),
            suggestions: Vec::new(),
        }
    }

    fn test_inbox(path: std::path::PathBuf, cx: &mut Context<Inbox>) -> Inbox {
        Inbox::with_startup(
            crate::mailbox::Startup {
                keymap: keymap::Keymap::default(),
                split: 0.4,
                read_dwell_ms: 0,
                error: None,
                account_missing: true,
                config: None,
                path: Ok(path),
            },
            true,
            None,
            cx,
        )
    }

    #[test]
    fn paging_and_wheeling_keep_the_whole_band_outside_the_mail_viewport() {
        let note = note(1, true);
        let pin = NotePin::opened(Some(1), Some(1));
        let ui_px = 14.;
        let full_height = px(780.);
        let subject_bottom = px(66.);
        let viewport = |offset| {
            let top = subject_bottom + pin.band(Some(&note), false, Some(offset), ui_px);
            (top, full_height - top)
        };
        let landed = px(-660.);
        let (mail_top, page_height) = viewport(landed);
        assert_eq!(mail_top, subject_bottom + header_height(ui_px) + px(9.));
        // Page by one actual scrollport, then wheel by one line.
        assert_eq!(viewport(landed - page_height), (mail_top, page_height));
        assert_eq!(
            viewport(landed - page_height - px(24.)),
            (mail_top, page_height)
        );
    }

    #[test]
    fn the_band_replaces_the_inline_header_on_the_first_scroll() {
        let note = note(1, true);
        let pin = NotePin::opened(Some(1), Some(1));
        let ui_px = 14.;
        let header = header_height(ui_px);
        assert_eq!(pin.band(Some(&note), false, Some(px(0.)), ui_px), px(0.));
        let band = pin.band(Some(&note), false, Some(px(-1.)), ui_px);
        assert_eq!(band, header + px(9.));
        assert_eq!(pin.band(Some(&note), true, Some(px(0.)), ui_px), band);
    }

    #[test]
    fn a_header_or_chip_counts_even_when_the_note_cannot_fit() {
        use gpui::Render;

        struct Reader {
            note: NoteView,
            scroll: ScrollHandle,
        }

        impl Render for Reader {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let note = self.note.frame(Instant::now()).unwrap();
                div()
                    .id("short-reader")
                    .h(px(32.))
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(div().relative().h(px(26.)).child(note.visibility_probe()))
                    .child(div().h(px(75.)))
                    .child(div().relative().h(px(20.)).child(note.visibility_probe()))
                    .child(div().h(px(80.)))
            }
        }

        let mut app = TestAppContext::single();
        let (reader, cx) = app.add_window_view(|_, _| {
            let mut view = NoteView::default();
            view.observe(
                Some("thread"),
                Some(&note(1, false)),
                false,
                &Motion::new(false),
                Instant::now(),
            );
            Reader {
                note: view,
                scroll: ScrollHandle::new(),
            }
        });
        cx.run_until_parked();
        let visible = |cx: &mut _| reader.read_with(cx, |reader, _| reader.note.visible(1));
        assert!(visible(cx), "the header fits above the fold");
        for (offset, expected) in [
            (25., false), // One pixel of the header.
            (13., true),  // Half of the header.
            (55., false),
            (70., false), // One pixel of the chip.
            (79., true),  // Half of the chip.
            (145., false),
        ] {
            reader.update(cx, |reader, cx| {
                reader.scroll.set_offset(point(px(0.), px(-offset)));
                cx.notify();
            });
            cx.run_until_parked();
            assert_eq!(visible(cx), expected, "scroll offset {offset}");
        }
    }

    #[test]
    fn a_dismissed_open_note_cannot_pin_a_seen_or_already_known_replacement() {
        let start = Instant::now();
        let motion = Motion::new(true);
        let mut pin = NotePin::opened(Some(3), Some(4));
        assert!(pin.matches(Some(&note(3, true))));
        assert!(!pin.matches(None));
        let mut reopened = NotePin::opened(None, Some(3));
        reopened.observe(Some(&note(3, false)), Some(3), true, &motion, start);
        assert!(!reopened.matches(Some(&note(3, false))));

        pin.observe(Some(&note(4, false)), Some(4), true, &motion, start);
        assert!(!pin.matches(Some(&note(4, false))));
        pin.observe(Some(&note(5, true)), Some(5), true, &motion, start);
        assert!(!pin.matches(Some(&note(5, true))));
        pin.observe(Some(&note(6, false)), Some(6), false, &motion, start);
        pin.observe(Some(&note(6, false)), Some(6), true, &motion, start);
        assert!(!pin.matches(Some(&note(6, false))));

        // A genuinely new, unseen annotation can pin while reading.
        pin.observe(Some(&note(7, false)), Some(7), true, &motion, start);
        assert!(pin.matches(Some(&note(7, false))));
        let mut paint = |now| {
            pin.paint(pinned_header_height(14.), px(0.), None, &motion, now)
                .unwrap()
                .alpha
        };
        assert_eq!(paint(start), 0.);
        assert!(paint(start + Duration::from_millis(60)) > 0.);
        assert_eq!(paint(start + motion::FADE), 1.);
    }

    #[test]
    fn crossing_the_band_preserves_message_and_page_glide_destinations() {
        let now = Instant::now();
        let pin = NotePin::opened(Some(1), Some(1));
        let note = note(1, true);
        for target in [-92., -680.] {
            assert_eq!(pin.band(Some(&note), false, Some(px(0.)), 14.), px(0.));
            let mut glide =
                motion::Glide::start(&Motion::new(true), 0., target, 0., motion::SLIDE, now)
                    .unwrap();
            let crossing = now + Duration::from_millis(16);
            let drawn = glide.value(crossing);
            let band = f32::from(pin.band(Some(&note), false, Some(px(drawn)), 14.));
            assert!(band > 0.);
            glide.move_origin(crossing, drawn, band);
            assert!((band + glide.value(crossing) - drawn).abs() < 0.001);
            let destination_top = -target + band + glide.value(now + motion::SLIDE);
            assert_eq!(
                destination_top, band,
                "destination is flush with the mail viewport"
            );
        }
    }

    #[test]
    fn collapsing_a_scrolled_pinned_note_without_a_visit_keeps_the_offset() {
        use gpui::{Entity, Render};

        struct Reader(Entity<Inbox>);

        impl Render for Reader {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let inbox = self.0.read(cx);
                div()
                    .h(px(400.))
                    .id("test-reader")
                    .overflow_y_scroll()
                    .track_scroll(&inbox.conv_scroll)
                    .children((0..12).map(|_| div().h(px(80.))))
            }
        }

        let Some(root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let mut app = TestAppContext::single();
        let (reader, cx) =
            app.add_window_view(|_, cx| Reader(cx.new(|cx| test_inbox(root.join("test.db"), cx))));
        cx.run_until_parked();
        let inbox = reader.read_with(cx, |reader, _| reader.0.clone());
        let store = fell_core::Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        store.upsert_account(&account).unwrap();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let thread = store.list_threads(&account.id).unwrap().remove(0).thread_id;
        let messages = crate::view::read_conversation(
            &store,
            &account.id,
            &thread,
            (&account.email, &Default::default(), &[], "INBOX"),
        )
        .unwrap();
        inbox.update(cx, |inbox, _| {
            let id = thread.0;
            inbox.open_thread = Some(id.clone());
            inbox.session.cursor_id = Some(id.clone());
            inbox.session.pane = Pane::Conversation;
            inbox.session.messages = vec![messages[0].clone(); 11];
            inbox
                .session
                .agent_notes
                .update(Some(id), vec![note(1, true)]);
            inbox.note_band = pinned_header_height(inbox.theme.ui_px);
            inbox.conv_note_slot = true;
            inbox.conv_scroll.set_offset(point(px(0.), px(-140.)));
            inbox.reader_took_over(false);
            assert_eq!(clamped_offset(&inbox.conv_scroll), px(-140.));

            assert!(
                inbox
                    .session
                    .perform(Action::ToggleAgentNote, 1)
                    .toggle_agent_note
            );
            inbox.toggle_pinned_note();
            assert!(inbox.session.agent_notes.collapsed);
            assert_eq!(inbox.conv_scroll.offset().y, px(-140.));
            assert!(inbox.motion.conv_glide.is_none());
            assert!(inbox.note_visit.is_none());
        });
    }

    #[test]
    fn a_refresh_started_before_a_note_decision_cannot_restore_old_chips() {
        let Some(root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let mut app = TestAppContext::single();
        let inbox = app.new(|cx| test_inbox(root.join("test.db"), cx));
        inbox.update(&mut app, |inbox, _| {
            let thread = "survey";
            let mut before = note(1, true);
            before
                .suggestions
                .push(fell_core::notes::PendingSuggestion {
                    position: 0,
                    action: Suggestion::Star,
                });
            inbox.session.cursor_id = Some(thread.into());
            inbox.open_thread = Some(thread.into());
            let current = inbox.note_generation;
            assert!(inbox.update_agent_notes(current, thread, vec![before.clone()]));
            for decision in [NoteDecision::Accept(0), NoteDecision::Dismiss] {
                let old_generation = inbox.note_generation;
                let old_notes = vec![inbox.session.note().unwrap().clone()];
                inbox.note_in_flight = Some(1);
                assert!(!inbox.update_agent_notes(old_generation, thread, old_notes.clone()));
                inbox.agent_note_resolved(&NoteRequest {
                    thread: thread.into(),
                    id: 1,
                    decision,
                    reply: false,
                });
                assert!(!inbox.update_agent_notes(old_generation, thread, old_notes));
                match decision {
                    NoteDecision::Accept(_) => {
                        assert!(inbox.session.note().unwrap().suggestions.is_empty())
                    }
                    NoteDecision::Dismiss => assert!(inbox.session.note().is_none()),
                }
            }
            assert!(inbox.update_agent_notes(inbox.note_generation, thread, vec![note(2, false)]));
            assert_eq!(inbox.session.note().unwrap().id, 2);
        });
    }

    #[test]
    fn dismissal_keeps_the_band_until_the_header_finishes_fading() {
        let now = Instant::now();
        let motion = Motion::new(true);
        let note = note(1, true);
        let mut view = NoteView::default();
        let mut pin = NotePin::opened(Some(1), Some(1));
        view.observe(Some("thread"), Some(&note), false, &motion, now);
        view.observe(Some("thread"), None, false, &motion, now);
        let paint = |pin: &mut NotePin, elapsed| {
            let frame = view.frame(now + elapsed);
            pin.paint(
                px(0.),
                pinned_header_height(14.),
                frame.as_ref(),
                &motion,
                now + elapsed,
            )
        };
        assert_eq!(paint(&mut pin, Duration::ZERO).unwrap().alpha, 1.);
        let halfway = paint(&mut pin, Duration::from_millis(60)).unwrap();
        assert!(halfway.alpha > 0. && halfway.alpha < 1.);
        assert!(paint(&mut pin, motion::FADE).is_none());
    }

    #[test]
    fn a_live_note_grows_in_without_covering_the_selected_message() {
        let now = Instant::now();
        let note = note(1, false);
        let target = pinned_header_height(14.);
        let motion = Motion::new(true);
        for selected_y in [None, Some(px(-3.)), Some(px(3.)), Some(target)] {
            let mut pin = NotePin::default();
            assert!(pin.observe(Some(&note), Some(1), true, &motion, now));
            let mut previous = px(0.);
            // Sample every millisecond, including the first and settled frames.
            for ms in 0..=motion::FADE.as_millis() as u64 {
                let time = now + Duration::from_millis(ms);
                let paint = pin.paint(target, previous, None, &motion, time).unwrap();
                assert_eq!(paint.height, target * paint.alpha);
                assert_eq!(paint.divider, paint.alpha);
                assert!(paint.height >= previous);
                let selected = selected_y.map(|y| ScrollAnchor {
                    index: 4,
                    y,
                    snap_headers: false,
                    body_scroll: None,
                });
                let hold = pin.arrival_anchor(target, paint.height, selected, time);
                match selected_y {
                    Some(y) if y < target => {
                        let hold = hold.unwrap();
                        assert_eq!(hold.index, 4);
                        let row_top = paint.height + hold.y;
                        assert!(row_top >= paint.height);
                        assert_eq!(row_top, y.max(paint.height));
                    }
                    _ => assert!(hold.is_none()),
                }
                previous = paint.height;
            }
            assert_eq!(previous, target);
        }

        let mut pin = NotePin::opened(Some(1), Some(1));
        let replacement = self::note(2, false);
        assert!(pin.observe(Some(&replacement), Some(2), true, &motion, now));
        let paint = pin.paint(target, target, None, &motion, now).unwrap();
        assert_eq!(paint.height, target, "an existing band keeps its space");
    }

    #[test]
    fn undo_moves_remaining_chips_before_revealing_the_restored_chip() {
        let sample = |ms: f32| ChipMotion::sample(ms / 160., false);
        assert_eq!(sample(0.).restored, 0.);
        assert_eq!(sample(59.).restored, 0.);
        assert!(sample(59.).reflow < 1.);
        assert_eq!(sample(60.).reflow, 1.);
        assert!(sample(90.).slide < 1.);
        assert_eq!(sample(100.).slide, 1.);
        assert_eq!(sample(100.).restored, 0.);
        assert!(sample(130.).restored > 0. && sample(130.).restored < 1.);
        assert_eq!(sample(160.).restored, 1.);
    }

    #[test]
    fn details_and_accepted_chips_fade_before_their_space_moves() {
        let linear =
            |millis| Duration::from_millis(millis).as_secs_f32() / motion::PANE.as_secs_f32();
        let (alpha, movement) = fade_then_slide(linear(32));
        assert!(alpha > 0. && alpha < 1.);
        assert_eq!(movement, 0.);
        assert_eq!(fade_then_slide(linear(60)), (0., 0.));
        let (alpha, movement) = fade_then_slide(linear(90));
        assert_eq!(alpha, 0.);
        assert!(movement > 0. && movement < 1.);
        assert_eq!(fade_then_slide(1.), (0., 1.));
    }
}
