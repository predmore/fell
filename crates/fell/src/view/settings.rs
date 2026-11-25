use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fell_core::BackendKind;
use fell_core::embed::{
    LocalEmbedder, MINILM_L6_V2_INT8, ModelError, default_models_dir, load_local_embedder,
    model_dir, remove_model,
};
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt};
use gpui::{
    Bounds, Context, FontWeight, InteractiveElement, MouseButton, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, TitlebarOptions, WindowOptions, div, prelude::*, px, rgb,
};

use super::Inbox;
use crate::model::Overlay;
use crate::model::settings::{self as nav, Page, Request, Row, Section};
use crate::motion::{self, Glide, Motion, NumberFade, Transition};
use crate::settings::{self, Edit, Snapshot, Values};
use crate::{embeddings, mailbox, theme};

const MODEL: fell_core::embed::LocalModel = MINILM_L6_V2_INT8;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ModelStatus {
    #[default]
    Checking,
    Missing,
    Installed,
    NeedsRepair,
    Unavailable,
    Installing,
    Removing,
}

impl ModelStatus {
    fn read(root: &Path) -> Self {
        match load_local_embedder(&MODEL, Some(root)) {
            LocalEmbedder::Model(_) => Self::Installed,
            LocalEmbedder::Placeholder(ModelError::Missing)
                if !model_dir(&MODEL, root).exists() =>
            {
                Self::Missing
            }
            LocalEmbedder::Placeholder(_) | LocalEmbedder::Unavailable(_) => Self::NeedsRepair,
        }
    }

    fn busy(self) -> bool {
        matches!(self, Self::Installing | Self::Removing)
    }

    fn status(self) -> (&'static str, &'static str) {
        match self {
            Self::Checking => ("Checking model files…", "Search can always match keywords."),
            Self::Missing => (
                "Keyword search",
                "Install a local model to find mail by meaning too.",
            ),
            Self::Installed => (
                "Model installed",
                "Ready for meaning-based search when Fell opens the mailbox.",
            ),
            Self::NeedsRepair => (
                "Model needs repair",
                "Search matches keywords until a usable model is installed.",
            ),
            Self::Unavailable => (
                "Data directory unavailable",
                "Set XDG_DATA_HOME or FELL_MODEL_DIR to manage the model.",
            ),
            Self::Installing => (
                "Downloading model…",
                "Fell verifies each file against its pinned checksum.",
            ),
            Self::Removing => (
                "Removing model…",
                "Mail and the keyword index stay available.",
            ),
        }
    }
}

pub(super) struct View {
    snapshot: Snapshot,
    edits: Option<mpsc::UnboundedSender<Edit>>,
    scroll: ScrollHandle,
    consent_scroll: Option<Glide>,
    fade: NumberFade<Values>,
    page: Option<Page>,
    page_fade: Option<Transition>,
    account: Option<BackendKind>,
    account_file_exists: bool,
    setup: Option<gpui::AnyWindowHandle>,
    model_root: Option<PathBuf>,
    model: ModelStatus,
    model_generation: u64,
    model_notice: Option<(String, bool)>,
}

impl View {
    pub fn new(
        snapshot: Snapshot,
        account: Option<BackendKind>,
        account_file_exists: bool,
    ) -> Self {
        Self {
            snapshot,
            edits: None,
            scroll: ScrollHandle::new(),
            consent_scroll: None,
            fade: NumberFade::default(),
            page: None,
            page_fade: None,
            account,
            account_file_exists,
            setup: None,
            model_root: default_models_dir(),
            model: ModelStatus::Checking,
            model_generation: 0,
            model_notice: None,
        }
    }

    pub fn scroll_to(&self, row: usize) {
        self.scroll.scroll_to_item(row);
    }

    pub fn reset_scroll(&mut self) {
        self.consent_scroll = None;
        self.scroll.set_offset(gpui::point(px(0.), px(0.)));
    }

