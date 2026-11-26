//! Keep each mailbox and its view open so switching only changes the shown view.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use fell_core::accounts::{Accounts, FileIdentity, Registry, RemovalOutcome};
use futures::{FutureExt as _, StreamExt as _, channel::oneshot, future::Shared};
use gpui::{App, Context, Entity, IntoElement, Render, Window, div, prelude::*, rgb};

use super::{ExternalCompose, Inbox, InboxWindow, Onboarding, mailbox};
use crate::inbox_motion::Layer;
use crate::model::settings::AccountRemovalKind;
use crate::model::{Overlay, PickerChoice, SyncKind};
use crate::motion::{self, Transition};

pub(super) struct AccountWindow {
    active: Entity<Inbox>,
    active_id: Option<String>,
    views: HashMap<String, Entity<Inbox>>,
    stopping: HashSet<String>,
    reloading: HashMap<String, Entity<Inbox>>,
    catalog: Accounts,
    picker_clock: motion::Clock,
    picker_exit: Option<(Entity<Inbox>, Layer, Option<f32>, Transition)>,
    active_changed: (oneshot::Sender<()>, Shared<oneshot::Receiver<()>>),
    _compose_listener: Option<gpui::Task<()>>,
}

pub(super) struct RequestedRemoval {
    pub kind: AccountRemovalKind,
    pub path: PathBuf,
    pub expected: Option<FileIdentity>,
    pub delete: bool,
    pub name: String,
}

fn catalog(registry: &Registry) -> Accounts {
    match mailbox::store_path() {
        Ok(base) => registry.load_with_removals(&base),
        Err(_) => registry.load(),
    }
}

impl AccountWindow {
    pub fn new(
        startup: mailbox::Startup,
        demo: bool,
        setup: Option<Entity<Onboarding>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let active_id = startup.config.as_ref().map(|config| config.id.clone());
        let initial = startup.config.clone();
        let active = cx.new(|cx| {
            let mut inbox = Inbox::with_startup(startup, demo, Some(window), cx);
            if let Some(setup) = setup {
                inbox.account_setup_handoff(setup);
            }
            inbox
        });
        let views = active_id
            .iter()
            .map(|id| (id.clone(), active.clone()))
            .collect();
        let mut view = Self {
            active,
            active_id,
            views,
            stopping: HashSet::new(),
            reloading: HashMap::new(),
            catalog: Accounts::default(),
            picker_clock: motion::Clock::default(),
            picker_exit: None,
            active_changed: Self::active_signal(),
            _compose_listener: Self::watch_compose(window, cx),
        };
        view.focus(window, cx);
        cx.set_global(InboxWindow(window.window_handle()));
        cx.on_focus_lost(window, |view, window, cx| view.focus(window, cx))
            .detach();
        let mut viewport = (window.viewport_size(), window.scale_factor());
        cx.observe_window_bounds(window, move |_, window, cx| {
            let previous = viewport;
            viewport = (window.viewport_size(), window.scale_factor());
            if viewport != previous {
                window.defer(cx, |window, cx| window.draw(cx).clear());
            }
        })
        .detach();
        cx.observe_window_activation(window, |view, window, cx| {
            if window.is_window_active() {
                view.active.update(cx, |inbox, cx| {
                    inbox.refresh_settings_identities(cx);
                    cx.notify();
                });
            }
        })
        .detach();
        if let Ok(registry) = Registry::resolve() {
            let catalog = catalog(&registry);
            if let Some(config) = initial
                && !catalog.configs().any(|current| current == &config)
            {
                let reload = catalog.configs().any(|current| current.id == config.id);
                view.retire(&config.id, &registry, None, reload, window, cx);
            }
            view.reconcile(&registry, catalog, window, cx);
            view.watch(registry, window, cx);
        }
        view
    }

    pub fn active(&self) -> Entity<Inbox> {
        self.active.clone()
    }

    fn active_signal() -> (oneshot::Sender<()>, Shared<oneshot::Receiver<()>>) {
        let (send, receive) = oneshot::channel();
        (send, receive.shared())
    }

    fn signal_active_changed(&mut self) {
        let (send, _) = std::mem::replace(&mut self.active_changed, Self::active_signal());
        let _ = send.send(());
    }

