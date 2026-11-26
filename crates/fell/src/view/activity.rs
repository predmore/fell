//! The paged agent activity card and its exact-action undo controls.

use std::time::{Duration, Instant};

use fell_core::agent::{
    ActivityBlocker, AgentActivity, BLOCKER_MAILBOX_CHANGED, SNOOZE_CHANGED_NOTICE,
};
use gpui::{
    Context, Div, FontWeight, InteractiveElement, MouseButton, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, Window, div, point, prelude::FluentBuilder, px, rgb,
};

use super::{Inbox, ReadIntent, now_unix};
use crate::inbox_motion::SELECTION_FROM;
use crate::keymap::KeyId;
use crate::model::{
    Overlay,
    activity::{Activity, Request},
};
use crate::motion::{self, Motion, Transition};
use crate::present::{activity_time, compact_relative_date, display_safe, relative_date};
use crate::theme::{Theme, ensure_all};

const ROW_HEIGHT: f32 = 54.;
const TITLE_HEIGHT: f32 = 48.;
const FOOTER_HEIGHT: f32 = 48.;
const LOADING_DELAY: Duration = Duration::from_millis(150);

pub(super) fn card_height(available_height: f32) -> f32 {
    (TITLE_HEIGHT + 7. * ROW_HEIGHT + FOOTER_HEIGHT + 2.).min(available_height.max(158.))
}

const CONTENT_OUT: Duration = Duration::from_millis(60);
const CONTENT_DELAY: Duration = Duration::from_millis(40);
const CONTENT_RISE: f32 = 4.;

struct Leaving<T> {
    content: T,
    alpha: f32,
    fade: Transition,
}

impl<T> Leaving<T> {
    fn opacity(&self, now: Instant) -> f32 {
        self.alpha * self.fade.remaining(now)
    }
}

struct Page {
    activity: Activity,
    offset: f32,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct FooterText {
    text: String,
    compact: Option<String>,
    consequence: Option<&'static str>,
}

struct Footer {
    text: FooterText,
    buttons: [Option<FooterButton>; 3],
    enabled: [bool; 3],
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FooterButton {
    Newer,
    Older,
    Undo,
    Cancel,
    Confirm,
}

impl FooterButton {
    fn label(self, compact: bool) -> (&'static str, KeyId) {
        match self {
            Self::Newer => ("Newer", KeyId::Ctrl('u')),
            Self::Older => ("Older", KeyId::Ctrl('d')),
            Self::Undo => (
                if compact { "Undo" } else { "Undo action" },
                KeyId::Char('u'),
            ),
            Self::Cancel => ("Cancel", KeyId::Escape),
            Self::Confirm => ("Undo · y", KeyId::Char('y')),
        }
    }
}

#[derive(Default)]
struct SlotMotion {
    button: Option<FooterButton>,
    leaving: Option<Leaving<FooterButton>>,
    incoming: Option<(f32, Transition)>,
    enabled: bool,
    enable_fade: Option<(f32, Transition)>,
}

impl SlotMotion {
    fn frame(&self, now: Instant) -> Option<(FooterButton, f32)> {
        if let Some(old) = &self.leaving
            && old.fade.running(now)
        {
            return Some((old.content, old.opacity(now)));
        }
        self.button.map(|button| {
            (
                button,
                self.incoming.map_or(1., |(from, fade)| {
                    motion::lerp(from, 1., fade.progress(now))
                }),
            )
        })
    }

    fn enabled_at(&self, now: Instant) -> f32 {
        let target = if self.enabled { 1. } else { 0. };
        self.enable_fade.map_or(target, |(before, fade)| {
            motion::lerp(before, target, fade.progress(now))
        })
    }

    fn update(
        &mut self,
        next: Option<FooterButton>,
        enabled: bool,
        delay: Duration,
        motion: &Motion,
        now: Instant,
    ) -> bool {
        if self.enabled != enabled {
            let before = self.enabled_at(now);
            self.enabled = enabled;
            self.enable_fade = motion.start(motion::FADE, now).map(|fade| (before, fade));
        }
        if self.button != next {
            let drawn = self.frame(now).filter(|(_, alpha)| *alpha > 0.);
            self.button = next;
            self.incoming = None;
            self.leaving = None;
            if motion.enabled() {
                match drawn {
                    Some((button, alpha)) if Some(button) == next => {
                        self.incoming = motion.start(motion::FADE, now).map(|fade| (alpha, fade));
                    }
                    Some((button, alpha)) => {
                        self.leaving = motion.start(CONTENT_OUT, now).map(|fade| Leaving {
                            content: button,
                            alpha,
                            fade,
                        });
                        if next.is_some() {
                            self.incoming =
                                Some((0., Transition::new(now + CONTENT_OUT, CONTENT_OUT)));
                        }
                    }
                    None if next.is_some() => {
                        self.incoming = motion
                            .start(motion::FADE, now + delay)
                            .map(|fade| (0., fade));
                    }
                    None => {}
                }
            }
        }
        self.leaving = self.leaving.take().filter(|old| old.fade.running(now));
        self.leaving.is_some()
            || self.incoming.is_some_and(|(_, fade)| fade.running(now))
            || self.enable_fade.is_some_and(|(_, fade)| fade.running(now))
    }
}

#[derive(Default)]
pub(super) struct ActivityMotion {
    page: Option<Leaving<Page>>,
    row_in: Option<Transition>,
    selection: Option<i64>,
    selection_fade: Option<(Option<i64>, Transition)>,
    text: FooterText,
    old_text: Option<Leaving<FooterText>>,
    text_in: Option<Transition>,
    slots: [SlotMotion; 3],
    undone: Option<(i64, Transition)>,
}

impl ActivityMotion {
    fn for_open(activity: &Activity, now: i64) -> Self {
        let footer = Footer::of(activity, now);
        Self {
            selection: activity.selected_entry().map(|entry| entry.action.id),
            text: footer.text,
            slots: std::array::from_fn(|index| SlotMotion {
                button: footer.buttons[index],
                enabled: footer.enabled[index],
                ..SlotMotion::default()
            }),
            ..Self::default()
        }
    }

