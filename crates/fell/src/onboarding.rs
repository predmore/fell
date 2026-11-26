//! The first-run window. Login checks and account writes run on workers.

mod form;
mod input;
mod layout;

use std::collections::HashMap;
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use fell_core::credentials::SecretService;
use fell_core::onboarding::{ConnectionReport, check_connection};
use fell_core::{AccountConfig, BackendKind, SyncFailureKind, classify_sync_error};
use futures::StreamExt;
use futures::channel::mpsc;
use gpui::{Context, FocusHandle, KeyDownEvent, Window};

use crate::mailbox;
use crate::motion::{self, LayerSwap, Motion};
use crate::setup::{connection_check_config, save_account};
use crate::theme::{self, Theme};

use self::form::{Check, Field, Form, Provider, Service, Step, Storage};
use self::input::{Editor, FieldLayout};

const STEP_OUT: Duration = Duration::from_millis(60);
const STEP_IN: Duration = Duration::from_millis(140);
const STEP_OFFSET: f32 = 8.;

const BRIDGE_URL: &str = "https://proton.me/mail/bridge";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Control {
    Provider(Provider),
    Install,
    Field(Field),
    Servers,
    StartTls(Service),
    Next,
    Back,
    Inline,
    Reconnect,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Bridge {
    #[default]
    Checking,
    Listening,
    Offline,
}

struct FinishedCheck {
    attempt: u64,
    config: AccountConfig,
    result: fell_core::Result<ConnectionReport>,
    retained_mailbox: Option<String>,
}

pub(crate) struct Onboarding {
    form: Form,
    focus: FocusHandle,
    control: Control,
    editor: Editor,
    painted_fields: HashMap<Field, FieldLayout>,
    theme: Theme,
    motion: Motion,
    clock: motion::Clock,
    transition: Option<(Step, LayerSwap)>,
    feedback: layout::Feedback,
    bridge: Bridge,
    check_started: Instant,
    checked: Option<FinishedCheck>,
    return_to_inbox: Option<gpui::AnyWindowHandle>,
    reconnect: bool,
}

impl Onboarding {
    pub(crate) fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let view = Self::init(window, cx);
        view.watch_bridge(cx);
        theme::follow(cx, |view, next, motion, cx| {
            if view.apply_theme(next, motion) {
                cx.notify();
            }
        });
        view
    }

    pub(crate) fn from_inbox(
        inbox: gpui::AnyWindowHandle,
        previous: Option<AccountConfig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::new(window, cx);
        if let Some(config) = previous {
            view.form = Form::editing(config);
            view.select(Control::Field(Field::Username));
        }
        view.return_to_inbox = Some(inbox);
        view
    }

    fn apply_theme(&mut self, next: Theme, motion: Motion) -> bool {
        if self.theme == next && self.motion.enabled() == motion.enabled() {
            return false;
        }
        self.theme = next;
        self.motion = motion;
        self.transition = None;
        self.feedback.settle();
        true
    }

    fn init(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        window.focus(&focus);
        Self {
            form: Form::default(),
            focus,
            control: Control::Provider(Provider::Bridge),
            editor: Editor::default(),
            painted_fields: HashMap::new(),
            theme: theme::load(),
            motion: Motion::from_env(),
            clock: motion::Clock::default(),
            transition: None,
            feedback: layout::Feedback::default(),
            bridge: Bridge::Checking,
            check_started: Instant::now(),
            checked: None,
            return_to_inbox: None,
            reconnect: false,
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
                let mut controls = vec![
                    Control::Provider(Provider::Bridge),
                    Control::Provider(Provider::Custom),
                ];
                if self.bridge == Bridge::Offline {
                    controls.insert(1, Control::Install);
                }
                controls
            }
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
                    let fields = [
                        Input(Field::Host(service)),
                        Input(Field::Port(service)),
                        Input(Field::Security(service)),
                    ];
                    fields.into_iter().chain(
                        self.form
                            .offers_starttls(service)
                            .then_some(Control::StartTls(service)),
                    )
                })
                .chain([Next, Back])
                .collect(),
            Step::Connection if self.form.failed_check => vec![Back, Next],
            Step::Connection if self.form.can_retry() => vec![Next, Back, Control::Install],
            Step::Connection if self.form.checks == [Check::Failed; 2] => vec![Back],
            Step::Connection => vec![Next, Back],
            Step::Done => match self.form.storage {
                Storage::Saving { .. } => vec![Next],
                _ if self.form.storage.must_reopen() => vec![Next],
                Storage::ConfirmInline(_) => vec![Back, Next],
                _ if self.form.inline_available() => vec![Next, Inline, Back],
                Storage::Keyring if self.form.retained_mailbox.is_some() => {
                    vec![Control::Reconnect, Next, Back]
                }
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
            self.transition =
                LayerSwap::start(self.motion, now, STEP_OUT, STEP_IN).map(|swap| (before, swap));
            self.editor.reset(0);
        }
        self.select(control);
        cx.notify();
    }

    fn back(&mut self, cx: &mut Context<Self>) {
        let before = self.form.step;
        if before == Step::Connection
            && self.form.failed_check
            && let Some(issue) = self.form.issues.first()
        {
            self.form.step = issue.field.step();
            self.change_step(before, Control::Field(issue.field), cx);
            return;
        }
        let confirming = matches!(self.form.storage, Storage::ConfirmInline(_));
        self.form.back();
        let control = match (before, self.form.step) {
            (_, Step::Provider) => Control::Provider(self.form.provider),
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
            Control::Install => cx.open_url(BRIDGE_URL),
            Control::StartTls(service) => {
                self.form.use_starttls(service);
                self.select(Control::Field(Field::Security(service)));
                cx.notify();
            }
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
                Step::Connection if self.form.can_retry() && !self.form.failed_check => {
                    self.check(window, cx)
                }
                Step::Connection if self.form.connected() || self.form.failed_check => {
                    self.form.done();
                    self.feedback.begin_save();
                    let control = if self.form.replacement() || self.form.failed_check {
                        Control::Back
                    } else if self.form.retained_mailbox.is_some() {
                        Control::Reconnect
                    } else {
                        Control::Next
                    };
                    self.change_step(before, control, cx);
                }
                Step::Done if self.form.storage.must_reopen() => cx.quit(),
                Step::Done => {
                    if matches!(self.form.storage, Storage::Keyring) {
                        self.reconnect = false;
                    }
                    self.save(window, cx);
                }
                _ => {}
            },
            Control::Back if before == Step::Identity && self.form.previous.is_some() => {
                window.remove_window();
            }
            Control::Back => self.back(cx),
            Control::Inline if self.form.inline_available() => {
                self.form.storage.confirm_inline();
                self.select(Control::Back);
                cx.notify();
            }
            Control::Inline => {}
            Control::Reconnect
                if matches!(self.form.storage, Storage::Keyring)
                    && self.form.retained_mailbox.is_some() =>
            {
                self.reconnect = true;
                self.save(window, cx);
            }
            Control::Reconnect => {}
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
        self.checked = None;
        if before != Step::Connection {
            self.feedback.begin_check(&self.form);
        }
        self.change_step(before, Control::Next, cx);
        let attempt = self.form.attempt;
        let previous = self.form.previous.clone();
        let (progress, mut events) = mpsc::unbounded();
        let work = cx.background_executor().spawn(async move {
            let result = connection_check_config(&config, previous.as_ref(), &SecretService)
                .and_then(|checked| {
                    check_connection(&checked, |event| {
                        let _ = progress.unbounded_send(Check::update(event));
                    })
                });
            let retained_mailbox = previous
                .is_none()
                .then(|| {
                    result
                        .as_ref()
                        .ok()
                        .filter(|report| report.errors().next().is_none())
                        .and_then(|_| mailbox::retained_mailbox(&config))
                })
                .flatten();
            (config, result, retained_mailbox)
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
            let (config, result, retained_mailbox) = work.await;
            let _ = this.update(cx, |view, cx| {
                if !view.form.accepts_check(attempt) {
                    return;
                }
                let returning_to_fields = result.as_ref().is_ok_and(|report| {
                    report.errors().any(|error| {
                        config.backend != BackendKind::Bridge
                            || classify_sync_error(error) != SyncFailureKind::Unreachable
                    })
                });
                let check = FinishedCheck {
                    attempt,
                    config,
                    result,
                    retained_mailbox,
                };
                if view.form.previous.is_none()
                    && returning_to_fields
                    && (view.motion.enabled() || view.form.checks == [Check::Failed; 2])
                {
                    // Paint both final sign-in results before the test page
                    // gives way to the field that needs attention.
                    view.checked = Some(check);
                    cx.notify();
                } else {
                    view.finish_check(check, cx);
                }
            });
        })
        .detach();
    }

    fn finish_check(&mut self, check: FinishedCheck, cx: &mut Context<Self>) {
        if self.form.accepts_check(check.attempt) {
            self.form.retained_mailbox = check.retained_mailbox;
            let control = self
                .form
                .finish_check(check.attempt, &check.config, check.result)
                .map_or_else(
                    || {
                        if self.form.failed_check {
                            Control::Back
                        } else {
                            Control::Next
                        }
                    },
                    Control::Field,
                );
            self.change_step(Step::Connection, control, cx);
        }
    }

    fn save(&mut self, window: &Window, cx: &mut Context<Self>) {
        let Some((mut config, inline)) = self.form.begin_save() else {
            return;
        };
        self.select(Control::Next);
        cx.notify();
        let reconnect = self
            .reconnect
            .then(|| self.form.retained_mailbox.clone())
            .flatten();
        let previous = self.form.previous.clone();
        let work = cx.background_executor().spawn(async move {
            let save = if let Some(previous) = &previous {
                if previous.same_mailbox(&config) {
                    mailbox::AccountSave::Update
                } else {
                    mailbox::AccountSave::ReplaceMailbox
                }
            } else if let Some(id) = reconnect {
                config.id = id;
                mailbox::AccountSave::Reconnect
            } else {
                config.id = mailbox::next_account_id()
                    .map_err(|error| crate::setup::SaveAccountError::File(error.into()))?;
                mailbox::AccountSave::New
            };
            save_account(
                config,
                inline,
                previous.as_ref(),
                &SecretService,
                |config| mailbox::stage_connection_config(config, save, previous.as_ref()),
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = work.await;
            let _ = this.update_in(cx, |view, window, cx| match result {
                Ok(_) => {
                    let setup = cx.entity();
                    let previous = view.return_to_inbox.take();
                    window.defer(cx, move |window, cx| {
                        if let Some(previous) = previous {
                            let _ = previous.update(cx, |_, window, _| window.activate_window());
                            window.remove_window();
                        } else {
                            crate::view::finish_onboarding(setup, window, cx);
                        }
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
                let offset = if key.modifiers.shift {
                    controls.len() - 1
                } else {
                    1
                };
                let index = controls
                    .iter()
                    .position(|control| *control == self.control)
                    .map_or_else(
                        || {
                            if key.modifiers.shift {
                                controls.len() - 1
                            } else {
                                0
                            }
                        },
                        |current| (current + offset) % controls.len(),
                    );
                self.select(controls[index]);
                cx.notify();
            }
            "escape" if self.form.step == Step::Provider && self.return_to_inbox.is_some() => {
                window.remove_window();
            }
            "escape" if self.form.step == Step::Identity && self.form.previous.is_some() => {
                window.remove_window();
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
