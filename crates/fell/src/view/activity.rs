//! The paged agent activity card and its exact-action undo controls.

use std::time::{Duration, Instant};

use fell_core::agent::{
    ActivityBlocker, ActivityGrant, AgentActivity, BLOCKER_MAILBOX_CHANGED, SNOOZE_CHANGED_NOTICE,
};
use gpui::{
    Context, Div, FontWeight, InteractiveElement, MouseButton, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, div, point, prelude::FluentBuilder, px, rgb,
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
const FOOTER_HEIGHT: f32 = 64.;
const LOADING_DELAY: Duration = Duration::from_millis(150);

pub(super) fn card_height(available_height: f32) -> f32 {
    (TITLE_HEIGHT + 7. * ROW_HEIGHT + FOOTER_HEIGHT + 2.).min(available_height.max(158.))
}

struct PageFade {
    activity: Activity,
    offset: f32,
    fade: Transition,
}

#[derive(PartialEq, Eq)]
struct Footer {
    text: String,
    consequence: Option<&'static str>,
    confirm: bool,
    undo: bool,
    newer: bool,
    older: bool,
    pending: bool,
}

struct FooterFade {
    before: Footer,
    fade: Transition,
}

#[derive(Default)]
pub(super) struct ActivityMotion {
    revision: u64,
    page: Option<PageFade>,
    arrival: Option<Transition>,
    selection: Option<i64>,
    selection_fade: Option<(Option<i64>, Transition)>,
    footer: Option<Footer>,
    footer_fade: Option<FooterFade>,
    undone: Option<(i64, Transition)>,
}

impl ActivityMotion {
    pub(super) fn settle(&mut self) {
        self.arrival = None;
        self.selection_fade = None;
    }

    pub(super) fn frame(&mut self, activity: &Activity, motion: &Motion, now: Instant) -> bool {
        if self.revision != activity.page_revision {
            self.revision = activity.page_revision;
            self.arrival = motion.start(motion::QUICK, now);
        }
        let selected = activity.selected_entry().map(|entry| entry.action.id);
        if self.selection != selected {
            let before = std::mem::replace(&mut self.selection, selected);
            self.selection_fade = motion.start(motion::FADE, now).map(|fade| (before, fade));
        }
        let footer = Footer::of(activity, now_unix());
        if self.footer.as_ref() != Some(&footer) {
            let before = self.footer.replace(footer);
            self.footer_fade = before
                .zip(motion.start(motion::FADE, now))
                .map(|(before, fade)| FooterFade { before, fade });
        }
        self.footer_fade = self
            .footer_fade
            .take()
            .filter(|footer| footer.fade.running(now));
        self.page = self.page.take().filter(|page| page.fade.running(now));
        self.page.is_some()
            || self.arrival.is_some_and(|fade| fade.running(now))
            || self.footer_fade.is_some()
            || self
                .selection_fade
                .is_some_and(|(_, fade)| fade.running(now))
            || self.undone.is_some_and(|(_, fade)| fade.running(now))
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
        match request {
            Request::Load {
                direction,
                ref filter,
                ..
            } => {
                let account_wide = filter.is_none();
                let account = self.account_id.clone();
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
                                if before.page_revision != activity.page_revision {
                                    let now = inbox.motion.clock.now();
                                    inbox.activity_motion.page = (!before.page.entries.is_empty())
                                        .then(|| inbox.motion.motion.start(motion::QUICK, now))
                                        .flatten()
                                        .map(|fade| PageFade {
                                            activity: before,
                                            offset: inbox.activity_scroll.offset().y.into(),
                                            fade,
                                        });
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
                        }
                        let (notice, error, changed) = match result {
                            Ok(action) => {
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
                                let notice = if retired {
                                    "Snooze changed since — left as is"
                                } else {
                                    "Agent action undone"
                                };
                                (notice.into(), false, viewing_account)
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
                            Err(fell_core::Error::AgentActivityAlreadyUndone {
                                snooze_retired,
                            }) => {
                                activity.mark_undone(id, snooze_retired);
                                return None;
                            }
                            Err(fell_core::Error::AgentActivityGrant {
                                enabled, scope, ..
                            }) => {
                                activity.mark_grant_missing(id, ActivityGrant { enabled, scope });
                                return None;
                            }
                            Err(
                                fell_core::Error::Action(message)
                                | fell_core::Error::Invalid(message),
                            ) => (display_safe(&message), true, false),
                            Err(_) => ("Could not undo agent action".into(), true, false),
                        };
                        inbox.session.show_toast(notice, error);
                        inbox.expire_action_notice(cx);
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
    height: f32,
    compact: bool,
    frame: Option<(&ActivityMotion, Instant)>,
    cx: &mut Context<Inbox>,
) -> Div {
    let live = scroll.is_some();
    let now = now_unix();
    let dim = ensure_all(theme.dimmed, &[theme.surface], 4.65);
    let leaving = frame.and_then(|(motion, time)| Some((motion.page.as_ref()?, time)));
    let arrival = frame.map_or(1., |(motion, time)| {
        motion::progress(motion.arrival.as_ref(), time)
    });
    let footer = Footer::of(activity, now);
    let footer_fade = frame.and_then(|(motion, time)| Some((motion.footer_fade.as_ref()?, time)));
    let rows = div()
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
                    .top(px(old.offset))
                    .opacity(old.fade.remaining(time))
                    .children(
                        (0..old.activity.page.entries.len())
                            .map(|index| row(theme, &old.activity, index, now, compact, None)),
                    ),
            )
        })
        .child(
            div()
                .id("agent-activity-list")
                .relative()
                .top(px(motion::RISE_PX * (1. - arrival)))
                .size_full()
                .overflow_y_scroll()
                .opacity(motion::lerp(motion::ARRIVE_FROM, 1., arrival))
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
                .when_some(footer_fade, |bottom, (old, time)| {
                    bottom.child(
                        old.before
                            .render(theme, false, cx)
                            .absolute()
                            .inset_0()
                            .opacity(old.fade.remaining(time)),
                    )
                })
                .child(
                    footer
                        .render(theme, live, cx)
                        .opacity(footer_fade.map_or(1., |(old, time)| old.fade.progress(time))),
                ),
        )
}

impl Footer {
    fn of(activity: &Activity, now: i64) -> Self {
        let mut footer = Self {
            text: String::new(),
            consequence: None,
            confirm: false,
            undo: false,
            pending: activity.pending,
            newer: activity.page.has_newer,
            older: activity.page.has_older,
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
            footer.text = format!(
                "{effect} {} conversations from {}?",
                prompt.conversations,
                actor_name(&entry.action.actor)
            );
            footer.consequence = Some(consequence);
            footer.confirm = true;
            return footer;
        }
        let entry = activity.selected_entry();
        footer.undo = !activity.pending
            && entry.is_some_and(|entry| {
                !entry.action.undone
                    && !entry.draft_discarded
                    && entry.blocker.is_none()
                    && entry.missing_grant.is_none()
            });
        footer.text = if let Some(blocker) = &activity.blocker_notice {
            blocker_notice(blocker, now)
        } else if activity.load_failed && !activity.page.entries.is_empty() {
            "Could not load agent activity".into()
        } else if activity.pending && activity.loading_slow && !activity.page.entries.is_empty() {
            "Loading activity…".into()
        } else if let Some((entry, grant)) = entry.and_then(|e| Some((e, e.missing_grant?))) {
            let name = actor_name(&entry.action.actor);
            let scope = grant.scope;
            if grant.enabled {
                format!("{scope} is off for {name}. Turn on {scope} to undo.")
            } else {
                format!("{name} is turned off. Turn on {scope} to undo.")
            }
        } else {
            entry
                .filter(|e| e.action.created_unix > 0)
                .map_or_else(String::new, |entry| {
                    activity_time(now, entry.action.created_unix)
                })
        };
        footer
    }

    fn render(&self, theme: &Theme, live: bool, cx: &mut Context<Inbox>) -> Div {
        let dim = ensure_all(theme.dimmed, &[theme.surface], 4.65);
        let mut controls = div().flex_none().flex().items_center().gap(px(6.));
        if self.confirm {
            controls = controls
                .child(button(theme, "Cancel", KeyId::Escape, live, cx))
                .child(button(theme, "Undo · y", KeyId::Char('y'), live, cx));
        } else {
            controls = controls
                .when(self.newer, |line| {
                    line.child(button(
                        theme,
                        "Newer",
                        KeyId::Ctrl('u'),
                        live && !self.pending,
                        cx,
                    ))
                })
                .when(self.older, |line| {
                    line.child(button(
                        theme,
                        "Older",
                        KeyId::Ctrl('d'),
                        live && !self.pending,
                        cx,
                    ))
                })
                .when(self.undo, |line| {
                    line.child(button(theme, "Undo action", KeyId::Char('u'), live, cx))
                });
        }
        div()
            .size_full()
            .px(px(14.))
            .pb(px(10.))
            .flex()
            .items_end()
            .gap(px(8.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(
                        div()
                            .line_clamp(2)
                            .text_ellipsis()
                            .when_else(
                                self.confirm,
                                |text| text.font_weight(FontWeight::SEMIBOLD),
                                |text| text.text_color(rgb(dim)),
                            )
                            .child(self.text.clone()),
                    )
                    .when_some(self.consequence, |text, consequence| {
                        text.child(
                            div()
                                .line_clamp(1)
                                .text_ellipsis()
                                .text_color(rgb(dim))
                                .child(consequence),
                        )
                    }),
            )
            .child(controls)
    }
}

fn blocker_notice(blocker: &ActivityBlocker, now: i64) -> String {
    let kind = match blocker.kind.as_str() {
        "star" => "star",
        "unstar" => "unstar",
        "archive" => "archive",
        "move" => "move",
        "draft" => "draft",
        "mark_read" | "mark_unread" => "read change",
        "snooze" | "unsnooze" => "snooze change",
        "label" | "unlabel" => "label change",
        "annotate" => "note",
        _ => "action",
    };
    match blocker.actor.as_str() {
        "local" => format!("Undo your newer {kind} in the mailbox first"),
        "outbox" => "A newer move is waiting to sync".into(),
        _ if blocker.kind == BLOCKER_MAILBOX_CHANGED => {
            "Mail folders changed since this action".into()
        }
        actor if actor.starts_with("agent:") => format!(
            "Undo {}’s newer {kind} first · {}",
            actor_name(actor),
            relative_date(now, blocker.created_unix)
        ),
        _ => format!("Undo the newer {kind} in the mailbox first"),
    }
}

fn button(
    theme: &Theme,
    label: &'static str,
    key: KeyId,
    live: bool,
    cx: &mut Context<Inbox>,
) -> Div {
    div()
        .px(px(8.))
        .py(px(3.))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.label_outline(theme.surface)))
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
    } else if let Some(grant) = entry.missing_grant {
        Some(if !grant.enabled {
            "Agent off"
        } else {
            match grant.scope {
                "Draft" => "Draft off",
                "Annotate" => "Annotate off",
                _ => "Triage off",
            }
        })
    } else if let Some(blocker) = &entry.blocker {
        Some(if blocker.kind == BLOCKER_MAILBOX_CHANGED {
            "Folders changed"
        } else if compact {
            "Blocked"
        } else {
            "Undo newer first"
        })
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
    use fell_core::{AccountId, RecordedAction};

    #[test]
    fn activity_presentation_filters_agent_names_and_subjects() {
        let entry = AgentActivity {
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
            missing_grant: None,
        };
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