    pub(super) fn settle(&mut self) {
        self.selection_fade = None;
    }

    fn change_page(&mut self, before: Activity, offset: f32, motion: &Motion, now: Instant) {
        let alpha = motion::progress(self.row_in.as_ref(), now);
        let page = Page {
            activity: before,
            offset,
        };
        let (page, alpha) = match self.page.take() {
            Some(old) if old.opacity(now) > alpha => {
                let opacity = old.opacity(now);
                (old.content, opacity)
            }
            _ => (page, alpha),
        };
        let replacing = !page.activity.page.entries.is_empty();
        self.page = replacing
            .then(|| motion.start(CONTENT_OUT, now))
            .flatten()
            .map(|fade| Leaving {
                content: page,
                alpha,
                fade,
            });
        let delay = if replacing {
            CONTENT_DELAY
        } else {
            Duration::ZERO
        };
        self.row_in = motion.start(motion::FADE, now + delay);
    }

    pub(super) fn frame(&mut self, activity: &Activity, motion: &Motion, now: Instant) -> bool {
        let selected = activity.selected_entry().map(|entry| entry.action.id);
        if self.selection != selected {
            let before = std::mem::replace(&mut self.selection, selected);
            self.selection_fade = motion.start(motion::FADE, now).map(|fade| (before, fade));
        }
        let footer = Footer::of(activity, now_unix());
        if self.text != footer.text {
            let before = std::mem::replace(&mut self.text, footer.text);
            let alpha = motion::progress(self.text_in.as_ref(), now);
            self.old_text = match self.old_text.take() {
                Some(old) if old.opacity(now) > alpha => Some(old),
                _ if !before.text.is_empty() => {
                    motion.start(CONTENT_OUT, now).map(|fade| Leaving {
                        content: before,
                        alpha,
                        fade,
                    })
                }
                _ => None,
            };
            let delay = if self.text.consequence.is_some() || self.confirmation_leaving() {
                CONTENT_OUT
            } else if self.old_text.is_some() {
                CONTENT_DELAY
            } else {
                Duration::ZERO
            };
            self.text_in = motion.start(motion::FADE, now + delay);
        }
        self.old_text = self.old_text.take().filter(|text| text.fade.running(now));
        self.page = self.page.take().filter(|page| page.fade.running(now));
        let confirmation_leaving = self.confirmation_leaving();
        let mut slots_animating = false;
        for (index, slot) in self.slots.iter_mut().enumerate() {
            // The confirmation title uses the Undo slot's space.
            let delay = if index == 0 && confirmation_leaving {
                CONTENT_OUT
            } else {
                Duration::ZERO
            };
            slots_animating |= slot.update(
                footer.buttons[index],
                footer.enabled[index],
                delay,
                motion,
                now,
            );
        }
        slots_animating
            || self.page.is_some()
            || self.row_in.is_some_and(|fade| fade.running(now))
            || self.old_text.is_some()
            || self.text_in.is_some_and(|fade| fade.running(now))
            || self
                .selection_fade
                .is_some_and(|(_, fade)| fade.running(now))
            || self.undone.is_some_and(|(_, fade)| fade.running(now))
    }

    fn confirmation_leaving(&self) -> bool {
        self.old_text
            .as_ref()
            .is_some_and(|text| text.content.consequence.is_some())
    }

