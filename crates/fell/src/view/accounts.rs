//! Keep each mailbox and its view open so switching only changes the shown view.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fell_core::accounts::{Accounts, FileIdentity, Registry, RemovalOutcome};
use fell_core::compose::AccountDraftRemoval;
use futures::{FutureExt as _, StreamExt as _, channel::oneshot, future::Shared};
use gpui::{App, Context, Entity, IntoElement, Render, Window, div, prelude::*, rgb};

use super::{
    DRAFT_MAILBOX_UNAVAILABLE, ExternalCompose, FontOpt, Inbox, InboxWindow, Onboarding, mailbox,
};
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
    preparing_removal: HashSet<String>,
    removing: HashMap<String, (RequestedRemoval, Option<AccountDraftRemoval>)>,
    catalog: Accounts,
    picker_clock: motion::Clock,
    picker_exit: Option<PickerExit>,
    active_changed: (oneshot::Sender<()>, Shared<oneshot::Receiver<()>>),
    _compose_listener: Option<gpui::Task<()>>,
}

struct PickerExit {
    outgoing: Entity<Inbox>,
    layer: Layer,
    height: Option<f32>,
    fade: Transition,
    settings: Option<super::settings::FrozenAccounts>,
}

#[derive(Clone)]
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
            preparing_removal: HashSet::new(),
            removing: HashMap::new(),
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
            view.reconcile(&registry, catalog, None, window, cx);
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
                        view.reconcile(&registry, accounts, None, window, cx);
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
                let result = match target {
                    Ok((home, account, from)) => {
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
                    }
                    Err(err) => Err(err),
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
        mut accounts: Accounts,
        preferred: Option<&Path>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        accounts.retain_temporarily_unavailable(&self.catalog);
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
        self.reloading.retain(|id, _| {
            self.removing.contains_key(id) || accounts.configs().any(|config| config.id == *id)
        });

        let removed = self
            .active_id
            .as_deref()
            .filter(|id| !self.views.contains_key(*id))
            .filter(|id| !self.removing.contains_key(*id))
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
            let usable = |config: &fell_core::AccountConfig| {
                self.views
                    .get(&config.id)
                    .is_some_and(|view| view.read(cx).session.sync != SyncKind::StoreError)
            };
            let next = if let Some(path) = preferred {
                super::settings::selected_account_path(&accounts, path, usable)
                    .and_then(|path| accounts.entries.iter().find(|entry| entry.path == path))
                    .and_then(|entry| entry.config.as_ref().ok())
            } else {
                accounts
                    .default_config()
                    .filter(|config| usable(config))
                    .or_else(|| accounts.configs().find(|config| usable(config)))
            };
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
            && accounts.interrupted.iter().all(|pending| pending.id != id)
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
        if let Some(pending) = accounts.interrupted.iter().find(|pending| {
            self.catalog
                .interrupted
                .iter()
                .all(|old| old.id != pending.id)
        }) {
            self.notice(
                format!(
                    "Deletion interrupted for {} · Settings → Accounts",
                    pending.name
                ),
                true,
                cx,
            );
        }

        self.catalog = accounts;
        self.update_choices(changed || switched || added, cx);
        if changed || switched || added {
            cx.notify();
        }
    }

    fn request_removal(
        &mut self,
        id: &str,
        registry: &Registry,
        removal: RequestedRemoval,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        if self.removing.contains_key(id) || self.preparing_removal.contains(id) {
            return;
        }
        let hub = cx
            .try_global::<crate::draft_host::AgentDrafts>()
            .map(|drafts| drafts.0.clone());
        if let Some(hub) =
            hub.filter(|_| removal.delete && removal.kind != AccountRemovalKind::BrokenFile)
        {
            self.preparing_removal.insert(id.to_owned());
            self.update_choices(false, cx);
            let account = fell_core::AccountId(id.to_owned());
            let registry = registry.clone();
            cx.spawn_in(window, async move |this, cx| {
                let id = account.0.clone();
                let drafts = cx
                    .background_executor()
                    .spawn(async move { hub.release_account_drafts(&account) })
                    .await;
                let _ = this.update_in(cx, |view, window, cx| {
                    view.preparing_removal.remove(&id);
                    match drafts {
                        Ok(drafts) => view.retire(
                            &id,
                            &registry,
                            Some((removal, Some(drafts))),
                            false,
                            window,
                            cx,
                        ),
                        Err(error) => {
                            view.update_choices(false, cx);
                            view.notice(
                                format!("Couldn't remove {} · {error}", removal.name),
                                true,
                                cx,
                            );
                        }
                    }
                });
            })
            .detach();
        } else {
            self.retire(id, registry, Some((removal, None)), false, window, cx);
        }
    }

    fn retire(
        &mut self,
        id: &str,
        registry: &Registry,
        mut remove: Option<(RequestedRemoval, Option<AccountDraftRemoval>)>,
        reload: bool,
        window: &Window,
        cx: &mut Context<Self>,
    ) {
        let broken = remove
            .as_ref()
            .is_some_and(|(request, _)| request.kind == AccountRemovalKind::BrokenFile);
        let keep_view = reload || remove.is_some();
        if !broken {
            if let Some(request) = remove.take() {
                self.removing.entry(id.to_owned()).or_insert(request);
                self.update_choices(false, cx);
            }
            if !self.stopping.insert(id.to_owned()) {
                return;
            }
        }
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
                .is_some_and(|exit| exit.outgoing == view)
            {
                self.picker_exit = None;
            }
            view.update(cx, |inbox, _| {
                inbox.retired = true;
                if let Some(queue) = &inbox.notifications {
                    queue.set_viewing(false);
                }
                parts = Some(inbox.attachment_fetcher.clone());
                inbox.draft_home = Err(DRAFT_MAILBOX_UNAVAILABLE.into());
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
            if keep_view {
                self.reloading.insert(id.to_owned(), view);
            }
        } else if !broken && let Some(hub) = &hub {
            hub.remove_server_draft_wake(&account);
        }
        let registry = registry.clone();
        let id = id.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let (request, drafts, removing_active) = if broken {
                (remove.expect("broken removal has a request").0, None, false)
            } else {
                let read = registry.clone();
                let catalog = cx
                    .background_executor()
                    .spawn(async move {
                        if let Some(parts) = parts {
                            parts.stop();
                        }
                        for worker in workers {
                            let _ = worker.join();
                        }
                        catalog(&read)
                    })
                    .await;
                let Ok(Some(pending)) = this.update_in(cx, |view, window, cx| {
                    if let Some((request, drafts)) = view.removing.get_mut(&id) {
                        return Some((
                            request.clone(),
                            drafts.take(),
                            view.active_id.as_deref() == Some(&id),
                        ));
                    }
                    view.stopping.remove(&id);
                    view.reconcile(&registry, catalog, None, window, cx);
                    None
                }) else {
                    return;
                };
                pending
            };
            let read = registry.clone();
            let retired = id.clone();
            let name = request.name.clone();
            let path = request.path.clone();
            let (result, catalog) = cx
                .background_executor()
                .spawn(async move {
                    let result = {
                        let run = || {
                            if request.kind == AccountRemovalKind::BrokenFile {
                                read.remove_broken_file(
                                    &request.path,
                                    request.expected.ok_or("The account file changed.")?,
                                )?;
                                return Ok(None);
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
                        run()
                    };
                    drop(drafts);
                    (result, catalog(&read))
                })
                .await;
            let _ = this.update_in(cx, |view, window, cx| {
                use crate::model::settings::{Page, Section};
                let active = view.active.read(cx);
                let selection = (result.is_ok()
                    && active.session.overlay == Overlay::Settings
                    && active.session.settings_nav.page == Page::Section(Section::Accounts))
                .then(|| {
                    active
                        .settings_view
                        .account_selection(&active.session.settings_nav)
                })
                .flatten();
                let preferred = (matches!(
                    result,
                    Ok(Some(RemovalOutcome::Kept | RemovalOutcome::Deleted))
                ) && view.active_id.as_deref() == Some(id.as_str()))
                .then_some(path.as_path());
                if !broken {
                    view.stopping.remove(&id);
                    view.removing.remove(&id);
                    if result.is_ok() {
                        view.reloading.remove(&id);
                    }
                }
                view.reconcile(&registry, catalog, preferred, window, cx);
                let removed_active = preferred.is_some();
                if let Some((index, mut path)) = selection {
                    if removed_active
                        && let Some(id) = &view.active_id
                        && let Ok(active_path) = registry.path(id)
                    {
                        path = active_path;
                    }
                    view.active.update(cx, |inbox, cx| {
                        inbox.session.overlay = Overlay::Settings;
                        inbox.settings_view.restore_account_selection(
                            &mut inbox.session.settings_nav,
                            (index, path),
                        );
                        cx.notify();
                    });
                }
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
                        format!("Restored {name} · account and local mail")
                    }
                    Ok(Some(RemovalOutcome::Restored { connected: false })) => {
                        format!("Restored local mail for {name} · account disconnected")
                    }
                    Ok(Some(RemovalOutcome::Cleared)) => {
                        format!("Cleared interrupted deletion for {name}")
                    }
                    Ok(None) => format!("Deleted {name}"),
                    Err(err) => {
                        let hint = if err.contains("Close Fell and any fell-mcp sessions") {
                            "close other Fell or fell-mcp sessions, then retry"
                        } else {
                            err.as_str()
                        };
                        view.notice(format!("Couldn't remove {name} · {hint}"), true, cx);
                        return;
                    }
                };
                view.notice(message, false, cx);
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
            .map(|error| error.detail.as_str())
            .or_else(|| {
                if self.catalog.configs().next().is_some() {
                    return None;
                }
                self.catalog.entries.iter().find_map(|entry| {
                    let err = entry.config.as_ref().err()?;
                    Some(err.detail.as_str())
                })
            });
        for view in self.views.values() {
            self.set_choices(view, &statuses, error, catalog_changed, cx);
        }
        if self
            .active_id
            .as_ref()
            .is_none_or(|id| !self.views.contains_key(id))
        {
            self.set_choices(&self.active, &statuses, error, catalog_changed, cx);
        }
    }

    fn set_choices(
        &self,
        view: &Entity<Inbox>,
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
                            mark: (self.active_id.as_ref() == Some(&config.id)).then_some("✓"),
                            divider: false,
                        }
                    })
                    .collect();
                inbox.session.account_config_error = error.map(str::to_owned);
            }
            if inbox.settings_view.set_accounts(
                &self.catalog,
                self.active_id.as_deref(),
                statuses,
                |id| self.preparing_removal.contains(id) || self.removing.contains_key(id),
            ) || catalog_changed
            {
                inbox.refresh_settings_availability();
                cx.notify();
            }
        });
    }

    pub fn switch(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.switch_with_input(id, false, window, cx);
    }

    fn switch_with_input(
        &mut self,
        id: &str,
        confirmed_with_enter: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active_id.as_deref() == Some(id) && self.views.get(id) == Some(&self.active) {
            return;
        }
        if let Some(next) = self.views.get(id).cloned() {
            let frames = self.active.read(cx).frames.clone();
            let account_text = &self.active.read(cx).session;
            let header_leaving = (
                account_text.account_name.clone(),
                account_text.account_email.clone(),
            );
            let outgoing = self.active.clone();
            self.picker_clock.idle();
            let fade_now = self.picker_clock.now();
            self.picker_exit = outgoing.update(cx, |inbox, _| {
                let now = inbox.motion.clock.now();
                let layer = inbox.motion.settle_account_switch(&inbox.session, now);
                let settings = (inbox.session.overlay == Overlay::Settings)
                    .then(|| inbox.settings_view.freeze_accounts());
                inbox.session.cancel_read_delay();
                if inbox.session.overlay != Overlay::None {
                    inbox.session.dismiss_overlay();
                }
                inbox.window_active = false;
                if let Some(queue) = &inbox.notifications {
                    queue.set_viewing(false);
                }
                let fade = inbox.motion.motion.start(motion::FADE, fade_now);
                layer.zip(fade).map(|((layer, height), fade)| PickerExit {
                    outgoing: outgoing.clone(),
                    layer,
                    height,
                    fade,
                    settings,
                })
            });
            self.active = next;
            self.active_id = Some(id.to_owned());
            self.signal_active_changed();
            self.active.update(cx, |inbox, cx| {
                inbox.frames = frames;
                inbox.notification_window_activation(window, cx);
                let now = Instant::now();
                inbox.switch_enter_guard =
                    confirmed_with_enter.then(|| super::AccountSwitchEnterGuard::new(now));
                inbox.account_header_leaving = Some(header_leaving);
                inbox.start_account_arrival(now);
                cx.notify();
            });
            self.focus(window, cx);
            self.update_choices(true, cx);
            cx.notify();
        }
    }

    fn picker_opacity(&mut self, real: Instant) -> Option<f32> {
        let now = self.picker_clock.frame(real);
        self.picker_exit = self
            .picker_exit
            .take()
            .filter(|exit| exit.fade.running(now));
        self.picker_exit
            .as_ref()
            .map(|exit| exit.fade.remaining(now))
    }
}