    pub fn settle(&mut self) {
        self.fade = NumberFade::default();
        self.page_fade = None;
    }

    fn scroll_consent(&mut self, lines: i32, motion: Motion, now: Instant) {
        let measured = f32::from(self.scroll.offset().y);
        let end = self.consent_scroll.map_or(measured, |glide| glide.to);
        let max = f32::from(self.scroll.max_offset().height);
        let target = (end - lines as f32 * 19.).clamp(-max, 0.);
        self.consent_scroll = Glide::start(&motion, measured, target, max, motion::SLIDE, now);
        if self.consent_scroll.is_none() {
            self.scroll.set_offset(gpui::point(px(0.), px(target)));
        }
    }

    pub fn observe(&mut self, page: Option<Page>, now: Instant, motion: Motion) -> bool {
        if page.is_none() || !motion.enabled() {
            if let Some(glide) = self.consent_scroll.take() {
                self.scroll.set_offset(gpui::point(px(0.), px(glide.to)));
            }
        } else if let Some(glide) = self.consent_scroll {
            self.scroll
                .set_offset(gpui::point(px(0.), px(glide.value(now))));
            if !glide.running(now) {
                self.consent_scroll = None;
            }
        }
        if self.page != page {
            self.page_fade = if self.page.is_some() && page.is_some() {
                motion.start(motion::FADE, now)
            } else {
                None
            };
            self.page = page;
        }
        self.consent_scroll.is_some()
            || self.fade.frame(now).is_some()
            || self.page_fade.is_some_and(|fade| fade.running(now))
    }
}

