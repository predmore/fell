//! The paged agent activity card and its exact-action undo controls.

use std::time::Instant;

use fell_core::agent::{AgentActivity, SNOOZE_CHANGED_NOTICE};
use gpui::{
    Context, Div, FontWeight, InteractiveElement, MouseButton, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px, rgb,
};

use super::{Inbox, ReadIntent, now_unix};
use crate::keymap::KeyId;
use crate::model::{
    Overlay,
    activity::{Activity, Request},
};
use crate::motion::{self, ListShift, Motion, Transition};
use crate::present::{display_safe, relative_date};
use crate::theme::{Theme, ensure_all};

const ROW_HEIGHT: f32 = 54.;
const TITLE_HEIGHT: f32 = 48.;
const FOOTER_HEIGHT: f32 = 72.;

#[derive(Default)]
pub(super) struct ActivityMotion {
    revision: u64,
    confirmation: Option<i64>,
    rows: Option<ListShift<()>>,
    footer: Option<Transition>,
}

impl ActivityMotion {
    pub(super) fn settle(&mut self) {
        self.rows = None;
        self.footer = None;
    }

    pub(super) fn frame(&mut self, activity: &Activity, motion: &Motion, now: Instant) -> bool {
        if self.revision != activity.page_revision {
            self.revision = activity.page_revision;
            self.rows = ListShift::start(motion, vec![], vec![], vec![], 0., true, now);
        }
        let confirmation = activity.confirmation.as_ref().map(|prompt| prompt.id);
        if self.confirmation != confirmation {
            self.confirmation = confirmation;
            self.footer = motion.start(motion::FADE, now);
        }
        self.rows.as_ref().is_some_and(|rows| rows.running(now))
            || self.footer.is_some_and(|fade| fade.running(now))
    }

    fn row(&mut self, entry: &AgentActivity, now: Instant) -> f32 {
        self.rows.as_mut().map_or(1., |rows| {
            rows.row(&entry.action.id.to_string(), 0., ROW_HEIGHT, [], now)
                .opacity
        })
    }
}

