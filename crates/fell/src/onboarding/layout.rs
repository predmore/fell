use std::time::{Duration, Instant};

use fell_core::ImapSecurity;
use gpui::{
    AnyElement, Context, CursorStyle, Div, FontWeight, KeyDownEvent, MouseButton, Render,
    SharedString, Stateful, Window, div, prelude::*, px, rems, rgb,
};

use crate::motion::{self, ARRIVE_FROM, SwapFrame};
use crate::view::disclosure::layered;

use super::form::{Check, Issue};
use super::{Bridge, Control, Field, Onboarding, Provider, Service, Step};

mod feedback;
pub(super) use feedback::Feedback;
use feedback::{Actions, Connection, SaveState, ServerIssues};

type Footer = Option<AnyElement>;

const STEP_WIDTH: f32 = 520.;
const STEP_HEIGHT: f32 = 520.;
const CHOICE_PADDING: f32 = 16.;
const CHOICE_GAP: f32 = 12.;
const CHOICE_ARROW: f32 = 16.;
const SERVER_INPUT_PADDING_REM: f32 = 0.375;
const FIELD_GAP: f32 = 8.;
const SECURITY_PADDING: f32 = 5.;
const SECURITY_GAP: f32 = 6.;
const SECURITY_CAPTION: &str = "Security";
const SECURITY_ICON: &str = "↻";

#[derive(Clone, Copy)]
struct ServerFieldWidths {
    port: f32,
    security: f32,
}

impl ServerFieldWidths {
    fn measure(window: &Window, theme: &crate::theme::Theme) -> Self {
        let mut style = window.text_style();
        if let Some(family) = &theme.ui_family {
            style.font_family = family.clone().into();
        }
        let text_width = |text: &'static str, size, style: &gpui::TextStyle| {
            let run = style.to_run(text.len());
            let line = window
                .text_system()
                .shape_line(text.into(), px(size), &[run], None);
            f32::from(line.width)
        };
        let digits = text_width("65535", theme.ui_px, &style);
        // Allow for the border, horizontal padding, and the caret at the end.
        let input_padding = f32::from(window.rem_size()) * SERVER_INPUT_PADDING_REM;
        let port = (digits + 2. * (input_padding + 1.) + 2.).ceil();

        // Reserve space for every option so cycling keeps the fields aligned.
        let size = security_text_size(theme);
        let label = [
            ImapSecurity::Tls,
            ImapSecurity::StartTls,
            ImapSecurity::Plain,
        ]
        .into_iter()
        .map(|mode| text_width(security_label(mode), size, &style))
        .fold(0., f32::max);
        let icon = text_width(SECURITY_ICON, size, &style);
        style.font_weight = FontWeight::MEDIUM;
        let caption = text_width(SECURITY_CAPTION, theme.ui_px - 1., &style);
        let security = (label + icon + SECURITY_GAP + 2. * (SECURITY_PADDING + 1.))
            .max(caption)
            .ceil();
        Self { port, security }
    }
}

fn security_text_size(theme: &crate::theme::Theme) -> f32 {
    theme.ui_px - 2.
}

