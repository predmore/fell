use std::time::{Duration, Instant};

use fell_core::ImapSecurity;
use gpui::{
    AnyElement, Context, CursorStyle, Div, FontWeight, KeyDownEvent, MouseButton, Render,
    SharedString, Stateful, Window, div, prelude::*, px, rgb,
};

use crate::motion::{self, ARRIVE_FROM, SwapFrame};

use super::form::{Check, Issue};
use super::{Bridge, Control, Field, Onboarding, Provider, Service, Step};

mod feedback;
pub(super) use feedback::Feedback;
use feedback::{Actions, Connection, SaveState};

type Footer = Option<AnyElement>;

const SAVE_WIDTH: f32 = 164.;
const PORT_WIDTH: f32 = 62.;
const SECURITY_WIDTH: f32 = 116.;
const FIELD_GAP: f32 = 8.;

fn focus_ring(accent: u32, radius: f32) -> Div {
    div()
        .absolute()
        .inset(px(-3.))
        .border_2()
        .rounded(px(radius))
        .border_color(rgb(accent))
}

impl Render for Onboarding {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let now = self.clock.frame(Instant::now());
        let mut old_layer = None;
        let mut arrival = 1.;
        let mut slide = 0.;
        match self
            .transition
            .and_then(|(old, swap)| Some((old, swap.frame(now)?)))
        {
            Some((old, SwapFrame::Leaving(opacity))) => {
                arrival = 0.;
                old_layer = Some((old, opacity));
            }
            Some((_, SwapFrame::Arriving(progress))) => {
                arrival = motion::lerp(ARRIVE_FROM, 1., progress);
                slide = motion::lerp(super::STEP_OFFSET, 0., progress);
            }
            None => self.transition = None,
        }
        let feedback = self
            .feedback
            .refresh(&self.form, self.bridge, self.motion, now);
        if self.feedback.checks_settled()
            && let Some(check) = self.checked.take()
        {
            if self.form.checks == [Check::Failed; 2] {
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let _ = this.update(cx, |view, cx| view.finish_check(check, cx));
                })
                .detach();
            } else {
                let entity = cx.entity();
                window.defer(cx, move |_, cx| {
                    entity.update(cx, |view, cx| view.finish_check(check, cx));
                });
            }
        }
        let pulsing = self.motion.enabled()
            && self.form.step == Step::Connection
            && self.form.checks.contains(&Check::Testing);
        let pulse = if pulsing {
            let phase =
                now.duration_since(self.check_started).as_secs_f32() * std::f32::consts::TAU;
            motion::lerp(1., 0.35, (1. - phase.cos()) * 0.5)
        } else {
            1.
        };
        if self.transition.is_some() || feedback || pulsing {
            window.request_animation_frame();
        } else {
            self.clock.idle();
        }
        div()
            .id("onboarding")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                view.key_down(event, window, cx)
            }))
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(self.theme.background))
            .child(self.content(window, cx, true, (arrival, slide), old_layer, pulse))
    }
}

impl Onboarding {
    pub(crate) fn leaving_layer(&self, window: &Window, cx: &Context<Self>) -> Div {
        self.content(window, cx, false, (1., 0.), None, 1.)
    }