    fn watch(&self, registry: Registry, window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let read = registry.clone();
                let accounts = cx
                    .background_executor()
                    .spawn(async move { catalog(&read) })
                    .await;
                if this
                    .update_in(cx, |view, window, cx| {
                        view.reconcile(&registry, accounts, window, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn watch_compose(window: &Window, cx: &mut Context<Self>) -> Option<gpui::Task<()>> {
        cx.try_global::<ExternalCompose>()?;
        let mut requests = cx.global_mut::<ExternalCompose>().0.take()?;
        let hub = cx.global::<crate::draft_host::AgentDrafts>().0.clone();
        Some(cx.spawn_in(window, async move |this, cx| {
            while let Some(request) = requests.next().await {
                let target = loop {
                    let Ok((target, ready, changed)) = this.update_in(cx, |view, _, cx| {
                        let inbox = view.active.read(cx);
                        (
                            view.active.downgrade(),
                            (!inbox.retired).then(|| inbox.compose_ready.clone()),
                            view.active_changed.1.clone(),
                        )
                    }) else {
                        return;
                    };
                    let Some(ready) = ready else {
                        let _ = changed.await;
                        continue;
                    };
                    if matches!(
                        futures::future::select(ready, changed).await,
                        futures::future::Either::Right(_)
                    ) {
                        continue;
                    }
                    let Ok(target) = this.update_in(cx, |view, _, cx| {
                        let inbox = view.active.read(cx);
                        if view.active.downgrade() != target || inbox.retired {
                            return None;
                        }
                        Some(inbox.draft_home.clone().map(|home| {
                            (
                                home,
                                inbox.account_id.clone(),
                                fell_core::compose::configured_from(
                                    &inbox.session.account_name,
                                    &inbox.session.account_email,
                                ),
                            )
                        }))
                    }) else {
                        return;
                    };
                    if let Some(target) = target {
                        break target;
                    }
                };
                let result = if let Some((home, account, from)) = target {
                    let hub = hub.clone();
                    let (fields, body) = request.input.into_fields_and_body(from);
                    let start = fell_core::compose::ReplyStart {
                        fields,
                        parts: Default::default(),
                    };
                    cx.background_executor()
                        .spawn(async move {
                            home.new_seeded(&hub, &account, start, body, request.files)
                        })
                        .await
                } else {
                    Err("The mailbox did not open, so a draft has nowhere to go".into())
                };
                match result {
                    Ok(id) => {
                        crate::draft_host::show_draft(&hub, &id);
                    }
                    Err(err) => {
                        let _ = this.update_in(cx, |view, _, cx| {
                            view.notice(format!("Couldn't start a draft. {err}"), true, cx);
                        });
                    }
                }
            }
        }))
    }

    fn reconcile(
        &mut self,
        registry: &Registry,
        accounts: Accounts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.catalog != accounts;
        if changed {
            let old_configs: Vec<_> = self.catalog.configs().cloned().collect();
            for old in old_configs {
                let Some(view) = self.views.get(&old.id) else {
                    continue;
                };
                let current = accounts.configs().find(|config| config.id == old.id);
                match current {
                    Some(current) if old.same_except_identities(current) => {
                        if old.identities != current.identities {
                            view.update(cx, |inbox, cx| inbox.set_settings_identities(current, cx));
                        }
                    }
                    _ => self.retire(&old.id, registry, None, current.is_some(), window, cx),
                }
            }
        }

        let mut added = false;
        for config in accounts.configs() {
            if self.views.contains_key(&config.id) || self.stopping.contains(&config.id) {
                continue;
            }
            let path = mailbox::store_path().and_then(|base| {
                registry
                    .store_path(&base, &config.id)
                    .map_err(|err| err.to_string())
            });
            let startup = mailbox::startup_for(Some(config.clone()), false, None, path);
            let previous = self.reloading.remove(&config.id);
            let active = self.active_id.as_deref() == Some(&config.id);
            let entity = cx.new(|cx| {
                let mut inbox = Inbox::with_startup(startup, false, Some(window), cx);
                if let Some(previous) = previous.as_ref() {
                    inbox.adopt_account_view(previous.read(cx));
                }
                if !active {
                    inbox.window_active = false;
                    if let Some(queue) = &inbox.notifications {
                        queue.set_viewing(false);
                    }
                }
                inbox
            });
            if active {
                self.active = entity.clone();
                self.signal_active_changed();
                self.focus(window, cx);
            }
            self.views.insert(config.id.clone(), entity);
            added = true;
        }
        self.reloading
            .retain(|id, _| accounts.configs().any(|config| config.id == *id));

        let removed = self
            .active_id
            .as_deref()
            .filter(|id| !self.views.contains_key(*id))
            .filter(|id| {
                !self.stopping.contains(*id) || !accounts.configs().any(|config| config.id == *id)
            })
            .map(|id| {
                (
                    id.to_owned(),
                    self.active.read(cx).session.account_name.clone(),
                )
            });
        let mut switched = false;
        if self.active_id.is_none() || removed.is_some() {
            let next = accounts
                .default_config()
                .filter(|config| self.views.contains_key(&config.id))
                .or_else(|| {
                    accounts
                        .configs()
                        .find(|config| self.views.contains_key(&config.id))
                });
            if let Some(next) = next {
                self.switch(&next.id, window, cx);
                switched = true;
            } else if self.active_id.take().is_some() {
                let startup =
                    mailbox::startup_for(None, false, None, Err("No account configured".into()));
                self.active = cx.new(|cx| Inbox::with_startup(startup, false, Some(window), cx));
                self.signal_active_changed();
                self.focus(window, cx);
                switched = true;
            }
        }
        if let Some((id, name)) = removed
            && !accounts.entries.iter().any(|entry| {
                entry
                    .path
                    .file_stem()
                    .is_some_and(|stem| stem == id.as_str())
            })
        {
            let next = &self.active.read(cx).session.account_name;
            let message = if self.active_id.is_some() {
                format!("Removed {name} · showing {next}")
            } else {
                format!("Removed {name}")
            };
            self.notice(message, false, cx);
        }

        self.catalog = accounts;
        self.update_choices(changed || switched || added, cx);
        if changed || switched || added {
            cx.notify();
        }
    }

    fn retire(
        &mut self,
        id: &str,
        registry: &Registry,
        remove: Option<RequestedRemoval>,
        reload: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let broken = remove
            .as_ref()
            .is_some_and(|request| request.kind == AccountRemovalKind::BrokenFile);
        if !broken && !self.stopping.insert(id.to_owned()) {
            return;
        }
        let removing_active = !broken && remove.is_some() && self.active_id.as_deref() == Some(id);
        let account = fell_core::AccountId(id.to_owned());
        let hub = cx
            .try_global::<crate::draft_host::AgentDrafts>()
            .map(|drafts| drafts.0.clone());
        let mut workers = Vec::new();
        let mut parts = None;
        if !broken && let Some(view) = self.views.remove(id) {
            cx.default_global::<crate::compose_ui::RecipientStore>()
                .0
                .remove(&account);
            cx.default_global::<crate::reply_check::ReplyChecks>()
                .0
                .remove(&account);
            if let Some(hub) = &hub {
                hub.remove_server_draft_wake(&account);
            }
            if self
                .picker_exit
                .as_ref()
                .is_some_and(|(outgoing, ..)| outgoing == &view)
            {
                self.picker_exit = None;
            }
            view.update(cx, |inbox, _| {
                inbox.retired = true;
                parts = Some(inbox.attachment_fetcher.clone());
                inbox.draft_home = None;
                inbox.reader_bodies.active = None;
                workers.extend(
                    inbox
                        .sync
                        .take()
                        .and_then(crate::sync_worker::SyncHandle::stop),
                );
                workers.extend(
                    inbox
                        .reader_fetcher
                        .take()
                        .map(crate::reader_body_worker::Handle::stop),
                );
                workers.extend(inbox.store.stop());
            });
            if self.active_id.as_deref() == Some(id) {
                self.signal_active_changed();
            }
            if reload {
                self.reloading.insert(id.to_owned(), view);
            }
        } else if !broken && let Some(hub) = &hub {
            hub.remove_server_draft_wake(&account);
        }
        let registry = registry.clone();
        let id = id.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let read = registry.clone();
            let retired = id.clone();
            let (result, catalog) = cx
                .background_executor()
                .spawn(async move {
                    if let Some(parts) = parts {
                        parts.stop();
                    }
                    for worker in workers {
                        let _ = worker.join();
                    }
                    let result = remove.map(|request| {
                        let run = || {
                            if request.kind == AccountRemovalKind::BrokenFile {
                                read.remove_broken_file(
                                    &request.path,
                                    request.expected.ok_or("The account file changed.")?,
                                )?;
                                return Ok(None);
                            }
                            if request.delete
                                && let Some(hub) = hub
                            {
                                hub.release_account_drafts(&account)?;
                            }
                            let base = mailbox::store_path()?;
                            match request.kind {
                                AccountRemovalKind::Interrupted { .. } => read.recover_removal(
                                    &base,
                                    &retired,
                                    request.delete,
                                    request.expected,
                                ),
                                _ => read.remove_if_unchanged(
                                    &base,
                                    &retired,
                                    request.delete,
                                    request.expected,
                                ),
                            }
                            .map(Some)
                        };
                        (request.name.clone(), run())
                    });
                    (result, catalog(&read))
                })
                .await;
            let _ = this.update_in(cx, |view, window, cx| {
                if !broken {
                    view.stopping.remove(&id);
                }
                view.reconcile(&registry, catalog, window, cx);
                if let Some((name, result)) = result {
                    let message = match result {
                        Ok(Some(outcome @ (RemovalOutcome::Kept | RemovalOutcome::Deleted))) => {
                            let removed = if removing_active && view.active_id.is_some() {
                                let next = &view.active.read(cx).session.account_name;
                                format!("Removed {name} · showing {next}")
                            } else {
                                format!("Removed {name}")
                            };
                            let action = if outcome == RemovalOutcome::Kept {
                                "kept"
                            } else {
                                "deleted"
                            };
                            format!("{removed} · local mail {action}")
                        }
                        Ok(Some(RemovalOutcome::Restored { connected: true })) => {
                            format!("Restored local mail for {name}")
                        }
                        Ok(Some(RemovalOutcome::Restored { connected: false })) => {
                            format!("Restored local mail for {name} · account disconnected")
                        }
                        Ok(Some(RemovalOutcome::Cleared)) => {
                            format!("Cleared interrupted deletion for {name}")
                        }
                        Ok(None) => format!("Deleted {name}"),
                        Err(err) => {
                            view.notice(format!("Couldn't finish account action. {err}"), true, cx);
                            return;
                        }
                    };
                    view.notice(message, false, cx);
                }
            });
        })
        .detach();
    }

    fn notice(&self, message: String, error: bool, cx: &mut App) {
        self.active.update(cx, |inbox, cx| {
            inbox.session.show_toast(message, error);
            inbox.dismiss_notice_after(
                inbox.session.toast_generation(),
                Duration::from_secs(5),
                cx,
            );
            cx.notify();
        });
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.active.read(cx).focus);
    }

    fn update_choices(&self, catalog_changed: bool, cx: &mut App) {
        let statuses: HashMap<_, _> = self
            .views
            .iter()
            .map(|(id, view)| {
                let session = &view.read(cx).session;
                let status = match &session.sync {
                    SyncKind::Failed(failure) => (failure.detail.clone(), true),
                    SyncKind::StoreError => (
                        session
                            .startup_error
                            .clone()
                            .unwrap_or_else(|| "Mailbox unavailable".into()),
                        true,
                    ),
                    _ if self.active_id.as_ref() == Some(id) => ("Active".into(), false),
                    _ => (
                        session.last_sync_at.map_or_else(
                            || "Waiting for first sync".into(),
                            |at| {
                                let time = crate::present::activity_time(super::now_unix(), at);
                                format!(
                                    "Last sync {}",
                                    time.strip_prefix("Today · ").unwrap_or(&time)
                                )
                            },
                        ),
                        false,
                    ),
                };
                (id.clone(), status)
            })
            .collect();
        let error = self
            .catalog
            .error
            .as_ref()
            .map(|error| error.detail.clone())
            .or_else(|| {
                self.catalog.entries.iter().find_map(|entry| {
                    let err = entry.config.as_ref().err()?;
                    let name = entry.path.file_name()?.to_string_lossy();
                    Some(format!("{name} · {}", err.reason(&entry.path)))
                })
            });
        for (id, view) in &self.views {
            self.set_choices(
                view,
                Some(id),
                &statuses,
                error.as_deref(),
                catalog_changed,
                cx,
            );
        }
        if self
            .active_id
            .as_ref()
            .is_none_or(|id| !self.views.contains_key(id))
        {
            self.set_choices(
                &self.active,
                self.active_id.as_ref(),
                &statuses,
                error.as_deref(),
                catalog_changed,
                cx,
            );
        }
    }

    fn set_choices(
        &self,
        view: &Entity<Inbox>,
        current: Option<&String>,
        statuses: &HashMap<String, (String, bool)>,
        error: Option<&str>,
        catalog_changed: bool,
        cx: &mut App,
    ) {
        view.update(cx, |inbox, cx| {
            if catalog_changed {
                inbox.session.account_choices = self
                    .catalog
                    .configs()
                    .map(|config| {
                        let account = config.account();
                        PickerChoice {
                            name: config.id.clone(),
                            title: account.display_name,
                            detail: Some(account.email),
                            date: None,
                            mark: (current == Some(&config.id)).then_some("✓"),
                            divider: false,
                        }
                    })
                    .collect();
                inbox.session.account_config_error = error.map(str::to_owned);
            }
            if inbox.settings_view.set_accounts(
                &self.catalog,
                current.map(String::as_str),
                statuses,
            ) || catalog_changed
            {
                inbox.refresh_settings_availability();
                cx.notify();
            }
        });
    }

    pub fn switch(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_id.as_deref() == Some(id) && self.views.get(id) == Some(&self.active) {
            return;
        }
        if let Some(next) = self.views.get(id).cloned() {
            let frames = self.active.read(cx).frames.clone();
            let outgoing = self.active.clone();
            self.picker_clock.idle();
            let fade_now = self.picker_clock.now();
            self.picker_exit = outgoing.update(cx, |inbox, _| {
                inbox.session.cancel_read_delay();
                if inbox.session.overlay != Overlay::None {
                    inbox.session.dismiss_overlay();
                }
                inbox.window_active = false;
                if let Some(queue) = &inbox.notifications {
                    queue.set_viewing(false);
                }
                let now = inbox.motion.clock.now();
                let picker = inbox.motion.settle_account_switch(now);
                let fade = inbox.motion.motion.start(motion::FADE, fade_now);
                picker
                    .zip(fade)
                    .map(|((layer, height), fade)| (outgoing.clone(), layer, height, fade))
            });
            self.active = next;
            self.active_id = Some(id.to_owned());
            self.signal_active_changed();
            self.active.update(cx, |inbox, cx| {
                inbox.frames = frames;
                inbox.notification_window_activation(window, cx);
                inbox.ignore_enter_until = Some(Instant::now() + Duration::from_millis(150));
                inbox.motion.clock.idle();
                inbox.account_arrival = inbox
                    .motion
                    .motion
                    .start(motion::FADE, inbox.motion.clock.now());
                cx.notify();
            });
            self.focus(window, cx);
            self.update_choices(false, cx);
            cx.notify();
        }
    }

    fn picker_opacity(&mut self, real: Instant) -> Option<f32> {
        let now = self.picker_clock.frame(real);
        self.picker_exit = self
            .picker_exit
            .take()
            .filter(|(_, _, _, fade)| fade.running(now));
        self.picker_exit
            .as_ref()
            .map(|(_, _, _, fade)| fade.remaining(now))
    }
}

impl Render for AccountWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let picker_opacity = self.picker_opacity(Instant::now());
        let theme = &self.active.read(cx).theme;
        let background = theme.background;
        let picker = self.picker_exit.as_mut().zip(picker_opacity).map(
            |((outgoing, layer, height, _), opacity)| {
                let entity = cx.entity().downgrade();
                window.on_next_frame(move |_, cx| {
                    let _ = entity.update(cx, |_, cx| cx.notify());
                });
                outgoing.update(cx, |inbox, cx| {
                    layer.swap(&mut inbox.session);
                    let picker = super::overlay(
                        &inbox.theme,
                        &inbox.session,
                        (opacity, 0.0),
                        *height,
                        false,
                        (0.0, super::CARD_TOP_PX),
                        &inbox.palette_scroll,
                        &inbox.folder_scroll,
                        super::PickerCard {
                            choices: &layer.picker_choices,
                            prompt: &layer.folder_prompt,
                            target: layer.folder_target.as_deref(),
                            mark_opacity: layer.label_marks,
                            merge_feedback: 0.0,
                        },
                        None,
                        None,
                        window,
                        cx,
                    );
                    layer.swap(&mut inbox.session);
                    picker
                })
            },
        );
        div()
            .relative()
            .size_full()
            .bg(rgb(background))
            .child(self.active.clone())
            .children(picker)
    }
}