impl Render for AccountWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let picker_opacity = self.picker_opacity(Instant::now());
        let theme = &self.active.read(cx).theme;
        let (background, foreground, ui_px, ui_family) = (
            theme.background,
            theme.foreground,
            theme.ui_px,
            theme.ui_family.clone(),
        );
        let picker = self.picker_exit.as_mut().zip(picker_opacity).map(
            |(
                PickerExit {
                    outgoing,
                    layer,
                    height,
                    settings,
                    ..
                },
                opacity,
            )| {
                let entity = cx.entity().downgrade();
                window.on_next_frame(move |_, cx| {
                    let _ = entity.update(cx, |_, cx| cx.notify());
                });
                outgoing.update(cx, |inbox, cx| {
                    layer.swap(&mut inbox.session);
                    let height = height.map(|height| motion::snap(height, window.scale_factor()));
                    let settings = settings.as_ref().map(|settings| {
                        super::settings::card(
                            &inbox.theme,
                            &inbox.session,
                            &inbox.settings_view,
                            super::settings::CardMode::AccountSwitch(settings),
                            (
                                height.unwrap_or_else(|| {
                                    super::settings::card_height(
                                        &inbox.session.settings_nav,
                                        f32::from(window.viewport_size().height)
                                            - super::CARD_TOP_PX
                                            - 16.,
                                        f32::from(window.viewport_size().width) < 500.,
                                    )
                                }),
                                window,
                            ),
                            inbox.motion.clock.now(),
                            cx,
                        )
                    });
                    let picker = super::overlay(
                        &inbox.theme,
                        &inbox.session,
                        (opacity, 0.0),
                        height,
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
                        settings,
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
            .text_color(rgb(foreground))
            .text_size(gpui::px(ui_px))
            .font_family_opt(ui_family)
            .child(self.active.clone())
            .children(picker)
    }
}

pub(super) fn switch(id: &str, confirmed_with_enter: bool, window: &mut Window, cx: &mut App) {
    if let Some(root) = window.root::<AccountWindow>().flatten() {
        root.update(cx, |view, cx| {
            view.switch_with_input(id, confirmed_with_enter, window, cx)
        });
    }
}

pub(super) fn remove(id: &str, removal: RequestedRemoval, window: &mut Window, cx: &mut App) {
    if let Some(root) = window.root::<AccountWindow>().flatten()
        && let Ok(registry) = Registry::resolve()
    {
        root.update(cx, |view, cx| {
            view.request_removal(id, &registry, removal, window, cx);
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
            "email = '{id}@example.test'\npassword = 'fixture'\nport = 1\nnotifications = 'off'\n{suffix}"
        )
        .unwrap();
    }

    #[test]
    fn a_catalog_scan_during_a_wal_write_keeps_the_open_account_view() {
        let Some(_temp) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        write_account(&registry, "mail", "");
        let mut app = TestAppContext::single();
        let window = app.add_window(|window, cx| {
            AccountWindow::new(mailbox::startup(), false, None, window, cx)
        });
        let original = window.update(&mut app, |view, _, _| view.active()).unwrap();
        let store = original.read_with(&app, |inbox, _| inbox.store.clone());
        futures::executor::block_on(store.call(|_| ())).unwrap();
        app.run_until_parked();

        let base = mailbox::store_path().unwrap();
        let path = registry.store_path(&base, "mail").unwrap();
        let writer = rusqlite::Connection::open(path).unwrap();
        assert_eq!(
            writer
                .query_row::<String, _, _>("PRAGMA journal_mode", [], |row| row.get(0))
                .unwrap(),
            "wal"
        );
        writer
            .execute_batch(
                "BEGIN IMMEDIATE; UPDATE accounts SET email = 'uncommitted@example.test'",
            )
            .unwrap();
        window
            .update(&mut app, |view, window, cx| {
                original.update(cx, |inbox, _| {
                    inbox.session.folder = "Sent".into();
                    inbox.session.cursor_id = Some("selected-thread".into());
                    inbox.session.search = Some(Default::default());
                    inbox.session.search.as_mut().unwrap().query = "trail".into();
                });
                let accounts = catalog(&registry);
                assert_eq!(
                    accounts.default_config().unwrap().email,
                    "mail@example.test"
                );
                view.reconcile(&registry, accounts, None, window, cx);
                assert_eq!(view.active_id.as_deref(), Some("mail"));
                assert_eq!(view.active(), original);
                let inbox = original.read(cx);
                assert!(!inbox.retired);
                assert_eq!(inbox.session.folder, "Sent");
                assert_eq!(inbox.session.cursor_id.as_deref(), Some("selected-thread"));
                assert_eq!(inbox.session.search.as_ref().unwrap().query, "trail");
            })
            .unwrap();
        writer.execute_batch("ROLLBACK").unwrap();
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
        let sibling_store = window
            .update(&mut app, |view, _, cx| {
                view.views["mail"].read(cx).store.clone()
            })
            .unwrap();
        futures::executor::block_on(sibling_store.call(|_| ())).unwrap();
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
                view.reconcile(&registry, registry.load(), None, window, cx);
                assert_eq!(view.active(), old);
                assert!(view.stopping.is_empty());
                check_position(old.read(cx));
            })
            .unwrap();
        futures::executor::block_on(store.call(|_| ())).unwrap();

        write_account(&registry, "work", "name = 'New work'");
        window
            .update(&mut app, |view, window, cx| {
                view.reconcile(&registry, registry.load(), None, window, cx);
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
    fn account_switch_rebuilds_the_picker_and_guards_queued_enter() {
        let Some(_temp) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        write_account(&registry, "mail", "");
        write_account(&registry, "work", "");
        let leftover = registry.directory().parent().unwrap().join("account.toml");
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(leftover)
            .unwrap()
            .write_all(b"invalid = [")
            .unwrap();
        let mut app = TestAppContext::single();
        let window = app.add_window(|window, cx| {
            AccountWindow::new(mailbox::startup(), false, None, window, cx)
        });
        window
            .update(&mut app, |view, window, cx| {
                let original = view.active();
                assert!(original.read(cx).session.account_config_error.is_none());
                original.update(cx, |inbox, _| {
                    inbox.motion.motion = motion::Motion::new(true);
                    inbox.session.overlay = Overlay::Settings;
                    inbox.session.settings_nav.page = crate::model::settings::Page::Section(
                        crate::model::settings::Section::Accounts,
                    );
                });
                view.switch_with_input("work", true, window, cx);
                let at = Instant::now();
                assert!(view.picker_opacity(at).unwrap() > 0.9);
                for millis in [20, 40, 60, 80, 100] {
                    assert!(
                        view.picker_opacity(at + Duration::from_millis(millis))
                            .is_some()
                    );
                }
                assert!(view.picker_opacity(at + motion::FADE).is_none());
                view.active().update(cx, |inbox, _| {
                    let after_stall = at + Duration::from_millis(300);
                    assert!(inbox.advance_switch_enter_guard(after_stall));
                    for _ in 0..4 {
                        assert!(inbox.swallow_switch_enter(false, after_stall));
                    }
                    for millis in (316..=600).step_by(16) {
                        inbox.advance_switch_enter_guard(at + Duration::from_millis(millis));
                    }
                    let deliberate = at + Duration::from_millis(600);
                    assert!(!inbox.swallow_switch_enter(false, deliberate));
                    assert!(inbox.swallow_switch_enter(true, deliberate));

                    inbox.session.on_key(KeyId::Char('g'), 10);
                    inbox.session.on_key(KeyId::Char('A'), 10);
                    assert_eq!(inbox.session.overlay, Overlay::Folders);
                    let choices = inbox.session.picker_choices();
                    assert_eq!(
                        choices
                            .iter()
                            .filter(|choice| choice.mark == Some("✓"))
                            .map(|choice| choice.name.as_str())
                            .collect::<Vec<_>>(),
                        ["work"]
                    );
                    assert_eq!(choices[inbox.session.folder_index].name, "work");
                });
                view.switch("mail", window, cx);
                original.update(cx, |inbox, _| {
                    assert!(!inbox.swallow_switch_enter(false, Instant::now()));
                });
            })
            .unwrap();
    }

    #[test]
    fn the_switch_enter_guard_expires_with_slow_frames() {
        for interval in [50, 100] {
            let start = Instant::now();
            let mut guard = super::super::AccountSwitchEnterGuard::new(start);
            for millis in (0..=600).step_by(interval) {
                guard.frame(start + Duration::from_millis(millis));
            }
            assert!(
                !guard.armed(start + Duration::from_millis(600)),
                "{interval} ms frames"
            );
        }
    }

    #[test]
    fn the_switch_enter_guard_keeps_queued_input_blocked_after_a_stall() {
        let start = Instant::now();
        let mut guard = super::super::AccountSwitchEnterGuard::new(start);
        assert!(guard.frame(start));
        let late_frame = start + Duration::from_millis(300);
        assert!(guard.frame(late_frame));
        for millis in [300, 310, 320] {
            assert!(guard.armed(start + Duration::from_millis(millis)));
        }
    }

    #[test]
    fn the_switch_enter_guard_expires_during_repeated_presses_without_frames() {
        let start = Instant::now();
        let guard = super::super::AccountSwitchEnterGuard::new(start);
        for millis in [300, 601, 950] {
            assert!(guard.armed(start + Duration::from_millis(millis)));
        }
        for millis in [1000, 1050, 1200] {
            assert!(!guard.armed(start + Duration::from_millis(millis)));
        }
    }

    #[test]
    fn a_failed_removal_reopens_the_outgoing_mailbox_view() {
        let Some(_temp) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        write_account(&registry, "mail", "name = 'Harbor Desk'");
        write_account(&registry, "work", "");
        let mut app = TestAppContext::single();
        let window = app.add_window(|window, cx| {
            AccountWindow::new(mailbox::startup(), false, None, window, cx)
        });
        let original = window.update(&mut app, |view, _, _| view.active()).unwrap();
        let store = original.read_with(&app, |inbox, _| inbox.store.clone());
        futures::executor::block_on(store.call(|_| ())).unwrap();
        app.run_until_parked();
        let base = mailbox::store_path().unwrap();
        let mailbox = registry.mailbox_path(&base, "mail").unwrap();
        let other_session = fell_core::Store::open_existing(&mailbox).unwrap();
        let path = registry.path("mail").unwrap();
        let expected = catalog(&registry)
            .entries
            .into_iter()
            .find(|entry| entry.path == path)
            .unwrap()
            .file_identity;
        window
            .update(&mut app, |view, window, cx| {
                original.update(cx, |inbox, _| {
                    inbox.session.folder = "Sent".into();
                    inbox.session.cursor_id = Some("selected-thread".into());
                    inbox.session.search = Some(Default::default());
                    inbox.session.search.as_mut().unwrap().query = "trail".into();
                });
                view.request_removal(
                    "mail",
                    &registry,
                    RequestedRemoval {
                        kind: AccountRemovalKind::Account,
                        path: path.clone(),
                        expected,
                        delete: true,
                        name: "Harbor Desk".into(),
                    },
                    window,
                    cx,
                );
            })
            .unwrap();
        app.run_until_parked();
        window
            .update(&mut app, |view, _, cx| {
                assert_eq!(view.active_id.as_deref(), Some("mail"));
                assert_ne!(view.active(), original);
                let inbox = view.active.read(cx);
                assert_eq!(inbox.session.folder, "Sent");
                assert_eq!(inbox.session.cursor_id.as_deref(), Some("selected-thread"));
                assert_eq!(inbox.session.search.as_ref().unwrap().query, "trail");
                let toast = inbox.session.toast.as_ref().unwrap();
                assert!(toast.text.starts_with("Couldn't remove Harbor Desk"));
                assert!(toast.text.contains("fell-mcp"));
            })
            .unwrap();
        assert!(path.is_file());
        assert!(mailbox.is_file());
        drop(other_session);
    }

    #[test]
    fn deleting_the_active_account_moves_the_inbox_and_cursor_past_damaged_rows() {
        use crate::model::settings::{Page, Row, Section};

        let Some(_root) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let registry = Registry::resolve().unwrap();
        std::fs::create_dir_all(registry.directory()).unwrap();
        for id in ["mail", "studio", "work"] {
            write_account(&registry, id, "");
        }
        let damaged = registry.path("unreadable").unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&damaged)
            .unwrap()
            .write_all(b"invalid = [")
            .unwrap();
        let mut app = TestAppContext::single();
        let window = app.add_window(|window, cx| {
            AccountWindow::new(mailbox::startup(), false, None, window, cx)
        });
        // This interrupted row joins the catalog while removal is in progress.
        let base = mailbox::store_path().unwrap();
        std::fs::create_dir_all(base.with_extension("accounts").join("000.db.removing")).unwrap();
        for (id, index, next, next_index) in [
            ("studio", 1, Some("work"), 3),
            ("work", 3, Some("mail"), 1),
            ("mail", 1, None, 1),
        ] {
            let path = registry.path(id).unwrap();
            let expected = catalog(&registry)
                .entries
                .into_iter()
                .find(|entry| entry.path == path)
                .unwrap()
                .file_identity;
            window
                .update(&mut app, |view, window, cx| {
                    view.switch(id, window, cx);
                    assert_eq!(view.active_id.as_deref(), Some(id));
                    view.active.update(cx, |inbox, _| {
                        inbox.session.overlay = Overlay::Settings;
                        inbox.session.settings_nav.page = Page::Section(Section::Accounts);
                        inbox.session.settings_nav.select_row(Row::Account(index));
                    });
                    view.request_removal(
                        id,
                        &registry,
                        RequestedRemoval {
                            kind: AccountRemovalKind::Account,
                            path: path.clone(),
                            expected,
                            delete: true,
                            name: id.into(),
                        },
                        window,
                        cx,
                    );
                })
                .unwrap();
            app.run_until_parked();
            window
                .update(&mut app, |view, _, cx| {
                    let inbox = view.active.read(cx);
                    assert_eq!(view.active_id.as_deref(), next, "{:?}", inbox.session.toast);
                    let selected =
                        next.map_or_else(|| damaged.clone(), |id| registry.path(id).unwrap());
                    let selection = inbox
                        .settings_view
                        .account_selection(&inbox.session.settings_nav);
                    assert_eq!(selection, Some((next_index, selected)));
                })
                .unwrap();
            assert!(!path.exists());
        }
        assert!(damaged.is_file());
    }

    #[test]
    fn a_confirmed_removal_waiting_on_reload_runs_before_reopening() {
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
        let sibling_store = window
            .update(&mut app, |view, _, cx| {
                view.views["mail"].read(cx).store.clone()
            })
            .unwrap();
        futures::executor::block_on(sibling_store.call(|_| ())).unwrap();
        let (entered, wait) = std::sync::mpsc::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let job = window
            .update(&mut app, |view, _, cx| {
                view.views["work"].read(cx).store.call(move |_| {
                    entered.send(()).unwrap();
                    blocked.recv().unwrap();
                })
            })
            .unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        write_account(&registry, "work", "name = 'New work'");
        let catalog = registry.load();
        let path = registry.path("work").unwrap();
        let expected = catalog
            .entries
            .iter()
            .find(|entry| entry.path == path)
            .unwrap()
            .file_identity;
        window
            .update(&mut app, |view, window, cx| {
                view.reconcile(&registry, catalog, None, window, cx);
                assert!(view.stopping.contains("work"));
                view.request_removal(
                    "work",
                    &registry,
                    RequestedRemoval {
                        kind: AccountRemovalKind::Account,
                        path: path.clone(),
                        expected,
                        delete: false,
                        name: "New work".into(),
                    },
                    window,
                    cx,
                );
                assert!(view.removing.contains_key("work"));
                assert!(!view.views.contains_key("work"));
                assert!(path.is_file());
            })
            .unwrap();
        release.send(()).unwrap();
        futures::executor::block_on(job).unwrap();
        app.run_until_parked();
        window
            .update(&mut app, |view, _, cx| {
                assert!(!view.stopping.contains("work"));
                assert!(!view.removing.contains_key("work"));
                assert!(
                    !view.views.contains_key("work"),
                    "{:?}",
                    view.active.read(cx).session.toast
                );
            })
            .unwrap();
        assert!(!path.exists());
        let base = mailbox::store_path().unwrap();
        assert!(registry.store_path(&base, "work").unwrap().is_file());
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
                view.reconcile(&registry, registry.load(), None, window, cx);
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
