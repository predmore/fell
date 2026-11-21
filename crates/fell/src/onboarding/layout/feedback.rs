use std::time::{Duration, Instant};

use fell_core::credentials::KeyringError;
use gpui::{Div, prelude::*};

use crate::mailbox::AccountFileError;
use crate::motion::{self, LayerSwap, Motion, SwapFrame, Transition};
use crate::setup::SaveAccountError;

use super::super::form::{Check, Field, Form, Issue, Service, Step, Storage};
use super::super::{Bridge, Control, STEP_OUT};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Connection {
    #[default]
    Testing,
    Verified,
    BridgeOffline,
    BothFailed,
}

impl Connection {
    pub const ALL: [Self; 4] = [
        Self::Testing,
        Self::Verified,
        Self::BridgeOffline,
        Self::BothFailed,
    ];

    pub fn heading(self) -> (&'static str, &'static str) {
        match self {
            Self::Testing => ("Testing connection", "Checking both sign-ins."),
            Self::Verified => ("Connection verified", "Both sign-ins work."),
            Self::BridgeOffline => ("Connection test failed", "Open Bridge and sign in."),
            Self::BothFailed => ("Connection test failed", "Both sign-ins failed."),
        }
    }
}

#[derive(Default, PartialEq, Eq)]
pub(super) enum SaveState {
    #[default]
    Ready,
    SavingKeyring,
    ConfirmFile,
    Failed {
        title: &'static str,
        detail: Option<String>,
        inline_available: bool,
    },
}

impl SaveState {
    const KEYRING: (&'static str, &'static str) =
        ("Save your account", "Store your password to sync mail.");
    const FILE: (&'static str, &'static str) = (
        "Save password in file",
        "Choose where to store your password.",
    );
    const FAILED: (&'static str, &'static str) =
        ("Save your account", "Choose where to store your password.");
    pub const HEADINGS: [(&'static str, &'static str); 3] =
        [Self::KEYRING, Self::FILE, Self::FAILED];

    fn from_storage(storage: &Storage) -> Self {
        match storage {
            Storage::Keyring => Self::Ready,
            Storage::Saving { inline: false } => Self::SavingKeyring,
            Storage::Saving { inline: true } | Storage::ConfirmInline(_) => Self::ConfirmFile,
            Storage::Failed(error) => {
                let (title, detail) = match error {
                    SaveAccountError::Keyring {
                        kind: Some(KeyringError::Unavailable { .. }),
                        ..
                    } => (
                        "Couldn't reach your keyring",
                        Some(
                            "Start GNOME Keyring or KWallet, then retry. You can also save the password in a file.",
                        ),
                    ),
                    SaveAccountError::Keyring {
                        kind: Some(KeyringError::Locked),
                        recovery,
                        ..
                    } => ("Keyring is locked", *recovery),
                    SaveAccountError::Keyring { recovery, .. } => {
                        ("Couldn't save to your keyring", *recovery)
                    }
                    SaveAccountError::File(AccountFileError::AlreadyConfigured) => (
                        "An account is already configured",
                        Some("Quit and reopen Fell to use it."),
                    ),
                    SaveAccountError::File(AccountFileError::Write(detail)) => {
                        ("Couldn’t save account.toml", Some(detail.as_str()))
                    }
                };
                Self::Failed {
                    title,
                    detail: detail.map(str::to_owned),
                    inline_available: storage.inline_available(),
                }
            }
        }
    }

    pub fn heading(&self) -> (&'static str, &'static str) {
        match self {
            Self::ConfirmFile => Self::FILE,
            Self::Failed { .. } => Self::FAILED,
            _ => Self::KEYRING,
        }
    }

