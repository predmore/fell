use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fell_core::embed::{
    LocalEmbedder, MINILM_L6_V2_INT8, ModelError, default_models_dir, load_local_embedder,
    model_dir, remove_model,
};
use fell_core::{AccountConfig, BackendKind};
use futures::channel::mpsc;
use futures::{FutureExt, StreamExt};
use gpui::{
    Bounds, Context, FontWeight, InteractiveElement, MouseButton, ParentElement, ScrollHandle,
    StatefulInteractiveElement, Styled, TitlebarOptions, WindowOptions, div, prelude::*, px, rgb,
};

use super::{FontOpt, Inbox};
use crate::model::Overlay;
use crate::model::settings::{self as nav, Availability, Page, Request, Row, Section};
use crate::motion::{self, Glide, LayerSwap, Motion, SwapFrame, Transition};
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
            Self::Missing => ("Keyword search", "Finds mail by the words it contains."),
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
    scroll_glide: Option<Glide>,
    fade: Option<(Snapshot, LayerSwap)>,
    notice_generation: Option<u64>,
    reveal_index_selection: bool,
    selection: Option<(Page, usize)>,
    chevrons: Option<Transition>,
    page: Option<Page>,
    page_fade: Option<Transition>,
    server: Option<String>,
    setup: Option<gpui::AnyWindowHandle>,
    model_root: Option<PathBuf>,
    model: ModelStatus,
    model_generation: u64,
    model_notice: Option<(String, bool)>,
}

impl View {
    pub fn new(snapshot: Snapshot, account: Option<&AccountConfig>) -> Self {
        let model_root = default_models_dir();
        let model = model_root
            .as_deref()
            .map_or(ModelStatus::Unavailable, |root| {
                if model_dir(&MODEL, root).symlink_metadata().is_ok() {
                    ModelStatus::Checking
                } else {
                    ModelStatus::Missing
                }
            });
        Self {
            snapshot,
            edits: None,
            scroll: ScrollHandle::new(),
            scroll_glide: None,
            fade: None,
            notice_generation: None,
            reveal_index_selection: false,
            selection: None,
            chevrons: None,
            page: None,
            page_fade: None,
            server: account.map(|config| {
                let host = crate::present::display_safe(&config.host);
                let host = if host.contains(':') && !host.starts_with('[') {
                    format!("[{host}]")
                } else {
                    host
                };
                match config.backend {
                    BackendKind::Bridge => format!("Proton Mail Bridge on {host}:{}", config.port),
                    BackendKind::Imap => format!("IMAP · {host}:{}", config.port),
                    BackendKind::ProtonApi => "Proton Mail".into(),
                }
            }),
            setup: None,
            model_root,
            model,
            model_generation: 0,
            model_notice: None,
        }
    }

    pub fn availability(&self) -> Availability {
        Availability {
            account_setup: !account_file_exists(mailbox::account_config_path().as_deref()),
            install_model: matches!(
                self.model,
                ModelStatus::Checking | ModelStatus::Missing | ModelStatus::NeedsRepair
            ),
            remove_model: matches!(
                self.model,
                ModelStatus::Checking | ModelStatus::Installed | ModelStatus::NeedsRepair
            ),
        }
    }

    pub fn scroll_to(&self, row: usize) {
        self.scroll.scroll_to_item(row);
    }

    pub fn reveal_index_row(&mut self, row: usize, height: f32, final_height: f32) {
        if !self.reveal_index_selection {
            return;
        }
        // The old card can still be growing. Keep the index at its reset
        // position until the final viewport can show the selected row.
        let viewport = final_height - HEADER_HEIGHT - BODY_PADDING - INDEX_FOOTER_HEIGHT - 2.;
        let top = -f32::from(self.scroll.offset().y);
        let row_top = row as f32 * INDEX_ROW_HEIGHT;
        if row_top >= top && row_top + INDEX_ROW_HEIGHT <= top + viewport {
            self.reveal_index_selection = false;
        } else if height >= final_height - 1. {
            self.scroll.scroll_to_item(row);
            self.reveal_index_selection = false;
        }
    }

    pub fn reset_scroll(&mut self) {
        self.scroll_glide = None;
        self.scroll.set_offset(gpui::point(px(0.), px(0.)));
    }

