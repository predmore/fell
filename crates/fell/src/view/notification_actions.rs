use std::collections::HashSet;
use std::time::Duration;

use fell_core::{AccountId, FlagChange, FolderRole, MessageId, NotificationMode, Store, ThreadId};
use futures::StreamExt;
use futures::channel::mpsc::UnboundedReceiver;
use gpui::{AnyWindowHandle, Context, Window};

use super::{Inbox, ReadIntent, archive_failure};
use crate::keymap::Action;
use crate::model::Overlay;
use crate::notifications::{self, Invoked, Receipt};
use crate::present::Pane;

const GONE: &str = "Conversation is no longer in the Inbox";

enum Edit {
    Gone,
    Unchanged,
    Changed(i64),
}

impl Inbox {
    pub(super) fn track_inbox_visibility(&self, window: &Window) {
        let Some(queue) = &self.notifications else {
            return;
        };
        let viewing = window.is_window_active()
            && self.session.search.is_none()
            && self.session.roles.is(FolderRole::Inbox, &self.list_folder)
            && self.session.folder == self.list_folder;
        queue.set_viewing(viewing);
    }

    pub(super) fn notification_window_activation(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.window_active = window.is_window_active();
        self.track_inbox_visibility(window);
        if self.window_active
            && let Some((text, alert)) = self.notification_toast.take()
        {
            self.session.show_toast(text, alert);
            cx.notify();
        }
    }

    pub(super) fn notification_feedback(
        &mut self,
        receipt: Receipt,
        text: String,
        undo: Option<i64>,
        alert: bool,
    ) {
        if let Some(queue) = &self.notifications {
            queue.feedback(receipt, text.clone(), undo);
        }
        let toast = if undo.is_some() {
            format!("{text} · u")
        } else {
            text
        };
        self.notification_toast = (!self.window_active).then(|| (toast.clone(), alert));
        self.session.show_toast(toast, alert);
    }