impl Inbox {
    pub(super) fn watch_settings(&mut self, legacy: Values, cx: &mut Context<Self>) {
        if let Some(notice) = self.settings_view.snapshot.notice.clone() {
            self.settings_notice(notice, true, cx);
        }
        report_unknown(&self.settings_view.snapshot);
        let (edits, mut requests) = mpsc::unbounded();
        self.settings_view.edits = Some(edits);
        let path = settings::path();
        let (changed, mut changes) = mpsc::unbounded();
        let watching = theme::watch(changed);
        cx.spawn(async move |this, cx| {
            let mut startup = true;
            loop {
                let edit = if std::mem::take(&mut startup) {
                    None
                } else {
                    let event = async {
                        if watching {
                            changes.next().await
                        } else {
                            cx.background_executor().timer(Duration::from_secs(1)).await;
                            Some(())
                        }
                    };
                    futures::pin_mut!(event);
                    futures::select_biased! {
                        edit = requests.next().fuse() => match edit {
                            Some(edit) => Some(edit),
                            None => break,
                        },
                        event = event.fuse() => match event {
                            Some(()) => None,
                            None => break,
                        },
                    }
                };
                let read_path = path.clone();
                let (snapshot, theme, motion, save_error) = cx
                    .background_executor()
                    .spawn(async move {
                        let path = read_path.as_deref();
                        let (snapshot, save_error) = match (edit, path) {
                            (Some(edit), Some(path)) => match settings::save(path, legacy, edit) {
                                Ok(snapshot) => (snapshot, None),
                                Err(err) => {
                                    (Snapshot::read(Some(path), legacy), Some(err.to_string()))
                                }
                            },
                            (Some(_), None) => (
                                Snapshot::read(None, legacy),
                                Some("No config directory".into()),
                            ),
                            (None, _) => (Snapshot::read(path, legacy), None),
                        };
                        let theme = theme::load_with(snapshot.values);
                        let motion = Motion::for_preference(snapshot.values.reduce_motion);
                        (snapshot, theme, motion, save_error)
                    })
                    .await;
                if this
                    .update(cx, |inbox, cx| {
                        inbox.apply_settings(snapshot, theme, motion, cx);
                        if let Some(error) = save_error {
                            inbox.settings_notice(
                                format!("Could not save settings · {error}"),
                                true,
                                cx,
                            );
                        } else if let Some(Edit::Notifications(mode)) = edit {
                            inbox.settings_notice(
                                crate::keymap::Action::Notifications(mode).label(),
                                false,
                                cx,
                            );
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn apply_settings(
        &mut self,
        snapshot: Snapshot,
        theme: theme::Theme,
        next_motion: Motion,
        cx: &mut Context<Self>,
    ) {
        let prior = &self.settings_view.snapshot;
        if *prior == snapshot && self.theme == theme && self.motion.motion == next_motion {
            return;
        }
        let before = prior.values;
        let next = snapshot.values;
        if prior.unknown != snapshot.unknown {
            report_unknown(&snapshot);
        }
        let notice = (prior.notice != snapshot.notice)
            .then(|| snapshot.notice.clone())
            .flatten();
        if self.motion.motion != next_motion {
            self.settle_motion();
            if next_motion.enabled() {
                self.motion.motion = next_motion;
            } else {
                self.motion = crate::inbox_motion::InboxMotion::new(next_motion);
            }
            self.row_widths = crate::row_widths::RowWidths::new(next_motion);
        }
        if self.session.overlay == Overlay::Settings && before != next {
            self.settings_view
                .fade
                .change(before, next_motion, self.motion.clock.now());
        }
        if before.notifications != next.notifications {
            self.session.notification_mode = Some(next.notifications);
            if let Some(queue) = &self.notifications {
                queue.set_mode(next.notifications);
            }
        }
        self.session.read_dwell_ms = next.read_dwell_ms;
        if self.session.keymap.which_key_ms != next.which_key_ms {
            self.session.keymap.which_key_ms = next.which_key_ms;
            if !self.session.chord.is_empty() && !self.session.which_visible {
                self.session.chord_gen += 1;
                self.arm_which(cx);
            }
        }
        if self.theme != theme {
            self.theme = theme;
            self.row_widths.clear();
        }
        self.settings_view.snapshot = snapshot;
        if let Some(notice) = notice {
            self.settings_notice(notice, true, cx);
        }
        cx.notify();
    }

    pub(super) fn settings_request(&mut self, request: Request, cx: &mut Context<Self>) {
        match request {
            Request::Save(edit) => {
                if let Some(edits) = &self.settings_view.edits
                    && edits.unbounded_send(edit).is_ok()
                {
                    return;
                }
                self.settings_notice("Could not save settings", true, cx);
            }
            Request::AccountSetup => self.open_account_setup(cx),
            Request::EditKeymap => self.open_keymap(cx),
            Request::CheckModel => self.read_model_status(cx),
            Request::ScrollConsent(lines) => self.settings_view.scroll_consent(
                lines,
                self.motion.motion,
                self.motion.clock.now(),
            ),
            Request::ChangeModel { install } => self.change_model(install, cx),
        }
    }

    fn settings_notice(&mut self, message: impl Into<String>, error: bool, cx: &mut Context<Self>) {
        self.session.show_toast(message, error);
        self.dismiss_notice_after(self.session.toast_generation(), Duration::from_secs(5), cx);
        cx.notify();
    }

    fn open_account_setup(&mut self, cx: &mut Context<Self>) {
        if self.settings_view.account_file_exists
            || mailbox::account_config_path().is_some_and(|path| path.exists())
        {
            self.settings_notice("An account file already exists", true, cx);
            return;
        }
        if let Some(handle) = self.settings_view.setup
            && handle
                .update(cx, |_, window, _| window.activate_window())
                .is_ok()
        {
            return;
        }
        let (width, height) = super::window_size();
        let options = WindowOptions {
            window_bounds: Some(gpui::WindowBounds::Windowed(Bounds::centered(
                None,
                gpui::size(px(width), px(height)),
                cx,
            ))),
            titlebar: Some(TitlebarOptions {
                title: Some("Fell account setup".into()),
                ..Default::default()
            }),
            app_id: Some("dev.predmore.fell".into()),
            ..Default::default()
        };
        let Some(previous) = cx.try_global::<super::InboxWindow>().copied() else {
            return;
        };
        match cx.open_window(options, |window, cx| {
            cx.new(|cx| crate::onboarding::Onboarding::from_inbox(previous.0, window, cx))
        }) {
            Ok(handle) => self.settings_view.setup = Some(handle.into()),
            Err(_) => self.settings_notice("Could not open account setup", true, cx),
        }
    }

    fn open_keymap(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async {
                    let dir = mailbox::config_dir()
                        .ok_or_else(|| io::Error::other("No config directory"))?;
                    fs::create_dir_all(&dir)?;
                    let path = dir.join("keymap.toml");
                    match OpenOptions::new().write(true).create_new(true).open(&path) {
                        Ok(mut file) => file.write_all(
                            b"# Change the leader key, then reopen Fell.\nleader = \"space\"\n",
                        )?,
                        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
                        Err(err) => return Err(err),
                    }
                    super::diagnostics::open_in_editor(&path)
                })
                .await;
            if let Err(error) = result {
                let _ = this.update(cx, |inbox, cx| {
                    inbox.settings_notice(
                        format!("Could not open keymap.toml · {error}"),
                        true,
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    fn read_model_status(&mut self, cx: &mut Context<Self>) {
        if self.settings_view.model.busy() {
            return;
        }
        let Some(root) = self.settings_view.model_root.clone() else {
            self.settings_view.model = ModelStatus::Unavailable;
            return;
        };
        self.settings_view.model_generation += 1;
        let generation = self.settings_view.model_generation;
        cx.spawn(async move |this, cx| {
            let status = cx
                .background_executor()
                .spawn(async move { ModelStatus::read(&root) })
                .await;
            let _ = this.update(cx, |inbox, cx| {
                if inbox.settings_view.model_generation == generation {
                    inbox.settings_view.model = status;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn change_model(&mut self, install: bool, cx: &mut Context<Self>) {
        let Some(root) = self.settings_view.model_root.clone() else {
            return;
        };
        if self.settings_view.model.busy() {
            return;
        }
        self.settings_view.model_generation += 1;
        self.settings_view.model_notice = None;
        self.settings_view.model = if install {
            ModelStatus::Installing
        } else {
            ModelStatus::Removing
        };
        cx.spawn(async move |this, cx| {
            let (result, status) =
                cx.background_executor()
                    .spawn(async move {
                        let result =
                            if install {
                                embeddings::install_from_settings(&root)
                                    .map(|()| "Model installed. Reopen Fell to start indexing.")
                            } else {
                                remove_model(&MODEL, &root)
                        .map(|removed| {
                            if removed { "Model removed. Reopen Fell to return to keyword search." }
                            else { "No model files to remove." }
                        })
                        .map_err(|error| error.to_string())
                            };
                        (result, ModelStatus::read(&root))
                    })
                    .await;
            let _ = this.update(cx, |inbox, cx| {
                inbox.settings_view.model = status;
                let notice = match result {
                    Ok(message) => (message.to_owned(), false),
                    Err(error) => (format!("Model change failed · {error}"), true),
                };
                if inbox.session.overlay != Overlay::Settings {
                    inbox.settings_notice(notice.0.clone(), notice.1, cx);
                }
                inbox.settings_view.model_notice = Some(notice);
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn settings_click(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.session.overlay != Overlay::Settings {
            return;
        }
        self.settle_motion();
        self.session.settings_nav.selected = index;
        let before = self.before_step();
        let step = self
            .session
            .on_key(crate::keymap::KeyId::Enter, self.page_rows());
        self.apply_step(step, before, cx);
    }

    pub(super) fn validate_settings_step(&mut self, cx: &mut Context<Self>) {
        if let Page::ConfirmModel { install } = self.session.settings_nav.page
            && self.session.overlay == Overlay::Settings
        {
            let issue = if self.settings_view.model.busy() {
                Some("A model change is already running")
            } else if self.settings_view.model_root.is_none() {
                Some("No model data directory")
            } else if install && self.settings_view.model == ModelStatus::Installed {
                Some("The model is already installed")
            } else if !install && self.settings_view.model == ModelStatus::Missing {
                Some("No model files to remove")
            } else {
                None
            };
            if let Some(issue) = issue {
                self.session.settings_nav.back();
                self.settings_notice(issue, false, cx);
            }
        }
    }
}

fn report_unknown(snapshot: &Snapshot) {
    if !snapshot.unknown.is_empty() {
        fell_core::diagnostic!(INFO, "Ignored unknown settings.toml keys");
    }
}

const HEADER_HEIGHT: f32 = 64.;
const SECTION_ROW_HEIGHT: f32 = 64.;
const INDEX_ROW_HEIGHT: f32 = 49.;
const ACCOUNT_SUMMARY_HEIGHT: f32 = 64.;
const INDEX_FOOTER_HEIGHT: f32 = 21.;
const SEARCH_SUMMARY_HEIGHT: f32 = 108.;

pub(super) fn card_height(page: Page, room: f32) -> f32 {
    let rows = match page {
        Page::Index => Section::ALL.len() as f32 * INDEX_ROW_HEIGHT + INDEX_FOOTER_HEIGHT,
        Page::Section(section) => {
            section.rows().len() as f32 * SECTION_ROW_HEIGHT
                + match section {
                    Section::Accounts => ACCOUNT_SUMMARY_HEIGHT,
                    Section::Search => SEARCH_SUMMARY_HEIGHT,
                    _ => 0.,
                }
        }
        Page::Bindings | Page::ConfirmModel { .. } => 370.,
    };
    (HEADER_HEIGHT + rows + 18.).min(room.max(120.))
}

pub(super) fn card(
    theme: &theme::Theme,
    session: &crate::model::Session,
    view: &View,
    live: bool,
    height: f32,
    now: Instant,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let page = session.settings_nav.page;
    let content_opacity = if live {
        motion::lerp(
            motion::ARRIVE_FROM,
            1.,
            motion::progress(view.page_fade.as_ref(), now),
        )
    } else {
        1.
    };
    let back = match page {
        Page::Index => None,
        Page::Section(_) => Some("Settings"),
        Page::Bindings => Some("Keymap"),
        Page::ConfirmModel { .. } => Some("Search"),
    };
    let title = match page {
        Page::Index => "Settings",
        Page::Section(section) => section.label(),
        Page::Bindings => "Effective bindings",
        Page::ConfirmModel { install: true } => "Install local search model",
        Page::ConfirmModel { install: false } => "Remove local search model",
    };
    let header = div()
        .h(px(HEADER_HEIGHT))
        .flex_none()
        .flex()
        .flex_col()
        .justify_center()
        .px(px(20.))
        .border_b_1()
        .border_color(rgb(theme.border))
        .when_some(back, |header, back| {
            header.child(
                div()
                    .id("settings-back")
                    .text_size(px(12.))
                    .text_color(rgb(theme.dimmed))
                    .when(live, |label| {
                        label.cursor_pointer().on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|inbox, _, _, cx| {
                                if inbox.session.overlay == Overlay::Settings {
                                    cx.stop_propagation();
                                    inbox.session.settings_nav.back();
                                    inbox.settings_view.reset_scroll();
                                    cx.notify();
                                }
                            }),
                        )
                    })
                    .child(format!("‹  {back}")),
            )
        })
        .child(
            div()
                .text_size(px(20.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(title),
        );

    let mut body = div()
        .id("settings-body")
        .flex_1()
        .min_h(px(0.))
        .overflow_y_scroll()
        .px(px(10.))
        .py(px(8.))
        .when(live, |body| body.track_scroll(&view.scroll));
    let selected = session.settings_nav.selected;
    let mut details = None;
    let mut footer = None;
    match page {
        Page::Index => {
            for (index, section) in Section::ALL.into_iter().enumerate() {
                body = body.child(
                    row(theme, index, selected == index, live, INDEX_ROW_HEIGHT, cx)
                        .child(row_label(
                            theme,
                            section.label(),
                            Some(section.description()),
                            selected == index,
                        ))
                        .child(trailing(theme, "›", selected == index)),
                );
            }
            let unknown = &view.snapshot.unknown;
            let note = (!unknown.is_empty()).then(|| {
                format!(
                    "Ignored settings · {}",
                    crate::present::display_safe(&unknown.join(", ")),
                )
            });
            footer = Some(
                div()
                    .h(px(INDEX_FOOTER_HEIGHT))
                    .flex_none()
                    .px(px(20.))
                    .text_size(px(11.))
                    .text_color(rgb(theme.dimmed))
                    .truncate()
                    .children(note),
            );
        }
        Page::Section(section) => {
            // Keep scroll indices aligned with the selectable rows.
            for (index, setting) in section.rows().iter().copied().enumerate() {
                let selected = selected == index;
                let (label, description, state, enabled) = match setting {
                    Row::Value(preference) => (
                        preference.label(),
                        Some(preference.description()),
                        preference.display(view.snapshot.values),
                        true,
                    ),
                    Row::AccountSetup => (
                        "Open account setup",
                        Some("Connect through Proton Bridge or IMAP."),
                        if view.account_file_exists {
                            "Configured"
                        } else {
                            "›"
                        }
                        .to_owned(),
                        !view.account_file_exists,
                    ),
                    Row::EffectiveBindings => (
                        "Effective bindings",
                        Some("Shortcuts for this session."),
                        "›".to_owned(),
                        true,
                    ),
                    Row::EditKeymap => (
                        "Edit keymap.toml",
                        Some("Reopen Fell to apply leader key changes."),
                        "↗".to_owned(),
                        true,
                    ),
                    Row::InstallModel => (
                        if view.model == ModelStatus::NeedsRepair {
                            "Reinstall local model"
                        } else {
                            "Install local model"
                        },
                        Some("Download the pinned model from Hugging Face."),
                        embeddings::megabytes(MODEL.download_bytes()),
                        !view.model.busy()
                            && !matches!(
                                view.model,
                                ModelStatus::Installed | ModelStatus::Unavailable
                            ),
                    ),
                    Row::RemoveModel => (
                        "Remove local model",
                        Some("Return to keyword search."),
                        "›".to_owned(),
                        !view.model.busy()
                            && !matches!(
                                view.model,
                                ModelStatus::Missing | ModelStatus::Unavailable
                            ),
                    ),
                    Row::ManageAgents => (
                        "Open Agents",
                        Some("Review connected agents, access and activity."),
                        "›".to_owned(),
                        true,
                    ),
                };
                let fade = if let Row::Value(preference) = setting {
                    view.fade.frame(now).and_then(|(old, progress)| {
                        let old = preference.display(*old);
                        (old != state).then_some((old, progress))
                    })
                } else {
                    None
                };
                let value = if matches!(setting, Row::Value(_)) {
                    let (old, progress) =
                        fade.map_or((None, 1.), |(old, progress)| (Some(old), progress));
                    div()
                        .relative()
                        .w(px(142.))
                        .flex_none()
                        .text_right()
                        .child(value_text(theme, state, selected).opacity(progress))
                        .when_some(old, |value, old| {
                            value.child(
                                value_text(theme, old, selected)
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .right_0()
                                    .opacity(1. - progress),
                            )
                        })
                } else {
                    trailing(theme, state, selected)
                };
                body = body.child(
                    row(
                        theme,
                        index,
                        selected,
                        live && enabled,
                        SECTION_ROW_HEIGHT,
                        cx,
                    )
                    .child(row_label(theme, label, description, selected))
                    .child(value.opacity(if enabled { 1. } else { 0.75 })),
                );
            }

            details = match section {
                Section::Accounts => {
                    let title = if view.account.is_some() {
                        crate::present::display_safe(&session.account_email)
                    } else if view.account_file_exists {
                        "Account unavailable".to_owned()
                    } else {
                        "No account configured".to_owned()
                    };
                    let description = match view.account {
                        Some(BackendKind::Bridge) => "Proton Mail Bridge",
                        Some(BackendKind::Imap) => "IMAP",
                        Some(BackendKind::ProtonApi) => "Proton Mail",
                        None if view.account_file_exists => {
                            "Check account.toml for configuration errors."
                        }
                        None => "Set up an account to sync your mailbox.",
                    };
                    Some(detail(theme, title, description).h(px(ACCOUNT_SUMMARY_HEIGHT)))
                }
                Section::Search => {
                    let (status, description) = view.model.status();
                    Some(
                        detail(theme, status, description)
                            .h(px(SEARCH_SUMMARY_HEIGHT))
                            .overflow_hidden()
                            .when_some(view.model_notice.as_ref(), |line, (message, alert)| {
                                line.child(
                                    div()
                                        .mt(px(6.))
                                        .text_color(rgb(if *alert {
                                            theme.error
                                        } else {
                                            theme.dimmed
                                        }))
                                        .child(message.clone()),
                                )
                            }),
                    )
                }
                _ => None,
            };
        }
        Page::Bindings => {
            for (index, (action, keys)) in nav::bindings(&session.keymap).enumerate() {
                body = body.child(
                    row(theme, index, selected == index, false, 39., cx)
                        .child(row_label(theme, action.label(), None, selected == index))
                        .child(trailing(theme, keys, selected == index)),
                );
            }
        }
        Page::ConfirmModel { install } => {
            let text = if install {
                view.model_root
                    .as_deref()
                    .map(|root| embeddings::plan(&MODEL, root))
                    .unwrap_or_default()
            } else {
                "Fell will remove the local model files. Your mail stays on this device. Reopen Fell to return to keyword search.".to_owned()
            };
            body = body.child(
                div()
                    .px(px(10.))
                    .py(px(6.))
                    .text_size(px(13.))
                    .line_height(px(19.))
                    .text_color(rgb(theme.foreground))
                    .children(text.split('\n').map(|line| {
                        div()
                            .min_h(px(10.))
                            .whitespace_normal()
                            .child(line.to_owned())
                    })),
            );
            footer = Some(
                div()
                    .flex_none()
                    .flex()
                    .justify_end()
                    .gap(px(8.))
                    .p(px(12.))
                    .border_t_1()
                    .border_color(rgb(theme.border))
                    .child(confirm_button(theme, "Cancel", false, live, cx))
                    .child(confirm_button(
                        theme,
                        if install {
                            "Download  ·  y"
                        } else {
                            "Remove  ·  y"
                        },
                        true,
                        live,
                        cx,
                    )),
            );
        }
    }
    div()
        .w(px(530.))
        .max_w(gpui::relative(0.94))
        .h(px(height))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .text_color(rgb(theme.foreground))
        .when(live, |card| {
            card.on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _, _, cx| cx.stop_propagation()),
            )
        })
        .child(header)
        .children(details.map(|details| details.opacity(content_opacity)))
        .child(body.opacity(content_opacity))
        .children(footer)
}

fn detail(
    theme: &theme::Theme,
    title: impl Into<gpui::SharedString>,
    explanation: &'static str,
) -> gpui::Div {
    div()
        .flex_none()
        .px(px(20.))
        .py(px(13.))
        .border_b_1()
        .border_color(rgb(theme.border))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .truncate()
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(12.))
                .text_color(rgb(theme.dimmed))
                .child(explanation),
        )
}

fn row(
    theme: &theme::Theme,
    index: usize,
    selected: bool,
    clickable: bool,
    height: f32,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(gpui::ElementId::named_usize("settings-row", index))
        .h(px(height))
        .flex_none()
        .flex()
        .items_center()
        .gap(px(9.))
        .px(px(10.))
        .rounded_md()
        .when(selected, |row| {
            row.bg(rgb(theme.selection))
                .text_color(rgb(theme.on_selection))
        })
        .when(clickable, |row| {
            row.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(move |inbox, _, _, cx| {
                    cx.stop_propagation();
                    inbox.settings_click(index, cx);
                }),
            )
        })
}

fn row_label(
    theme: &theme::Theme,
    label: &'static str,
    description: Option<&'static str>,
    selected: bool,
) -> gpui::Div {
    div()
        .min_w(px(0.))
        .flex_1()
        .flex()
        .flex_col()
        .justify_center()
        .child(
            div()
                .truncate()
                .font_weight(FontWeight::MEDIUM)
                .child(label),
        )
        .when_some(description, |label, description| {
            label.child(
                div()
                    .text_size(px(12.))
                    .line_height(px(15.))
                    .text_color(rgb(if selected {
                        theme.on_selection_dim
                    } else {
                        theme.dimmed
                    }))
                    .child(description),
            )
        })
}

fn trailing(
    theme: &theme::Theme,
    text: impl Into<gpui::SharedString>,
    selected: bool,
) -> gpui::Div {
    value_text(theme, text, selected)
        .max_w(gpui::relative(0.47))
        .flex_none()
}

fn value_text(
    theme: &theme::Theme,
    text: impl Into<gpui::SharedString>,
    selected: bool,
) -> gpui::Div {
    div()
        .text_size(px(13.))
        .text_color(rgb(if selected {
            theme.on_selection_accent
        } else {
            theme.accent
        }))
        .truncate()
        .child(text.into())
}

fn confirm_button(
    theme: &theme::Theme,
    text: &'static str,
    confirm: bool,
    live: bool,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(if confirm {
            "settings-confirm"
        } else {
            "settings-cancel"
        })
        .px(px(10.))
        .py(px(5.))
        .rounded_md()
        .border_1()
        .border_color(rgb(theme.border))
        .when(confirm, |button| {
            button
                .bg(rgb(theme.selection))
                .text_color(rgb(theme.on_selection))
        })
        .when(live, |button| {
            button.cursor_pointer().on_mouse_down(
                MouseButton::Left,
                cx.listener(move |inbox, _, _, cx| {
                    if inbox.session.overlay != Overlay::Settings {
                        return;
                    }
                    cx.stop_propagation();
                    let page = inbox.page_rows();
                    let before = inbox.before_step();
                    let step = inbox.session.on_key(
                        crate::keymap::KeyId::Char(if confirm { 'y' } else { 'n' }),
                        page,
                    );
                    inbox.settings_view.reset_scroll();
                    inbox.apply_step(step, before, cx);
                }),
            )
        })
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fell_core::NotificationMode;

    #[test]
    fn a_reloaded_snapshot_changes_the_running_inbox() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        fs::write(
            &path,
            "theme = 'light'\nfont_size = 17\nreduce_motion = true\nwhich_key_ms = 0\nread_dwell_ms = 500\nnotifications = 'private'\n",
        )
        .unwrap();
        let snapshot = Snapshot::read(Some(&path), Values::default());
        let next = theme::load_with(snapshot.values);
        let (mut app, handle) = crate::view::tests::pending_inbox();
        handle
            .update(&mut app, |inbox, _, cx| {
                let selected = inbox.session.cursor_id.clone();
                inbox.apply_settings(snapshot, next, Motion::new(false), cx);
                assert_eq!(inbox.session.cursor_id, selected);
                assert!(inbox.theme.is_light());
                assert_eq!(inbox.theme.ui_px, 17.);
                assert!(!inbox.motion.motion.enabled());
                assert_eq!(inbox.session.keymap.which_key_ms, 0);
                assert_eq!(inbox.session.read_dwell_ms, 500);
                assert_eq!(
                    inbox.session.notification_mode,
                    Some(NotificationMode::Private)
                );
            })
            .unwrap();
    }
}