    fn selection(&self, id: i64, selected: bool, now: Instant) -> f32 {
        match self.selection_fade {
            Some((_, fade)) if selected => motion::lerp(SELECTION_FROM, 1., fade.progress(now)),
            Some((Some(before), fade)) if before == id => SELECTION_FROM * fade.remaining(now),
            _ if selected => 1.,
            _ => 0.,
        }
    }
}

impl Inbox {
    pub(super) fn request_agent_activity(&mut self, request: Request, cx: &mut Context<Self>) {
        let generation = self.session.activity.generation;
        let delay = cx.background_executor().timer(LOADING_DELAY);
        cx.spawn(async move |this, cx| {
            delay.await;
            let _ = this.update(cx, |inbox, cx| {
                let activity = &mut inbox.session.activity;
                if activity.generation == generation && activity.pending {
                    activity.loading_slow = true;
                    cx.notify();
                }
            });
        })
        .detach();
        match request {
            Request::Load {
                direction,
                ref filter,
                ..
            } => {
                let account_wide = filter.is_none();
                let opening = direction == fell_core::agent::ActivityDirection::Latest;
                if opening {
                    self.activity_motion =
                        ActivityMotion::for_open(&self.session.activity, now_unix());
                    self.activity_scroll.set_offset(point(px(0.), px(0.)));
                }
                let account = self.account_id.clone();
                let reply = self.store.call(move |store| request.load(store, &account));
                cx.spawn(async move |this, cx| {
                    let reply = reply.await;
                    let _ = this.update(cx, |inbox, cx| {
                        let activity = &mut inbox.session.activity;
                        if activity.generation != generation {
                            return;
                        }
                        let before = activity.clone();
                        activity.pending = false;
                        activity.loading_slow = false;
                        match reply {
                            Ok(Ok(page)) => {
                                if let fell_core::agent::ActivityDirection::At(id) = direction {
                                    let (show, retry) =
                                        activity.finish_jump(&page, id, account_wide);
                                    if !show {
                                        if let Some(request) = retry
                                            && inbox.session.overlay == Overlay::AgentActivity
                                        {
                                            inbox.request_agent_activity(request, cx);
                                        }
                                        cx.notify();
                                        return;
                                    }
                                }
                                activity.loaded(page, direction);
                                if opening {
                                    inbox.activity_motion =
                                        ActivityMotion::for_open(activity, now_unix());
                                } else if before.page_revision != activity.page_revision {
                                    let now = inbox.motion.clock.now();
                                    inbox.activity_motion.change_page(
                                        before,
                                        inbox.activity_scroll.offset().y.into(),
                                        &inbox.motion.motion,
                                        now,
                                    );
                                    inbox.activity_scroll.set_offset(point(px(0.), px(0.)));
                                }
                                inbox.activity_scroll.scroll_to_item(activity.selected);
                            }
                            _ => activity.load_failed = true,
                        }
                        cx.notify();
                    });
                })
                .detach();
            }
            Request::Undo {
                account,
                id,
                confirmed_bulk,
            } => {
                let viewing_account = account == self.account_id;
                self.change_mailbox(
                    move |store| store.undo_agent_activity(&account, id, confirmed_bulk),
                    move |inbox, result, cx| {
                        let activity = &mut inbox.session.activity;
                        let current = activity.generation == generation;
                        if current {
                            activity.pending = false;
                            activity.pending_bulk_undo = None;
                            activity.loading_slow = false;
                        }
                        let (notice, error, changed) = match result {
                            Ok(action) => {
                                let row_visible = inbox.session.overlay == Overlay::AgentActivity
                                    && activity
                                        .page
                                        .entries
                                        .iter()
                                        .position(|entry| entry.action.id == id)
                                        .and_then(|index| {
                                            inbox.activity_scroll.bounds_for_item(index)
                                        })
                                        .is_some_and(|bounds| {
                                            bounds.intersects(&inbox.activity_scroll.bounds())
                                        });
                                let retired = action.summary == SNOOZE_CHANGED_NOTICE;
                                activity.mark_undone(action.id, retired);
                                let now = inbox.motion.clock.now();
                                inbox.activity_motion.undone = inbox
                                    .motion
                                    .motion
                                    .start(motion::FADE, now)
                                    .map(|fade| (action.id, fade));
                                if viewing_account {
                                    inbox.note_generation = inbox.note_generation.wrapping_add(1);
                                    inbox.send_to_server();
                                }
                                (
                                    (!retired && (confirmed_bulk.is_some() || !row_visible))
                                        .then(|| "Agent action undone".into()),
                                    false,
                                    viewing_account,
                                )
                            }
                            Err(fell_core::Error::AgentActivityBlocked(blocker)) if current => {
                                let request = activity.record_blocker(id, blocker);
                                inbox.activity_scroll.scroll_to_item(activity.selected);
                                if let Some(request) = request
                                    && inbox.session.overlay == Overlay::AgentActivity
                                {
                                    inbox.request_agent_activity(request, cx);
                                }
                                return None;
                            }
                            Err(fell_core::Error::AgentActivityNeedsConfirmation {
                                conversations,
                            }) if current && inbox.session.overlay == Overlay::AgentActivity => {
                                activity.request_confirmation(id, conversations);
                                return None;
                            }
                            Err(fell_core::Error::AgentActivityAlreadyUndone {
                                snooze_retired,
                            }) => {
                                activity.mark_undone(id, snooze_retired);
                                return None;
                            }
                            Err(
                                fell_core::Error::Action(message)
                                | fell_core::Error::Invalid(message),
                            ) => (Some(display_safe(&message)), true, false),
                            Err(_) => (Some("Could not undo agent action".into()), true, false),
                        };
                        if let Some(notice) = notice {
                            inbox.session.show_toast(notice, error);
                            inbox.expire_action_notice(cx);
                        }
                        changed.then_some(ReadIntent {
                            changed: true,
                            refresh_open: true,
                            ..ReadIntent::default()
                        })
                    },
                    cx,
                );
            }
        }
    }

