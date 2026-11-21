use std::time::Instant;

use fell_core::ImapSecurity;
use gpui::{
    AnyElement, Context, CursorStyle, Div, FontWeight, KeyDownEvent, MouseButton, Render,
    SharedString, Stateful, Window, div, prelude::*, px, rgb,
};

use crate::setup::SaveAccountError;

use super::form::{Check, Issue};
use super::{Bridge, Control, Field, Onboarding, Provider, Service, Step, Storage};

type Footer = Option<AnyElement>;

impl Render for Onboarding {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let transition = self.transition.and_then(|(old, transition)| {
            let now = self.clock.frame(Instant::now());
            transition
                .running(now)
                .then(|| (old, transition.progress(now)))
        });
        if transition.is_some() {
            window.request_animation_frame();
        } else {
            self.transition = None;
            self.clock.idle();
        }
        let theme = &self.theme;
        let side = if window.viewport_size().width < px(540.) {
            24.
        } else {
            44.
        };
        let top = (f32::from(window.viewport_size().height) * 0.08).clamp(23., 68.);
        div()
            .id("onboarding")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|view, event: &KeyDownEvent, window, cx| {
                view.key_down(event, window, cx)
            }))
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(theme.background))
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
                            .when_some(transition, |el, (old, progress)| {
                                el.child(
                                    div()
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .w_full()
                                        .opacity(1. - progress)
                                        .child(self.step(old, false, cx)),
                                )
                            })
                            .child(
                                div()
                                    .w_full()
                                    .opacity(transition.map_or(1., |(_, progress)| progress))
                                    .child(self.step(self.form.step, true, cx)),
                            ),
                    ),
            )
    }
}

impl Onboarding {
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
                    .flex()
                    .justify_between()
                    .items_center()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Account setup"),
                    )
                    .child(
                        div()
                            .text_color(rgb(theme.dimmed))
                            .child(format!("Step {step} of 4")),
                    ),
            )
            .child(div().flex().gap_1p5().children((1..=4).map(|index| {
                let color = if index == step {
                    theme.accent
                } else if index < step {
                    crate::motion::mix(theme.border, theme.accent, 0.52)
                } else {
                    theme.border
                };
                div().flex_1().h(px(2.)).rounded_full().bg(rgb(color))
            })))
    }

    fn step(&self, step: Step, live: bool, cx: &Context<Self>) -> Div {
        let column = || div().flex().flex_col().gap_2();
        let (title, detail, content, footer): (&str, &str, Div, [Footer; 3]) = match step {
            Step::Provider => {
                let (status, color) = match self.bridge {
                    Bridge::Checking => ("Checking 127.0.0.1:1143…", self.theme.dimmed),
                    Bridge::Listening => {
                        ("Bridge is listening on 127.0.0.1:1143", self.theme.accent)
                    }
                    Bridge::Offline => {
                        ("Bridge isn’t listening on 127.0.0.1:1143", self.theme.error)
                    }
                };
                (
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
                        ))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap_x_3()
                                .gap_y_0p5()
                                .mt_2()
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap_2()
                                        .child(div().size(px(5.)).rounded_full().bg(rgb(color)))
                                        .child(self.meta(status)),
                                )
                                .child(self.button(
                                    Control::Install,
                                    "Install Bridge ↗",
                                    false,
                                    live,
                                    cx,
                                )),
                        ),
                    [None, None, None],
                )
            }
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
                (
                    if passed {
                        "Connection verified"
                    } else {
                        "Testing connection"
                    },
                    "Fell checks both sign-ins without sending or downloading mail.",
                    column().children(
                        Service::ALL
                            .into_iter()
                            .map(|service| self.check_row(service)),
                    ),
                    self.footer("Back", None, passed.then_some("Continue"), live, cx),
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
                        .when(saving, |el| {
                            el.child(self.meta(
                                "Saving your account… Approve any keyring prompt to continue.",
                            ))
                        })
                        .when_some(
                            match &self.form.storage {
                                Storage::Failed(error) => Some(error),
                                _ => None,
                            },
                            |el, error| {
                                let (title, detail) = match error {
                                    SaveAccountError::Keyring { detail, recovery } => {
                                        (detail.as_str(), *recovery)
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
                        None,
                        None,
                        Some(self.disabled_button("Saving…").into_any_element()),
                    ]
                } else if inline {
                    self.footer("Use keyring", None, Some("Save in file"), live, cx)
                } else {
                    let fallback = self
                        .form
                        .storage
                        .inline_available()
                        .then_some((Control::Inline, "Save in file…"));
                    let primary = if fallback.is_some() {
                        "Retry keyring"
                    } else {
                        "Save and open inbox"
                    };
                    self.footer("Back", fallback, Some(primary), live, cx)
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
                let [back, secondary, primary] = footer;
                el.child(
                    div()
                        .flex()
                        .flex_wrap()
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
            .gap_3()
            .px_4()
            .py_2p5()
            .border_1()
            .rounded_lg()
            .border_color(rgb(if focused {
                self.theme.accent
            } else {
                self.theme.border
            }))
            .bg(rgb(self.theme.surface))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(title))
                    .child(self.meta(detail)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(rgb(if focused {
                        self.theme.accent
                    } else {
                        self.theme.dimmed
                    }))
                    .child("›"),
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
                el.bg(rgb(theme.selection))
                    .text_color(rgb(theme.on_selection))
                    .font_weight(FontWeight::SEMIBOLD)
            })
            .when(!primary && control == Control::Install, |el| {
                el.text_color(rgb(theme.foreground))
            })
            .border_color(if focused {
                rgb(theme.accent).into()
            } else if primary {
                rgb(theme.border).into()
            } else {
                gpui::transparent_black()
            })
            .child(label.to_owned())
    }

    fn disabled_button(&self, label: &str) -> Div {
        div()
            .px_3()
            .py_1p5()
            .rounded_md()
            .text_color(rgb(self.theme.dimmed))
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
                    .overflow_hidden()
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
                    .child(self.field_line(field, placeholder.into(), live, cx)),
            )
    }

    fn field_border(&self, field: Field, focus: bool) -> u32 {
        if focus {
            self.theme.accent
        } else if self.form.issues.iter().any(|issue| issue.field == field) {
            self.theme.error
        } else {
            self.theme.border
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

    fn check_row(&self, service: Service) -> Div {
        let (status, color) = match self.form.checks[service as usize] {
            Check::Waiting => ("Waiting", self.theme.dimmed),
            Check::Testing => ("Signing in…", self.theme.accent),
            Check::Connected => ("Connected", self.theme.accent),
            Check::Failed => ("Failed", self.theme.error),
        };
        div()
            .px_4()
            .py_3()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .bg(rgb(self.theme.surface))
            .border_1()
            .border_color(rgb(self.theme.border))
            .rounded_lg()
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
                    .child(div().size(px(5.)).rounded_full().bg(rgb(color)))
                    .child(self.meta(status)),
            )
    }
}