fn security_label(mode: ImapSecurity) -> &'static str {
    match mode {
        ImapSecurity::StartTls => "STARTTLS",
        ImapSecurity::Tls => "TLS",
        ImapSecurity::Plain => "Plain",
    }
}

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
        self.painted_fields.clear();
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
        let shared_back = self.transition.is_some_and(|(old, _)| {
            self.has_back_label(old) && self.has_back_label(self.form.step)
        });
        let narrow = window.viewport_size().width < px(540.);
        let compact_footer = window.viewport_size().width < px(400.);
        let side = if narrow { 20. } else { 44. };
        let measure = (f32::from(window.viewport_size().width) - 2. * side).clamp(1., STEP_WIDTH);
        let layout = (window, measure, compact_footer);
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
                    .max_w(px(STEP_WIDTH))
                    .mx_auto()
                    .child(self.progress())
                    .child(
                        div()
                            .relative()
                            .w_full()
                            .when_some(old_layer, |el, (step, opacity)| {
                                el.child(
                                    self.step_layer(
                                        step,
                                        false,
                                        layout,
                                        (1., 0.),
                                        (opacity, if shared_back { 0. } else { opacity }),
                                        cx,
                                    )
                                    .absolute()
                                    .top_0()
                                    .left_0(),
                                )
                            })
                            .child(self.step_layer(
                                self.form.step,
                                live,
                                layout,
                                (pulse, dy),
                                (arrival, if shared_back { 1. } else { arrival }),
                                cx,
                            ))
                            .children(self.primary_button(window, live, cx)),
                    ),
            )
    }

    fn has_back_label(&self, step: Step) -> bool {
        match step {
            Step::Provider => false,
            Step::Done => self.feedback.save_actions[0]
                .value
                .is_some_and(|action| action.label == "Back"),
            _ => true,
        }
    }

    fn step_layer(
        &self,
        step: Step,
        live: bool,
        layout: (&Window, f32, bool),
        (pulse, dy): (f32, f32),
        opacity: (f32, f32),
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        // Each painted step owns an ID scope, including the inert copy.
        // Shared labels (Back, Continue) and fields can coexist during a fade.
        div()
            .id(("setup-step", step as usize))
            .w_full()
            .child(self.step(step, live, layout, (pulse, dy), opacity, cx))
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
                    .child(if self.form.previous.is_some() {
                        "Edit connection"
                    } else {
                        "Account setup"
                    }),
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

    fn step(
        &self,
        step: Step,
        live: bool,
        (window, measure, narrow): (&Window, f32, bool),
        (pulse, dy): (f32, f32),
        (opacity, back_opacity): (f32, f32),
        cx: &Context<Self>,
    ) -> Div {
        let column = || div().flex().flex_col().gap_2();
        let (title, detail, content, footer): (&str, &str, Div, [Footer; 2]) = match step {
            Step::Provider => (
                "Connect your mail",
                "Choose how Fell connects to your mailbox.",
                column()
                    .child(self.choice(
                        Control::Provider(Provider::Bridge),
                        "Proton Mail Bridge",
                        "Connect through the Bridge app on this computer.",
                        live,
                        Some(measure),
                        cx,
                    ))
                    .child(self.choice(
                        Control::Provider(Provider::Custom),
                        "Other IMAP account",
                        "Enter your IMAP and SMTP server settings.",
                        live,
                        None,
                        cx,
                    )),
                [None, None],
            ),
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
                        || self.meta(if self.form.previous.is_some() {
                            "Leave blank to keep the saved password."
                        } else {
                            self.form.provider.password_hint()
                        }),
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
                    .child(self.field_box(Field::Password, "Password",
                        if self.form.previous.is_some() { "Unchanged" } else { "" }, live, cx))
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
                        if self.form.previous.is_some() { "Cancel" } else { "Back" },
                        (!more).then_some((Control::Servers, "Edit servers")),
                        live,
                        cx,
                    ),
                )
            }
            Step::Servers => {
                let fields = ServerFieldWidths::measure(window, &self.theme);
                (
                    "Server settings",
                    self.form.provider.name(),
                    column().gap_4().children(
                        Service::ALL
                            .into_iter()
                            .map(|service| self.server(service, measure, fields, live, cx)),
                    ),
                    self.footer("Back", None, live, cx),
                )
            }
            Step::Connection => (
                "",
                "",
                column().children(
                    Service::ALL
                        .into_iter()
                        .map(|service| self.check_row(service, pulse, measure)),
                ).when(self.form.failed_check, |column| {
                    column.children(self.form.issues.iter().take(1).map(|issue| self.field_issue(issue)))
                }),
                [None, None],
            ),
            Step::Done => (
                "",
                "",
                column()
                    .when(self.form.retained_mailbox.is_none() && !self.form.replacement(), |column| column.child(
                        div()
                            .border_1()
                            .rounded_lg()
                            .border_color(rgb(self.theme.border))
                            .bg(rgb(self.theme.surface))
                            .p_4()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(self.form.text(Field::Email).trim().to_owned()),
                    ))
                    .child(
                        self.feedback.save_note.paint(false, |state, _| match state {
                            SaveState::Ready if self.form.retained_mailbox.is_some() =>
                                self.meta("Start empty keeps the old mail on this device but doesn't show it."),
                            SaveState::Ready if self.form.pauses_write_back() =>
                                self.meta("Fell keeps queued server changes on this device and pauses write-back after the IMAP endpoint changes."),
                            SaveState::Ready if self.form.previous.is_some() => div(),
                            SaveState::Ready => self.meta("Fell keeps your password in the desktop keyring."),
                            SaveState::SavingKeyring => self.meta("Approve any keyring prompt to continue."),
                            SaveState::ConfirmFile => self.meta(
                                "Your password will be saved as plain text in the account file. Fell restricts access to the file to your user account.",
                            ),
                            SaveState::Failed { title, detail } => self.error(title, detail.as_deref()),
                        }).min_h(px((self.theme.ui_px + 4.) * 5.)),
                    ).when(self.form.failed_check, |column| column.child(
                        self.error("Sync will fail with these settings", Some("You can return to Edit connection to fix them.")),
                    )),
                [None, None],
            ),
        };
        let heading = match step {
            Step::Connection => {
                let heading = self.feedback.connection.paint(false, |state, _| {
                    let (title, detail) = state.heading();
                    self.heading(title, detail)
                });
                self.reserved_heading(
                    Connection::ALL.map(|state| {
                        let (title, detail) = state.heading();
                        self.heading(title, detail)
                    }),
                    heading,
                )
            }
            Step::Done => {
                let reconnect_email = self
                    .form
                    .retained_mailbox
                    .as_ref()
                    .map(|_| crate::present::display_safe(self.form.text(Field::Email).trim()));
                let heading = if self.form.replacement() {
                    let previous = self
                        .form
                        .previous
                        .as_ref()
                        .expect("replacement has a previous account");
                    let old = crate::present::display_safe(&previous.email);
                    let new = crate::present::display_safe(self.form.text(Field::Email).trim());
                    let detail = if old.eq_ignore_ascii_case(&new) {
                        format!(
                            "Fell keeps {old}’s local mail for the old login and starts a fresh mailbox for the new login."
                        )
                    } else {
                        format!("Fell keeps {old}’s local mail and starts {new} fresh.")
                    };
                    self.heading("This is a different mailbox", &detail)
                } else if self.form.previous.is_some() {
                    self.heading(
                        "Save connection changes",
                        "Fell keeps this account’s local mail.",
                    )
                } else if let Some(email) = &reconnect_email
                    && (matches!(self.form.storage, super::Storage::Keyring) || self.reconnect)
                {
                    self.reconnect_heading(email)
                } else {
                    self.heading_column(
                        self.feedback
                            .save_title
                            .paint(false, |title, _| self.title(title)),
                        self.feedback
                            .save_subtitle
                            .paint(false, |detail, _| self.meta(*detail)),
                    )
                };
                let measures = SaveState::HEADINGS
                    .into_iter()
                    .map(|(title, detail)| self.heading(title, detail))
                    .chain(
                        reconnect_email
                            .as_ref()
                            .map(|email| self.reconnect_heading(email)),
                    );
                self.reserved_heading(measures, heading)
            }
            _ => self.heading(title, detail),
        };
        let footer = match step {
            Step::Connection => {
                let buttons =
                    self.animated_footer(&self.feedback.connection_actions, false, live, cx);
                self.footer_rows(buttons, narrow, (opacity, back_opacity))
            }
            Step::Done => {
                let buttons = self.animated_footer(&self.feedback.save_actions, true, live, cx);
                self.footer_rows(buttons, narrow, (opacity, back_opacity))
            }
            _ => {
                let stack = narrow && footer[1].is_some();
                self.footer_rows(footer, stack, (opacity, back_opacity))
            }
        };
        div()
            .flex()
            .flex_col()
            .min_h(px(STEP_HEIGHT + 18. * (self.theme.ui_px - 14.).max(0.)))
            .child(
                div()
                    .relative()
                    .top(px(dy))
                    .flex_1()
                    .opacity(opacity)
                    .child(heading.mb_5())
                    .child(content),
            )
            .child(footer)
    }

    fn footer_rows(
        &self,
        [back, secondary]: [Footer; 2],
        stacked: bool,
        (opacity, back_opacity): (f32, f32),
    ) -> Div {
        let faded = |element: AnyElement, alpha| div().flex_none().opacity(alpha).child(element);
        let beside_primary = self.feedback.primary.is_some() && !stacked && secondary.is_some();
        let mut secondary = secondary.map(|el| faded(el, opacity));
        if back.is_none() && secondary.is_none() {
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
                    .min_h(px(self.theme.ui_px + 18.))
                    .children(back.map(|el| faded(el, back_opacity)))
                    .child(div().flex_1())
                    .children(secondary)
                    .when(beside_primary, |row| {
                        row.child(div().w(px(10. * self.theme.ui_px)).flex_none())
                    }),
            )
    }

    fn heading(&self, title: &str, detail: &str) -> Div {
        self.heading_column(self.title(title), self.meta(detail.to_owned()))
    }

    fn reconnect_heading(&self, email: &str) -> Div {
        let title = div()
            .flex()
            .flex_wrap()
            .gap_x_2()
            .text_size(px(self.theme.ui_px + 12.))
            .line_height(px(self.theme.ui_px + 18.))
            .font_weight(FontWeight::SEMIBOLD)
            .child(div().flex_none().child("Reconnect"))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .max_w_full()
                    .child(div().min_w_0().truncate().child(email.to_owned()))
                    .child(div().flex_none().child("?")),
            );
        self.heading_column(title, self.meta("Use the mail already on this device."))
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

    fn reserved_heading(&self, headings: impl IntoIterator<Item = Div>, current: Div) -> Div {
        // Each measuring copy takes the full column width. The row keeps
        // the tallest heading's height at the current window size.
        div()
            .relative()
            .overflow_hidden()
            .child(
                div().flex().children(
                    headings
                        .into_iter()
                        .map(|heading| heading.w_full().flex_none().opacity(0.)),
                ),
            )
            .child(current.absolute().inset_0())
    }

    fn animated_footer(
        &self,
        actions: &Actions,
        saving: bool,
        live: bool,
        cx: &Context<Self>,
    ) -> [Footer; 2] {
        std::array::from_fn(|index| {
            Some(
                actions[index]
                    .paint(live, |action, live| {
                        div().children(action.map(|action| {
                            let back = index == 0 && saving;
                            let enabled = action.enabled
                                && (!back
                                    || !matches!(self.form.storage, super::Storage::Saving { .. }));
                            self.button(action.control, action.label, live && enabled, cx)
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
        live: bool,
        cx: &Context<Self>,
    ) -> [Footer; 2] {
        [
            Some(
                self.button(Control::Back, back, live, cx)
                    .into_any_element(),
            ),
            secondary
                .map(|(control, label)| self.button(control, label, live, cx).into_any_element()),
        ]
    }

    fn choice(
        &self,
        control: Control,
        title: &'static str,
        detail: &'static str,
        live: bool,
        bridge_measure: Option<f32>,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let focused = live && self.control == control;
        self.clickable(control, title, live, cx)
            .w_full()
            .flex()
            .justify_between()
            .items_center()
            .gap(px(CHOICE_GAP))
            .px(px(CHOICE_PADDING))
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
                    .when_some(bridge_measure, |el, measure| {
                        let width =
                            measure - 2. * (CHOICE_PADDING + 1.) - CHOICE_GAP - CHOICE_ARROW;
                        el.child(self.bridge_feedback(width.max(1.), live, cx))
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(CHOICE_ARROW))
                    .text_right()
                    .text_color(rgb(if focused {
                        self.theme.accent
                    } else {
                        self.theme.dimmed
                    }))
                    .child("›"),
            )
    }

    fn bridge_feedback(&self, width: f32, live: bool, cx: &Context<Self>) -> Div {
        let bridge = &self.feedback.bridge;
        let current = self.bridge_status(bridge.value, live, cx);
        let Some((old, alphas, height)) = bridge.layers() else {
            return current.mt_1();
        };
        let old = self.bridge_status(old, false, cx);
        div().mt_1().child(layered(
            width,
            [old.into_any_element(), current.into_any_element()],
            alphas,
            height,
            [true; 2],
        ))
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
        live: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        self.button_shell(control, label, false, (live, true), cx)
            .child(label.to_owned())
    }

    fn primary_button(&self, window: &Window, live: bool, cx: &Context<Self>) -> Option<Div> {
        let action = self.feedback.primary?;
        let mut style = window.text_style();
        if let Some(family) = &self.theme.ui_family {
            style.font_family = family.clone().into();
        }
        style.font_weight = FontWeight::SEMIBOLD;
        let padding = 12.;
        let width = |label: &'static str| {
            let run = style.to_run(label.len());
            let text =
                window
                    .text_system()
                    .shape_line(label.into(), px(self.theme.ui_px), &[run], None);
            f32::from(text.width).ceil() + 2. * (padding + 1.)
        };
        let (text, opacity, button_width) = self.feedback.primary_label_frame(action.label, width);
        Some(
            div()
                .absolute()
                .right_0()
                .bottom_0()
                .h(px(self.theme.ui_px + 18.))
                .flex()
                .items_center()
                .child(
                    self.button_shell(
                        action.control,
                        "primary-action",
                        true,
                        (live, action.enabled),
                        cx,
                    )
                    .relative()
                    .w(px(button_width))
                    .h(px(self.theme.ui_px + 18.))
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .bottom_0()
                            .left(px(padding))
                            .right(px(padding))
                            .overflow_hidden()
                            .flex()
                            .items_center()
                            .justify_center()
                            .whitespace_nowrap()
                            .opacity(opacity)
                            .child(div().flex_none().child(text)),
                    ),
                ),
        )
    }

    fn button_shell(
        &self,
        control: Control,
        label: &str,
        primary: bool,
        (live, enabled): (bool, bool),
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let focused = live && self.control == control;
        let theme = &self.theme;
        self.clickable(control, label, live && enabled, cx)
            .flex_shrink_0()
            .px_3()
            .py_1p5()
            .rounded_md()
            .border_1()
            .when(control == Control::Back, |el| el.ml(px(-13.)))
            .when(primary, |el| {
                let (fill, text) = if enabled {
                    (theme.accent, theme.background)
                } else {
                    (theme.surface, theme.dimmed)
                };
                el.bg(rgb(fill))
                    .text_color(rgb(text))
                    .font_weight(FontWeight::SEMIBOLD)
            })
            .border_color(if focused && primary {
                rgb(theme.background).into()
            } else if primary && !enabled {
                rgb(theme.input_border).into()
            } else {
                gpui::transparent_black()
            })
            .when(focused, |el| {
                el.relative().child(focus_ring(theme.accent, 7.))
            })
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
                    .map(|el| {
                        if matches!(field, Field::Host(_) | Field::Port(_)) {
                            el.px(rems(SERVER_INPUT_PADDING_REM))
                        } else {
                            el.px_2p5()
                        }
                    })
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
                                view.click_field(field, e.position, e.modifiers.shift);
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
        motion::mix(regular, self.theme.error, self.feedback.error_amount(field))
    }

    fn label(&self, label: &'static str) -> Div {
        div()
            .text_size(px(self.theme.ui_px - 1.))
            .font_weight(FontWeight::MEDIUM)
            .child(label)
    }

    fn server(
        &self,
        service: Service,
        width: f32,
        fields: ServerFieldWidths,
        live: bool,
        cx: &Context<Self>,
    ) -> Div {
        let field = Field::Security(service);
        let control = Control::Field(field);
        let focused = live && self.control == control;
        let border = self.field_border(field, focused);
        let security = security_label(self.form.security(service));
        let security_size = px(security_text_size(&self.theme));
        let issues = &self.feedback.servers[service as usize];
        let current = self
            .server_issues(service, issues.value, fields, live, cx)
            .into_any_element();
        let feedback = match issues.frame {
            Some((old, shown)) => layered(
                width,
                [
                    self.server_issues(service, old, fields, false, cx)
                        .into_any_element(),
                    current,
                ],
                [1. - shown, shown],
                shown,
                [true; 2],
            ),
            None => current,
        };
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
                            .flex_none()
                            .w(px(fields.port)),
                    )
                    .child(
                        div()
                            .w(px(fields.security))
                            .flex_none()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(self.label(SECURITY_CAPTION))
                            .child(
                                self.clickable(control, service.name(), live, cx)
                                    .debug_selector(|| format!("{service:?}-security"))
                                    .flex()
                                    .justify_between()
                                    .items_center()
                                    .gap(px(SECURITY_GAP))
                                    .px(px(SECURITY_PADDING))
                                    .py_1p5()
                                    .h(px(self.theme.ui_px + 20.))
                                    .border_1()
                                    .rounded_md()
                                    .border_color(rgb(border))
                                    .bg(rgb(self.theme.surface))
                                    .when(focused, |el| el.relative().child(focus_ring(border, 7.)))
                                    .child(
                                        div()
                                            .debug_selector(|| {
                                                format!("{service:?}-security-value")
                                            })
                                            .flex_none()
                                            .text_size(security_size)
                                            .child(security),
                                    )
                                    .child(
                                        self.meta(SECURITY_ICON)
                                            .debug_selector(|| format!("{service:?}-security-icon"))
                                            .flex_none()
                                            .text_size(security_size),
                                    ),
                            ),
                    ),
            )
            .child(feedback)
    }

    fn server_issues(
        &self,
        service: Service,
        [host, port, security]: ServerIssues,
        fields: ServerFieldWidths,
        live: bool,
        cx: &Context<Self>,
    ) -> Div {
        let link = |control| {
            self.clickable(control, "Use STARTTLS", live, cx)
                .relative()
                .rounded_sm()
                .text_size(px(self.theme.ui_px - 1.))
                .line_height(px(self.theme.ui_px + 4.))
                .text_color(rgb(self.theme.accent))
                .when(live && self.control == control, |link| {
                    link.child(focus_ring(self.theme.accent, 4.))
                })
                .child("Use STARTTLS")
        };
        div()
            .min_h(px(self.theme.ui_px + 4.))
            .flex()
            .flex_col()
            .when(host.is_some() || port.is_some(), |el| {
                el.child(
                    div()
                        .flex()
                        .gap(px(FIELD_GAP))
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .children(host.map(|issue| self.field_issue(&issue))),
                        )
                        .child(
                            div()
                                .flex_none()
                                .w(px(fields.port + fields.security + FIELD_GAP))
                                .children(port.map(|issue| self.field_issue(&issue))),
                        ),
                )
            })
            .when_some(security, |el, issue| {
                el.child(
                    div()
                        .flex()
                        .flex_col()
                        .items_end()
                        .gap_1()
                        .child(self.field_issue(&issue).w_full().text_right())
                        .when(issue.suggest_starttls, |el| {
                            el.child(link(Control::StartTls(service)))
                        }),
                )
            })
    }

    fn check_row(&self, service: Service, pulse: f32, measure: f32) -> Div {
        let state = &self.feedback.checks[service as usize];
        let (offline, _, shown) = self.feedback.bridge_offline[service as usize].visible(false);
        div()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
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
            .when(*offline, |card| {
                let error = self.error("Bridge isn't running", None).pt_1();
                if shown >= 1. {
                    card.child(error)
                } else {
                    card.child(layered(
                        (measure - 34.).max(1.),
                        [div().into_any_element(), error.into_any_element()],
                        [0., shown],
                        shown,
                        [true; 2],
                    ))
                }
            })
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
    use gpui::{EntityInputHandler, Modifiers, TestAppContext, point, size};

    #[test]
    fn port_width_reserves_room_for_five_digits_at_each_ui_size() {
        let mut app = TestAppContext::single();
        let (_, cx) = app.add_window_view(Onboarding::init);
        cx.update(|window, _| {
            let mut theme = crate::theme::load();
            theme.ui_family = Some("DejaVu Sans".into());
            let mut style = window.text_style();
            style.font_family = "DejaVu Sans".into();
            let mut previous_width = 0.;
            for size in [12., 15., 18.] {
                theme.ui_px = size;
                let digits = window.text_system().shape_line(
                    "65535".into(),
                    px(size),
                    &[style.to_run(5)],
                    None,
                );
                let width = ServerFieldWidths::measure(window, &theme).port;
                let insets = 2. * (f32::from(window.rem_size()) * 0.375 + 1.);
                assert!(width - insets >= f32::from(digits.width) + 2.);
                assert!(width > previous_width);
                previous_width = width;
            }
        });
    }

    #[test]
    fn security_options_fit_both_server_fields_at_supported_sizes() {
        let mut app = TestAppContext::single();
        let (entity, cx) = app.add_window_view(|window, cx| {
            let mut setup = Onboarding::init(window, cx);
            setup.form.choose(Provider::Custom);
            setup.form.edit_servers();
            for service in Service::ALL {
                setup
                    .form
                    .set_text(Field::Host(service), "127.0.0.1".into());
            }
            setup.select(Control::Field(Field::Security(Service::Smtp)));
            setup
        });
        for width in [360., 1180.] {
            cx.simulate_resize(size(px(width), px(760.)));
            for half_px in 24..=36 {
                let font_size = half_px as f32 / 2.;
                let mut previous = None;
                for _ in 0..3 {
                    cx.update(|_, cx| {
                        entity.update(cx, |setup, cx| {
                            setup.theme.ui_px = font_size;
                            for service in Service::ALL {
                                setup.form.cycle_security(service, false);
                            }
                            cx.notify();
                        });
                    });
                    cx.run_until_parked();
                    let fields = [
                        ["Imap-security", "Imap-security-value", "Imap-security-icon"],
                        ["Smtp-security", "Smtp-security-value", "Smtp-security-icon"],
                    ]
                    .map(|selectors| {
                        let [field, label, icon] =
                            selectors.map(|key| cx.debug_bounds(key).unwrap());
                        assert!(field.right() <= px(width - 20.));
                        assert!(label.left() > field.left());
                        assert!(label.right() < icon.left());
                        assert!(
                            icon.right() < field.right(),
                            "{width}px window, {font_size}px UI font"
                        );
                        field
                    });
                    assert_eq!(fields[0].left(), fields[1].left());
                    assert_eq!(fields[0].size.width, fields[1].size.width);
                    if let Some(previous) = previous {
                        assert_eq!(fields, previous);
                    }
                    previous = Some(fields);
                }
            }
        }
    }

    #[test]
    fn clicking_an_unfocused_field_inserts_text_at_the_click() {
        let mut app = TestAppContext::single();
        let (entity, cx) = app.add_window_view(|window, cx| {
            let mut setup = Onboarding::init(window, cx);
            setup.form.choose(Provider::Bridge);
            setup
                .form
                .set_text(Field::Email, "reader@example.test".into());
            setup.select(Control::Field(Field::Name));
            setup
        });
        let position = cx.update(|_, cx| {
            let layout = &entity.read(cx).painted_fields[&Field::Email];
            point(
                layout.origin.x + layout.line.x_for_index(6),
                layout.origin.y + px(4.),
            )
        });
        cx.simulate_click(position, Modifiers::default());
        cx.update(|window, cx| {
            entity.update(cx, |setup, cx| {
                assert_eq!(setup.control, Control::Field(Field::Email));
                setup.replace_text_in_range(None, "+tag", window, cx);
                assert_eq!(setup.form.text(Field::Email), "reader+tag@example.test");
            });
        });
    }

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
            assert!(!setup.apply_theme(theme.clone(), setup.motion));
            assert!(setup.transition.is_some());
            assert!(tick_feedback(setup));

            theme.accent ^= 1;
            assert!(setup.apply_theme(theme, setup.motion));
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