    pub fn settle(&mut self) {
        self.fade = None;
        self.page_fade = None;
    }

    fn scroll_lines(&mut self, lines: i32, motion: Motion, now: Instant) {
        let measured = f32::from(self.scroll.offset().y);
        let end = self.scroll_glide.map_or(measured, |glide| glide.to);
        let max = f32::from(self.scroll.max_offset().height);
        let target = (end - lines as f32 * 19.).clamp(-max, 0.);
        self.scroll_glide = Glide::start(&motion, measured, target, max, motion::SLIDE, now);
        if self.scroll_glide.is_none() {
            self.scroll.set_offset(gpui::point(px(0.), px(target)));
        }
    }

    fn value_fade(&self, now: Instant) -> Option<(&Snapshot, f32, f32)> {
        let (old, fade) = self.fade.as_ref()?;
        let (outgoing, incoming) = match fade.frame(now)? {
            SwapFrame::Leaving(opacity) => (opacity, 0.),
            SwapFrame::Arriving(opacity) => (0., opacity),
        };
        Some((old, outgoing, incoming))
    }

    pub fn observe(
        &mut self,
        page: Option<Page>,
        selected: usize,
        now: Instant,
        motion: Motion,
    ) -> bool {
        if page.is_none() || !motion.enabled() {
            if let Some(glide) = self.scroll_glide.take() {
                self.scroll.set_offset(gpui::point(px(0.), px(glide.to)));
            }
        } else if let Some(glide) = self.scroll_glide {
            self.scroll
                .set_offset(gpui::point(px(0.), px(glide.value(now))));
            if !glide.running(now) {
                self.scroll_glide = None;
            }
        }
        let selection = page.map(|page| (page, selected));
        if self.selection != selection {
            self.reveal_index_selection = page == Some(Page::Index);
            self.selection = selection;
            self.chevrons = selection.and_then(|_| motion.start(motion::FADE, now));
        }
        if self.page != page {
            self.page_fade = if self.page.is_some() && page.is_some() {
                motion.start(motion::FADE, now)
            } else {
                None
            };
            self.page = page;
        }
        self.scroll_glide.is_some()
            || self.value_fade(now).is_some()
            || self.page_fade.is_some_and(|fade| fade.running(now))
            || self.chevrons.is_some_and(|fade| fade.running(now))
    }
}