    fn content(
        &self,
        window: &Window,
        cx: &Context<Self>,
        live: bool,
        (arrival, dy): (f32, f32),
        old_layer: Option<(Step, f32)>,
        pulse: f32,
    ) -> Div {
        let theme = &self.theme;
        let narrow = window.viewport_size().width < px(540.);
        let compact_footer = window.viewport_size().width < px(400.);
        let side = if narrow { 20. } else { 44. };
        let top = (f32::from(window.viewport_size().height) * 0.08).clamp(23., 68.);
        div()
            .text_color(rgb(theme.foreground))
            .text_size(px(theme.ui_px))
            .when_some(theme.ui_family.clone(), |el, family| el.font_family(family))
            .px(px(side))
            .py(px(top))
            .child(
                div()
                    .w_full()
                    .max_w(px(520.))
                    .mx_auto()
                    .child(self.progress())
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .when_some(old_layer, |el, (step, opacity)| {
                                el.child(
                                    self.step_layer(step, false, compact_footer, 1., cx)
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .opacity(opacity),
                                )
                            })
                            .child(
                                self.step_layer(self.form.step, live, compact_footer, pulse, cx)
                                    .relative()
                                    .top(px(dy))
                                    .opacity(arrival),
                            ),
                    ),
            )
    }

    fn step_layer(
        &self,
        step: Step,
        live: bool,
        narrow: bool,
        pulse: f32,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        // Each painted step owns an ID scope, including the inert copy.
        // Shared labels (Back, Continue) and fields can coexist during a fade.
        div()
            .id(("setup-step", step as usize))
            .w_full()
            .child(self.step(step, live, narrow, pulse, cx))
    }

    fn progress(&self) -> Div {
        let theme = &self.theme;
        let step = self.form.step.number();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .mb_8()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Account setup"),
            )
            .child(div().flex().gap_1p5().children((1..=4).map(|index| {
                let color = if index == step {
                    theme.accent
                } else if index < step {
                    crate::motion::mix(theme.border, theme.accent, 0.52)
                } else {
                    theme.input_border
                };
                div().flex_1().h(px(2.)).rounded_full().bg(rgb(color))
            })))
    }

    fn step(&self, step: Step, live: bool, narrow: bool, pulse: f32, cx: &Context<Self>) -> Div {
        let column = || div().flex().flex_col().gap_2();
        let (title, detail, content, footer): (&str, &str, Div, [Footer; 3]) = match step {
            Step::Provider => (
                "Connect your mail",
                "Choose how Fell connects to your mailbox.",
                column()
                    .child(self.choice(
                        Control::Provider(Provider::Bridge),
                        "Proton Mail Bridge",
                        "Connect through the Bridge app on this computer.",
                        live,
                        cx,
                    ))
                    .child(self.choice(
                        Control::Other,
                        "Other IMAP account",
                        "Choose a preset or enter your server settings.",
                        live,
                        cx,
                    )),
                [None, None, None],
            ),
            Step::Preset => {
                let descriptions = [
                    "Fastmail app password",
                    "Google app password",
                    "Apple app-specific password",
                    "Enter IMAP and SMTP settings",
                ];
                (
                    "Choose a provider",
                    "Presets fill in the incoming and outgoing servers.",
                    column().children(Provider::PRESETS.into_iter().zip(descriptions).map(
                        |(provider, desc)| {
                            self.choice(
                                Control::Provider(provider),
                                provider.name(),
                                desc,
                                live,
                                cx,
                            )
                        },
                    )),
                    self.footer("Back", None, None, live, cx),
                )
            }
            Step::Identity => {
                let more = self.form.needs_servers();
                let user = self.form.text(Field::Email);
                let username: SharedString = if user.trim().is_empty() {
                    "Defaults to your email address".into()
                } else {
                    user.trim().to_owned().into()
                };
                let password_note = self.feedback.password.paint(false, |issue, _| {
                    issue.as_ref().map_or_else(
                        || self.meta(self.form.provider.password_hint()),
                        |issue| self.field_issue(issue),
                    )
                });
                let content = column()
                    .child(self.field_box(
                        Field::Email,
                        "Email address",
                        "you@example.com",
                        live,
                        cx,
                    ))
                    .child(self.field_box(
                        Field::Name,
                        "Display name · optional",
                        "Name shown on outgoing mail",
                        live,
                        cx,
                    ))
                    .child(self.field_box(Field::Username, "Username", username, live, cx))
                    .child(self.field_box(Field::Password, "Password", "", live, cx))
                    .child(
                        div()
                            .min_h(px((self.theme.ui_px + 4.) * 3.))
                            .child(password_note),
                    );
                (
                    "Account details",
                    self.form.provider.name(),
                    content,
                    self.footer(
                        "Back",
                        (!more).then_some((Control::Servers, "Edit servers")),
                        Some(if more { "Continue" } else { "Test connection" }),
                        live,
                        cx,
                    ),
                )
            }
            Step::Servers => (
                "Server settings",
                self.form.provider.name(),
                column().gap_4().children(
                    Service::ALL
                        .into_iter()
                        .map(|service| self.server(service, live, cx)),
                ),
                self.footer("Back", None, Some("Test connection"), live, cx),
            ),
            Step::Connection => (
                "",
                "",
                column().children(
                    Service::ALL
                        .into_iter()
                        .map(|service| self.check_row(service, pulse)),
                ),
                [None, None, None],
            ),
            Step::Done => (
                "",
                "",
                column()
                    .child(
                        div()
                            .border_1()
                            .rounded_lg()
                            .border_color(rgb(self.theme.border))
                            .bg(rgb(self.theme.surface))
                            .p_4()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(self.form.text(Field::Email).trim().to_owned()),
                    )
                    .child(
                        self.feedback.save_note.paint(false, |state, _| match state {
                            SaveState::Ready => self.meta("Fell keeps your password in the desktop keyring."),
                            SaveState::SavingKeyring => self.meta("Approve any keyring prompt to continue."),
                            SaveState::ConfirmFile => self.meta(
                                "Your password will be saved as plain text in account.toml. Fell restricts access to the file to your user account.",
                            ),
                            SaveState::Failed { title, detail, .. } => self.error(title, detail.as_deref()),
                        }).min_h(px((self.theme.ui_px + 4.) * 5.)),
                    ),
                [None, None, None],
            ),
        };
        let heading = match step {
            Step::Connection => {
                let heading = self.feedback.connection.paint(false, |state, _| {
                    let (title, detail) = state.heading();
                    self.heading(title, detail)
                });
                self.reserved_heading(Connection::ALL.map(Connection::heading), heading)
            }
            Step::Done => {
                let heading = self.heading_column(
                    self.feedback
                        .save_title
                        .paint(false, |title, _| self.title(title)),
                    self.feedback
                        .save_subtitle
                        .paint(false, |detail, _| self.meta(*detail)),
                );
                self.reserved_heading(SaveState::HEADINGS, heading)
            }
            _ => self.heading(title, detail),
        };
        let footer = match step {
            Step::Connection => {
                let buttons =
                    self.animated_footer(&self.feedback.connection_actions, false, live, cx);
                self.footer_rows(buttons, narrow)
            }
            Step::Done => {
                let buttons = self.animated_footer(&self.feedback.save_actions, true, live, cx);
                self.footer_rows(buttons, narrow)
            }
            _ => {
                let stack = narrow && footer[1].is_some();
                self.footer_rows(footer, stack)
            }
        };
        div()
            .flex()
            .flex_col()
            .min_h(px(482.))
            .child(heading.mb_5())
            .child(div().flex_1().child(content))
            .child(footer)
    }

    fn footer_rows(&self, [back, mut secondary, primary]: [Footer; 3], stacked: bool) -> Div {
        if back.is_none() && secondary.is_none() && primary.is_none() {
            return div();
        }
        div()
            .when(stacked, |el| {
                el.child(
                    div()
                        .flex()
                        .ml(px(-13.))
                        .min_h(px(self.theme.ui_px + 24.))
                        .children(secondary.take()),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .mt_3()
                    .children(back)
                    .child(div().flex_1())
                    .children(secondary)
                    .children(primary),
            )
    }

    fn heading(&self, title: &str, detail: &str) -> Div {
        self.heading_column(self.title(title), self.meta(detail.to_owned()))
    }

    fn title(&self, title: &str) -> Div {
        div()
            .text_size(px(self.theme.ui_px + 15.))
            .line_height(px(self.theme.ui_px + 21.))
            .font_weight(FontWeight::SEMIBOLD)
            .child(title.to_owned())
    }

    fn heading_column(&self, title: Div, detail: Div) -> Div {
        div().flex().flex_col().gap_1p5().child(title).child(detail)
    }

    fn reserved_heading<const N: usize>(&self, headings: [(&str, &str); N], current: Div) -> Div {
        // Each measuring copy takes the full column width. The row keeps
        // the tallest heading's height at the current window size.
        div()
            .relative()
            .overflow_hidden()
            .child(div().flex().children(headings.map(|(title, detail)| {
                self.heading(title, detail).w_full().flex_none().opacity(0.)
            })))
            .child(current.absolute().inset_0())
    }

    fn animated_footer(
        &self,
        actions: &Actions,
        saving: bool,
        live: bool,
        cx: &Context<Self>,
    ) -> [Footer; 3] {
        std::array::from_fn(|index| {
            if saving && index == 1 {
                return Some(
                    self.feedback
                        .save_note
                        .paint(live, |state, live| {
                            div().when(state.inline_available(), |el| {
                                el.child(self.button(
                                    Control::Inline,
                                    "Save in file…",
                                    false,
                                    live,
                                    cx,
                                ))
                            })
                        })
                        .flex_none()
                        .into_any_element(),
                );
            }
            Some(
                actions[index]
                    .paint(live, |action, live| {
                        div().children(action.map(|action| {
                            let primary = index == 2;
                            let back = index == 0 && saving;
                            let enabled = action.enabled
                                && (!back
                                    || !matches!(self.form.storage, super::Storage::Saving { .. }));
                            self.button(action.control, action.label, primary, live && enabled, cx)
                                .when(primary && saving, |button| {
                                    button.w(px(SAVE_WIDTH)).text_center()
                                })
                                .when(back, |button| {
                                    button.text_color(rgb(motion::mix(
                                        self.theme.foreground,
                                        self.theme.dimmed,
                                        self.feedback.back_amount,
                                    )))
                                })
                        }))
                    })
                    .flex_none()
                    .into_any_element(),
            )
        })
    }

    fn footer(
        &self,
        back: &str,
        secondary: Option<(Control, &str)>,
        primary: Option<&str>,
        live: bool,
        cx: &Context<Self>,
    ) -> [Footer; 3] {
        [
            Some(
                self.button(Control::Back, back, false, live, cx)
                    .into_any_element(),
            ),
            secondary.map(|(control, label)| {
                self.button(control, label, false, live, cx)
                    .into_any_element()
            }),
            primary.map(|label| {
                self.button(Control::Next, label, true, live, cx)
                    .into_any_element()
            }),
        ]
    }

    fn choice(
        &self,
        control: Control,
        title: &'static str,
        detail: &'static str,
        live: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let focused = live && self.control == control;
        self.clickable(control, title, live, cx)
            .w_full()
            .flex()
            .justify_between()
            .items_center()
            .gap(px(12.))
            .px(px(16.))
            .py_2p5()
            .border_1()
            .rounded_lg()
            .border_color(rgb(if focused {
                self.theme.accent
            } else {
                self.theme.border
            }))
            .bg(rgb(self.theme.surface))
            .when(focused, |el| {
                el.relative().child(focus_ring(self.theme.accent, 10.))
            })
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(self.meta(detail))
                    .when(control == Control::Provider(Provider::Bridge), |el| {
                        let status = self
                            .feedback
                            .bridge
                            .paint(live, |bridge, live| self.bridge_status(*bridge, live, cx));
                        // Keep the height of the wrapped offline hint even
                        // when the listener comes up at narrow widths.
                        el.child(
                            div()
                                .relative()
                                .mt_1()
                                .child(self.bridge_status(Bridge::Offline, false, cx).opacity(0.))
                                .child(status.absolute().inset_0()),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_color(rgb(if focused {
                        self.theme.accent
                    } else {
                        self.theme.dimmed
                    }))
                    .child("›"),
            )
    }

    fn bridge_status(&self, bridge: Bridge, live: bool, cx: &Context<Self>) -> Div {
        let (text, marker, color) = match bridge {
            Bridge::Checking => ("Checking Bridge…", "•", self.theme.dimmed),
            Bridge::Listening => ("Bridge is running", "✓", self.theme.success),
            Bridge::Offline => (
                "Bridge isn't running. Open it and sign in, or",
                "•",
                self.theme.dimmed,
            ),
        };
        let link_focused = live && self.control == Control::Install;
        div()
            .flex()
            .items_start()
            .gap_1p5()
            .child(
                div()
                    .flex_none()
                    .text_size(px(14.))
                    .text_color(rgb(color))
                    .child(marker),
            )
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_1()
                    .child(self.meta(text).min_w_0().max_w_full())
                    .when(bridge == Bridge::Offline, |el| {
                        let link = div()
                            .text_size(px(self.theme.ui_px - 1.))
                            .line_height(px(self.theme.ui_px + 4.))
                            .text_color(rgb(self.theme.accent))
                            .child("Install Bridge ↗");
                        if live {
                            el.child(
                                self.clickable(Control::Install, "Install Bridge ↗", true, cx)
                                    .rounded_sm()
                                    .when(link_focused, |link| {
                                        link.relative().child(focus_ring(self.theme.accent, 5.))
                                    })
                                    .child(link),
                            )
                        } else {
                            el.child(link)
                        }
                    }),
            )
    }

    fn button(
        &self,
        control: Control,
        label: &str,
        primary: bool,
        live: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let focused = live && self.control == control;
        let theme = &self.theme;
        self.clickable(control, label, live, cx)
            .flex_shrink_0()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .when(control == Control::Back, |el| el.ml(px(-13.)))
            .when(primary, |el| {
                el.bg(rgb(theme.accent))
                    .text_color(rgb(theme.background))
                    .font_weight(FontWeight::SEMIBOLD)
            })
            .border_color(if focused && primary {
                rgb(theme.background).into()
            } else {
                gpui::transparent_black()
            })
            .when(focused, |el| {
                el.relative().child(focus_ring(theme.accent, 7.))
            })
            .child(label.to_owned())
    }

    fn clickable(
        &self,
        control: Control,
        label: &str,
        live: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        div()
            .id(SharedString::from(format!("{control:?}-{label}")))
            .when(live, |el| {
                let accent = self.theme.accent;
                el.cursor_pointer()
                    .hover(move |style| style.border_color(rgb(accent)))
                    .on_click(cx.listener(move |view, _, window, cx| {
                        cx.stop_propagation();
                        view.transition = None;
                        view.select(control);
                        view.activate(control, window, cx);
                        cx.notify();
                    }))
            })
    }

    fn meta(&self, text: impl Into<SharedString>) -> Div {
        div()
            .text_size(px(self.theme.ui_px - 1.))
            .line_height(px(self.theme.ui_px + 4.))
            .text_color(rgb(self.theme.dimmed))
            .child(text.into())
    }

    fn error(&self, title: &str, detail: Option<&str>) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_0p5()
            .text_size(px(self.theme.ui_px - 1.))
            .line_height(px(self.theme.ui_px + 3.))
            .child(
                div()
                    .text_color(rgb(self.theme.error))
                    .font_weight(FontWeight::MEDIUM)
                    .child(title.to_owned()),
            )
            .when_some(detail, |el, detail| el.child(self.meta(detail.to_owned())))
    }

    fn field_issue(&self, issue: &Issue) -> Div {
        self.error(issue.title, issue.recovery)
    }

    fn field_box(
        &self,
        field: Field,
        label: &'static str,
        placeholder: impl Into<SharedString>,
        live: bool,
        cx: &Context<Self>,
    ) -> Div {
        let focus = live && self.control == Control::Field(field);
        let border = self.field_border(field, focus);
        div()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(self.label(label))
            .child(
                div()
                    .id(SharedString::from(format!("{field:?}")))
                    .px_2p5()
                    .py_1p5()
                    .w_full()
                    .border_1()
                    .rounded_md()
                    .border_color(rgb(border))
                    .bg(rgb(self.theme.surface))
                    .when(focus, |el| el.relative().child(focus_ring(border, 7.)))
                    .when(live, |el| {
                        el.cursor(CursorStyle::IBeam).on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |view, e: &gpui::MouseDownEvent, window, cx| {
                                window.focus(&view.focus);
                                view.transition = None;
                                view.select(Control::Field(field));
                                view.point_caret(field, e.position, e.modifiers.shift);
                                cx.notify();
                            }),
                        )
                    })
                    .child(div().min_w_0().overflow_hidden().child(self.field_line(
                        field,
                        placeholder.into(),
                        live,
                        cx,
                    ))),
            )
            .when(field == Field::Email, |el| {
                el.child(
                    self.feedback
                        .email
                        .paint(false, |issue, _| {
                            div().children(issue.as_ref().map(|issue| self.field_issue(issue)))
                        })
                        .min_h(px(self.theme.ui_px + 4.)),
                )
            })
    }

    fn field_border(&self, field: Field, focus: bool) -> u32 {
        let regular = if focus {
            self.theme.accent
        } else {
            self.theme.input_border
        };
        let error = self.feedback.issue(field).map_or(0., |issue| {
            if issue.value.is_some() {
                1.
            } else {
                issue.clearing()
            }
        });
        motion::mix(regular, self.theme.error, error)
    }

    fn label(&self, label: &'static str) -> Div {
        div()
            .text_size(px(self.theme.ui_px - 1.))
            .font_weight(FontWeight::MEDIUM)
            .child(label)
    }

    fn server(&self, service: Service, live: bool, cx: &Context<Self>) -> Div {
        let field = Field::Security(service);
        let control = Control::Field(field);
        let focused = live && self.control == control;
        let border = self.field_border(field, focused);
        let security = match self.form.security(service) {
            ImapSecurity::StartTls => "STARTTLS",
            ImapSecurity::Tls => "TLS",
            ImapSecurity::Plain => "Plain",
        };
        let [host_issue, port_issue, security_issue] = &self.feedback.servers[service as usize];
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(service.name()),
            )
            .child(
                div()
                    .flex()
                    .items_end()
                    .gap(px(FIELD_GAP))
                    .child(
                        self.field_box(Field::Host(service), "Host", "mail.example.com", live, cx)
                            .flex_1(),
                    )
                    .child(
                        self.field_box(Field::Port(service), "Port", "", live, cx)
                            .w(px(PORT_WIDTH)),
                    )
                    .child(
                        div()
                            .w(px(SECURITY_WIDTH))
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(self.label("Security"))
                            .child(
                                self.clickable(control, service.name(), live, cx)
                                    .flex()
                                    .justify_between()
                                    .items_center()
                                    .px_2p5()
                                    .py_1p5()
                                    .h(px(self.theme.ui_px + 20.))
                                    .border_1()
                                    .rounded_md()
                                    .border_color(rgb(border))
                                    .bg(rgb(self.theme.surface))
                                    .when(focused, |el| el.relative().child(focus_ring(border, 7.)))
                                    .child(
                                        div().text_size(px(self.theme.ui_px - 1.)).child(security),
                                    )
                                    .child(self.meta("↻")),
                            ),
                    ),
            )
            .child(
                div()
                    .relative()
                    .h(px((self.theme.ui_px + 4.) * 4.))
                    .child(
                        host_issue
                            .paint(false, |issue, _| {
                                div().children(issue.as_ref().map(|issue| self.field_issue(issue)))
                            })
                            .absolute()
                            .left_0()
                            .top_0()
                            .right(px(PORT_WIDTH + SECURITY_WIDTH + FIELD_GAP * 2.)),
                    )
                    .child(
                        port_issue
                            .paint(false, |issue, _| {
                                div().children(issue.as_ref().map(|issue| self.field_issue(issue)))
                            })
                            .absolute()
                            .top_0()
                            .right_0()
                            .w(px(PORT_WIDTH + SECURITY_WIDTH + FIELD_GAP)),
                    )
                    .child(
                        security_issue
                            .paint(live, |issue, live| {
                                div().flex().flex_col().items_end().gap_1().when_some(
                                    issue.as_ref(),
                                    |el, issue| {
                                        let validation = issue.recovery.is_none();
                                        el.when(validation, |el| {
                                            el.pt(px((self.theme.ui_px + 4.) * 2.))
                                        })
                                        .child(self.field_issue(issue).w_full().text_right())
                                        .when(
                                            issue.suggest_starttls,
                                            |el| {
                                                let control = Control::StartTls(service);
                                                el.child(
                                                    self.clickable(
                                                        control,
                                                        "Use STARTTLS",
                                                        live,
                                                        cx,
                                                    )
                                                    .relative()
                                                    .rounded_sm()
                                                    .text_size(px(self.theme.ui_px - 1.))
                                                    .line_height(px(self.theme.ui_px + 4.))
                                                    .text_color(rgb(self.theme.accent))
                                                    .when(live && self.control == control, |link| {
                                                        link.child(focus_ring(
                                                            self.theme.accent,
                                                            4.,
                                                        ))
                                                    })
                                                    .child("Use STARTTLS"),
                                                )
                                            },
                                        )
                                    },
                                )
                            })
                            .absolute()
                            .top_0()
                            .right_0()
                            .w_full(),
                    ),
            )
    }

    fn check_row(&self, service: Service, pulse: f32) -> Div {
        let state = &self.feedback.checks[service as usize];
        div()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_1()
            .bg(rgb(self.theme.surface))
            .border_1()
            .border_color(rgb(self.theme.border))
            .rounded_lg()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(service.name()),
                    )
                    .child(state.paint(false, |check, _| {
                        let (status, marker, color) = match check {
                            Check::Waiting => ("Waiting", "•", self.theme.dimmed),
                            Check::Testing => ("Signing in…", "•", self.theme.accent),
                            Check::Connected => ("Connected", "✓", self.theme.success),
                            Check::Failed => ("Failed", "✕", self.theme.error),
                        };
                        div()
                            .flex()
                            .justify_end()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(14.))
                                    .text_color(rgb(color))
                                    .opacity(if *check == Check::Testing { pulse } else { 1. })
                                    .child(marker),
                            )
                            .child(self.meta(status).whitespace_nowrap())
                    })),
            )
            .child(
                self.feedback.bridge_offline[service as usize]
                    .paint(false, |offline, _| {
                        div().when(*offline, |el| {
                            el.child(self.error("Bridge isn't running", None))
                        })
                    })
                    .min_h(px(self.theme.ui_px + 4.)),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::unix::fs::FileTypeExt;
    use std::path::PathBuf;
    use std::rc::Rc;

    use super::*;
    use crate::motion::Motion;

    #[test]
    #[ignore = "requires an isolated nested Wayland compositor"]
    fn renders_both_steps_with_shared_button_labels_during_a_fade() {
        check_in_nested_window(|setup, cx| {
            // Identity and Server settings share Back and Test connection.
            setup.form.edit_servers();
            setup.change_step(Step::Identity, Control::Back, cx);
            assert!(setup.transition.is_some());
        });
    }

    #[test]
    #[ignore = "requires an isolated nested Wayland compositor"]
    fn unchanged_theme_keeps_setup_transitions_running() {
        check_in_nested_window(|setup, cx| {
            setup.form.edit_servers();
            setup.change_step(Step::Identity, Control::Back, cx);
            setup.bridge = Bridge::Offline;
            let now = setup.clock.now();
            let tick_feedback = |setup: &mut Onboarding| {
                setup
                    .feedback
                    .refresh(&setup.form, setup.bridge, setup.motion, now)
            };
            assert!(tick_feedback(setup));

            let mut theme = setup.theme.clone();
            assert!(!setup.apply_theme(theme.clone()));
            assert!(setup.transition.is_some());
            assert!(tick_feedback(setup));

            theme.accent ^= 1;
            assert!(setup.apply_theme(theme));
            assert!(setup.transition.is_none());
            assert!(!tick_feedback(setup));
        });
    }

    fn check_in_nested_window(
        check: impl FnOnce(&mut Onboarding, &mut Context<Onboarding>) + 'static,
    ) {
        let display = std::env::var_os("WAYLAND_DISPLAY")
            .map(PathBuf::from)
            .unwrap();
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap();
        assert_eq!(
            std::env::var_os("FELL_TEST_NESTED_WAYLAND").as_deref(),
            Some(display.as_os_str())
        );
        assert!(display.is_absolute() && display.parent() == Some(runtime.as_path()));
        assert!(display.metadata().unwrap().file_type().is_socket());
        assert!(std::env::var_os("DISPLAY").is_none());

        let rendered = Rc::new(Cell::new(false));
        let completed = rendered.clone();
        gpui::Application::new().run(move |cx| {
            let handle = cx
                .open_window(gpui::WindowOptions::default(), |window, cx| {
                    cx.new(|cx| {
                        let mut setup = Onboarding::init(window, cx);
                        setup.motion = Motion::new(true);
                        setup.form.choose(Provider::Bridge);
                        setup
                    })
                })
                .unwrap();
            cx.spawn(async move |cx| {
                // Enter the compositor event loop before drawing and quitting.
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(30))
                    .await;
                cx.update(|cx| {
                    handle
                        .update(cx, |setup, window, cx| {
                            check(setup, cx);
                            window.defer(cx, move |window, cx| {
                                window.draw(cx).clear();
                                completed.set(true);
                                cx.quit();
                            });
                        })
                        .unwrap();
                })
                .unwrap();
            })
            .detach();
        });
        assert!(rendered.get());
    }
}
