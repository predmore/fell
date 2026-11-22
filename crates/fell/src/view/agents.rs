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
use crate::theme::{Theme, ensure_all};

use super::{FontOpt, Inbox, now_unix};

const ROW_HEIGHT: f32 = 82.;
const TITLE_HEIGHT: f32 = 48.;
const FOOTER_HEIGHT: f32 = 92.;
const EMPTY_HEIGHT: f32 = 102.;
const NOTICE_TIME: Duration = Duration::from_secs(3);
const SWITCH_CONTRAST: f64 = 3.15;
const ROW_TEXT_CONTRAST: f64 = 4.65;

#[derive(PartialEq, Eq)]
struct Confirmation {
    prompt: String,
    consequence: String,
    action: &'static str,
}

#[derive(PartialEq, Eq)]
enum Footer {
    Confirm(Confirmation),
    Message {
        text: &'static str,
        notice: Option<bool>,
        undo: bool,
    },
}

impl Footer {
    fn of(agents: &Agents) -> Self {
        if let Some(confirm) = &agents.confirm {
            let agent = display_safe(&confirm.row.agent_id);
            let account = agents
                .email(&confirm.row)
                .map_or_else(|| "this account".into(), display_safe);
            let consequence = format!("{agent} loses access to {account} on its next request.");
            return Self::Confirm(if confirm.control == Control::Revoke {
                Confirmation {
                    prompt: format!("Revoke {agent}?"),
                    consequence,
                    action: "Revoke · y",
                }
            } else {
                Confirmation {
                    prompt: format!("Turn off {} for {agent}?", confirm.control.label()),
                    consequence: format!("Last permission: {consequence}"),
                    action: "Turn off · y",
                }
            });
        }
        let (text, notice) = if let Some((text, error)) = agents.notice {
            (text, Some(error))
        } else if agents
            .rows
            .get(agents.selected)
            .is_some_and(|row| !row.grant.enabled)
        {
            ("No access", None)
        } else {
            (agents.control().description(), None)
        };
        Self::Message {
            text,
            notice,
            undo: agents.can_undo(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ControlFocus {
    row: usize,
    control: Control,
    confirming: bool,
}

impl ControlFocus {
    fn of(agents: &Agents) -> Self {
        Self {
            row: agents.selected,
            control: agents.control(),
            confirming: agents.confirm.is_some(),
        }
    }

    fn border(self, row: usize, control: Control, bg: u32, fg: u32, dim: u32) -> u32 {
        if self.row != row || self.control != control {
            bg
        } else if self.confirming {
            dim
        } else {
            fg
        }
    }
}

#[derive(Default)]
pub(super) struct AgentMotion {
    rows: Arc<Vec<AgentSummary>>,
    switches: Option<(Arc<Vec<AgentSummary>>, Transition)>,
    selected: Option<usize>,
    selection: Option<(usize, Transition)>,
    focused: Option<ControlFocus>,
    focus_fade: Option<(ControlFocus, Transition)>,
    footer: Option<Footer>,
    footer_fade: Option<FooterFade>,
}

struct FooterFade {
    before: Footer,
    from: f32,
    fade: Transition,
}

impl FooterFade {
    fn opacities(&self, now: Instant) -> (f32, f32) {
        footer_opacities(self.fade.linear(now), self.from)
    }
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
        let focused = selected.map(|_| ControlFocus::of(agents));
        if self.focused != focused {
            let previous = std::mem::replace(&mut self.focused, focused);
            self.focus_fade = previous.zip(motion.start(motion::FADE, now));
        }
        let footer = selected.map(|_| Footer::of(agents));
        if self.footer != footer {
            let before = std::mem::replace(&mut self.footer, footer);
            let prior = self.footer_fade.take();
            self.footer_fade = before
                .zip(motion.start(motion::FADE, now))
                .map(|(before, fade)| {
                    // A rapid change starts from the line already on screen.
                    let (before, from) = if let Some(prior) = prior {
                        let (old, new) = prior.opacities(now);
                        if old > 0. {
                            (prior.before, old)
                        } else {
                            (before, new)
                        }
                    } else {
                        (before, 1.)
                    };
                    FooterFade { before, from, fade }
                });
        }
        self.switches = self.switches.take().filter(|(_, fade)| fade.running(now));
        self.selection = self.selection.take().filter(|(_, fade)| fade.running(now));
        self.focus_fade = self.focus_fade.take().filter(|(_, fade)| fade.running(now));
        self.footer_fade = self
            .footer_fade
            .take()
            .filter(|fade| fade.fade.running(now));
        self.switches.is_some()
            || self.selection.is_some()
            || self.focus_fade.is_some()
            || self.footer_fade.is_some()
    }

    pub(super) fn settle(&mut self) {
        self.switches = None;
        self.selection = None;
        self.focus_fade = None;
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
        EMPTY_HEIGHT
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
                .children(agents.rows.iter().enumerate().map(|(index, row)| {
                    agent_row(
                        theme,
                        agents,
                        row,
                        index,
                        now,
                        frame,
                        live.then_some(&mut *cx),
                    )
                }))
                .when(agents.rows.is_empty(), |list| {
                    list.child(
                        div()
                            .px(px(16.))
                            .py(px(17.))
                            .text_color(rgb(row_dimmed(theme, theme.surface, 0.)))
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
                                .child(
                                    div()
                                        .pt(px(4.))
                                        .text_size(px(theme.ui_px - 2.))
                                        .child("Also supports codex, gemini, opencode, and goose."),
                                )
                            }),
                    )
                }),
        )
        .when(populated, |card| {
            card.child(footer(theme, agents, live, frame, cx))
        })
}

fn row_dimmed(theme: &Theme, bg: u32, selection: f32) -> u32 {
    let dim = motion::mix(theme.dimmed, theme.on_selection_dim, selection);
    ensure_all(dim, &[bg], ROW_TEXT_CONTRAST)
}

fn switch_outline(theme: &Theme, bg: u32) -> u32 {
    ensure_all(theme.label_outline(bg), &[bg], SWITCH_CONTRAST)
}

fn agent_row(
    theme: &Theme,
    agents: &Agents,
    row: &AgentSummary,
    index: usize,
    now: i64,
    frame: Option<(&AgentMotion, Instant)>,
    mut cx: Option<&mut Context<Inbox>>,
) -> Div {
    let selected = agents.selected == index;
    let selection = frame.map_or(f32::from(u8::from(selected)), |(motion, now)| {
        motion.selection_at(index, now)
    });
    let bg = motion::mix(theme.surface, theme.selection, selection);
    let fg = motion::mix(theme.foreground, theme.on_selection, selection);
    let dim = row_dimmed(theme, bg, selection);
    let outline = switch_outline(theme, bg);
    let focused = ControlFocus::of(agents);
    let focus_fade = frame.and_then(|(motion, now)| {
        let (before, fade) = motion.focus_fade?;
        Some((before, fade.progress(now)))
    });
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
        .when_some(cx.as_deref_mut(), |row, cx| {
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
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(8.))
                .children(Control::ALL.into_iter().map(|control| {
                    let scope = control.scope(row.grant);
                    let actionable = scope.is_some() || row.grant.enabled;
                    let border = focused.border(index, control, bg, fg, theme.dimmed);
                    let border = focus_fade.map_or(border, |(before, progress)| {
                        let before = before.border(index, control, bg, fg, theme.dimmed);
                        motion::mix(before, border, progress)
                    });
                    let label = if actionable {
                        control.label()
                    } else {
                        "Revoked"
                    };
                    let clickable = actionable
                        && (scope.is_some() || selected)
                        && !agents.pending
                        && agents.confirm.is_none();
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
                                .opacity(selection)
                        })
                        .when_some(cx.as_deref_mut().filter(|_| clickable), |button, cx| {
                            button.cursor_pointer().on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |inbox, _, _, cx| {
                                    cx.stop_propagation();
                                    if inbox.session.overlay == Overlay::Agents {
                                        inbox.agents_motion.settle();
                                        let request = inbox.session.agents.activate(index, control);
                                        inbox.agent_button(request, cx);
                                    }
                                }),
                            )
                        })
                        .child(label)
                        .when_some(scope, |button, on| {
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
                                    .debug_selector(|| format!("agent-{index}-{}", control.label()))
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
    let current = Footer::of(agents);
    let (line, opacity, live) = match frame.and_then(|(motion, now)| {
        motion
            .footer_fade
            .as_ref()
            .map(|fade| (fade, fade.opacities(now)))
    }) {
        Some((fade, (old, _))) if old > 0. => (&fade.before, old, false),
        Some((_, (_, new))) => (&current, new, live),
        None => (&current, 1., live),
    };
    div()
        .relative()
        .flex_none()
        .h(px(FOOTER_HEIGHT))
        .border_t(px(1.))
        .border_color(rgb(theme.border))
        .child(footer_content(theme, line, live && !agents.pending, cx).opacity(opacity))
}

fn footer_opacities(time: f32, from: f32) -> (f32, f32) {
    // Swap at the same opacity so even a frame just before the swap has text.
    if time < 0.5 {
        (
            motion::lerp(motion::ARRIVE_FROM, from, motion::fade_out(2. * time)),
            0.,
        )
    } else {
        (
            0.,
            motion::lerp(
                motion::ARRIVE_FROM,
                1.,
                motion::ease_out_quad(2. * time - 1.),
            ),
        )
    }
}

fn footer_content(theme: &Theme, footer: &Footer, live: bool, cx: &mut Context<Inbox>) -> Div {
    let (text, color, confirm, undo) = match footer {
        Footer::Confirm(confirm) => (&*confirm.prompt, theme.foreground, Some(confirm), false),
        Footer::Message { text, notice, undo } => {
            let color = match notice {
                Some(true) => theme.error,
                Some(false) => theme.foreground,
                None => row_dimmed(theme, theme.surface, 0.),
            };
            (*text, color, None, *undo)
        }
    };
    let buttons: &[(&str, Option<bool>)] = if let Some(confirm) = confirm {
        &[("Cancel", Some(false)), (confirm.action, Some(true))]
    } else if undo {
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
                .child(text.to_owned()),
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
        .py(px(10.))
        .flex()
        .flex_col()
        .justify_center()
        .gap(px(2.))
        .child(line)
        .when_some(confirm, |content, confirm| {
            content.child(
                div()
                    .text_size(px(theme.ui_px - 2.))
                    .line_height(px(theme.ui_px + 1.))
                    .text_color(rgb(theme.foreground))
                    .child(confirm.consequence.clone()),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::{AgentGrant, Store};
    use gpui::{IntoElement, Render, TestAppContext, Window};

    struct Rows {
        agents: Agents,
        theme: Theme,
        width: f32,
    }

    impl Render for Rows {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().w(px(self.width)).flex().flex_col().children(
                self.agents.rows.iter().enumerate().map(|(index, row)| {
                    agent_row(&self.theme, &self.agents, row, index, 0, None, None)
                }),
            )
        }
    }

    #[test]
    fn switch_layout_is_the_same_on_selected_and_unselected_rows() {
        let store = Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        store.upsert_account(&account).unwrap();
        for agent in ["codex", "gemini"] {
            store
                .set_agent_grant(&account.id, agent, AgentGrant::enabled(true, false, false))
                .unwrap();
        }
        let mut agents = Agents::default();
        agents.answer(Request::Load.run(&store).unwrap());
        let theme = crate::theme::every_test_theme().remove(0);
        let mut app = TestAppContext::single();
        let (view, cx) = app.add_window_view(|_, _| Rows {
            agents,
            theme,
            width: 578.,
        });
        let switches = [
            [
                "agent-0-Read",
                "agent-0-Triage",
                "agent-0-Draft",
                "agent-0-Annotate",
            ],
            [
                "agent-1-Read",
                "agent-1-Triage",
                "agent-1-Draft",
                "agent-1-Annotate",
            ],
        ];
        for width in [578., 386.] {
            let mut before = None;
            for selected in [0, 1] {
                view.update(cx, |view, cx| {
                    view.width = width;
                    view.agents.select(selected);
                    cx.notify();
                });
                cx.run_until_parked();
                let positions =
                    switches.map(|row| row.map(|id| cx.debug_bounds(id).unwrap().origin.x));
                assert_eq!(positions[0], positions[1], "width {width}, row {selected}");
                if let Some(before) = before {
                    assert_eq!(positions, before, "selecting a row moved its switches");
                }
                before = Some(positions);
            }
        }
    }

    #[test]
    fn agent_rows_have_contrast_headroom_on_every_stock_theme() {
        for theme in crate::theme::every_test_theme() {
            for (bg, selection) in [(theme.surface, 0.), (theme.selection, 1.)] {
                let outline = crate::theme::contrast(switch_outline(&theme, bg), bg);
                let dimmed = crate::theme::contrast(row_dimmed(&theme, bg, selection), bg);
                assert!(outline >= SWITCH_CONTRAST, "{}: {outline:.3}", theme.name);
                assert!(dimmed >= ROW_TEXT_CONTRAST, "{}: {dimmed:.3}", theme.name);
            }
        }
    }

    #[test]
    fn footer_fades_out_before_the_next_line_fades_in() {
        assert_eq!(footer_opacities(0., 1.), (1., 0.));
        assert_eq!(footer_opacities(0.5, 1.), (0., motion::ARRIVE_FROM));
        assert_eq!(footer_opacities(1., 1.), (0., 1.));
        let (old, new) = footer_opacities(0.25, 1.);
        assert!(old > motion::ARRIVE_FROM && old < 1. && new == 0.);
        let (old, new) = footer_opacities(0.75, 1.);
        assert!(old == 0. && new > motion::ARRIVE_FROM && new < 1.);
    }
}