impl Inbox {
    pub(super) fn request_agent_activity(&mut self, request: Request, cx: &mut Context<Self>) {
        let generation = self.session.activity.generation;
        match request {
            Request::Load { direction, .. } => {
                let account = self.account_id.clone();
                let reply = self.store.call(move |store| request.load(store, &account));
                cx.spawn(async move |this, cx| {
                    let reply = reply.await;
                    let _ = this.update(cx, |inbox, cx| {
                        let activity = &mut inbox.session.activity;
                        if activity.generation != generation {
                            return;
                        }
                        activity.pending = false;
                        match reply {
                            Ok(Ok(page)) => {
                                activity.loaded(page, direction);
                                inbox.activity_scroll.scroll_to_item(activity.selected);
                            }
                            _ => {
                                activity.load_failed = true;
                                inbox
                                    .session
                                    .show_toast("Could not load agent activity", true);
                                inbox.expire_action_notice(cx);
                            }
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
                        if activity.generation == generation {
                            activity.pending = false;
                        }
                        let (notice, error, changed) = match result {
                            Ok(action) => {
                                activity.mark_undone(action.id);
                                if viewing_account {
                                    inbox.note_generation = inbox.note_generation.wrapping_add(1);
                                    inbox.send_to_server();
                                }
                                let notice = if action.summary == SNOOZE_CHANGED_NOTICE {
                                    SNOOZE_CHANGED_NOTICE
                                } else {
                                    "Agent action undone"
                                };
                                (notice.into(), false, viewing_account)
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
    available_height: f32,
    mut motion: Option<(&mut ActivityMotion, Instant)>,
    cx: &mut Context<Inbox>,
) -> Div {
    let live = scroll.is_some();
    let now = now_unix();
    let dim = ensure_all(theme.dimmed, &[theme.surface], 4.65);
    let footer_alpha = motion.as_ref().map_or(1., |(motion, now)| {
        motion::lerp(
            motion::ARRIVE_FROM,
            1.,
            motion::progress(motion.footer.as_ref(), *now),
        )
    });
    let selected = activity.page.entries.get(activity.selected);
    let rows = div()
        .id("agent-activity-list")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .when_some(scroll, |list, scroll| list.track_scroll(scroll))
        .children(
            activity
                .page
                .entries
                .iter()
                .enumerate()
                .map(|(index, entry)| {
                    let opacity = motion
                        .as_mut()
                        .map_or(1., |(motion, now)| motion.row(entry, *now));
                    row(theme, entry, index == activity.selected, now)
                        .opacity(opacity)
                        .when(live, |row| {
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
                }),
        )
        .when(activity.page.entries.is_empty(), |list| {
            let text = if activity.pending {
                "Loading activity…"
            } else if activity.load_failed {
                "Activity is unavailable"
            } else {
                "No agent activity yet"
            };
            list.child(
                div()
                    .px(px(16.))
                    .py(px(16.))
                    .text_color(rgb(dim))
                    .child(text),
            )
        });
    let bottom = div()
        .flex_none()
        .h(px(FOOTER_HEIGHT))
        .border_t(px(1.))
        .border_color(rgb(theme.border))
        .px(px(12.))
        .flex()
        .flex_col()
        .justify_center();
    let footer = if let Some(prompt) = &activity.confirmation {
        bottom.child(
            div()
                .flex()
                .flex_col()
                .gap(px(4.))
                .opacity(footer_alpha)
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                    "Undo this action for {} conversations?",
                    prompt.conversations
                )))
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(6.))
                        .child(button(theme, "Cancel", KeyId::Escape, live, cx))
                        .child(button(theme, "Undo · y", KeyId::Char('y'), live, cx)),
                ),
        )
    } else {
        bottom.child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .opacity(footer_alpha)
                .when(activity.page.has_newer, |line| {
                    line.child(button(
                        theme,
                        "Newer",
                        KeyId::Ctrl('u'),
                        live && !activity.pending,
                        cx,
                    ))
                })
                .when(activity.page.has_older, |line| {
                    line.child(button(
                        theme,
                        "Older",
                        KeyId::Ctrl('d'),
                        live && !activity.pending,
                        cx,
                    ))
                })
                .child(div().flex_1())
                .when(selected.is_some_and(|entry| !entry.action.undone), |line| {
                    line.child(button(
                        theme,
                        "Undo action",
                        KeyId::Char('u'),
                        live && !activity.pending,
                        cx,
                    ))
                }),
        )
    };
    div()
        .w(px(560.))
        .max_w_full()
        .mx(px(10.))
        .h(px(
            (TITLE_HEIGHT + 7. * ROW_HEIGHT + FOOTER_HEIGHT).min(available_height.max(158.))
        ))
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
                            .truncate()
                            .text_color(rgb(dim))
                            .child(agent_name(&agent.agent_id)),
                    )
                }),
        )
        .child(rows)
        .child(footer)
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

fn row(theme: &Theme, entry: &AgentActivity, selected: bool, now: i64) -> Div {
    let (bg, fg, muted) = if selected {
        (theme.selection, theme.on_selection, theme.on_selection_dim)
    } else {
        (theme.surface, theme.foreground, theme.dimmed)
    };
    let dim = ensure_all(muted, &[bg], 4.65);
    let name = agent_name(
        entry
            .action
            .actor
            .strip_prefix("agent:")
            .unwrap_or_default(),
    );
    let target = target_name(entry);
    div()
        .h(px(ROW_HEIGHT))
        .px(px(16.))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(2.))
        .border_b(px(1.))
        .border_color(rgb(theme.border))
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .flex_none()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(action_name(&entry.action.kind)),
                )
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_color(rgb(dim))
                        .child(name),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(rgb(dim))
                        .child(relative_date(now, entry.action.created_unix)),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(div().min_w_0().flex_1().truncate().child(target))
                .when(entry.action.undone, |line| {
                    line.child(div().flex_none().text_color(rgb(dim)).child("Undone"))
                }),
        )
}

fn agent_name(name: &str) -> String {
    let name = display_safe(name);
    if name.trim().is_empty() {
        "Agent".into()
    } else {
        name
    }
}

fn target_name(entry: &AgentActivity) -> String {
    if entry.conversations > 1 {
        return format!("{} conversations", entry.conversations);
    }
    match entry.subject.as_deref().map(display_safe) {
        Some(subject) if !subject.trim().is_empty() => subject,
        Some(_) => "(No subject)".into(),
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
        };
        assert_eq!(
            agent_name(entry.action.actor.strip_prefix("agent:").unwrap()),
            "Fern"
        );
        assert_eq!(target_name(&entry), "Trip plan");
        assert_eq!(
            target_name(&AgentActivity {
                conversations: 12,
                ..entry
            }),
            "12 conversations"
        );
    }
}