pub(super) fn switch(id: &str, window: &mut Window, cx: &mut App) {
    if let Some(root) = window.root::<AccountWindow>().flatten() {
        root.update(cx, |view, cx| view.switch(id, window, cx));
    }
}

pub(super) fn remove(id: &str, removal: RequestedRemoval, window: &mut Window, cx: &mut App) {
    if let Some(root) = window.root::<AccountWindow>().flatten()
        && let Ok(registry) = Registry::resolve()
    {
        root.update(cx, |view, cx| {
            view.retire(id, &registry, Some(removal), false, window, cx);
        });
    }
}

pub(super) fn active_inbox(window: &Window, cx: &App) -> Option<Entity<Inbox>> {
    window
        .root::<AccountWindow>()
        .flatten()
        .map(|root| root.read(cx).active())
        .or_else(|| window.root::<Inbox>().flatten())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeyId;
    use gpui::TestAppContext;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    fn write_account(registry: &Registry, id: &str, suffix: &str) {
        let path = registry.path(id).unwrap();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        writeln!(
            file,
            "email = '{id}@example.test'\npassword_command = 'exit 1'\n{suffix}"
        )
        .unwrap();
    }

    #[test]
    fn replaced_accounts_wait_for_the_previous_worker_before_reopening() {
        let Some(_temp) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        write_account(&registry, "mail", "");
        write_account(&registry, "work", "");
        let mut app = TestAppContext::single();
        let window = app.add_window(|window, cx| {
            AccountWindow::new(mailbox::startup(), false, None, window, cx)
        });
        let old = window
            .update(&mut app, |view, _, _| view.views["work"].clone())
            .unwrap();
        let store = old.read_with(&app, |inbox, _| inbox.store.clone());
        futures::executor::block_on(store.call(|_| ())).unwrap();
        app.run_until_parked();
        let check_position = |inbox: &Inbox| {
            assert_eq!(inbox.session.folder, "Sent");
            assert_eq!(inbox.session.cursor_id.as_deref(), Some("selected-thread"));
            assert_eq!(inbox.session.search.as_ref().unwrap().query, "trail");
        };
        window
            .update(&mut app, |view, window, cx| {
                view.switch("work", window, cx);
                old.update(cx, |inbox, _| {
                    inbox.session.folder = "Sent".into();
                    inbox.session.search = Some(Default::default());
                    inbox.session.search.as_mut().unwrap().query = "trail".into();
                    inbox.session.cursor_id = Some("selected-thread".into());
                });
            })
            .unwrap();

        mailbox::import_identities(
            &fell_core::AccountId("work".into()),
            vec!["alias@example.test".into()],
        )
        .unwrap();
        window
            .update(&mut app, |view, window, cx| {
                view.reconcile(&registry, registry.load(), window, cx);
                assert_eq!(view.active(), old);
                assert!(view.stopping.is_empty());
                check_position(old.read(cx));
            })
            .unwrap();
        futures::executor::block_on(store.call(|_| ())).unwrap();

        write_account(&registry, "work", "name = 'New work'");
        window
            .update(&mut app, |view, window, cx| {
                view.reconcile(&registry, registry.load(), window, cx);
                assert!(view.stopping.contains("work"));
                assert!(!view.views.contains_key("work"));
                assert!(old.read(cx).retired);
                assert_eq!(view.active(), old);
                assert_eq!(view.active_id.as_deref(), Some("work"));
                check_position(old.read(cx));
            })
            .unwrap();
        app.run_until_parked();
        window
            .update(&mut app, |view, _, cx| {
                let next = &view.views["work"];
                assert_ne!(next, &old);
                assert_eq!(&view.active(), next);
                assert_eq!(next.read(cx).session.account_name, "New work");
                check_position(next.read(cx));
                assert!(!view.stopping.contains("work"));
            })
            .unwrap();
        assert!(futures::executor::block_on(store.call(|_| ())).is_err());
    }

    #[test]
    fn mailto_waits_for_the_new_active_account() {
        let Some(_temp) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        write_account(&registry, "mail", "");
        write_account(&registry, "work", "");
        let mut app = TestAppContext::single();
        let hub = std::sync::Arc::new(fell_core::compose::DraftHub::new());
        let (mut send, requests) = futures::channel::mpsc::channel(4);
        app.update(|cx| {
            cx.set_global(ExternalCompose(Some(requests)));
            cx.set_global(crate::draft_host::AgentDrafts(hub.clone()));
        });
        let window = app.add_window(|window, cx| {
            AccountWindow::new(mailbox::startup(), false, None, window, cx)
        });
        let clients = window
            .update(&mut app, |view, _, cx| {
                view.views
                    .values()
                    .map(|inbox| inbox.read(cx).store.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap();
        for client in clients {
            futures::executor::block_on(client.call(|_| ())).unwrap();
        }
        app.run_until_parked();
        let first = window.update(&mut app, |view, _, _| view.active()).unwrap();
        let (first_ready, first_wait) = oneshot::channel();
        let (second_ready, second_wait) = oneshot::channel();
        window
            .update(&mut app, |view, _, cx| {
                first.update(cx, |inbox, _| inbox.compose_ready = first_wait.shared());
                view.views["work"]
                    .update(cx, |inbox, _| inbox.compose_ready = second_wait.shared());
            })
            .unwrap();
        send.try_send(crate::compose_launch::mailto("mailto:friend@example.test").unwrap())
            .unwrap();
        app.run_until_parked();
        assert!(hub.focused_id().is_none());

        window
            .update(&mut app, |view, window, cx| view.switch("work", window, cx))
            .unwrap();
        let _ = first_ready.send(());
        app.run_until_parked();
        assert!(hub.focused_id().is_none());

        second_ready.send(()).unwrap();
        app.run_until_parked();
        let draft = hub.focused_id().unwrap();
        assert_eq!(
            hub.with_session(&draft, |draft| draft.account_id().0.clone())
                .as_deref(),
            Some("work")
        );
    }

    #[test]
    fn account_picker_and_inbox_fade_on_every_switch() {
        let Some(_temp) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        write_account(&registry, "mail", "");
        write_account(&registry, "work", "");
        let mut app = TestAppContext::single();
        let window = app.add_window(|window, cx| {
            AccountWindow::new(mailbox::startup(), false, None, window, cx)
        });
        for id in ["work", "mail", "work"] {
            window
                .update(&mut app, |view, _, cx| {
                    let current = view.active();
                    current.update(cx, |inbox, cx| {
                        inbox.motion.motion = motion::Motion::new(true);
                        inbox.session.on_key(KeyId::Char('g'), 10);
                        inbox.session.on_key(KeyId::Char('A'), 10);
                        assert_eq!(inbox.session.overlay, Overlay::Folders);
                        cx.notify();
                    });
                })
                .unwrap();
            app.update(|cx| {
                gpui::AnyWindowHandle::from(window)
                    .update(cx, |_, window, cx| window.draw(cx).clear())
                    .unwrap();
            });
            window
                .update(&mut app, |view, window, cx| {
                    let stale = Instant::now() - Duration::from_secs(1);
                    for inbox in [&view.active, &view.views[id]] {
                        inbox.update(cx, |inbox, _| {
                            inbox.motion.motion = motion::Motion::new(true);
                            inbox.motion.clock.idle();
                            inbox.motion.clock.now_at(stale);
                        });
                    }
                    view.switch(id, window, cx);
                    let active = view.active();
                    let first = Instant::now();
                    let opacity = |real, cx: &mut App| {
                        active.update(cx, |inbox, _| {
                            let now = inbox.motion.clock.frame(real);
                            inbox.account_opacity(now)
                        })
                    };
                    let mut arrival = opacity(first, cx);
                    let mut exit = view.picker_opacity(first).unwrap();
                    assert!(arrival < 1.0 && exit > 0.9);
                    for millis in [20, 40, 60, 80] {
                        let frame = first + Duration::from_millis(millis);
                        let next_arrival = opacity(frame, cx);
                        let next_exit = view.picker_opacity(frame).unwrap();
                        assert!(next_arrival > arrival && next_arrival < 1.0);
                        assert!(next_exit < exit && next_exit > 0.0);
                        arrival = next_arrival;
                        exit = next_exit;
                    }
                    let _ = opacity(first + Duration::from_millis(100), cx);
                    let _ = view.picker_opacity(first + Duration::from_millis(100));
                    let last = first + motion::FADE;
                    assert_eq!(opacity(last, cx), 1.0);
                    assert!(view.picker_opacity(last).is_none());
                })
                .unwrap();
        }
    }

    #[test]
    fn compose_listener_routes_again_after_its_first_account_is_removed() {
        let Some(_temp) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        write_account(&registry, "mail", "");
        write_account(&registry, "work", "");
        let mut app = TestAppContext::single();
        let hub = std::sync::Arc::new(fell_core::compose::DraftHub::new());
        let (send, requests) = futures::channel::mpsc::channel(4);
        app.update(|cx| {
            cx.set_global(ExternalCompose(Some(requests)));
            cx.set_global(crate::draft_host::AgentDrafts(hub.clone()));
        });
        let window = app.add_window(|window, cx| {
            AccountWindow::new(mailbox::startup(), false, None, window, cx)
        });
        let clients = window
            .update(&mut app, |view, _, cx| {
                view.views
                    .values()
                    .map(|view| view.read(cx).store.clone())
                    .collect::<Vec<_>>()
            })
            .unwrap();
        for client in clients {
            futures::executor::block_on(client.call(|_| ())).unwrap();
        }
        app.run_until_parked();
        let mut send = send;
        send.try_send(crate::compose_launch::mailto("mailto:first@example.test").unwrap())
            .unwrap();
        app.run_until_parked();
        let first = hub.focused_id().unwrap();
        assert_eq!(
            hub.with_session(&first, |draft| draft.account_id().0.clone())
                .as_deref(),
            Some("mail")
        );

        registry
            .remove(&mailbox::store_path().unwrap(), "mail", false)
            .unwrap();
        window
            .update(&mut app, |view, window, cx| {
                view.reconcile(&registry, registry.load(), window, cx);
                assert_eq!(view.active_id.as_deref(), Some("work"));
            })
            .unwrap();
        send.try_send(crate::compose_launch::cli(&["--to=second@example.test".into()]).unwrap())
            .unwrap();
        app.run_until_parked();
        let second = hub.focused_id().unwrap();
        assert_ne!(first, second);
        assert_eq!(
            hub.with_session(&second, |draft| draft.account_id().0.clone())
                .as_deref(),
            Some("work")
        );
    }

    #[test]
    fn switching_restores_each_accounts_folder_selection_and_search() {
        let Some(_temp) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        for id in ["mail", "work"] {
            let mut file = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(registry.path(id).unwrap())
                .unwrap();
            writeln!(
                file,
                "email = '{id}@example.test'\npassword_command = 'exit 1'"
            )
            .unwrap();
        }

        let mut app = TestAppContext::single();
        let window = app.add_window(|window, cx| {
            AccountWindow::new(mailbox::startup(), false, None, window, cx)
        });
        app.update(|cx| {
            window
                .update(cx, |view, window, cx| {
                    let personal = view.active();
                    personal.update(cx, |inbox, _| {
                        inbox.session.folder = "Sent".into();
                        inbox.session.on_key(KeyId::Char('/'), 10);
                        inbox.session.on_key(KeyId::Char('x'), 10);
                        inbox.session.on_key(KeyId::Char('g'), 10);
                        inbox.session.on_key(KeyId::Enter, 10);
                        inbox.session.cursor_id = Some("personal-thread".into());
                        assert_eq!(inbox.session.search.as_ref().unwrap().query, "xg");
                        inbox.session.on_key(KeyId::Char('g'), 10);
                        inbox.session.on_key(KeyId::Char('A'), 10);
                        inbox.session.on_key(KeyId::Char('w'), 10);
                        assert_eq!(
                            inbox.session.confirm_folder().switch_account.as_deref(),
                            Some("work")
                        );
                    });
                    view.switch("work", window, cx);
                    let work = view.active();
                    assert_ne!(personal, work);
                    work.update(cx, |inbox, _| {
                        assert!(inbox.session.search.is_none());
                        inbox.session.folder = "Drafts".into();
                        inbox.session.cursor_id = Some("work-thread".into());
                        inbox.session.on_key(KeyId::Space, 10);
                        inbox.session.on_key(KeyId::Char('A'), 10);
                        inbox.session.on_key(KeyId::Char('m'), 10);
                        assert_eq!(
                            inbox.session.confirm_folder().switch_account.as_deref(),
                            Some("mail")
                        );
                    });
                    view.switch("mail", window, cx);
                    let personal = view.active();
                    let session = &personal.read(cx).session;
                    assert_eq!(session.folder, "Sent");
                    assert_eq!(session.cursor_id.as_deref(), Some("personal-thread"));
                    assert_eq!(session.search.as_ref().unwrap().query, "xg");
                    view.switch("work", window, cx);
                    let work = view.active();
                    assert_eq!(work.read(cx).session.folder, "Drafts");
                    assert_eq!(
                        work.read(cx).session.cursor_id.as_deref(),
                        Some("work-thread")
                    );
                })
                .unwrap();
        });
    }
}
