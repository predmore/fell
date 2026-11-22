use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::time::Duration;

use fell_core::{FlagChange, FolderRole};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{AnyWindowHandle, Context, Window};

use super::{Inbox, ReadIntent, archive_failure, archived_notice};
use crate::keymap::Action;
use crate::model::Overlay;
use crate::notifications::{self, Invoked};
use crate::present::Pane;

impl Inbox {
    pub(super) fn track_inbox_visibility(&self, window: &Window) {
        let viewing = window.is_window_active()
            && self.session.search.is_none()
            && self.session.roles.is(FolderRole::Inbox, &self.list_folder)
            && self.session.folder == self.list_folder;
        self.viewing_inbox.store(viewing, Ordering::Relaxed);
    }

    pub(super) fn watch_notifications(
        window: AnyWindowHandle,
        mut invoked: UnboundedReceiver<Invoked>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(invoked) = invoked.next().await {
                let result = window.update(cx, |_, window, cx| {
                    this.update(cx, |inbox, cx| {
                        inbox.notification_action(invoked, window, cx)
                    })
                });
                if !matches!(result, Ok(Ok(()))) {
                    break;
                }
            }
        })
        .detach();
    }

    fn notification_action(
        &mut self,
        invoked: Invoked,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let account = self.account_id.clone();
        let folder = self.session.roles.name(FolderRole::Inbox).to_owned();
        if matches!(invoked.action, notifications::Action::Open) {
            self.activate_from_notification(window, invoked.token, cx);
            window.focus(&self.focus);
            let reply = self.store.call(move |store| {
                for id in invoked.messages {
                    if let Some(thread) = store.thread_for_message(&account, &id)?
                        && let Some(key) = store.folder_key_of(&account, &folder, &thread)?
                    {
                        return Ok::<_, fell_core::Error>(Some(key));
                    }
                }
                Ok(None)
            });
            cx.spawn(async move |this, cx| {
                let result = reply.await;
                let _ = this.update(cx, |inbox, cx| {
                    if let Ok(Ok(Some(key))) = result {
                        let before = inbox.before_step();
                        inbox.session.overlay = Overlay::None;
                        let step = inbox.session.perform(Action::Inbox, inbox.page_rows());
                        inbox.apply_step(step, before, cx);
                        inbox.request_threads(
                            ReadIntent {
                                select: Some(key),
                                open_selected: true,
                                ..Default::default()
                            },
                            cx,
                        );
                    } else {
                        inbox
                            .session
                            .show_toast("Conversation is no longer in the Inbox", false);
                        cx.notify();
                    }
                });
            })
            .detach();
            return;
        }

        let archive = matches!(invoked.action, notifications::Action::Archive);
        self.change_mailbox(
            move |store| {
                let mut seen = HashSet::new();
                let mut threads = Vec::new();
                for id in invoked.messages {
                    if let Some(thread) = store.thread_for_message(&account, &id)?
                        && seen.insert(thread.clone())
                    {
                        threads.push((thread, folder.clone()));
                    }
                }
                if archive {
                    let (_, changed) = store.archive_scoped_threads(&account, &threads)?;
                    Ok(Some(archived_notice(threads.len(), changed)))
                } else {
                    Ok(store
                        .flag_scoped_threads(&account, &threads, FlagChange::Read)?
                        .map(|_| "Marked read".to_owned()))
                }
            },
            move |inbox, result| {
                match result {
                    Ok(Some(notice)) => {
                        inbox.session.show_toast(notice, false);
                        inbox.send_to_server();
                    }
                    Ok(None) => {}
                    Err(err) => {
                        let text = if archive {
                            archive_failure(&err)
                        } else {
                            err.to_string()
                        };
                        inbox.session.show_toast(text, true);
                    }
                }
                Some(ReadIntent {
                    changed: true,
                    ..Default::default()
                })
            },
            cx,
        );
    }

    pub(super) fn open_notification_selection(&mut self, cx: &mut Context<Self>) {
        let before = self.before_step();
        self.session.pane = Pane::List;
        self.session.overlay = Overlay::None;
        let step = self.session.perform(Action::Open, self.page_rows());
        self.apply_step(step, before, cx);
    }

    fn activate_from_notification(
        &mut self,
        window: &Window,
        token: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.pending_activation =
            token.and_then(|token| notifications::Activation::connect(window, token));
        if self.pending_activation.is_none() {
            window.activate_window();
            return;
        }
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(10))
                    .await;
                let pending = handle.update(cx, |_, window, cx| {
                    this.update(cx, |inbox, _| {
                        if inbox
                            .pending_activation
                            .as_mut()
                            .is_some_and(|activation| activation.pending(window))
                        {
                            return true;
                        }
                        inbox.pending_activation = None;
                        false
                    })
                });
                if !matches!(pending, Ok(Ok(true))) {
                    break;
                }
            }
        })
        .detach();
    }
}