    fn activity_button(&mut self, key: KeyId, cx: &mut Context<Self>) {
        if self.session.overlay != Overlay::AgentActivity {
            return;
        }
        let before = self.before_step();
        let step = self.session.on_key(key, self.page_rows());
        self.apply_step(step, before, cx);
    }
}

pub(super) fn card(
    theme: &Theme,
    activity: &Activity,
    scroll: Option<&ScrollHandle>,
    (height, compact): (f32, bool),
    frame: Option<(&ActivityMotion, Instant)>,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> Div {
    let live = scroll.is_some();
    let now = now_unix();
    let dim = ensure_all(theme.dimmed, &[theme.surface], 4.65);
    let leaving = frame.and_then(|(motion, time)| Some((motion.page.as_ref()?, time)));
    let arrival = frame.map_or(1., |(motion, time)| {
        motion::progress(motion.row_in.as_ref(), time)
    });
    let footer = Footer::of(activity, now);
    let rows =
        div()
            .relative()
            .flex_1()
            .min_h(px(0.))
            .overflow_hidden()
            .when_some(leaving, |list, (old, time)| {
                list.child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(px(old.content.offset))
                        .opacity(old.opacity(time))
                        .children((0..old.content.activity.page.entries.len()).map(|index| {
                            row(theme, &old.content.activity, index, now, compact, None)
                        })),
                )
            })
            .child(
                div()
                    .id("agent-activity-list")
                    .relative()
                    .top(px(CONTENT_RISE * (1. - arrival)))
                    .size_full()
                    .overflow_y_scroll()
                    .opacity(arrival)
                    .when_some(scroll, |list, scroll| list.track_scroll(scroll))
                    .children((0..activity.page.entries.len()).map(|index| {
                        row(theme, activity, index, now, compact, frame).when(live, |row| {
                            row.on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |inbox, _, _, cx| {
                                    if inbox.session.overlay == Overlay::AgentActivity {
                                        inbox.session.activity.select(index);
                                        cx.notify();
                                    }
                                }),
                            )
                        })
                    }))
                    .when(activity.page.entries.is_empty(), |list| {
                        let text = if activity.pending {
                            activity.loading_slow.then_some("Loading activity…")
                        } else if activity.load_failed {
                            Some("Could not load agent activity")
                        } else {
                            Some("No agent activity yet")
                        };
                        list.when_some(text, |list, text| {
                            list.child(
                                div()
                                    .px(px(16.))
                                    .py(px(16.))
                                    .text_color(rgb(dim))
                                    .child(text),
                            )
                        })
                    }),
            );
    div()
        .w(px(super::AGENT_CARD_WIDTH))
        .max_w_full()
        .mx(px(10.))
        .h(px(height))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .when(live, |card| {
            card.on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        })
        .child(
            div()
                .flex_none()
                .h(px(TITLE_HEIGHT))
                .px(px(16.))
                .flex()
                .items_center()
                .gap(px(10.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_size(px(theme.ui_px + 3.))
                        .child("Agent activity"),
                )
                .when_some(activity.filter.as_ref(), |header, agent| {
                    header.child(
                        div()
                            .min_w_0()
                            .line_clamp(1)
                            .text_ellipsis()
                            .text_color(rgb(dim))
                            .child(agent_name(&agent.agent_id)),
                    )
                }),
        )
        .child(rows)
        .child(
            div()
                .relative()
                .flex_none()
                .h(px(FOOTER_HEIGHT))
                .border_t(px(1.))
                .border_color(rgb(theme.border))
                .overflow_hidden()
                .child(footer.render(theme, compact, frame, live && !activity.pending, window, cx)),
        )
}

impl Footer {
    fn of(activity: &Activity, now: i64) -> Self {
        let enabled = !activity.pending || !activity.loading_slow;
        let mut footer = Self {
            text: FooterText::default(),
            buttons: [None, Some(FooterButton::Newer), Some(FooterButton::Older)],
            enabled: [
                false,
                activity.page.has_newer && enabled,
                activity.page.has_older && enabled,
            ],
        };
        if let Some(prompt) = &activity.confirmation
            && let Some(entry) = activity.selected_entry()
        {
            let (effect, consequence) = match entry.action.kind.as_str() {
                "archive" => ("Unarchive", "They return to Inbox"),
                "move" => ("Move back", "They return to their previous folders"),
                "label" | "unlabel" => ("Restore labels on", "Their previous label state returns"),
                "star" | "unstar" => ("Restore stars on", "Their previous star state returns"),
                "mark_read" | "mark_unread" => (
                    "Restore read status on",
                    "Their previous read state returns",
                ),
                "snooze" | "unsnooze" => {
                    ("Restore snooze on", "Their previous snooze state returns")
                }
                _ => ("Undo the change to", "Their previous state returns"),
            };
            let name = actor_name(&entry.action.actor);
            footer.text.text = format!(
                "{effect} {} conversations from {name}?",
                prompt.conversations
            );
            footer.text.compact = Some(format!("{effect} {} from {name}?", prompt.conversations));
            footer.text.consequence = Some(consequence);
            footer.buttons = [
                None,
                Some(FooterButton::Cancel),
                Some(FooterButton::Confirm),
            ];
            footer.enabled = [false, true, true];
            return footer;
        }
        let entry = activity.selected_entry();
        if entry.is_some_and(|entry| {
            !entry.action.undone
                && !entry.draft_discarded
                && entry.blocker.is_none()
                && activity.pending_bulk_undo != Some(entry.action.id)
        }) {
            footer.buttons[0] = Some(FooterButton::Undo);
            footer.enabled[0] = enabled;
        }
        footer.text.text = if let Some(blocker) = &activity.blocker_notice {
            footer.text.compact = Some(blocker_notice(blocker, now, true));
            blocker_notice(blocker, now, false)
        } else if activity.load_failed && !activity.page.entries.is_empty() {
            "Could not load agent activity".into()
        } else if activity.pending
            && activity.loading_slow
            && activity.loading_page
            && !activity.page.entries.is_empty()
        {
            "Loading activity…".into()
        } else {
            entry
                .filter(|e| e.action.created_unix > 0)
                .map_or_else(String::new, |entry| {
                    activity_time(now, entry.action.created_unix)
                })
        };
        footer
    }

