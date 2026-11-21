//! The Agents settings section. The shell decides where to display the card.

use std::sync::Arc;
use std::time::Instant;

use fell_core::{AgentGrant, AgentSummary};
use gpui::{
    Context, Div, FontWeight, InteractiveElement, MouseButton, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, div, prelude::FluentBuilder, px, rgb,
};

use crate::model::Overlay;
use crate::model::agents::{Agents, Control, Request};
use crate::motion::{self, Motion, Transition};
use crate::present::{display_safe, relative_date};
use crate::theme::Theme;

use super::{Inbox, now_unix};

#[derive(Default, PartialEq, Eq)]
struct Footer {
    question: Option<String>,
    notice: Option<(&'static str, bool)>,
    undo: bool,
}

impl Footer {
    fn of(agents: &Agents) -> Self {
        Self {
            question: agents.confirm.as_ref().map(|row| {
                format!(
                    "Revoke {} on {}?",
                    display_safe(&row.agent_id),
                    display_safe(row.account_id.as_str())
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
        let footer = Footer::of(agents);
        if self.footer != footer {
            let before = std::mem::replace(&mut self.footer, footer);
            self.footer_fade = motion.start(motion::FADE, now).map(|fade| (before, fade));
        }
        self.switches = self
            .switches
            .take()
            .filter(|(_, fade)| fade.linear(now) < 1.);
        self.footer_fade = self
            .footer_fade
            .take()
            .filter(|(_, fade)| fade.linear(now) < 1.);
        self.switches.is_some() || self.footer_fade.is_some()
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
    div()
        .w(px(580.))
        .max_w_full()
        .mx(px(10.))
        .h(px(344_f32.min(available_height.max(120.))))
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
                .h(px(48.))
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
                .children(agents.rows.iter().enumerate().map(|(index, row)| {
                    let previous = frame.and_then(|(motion, now)| {
                        let (rows, fade) = motion.switches.as_ref()?;
                        let previous = rows.get(index).filter(|old| {
                            old.agent_id == row.agent_id && old.account_id == row.account_id
                        })?;
                        Some((previous.grant, motion::ease_out_quad(fade.linear(now))))
                    });
                    agent_row(theme, agents, row, index, now, live, previous, cx)
                }))
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
                                hint.child(div().pt(px(6.)).child("Connect one with fell mcp add."))
                            }),
                    )
                }),
        )
        .child(footer(theme, agents, live, frame, cx))
}

#[allow(clippy::too_many_arguments)]
fn agent_row(
    theme: &Theme,
    agents: &Agents,
    row: &AgentSummary,
    index: usize,
    now: i64,
    live: bool,
    previous: Option<(AgentGrant, f32)>,
    cx: &mut Context<Inbox>,
) -> Div {
    let selected = agents.selected == index;
    let bg = if selected {
        theme.selection
    } else {
        theme.surface
    };
    let fg = if selected {
        theme.on_selection
    } else {
        theme.foreground
    };
    let dim = if selected {
        theme.on_selection_dim
    } else {
        theme.dimmed
    };
    let last_action = row.last_action_unix.map_or_else(
        || "No recorded actions".into(),
        |time| format!("Last action · {}", relative_date(now, time)),
    );
    div()
        .h(px(82.))
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
                        inbox.session.agents.selected = index;
                        cx.notify();
                    }
                }),
            )
        })
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(display_safe(&row.agent_id)),
                )
                .child(
                    div()
                        .flex_none()
                        .text_size(px(theme.ui_px - 2.))
                        .text_color(rgb(dim))
                        .child(last_action),
                ),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .child(
                    div()
                        .min_w_0()
                        .flex_1()
                        .truncate()
                        .text_size(px(theme.ui_px - 2.))
                        .text_color(rgb(dim))
                        .child(display_safe(row.account_id.as_str())),
                )
                .children(Control::ALL.into_iter().map(|control| {
                    let scope = control.scope(row.grant);
                    let actionable = scope.is_some() || row.grant.enabled;
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
                            button.w(px((theme.ui_px * 4.5).max(67.))).justify_center()
                        })
                        .when(
                            live && actionable && !agents.pending && agents.confirm.is_none(),
                            |button| {
                                button.cursor_pointer().on_mouse_down(
                                    MouseButton::Left,
                                    cx.listener(move |inbox, _, _, cx| {
                                        cx.stop_propagation();
                                        if inbox.session.overlay == Overlay::Agents {
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
                            let track = if on { fg } else { theme.label_outline(bg) };
                            let to = if on { 1. } else { 0. };
                            let offset = previous
                                .and_then(|(grant, progress)| {
                                    Some((control.scope(grant)?, progress))
                                })
                                .map_or(to, |(was_on, progress)| {
                                    motion::lerp(if was_on { 1. } else { 0. }, to, progress)
                                });
                            button.child(
                                div()
                                    .w(px(18.))
                                    .h(px(11.))
                                    .rounded_full()
                                    .border_1()
                                    .border_color(rgb(track))
                                    .bg(rgb(if on { track } else { bg }))
                                    .p(px(1.))
                                    .flex()
                                    .items_center()
                                    .child(
                                        div()
                                            .size(px(5.))
                                            .ml(px(9. * offset))
                                            .rounded_full()
                                            .bg(rgb(if on { bg } else { track })),
                                    ),
                            )
                        })
                })),
        )
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
    div()
        .relative()
        .flex_none()
        .h(px(48.))
        .border_t(px(1.))
        .border_color(rgb(theme.border))
        .when_some(transition, |line, (before, alpha)| {
            line.child(footer_content(theme, before, false, cx).opacity(alpha))
        })
        .child(
            footer_content(theme, &Footer::of(agents), live && !agents.pending, cx)
                .opacity(transition.map_or(1., |(_, alpha)| 1. - alpha)),
        )
}

fn footer_content(theme: &Theme, footer: &Footer, live: bool, cx: &mut Context<Inbox>) -> Div {
    let message = footer
        .question
        .as_ref()
        .map(|question| (question.clone(), true))
        .or_else(|| footer.notice.map(|(text, alert)| (text.into(), alert)));
    let buttons: &[(&str, Option<bool>)] = if footer.question.is_some() {
        &[("Cancel", Some(false)), ("Revoke · y", Some(true))]
    } else if footer.undo {
        &[("Undo revoke · u", None)]
    } else {
        &[]
    };
    div()
        .absolute()
        .inset_0()
        .px(px(14.))
        .flex()
        .items_center()
        .gap(px(8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .when_some(message, |label, (text, alert)| {
                    label
                        .text_color(rgb(if alert { theme.error } else { theme.dimmed }))
                        .child(text)
                }),
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
        }))
}
