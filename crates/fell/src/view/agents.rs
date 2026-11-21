//! The Agents settings section. The shell decides where to display the card.

use std::sync::Arc;
use std::time::{Duration, Instant};

use fell_core::AgentSummary;
use gpui::{
    Context, Div, FontWeight, InteractiveElement, MouseButton, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px, relative, rgb,
};

use crate::model::Overlay;
use crate::model::agents::{Agents, Control, Request};
use crate::motion::{self, Motion, Transition};
use crate::present::{display_safe, relative_date};
use crate::theme::Theme;

use super::{FontOpt, Inbox, now_unix};

const ROW_HEIGHT: f32 = 82.;
const TITLE_HEIGHT: f32 = 48.;
const FOOTER_HEIGHT: f32 = 70.;
const NOTICE_TIME: Duration = Duration::from_secs(3);

#[derive(Default, PartialEq, Eq)]
struct Footer {
    confirmation: Option<(String, String)>,
    notice: Option<(&'static str, bool)>,
    undo: bool,
}

impl Footer {
    fn of(agents: &Agents) -> Self {
        Self {
            confirmation: agents.confirm.as_ref().map(|row| {
                let agent = display_safe(&row.agent_id);
                let account = agents
                    .email(row)
                    .map_or_else(|| "this account".into(), display_safe);
                (
                    format!("Revoke {agent}?"),
                    format!("{account} access ends on its next request."),
                )
            }),
            notice: agents.notice,
            undo: agents.can_undo(),
        }
    }
}

#[derive(Default)]
pub(super) struct AgentMotion {
    rows: Arc<Vec<AgentSummary>>,
    switches: Option<(Arc<Vec<AgentSummary>>, Transition)>,
    selected: Option<usize>,
    selection: Option<(usize, Transition)>,
    footer: Footer,
    footer_fade: Option<(Footer, Transition)>,
}

impl AgentMotion {
    pub(super) fn observe(
        &mut self,
        agents: Option<&Agents>,
        motion: &Motion,
        now: Instant,
    ) -> bool {
        let Some(agents) = agents else {
            *self = Self::default();
            return false;
        };
        if !Arc::ptr_eq(&self.rows, &agents.rows) {
            let before = std::mem::replace(&mut self.rows, Arc::clone(&agents.rows));
            self.switches = motion.start(motion::QUICK, now).map(|fade| (before, fade));
        }
        let selected = (!agents.rows.is_empty()).then_some(agents.selected);
        if self.selected != selected {
            let previous = std::mem::replace(&mut self.selected, selected);
            self.selection = previous.zip(motion.start(motion::FADE, now));
        }
        let footer = Footer::of(agents);
        if self.footer != footer {
            let before = std::mem::replace(&mut self.footer, footer);
            self.footer_fade = motion.start(motion::FADE, now).map(|fade| (before, fade));
        }
        self.switches = self.switches.take().filter(|(_, fade)| fade.running(now));
        self.selection = self.selection.take().filter(|(_, fade)| fade.running(now));
        self.footer_fade = self
            .footer_fade
            .take()
            .filter(|(_, fade)| fade.running(now));
        self.switches.is_some() || self.selection.is_some() || self.footer_fade.is_some()
    }

    pub(super) fn settle(&mut self) {
        self.switches = None;
        self.selection = None;
    }