    fn render(
        &self,
        theme: &Theme,
        compact: bool,
        frame: Option<(&ActivityMotion, Instant)>,
        live: bool,
        window: &Window,
        cx: &mut Context<Inbox>,
    ) -> Div {
        const GAP: f32 = 6.;
        let pad = if compact { 10. } else { 14. };
        let width = |labels: &[&str], minimum: f32| {
            (labels
                .iter()
                .map(|label| {
                    super::bar_px(window, label, theme.ui_px - 1., theme.ui_family.as_ref())
                })
                .fold(0., f32::max)
                .ceil()
                + 22.)
                .max(minimum)
        };
        let widths = footer_slot_widths(compact, width);
        let incoming = frame.map_or(1., |(motion, now)| {
            motion::progress(motion.text_in.as_ref(), now)
        });
        let leaving = frame.and_then(|(motion, now)| Some((motion.old_text.as_ref()?, now)));
        let panel =
            (f32::from(window.viewport_size().width) - 20.).min(super::AGENT_CARD_WIDTH) - 2.;
        let text_layer = |text: &FooterText, alpha, rise| {
            let reserved = if text.consequence.is_some() {
                widths[1..].iter().sum::<f32>() + 2. * GAP
            } else {
                widths.iter().sum::<f32>() + 3. * GAP
            };
            text.render(
                theme,
                compact,
                (panel - 2. * pad - reserved).max(0.),
                window,
            )
            .absolute()
            .left(px(pad))
            .right(px(pad + reserved))
            .top(px(rise))
            .bottom(px(-rise))
            .opacity(alpha)
        };
        div()
            .relative()
            .size_full()
            .px(px(pad))
            .flex()
            .items_center()
            .when_some(leaving, |footer, (old, now)| {
                footer.child(text_layer(&old.content, old.opacity(now), 0.))
            })
            .child(text_layer(
                &self.text,
                incoming,
                CONTENT_RISE * (1. - incoming),
            ))
            .child(div().flex_1())
            .child(div().flex().flex_none().gap(px(GAP)).children(
                widths.into_iter().enumerate().map(|(index, width)| {
                    let selected = self.buttons[index];
                    let shown = match frame {
                        Some((motion, now)) => motion.slots[index].frame(now),
                        None => selected.map(|button| (button, 1.)),
                    };
                    let enabled = self.enabled[index];
                    let strength = frame.map_or(if enabled { 1. } else { 0. }, |(motion, now)| {
                        motion.slots[index].enabled_at(now)
                    });
                    div()
                        .w(px(width))
                        .flex_none()
                        .when_some(shown, |slot, (control, alpha)| {
                            let (label, key) = control.label(compact);
                            slot.child(
                                button(
                                    theme,
                                    label,
                                    key,
                                    strength,
                                    live && enabled && selected == Some(control),
                                    cx,
                                )
                                .w_full()
                                .opacity(alpha),
                            )
                        })
                }),
            ))
    }
}

fn footer_slot_widths(compact: bool, width: impl Fn(&[&str], f32) -> f32) -> [f32; 3] {
    let paging = width(
        &[
            FooterButton::Newer,
            FooterButton::Cancel,
            FooterButton::Older,
            FooterButton::Confirm,
        ]
        .map(|button| button.label(compact).0),
        if compact { 64. } else { 68. },
    );
    [
        width(
            &[FooterButton::Undo.label(compact).0],
            if compact { 76. } else { 108. },
        ),
        paging,
        paging,
    ]
}

impl FooterText {
    fn render(&self, theme: &Theme, compact: bool, room: f32, window: &Window) -> Div {
        let dim = ensure_all(theme.dimmed, &[theme.surface], 4.65);
        let text = if compact {
            self.compact.as_ref().unwrap_or(&self.text)
        } else {
            &self.text
        };
        let style = super::conversation_line_style(window, theme, px(theme.ui_px - 1.));
        let weight = if self.consequence.is_some() {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };
        let color = if self.consequence.is_some() {
            theme.foreground
        } else {
            dim
        };
        let title = super::clip_shaped(window, &style, text, room, weight, Some(color))
            .map(|line| super::shaped_text(line, window).h(px(theme.ui_px + 3.)));
        let detail = self
            .consequence
            .and_then(|text| {
                super::clip_shaped(window, &style, text, room, FontWeight::NORMAL, Some(dim))
            })
            .map(|line| super::shaped_text(line, window).h(px(theme.ui_px + 3.)));
        div()
            .flex()
            .flex_col()
            .justify_center()
            .overflow_hidden()
            .children(title)
            .children(detail)
    }
}

fn blocker_notice(blocker: &ActivityBlocker, now: i64, compact: bool) -> String {
    let kind = match blocker.kind.as_str() {
        "star" => "star",
        "unstar" => "unstar",
        "archive" => "archive",
        "move" => "move",
        "draft" => "draft",
        "mark_read" | "mark_unread" | "read" | "unread" => "read change",
        "snooze" | "unsnooze" => "snooze change",
        "label" | "unlabel" => "label change",
        "annotate" => "note",
        _ => "action",
    };
    match blocker.actor.as_str() {
        "local" => format!("Undo your newer {kind} in the mailbox first"),
        "outbox" => "A newer move is waiting to sync".into(),
        _ if blocker.kind == BLOCKER_MAILBOX_CHANGED => "Mail changed since this action".into(),
        actor if actor.starts_with("agent:") && compact => {
            format!("Undo {}’s {kind} first", actor_name(actor))
        }
        actor if actor.starts_with("agent:") => format!(
            "Undo {}’s newer {kind} first · {}",
            actor_name(actor),
            relative_date(now, blocker.created_unix)
        ),
        _ => format!("Undo the newer {kind} in the mailbox first"),
    }
}

fn footer_button_outline(theme: &Theme) -> u32 {
    let outline = theme.label_outline(theme.surface);
    let toward_text = crate::theme::mix(outline, theme.foreground, 0.08);
    ensure_all(toward_text, &[theme.surface], 3.5)
}

fn button(
    theme: &Theme,
    label: &'static str,
    key: KeyId,
    strength: f32,
    live: bool,
    cx: &mut Context<Inbox>,
) -> Div {
    let dim = motion::mix(theme.surface, theme.dimmed, 0.4);
    div()
        .px(px(10.))
        .h(px(28.))
        .flex()
        .justify_center()
        .items_center()
        .rounded_md()
        .border_1()
        .border_color(rgb(footer_button_outline(theme)))
        .text_color(rgb(motion::mix(dim, theme.foreground, strength)))
        .text_size(px(theme.ui_px - 1.))
        .child(label)
        .when(live, |button| {
            button.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(move |inbox, _, _, cx| {
                    cx.stop_propagation();
                    inbox.activity_button(key, cx);
                }),
            )
        })
}