    pub fn inline_available(&self) -> bool {
        matches!(
            self,
            Self::Failed {
                inline_available: true,
                ..
            }
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Action {
    pub control: Control,
    pub label: &'static str,
    pub enabled: bool,
}

impl Action {
    fn new(control: Control, label: &'static str) -> Self {
        Self {
            control,
            label,
            enabled: true,
        }
    }

    fn disabled(self) -> Self {
        Self {
            enabled: false,
            ..self
        }
    }
}

pub(super) type Actions = [Change<Option<Action>>; 3];

impl Connection {
    fn actions(self) -> [Option<Action>; 3] {
        let back = Some(Action::new(Control::Back, "Back"));
        let (secondary, primary) = match self {
            Self::Testing | Self::BothFailed => (None, None),
            Self::Verified => (None, Some("Continue")),
            Self::BridgeOffline => (
                Some(Action::new(Control::Install, "Install Bridge ↗")),
                Some("Try again"),
            ),
        };
        [
            back,
            secondary,
            primary.map(|label| Action::new(Control::Next, label)),
        ]
    }
}

fn save_actions(storage: &Storage) -> [Option<Action>; 3] {
    let back = Action::new(Control::Back, "Back");
    let use_keyring = Action::new(Control::Back, "Use keyring");
    let next = |label| Action::new(Control::Next, label);
    match storage {
        Storage::Saving { inline } => [
            Some(if *inline { use_keyring } else { back }),
            None,
            Some(next("Saving…").disabled()),
        ],
        Storage::ConfirmInline(_) => [Some(use_keyring), None, Some(next("Save in file"))],
        _ if storage.must_reopen() => [None, None, Some(next("Quit"))],
        _ if storage.inline_available() => [Some(back), None, Some(next("Retry keyring"))],
        _ => [Some(back), None, Some(next("Save account"))],
    }
}

#[derive(Default)]
pub(super) struct Change<T> {
    pub value: T,
    swap: Option<(T, LayerSwap, f32)>,
    frame: Option<SwapFrame>,
}

impl<T: PartialEq> Change<T> {
    fn replace(&mut self, value: T, motion: Motion, now: Instant) {
        self.update(value, motion, now, STEP_OUT, motion::FADE);
    }

    fn update(&mut self, value: T, motion: Motion, now: Instant, out: Duration, input: Duration) {
        if value == self.value {
            return;
        }
        let old = std::mem::replace(&mut self.value, value);
        if motion.enabled()
            && let Some((leaving, swap, _)) = &mut self.swap
            && matches!(swap.frame(now), Some(SwapFrame::Leaving(_)))
        {
            // Keep fading the text that's on screen. Intermediate values
            // that never painted can give way to the newest value.
            if *leaving != self.value {
                swap.set_arrival_duration(input);
                return;
            }
            self.settle();
        } else {
            let opacity = self
                .swap
                .as_ref()
                .and_then(|(_, swap, _)| swap.frame(now))
                .map_or(1., |frame| match frame {
                    SwapFrame::Leaving(_) => 0.,
                    SwapFrame::Arriving(progress) => progress,
                });
            self.swap = LayerSwap::start(motion, now, out, input).map(|swap| (old, swap, opacity));
        }
    }

    fn settle(&mut self) {
        self.swap = None;
        self.frame = None;
    }

    fn tick(&mut self, now: Instant) -> bool {
        self.frame = self.swap.as_ref().and_then(|(_, swap, _)| swap.frame(now));
        if self.frame.is_none() {
            self.swap = None;
        }
        self.frame.is_some()
    }

    pub fn paint(&self, live: bool, draw: impl Fn(&T, bool) -> Div) -> Div {
        match (&self.frame, &self.swap) {
            (Some(SwapFrame::Leaving(opacity)), Some((old, _, from))) => {
                draw(old, false).opacity(opacity * from)
            }
            (Some(SwapFrame::Arriving(progress)), _) => draw(&self.value, live).opacity(*progress),
            _ => draw(&self.value, live),
        }
    }
}

impl<T: PartialEq> Change<Option<T>> {
    fn update_optional(
        &mut self,
        value: Option<T>,
        has_helper: bool,
        motion: Motion,
        now: Instant,
    ) {
        let out = match (&self.value, &value, has_helper) {
            (Some(_), None, _) => motion::FADE,
            (None, _, false) => Duration::ZERO,
            _ => STEP_OUT,
        };
        let arrival = if value.is_some() || has_helper {
            motion::FADE
        } else {
            Duration::ZERO
        };
        self.update(value, motion, now, out, arrival);
    }

    pub fn clearing(&self) -> f32 {
        if self.value.is_none()
            && let Some(SwapFrame::Leaving(remaining)) = &self.frame
        {
            return *remaining;
        }
        0.
    }
}

#[derive(Default)]
pub(in crate::onboarding) struct Feedback {
    pub(super) bridge: Change<Bridge>,
    pub(super) connection: Change<Connection>,
    pub(super) checks: [Change<Check>; 2],
    pub(super) bridge_offline: [Change<bool>; 2],
    pub(super) connection_actions: Actions,
    pub(super) save_actions: Actions,
    pub(super) save_title: Change<&'static str>,
    pub(super) save_subtitle: Change<&'static str>,
    pub(super) save_note: Change<SaveState>,
    back_disabled: bool,
    back_tint: Option<(f32, Transition)>,
    pub(super) back_amount: f32,
    pub(super) email: Change<Option<Issue>>,
    pub(super) password: Change<Option<Issue>>,
    pub(super) servers: [[Change<Option<Issue>>; 3]; 2],
}

impl Feedback {
    pub fn settle(&mut self) {
        self.bridge.settle();
        self.connection.settle();
        for action in self
            .connection_actions
            .iter_mut()
            .chain(&mut self.save_actions)
        {
            action.settle();
        }
        self.save_title.settle();
        self.save_subtitle.settle();
        self.save_note.settle();
        self.back_tint = None;
        self.email.settle();
        self.password.settle();
        for check in &mut self.checks {
            check.settle();
        }
        for offline in &mut self.bridge_offline {
            offline.settle();
        }
        for issue in self.servers.iter_mut().flatten() {
            issue.settle();
        }
    }

    pub fn checks_settled(&self) -> bool {
        self.checks.iter().all(|check| check.frame.is_none())
    }

    pub fn begin_check(&mut self, form: &Form) {
        self.connection = Change::default();
        for (action, value) in self
            .connection_actions
            .iter_mut()
            .zip(Connection::Testing.actions())
        {
            *action = Change {
                value,
                ..Change::default()
            };
        }
        for (check, status) in self.checks.iter_mut().zip(form.checks) {
            *check = Change {
                value: status,
                ..Change::default()
            };
        }
        self.bridge_offline = Default::default();
    }

    pub fn begin_save(&mut self) {
        for (action, value) in self
            .save_actions
            .iter_mut()
            .zip(save_actions(&Storage::Keyring))
        {
            *action = Change {
                value,
                ..Change::default()
            };
        }
        let (title, subtitle) = SaveState::KEYRING;
        self.save_title = Change {
            value: title,
            ..Change::default()
        };
        self.save_subtitle = Change {
            value: subtitle,
            ..Change::default()
        };
        self.save_note = Change::default();
        self.back_disabled = false;
        self.back_tint = None;
        self.back_amount = 0.;
    }

    pub(super) fn issue(&self, field: Field) -> Option<&Change<Option<Issue>>> {
        match field {
            Field::Email => Some(&self.email),
            Field::Password => Some(&self.password),
            Field::Host(service) => Some(&self.servers[service as usize][0]),
            Field::Port(service) => Some(&self.servers[service as usize][1]),
            Field::Security(service) => Some(&self.servers[service as usize][2]),
            Field::Name | Field::Username => None,
        }
    }

    pub fn refresh(&mut self, form: &Form, bridge: Bridge, motion: Motion, now: Instant) -> bool {
        self.bridge.replace(bridge, motion, now);
        if form.step == Step::Connection {
            let state = if form.connected() {
                Connection::Verified
            } else if form.can_retry() {
                Connection::BridgeOffline
            } else if form.checks == [Check::Failed; 2] {
                Connection::BothFailed
            } else {
                Connection::Testing
            };
            self.connection.replace(state, motion, now);
            for (action, value) in self.connection_actions.iter_mut().zip(state.actions()) {
                action.update_optional(value, false, motion, now);
            }
            for (index, check) in self.checks.iter_mut().enumerate() {
                check.replace(form.checks[index], motion, now);
                let offline = form.bridge_unreachable[index];
                let (out, arrival) = if offline {
                    (Duration::ZERO, motion::FADE)
                } else {
                    (motion::FADE, Duration::ZERO)
                };
                self.bridge_offline[index].update(offline, motion, now, out, arrival);
            }
        }
        if form.step == Step::Done {
            let state = SaveState::from_storage(&form.storage);
            let (title, subtitle) = state.heading();
            self.save_title.replace(title, motion, now);
            self.save_subtitle.replace(subtitle, motion, now);
            self.save_note.replace(state, motion, now);
            for (action, value) in self
                .save_actions
                .iter_mut()
                .zip(save_actions(&form.storage))
            {
                action.update_optional(value, false, motion, now);
            }
            let disabled = matches!(form.storage, Storage::Saving { .. });
            if disabled != self.back_disabled {
                self.back_disabled = disabled;
                self.back_tint = motion
                    .start(motion::FADE, now)
                    .map(|fade| (self.back_amount, fade));
            }
        }
        if self.back_tint.is_some_and(|(_, fade)| !fade.running(now)) {
            self.back_tint = None;
        }
        let target = f32::from(u8::from(self.back_disabled));
        self.back_amount = self.back_tint.map_or(target, |(from, fade)| {
            motion::lerp(from, target, fade.progress(now))
        });
        let issue = |field| {
            form.issues
                .iter()
                .find(|issue| issue.field == field)
                .copied()
        };
        self.email
            .update_optional(issue(Field::Email), false, motion, now);
        self.password
            .update_optional(issue(Field::Password), true, motion, now);
        for service in Service::ALL {
            for (slot, field) in self.servers[service as usize].iter_mut().zip([
                Field::Host(service),
                Field::Port(service),
                Field::Security(service),
            ]) {
                let issue = issue(field).map(|mut issue| {
                    if issue.suggest_starttls {
                        issue.suggest_starttls = form.offers_starttls(service);
                    }
                    issue
                });
                slot.update_optional(issue, false, motion, now);
            }
        }

        let mut active = self.bridge.tick(now)
            | self.connection.tick(now)
            | self.save_title.tick(now)
            | self.save_subtitle.tick(now)
            | self.save_note.tick(now)
            | self.back_tint.is_some()
            | self.email.tick(now)
            | self.password.tick(now);
        for index in 0..self.checks.len() {
            active |= self.checks[index].tick(now) | self.bridge_offline[index].tick(now);
        }
        for issue in self.servers.iter_mut().flatten() {
            active |= issue.tick(now);
        }
        for action in self
            .connection_actions
            .iter_mut()
            .chain(&mut self.save_actions)
        {
            active |= action.tick(now);
        }
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaced_text_fades_in_only_after_the_old_text_leaves() {
        let now = Instant::now();
        let mut label = Change {
            value: "Signing in",
            ..Change::default()
        };
        label.replace("Connected", Motion::new(true), now);
        assert!(label.tick(now));
        assert!(matches!(label.frame, Some(SwapFrame::Leaving(1.0))));
        assert!(label.tick(now + STEP_OUT));
        assert!(matches!(label.frame, Some(SwapFrame::Arriving(0.0))));
        assert!(!label.tick(now + STEP_OUT + motion::FADE));
    }

    #[test]
    fn clearing_a_password_error_fades_it_before_returning_the_helper() {
        let now = Instant::now();
        let mut label = Change {
            value: Some(Issue {
                field: Field::Password,
                title: "Enter your mail password.",
                recovery: None,
                suggest_starttls: false,
            }),
            ..Change::default()
        };
        label.update_optional(None, true, Motion::new(true), now);
        assert!(label.tick(now + STEP_OUT));
        assert!(matches!(label.frame, Some(SwapFrame::Leaving(opacity)) if opacity > 0.15));
        assert!(label.tick(now + motion::FADE));
        assert!(matches!(label.frame, Some(SwapFrame::Arriving(0.0))));
    }
}
