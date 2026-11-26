//! Keep each mailbox and its view open so switching only changes the shown view.

use std::collections::HashMap;
use std::time::Duration;

use fell_core::accounts::{Accounts, Registry};
use gpui::{App, Context, Entity, IntoElement, Render, Window, div, prelude::*};

use super::{Inbox, Onboarding, mailbox};
use crate::model::PickerChoice;
use crate::motion::{self, Transition};

pub(super) struct AccountWindow {
    active: Entity<Inbox>,
    active_id: Option<String>,
    views: HashMap<String, Entity<Inbox>>,
    catalog: Accounts,
    entering: Option<Transition>,
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
        let active = cx.new(|cx| {
            let mut inbox = Inbox::focused(startup, demo, window, cx);
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
            catalog: Accounts::default(),
            entering: None,
        };
        if let Ok(registry) = Registry::resolve() {
            view.reconcile(&registry, registry.load(), window, cx);
            view.watch(registry, window, cx);
        }
        view
    }

    pub fn active(&self) -> Entity<Inbox> {
        self.active.clone()
    }

    fn watch(&self, registry: Registry, window: &Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                let read = registry.clone();
                let accounts = cx
                    .background_executor()
                    .spawn(async move { read.load() })
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

    fn reconcile(
        &mut self,
        registry: &Registry,
        accounts: Accounts,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.catalog == accounts {
            return;
        }

        let configs: Vec<_> = accounts
            .entries
            .iter()
            .filter_map(|entry| entry.config.as_ref().ok())
            .collect();
        for config in &configs {
            if !self.views.contains_key(&config.id) {
                let path = mailbox::store_path().map(|base| registry.store_path(&base, &config.id));
                let startup = mailbox::startup_for(Some((*config).clone()), false, None, path);
                let entity = cx.new(|cx| {
                    let mut inbox = Inbox::with_startup(startup, false, Some(window), cx);
                    inbox.window_active = false;
                    if let Some(queue) = &inbox.notifications {
                        queue.set_viewing(false);
                    }
                    inbox
                });
                self.views.insert(config.id.clone(), entity);
            }
        }
        self.views.retain(|id, _| {
            if configs.iter().any(|config| &config.id == id) {
                true
            } else {
                let account = fell_core::AccountId(id.clone());
                cx.default_global::<crate::compose_ui::RecipientStore>()
                    .0
                    .remove(&account);
                cx.default_global::<crate::reply_check::ReplyChecks>()
                    .0
                    .remove(&account);
                if let Some(hub) = cx.try_global::<crate::draft_host::AgentDrafts>() {
                    hub.0.remove_server_draft_wake(&account);
                }
                false
            }
        });

        if self
            .active_id
            .as_ref()
            .is_none_or(|id| !self.views.contains_key(id))
        {
            if let Some(config) = accounts.default_config() {
                self.switch(&config.id, window, cx);
            } else if self.active_id.take().is_some() {
                let startup =
                    mailbox::startup_for(None, false, None, Err("No account configured".into()));
                self.active = cx.new(|cx| Inbox::with_startup(startup, false, Some(window), cx));
                self.focus(window, cx);
            }
        }

        self.catalog = accounts;
        self.update_choices(cx);
        cx.notify();
    }

    fn focus(&self, window: &mut Window, cx: &mut App) {
        let active = self.active.read(cx);
        window.focus(&active.focus);
    }

    fn update_choices(&self, cx: &mut App) {
        for (id, view) in &self.views {
            self.set_choices(view, Some(id), cx);
        }
        if self.active_id.is_none() {
            self.set_choices(&self.active, None, cx);
        }
    }

    fn set_choices(&self, view: &Entity<Inbox>, current: Option<&str>, cx: &mut App) {
        view.update(cx, |inbox, cx| {
            let catalog = &self.catalog;
            inbox.session.account_choices = catalog
                .entries
                .iter()
                .filter_map(|entry| {
                    let config = entry.config.as_ref().ok()?;
                    let account = config.account();
                    Some(PickerChoice {
                        name: config.id.clone(),
                        title: account.display_name,
                        detail: Some(account.email),
                        date: None,
                        mark: (current == Some(config.id.as_str())).then_some("✓"),
                        divider: false,
                    })
                })
                .collect();
            inbox.settings_view.set_accounts(catalog, current);
            inbox.refresh_settings_availability();
            cx.notify();
        });
    }

    pub fn switch(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.active_id.as_deref() == Some(id) {
            return;
        }
        if let Some(next) = self.views.get(id).cloned() {
            let frames = self.active.read(cx).frames.clone();
            self.active.update(cx, |inbox, _| {
                inbox.session.cancel_read_delay();
                inbox.window_active = false;
                if let Some(queue) = &inbox.notifications {
                    queue.set_viewing(false);
                }
            });
            self.active = next;
            self.active_id = Some(id.to_owned());
            self.active.update(cx, |inbox, cx| {
                inbox.frames = frames;
                inbox.window_active = window.is_window_active();
                self.entering = inbox
                    .motion
                    .motion
                    .start(motion::QUICK, std::time::Instant::now());
                cx.notify();
            });
            self.focus(window, cx);
            cx.notify();
        }
    }
}

impl Render for AccountWindow {
    fn render(&mut self, window: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let now = std::time::Instant::now();
        self.entering = self.entering.filter(|transition| transition.running(now));
        let opacity = self.entering.map_or(1., |transition| {
            window.request_animation_frame();
            motion::lerp(0.4, 1., transition.progress(now))
        });
        div()
            .size_full()
            .opacity(opacity)
            .child(self.active.clone())
    }
}

pub(super) fn switch(id: &str, window: &mut Window, cx: &mut App) {
    if let Some(root) = window.root::<AccountWindow>().flatten() {
        root.update(cx, |view, cx| view.switch(id, window, cx));
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