fn row(
    theme: &Theme,
    activity: &Activity,
    index: usize,
    now: i64,
    compact: bool,
    frame: Option<(&ActivityMotion, Instant)>,
) -> Div {
    let entry = &activity.page.entries[index];
    let selected = index == activity.selected;
    let selection = frame.map_or(f32::from(u8::from(selected)), |(motion, time)| {
        motion.selection(entry.action.id, selected, time)
    });
    let bg = motion::mix(theme.surface, theme.selection, selection);
    let fg = ensure_all(
        motion::mix(theme.foreground, theme.on_selection, selection),
        &[bg],
        4.65,
    );
    let dim = ensure_all(
        motion::mix(theme.dimmed, theme.on_selection_dim, selection),
        &[bg],
        4.65,
    );
    let mut date = div()
        .w(px(88.))
        .flex_none()
        .text_right()
        .text_color(rgb(dim))
        .line_clamp(1)
        .text_ellipsis()
        .child(compact_relative_date(now, entry.action.created_unix));
    date.text_style()
        .get_or_insert_with(Default::default)
        .font_features = Some(super::tabular_figures());
    let status_alpha = frame
        .and_then(|(motion, time)| {
            let (id, fade) = motion.undone?;
            (id == entry.action.id).then(|| fade.progress(time))
        })
        .unwrap_or(1.);
    div()
        .relative()
        .h(px(ROW_HEIGHT))
        .px(px(16.))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(2.))
        .when(index + 1 < activity.page.entries.len(), |row| {
            row.border_b(px(1.))
        })
        .border_color(rgb(theme.border))
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .when(selection > 0., |row| {
            row.child(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(2.))
                    .bg(rgb(theme.accent))
                    .opacity(selection),
            )
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .when(activity.filter.is_none(), |line| {
                    line.child(super::agent_notes::marker(theme, false))
                        .child(
                            div()
                                .min_w_0()
                                .line_clamp(1)
                                .text_ellipsis()
                                .child(actor_name(&entry.action.actor)),
                        )
                        .child(div().flex_none().text_color(rgb(dim)).child("·"))
                })
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(action_name(&entry.action.kind)),
                )
                .child(div().flex_1())
                .child(date),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(5.))
                .when(activity.filter.is_none(), |line| {
                    line.pl(px(super::agent_notes::MARKER_SLOT + 6.))
                })
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .child(
                            div()
                                .min_w_0()
                                .line_clamp(1)
                                .text_ellipsis()
                                .child(target_name(entry)),
                        )
                        .when(
                            entry.conversations > 1 && entry.subject.is_some(),
                            |target| {
                                target.child(
                                    div()
                                        .flex_none()
                                        .text_color(rgb(dim))
                                        .child(format!("+ {} more", entry.conversations - 1)),
                                )
                            },
                        ),
                )
                .when_some(row_status(entry, compact), |line, status| {
                    line.child(
                        div()
                            .flex_none()
                            .text_color(rgb(dim))
                            .opacity(status_alpha)
                            .child(status),
                    )
                }),
        )
}