impl Inbox {
    pub(super) fn watch_settings(&mut self, legacy: Values, cx: &mut Context<Self>) {
        self.update_settings_notice(true, cx);
        report_unknown(&self.settings_view.snapshot);
        let (edits, mut requests) = mpsc::unbounded();
        self.settings_view.edits = Some(edits);
        let path = settings::path();
        let (changed, mut changes) = mpsc::unbounded();
        let watching = theme::watch(changed);
        let mut previous = self.settings_view.snapshot.clone();
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
                                    (previous.reload(Some(path), legacy), Some(err.to_string()))
                                }
                            },
                            (Some(_), None) => (
                                Snapshot::read(None, legacy),
                                Some("No config directory".into()),
                            ),
                            (None, _) => (previous.reload(path, legacy), None),
                        };
                        let theme = theme::load_with(snapshot.values);
                        let motion = Motion::for_preference(snapshot.values.reduce_motion);
                        (snapshot, theme, motion, save_error)
                    })
                    .await;
                previous = snapshot.clone();
                if this
                    .update(cx, |inbox, cx| {
                        inbox.apply_settings(snapshot, theme, motion, cx);
                        if let Some(error) = save_error {
                            inbox.settings_notice(
                                format!("Could not save settings · {error}"),
                                true,
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
        let availability_changed = self.refresh_settings_availability();
        let prior = &self.settings_view.snapshot;
        if *prior == snapshot && self.theme == theme && self.motion.motion == next_motion {
            if availability_changed {
                cx.notify();
            }
            return;
        }
        let before = prior.values;
        let next = snapshot.values;
        if prior.unknown != snapshot.unknown {
            report_unknown(&snapshot);
        }
        let notice_changed = prior.notice() != snapshot.notice();
        let old = (before != next || prior.invalid != snapshot.invalid).then(|| prior.clone());
        if self.motion.motion != next_motion {
            self.settle_motion();
            if next_motion.enabled() {
                self.motion.motion = next_motion;
            } else {
                self.motion = crate::inbox_motion::InboxMotion::new(next_motion);
            }
            self.row_widths = crate::row_widths::RowWidths::new(next_motion);
        }
        if self.session.overlay == Overlay::Settings
            && let Some(old) = old
        {
            let half = motion::FADE / 2;
            self.settings_view.fade =
                LayerSwap::start(next_motion, self.motion.clock.now(), half, half)
                    .map(|fade| (old, fade));
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
        self.update_settings_notice(notice_changed, cx);
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
            Request::Scroll(lines) => {
                self.settings_view
                    .scroll_lines(lines, self.motion.motion, self.motion.clock.now())
            }
            Request::ChangeModel { install } => self.change_model(install, cx),
        }
    }

    fn settings_notice(&mut self, message: impl Into<String>, error: bool, cx: &mut Context<Self>) {
        self.session.show_toast(message, error);
        self.dismiss_notice_after(self.session.toast_generation(), Duration::from_secs(5), cx);
        cx.notify();
    }

    pub(super) fn update_settings_notice(&mut self, changed: bool, cx: &mut Context<Self>) {
        let snapshot = &self.settings_view.snapshot;
        let inline = snapshot.read_error.is_none()
            && self.session.overlay == Overlay::Settings
            && shows_invalid_value(self.session.settings_nav.page, snapshot);
        let notice = if inline { None } else { snapshot.notice() };
        if let Some(notice) = notice {
            if changed {
                self.settings_notice(notice, true, cx);
                self.settings_view.notice_generation = Some(self.session.toast_generation());
            }
        } else if let Some(generation) = self.settings_view.notice_generation.take() {
            self.session.clear_toast_if(generation);
        }
    }

    fn open_account_setup(&mut self, cx: &mut Context<Self>) {
        if account_file_exists(mailbox::account_config_path().as_deref()) {
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
                    let path = ensure_keymap(&dir)?;
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

    fn apply_model_status(&mut self, status: ModelStatus) {
        self.settings_view.model = status;
        self.refresh_settings_availability();
    }

    pub(super) fn refresh_settings_availability(&mut self) -> bool {
        let availability = self.settings_view.availability();
        if self.session.settings_nav.availability == availability {
            return false;
        }
        self.session.settings_nav.set_availability(availability);
        if self.session.overlay == Overlay::Palette {
            self.session.palette_index = self
                .session
                .palette_index
                .min(self.session.palette().len().saturating_sub(1));
        }
        true
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
                    inbox.apply_model_status(status);
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
        self.apply_model_status(if install {
            ModelStatus::Installing
        } else {
            ModelStatus::Removing
        });
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
                inbox.apply_model_status(status);
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

    fn settings_click(&mut self, index: usize, key: crate::keymap::KeyId, cx: &mut Context<Self>) {
        if self.session.overlay != Overlay::Settings {
            return;
        }
        self.settle_motion();
        self.session.settings_nav.selected = index;
        let before = self.before_step();
        let step = self.session.on_key(key, self.page_rows());
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

fn account_file_exists(path: Option<&Path>) -> bool {
    path.is_some_and(|path| {
        !matches!(fs::symlink_metadata(path), Err(error) if error.kind() == io::ErrorKind::NotFound)
    })
}

fn shows_invalid_value(page: Page, snapshot: &Snapshot) -> bool {
    let Page::Section(section) = page else {
        return false;
    };
    snapshot
        .invalid
        .iter()
        .any(|(key, _)| section.rows().contains(&Row::Value(*key)))
}

fn ensure_keymap(dir: &Path) -> io::Result<PathBuf> {
    mailbox::private_config_dir(dir)?;
    let path = dir.join("keymap.toml");
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(b"# Change the leader key, then reopen Fell.\nleader = \"space\"\n")?;
        }
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            match OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)
            {
                Ok(file) if file.metadata()?.is_file() => {
                    mailbox::restrict_permissions(&file, 0o600)?;
                }
                Err(err) if err.raw_os_error() == Some(libc::ELOOP) => {}
                Err(err) => return Err(err),
                Ok(_) => return Err(io::Error::other("keymap.toml must be a regular file")),
            }
        }
        Err(err) => return Err(err),
    }
    Ok(path)
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
const ACCOUNT_SERVER_HEIGHT: f32 = 80.;
const BODY_PADDING: f32 = 16.;
const BINDING_ROW_HEIGHT: f32 = 39.;
const MAX_BINDING_ROWS: f32 = 9.;
const INDEX_FOOTER_HEIGHT: f32 = 21.;
const SEARCH_SUMMARY_HEIGHT: f32 = 70.;

pub(super) fn card_height(nav: &nav::Nav, room: f32, narrow: bool) -> f32 {
    if nav.page == Page::Bindings {
        let chrome = HEADER_HEIGHT + BODY_PADDING + 2.;
        let rows = ((room - chrome) / BINDING_ROW_HEIGHT)
            .floor()
            .clamp(1., MAX_BINDING_ROWS);
        return chrome + rows * BINDING_ROW_HEIGHT;
    }
    let rows = match nav.page {
        Page::Index => Section::ALL.len() as f32 * INDEX_ROW_HEIGHT + INDEX_FOOTER_HEIGHT,
        Page::Section(section) => {
            nav.rows(section)
                .map(|row| {
                    SECTION_ROW_HEIGHT
                        + if narrow && matches!(row, Row::Value(_)) {
                            8.
                        } else {
                            0.
                        }
                })
                .sum::<f32>()
                + match section {
                    Section::Accounts if nav.availability.account_setup => ACCOUNT_SUMMARY_HEIGHT,
                    Section::Accounts => ACCOUNT_SERVER_HEIGHT - 10.,
                    Section::Search => SEARCH_SUMMARY_HEIGHT,
                    _ => 0.,
                }
        }
        Page::ConfirmModel { install: true } if narrow => 445.,
        Page::ConfirmModel { .. } => 370.,
        Page::Bindings => unreachable!(),
    };
    (HEADER_HEIGHT + rows + 18.).min(room.max(120.))
}

pub(super) fn card(
    theme: &theme::Theme,
    session: &crate::model::Session,
    view: &View,
    live: bool,
    (height, width): (f32, f32),
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
                                    inbox.update_settings_notice(false, cx);
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
    let mut binding_fade = false;
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
            for (index, setting) in session.settings_nav.rows(section).enumerate() {
                let selected = selected == index;
                let mut control =
                    row(theme, index, selected, live, SECTION_ROW_HEIGHT, cx).py(px(10.));
                if let Row::Value(preference) = setting {
                    let state = preference.display(view.snapshot.values);
                    let note = preference_note(&view.snapshot, preference, &state, width);
                    let previous = view.value_fade(now);
                    let changed = previous.and_then(|(snapshot, outgoing, incoming)| {
                        let old = preference.display(snapshot.values);
                        (old != state).then_some((old, outgoing, incoming))
                    });
                    let old_note = previous.and_then(|(snapshot, outgoing, incoming)| {
                        let old = preference.display(snapshot.values);
                        let old = preference_note(snapshot, preference, &old, width);
                        (old != note).then_some((old, outgoing, incoming))
                    });
                    let opacity = if selected {
                        motion::progress(view.chevrons.as_ref(), now)
                    } else {
                        0.
                    };
                    let incoming = changed.as_ref().map_or(1., |(_, _, incoming)| *incoming);
                    let value = div()
                        .relative()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .text_center()
                        .child(value_text(theme, state, selected).opacity(incoming))
                        .when_some(changed, |value, (old, outgoing, _)| {
                            // Keep the outgoing value's width until its fade ends.
                            value
                                .child(
                                    value_text(theme, old.clone(), selected)
                                        .h(px(0.))
                                        .opacity(0.),
                                )
                                .child(
                                    value_text(theme, old, selected)
                                        .absolute()
                                        .top_0()
                                        .left_0()
                                        .right_0()
                                        .opacity(outgoing),
                                )
                        });
                    control = control
                        .child(row_label(theme, preference.label(), None, selected).child({
                            let incoming =
                                old_note.as_ref().map_or(1., |(_, _, incoming)| *incoming);
                            let render = |(text, alert): (String, bool)| {
                                div()
                                    .text_size(px(12.))
                                    .line_height(px(15.))
                                    .text_color(rgb(if alert {
                                        theme.error
                                    } else if selected {
                                        theme.on_selection_dim
                                    } else {
                                        theme.dimmed
                                    }))
                                    .child(text)
                            };
                            div()
                                .relative()
                                .child(render(note).opacity(incoming))
                                .when_some(old_note, |note, (old, outgoing, _)| {
                                    note.child(
                                        render(old)
                                            .absolute()
                                            .top_0()
                                            .left_0()
                                            .right_0()
                                            .opacity(outgoing),
                                    )
                                })
                        }))
                        .child(
                            div()
                                .w_auto()
                                .min_w(px(56.))
                                .max_w(px(142.))
                                .flex_none()
                                .flex()
                                .items_center()
                                .child(chevron(theme, index, true, live && selected, opacity, cx))
                                .child(value)
                                .child(chevron(theme, index, false, live && selected, opacity, cx)),
                        );
                } else {
                    let (label, description, suffix, info) = match setting {
                        Row::AccountSetup => (
                            "Open account setup",
                            "Connect via Bridge or IMAP.",
                            "›",
                            false,
                        ),
                        Row::EffectiveBindings => (
                            "Effective bindings",
                            "Shortcuts for this session.",
                            "›",
                            false,
                        ),
                        Row::EditKeymap => (
                            "Edit keymap.toml",
                            "Reopen Fell to apply leader changes.",
                            "↗",
                            false,
                        ),
                        Row::InstallModel => (
                            if view.model == ModelStatus::NeedsRepair {
                                "Reinstall local model"
                            } else {
                                "Install local model"
                            },
                            "Download from Hugging Face.",
                            "",
                            true,
                        ),
                        Row::RemoveModel => (
                            "Remove local model",
                            "Return to keyword search.",
                            "›",
                            false,
                        ),
                        Row::ManageAgents => (
                            "Open Agents",
                            "Review connected agents and access.",
                            "›",
                            false,
                        ),
                        Row::Value(_) => unreachable!(),
                    };
                    let suffix = if info {
                        div()
                            .flex_none()
                            .text_size(px(13.))
                            .text_color(rgb(if selected {
                                theme.on_selection_dim
                            } else {
                                theme.dimmed
                            }))
                            .child(embeddings::megabytes(MODEL.download_bytes()))
                    } else {
                        trailing(theme, suffix, selected)
                    };
                    control = control
                        .child(row_label(theme, label, Some(description), selected))
                        .child(suffix);
                }
                body = body.child(control);
            }

            details = match section {
                Section::Accounts => {
                    let (title, description) = if session.settings_nav.availability.account_setup {
                        (
                            "No account configured".to_owned(),
                            "Set up an account to sync your mailbox.",
                        )
                    } else if let Some(server) = &view.server {
                        (
                            crate::present::display_safe(&session.account_email),
                            server.as_str(),
                        )
                    } else {
                        (
                            "Account unavailable".to_owned(),
                            "Check account.toml or reopen Fell to connect.",
                        )
                    };
                    let setup = session.settings_nav.availability.account_setup;
                    let sync = view.server.as_ref().filter(|_| !setup).map(|_| {
                        session.last_sync_at.map_or_else(
                            || "Waiting for first sync".into(),
                            |at| {
                                format!(
                                    "Last sync · {}",
                                    crate::present::activity_time(super::now_unix(), at)
                                )
                            },
                        )
                    });
                    Some(
                        detail(theme, title, description, false, setup)
                            .h(px(if setup {
                                ACCOUNT_SUMMARY_HEIGHT
                            } else {
                                ACCOUNT_SERVER_HEIGHT
                            }))
                            .children(sync.map(|label| {
                                div()
                                    .text_size(px(12.))
                                    .text_color(rgb(theme.dimmed))
                                    .child(label)
                            })),
                    )
                }
                Section::Search => {
                    let (status, description) = view.model.status();
                    let (message, alert) = view
                        .model_notice
                        .as_ref()
                        .map_or((description, false), |(message, alert)| {
                            (message.as_str(), *alert)
                        });
                    Some(detail(
                        theme,
                        status,
                        message,
                        alert,
                        session.settings_nav.rows(section).next().is_some(),
                    ))
                }
                _ => None,
            };
        }
        Page::Bindings => {
            let mut rows = 0;
            for (action, keys) in nav::bindings(&session.keymap) {
                rows += 1;
                body = body.child(
                    div()
                        .h(px(BINDING_ROW_HEIGHT))
                        .flex_none()
                        .px(px(10.))
                        .py(px(5.))
                        .flex()
                        .items_center()
                        .gap(px(8.))
                        .child(row_label(theme, action.label(), None, false))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .justify_end()
                                .gap(px(4.))
                                .max_w(gpui::relative(0.57))
                                .children(keys.split(" · ").map(|keys| {
                                    div()
                                        .px(px(5.))
                                        .py(px(2.))
                                        .rounded_sm()
                                        .border_1()
                                        .border_color(rgb(theme.border))
                                        .font_family_opt(theme.mono_family.clone())
                                        .text_color(rgb(theme.dimmed))
                                        .text_size(px(12.))
                                        .whitespace_nowrap()
                                        .child(keys.to_owned())
                                })),
                        ),
                );
            }
            let scrollable =
                rows as f32 * BINDING_ROW_HEIGHT + HEADER_HEIGHT + BODY_PADDING + 2. - height;
            let offset = if live {
                f32::from(view.scroll.offset().y)
            } else {
                0.
            };
            binding_fade = scrollable + offset > 1.;
        }
        Page::ConfirmModel { install } => {
            body = body.child(if install {
                install_plan(theme, view.model_root.as_deref())
            } else {
                div().px(px(10.)).py(px(6.)).text_size(px(13.)).line_height(px(19.))
                    .child("Fell will remove the local model files. Your mail stays on this device. Reopen Fell to return to keyword search.")
            });
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
        .relative()
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
        .when(binding_fade, |card| {
            card.child(super::help::edge(theme, false, 24.))
        })
}

fn install_plan(theme: &theme::Theme, root: Option<&Path>) -> gpui::Div {
    div()
        .px(px(10.))
        .py(px(8.))
        .flex()
        .flex_col()
        .gap(px(10.))
        .text_size(px(13.))
        .line_height(px(19.))
        .child(
            div()
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!(
                    "Hugging Face · {}",
                    embeddings::megabytes(MODEL.download_bytes())
                )))
                .child(div().text_color(rgb(theme.dimmed)).child(MODEL.title)),
        )
        .child(
            div()
                .p(px(8.))
                .rounded_sm()
                .border_1()
                .border_color(rgb(theme.border))
                .font_family_opt(theme.mono_family.clone())
                .text_size(px(12.))
                .children(MODEL.files().into_iter().map(|file| {
                    let (start, end) = file.sha256.split_at(8);
                    let end = &end[end.len() - 8..];
                    div()
                        .py(px(3.))
                        .child(div().child(format!(
                            "{} · {}",
                            file.name,
                            embeddings::megabytes(file.bytes)
                        )))
                        .child(
                            div()
                                .whitespace_nowrap()
                                .text_color(rgb(theme.dimmed))
                                .child(format!("SHA-256 {start}…{end}")),
                        )
                })),
        )
        .child(
            div()
                .text_color(rgb(theme.dimmed))
                .child(format!("License · {}", MODEL.license))
                .children(root.map(|root| {
                    div().child(format!(
                        "Saved to {} (owner-only)",
                        model_dir(&MODEL, root).display()
                    ))
                })),
        )
        .child(div().child(embeddings::INSTALL_EFFECT.replace('\n', " ")))
}

fn detail(
    theme: &theme::Theme,
    title: impl Into<gpui::SharedString>,
    explanation: &str,
    alert: bool,
    rows_follow: bool,
) -> gpui::Div {
    div()
        .flex_none()
        .px(px(20.))
        .py(px(13.))
        .when(rows_follow, |summary| {
            summary.border_b_1().border_color(rgb(theme.border))
        })
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .truncate()
                .child(title.into()),
        )
        .child(
            div()
                .text_size(px(12.))
                .line_height(px(17.))
                .text_color(rgb(if alert { theme.error } else { theme.dimmed }))
                .child(explanation.to_owned()),
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
        .min_h(px(height))
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
                    inbox.settings_click(index, crate::keymap::KeyId::Enter, cx);
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

fn preference_note(
    snapshot: &Snapshot,
    preference: settings::Preference,
    state: &str,
    width: f32,
) -> (String, bool) {
    let invalid = snapshot.invalid.iter().find(|(key, _)| *key == preference);
    match invalid {
        Some(_) if width < 320. => (format!("Invalid · using {state}"), true),
        Some((_, value)) => (format!("{value} isn't valid · using {state}"), true),
        None => (preference.description().to_owned(), false),
    }
}

fn chevron(
    theme: &theme::Theme,
    index: usize,
    previous: bool,
    live: bool,
    opacity: f32,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    use crate::keymap::KeyId;

    div()
        .id(if previous {
            "previous-value"
        } else {
            "next-value"
        })
        .w(px(11.))
        .flex_none()
        .text_center()
        .text_size(px(12.))
        .text_color(rgb(theme.dimmed))
        .opacity(opacity)
        .when(live, |control| {
            control.on_mouse_down(
                MouseButton::Left,
                cx.listener(move |inbox, _, _, cx| {
                    cx.stop_propagation();
                    inbox.settings_click(
                        index,
                        if previous { KeyId::Left } else { KeyId::Right },
                        cx,
                    );
                }),
            )
        })
        .child(if previous { "‹" } else { "›" })
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
    fn edit_keymap_creates_and_repairs_private_permissions() {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("fell");
        let path = ensure_keymap(&dir).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        fs::write(&path, "leader = ','").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(ensure_keymap(&dir).unwrap(), path);
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o600);
        assert_eq!(fs::read_to_string(path).unwrap(), "leader = ','");
    }

    #[test]
    fn keymap_permissions_leave_symlink_targets_alone() {
        use std::os::unix::fs::{MetadataExt, symlink};

        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("fell");
        fs::create_dir(&dir).unwrap();
        let owned = temp.path().join("preferences.toml");
        fs::write(&owned, "leader = ','").unwrap();
        fs::set_permissions(&owned, fs::Permissions::from_mode(0o644)).unwrap();
        let path = dir.join("keymap.toml");
        symlink(&owned, &path).unwrap();
        assert_eq!(ensure_keymap(&dir).unwrap(), path);
        assert!(path.is_symlink());
        assert_eq!(fs::metadata(&owned).unwrap().mode() & 0o777, 0o644);
    }

    #[test]
    fn account_setup_guard_rechecks_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("account.toml");
        assert!(!account_file_exists(Some(&path)));
        fs::write(&path, "").unwrap();
        assert!(account_file_exists(Some(&path)));
        fs::rename(&path, temp.path().join("account.bak")).unwrap();
        assert!(!account_file_exists(Some(&path)));
    }

    #[test]
    fn invalid_value_toast_clears_when_fixed_and_stays_inline_in_its_section() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("settings.toml");
        let defaults = Values::default();
        let apply = |inbox: &mut Inbox, text, cx: &mut Context<Inbox>| {
            fs::write(&path, text).unwrap();
            let snapshot = Snapshot::read(Some(&path), defaults);
            inbox.apply_settings(snapshot, inbox.theme.clone(), Motion::new(false), cx);
        };
        let (mut app, handle) = crate::view::tests::pending_inbox();
        handle
            .update(&mut app, |inbox, _, cx| {
                inbox.session.clear_toast();
                apply(inbox, "read_dwell_ms = 'soon'", cx);
                assert!(inbox.settings_view.notice_generation.is_some());
                assert!(inbox.session.toast.is_some());
                apply(inbox, "read_dwell_ms = 500", cx);
                assert!(inbox.session.toast.is_none());

                apply(inbox, "read_dwell_ms = 'soon'", cx);
                inbox.session.show_toast("Another action", false);
                let newer = inbox.session.toast_generation();
                apply(inbox, "read_dwell_ms = 500", cx);
                assert_eq!(inbox.session.toast_generation(), newer);
                assert!(inbox.session.toast.is_some());

                inbox.session.clear_toast();
                inbox.session.overlay = Overlay::Settings;
                inbox.session.settings_nav.page = Page::Section(Section::Reading);
                apply(inbox, "read_dwell_ms = 'soon'", cx);
                assert!(inbox.session.toast.is_none());
                assert_eq!(
                    inbox.settings_view.snapshot.invalid,
                    [(settings::Preference::ReadDwell, r#""soon""#.to_owned())]
                );
            })
            .unwrap();
    }

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
