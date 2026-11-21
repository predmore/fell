use std::time::Instant;

use fell_core::ImapSecurity;
use fell_core::credentials::KeyringError;
use gpui::{
    AnyElement, Context, CursorStyle, Div, FontWeight, KeyDownEvent, MouseButton, Render,
    SharedString, Stateful, Window, div, prelude::*, px, rgb,
};

use crate::motion::{self, ARRIVE_FROM};
use crate::setup::SaveAccountError;

use super::form::{Check, Issue};
use super::{Bridge, Control, Field, Onboarding, Provider, Service, Step, Storage};

type Footer = Option<AnyElement>;

const SAVE_WIDTH: f32 = 164.;

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
        if let Some((old, outgoing, incoming)) = self.transition
            && incoming.running(now)
        {
            let progress = incoming.progress(now);
            let leaving = outgoing.remaining(now);
            // Let the old step dim below the arrival floor before painting
            // the incoming text. The outgoing layer only paints.
            arrival = if leaving >= ARRIVE_FROM {
                0.
            } else {
                motion::lerp(ARRIVE_FROM, 1., progress)
            };
            slide = motion::lerp(super::STEP_OFFSET, 0., progress);
            if outgoing.running(now) {
                old_layer = Some((old, leaving));
            }
            window.request_animation_frame();
        } else {
            self.transition = None;
        }
        let pulsing = self.motion.enabled()
            && self.form.step == Step::Connection
            && self.form.checks.contains(&Check::Testing);
        let pulse = if pulsing {
            window.request_animation_frame();
            let phase =
                now.duration_since(self.check_started).as_secs_f32() * std::f32::consts::TAU;
            motion::lerp(1., 0.35, (1. - phase.cos()) * 0.5)
        } else {
            1.
        };
        if self.transition.is_none() && !pulsing {
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
                let password_note = self
                    .form
                    .issues
                    .iter()
                    .find(|issue| issue.field == Field::Password)
                    .map_or_else(
                        || self.meta(self.form.provider.password_hint()),
                        |issue| self.field_issue(issue),
                    );
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
            Step::Connection => {
                let passed = self.form.connected();
                let retry = self.form.can_retry();
                let (title, detail, primary) = if passed {
                    (
                        "Connection verified",
                        "Both sign-ins work. Nothing was sent or downloaded.",
                        Some("Continue"),
                    )
                } else if retry {
                    (
                        "Connection test failed",
                        "Check Bridge, then test both sign-ins again.",
                        Some("Try again"),
                    )
                } else {
                    (
                        "Testing connection",
                        "Fell checks both sign-ins without sending or downloading mail.",
                        None,
                    )
                };
                (
                    title,
                    detail,
                    column().children(
                        Service::ALL
                            .into_iter()
                            .map(|service| self.check_row(service, pulse)),
                    ),
                    self.footer("Back", None, primary, live, cx),
                )
            }
            Step::Done => {
                let saving = matches!(self.form.storage, Storage::Saving { .. });
                let inline = matches!(
                    self.form.storage,
                    Storage::ConfirmInline(_) | Storage::Saving { inline: true }
                );
                let body = if inline {
                    "Your password will be saved as plain text in account.toml. Fell restricts access to the file to your user account."
                } else if self.form.storage.inline_available() {
                    "Fell couldn't store the password in your keyring."
                } else {
                    "Fell stores the password in your desktop keyring and uses it to sync mail."
                };
                let content =
                    column()
                        .child(
                            div()
                                .border_1()
                                .rounded_lg()
                                .border_color(rgb(self.theme.border))
                                .bg(rgb(self.theme.surface))
                                .p_4()
                                .flex()
                                .flex_col()
                                .gap_2()
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .truncate()
                                        .child(self.form.text(Field::Email).trim().to_owned()),
                                )
                                .child(self.meta(body)),
                        )
                        .when(saving && !inline, |el| {
                            el.child(self.meta("Approve any keyring prompt to continue."))
                        })
                        .when_some(
                            match &self.form.storage {
                                Storage::Failed(error) => Some(error),
                                _ => None,
                            },
                            |el, error| {
                                let (title, detail) = match error {
                                    SaveAccountError::Keyring { kind: Some(KeyringError::Unavailable { .. }), .. } => (
                                        "Couldn't reach your keyring",
                                        Some("Start GNOME Keyring or KWallet, then retry. Or save the password in a file."),
                                    ),
                                    SaveAccountError::Keyring { kind: Some(KeyringError::Locked), recovery, .. } => {
                                        ("Keyring is locked", *recovery)
                                    }
                                    SaveAccountError::Keyring { recovery, .. } => {
                                        ("Couldn't save to your keyring", *recovery)
                                    }
                                    SaveAccountError::File(detail) => {
                                        ("Couldn’t save account.toml", Some(detail.as_str()))
                                    }
                                };
                                el.child(self.error(title, detail))
                            },
                        );
                let title = if inline {
                    "Save password in file"
                } else {
                    "Save your account"
                };
                let detail = if inline {
                    "Choose where Fell keeps your mail password."
                } else {
                    "Your account is ready to sync."
                };
                let footer = if saving {
                    [
                        Some(
                            self.button(
                                Control::Back,
                                if inline { "Use keyring" } else { "Back" },
                                false,
                                false,
                                cx,
                            )
                            .text_color(rgb(self.theme.dimmed))
                            .into_any_element(),
                        ),
                        None,
                        Some(
                            self.button(Control::Next, "Saving…", true, false, cx)
                                .w(px(SAVE_WIDTH))
                                .text_center()
                                .into_any_element(),
                        ),
                    ]
                } else if inline {
                    self.save_footer("Use keyring", None, "Save in file", live, cx)
                } else {
                    let fallback = self
                        .form
                        .storage
                        .inline_available()
                        .then_some((Control::Inline, "Save in file…"));
                    let primary = if fallback.is_some() {
                        "Retry keyring"
                    } else {
                        "Save account"
                    };
                    self.save_footer("Back", fallback, primary, live, cx)
                };
                (title, detail, content, footer)
            }
        };
        div()
            .flex()
            .flex_col()
            .min_h(px(482.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .mb_5()
                    .child(
                        div()
                            .text_size(px(self.theme.ui_px + 15.))
                            .line_height(px(self.theme.ui_px + 21.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title.to_owned()),
                    )
                    .child(self.meta(detail.to_owned())),
            )
            .child(div().flex_1().child(content))
            .when(footer.iter().any(Option::is_some), |el| {
                let [back, mut secondary, primary] = footer;
                let alt = (narrow && secondary.is_some())
                    .then(|| div().flex().justify_end().children(secondary.take()));
                el.children(alt).child(
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
            })
    }

    fn save_footer(
        &self,
        back: &str,
        secondary: Option<(Control, &str)>,
        primary: &str,
        live: bool,
        cx: &Context<Self>,
    ) -> [Footer; 3] {
        let [back, secondary, _] = self.footer(back, secondary, None, live, cx);
        [
            back,
            secondary,
            Some(
                self.button(Control::Next, primary, true, live, cx)
                    .w(px(SAVE_WIDTH))
                    .text_center()
                    .into_any_element(),
            ),
        ]
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
                        el.child(self.bridge_status(live, cx))
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

    fn bridge_status(&self, live: bool, cx: &Context<Self>) -> Div {
        let (text, marker, color) = match self.bridge {
            Bridge::Checking => ("Checking Bridge…", "•", self.theme.dimmed),
            Bridge::Listening => ("Bridge is running", "✓", self.theme.accent),
            Bridge::Offline => (
                "Bridge isn't running. Open it and sign in, or",
                "•",
                self.theme.dimmed,
            ),
        };
        let link_focused = live && self.control == Control::Install;
        div()
            .mt_1()
            .flex()
            .items_start()
            .gap_1p5()
            .child(div().flex_none().text_color(rgb(color)).child(marker))
            .child(
                div()
                    .min_w_0()
                    .flex_1()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_1()
                    .child(self.meta(text).min_w_0().max_w_full())
                    .when(self.bridge == Bridge::Offline, |el| {
                        el.child(
                            self.clickable(Control::Install, "Install Bridge ↗", live, cx)
                                .text_size(px(self.theme.ui_px - 1.))
                                .line_height(px(self.theme.ui_px + 4.))
                                .text_color(rgb(self.theme.accent))
                                .rounded_sm()
                                .when(link_focused, |link| {
                                    link.relative().child(focus_ring(self.theme.accent, 5.))
                                })
                                .child("Install Bridge ↗"),
                        )
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
            .id(SharedString::from(label.to_owned()))
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
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(self.label(label))
                    .when_some(
                        self.form
                            .issues
                            .iter()
                            .find(|issue| field == Field::Email && issue.field == field),
                        |el, issue| el.child(self.field_issue(issue)),
                    ),
            )
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
                    .when(focus, |el| {
                        el.relative().child(focus_ring(self.theme.accent, 7.))
                    })
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
    }

    fn field_border(&self, field: Field, focus: bool) -> u32 {
        if self.form.issues.iter().any(|issue| issue.field == field) {
            self.theme.error
        } else if focus {
            self.theme.accent
        } else {
            self.theme.input_border
        }
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
        let security = match self.form.security(service) {
            ImapSecurity::StartTls => "STARTTLS",
            ImapSecurity::Tls => "TLS",
            ImapSecurity::Plain => "Plain",
        };
        let fields = [Field::Host(service), Field::Port(service), field];
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
                    .gap_2()
                    .child(
                        self.field_box(Field::Host(service), "Host", "mail.example.com", live, cx)
                            .flex_1(),
                    )
                    .child(
                        self.field_box(Field::Port(service), "Port", "", live, cx)
                            .w(px(62.)),
                    )
                    .child(
                        div()
                            .w(px(104.))
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
                                    .border_color(rgb(self.field_border(field, focused)))
                                    .bg(rgb(self.theme.surface))
                                    .when(focused, |el| {
                                        el.relative().child(focus_ring(self.theme.accent, 7.))
                                    })
                                    .child(
                                        div().text_size(px(self.theme.ui_px - 1.)).child(security),
                                    )
                                    .child(self.meta("↻")),
                            ),
                    ),
            )
            .child(
                div().min_h(px((self.theme.ui_px + 4.) * 3.)).children(
                    self.form
                        .issues
                        .iter()
                        .filter(|issue| fields.contains(&issue.field))
                        .map(|issue| self.field_issue(issue)),
                ),
            )
    }

    fn check_row(&self, service: Service, pulse: f32) -> Div {
        let (status, marker, color) = match self.form.checks[service as usize] {
            Check::Waiting => ("Waiting", "•", self.theme.dimmed),
            Check::Testing => ("Signing in…", "•", self.theme.accent),
            Check::Connected => ("Connected", "✓", self.theme.accent),
            Check::Failed => ("Failed", "✕", self.theme.error),
        };
        div()
            .px_4()
            .py_3()
            .flex()
            .flex_col()
            .gap_2()
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
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .text_color(rgb(color))
                                    .opacity(
                                        if self.form.checks[service as usize] == Check::Testing {
                                            pulse
                                        } else {
                                            1.
                                        },
                                    )
                                    .child(marker),
                            )
                            .child(self.meta(status)),
                    ),
            )
            .when(
                self.form.bridge_unreachable[service as usize]
                    && (service == Service::Imap
                        || !self.form.bridge_unreachable[Service::Imap as usize]),
                |el| {
                    el.child(self.error(
                        "Bridge isn't running",
                        Some("Open Proton Mail Bridge and sign in, then try again."),
                    ))
                },
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
                            // Identity and Server settings share Back and Test connection.
                            setup.form.edit_servers();
                            setup.change_step(Step::Identity, Control::Back, cx);
                            assert!(setup.transition.is_some());
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