fn actor_name(actor: &str) -> String {
    agent_name(actor.strip_prefix("agent:").unwrap_or(actor))
}

fn agent_name(name: &str) -> String {
    let safe = display_safe(name);
    let mut chars = safe.chars();
    let Some(first) = chars.next().filter(|_| !safe.trim().is_empty()) else {
        return "Agent".into();
    };
    first.to_uppercase().chain(chars).collect()
}

fn row_status(entry: &AgentActivity, compact: bool) -> Option<&'static str> {
    if entry.snooze_retired {
        Some(if compact {
            "Skipped"
        } else {
            "Skipped · snooze changed since"
        })
    } else if entry.action.undone {
        Some("Undone")
    } else if entry.draft_discarded {
        Some("Draft discarded")
    } else if let Some(blocker) = &entry.blocker {
        Some(if blocker.kind == BLOCKER_MAILBOX_CHANGED {
            "Mail changed"
        } else {
            "Blocked"
        })
    } else if entry.agent_off {
        Some("Agent off")
    } else {
        None
    }
}

fn target_name(entry: &AgentActivity) -> String {
    match entry.subject.as_deref().map(display_safe) {
        Some(subject) if !subject.trim().is_empty() => subject,
        Some(_) => "(No subject)".into(),
        None if entry.conversations > 1 => format!("{} conversations", entry.conversations),
        None if entry.action.kind == "draft" => "Review draft".into(),
        None => "Conversation unavailable".into(),
    }
}

