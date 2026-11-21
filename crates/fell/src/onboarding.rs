//! The first-run window. Login checks and account writes run on workers.

mod form;
mod input;
mod layout;

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use fell_core::credentials::SecretService;
use fell_core::onboarding::check_connection;
use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{Context, FocusHandle, KeyDownEvent, Window};

use crate::mailbox::stage_new_account_config;
use crate::motion::{self, Motion, Transition};
use crate::setup::save_account;
use crate::theme::{self, Theme};

use self::form::{Check, Field, Form, Provider, Service, Step, Storage};
use self::input::Editor;

const STEP_OUT: Duration = Duration::from_millis(60);
const STEP_IN: Duration = Duration::from_millis(140);
const STEP_IN_DELAY: Duration = Duration::from_millis(24);
const STEP_OFFSET: f32 = 8.;

const BRIDGE_URL: &str = "https://proton.me/mail/bridge";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Provider(Provider),
    Other,
    Install,
    Field(Field),
    Servers,
    Next,
    Back,
    Inline,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bridge {
    Checking,
    Listening,
    Offline,
}

pub(crate) struct Onboarding {
    form: Form,
    focus: FocusHandle,
    control: Control,
    editor: Editor,
    theme: Theme,
    motion: Motion,
    clock: motion::Clock,
    transition: Option<(Step, Transition, Transition)>,
    bridge: Bridge,
    check_started: Instant,
}