    fn selection_at(&self, index: usize, now: Instant) -> f32 {
        let selected = self.selected == Some(index);
        match self.selection {
            Some((_, fade)) if selected => fade.progress(now),
            Some((previous, fade)) if previous == index => fade.remaining(now),
            _ => f32::from(u8::from(selected)),
        }
    }
}

impl Inbox {
    pub(super) fn request_agent_access(&mut self, request: Request, cx: &mut Context<Self>) {
        let reading = matches!(request, Request::Load);
        let reply = self.store.call(move |store| request.run(store));
        cx.spawn(async move |this, cx| {
            let response = reply.await;
            let _ = this.update(cx, |inbox, cx| {
                match response {
                    Ok(Ok(reply)) => inbox.session.agents.answer(reply),
                    failure => {
                        if let Ok(Err(error)) = failure {
                            eprintln!("fell: agents: {error}");
                        }
                        inbox.session.agents.fail(if reading {
                            "Could not load agent access"
                        } else {
                            "Could not save agent access"
                        });
                    }
                }
                inbox
                    .agents_scroll
                    .scroll_to_item(inbox.session.agents.selected);
                if matches!(inbox.session.agents.notice, Some((_, false))) {
                    inbox.dismiss_agent_notice(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn dismiss_agent_notice(&self, cx: &mut Context<Self>) {
        let generation = self.session.agents.notice_generation;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_TIME).await;
            let _ = this.update(cx, |inbox, cx| {
                inbox.session.agents.clear_notice(generation);
                cx.notify();
            });
        })
        .detach();
    }
}

pub(super) fn card(
    theme: &Theme,
    agents: &Agents,
    scroll: Option<&ScrollHandle>,
    available_height: f32,
    frame: Option<(&AgentMotion, Instant)>,
    cx: &mut Context<Inbox>,
) -> Div {
    let live = scroll.is_some();
    let now = now_unix();
    let populated = !agents.rows.is_empty();
    let list_height = if populated {
        ROW_HEIGHT * agents.rows.len() as f32 + FOOTER_HEIGHT
    } else {
        ROW_HEIGHT
    };
    let height = TITLE_HEIGHT + list_height + 2.;
    div()
        .w(px(580.))
        .max_w_full()
        .mx(px(10.))
        .h(px(height.min(available_height.max(120.))))
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
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .font_weight(FontWeight::SEMIBOLD)
                .text_size(px(theme.ui_px + 3.))
                .child("Agents"),
        )
        .child(
            div()
                .id("agent-access-list")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .when_some(scroll, |list, scroll| list.track_scroll(scroll))
                .children(
                    agents.rows.iter().enumerate().map(|(index, row)| {
                        agent_row(theme, agents, row, index, now, live, frame, cx)
                    }),
                )
                .when(agents.rows.is_empty(), |list| {
                    list.child(
                        div()
                            .px(px(16.))
                            .py(px(17.))
                            .text_color(rgb(theme.dimmed))
                            .child(if agents.pending {
                                "Loading agent access…"
                            } else if agents.notice.is_some() {
                                "Agent access is unavailable"
                            } else {
                                "No agents connected"
                            })
                            .when(!agents.pending && agents.notice.is_none(), |hint| {
                                hint.child(
                                    div()
                                        .pt(px(6.))
                                        .text_color(rgb(theme.foreground))
                                        .font_family_opt(theme.mono_family.clone())
                                        .text_size(px(theme.mono_px.max(theme.ui_px - 1.)))
                                        .child("fell mcp add claude-code"),
                                )
                            }),
                    )
                }),
        )
        .when(populated, |card| {
            card.child(footer(theme, agents, live, frame, cx))
        })
}

#[allow(clippy::too_many_arguments)]
fn agent_row(
    theme: &Theme,
    agents: &Agents,
    row: &AgentSummary,
    index: usize,
    now: i64,
    live: bool,
    frame: Option<(&AgentMotion, Instant)>,
    cx: &mut Context<Inbox>,
) -> Div {
    let selected = agents.selected == index;
    let selection = frame.map_or(f32::from(u8::from(selected)), |(motion, now)| {
        motion.selection_at(index, now)
    });
    let bg = motion::mix(theme.surface, theme.selection, selection);
    let fg = motion::mix(theme.foreground, theme.on_selection, selection);
    let dim = motion::mix(theme.dimmed, theme.on_selection_dim, selection);
    let previous = frame.and_then(|(motion, now)| {
        let (rows, fade) = motion.switches.as_ref()?;
        let previous = rows
            .get(index)
            .filter(|old| old.agent_id == row.agent_id && old.account_id == row.account_id)?;
        Some((previous.grant, fade.progress(now)))
    });
    let last_action = row.last_action_unix.map_or_else(
        || "No actions yet".into(),
        |time| format!("Last action · {}", relative_date(now, time)),
    );
    let show_account = agents.show_accounts();
    div()
        .h(px(ROW_HEIGHT))
        .px(px(14.))
        .py(px(8.))
        .border_b(px(1.))
        .border_color(rgb(theme.border))
        .bg(rgb(bg))
        .text_color(rgb(fg))
        .flex()
        .flex_col()
        .justify_between()
        .when(live, |row| {
            row.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |inbox, _, _, cx| {
                    if inbox.session.overlay == Overlay::Agents
                        && inbox.session.agents.confirm.is_none()
                    {
                        inbox.agents_motion.settle();
                        inbox.session.agents.select(index);
                        cx.notify();
                    }
                }),
            )
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .min_w_0()
                        .when_else(
                            show_account,
                            |name| name.w(relative(0.30)).flex_none(),
                            |name| name.flex_1(),
                        )
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(display_safe(&row.agent_id)),
                )
                .when_some(
                    show_account.then(|| agents.email(row).unwrap_or("")),
                    |line, email| {
                        line.child(
                            div()
                                .min_w_0()
                                .flex_1()
                                .truncate()
                                .text_size(px(theme.ui_px - 2.))
                                .text_color(rgb(dim))
                                .child(display_safe(email)),
                        )
                    },
                )
                .child(
                    div()
                        .flex_none()
                        .when(show_account, |time| {
                            time.w(px(theme.ui_px * 7.5)).text_right().truncate()
                        })
                        .text_size(px(theme.ui_px - 2.))
                        .text_color(rgb(dim))
                        .child(last_action),
                ),
        )
        .child(div().flex().items_center().gap(px(8.)).children(
            Control::ALL.into_iter().filter_map(|control| {
                let scope = control.scope(row.grant);
                let actionable = scope.is_some() || row.grant.enabled;
                if control == Control::Revoke && !selected && actionable {
                    return None;
                }
                let focused = selected
                    && actionable
                    && control == agents.control()
                    && agents.confirm.is_none();
                let border = if focused { fg } else { bg };
                let label = if actionable {
                    control.label()
                } else {
                    "Revoked"
                };
                Some(
                    div()
                        .flex_none()
                        .h(px(27.))
                        .px(px(5.))
                        .rounded_md()
                        .border_1()
                        .border_color(rgb(border))
                        .text_color(rgb(if actionable { fg } else { dim }))
                        .text_size(px(theme.ui_px - 2.))
                        .flex()
                        .items_center()
                        .gap(px(4.))
                        .when(control == Control::Revoke, |button| {
                            button
                                .ml_auto()
                                .w(px((theme.ui_px * 4.5).max(67.)))
                                .justify_center()
                        })
                        .when(
                            live && actionable && !agents.pending && agents.confirm.is_none(),
                            |button| {
                                button.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |inbox, _, _, cx| {
                                        cx.stop_propagation();
                                        if inbox.session.overlay == Overlay::Agents {
                                            inbox.agents_motion.settle();
                                            let request =
                                                inbox.session.agents.activate(index, control);
                                            inbox.agent_button(request, cx);
                                        }
                                    }),
                                )
                            },
                        )
                        .child(label)
                        .when_some(scope, |button, on| {
                            let outline = theme.label_outline(bg);
                            let to = f32::from(u8::from(on));
                            let offset = previous
                                .and_then(|(grant, progress)| {
                                    Some((control.scope(grant)?, progress))
                                })
                                .map_or(to, |(was_on, progress)| {
                                    motion::lerp(if was_on { 1. } else { 0. }, to, progress)
                                });
                            let track = motion::mix(outline, fg, offset);
                            button.child(
                                div()
                                    .w(px(18.))
                                    .h(px(11.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(rgb(track))
                                    .bg(rgb(motion::mix(bg, fg, offset)))
                                    .p(px(1.))
                                    .flex()
                                    .items_center()
                                    .child(
                                        div()
                                            .size(px(5.))
                                            .ml(px(9. * offset))
                                            .rounded_full()
                                            .bg(rgb(motion::mix(outline, bg, offset))),
                                    ),
                            )
                        }),
                )
            }),
        ))
}

impl Inbox {
    fn agent_button(&mut self, request: Option<Request>, cx: &mut Context<Self>) {
        if let Some(request) = request {
            self.request_agent_access(request, cx);
        }
        cx.notify();
    }
}

fn footer(
    theme: &Theme,
    agents: &Agents,
    live: bool,
    frame: Option<(&AgentMotion, Instant)>,
    cx: &mut Context<Inbox>,
) -> Div {
    let transition = frame.and_then(|(motion, now)| {
        let (before, fade) = motion.footer_fade.as_ref()?;
        Some((before, motion::fade_out(fade.linear(now))))
    });
    let description = agents.control().description();
    div()
        .relative()
        .flex_none()
        .h(px(FOOTER_HEIGHT))
        .border_t(px(1.))
        .border_color(rgb(theme.border))
        .when_some(transition, |line, (before, alpha)| {
            line.child(footer_content(theme, before, description, false, cx).opacity(alpha))
        })
        .child(
            footer_content(
                theme,
                &Footer::of(agents),
                description,
                live && !agents.pending,
                cx,
            )
            .opacity(transition.map_or(1., |(_, alpha)| 1. - alpha)),
        )
}

fn footer_content(
    theme: &Theme,
    footer: &Footer,
    description: &str,
    live: bool,
    cx: &mut Context<Inbox>,
) -> Div {
    let (text, color) = match (&footer.confirmation, footer.notice) {
        (Some((prompt, _)), _) => (prompt.clone(), theme.foreground),
        (None, Some((text, true))) => (text.into(), theme.error),
        (None, Some((text, false))) => (text.into(), theme.foreground),
        (None, None) => (description.into(), theme.dimmed),
    };
    let buttons: &[(&str, Option<bool>)] = if footer.confirmation.is_some() {
        &[("Cancel", Some(false)), ("Revoke · y", Some(true))]
    } else if footer.undo {
        &[("Undo revoke", None)]
    } else {
        &[]
    };
    let line = div()
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(theme.ui_px - 2.))
                .line_height(px(theme.ui_px + 1.))
                .text_color(rgb(color))
                .child(text),
        )
        .children(buttons.iter().map(|&(label, answer)| {
            let color = if answer == Some(true) {
                theme.error
            } else {
                theme.foreground
            };
            div()
                .flex_none()
                .px(px(8.))
                .py(px(4.))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.label_outline(theme.surface)))
                .text_color(rgb(color))
                .text_size(px(theme.ui_px - 1.))
                .child(label.to_owned())
                .when(live, |button| {
                    button.cursor_pointer().on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |inbox, _, _, cx| {
                            cx.stop_propagation();
                            if inbox.session.overlay == Overlay::Agents {
                                let request = match answer {
                                    Some(yes) => inbox.session.agents.confirm_revoke(yes),
                                    None => inbox.session.agents.undo(),
                                };
                                inbox.agent_button(request, cx);
                            }
                        }),
                    )
                })
        }));
    div()
        .absolute()
        .inset_0()
        .px(px(14.))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(2.))
        .child(line)
        .when_some(footer.confirmation.as_ref(), |content, (_, consequence)| {
            content.child(
                div()
                    .text_size(px(theme.ui_px - 2.))
                    .line_height(px(theme.ui_px + 1.))
                    .text_color(rgb(theme.foreground))
                    .child(consequence.clone()),
            )
        })
}