fn action_name(kind: &str) -> &'static str {
    match kind {
        "archive" => "Archived",
        "move" => "Moved",
        "label" => "Added label",
        "unlabel" => "Removed label",
        "mark_read" => "Marked read",
        "mark_unread" => "Marked unread",
        "star" => "Starred",
        "unstar" => "Unstarred",
        "snooze" => "Snoozed",
        "unsnooze" => "Unsnoozed",
        "draft" => "Saved draft",
        "annotate" => "Added note",
        _ => "Agent action",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::agent::{ActivityDirection, AgentActivityPage};
    use fell_core::{AccountId, RecordedAction};
    use std::sync::Arc;

    fn entry() -> AgentActivity {
        AgentActivity {
            action: RecordedAction {
                id: 1,
                account_id: AccountId("demo".into()),
                actor: "agent:Fe\u{202e}rn".into(),
                kind: "archive".into(),
                summary: String::new(),
                undone: false,
                created_unix: 10,
            },
            subject: Some("Trip\n plan\u{200b}".into()),
            conversations: 1,
            blocker: None,
            draft_discarded: false,
            snooze_retired: false,
            agent_off: false,
        }
    }

    fn bulk_activity() -> Activity {
        let mut activity = Activity::default();
        activity.page = Arc::new(AgentActivityPage {
            entries: vec![AgentActivity {
                conversations: 12,
                ..entry()
            }],
            has_newer: true,
            has_older: true,
        });
        activity
    }

    #[test]
    fn paging_slots_reserve_the_widest_label_at_both_widths() {
        for compact in [false, true] {
            for (widest, paging) in [("Undo · y", 95.), ("Cancel", 100.), ("", 68.)] {
                let widths = footer_slot_widths(compact, |labels, minimum| {
                    if labels.contains(&widest) {
                        paging
                    } else {
                        minimum
                    }
                });
                let expected = if compact && widest.is_empty() {
                    64.
                } else {
                    paging
                };
                assert_eq!(
                    widths,
                    [if compact { 76. } else { 108. }, expected, expected]
                );
            }
        }
    }

    #[test]
    fn footer_preserves_button_strength_until_a_request_is_slow() {
        let mut activity = bulk_activity();
        let motion = Motion::new(false);
        let now = Instant::now();
        let mut frame = ActivityMotion::for_open(&activity, now_unix());
        let button_strength =
            |frame: &ActivityMotion| frame.slots.each_ref().map(|slot| slot.enabled_at(now));

        assert!(activity.page(true).is_some());
        assert!(activity.page(true).is_none());
        assert!(!frame.frame(&activity, &motion, now));
        assert_eq!(button_strength(&frame), [1., 1., 1.]);

        activity.pending = false;
        let mut last_page = (*activity.page).clone();
        last_page.has_older = false;
        activity.loaded(last_page, ActivityDirection::Older(1));
        frame.frame(&activity, &motion, now);
        assert_eq!(button_strength(&frame), [1., 1., 0.]);

        assert!(activity.undo().is_none());
        assert!(activity.confirm(true).is_some());
        frame.frame(&activity, &motion, now);
        assert_eq!(button_strength(&frame), [0., 1., 0.]);
        assert!(frame.slots[0].frame(now).is_none());
        activity.loading_slow = true;
        frame.frame(&activity, &motion, now);
        assert_eq!(button_strength(&frame), [0., 0., 0.]);
    }

    #[test]
    fn a_slow_undo_keeps_the_footer_time_instead_of_saying_loading() {
        let mut activity = bulk_activity();
        let now = now_unix();
        let time = Footer::of(&activity, now).text.text;
        assert!(activity.undo().is_none());
        assert!(activity.confirm(true).is_some());
        assert!(activity.pending && !activity.loading_page);
        activity.loading_slow = true;
        assert_eq!(Footer::of(&activity, now).text.text, time);

        activity.pending = false;
        activity.loading_slow = false;
        assert!(activity.page(true).is_some());
        assert!(activity.pending && activity.loading_page);
        activity.loading_slow = true;
        assert_eq!(Footer::of(&activity, now).text.text, "Loading activity…");
    }

    #[test]
    fn confirmation_text_and_undo_slot_take_turns() {
        let mut activity = bulk_activity();
        let motion = Motion::new(true);
        let start = Instant::now();
        let mut frame = ActivityMotion::for_open(&activity, now_unix());
        for confirming in [true, false] {
            let start = if confirming {
                activity.undo();
                start
            } else {
                activity.confirm(false);
                start + Duration::from_millis(200)
            };
            for elapsed in 0..=190 {
                let now = start + Duration::from_millis(elapsed);
                frame.frame(&activity, &motion, now);
                let undo = frame.slots[0]
                    .frame(now)
                    .is_some_and(|(_, alpha)| alpha > 0.);
                let confirm = (frame.text.consequence.is_some()
                    && motion::progress(frame.text_in.as_ref(), now) > 0.)
                    || frame.old_text.as_ref().is_some_and(|old| {
                        old.content.consequence.is_some() && old.opacity(now) > 0.
                    });
                assert!(
                    !(undo && confirm),
                    "confirming={confirming}, elapsed={elapsed}"
                );
                if elapsed == 50 || elapsed == 100 {
                    let entered = elapsed == 100;
                    assert_eq!(undo, entered != confirming);
                    assert_eq!(confirm, entered == confirming);
                }
            }
        }
    }

    #[test]
    fn activity_footer_button_outlines_reach_three_and_a_half_to_one() {
        for theme in crate::theme::every_test_theme() {
            let contrast = crate::theme::contrast(footer_button_outline(&theme), theme.surface);
            assert!(contrast >= 3.5, "{}: {contrast:.3}", theme.name);
        }
    }

    #[test]
    fn activity_presentation_filters_agent_names_and_subjects() {
        let entry = entry();
        assert_eq!(
            agent_name(entry.action.actor.strip_prefix("agent:").unwrap()),
            "Fern"
        );
        assert_eq!(target_name(&entry), "Trip plan");
        let mut snooze = entry.clone();
        snooze.action.kind = "snooze".into();
        snooze.action.undone = true;
        assert_eq!(row_status(&snooze, false), Some("Undone"));
        snooze.snooze_retired = true;
        assert_eq!(
            row_status(&snooze, false),
            Some("Skipped · snooze changed since")
        );
        assert_eq!(
            target_name(&AgentActivity {
                conversations: 12,
                ..entry
            }),
            "Trip plan"
        );
    }
}