    pub(super) fn set_notification_mode(&mut self, mode: NotificationMode, cx: &mut Context<Self>) {
        let Some(queue) = self.notifications.clone() else {
            self.session.show_toast("No account is set up", true);
            return;
        };
        let account = self.account_id.clone();
        let saved = self.store.call(move |_| {
            crate::mailbox::save_notification_mode(&account, mode)?;
            queue.set_mode(mode);
            Ok::<_, String>(())
        });
        cx.spawn(async move |this, cx| {
            let Ok(result) = saved.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                match result {
                    Ok(()) => {
                        inbox.session.notification_mode = Some(mode);
                        inbox
                            .session
                            .show_toast(Action::Notifications(mode).label(), false);
                    }
                    Err(err) => inbox
                        .session
                        .show_toast(format!("Could not save notifications · {err}"), true),
                }
                cx.notify();
            });
        })
        .detach();
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
        if let notifications::Action::Undo(id) = invoked.action {
            self.undo_mailbox(Some((invoked.receipt, id)), cx);
            return;
        }
        let account = self.account_id.clone();
        let folder = self.session.roles.name(FolderRole::Inbox).to_owned();
        let receipt = invoked.receipt;
        if matches!(
            invoked.action,
            notifications::Action::Open | notifications::Action::OpenInbox
        ) {
            let pane = if matches!(invoked.action, notifications::Action::OpenInbox) {
                Pane::List
            } else {
                Pane::Conversation
            };
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
                    let key = match result {
                        Ok(Ok(key)) => key,
                        Ok(Err(err)) => {
                            inbox.notification_feedback(receipt, err.to_string(), None, true);
                            cx.notify();
                            return;
                        }
                        Err(_) => {
                            inbox.notification_feedback(
                                receipt,
                                "Could not open the Inbox".into(),
                                None,
                                true,
                            );
                            cx.notify();
                            return;
                        }
                    };
                    if key.is_some() || pane == Pane::List {
                        let before = inbox.before_step();
                        inbox.session.overlay = Overlay::None;
                        let step = inbox.session.perform(Action::Inbox, inbox.page_rows());
                        if pane == Pane::List {
                            inbox.session.pane = Pane::List;
                            inbox.session.cancel_read_delay();
                            inbox.session.leave_attachments();
                        }
                        inbox.apply_step(step, before, cx);
                    }
                    if let Some(key) = key {
                        if let Some(queue) = &inbox.notifications {
                            queue.dismiss(receipt);
                        }
                        inbox.request_threads(
                            ReadIntent {
                                select: Some(key),
                                notification_pane: Some(pane),
                                ..Default::default()
                            },
                            cx,
                        );
                    } else {
                        inbox.notification_feedback(receipt, GONE.into(), None, false);
                        cx.notify();
                    }
                });
            })
            .detach();
            return;
        }

        let archive = matches!(invoked.action, notifications::Action::Archive);
        self.change_mailbox(
            move |store| edit_notification(store, &account, &folder, invoked.messages, archive),
            move |inbox, result| {
                let (title, undo, alert) = match result {
                    Ok(Edit::Changed(action_id)) => {
                        inbox.send_to_server();
                        let title = if archive { "Archived" } else { "Marked read" };
                        (title.into(), Some(action_id), false)
                    }
                    Ok(Edit::Gone) => (GONE.into(), None, false),
                    Ok(Edit::Unchanged) => ("Already read".into(), None, false),
                    Err(err) => {
                        let text = if archive {
                            archive_failure(&err)
                        } else {
                            err.to_string()
                        };
                        (text, None, true)
                    }
                };
                inbox.notification_feedback(receipt, title, undo, alert);
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

fn edit_notification(
    store: &Store,
    account: &AccountId,
    folder: &str,
    messages: Vec<MessageId>,
    archive: bool,
) -> fell_core::Result<Edit> {
    let mut seen = HashSet::<ThreadId>::new();
    let mut threads = Vec::new();
    for id in messages {
        if let Some(thread) = store.thread_for_message(account, &id)?
            && seen.insert(thread.clone())
            && store.folder_key_of(account, folder, &thread)?.is_some()
        {
            threads.push((thread, folder.to_owned()));
        }
    }
    if threads.is_empty() {
        return Ok(Edit::Gone);
    }
    if archive {
        let (action, _) = store.archive_scoped_threads(account, &threads)?;
        Ok(Edit::Changed(action.id))
    } else {
        Ok(
            match store.flag_scoped_threads(account, &threads, FlagChange::Read)? {
                Some(action) => Edit::Changed(action.id),
                None => Edit::Unchanged,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_notification_thread_does_not_block_marking_the_remaining_thread_read() {
        let store = Store::open_in_memory().unwrap();
        let account = crate::fixtures::fixture_account();
        crate::fixtures::seed_if_empty(&store, &account).unwrap();
        let account = account.id;
        let folder = FolderRole::Inbox.default_name();
        let mut unread = store
            .list_threads(&account)
            .unwrap()
            .into_iter()
            .filter_map(|row| {
                store
                    .thread_messages(&account, &row.thread_id)
                    .unwrap()
                    .into_iter()
                    .find(|msg| msg.unread && msg.folders.iter().any(|name| name == folder))
                    .map(|msg| (row.thread_id, msg.headers.message_id))
            });
        let (stale_thread, stale) = unread.next().unwrap();
        let (current_thread, current) = unread.next().unwrap();
        store
            .move_thread(&account, &stale_thread, folder, "Archive")
            .unwrap();
        let messages = vec![stale.clone(), current.clone(), current.clone()];
        assert_eq!(
            store
                .unread_inbox_messages(&account, &messages[..2])
                .unwrap(),
            std::slice::from_ref(&current)
        );
        assert!(
            store
                .flag_scoped_threads(
                    &account,
                    &[
                        (current_thread, folder.into()),
                        (stale_thread, folder.into())
                    ],
                    FlagChange::Read,
                )
                .is_err()
        );
        let Edit::Changed(action_id) =
            edit_notification(&store, &account, folder, messages, false).unwrap()
        else {
            panic!("expected an undoable read");
        };
        assert!(
            store
                .unread_inbox_messages(&account, std::slice::from_ref(&current))
                .unwrap()
                .is_empty()
        );
        assert!(store.undo_notification(&account, action_id - 1).is_err());
        store.undo_notification(&account, action_id).unwrap();
        assert_eq!(
            store
                .unread_inbox_messages(&account, &[stale.clone(), current.clone()])
                .unwrap(),
            [current]
        );
        assert!(matches!(
            edit_notification(&store, &account, folder, vec![stale], false).unwrap(),
            Edit::Gone
        ));
    }
}