impl Onboarding {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = Self::init(window, cx);
        view.watch_bridge(cx);
        theme::follow(cx, |view, next, cx| {
            view.theme = next;
            view.motion = Motion::from_env();
            view.transition = None;
            cx.notify();
        });
        view
    }

    fn init(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        Self {
            form: Form::default(),
            focus,
            control: Control::Provider(Provider::Bridge),
            editor: Editor::default(),
            theme: theme::load(),
            motion: Motion::from_env(),
            clock: motion::Clock::default(),
            transition: None,
            bridge: Bridge::Checking,
            check_started: Instant::now(),
        }
    }

    fn watch_bridge(&self, cx: &mut Context<Self>) {
        cx.spawn(async |this, cx| {
            loop {
                let Ok(has_probe) = this.update(cx, |view, _| view.form.step == Step::Provider)
                else {
                    break;
                };
                if has_probe {
                    let status = cx
                        .background_executor()
                        .spawn(async {
                            let address = SocketAddr::from(([127, 0, 0, 1], 1143));
                            if TcpStream::connect_timeout(&address, Duration::from_millis(350))
                                .is_ok()
                            {
                                Bridge::Listening
                            } else {
                                Bridge::Offline
                            }
                        })
                        .await;
                    if this
                        .update(cx, |view, cx| {
                            if view.bridge != status {
                                view.bridge = status;
                                if status != Bridge::Offline && view.control == Control::Install {
                                    view.select(Control::Provider(Provider::Bridge));
                                }
                                cx.notify();
                            }
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
        .detach();
    }

    fn controls(&self) -> Vec<Control> {
        use Control::{Back, Field as Input, Inline, Next, Servers};
        match self.form.step {
            Step::Provider => {
                let mut controls = vec![Control::Provider(Provider::Bridge), Control::Other];
                if self.bridge == Bridge::Offline {
                    controls.insert(1, Control::Install);
                }
                controls
            }
            Step::Preset => Provider::PRESETS
                .into_iter()
                .map(Control::Provider)
                .chain([Back])
                .collect(),
            Step::Identity => {
                let mut controls = vec![
                    Input(Field::Email),
                    Input(Field::Name),
                    Input(Field::Username),
                    Input(Field::Password),
                ];
                controls.push(Next);
                if !self.form.needs_servers() {
                    controls.push(Servers);
                }
                controls.push(Back);
                controls
            }
            Step::Servers => Service::ALL
                .into_iter()
                .flat_map(|service| {
                    [
                        Input(Field::Host(service)),
                        Input(Field::Port(service)),
                        Input(Field::Security(service)),
                    ]
                })
                .chain([Next, Back])
                .collect(),
            Step::Connection if self.form.connected() || self.form.can_retry() => vec![Next, Back],
            Step::Connection => vec![Back],
            Step::Done => match self.form.storage {
                Storage::Saving { .. } => vec![Next],
                Storage::ConfirmInline(_) => vec![Back, Next],
                _ if self.form.storage.inline_available() => vec![Next, Inline, Back],
                _ => vec![Next, Back],
            },
        }
    }

    fn select(&mut self, control: Control) {
        if self.control == control {
            return;
        }
        self.control = control;
        self.editor.reset(
            self.text_field()
                .map_or(0, |field| self.form.text(field).len()),
        );
    }

    fn change_step(&mut self, before: Step, control: Control, cx: &mut Context<Self>) {
        if before != self.form.step {
            let now = self.clock.now();
            self.transition = self
                .motion
                .start(STEP_IN, now + STEP_IN_DELAY)
                .map(|incoming| (before, Transition::new(now, STEP_OUT), incoming));
            self.editor.reset(0);
        }
        self.select(control);
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        let before = self.form.step;
        let confirming = matches!(self.form.storage, Storage::ConfirmInline(_));
        self.form.back();
        let control = match (before, self.form.step) {
            (Step::Preset, Step::Provider) => Control::Other,
            (_, Step::Provider) => Control::Provider(Provider::Bridge),
            (_, Step::Preset) => Control::Provider(self.form.provider),
            (Step::Servers, Step::Identity) => Control::Next,
            (_, Step::Identity | Step::Servers | Step::Connection) => Control::Next,
            (_, Step::Done) if confirming => Control::Next,
            _ => return,
        };
        self.change_step(before, control, cx);
    }

    fn activate(&mut self, control: Control, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.form.step;
        match control {
            Control::Provider(provider) => {
                self.form.choose(provider);
                self.change_step(before, Control::Field(Field::Email), cx);
            }
            Control::Other => {
                self.form.step = Step::Preset;
                let first = if self.form.provider == Provider::Bridge {
                    Provider::Fastmail
                } else {
                    self.form.provider
                };
                self.change_step(before, Control::Provider(first), cx);
            }
            Control::Install => cx.open_url(BRIDGE_URL),
            Control::Field(Field::Security(service)) => {
                self.form.cycle_security(service, false);
                cx.notify();
            }
            Control::Field(Field::Password) => self.activate(Control::Next, window, cx),
            Control::Field(_) => self.advance_field(cx),
            Control::Servers => {
                self.form.edit_servers();
                self.change_step(before, Control::Field(Field::Host(Service::Imap)), cx);
            }
            Control::Next => match before {
                Step::Identity if self.form.needs_servers() => {
                    if self.form.validate_identity() {
                        self.form.edit_servers();
                        self.change_step(before, Control::Field(Field::Host(Service::Imap)), cx);
                    } else {
                        self.focus_issue(before, cx);
                    }
                }
                Step::Identity | Step::Servers => self.check(window, cx),
                Step::Connection if self.form.can_retry() => self.check(window, cx),
                Step::Connection if self.form.connected() => {
                    self.form.done();
                    self.change_step(before, Control::Next, cx);
                }
                Step::Done => self.save(window, cx),
                _ => {}
            },
            Control::Back => self.back(cx),
            Control::Inline if self.form.storage.inline_available() => {
                self.form.storage.confirm_inline();
                self.select(Control::Back);
                cx.notify();
            }
            Control::Inline => {}
        }
    }

    fn advance_field(&mut self, cx: &mut Context<Self>) {
        let controls = self.controls();
        if let Some(next) = controls
            .iter()
            .skip_while(|control| **control != self.control)
            .nth(1)
        {
            self.select(*next);
            cx.notify();
        }
    }

    fn focus_issue(&mut self, before: Step, cx: &mut Context<Self>) {
        if let Some(issue) = self
            .form
            .issues
            .iter()
            .find(|issue| issue.field.step() == self.form.step)
        {
            self.change_step(before, Control::Field(issue.field), cx);
        }
    }

    fn check(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let before = self.form.step;
        let Some(config) = self.form.begin_check() else {
            self.focus_issue(before, cx);
            return;
        };
        self.check_started = self.clock.now();
        self.change_step(before, Control::Back, cx);
        let attempt = self.form.attempt;
        let (progress, mut events) = mpsc::unbounded();
        let work = cx.background_executor().spawn(async move {
            let result = check_connection(&config, |event| {
                let _ = progress.unbounded_send(Check::update(event));
            });
            (config, result)
        });
        cx.spawn_in(window, async move |this, cx| {
            while let Some((service, status)) = events.next().await {
                let _ = this.update(cx, |view, cx| {
                    if view.form.accepts_check(attempt) {
                        view.form.checks[service as usize] = status;
                        cx.notify();
                    }
                });
            }
            let (config, result) = work.await;
            let _ = this.update(cx, |view, cx| {
                if view.form.accepts_check(attempt) {
                    let control = view
                        .form
                        .finish_check(attempt, &config, result)
                        .map_or(Control::Next, Control::Field);
                    view.change_step(Step::Connection, control, cx);
                }
            });
        })
        .detach();
    }

    fn save(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some((config, inline)) = self.form.begin_save() else {
            return;
        };
        self.select(Control::Next);
        cx.notify();
        let work = cx.background_executor().spawn(async move {
            save_account(config, inline, &SecretService, stage_new_account_config)
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(_) => {
                    let setup = cx.entity();
                    window.defer(cx, move |window, cx| {
                        crate::view::finish_onboarding(setup, window, cx)
                    });
                }
                Err(error) => {
                    view.form.storage = Storage::Failed(error);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        if self.transition.take().is_some() {
            cx.notify();
        }
        match key.key.as_str() {
            "tab" => {
                let controls = self.controls();
                let current = controls
                    .iter()
                    .position(|control| *control == self.control)
                    .unwrap_or(0);
                let offset = if key.modifiers.shift {
                    controls.len() - 1
                } else {
                    1
                };
                self.select(controls[(current + offset) % controls.len()]);
                cx.notify();
            }
            "escape" => self.back(cx),
            "enter" if !event.is_held => self.activate(self.control, window, cx),
            "space" if !event.is_held && self.text_field().is_none() => {
                self.activate(self.control, window, cx);
            }
            "left" | "right" if let Control::Field(Field::Security(service)) = self.control => {
                self.form.cycle_security(service, key.key == "left");
                cx.notify();
            }
            _ if self.edit_key(key, cx) => {}
            _ => return,
        }
        cx.stop_propagation();
    }
}
