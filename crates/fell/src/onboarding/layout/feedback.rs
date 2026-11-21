use std::time::Instant;

use fell_core::credentials::KeyringError;
use gpui::{Div, div, prelude::*};

use crate::motion::{self, ARRIVE_FROM, Motion, Transition};
use crate::setup::SaveAccountError;

use super::super::form::{Check, Form, Issue, Step, Storage};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum Connection {
    #[default]
    Testing,
    Verified,
    BridgeOffline,
}

impl Connection {
    pub const ALL: [Self; 3] = [Self::Testing, Self::Verified, Self::BridgeOffline];

    pub fn heading(self) -> (&'static str, &'static str) {
        match self {
            Self::Testing => ("Testing connection", "Checking both sign-ins."),
            Self::Verified => ("Connection verified", "Both sign-ins work."),
            Self::BridgeOffline => ("Connection test failed", "Open Bridge and sign in."),
        }
    }
}

#[derive(Default, PartialEq, Eq)]
pub(super) enum SaveState {
    #[default]
    Ready,
    SavingKeyring,
    ConfirmFile,
    SavingFile,
    Failed {
        title: &'static str,
        detail: Option<String>,
        can_save_file: bool,
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
            Storage::Saving { inline: true } => Self::SavingFile,
            Storage::ConfirmInline(_) => Self::ConfirmFile,
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
                    SaveAccountError::File(detail) => {
                        ("Couldn’t save account.toml", Some(detail.as_str()))
                    }
                };
                Self::Failed {
                    title,
                    detail: detail.map(str::to_owned),
                    can_save_file: storage.inline_available(),
                }
            }
        }
    }

    pub fn heading(&self) -> (&'static str, &'static str) {
        match self {
            Self::ConfirmFile | Self::SavingFile => Self::FILE,
            Self::Failed { .. } => Self::FAILED,
            _ => Self::KEYRING,
        }
    }
}

/// A slot's previous content is paint-only. The incoming content keeps its
/// normal layout and accepts input as soon as the form changes.
#[derive(Default)]
pub(super) struct Change<T> {
    pub value: T,
    leaving: Option<(T, Transition)>,
    progress: f32,
}

impl<T: PartialEq> Change<T> {
    fn update(&mut self, value: T, motion: Motion, now: Instant) {
        if value != self.value {
            let old = std::mem::replace(&mut self.value, value);
            self.leaving = motion.start(motion::FADE, now).map(|fade| (old, fade));
        }
    }

    fn tick(&mut self, now: Instant) -> bool {
        if let Some((_, fade)) = &self.leaving
            && fade.running(now)
        {
            self.progress = fade.progress(now);
            true
        } else {
            self.leaving = None;
            false
        }
    }

    pub fn arrival(&self) -> f32 {
        self.leaving
            .as_ref()
            .map_or(1., |_| motion::lerp(ARRIVE_FROM, 1., self.progress))
    }

    pub fn paint(&self, live: bool, draw: impl Fn(&T, bool) -> Div) -> Div {
        div()
            .relative()
            .when_some(self.leaving.as_ref(), |el, (old, _)| {
                el.child(
                    draw(old, false)
                        .absolute()
                        .inset_0()
                        .opacity(1. - self.progress),
                )
            })
            .child(draw(&self.value, live).opacity(self.arrival()))
    }
}

#[derive(Default)]
pub(in crate::onboarding) struct Feedback {
    pub(super) connection: Change<Connection>,
    pub(super) checks: [Change<(Check, bool)>; 2],
    pub(super) save: Change<SaveState>,
    pub(super) issues: Change<Vec<Issue>>,
}

impl Feedback {
    pub fn settle(&mut self) {
        self.connection.leaving = None;
        self.save.leaving = None;
        self.issues.leaving = None;
        for check in &mut self.checks {
            check.leaving = None;
        }
    }

    pub fn refresh(&mut self, form: &Form, motion: Motion, now: Instant) -> bool {
        if form.step == Step::Connection {
            let state = if form.connected() {
                Connection::Verified
            } else if form.can_retry() {
                Connection::BridgeOffline
            } else {
                Connection::Testing
            };
            self.connection.update(state, motion, now);
            for (index, check) in self.checks.iter_mut().enumerate() {
                check.update(
                    (form.checks[index], form.bridge_unreachable[index]),
                    motion,
                    now,
                );
            }
        }
        if form.step == Step::Done {
            self.save
                .update(SaveState::from_storage(&form.storage), motion, now);
        }
        if self.issues.value != form.issues {
            self.issues.update(form.issues.clone(), motion, now);
        }
        self.connection.tick(now)
            | self.save.tick(now)
            | self.issues.tick(now)
            | self.checks[0].tick(now)
            | self.checks[1].tick(now)
    }
}
