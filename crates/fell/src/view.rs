//! Two-pane inbox. The window reads the local store and never talks to IMAP.

use std::time::{SystemTime, UNIX_EPOCH};

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use fell_core::{AccountConfig, AccountId, ThreadId};
use futures::StreamExt;
use gpui::{
    App, Application, Bounds, Context, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, Keystroke, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement, Render,
    ScrollHandle, ScrollStrategy, SharedString, StatefulInteractiveElement, Styled,
    TitlebarOptions, UniformListScrollHandle, Window, WindowBounds, WindowOptions, div, point,
    prelude::*, px, rgb, size, uniform_list,
};

use crate::keymap::KeyId;
use crate::mailbox;
use crate::model::{Overlay, QueryFault, Session, Step, SyncKind};
use crate::present::{self, ConvMessage, NameSpan, Pane, ThreadRow};
use crate::store_actor::StoreClient;
use crate::sync_worker::{self, SyncHandle};
use crate::text_render::BodyBlock;
use crate::theme::{self, Theme};

pub fn run() {
    Application::new().run(|cx: &mut App| {
        let (width, height) = window_size();
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let view = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Fell".into()),
                    ..Default::default()
                }),
                app_id: Some("dev.predmore.fell".into()),
                focus: true,
                show: true,
                ..Default::default()
            },
            |window, cx| {
                let view = cx.new(Inbox::new);
                let focus = view.read(cx).focus.clone();
                window.focus(&focus);
                view
            },
        );
        if view.is_err() {
            eprintln!("fell: could not open a window");
        }
        cx.activate(true);
    });
}

/// Why the folder is being reread. Requests made while a read is in flight
/// merge into one, so a sync refresh can never drop what the user asked for.
#[derive(Clone, Debug, Default)]
struct ReadIntent {
    /// The folder on screen changed. A failure goes back to `list_folder`; a
    /// selected thread missing from the new folder gives way to the first row.
    switch: bool,
    /// Archive, move, or undo wrote to the store. Scroll to the selection
    /// instead of holding the viewport still.
    changed: bool,
    /// Undo: select this thread when it is back in the folder.
    select: Option<String>,
}

impl ReadIntent {
    fn merge(mut self, later: ReadIntent) -> ReadIntent {
        self.switch |= later.switch;
        self.changed |= later.changed;
        self.select = later.select.or(self.select);
        self
    }

    /// Only sync asked: keep the selected row still on screen.
    fn is_refresh(&self) -> bool {
        !self.switch && !self.changed && self.select.is_none()
    }
}

type ThreadsResult = Result<(Vec<ThreadRow>, Vec<String>), String>;

struct Inbox {
    focus: FocusHandle,
    /// The mailbox connection's own thread. The window never calls SQLite.
    store: StoreClient,
    account_id: AccountId,
    session: Session,
    theme: Theme,
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    palette_scroll: ScrollHandle,
    folder_scroll: ScrollHandle,
    split: f32,
    dragging: bool,
    open_thread: Option<String>,
    /// Thread `m` just moved. `u` selects it again when it returns to this folder.
    moved_thread: Option<String>,
    /// Background sync for the account file. `None` for the demo mailbox.
    sync: Option<SyncHandle>,
    /// The folder the rows on screen came from. `session.folder` changes on
    /// the key press; this changes when that folder's read lands. Archive and
    /// move act on this one, the list the reader sees.
    list_folder: String,
    /// One folder read at a time is on the store thread.
    read_in_flight: bool,
    /// The read in flight was only a sync refresh, on the background lane.
    refresh_in_flight: bool,
    /// Bumped by every folder read. A read the user superseded is dropped.
    read_gen: u64,
    /// Asked for while a read was in flight. Issued when it lands.
    read_wanted: Option<ReadIntent>,
    /// Threads taken out of the list whose archive or move has not landed.
    /// A read that started before the write cannot paint them back.
    pending_removals: std::collections::HashSet<String>,
    /// Bumped by every conversation read. The store thread skips a read that
    /// is already stale, so holding `j` does not build a backlog.
    conv_gen: Arc<AtomicU64>,
}

impl Inbox {
    fn new(cx: &mut Context<Self>) -> Self {
        let startup = mailbox::startup();
        let theme = theme::load();
        let config = startup.config.clone();
        let path = startup.path.clone();
        let (store, ready) = StoreClient::spawn({
            let config = config.clone();
            move || mailbox::open_store(path, config.as_ref())
        });
        let (name, email) = match &config {
            Some(config) => {
                let account = config.account();
                (account.display_name, account.email)
            }
            None => (String::new(), String::new()),
        };
        let initial = if config.is_some() {
            SyncKind::Connecting
        } else {
            SyncKind::LocalOffline
        };
        let mut session = Session::new(name, email, initial, startup.keymap);
        session.loading = true;
        if let Some(error) = startup.error {
            session.record_startup_error(error);
        }
        let inbox = Self {
            focus: cx.focus_handle(),
            account_id: config
                .as_ref()
                .map(|config| config.account().id)
                .unwrap_or_else(|| AccountId(String::new())),
            session,
            store,
            theme,
            list_scroll: UniformListScrollHandle::new(),
            conv_scroll: ScrollHandle::new(),
            palette_scroll: ScrollHandle::new(),
            folder_scroll: ScrollHandle::new(),
            split: startup.split,
            dragging: false,
            open_thread: None,
            moved_thread: None,
            sync: None,
            list_folder: "INBOX".into(),
            read_in_flight: false,
            refresh_in_flight: false,
            read_gen: 0,
            read_wanted: None,
            pending_removals: std::collections::HashSet::new(),
            conv_gen: Arc::new(AtomicU64::new(0)),
        };
        let sync_with = config.zip(startup.path.ok());
        cx.spawn(async move |this, cx| {
            let Ok(opened) = ready.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| inbox.on_store_ready(opened, sync_with, cx));
        })
        .detach();
        inbox.watch_theme(cx);
        inbox.watch_dates(cx);
        inbox
    }

    fn on_store_ready(
        &mut self,
        opened: mailbox::Opened,
        sync_with: Option<(AccountConfig, std::path::PathBuf)>,
        cx: &mut Context<Self>,
    ) {
        self.account_id = opened.account.id.clone();
        self.session.account_name = opened.account.display_name.clone();
        self.session.account_email = opened.account.email.clone();
        if let Some(error) = opened.error {
            let joined = match self.session.startup_error.take() {
                Some(earlier) => format!("{earlier}\n{error}"),
                None => error,
            };
            self.session.record_startup_error(joined);
        }
        let store_failed = opened.sync == SyncKind::StoreError;
        self.session.sync = opened.sync;
        // Sync opens its own connection only after this one finished opening
        // and migrating the file, so the two never race for the first lock.
        if !store_failed && let Some((config, path)) = sync_with {
            let (handle, events) = sync_worker::start(config, path);
            self.sync = Some(handle);
            self.watch_sync(events, cx);
        }
        self.request_threads(ReadIntent::default(), cx);
        cx.notify();
    }

    /// Apply sync status as soon as it arrives. The sync thread writes the
    /// store; the window only asks its store thread to reread the folder.
    fn watch_sync(
        &self,
        mut events: futures::channel::mpsc::UnboundedReceiver<sync_worker::SyncEvent>,
        cx: &mut Context<Self>,
    ) {
        cx.spawn(async move |this, cx| {
            while let Some(kind) = events.next().await {
                let alive = this
                    .update(cx, |inbox, cx| {
                        let wrote = matches!(kind, SyncKind::Syncing { .. })
                            || matches!(kind, SyncKind::Synced { stored, .. } if stored > 0);
                        inbox.session.sync = kind;
                        if wrote {
                            inbox.request_threads(ReadIntent::default(), cx);
                        }
                        cx.notify();
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Reread the folder on screen on the store thread. While one read is in
    /// flight, further requests merge into a single follow-up, so a burst of
    /// sync batches is one read and never a queue.
    fn request_threads(&mut self, intent: ReadIntent, cx: &mut Context<Self>) {
        // A sync refresh in flight does not make the reader wait: their read
        // starts now on the interactive lane, and the refresh is dropped
        // when it lands. This read sees everything the refresh would have.
        let supersede = self.read_in_flight && self.refresh_in_flight && !intent.is_refresh();
        if self.read_in_flight && !supersede {
            self.read_wanted = Some(match self.read_wanted.take() {
                Some(earlier) => earlier.merge(intent),
                None => intent,
            });
            return;
        }
        self.read_in_flight = true;
        self.refresh_in_flight = intent.is_refresh();
        self.read_gen += 1;
        let generation = self.read_gen;
        let account = self.account_id.clone();
        let folder = self.session.folder.clone();
        let me = self.session.account_email.clone();
        let reading = folder.clone();
        let read = move |store: &fell_core::Store| -> ThreadsResult {
            let threads = store
                .folder_threads(&account, &reading)
                .map_err(|err| err.to_string())?;
            let names = store.folder_names(&account).unwrap_or_default();
            Ok((present::thread_rows(&threads, &me), names))
        };
        // Only sync asked: nobody is waiting, so keep the interactive lane
        // free for the conversation the reader opens next.
        let reply = if intent.is_refresh() {
            self.store.call_background(read)
        } else {
            self.store.call(read)
        };
        cx.spawn(async move |this, cx| {
            let Ok(result) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if inbox.read_gen == generation {
                    inbox.read_landed(folder, result, intent, cx);
                }
            });
        })
        .detach();
    }

    fn read_landed(
        &mut self,
        folder: String,
        result: ThreadsResult,
        intent: ReadIntent,
        cx: &mut Context<Self>,
    ) {
        self.read_in_flight = false;
        self.refresh_in_flight = false;
        let wanted = self.read_wanted.take();
        // A user action (switch, archive, move, undo) since this read started
        // makes it stale. Skip it and read again with both intents, so the
        // list never flashes a folder or a row the user already left.
        let stale =
            folder != self.session.folder || wanted.as_ref().is_some_and(|next| !next.is_refresh());
        if stale {
            let merged = match wanted {
                Some(next) => intent.merge(next),
                None => intent,
            };
            self.request_threads(merged, cx);
            return;
        }
        self.show_threads(folder, result, intent, cx);
        // Only sync wrote while that read ran. Show it first, then read once
        // more, so a fast sync cannot starve the list.
        if let Some(next) = wanted {
            self.request_threads(next, cx);
        }
    }

    fn show_threads(
        &mut self,
        folder: String,
        result: ThreadsResult,
        intent: ReadIntent,
        cx: &mut Context<Self>,
    ) {
        self.session.loading = false;
        let (mut rows, names) = match result {
            Ok(found) => found,
            Err(err) => {
                if intent.switch {
                    self.session.folder = self.list_folder.clone();
                }
                self.session.fail_query(QueryFault::Folder, err);
                cx.notify();
                return;
            }
        };
        self.session.clear_query_fault(QueryFault::Folder);
        if !names.is_empty() {
            self.session.folders = names;
        }
        let switched = folder != self.list_folder;
        if !switched {
            rows.retain(|row| !self.pending_removals.contains(&row.id));
        }
        self.list_folder = folder;
        if switched
            && let Some(id) = &self.session.cursor_id
            && !rows.iter().any(|row| &row.id == id)
        {
            // A new folder opens at its top, not at the old folder's index.
            self.session.cursor_id = None;
        }
        let row_height = self.list_row_height();
        let before_id = self.session.cursor_id.clone();
        let before_index = before_id
            .as_ref()
            .and_then(|id| self.session.threads.iter().position(|row| &row.id == id));
        let open_count = self.open_thread.as_ref().and_then(|id| {
            rows.iter()
                .find(|row| &row.id == id)
                .map(|row| row.message_count)
        });
        self.session.set_threads(rows);
        if let Some(id) = &intent.select {
            self.session.select_if_present(id);
        }
        let after_index = self
            .session
            .cursor_id
            .as_ref()
            .and_then(|id| self.session.threads.iter().position(|row| &row.id == id));
        let same_thread = self.session.cursor_id == before_id;
        if intent.is_refresh() && !switched && same_thread {
            // Rows sync put above the selection push it down. Scroll by the
            // same amount so the selected row stays where the reader left it.
            if let (Some(before), Some(after), Some(row)) = (before_index, after_index, row_height)
            {
                self.shift_list(after as i64 - before as i64, row);
            }
        } else {
            self.scroll_list_to_cursor();
        }
        let open_changed = self.open_thread != self.session.cursor_id;
        let grew = open_count.is_some_and(|count| count != self.session.messages.len());
        if open_changed || grew {
            self.request_messages(true, cx);
        }
        cx.notify();
    }

    /// One row's height as the list last laid it out. GPUI records the
    /// viewport as `item` and the whole list as `contents`.
    fn list_row_height(&self) -> Option<f32> {
        let state = self.list_scroll.0.borrow();
        let size = state.last_item_size?;
        let rows = self.session.threads.len();
        if rows == 0 {
            return None;
        }
        let row = f32::from(size.contents.height) / rows as f32;
        (row > 0.0).then_some(row)
    }

    /// Move the list viewport by `rows` rows of `row` pixels without moving
    /// the selection. GPUI offsets grow more negative further down the list.
    fn shift_list(&self, rows: i64, row: f32) {
        if rows == 0 {
            return;
        }
        let state = self.list_scroll.0.borrow();
        let handle = &state.base_handle;
        let offset = handle.offset();
        let max = f32::from(handle.max_offset().height) + rows.max(0) as f32 * row;
        let next = (f32::from(offset.y) - rows as f32 * row).clamp(-max, 0.0);
        handle.set_offset(point(offset.x, px(next)));
    }

    /// Read the selected thread on the store thread. A read that is stale
    /// by the time the store thread reaches it is skipped there.
    fn request_messages(&mut self, force: bool, cx: &mut Context<Self>) {
        let Some(id) = self.session.cursor_id.clone() else {
            self.conv_gen.fetch_add(1, Ordering::SeqCst);
            self.clear_open_message();
            return;
        };
        if !force && self.open_thread.as_deref() == Some(id.as_str()) {
            return;
        }
        let generation = self.conv_gen.fetch_add(1, Ordering::SeqCst) + 1;
        let current = self.conv_gen.clone();
        let account = self.account_id.clone();
        let me = self.session.account_email.clone();
        let thread = id.clone();
        let reply = self.store.call(move |store| {
            if current.load(Ordering::SeqCst) != generation {
                return None;
            }
            Some(
                store
                    .thread_messages(&account, &ThreadId(thread))
                    .map(|messages| present::conversation(&messages, &me))
                    .map_err(|err| err.to_string()),
            )
        });
        cx.spawn(async move |this, cx| {
            let Ok(Some(result)) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if inbox.conv_gen.load(Ordering::SeqCst) != generation
                    || inbox.session.cursor_id.as_deref() != Some(id.as_str())
                {
                    return;
                }
                match result {
                    Ok(view) => {
                        inbox.session.clear_query_fault(QueryFault::Message);
                        let same = inbox.open_thread.as_deref() == Some(id.as_str());
                        // Sync adding to the thread being read must not pull a
                        // reader who scrolled up back to the bottom.
                        let follow = if same {
                            let bottom = inbox.conversation_at_bottom();
                            inbox.session.refresh_messages(view, bottom)
                        } else {
                            inbox.session.set_messages(view);
                            true
                        };
                        inbox.open_thread = Some(id);
                        if follow {
                            inbox.scroll_conversation();
                        }
                    }
                    Err(err) => {
                        // Never leave another thread's body under this row.
                        if inbox.open_thread.as_deref() != Some(id.as_str()) {
                            inbox.clear_open_message();
                        }
                        inbox.session.fail_query(QueryFault::Message, err);
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Run a mailbox change on the store thread, then reread the folder.
    fn change_mailbox(
        &mut self,
        change: impl FnOnce(&fell_core::Store) -> Result<(), fell_core::Error> + Send + 'static,
        done: impl FnOnce(&mut Self, Result<(), fell_core::Error>) -> Option<ReadIntent> + 'static,
        cx: &mut Context<Self>,
    ) {
        let reply = self.store.call(change);
        cx.spawn(async move |this, cx| {
            let Ok(result) = reply.await else {
                return;
            };
            let _ = this.update(cx, |inbox, cx| {
                if let Some(after) = done(inbox, result) {
                    inbox.request_threads(after, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reload the theme when Omarchy, GTK, or Alacritty settings change.
    /// inotify wakes this; without it, a slow poll does.
    fn watch_theme(&self, cx: &mut Context<Self>) {
        let (changed, mut changes) = futures::channel::mpsc::unbounded();
        let watching = theme::watch(changed);
        cx.spawn(async move |this, cx| {
            loop {
                if watching {
                    if changes.next().await.is_none() {
                        break;
                    }
                } else {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(1))
                        .await;
                }
                let next = cx
                    .background_executor()
                    .spawn(async { theme::load() })
                    .await;
                let alive = this
                    .update(cx, |inbox, cx| {
                        if inbox.theme != next {
                            inbox.theme = next;
                            cx.notify();
                        }
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    fn watch_dates(&self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_secs(15))
                    .await;
                let alive = this.update(cx, |_, cx| cx.notify()).is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();
    }

    /// Move messages in the folder on screen to Archive. A Sent copy stays in
    /// Sent. The row leaves the list at once and the row in its place is
    /// selected, so a second `e` archives that one.
    fn archive_current(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.session.cursor_id.clone() else {
            self.session.show_toast("No conversation", true);
            return;
        };
        if !self.session.threads.iter().any(|thread| thread.id == id) {
            self.session.show_toast("No conversation", true);
            return;
        }
        let folder = self.list_folder.clone();
        let account = self.account_id.clone();
        let thread = ThreadId(id.clone());
        self.take_out_of_list(&id, cx);
        self.change_mailbox(
            move |store| store.archive_thread(&account, &thread, &folder).map(|_| ()),
            move |inbox, result| match result {
                Ok(()) => {
                    inbox.pending_removals.remove(&id);
                    inbox.moved_thread = Some(id);
                    inbox.session.show_toast("Archived · u", false);
                    Some(ReadIntent {
                        changed: true,
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    inbox.pending_removals.remove(&id);
                    inbox.session.show_toast(archive_failure(&err), true);
                    Some(ReadIntent {
                        changed: true,
                        select: Some(id),
                        ..ReadIntent::default()
                    })
                }
            },
            cx,
        );
    }

    fn apply_move(&mut self, destination: String, cx: &mut Context<Self>) {
        let Some(id) = self.session.cursor_id.clone() else {
            self.session.show_toast("No conversation", true);
            return;
        };
        if !self.session.threads.iter().any(|thread| thread.id == id) {
            self.session.show_toast("No conversation", true);
            return;
        }
        let account = self.account_id.clone();
        let thread = ThreadId(id.clone());
        let from = self.list_folder.clone();
        let to = destination.clone();
        self.take_out_of_list(&id, cx);
        self.change_mailbox(
            move |store| store.move_thread(&account, &thread, &from, &to).map(|_| ()),
            move |inbox, result| match result {
                Ok(()) => {
                    inbox.pending_removals.remove(&id);
                    inbox.moved_thread = Some(id);
                    let label = present::folder_label(&destination);
                    inbox.session.show_toast(format!("Moved to {label}"), false);
                    Some(ReadIntent {
                        changed: true,
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    inbox.pending_removals.remove(&id);
                    inbox.session.show_toast(err.to_string(), true);
                    Some(ReadIntent {
                        changed: true,
                        select: Some(id),
                        ..ReadIntent::default()
                    })
                }
            },
            cx,
        );
    }

    /// Remove `id` from the rows on screen before its write lands and open
    /// the row that took its place.
    fn take_out_of_list(&mut self, id: &str, cx: &mut Context<Self>) {
        self.pending_removals.insert(id.to_string());
        self.session.remove_thread(id);
        self.scroll_list_to_cursor();
        self.request_messages(true, cx);
    }

    fn apply_undo(&mut self, cx: &mut Context<Self>) {
        let account = self.account_id.clone();
        self.change_mailbox(
            move |store| store.undo(&account).map(|_| ()),
            |inbox, result| match result {
                Ok(()) => {
                    inbox.session.show_toast("Undone", false);
                    Some(ReadIntent {
                        changed: true,
                        select: inbox.moved_thread.take(),
                        ..ReadIntent::default()
                    })
                }
                Err(err) => {
                    let raw = err.to_string();
                    if raw == "nothing to undo" {
                        inbox.session.show_toast("Nothing to undo", false);
                    } else {
                        inbox.session.show_toast(raw, true);
                    }
                    None
                }
            },
            cx,
        );
    }

    fn clear_open_message(&mut self) {
        self.session.messages.clear();
        self.open_thread = None;
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = key_from(&event.keystroke) else {
            return;
        };
        window.focus(&self.focus);
        // The wheel moves the viewport without moving message_cursor. j, k,
        // and q aim at the message now on screen in either pane. Enter and o
        // do that only while the conversation is focused, so they expand the
        // message on screen. An open palette, folder filter, or help card
        // types those keys and must leave the cursor alone.
        if should_follow_visible(
            &self.session.overlay,
            self.session.chord.is_empty(),
            self.session.pane,
            key,
        ) {
            self.follow_visible_message();
        }
        let page = self.page_rows();
        let message_before = self.session.message_cursor;
        let thread_before = self.session.cursor_id.clone();
        let folder_before = self.session.folder.clone();
        self.session.clear_toast();
        let step = self.session.on_key(key, page);
        self.apply_step(step, message_before, thread_before, folder_before, cx);
        cx.stop_propagation();
    }

    fn apply_step(
        &mut self,
        step: Step,
        message_before: usize,
        thread_before: Option<String>,
        folder_before: String,
        cx: &mut Context<Self>,
    ) {
        if step.apply_move
            && let Some(destination) = self.session.pending_move.take()
        {
            self.apply_move(destination, cx);
        }
        if step.undo {
            self.apply_undo(cx);
        }
        if step.archive {
            self.archive_current(cx);
        }
        if step.sync_now {
            match &self.sync {
                Some(handle) => {
                    handle.sync_now();
                    self.session.show_toast("Syncing…", false);
                }
                None => self
                    .session
                    .show_toast("No account is set up · run fell account setup", true),
            }
        }
        // Reads go to the store thread. The list moves on this frame; the
        // conversation follows when its read lands.
        if step.reload_threads && self.session.folder != folder_before {
            self.request_threads(
                ReadIntent {
                    switch: true,
                    ..ReadIntent::default()
                },
                cx,
            );
        } else if step.reload_threads {
            self.request_threads(ReadIntent::default(), cx);
        } else if step.reload_messages {
            self.request_messages(true, cx);
        }
        if step.arm_which {
            self.arm_which(cx);
        }
        let thread_moved = self.session.cursor_id != thread_before;
        if thread_moved {
            self.scroll_list_to_cursor();
        }
        // A tall message aligns to its top when scrolled into view. Skip that
        // unless the reader moved to another message, or a new thread loaded.
        // Paging already moved the viewport by pixels, so it must not jump
        // the current message back to the top.
        if step.scroll_pages != 0 || step.scroll_lines != 0 {
            self.nudge_conversation(step.scroll_pages, step.scroll_lines);
        } else if self.session.message_cursor != message_before {
            self.scroll_conversation();
        }
        if self.session.overlay == Overlay::Palette {
            self.palette_scroll
                .scroll_to_item(self.session.palette_index);
        }
        if self.session.overlay == Overlay::Folders {
            self.folder_scroll.scroll_to_item(self.session.folder_index);
        }
        cx.notify();
    }

    fn follow_visible_message(&mut self) {
        if self.session.messages.is_empty() || self.conv_scroll.bounds_for_item(0).is_none() {
            return;
        }
        let index = self.conv_scroll.top_item();
        if index < self.session.messages.len() {
            self.session.message_cursor = index;
        }
    }

    /// The conversation viewport is scrolled to its end (or does not scroll).
    fn conversation_at_bottom(&self) -> bool {
        let max = f32::from(self.conv_scroll.max_offset().height);
        let offset = f32::from(self.conv_scroll.offset().y);
        max <= 1.0 || offset <= -max + 24.0
    }

    fn scroll_conversation(&mut self) {
        if !self.session.messages.is_empty() {
            self.conv_scroll.scroll_to_item(self.session.message_cursor);
        }
    }

    fn nudge_conversation(&mut self, pages: i32, lines: i32) {
        let viewport = f32::from(self.conv_scroll.bounds().size.height);
        let viewport = if viewport < 1.0 { 640.0 } else { viewport };
        let delta = pages as f32 * viewport + lines as f32 * 24.0;
        let measured = self.conv_scroll.offset();
        let scrollport_top = f32::from(self.conv_scroll.bounds().top());
        let mut item_bounds = Vec::new();
        let mut index = 0;
        while let Some(bounds) = self.conv_scroll.bounds_for_item(index) {
            item_bounds.push((f32::from(bounds.top()), f32::from(bounds.bottom())));
            index += 1;
        }
        let max = f32::from(self.conv_scroll.max_offset().height);
        let measured_y = f32::from(measured.y);
        let next_y = (measured_y - delta).clamp(-max, 0.0);
        let top = present::message_after_page(&item_bounds, scrollport_top, measured_y, next_y)
            .filter(|index| *index < self.session.messages.len());
        self.session.message_cursor = present::cursor_after_nudge(
            self.session.message_cursor,
            top,
            next_y < measured_y,
            next_y != measured_y,
        );
        self.conv_scroll.set_offset(point(measured.x, px(next_y)));
    }

    fn arm_which(&mut self, cx: &mut Context<Self>) {
        let generation = self.session.chord_gen;
        let delay = self.session.keymap.which_key_ms;
        if self.session.chord.is_empty() {
            self.session.which_visible = false;
            return;
        }
        if delay == 0 {
            self.session.which_visible = true;
            return;
        }
        self.session.which_visible = false;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(delay))
                .await;
            let _ = this.update(cx, |inbox, cx| {
                if inbox.session.chord_gen == generation && !inbox.session.chord.is_empty() {
                    inbox.session.which_visible = true;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn page_rows(&self) -> usize {
        let height = match self.session.pane {
            Pane::List => f32::from(self.list_scroll.0.borrow().base_handle.bounds().size.height),
            Pane::Conversation => f32::from(self.conv_scroll.bounds().size.height),
        };
        let height = if height < 1.0 { 640.0 } else { height };
        let row = match self.session.pane {
            Pane::List => 68.0,
            Pane::Conversation => 160.0,
        };
        ((height / row).floor() as usize).max(1)
    }

    fn scroll_list_to_cursor(&self) {
        self.list_scroll
            .scroll_to_item(self.session.cursor_index(), ScrollStrategy::Top);
    }

    fn select_thread(&mut self, id: String, cx: &mut Context<Self>) {
        self.session.cancel_pending_keys();
        let changed = self.session.cursor_id.as_deref() != Some(id.as_str());
        self.session.cursor_id = Some(id);
        self.session.pane = Pane::List;
        if changed {
            self.request_messages(true, cx);
        }
        self.scroll_list_to_cursor();
    }
}

impl Render for Inbox {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let width = f32::from(window.viewport_size().width);
        let theme = self.theme.clone();
        let session = &self.session;
        let list_scroll = self.list_scroll.clone();
        let conv_scroll = self.conv_scroll.clone();
        let palette_scroll = self.palette_scroll.clone();
        let folder_scroll = self.folder_scroll.clone();
        let focus = self.focus.clone();
        let split = self.split;
        shell(
            theme,
            session,
            width,
            split,
            list_scroll,
            conv_scroll,
            palette_scroll,
            folder_scroll,
            focus,
            window,
            cx,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn shell(
    theme: Theme,
    session: &Session,
    width: f32,
    split: f32,
    list_scroll: UniformListScrollHandle,
    conv_scroll: ScrollHandle,
    palette_scroll: ScrollHandle,
    folder_scroll: ScrollHandle,
    focus: FocusHandle,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> impl IntoElement {
    let (show_list, show_conv) = present::visible_panes(width, session.pane, session.full_width);
    let list_width = (width * split).clamp(280.0, (width - 400.0).max(280.0));
    div()
        .id("fell-root")
        .track_focus(&focus)
        .on_key_down(cx.listener(|inbox, event: &KeyDownEvent, window, cx| {
            inbox.on_key_down(event, window, cx);
        }))
        .on_mouse_move(cx.listener(|inbox, event: &MouseMoveEvent, window, cx| {
            if !inbox.dragging {
                return;
            }
            let width = f32::from(window.viewport_size().width).max(1.0);
            let x = f32::from(event.position.x);
            inbox.split = (x / width).clamp(0.22, 0.52);
            cx.notify();
        }))
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|inbox, _: &MouseUpEvent, _, cx| {
                if inbox.dragging {
                    inbox.dragging = false;
                    let split = inbox.split;
                    cx.background_executor()
                        .spawn(async move { mailbox::save_split(split) })
                        .detach();
                }
            }),
        )
        .size_full()
        .flex()
        .flex_col()
        .bg(rgb(theme.background))
        .text_color(rgb(theme.foreground))
        .text_size(px(theme.ui_px))
        .font_family_opt(theme.ui_family.clone())
        .overflow_hidden()
        .child(header(&theme, session))
        .child(
            div()
                .flex_1()
                .min_h(px(0.))
                .flex()
                .flex_row()
                .overflow_hidden()
                .when(show_list, |row| {
                    row.child(thread_list(
                        &theme,
                        session,
                        show_conv,
                        if show_conv { list_width } else { width },
                        list_scroll,
                        cx,
                    ))
                })
                .when(show_list && show_conv, |row| {
                    row.child(splitter(&theme, cx))
                })
                .when(show_conv, |row| {
                    let conv_width = if show_list {
                        (width - list_width).max(120.0)
                    } else {
                        width
                    };
                    row.child(conversation(
                        &theme,
                        session,
                        conv_scroll,
                        conv_width,
                        window,
                        cx,
                    ))
                }),
        )
        .child(status_bar(&theme, session))
        .when(
            session.which_visible && session.overlay == Overlay::None && !session.chord.is_empty(),
            |root| root.child(which_key_popup(&theme, session)),
        )
        .when(session.overlay != Overlay::None, |root| {
            root.child(overlay(
                &theme,
                session,
                &palette_scroll,
                &folder_scroll,
                cx,
            ))
        })
}

fn header(theme: &Theme, session: &Session) -> gpui::Div {
    let unread = session.unread_threads();
    div()
        .h(px(44.))
        .px(px(16.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(14.))
        .border_b(px(1.))
        .border_color(rgb(theme.border))
        .bg(rgb(theme.surface))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(session.account_name.clone()),
        )
        .child(
            div()
                .text_color(rgb(theme.dimmed))
                .child(session.account_email.clone()),
        )
        .child(div().flex_1())
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .child(session.folder_title()),
        )
        .child(
            div()
                .text_color(rgb(if unread > 0 {
                    theme.accent
                } else {
                    theme.dimmed
                }))
                .child(if unread == 1 {
                    "1 unread".to_string()
                } else {
                    format!("{unread} unread")
                }),
        )
}

fn thread_list(
    theme: &Theme,
    session: &Session,
    beside: bool,
    list_width: f32,
    scroll: UniformListScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    let entity = cx.entity();
    let theme_for_rows = theme.clone();
    let rows = uniform_list(
        "thread-rows",
        session.threads.len(),
        move |range, window, app| {
            entity.update(app, |inbox, cx| {
                let highlighted = inbox.session.highlighted();
                let text_width = (list_width - 2.0 - 3.0 - 24.0).max(48.0);
                range
                    .map(|index| {
                        let row = inbox.session.threads[index].clone();
                        thread_row(
                            &theme_for_rows,
                            &row,
                            index,
                            &highlighted,
                            text_width,
                            window,
                            cx,
                        )
                    })
                    .collect::<Vec<_>>()
            })
        },
    )
    .flex_1()
    .min_h(px(0.))
    .track_scroll(scroll);
    let mut pane = div()
        .id("thread-list")
        .flex()
        .flex_col()
        .min_h(px(0.))
        .min_w(px(0.))
        .overflow_hidden()
        .border_l(px(2.))
        .border_color(rgb(theme.border));
    pane = if beside {
        pane.w(px(list_width)).flex_none()
    } else {
        pane.flex_1()
    };
    if session.threads.is_empty() {
        pane.child(empty_state(theme, session))
    } else {
        pane.child(rows)
    }
}

fn thread_row(
    theme: &Theme,
    row: &ThreadRow,
    index: usize,
    highlighted: &std::collections::HashSet<String>,
    text_width: f32,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let selected = highlighted.contains(&row.id);
    let id = row.id.clone();
    let weight = if row.unread {
        FontWeight::BOLD
    } else {
        FontWeight::NORMAL
    };
    div()
        .id(gpui::ElementId::named_usize("thread", index))
        .h(px(68.))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .overflow_hidden()
        .bg(rgb(if selected {
            theme.selection
        } else {
            theme.background
        }))
        .text_color(rgb(if selected {
            theme.on_selection
        } else {
            theme.foreground
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |inbox, _, window, cx| {
                window.focus(&inbox.focus);
                inbox.select_thread(id.clone(), cx);
                cx.notify();
            }),
        )
        .child(div().w(px(3.)).h_full().bg(rgb(if row.unread {
            theme.accent
        } else {
            theme.background
        })))
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .h_full()
                .px(px(12.))
                .flex()
                .flex_col()
                .justify_center()
                .gap(px(3.))
                .child(thread_line_one(theme, row, weight, selected))
                .child(thread_line_two(
                    theme, row, weight, selected, text_width, window,
                )),
        )
        .into_any_element()
}

fn thread_line_one(
    theme: &Theme,
    row: &ThreadRow,
    weight: FontWeight,
    selected: bool,
) -> gpui::Div {
    let dim = if selected {
        theme.on_selection
    } else {
        theme.dimmed
    };
    div()
        .h(px(22.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .min_w(px(0.))
        .overflow_hidden()
        .child(names(theme, &row.spans, row.extra, weight, selected))
        .when(row.message_count > 1, |line| {
            line.child(
                div()
                    .text_color(rgb(dim))
                    .text_size(px(theme.ui_px - 1.0))
                    .child(row.message_count.to_string()),
            )
        })
        .when(row.starred, |line| {
            line.child(div().text_color(rgb(theme.star)).child("★"))
        })
        .when(row.has_attachment, |line| {
            line.child(div().text_color(rgb(dim)).child("⧉"))
        })
        .children(row.labels.iter().take(2).map(|label| {
            div()
                .px(px(6.))
                .py(px(1.))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme.border))
                .text_size(px((theme.ui_px - 2.0).max(11.0)))
                .text_color(rgb(dim))
                .child(label.clone())
        }))
        .child(div().flex_1())
        .child(
            div()
                .text_color(rgb(dim))
                .text_size(px(theme.ui_px - 1.0))
                .child(present::relative_date(now_unix(), row.date_unix)),
        )
}

fn names(
    theme: &Theme,
    spans: &[NameSpan],
    extra: usize,
    weight: FontWeight,
    selected: bool,
) -> gpui::Div {
    let dim = if selected {
        theme.on_selection
    } else {
        theme.dimmed
    };
    let mut line = div().flex().flex_row().min_w(px(0.)).overflow_hidden();
    for (index, span) in spans.iter().enumerate() {
        let span_weight = if weight == FontWeight::BOLD {
            FontWeight::BOLD
        } else if span.emphasis {
            FontWeight::SEMIBOLD
        } else {
            FontWeight::NORMAL
        };
        let last = index + 1 == spans.len() && extra == 0;
        let text = if last {
            span.text.clone()
        } else {
            format!("{}, ", span.text)
        };
        line = line.child(
            div()
                .font_weight(span_weight)
                .text_color(rgb(if selected {
                    theme.on_selection
                } else {
                    theme.foreground
                }))
                .child(text),
        );
    }
    if extra > 0 {
        line = line.child(div().text_color(rgb(dim)).child(format!("+{extra}")));
    }
    line
}

fn thread_line_two(
    theme: &Theme,
    row: &ThreadRow,
    weight: FontWeight,
    selected: bool,
    text_width: f32,
    window: &Window,
) -> gpui::Div {
    let snippet_color = if selected {
        theme.on_selection_dim
    } else {
        theme.dimmed
    };
    let subject_w = line_px(window, &row.subject, weight).min(280.0);
    let snippet_w = (text_width - subject_w - 8.0).max(24.0);
    let subject = clip_line(window, &row.subject, subject_w.min(text_width), weight);
    let snippet = clip_line(window, &row.snippet, snippet_w, FontWeight::NORMAL);
    div()
        .w(px(text_width))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(8.))
        .overflow_hidden()
        .child(
            div()
                .font_weight(weight)
                .flex_none()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(rgb(if selected {
                    theme.on_selection
                } else {
                    theme.foreground
                }))
                .child(subject),
        )
        .when(!row.snippet.is_empty(), |line| {
            line.child(
                div()
                    .w(px(snippet_w))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_color(rgb(snippet_color))
                    .child(snippet),
            )
        })
}

fn line_px(window: &Window, text: &str, weight: FontWeight) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let mut style = window.text_style();
    style.font_weight = weight;
    let font_px = style.font_size.to_pixels(window.rem_size());
    let run = style.to_run(text.len());
    let line = window.text_system().shape_line(
        SharedString::from(text.to_string()),
        font_px,
        &[run],
        None,
    );
    f32::from(line.width)
}

/// Shorten one line to `max_px`, ending on an ellipsis that reaches that width.
fn clip_line(window: &Window, text: &str, max_px: f32, weight: FontWeight) -> String {
    if line_px(window, text, weight) <= max_px {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut best = "…".to_string();
    let mut lo = 0;
    let mut hi = chars.len();
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let mut candidate: String = chars[..mid].iter().collect();
        candidate = candidate.trim_end().to_string();
        if !candidate.is_empty() {
            candidate.push('…');
        } else {
            candidate = "…".to_string();
        }
        if line_px(window, &candidate, weight) <= max_px {
            best = candidate;
            lo = mid;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    best
}

fn empty_state(theme: &Theme, session: &Session) -> gpui::Div {
    let banner = session.list_banner();
    div()
        .flex_1()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .px(px(24.))
        .child(
            div().text_color(rgb(theme.foreground)).child(
                banner
                    .as_ref()
                    .map(|item| item.title.clone())
                    .unwrap_or_default(),
            ),
        )
        .child(
            div().text_color(rgb(theme.dimmed)).child(
                banner
                    .as_ref()
                    .map(|item| item.hint.clone())
                    .unwrap_or_default(),
            ),
        )
}

fn splitter(theme: &Theme, cx: &mut Context<Inbox>) -> impl IntoElement {
    div()
        .id("split")
        .w(px(7.))
        .h_full()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .cursor_col_resize()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|inbox, _, _, cx| {
                inbox.dragging = true;
                cx.notify();
            }),
        )
        .child(div().w(px(1.)).h_full().bg(rgb(theme.border)))
}

fn conversation(
    theme: &Theme,
    session: &Session,
    scroll: ScrollHandle,
    conv_width: f32,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::Stateful<gpui::Div> {
    let subject = session
        .threads
        .iter()
        .find(|thread| Some(&thread.id) == session.cursor_id.as_ref())
        .map(|thread| thread.subject.clone())
        .unwrap_or_default();
    let messages: Vec<_> = session
        .messages
        .iter()
        .enumerate()
        .map(|(index, message)| {
            message_block(theme, session, message, index, conv_width, window, cx)
        })
        .collect();
    div()
        .id("conversation")
        .flex_1()
        .min_w(px(0.))
        .min_h(px(0.))
        .flex()
        .flex_col()
        .overflow_hidden()
        .border_l(px(2.))
        .border_color(rgb(if session.pane == Pane::Conversation {
            theme.accent
        } else {
            theme.background
        }))
        .when(!subject.is_empty(), |pane| {
            pane.child(
                div()
                    .px(px(28.))
                    .pt(px(22.))
                    .pb(px(14.))
                    .text_size(px(theme.ui_px + 11.0))
                    .font_weight(FontWeight::BOLD)
                    .line_height(px(theme.ui_px + 16.0))
                    .border_b(px(1.))
                    .border_color(rgb(theme.border))
                    .child(subject),
            )
        })
        .child(
            div()
                .id("conversation-scroll")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(&scroll)
                .px(px(20.))
                .pb(px(28.))
                .children(messages),
        )
}

fn message_block(
    theme: &Theme,
    session: &Session,
    message: &ConvMessage,
    index: usize,
    conv_width: f32,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let cursor = session.message_cursor == index && session.pane == Pane::Conversation;
    if message.expanded {
        expanded_message(theme, session, message, index, cursor, cx)
    } else {
        collapsed_message(theme, message, index, cursor, conv_width, window, cx)
    }
}

fn collapsed_message(
    theme: &Theme,
    message: &ConvMessage,
    index: usize,
    cursor: bool,
    conv_width: f32,
    window: &Window,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let date = present::relative_date(now_unix(), message.date_unix);
    let date_w = line_px(window, &date, FontWeight::NORMAL);
    // Sender column, gaps, row padding, and a little air before the date.
    let snippet_w = (conv_width - 56.0 - 140.0 - date_w).max(24.0);
    let snippet = clip_line(window, &message.snippet, snippet_w, FontWeight::NORMAL);
    div()
        .id(gpui::ElementId::named_usize("message", index))
        .h(px(40.))
        .w_full()
        .min_w(px(0.))
        .px(px(8.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .overflow_hidden()
        .rounded_md()
        .bg(rgb(if cursor {
            theme.selection
        } else {
            theme.background
        }))
        .text_color(rgb(if cursor {
            theme.on_selection
        } else {
            theme.foreground
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |inbox, _, window, cx| {
                window.focus(&inbox.focus);
                inbox.session.cancel_pending_keys();
                inbox.session.pane = Pane::Conversation;
                inbox.session.message_cursor = index;
                if let Some(message) = inbox.session.messages.get_mut(index) {
                    message.expanded = true;
                }
                cx.notify();
            }),
        )
        .child(
            div()
                .w(px(140.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .font_weight(FontWeight::MEDIUM)
                .child(message.sender.clone()),
        )
        .child(
            div()
                .w(px(snippet_w))
                .flex_none()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(rgb(if cursor {
                    theme.on_selection
                } else {
                    theme.dimmed
                }))
                .child(snippet),
        )
        .child(
            div()
                .flex_none()
                .text_color(rgb(if cursor {
                    theme.on_selection
                } else {
                    theme.dimmed
                }))
                .child(date),
        )
        .into_any_element()
}

fn expanded_message(
    theme: &Theme,
    session: &Session,
    message: &ConvMessage,
    index: usize,
    cursor: bool,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    let quotes = session.quotes_open.contains(&index);
    let dim = theme.dimmed;
    div()
        .id(gpui::ElementId::named_usize("message", index))
        .px(px(8.))
        .py(px(10.))
        .mb(px(8.))
        .rounded_md()
        .bg(rgb(if cursor {
            theme.surface
        } else {
            theme.background
        }))
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |inbox, _, window, cx| {
                window.focus(&inbox.focus);
                inbox.session.cancel_pending_keys();
                inbox.session.pane = Pane::Conversation;
                inbox.session.message_cursor = index;
                cx.notify();
            }),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(10.))
                .pb(px(8.))
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(message.sender.clone()),
                )
                .child(
                    div()
                        .text_color(rgb(dim))
                        .text_size(px(theme.ui_px - 1.0))
                        .child(message.email.clone()),
                )
                .when(!message.folders.is_empty(), |row| {
                    row.child(
                        div()
                            .text_color(rgb(dim))
                            .text_size(px(theme.ui_px - 1.0))
                            .child(message.folders.clone()),
                    )
                })
                .child(div().flex_1())
                .child(
                    div()
                        .text_color(rgb(dim))
                        .child(present::relative_date(now_unix(), message.date_unix)),
                ),
        )
        .children(
            message
                .body
                .blocks
                .iter()
                .map(|block| body_block(theme, block, quotes, index, cx)),
        )
        .into_any_element()
}

fn body_block(
    theme: &Theme,
    block: &BodyBlock,
    quotes_open: bool,
    index: usize,
    cx: &mut Context<Inbox>,
) -> gpui::AnyElement {
    match block {
        BodyBlock::Text(text) => div()
            .max_w(px(680.))
            .pb(px(8.))
            .line_height(px(theme.ui_px * 1.45))
            .child(text.clone())
            .into_any_element(),
        BodyBlock::Quote(text) | BodyBlock::Signature(text) => {
            let label = if matches!(block, BodyBlock::Quote(_)) {
                "Quoted text"
            } else {
                "Signature"
            };
            if quotes_open {
                div()
                    .max_w(px(680.))
                    .pl(px(10.))
                    .mb(px(8.))
                    .border_l(px(2.))
                    .border_color(rgb(theme.border))
                    .text_color(rgb(theme.dimmed))
                    .line_height(px(theme.ui_px * 1.4))
                    .child(text.clone())
                    .into_any_element()
            } else {
                div()
                    .h(px(28.))
                    .mb(px(6.))
                    .flex()
                    .items_center()
                    .text_color(rgb(theme.dimmed))
                    .child(format!("{label} · q"))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |inbox, _, window, cx| {
                            window.focus(&inbox.focus);
                            inbox.session.cancel_pending_keys();
                            inbox.session.pane = Pane::Conversation;
                            inbox.session.message_cursor = index;
                            if !inbox.session.quotes_open.remove(&index) {
                                inbox.session.quotes_open.insert(index);
                            }
                            cx.notify();
                        }),
                    )
                    .into_any_element()
            }
        }
    }
}

fn status_bar(theme: &Theme, session: &Session) -> gpui::Div {
    let hint = if !session.chord.is_empty() {
        session.prefix_text()
    } else if session.pane == Pane::Conversation {
        "j k  ·  esc  ·  ctrl-d  ·  space  ·  :".to_string()
    } else {
        "j k  ·  enter  ·  space  ·  :".to_string()
    };
    let count = if session.count.is_empty() {
        String::new()
    } else {
        format!("{} ", session.count)
    };
    div()
        .h(px(30.))
        .px(px(14.))
        .flex()
        .flex_row()
        .items_center()
        .gap(px(12.))
        .border_t(px(1.))
        .border_color(rgb(theme.border))
        .bg(rgb(theme.surface))
        .text_size(px((theme.ui_px - 1.0).max(12.0)))
        .child(
            div()
                .font_family_opt(theme.mono_family.clone())
                .text_color(rgb(if session.visual {
                    theme.accent
                } else {
                    theme.dimmed
                }))
                .child(session.mode_label().to_string()),
        )
        .child(div().text_color(rgb(theme.dimmed)).child("·"))
        .child(
            div()
                .text_color(rgb(if session.sync.is_error() {
                    theme.error
                } else {
                    theme.dimmed
                }))
                .child(session.sync.status()),
        )
        .when(session.detail_text().is_some(), |bar| {
            bar.child(div().text_color(rgb(theme.error)).child("· ? details"))
        })
        .child(
            div()
                .flex_1()
                .min_w(px(0.))
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(rgb(match &session.toast {
                    Some(toast) if toast.error => theme.error,
                    Some(_) => theme.foreground,
                    None => theme.dimmed,
                }))
                .child(
                    session
                        .toast
                        .as_ref()
                        .map(|toast| toast.text.clone())
                        .unwrap_or_default(),
                ),
        )
        .child(
            div()
                .font_family_opt(theme.mono_family.clone())
                .text_color(rgb(if session.chord.is_empty() {
                    theme.dimmed
                } else {
                    theme.accent
                }))
                .child(format!("{count}{hint}")),
        )
}

fn which_key_popup(theme: &Theme, session: &Session) -> gpui::Div {
    let items = session.which_items();
    div()
        .absolute()
        .bottom(px(40.))
        .left(px(16.))
        .min_w(px(220.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .px(px(12.))
        .py(px(10.))
        .flex()
        .flex_col()
        .gap(px(4.))
        .child(
            div()
                .pb(px(4.))
                .text_color(rgb(theme.dimmed))
                .font_family_opt(theme.mono_family.clone())
                .child(session.prefix_text()),
        )
        .children(items.into_iter().map(|item| {
            div()
                .flex()
                .flex_row()
                .gap(px(12.))
                .child(
                    div()
                        .w(px(72.))
                        .font_family_opt(theme.mono_family.clone())
                        .text_color(rgb(theme.accent))
                        .child(item.key),
                )
                .child(div().child(item.label))
        }))
}

fn overlay(
    theme: &Theme,
    session: &Session,
    palette_scroll: &ScrollHandle,
    folder_scroll: &ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    div()
        .absolute()
        .top(px(0.))
        .left(px(0.))
        .right(px(0.))
        .bottom(px(0.))
        .flex()
        .justify_center()
        .pt(px(72.))
        .occlude()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|inbox, _, _, cx| {
                inbox.session.overlay = Overlay::None;
                cx.notify();
            }),
        )
        .child(match session.overlay {
            Overlay::Palette => palette(theme, session, palette_scroll, cx).into_any_element(),
            Overlay::Folders => folder_picker(theme, session, folder_scroll, cx).into_any_element(),
            Overlay::Help => help(theme, session, cx).into_any_element(),
            Overlay::None => div().into_any_element(),
        })
}

fn palette(
    theme: &Theme,
    session: &Session,
    scroll: &ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let items = session.palette();
    let selected = session.palette_index;
    div()
        .w(px(560.))
        .max_h(px(420.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .h(px(44.))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .child(div().text_color(rgb(theme.accent)).child(":"))
                .child(div().child(session.palette_query.clone()))
                .child(div().w(px(2.)).h(px(16.)).bg(rgb(theme.accent))),
        )
        .child(
            div()
                .id("palette-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .py(px(4.))
                .children(items.iter().enumerate().map(|(index, item)| {
                    let action = item.action;
                    div()
                        .id(gpui::ElementId::named_usize("palette", index))
                        .h(px(36.))
                        .mx(px(4.))
                        .px(px(10.))
                        .flex()
                        .flex_row()
                        .items_center()
                        .rounded_md()
                        .bg(rgb(if index == selected {
                            theme.selection
                        } else {
                            theme.surface
                        }))
                        .text_color(rgb(if index == selected {
                            theme.on_selection
                        } else {
                            theme.foreground
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |inbox, _, window, cx| {
                                cx.stop_propagation();
                                window.focus(&inbox.focus);
                                inbox.session.overlay = Overlay::None;
                                let page = inbox.page_rows();
                                let message_before = inbox.session.message_cursor;
                                let thread_before = inbox.session.cursor_id.clone();
                                let folder_before = inbox.session.folder.clone();
                                let step = inbox.session.perform(action, page);
                                inbox.apply_step(
                                    step,
                                    message_before,
                                    thread_before,
                                    folder_before,
                                    cx,
                                );
                            }),
                        )
                        .child(div().flex_1().child(item.label.clone()))
                        .child(
                            div()
                                .font_family_opt(theme.mono_family.clone())
                                .text_size(px((theme.mono_px).max(12.0)))
                                .text_color(rgb(if index == selected {
                                    theme.on_selection
                                } else {
                                    theme.dimmed
                                }))
                                .child(item.binding.clone()),
                        )
                })),
        )
}

fn folder_picker(
    theme: &Theme,
    session: &Session,
    scroll: &ScrollHandle,
    cx: &mut Context<Inbox>,
) -> gpui::Div {
    let choices = session.folder_choices();
    let selected = session.folder_index;
    div()
        .w(px(420.))
        .max_h(px(380.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .h(px(44.))
                .px(px(14.))
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .child(
                    div()
                        .text_color(rgb(
                            if session.folder_intent == crate::model::FolderIntent::Move {
                                theme.accent
                            } else {
                                theme.dimmed
                            },
                        ))
                        .child(session.folder_prompt()),
                )
                .child(div().child(session.folder_query.clone()))
                .child(div().w(px(2.)).h(px(16.)).bg(rgb(theme.accent))),
        )
        .child(
            div()
                .id("folder-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .track_scroll(scroll)
                .py(px(4.))
                .children(choices.iter().enumerate().map(|(index, name)| {
                    let name = name.clone();
                    let label = present::folder_label(&name);
                    div()
                        .id(gpui::ElementId::named_usize("folder", index))
                        .h(px(34.))
                        .mx(px(4.))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .rounded_md()
                        .bg(rgb(if index == selected {
                            theme.selection
                        } else {
                            theme.surface
                        }))
                        .text_color(rgb(if index == selected {
                            theme.on_selection
                        } else {
                            theme.foreground
                        }))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |inbox, _, window, cx| {
                                cx.stop_propagation();
                                window.focus(&inbox.focus);
                                inbox.session.clear_toast();
                                inbox.session.folder_index = index;
                                let message_before = inbox.session.message_cursor;
                                let thread_before = inbox.session.cursor_id.clone();
                                let folder_before = inbox.session.folder.clone();
                                let step = inbox.session.confirm_folder();
                                inbox.apply_step(
                                    step,
                                    message_before,
                                    thread_before,
                                    folder_before,
                                    cx,
                                );
                            }),
                        )
                        .child(label)
                })),
        )
}

fn help(theme: &Theme, session: &Session, cx: &mut Context<Inbox>) -> gpui::Div {
    let lines: Vec<_> = crate::keymap::Action::all()
        .iter()
        .copied()
        .map(|action| {
            (
                action.label().to_string(),
                crate::keymap::binding_text(&session.keymap, action),
            )
        })
        .collect();
    div()
        .w(px(520.))
        .max_h(px(460.))
        .bg(rgb(theme.surface))
        .border_1()
        .border_color(rgb(theme.border))
        .rounded_md()
        .shadow_lg()
        .flex()
        .flex_col()
        .overflow_hidden()
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|_, _, _, cx| {
                cx.stop_propagation();
            }),
        )
        .child(
            div()
                .px(px(14.))
                .py(px(10.))
                .border_b(px(1.))
                .border_color(rgb(theme.border))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Keys"),
        )
        .when_some(session.detail_text(), |card, error| {
            card.child(
                div()
                    .px(px(14.))
                    .py(px(8.))
                    .text_color(rgb(theme.error))
                    .child(error),
            )
        })
        .child(
            div()
                .id("help-rows")
                .flex_1()
                .min_h(px(0.))
                .overflow_y_scroll()
                .px(px(14.))
                .py(px(8.))
                .flex()
                .flex_col()
                .gap(px(4.))
                .children(lines.into_iter().map(|(label, binding)| {
                    div()
                        .flex()
                        .flex_row()
                        .gap(px(12.))
                        .child(div().flex_1().child(label))
                        .child(
                            div()
                                .font_family_opt(theme.mono_family.clone())
                                .text_color(rgb(theme.dimmed))
                                .child(binding),
                        )
                })),
        )
}

fn archive_failure(err: &fell_core::Error) -> String {
    match err {
        fell_core::Error::Action(message) if message.contains("into Archive") => {
            "Already in Archive".into()
        }
        fell_core::Error::Action(message) if message.starts_with("no messages") => {
            "Nothing here to archive".into()
        }
        fell_core::Error::Action(message) if message.contains("no such thread") => {
            "No conversation".into()
        }
        other => other.to_string(),
    }
}

fn key_from(stroke: &Keystroke) -> Option<KeyId> {
    let bare = stroke.key.to_ascii_lowercase();
    if stroke.modifiers.control && stroke.modifiers.shift && bare == "p" {
        return Some(KeyId::CtrlShiftP);
    }
    if stroke.modifiers.control
        && !stroke.modifiers.alt
        && !stroke.modifiers.platform
        && let Some(ch) = ctrl_letter(&stroke.key)
    {
        return Some(KeyId::Ctrl(ch));
    }
    if stroke.modifiers.control || stroke.modifiers.alt || stroke.modifiers.platform {
        return None;
    }
    if let Some(ch) = physical_letter(&stroke.key) {
        return Some(KeyId::Char(if stroke.modifiers.shift {
            ch.to_ascii_uppercase()
        } else {
            ch.to_ascii_lowercase()
        }));
    }
    let typed = stroke
        .key_char
        .clone()
        .filter(|text| !text.is_empty())
        .unwrap_or_else(|| stroke.key.clone());
    match typed.as_str() {
        "escape" | "esc" => Some(KeyId::Escape),
        "backspace" => Some(KeyId::Backspace),
        "enter" | "return" => Some(KeyId::Enter),
        "space" | " " => Some(KeyId::Space),
        "up" => Some(KeyId::Up),
        "down" => Some(KeyId::Down),
        "left" | "right" | "tab" => None,
        other => {
            let mut chars = other.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return named_key(&stroke.key, stroke.modifiers.shift);
            }
            if stroke.modifiers.shift {
                Some(KeyId::Char(match ch {
                    '/' => '?',
                    ';' => ':',
                    other => other.to_ascii_uppercase(),
                }))
            } else {
                Some(KeyId::Char(ch))
            }
        }
    }
}

fn should_follow_visible(overlay: &Overlay, chord_empty: bool, pane: Pane, key: KeyId) -> bool {
    if *overlay != Overlay::None || !chord_empty {
        return false;
    }
    match key {
        KeyId::Char('j' | 'k' | 'q') => true,
        KeyId::Enter | KeyId::Char('o') => pane == Pane::Conversation,
        _ => false,
    }
}

fn ctrl_letter(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?.to_ascii_lowercase();
    if chars.next().is_some() || !ch.is_ascii_alphabetic() {
        return None;
    }
    Some(ch)
}

fn physical_letter(key: &str) -> Option<char> {
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() || !ch.is_ascii_alphabetic() {
        return None;
    }
    Some(ch)
}

fn named_key(key: &str, shift: bool) -> Option<KeyId> {
    match key {
        "escape" => Some(KeyId::Escape),
        "backspace" => Some(KeyId::Backspace),
        "enter" | "return" => Some(KeyId::Enter),
        "space" => Some(KeyId::Space),
        "up" => Some(KeyId::Up),
        "down" => Some(KeyId::Down),
        "g" if shift => Some(KeyId::Char('G')),
        "v" if shift => Some(KeyId::Char('V')),
        "slash" if shift => Some(KeyId::Char('?')),
        "semicolon" if shift => Some(KeyId::Char(':')),
        other if other.chars().count() == 1 => {
            let ch = other.chars().next().unwrap();
            Some(KeyId::Char(if shift {
                ch.to_ascii_uppercase()
            } else {
                ch
            }))
        }
        _ => None,
    }
}

fn window_size() -> (f32, f32) {
    if let Ok(raw) = std::env::var("FELL_WINDOW")
        && let Some((width, height)) = raw.split_once('x')
        && let (Ok(width), Ok(height)) = (width.parse::<f32>(), height.parse::<f32>())
    {
        return (width, height);
    }
    (1180.0, 760.0)
}

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

trait FontOpt: Styled {
    fn font_family_opt(self, family: Option<String>) -> Self;
}

impl<T: Styled> FontOpt for T {
    fn font_family_opt(self, family: Option<String>) -> Self {
        if let Some(family) = family {
            self.font_family(family)
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ctrl_letter, key_from, should_follow_visible};
    use crate::keymap::KeyId;
    use crate::model::Overlay;
    use crate::present::Pane;
    use gpui::{Keystroke, Modifiers};

    fn stroke(key: &str, key_char: &str, shift: bool) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                shift,
                ..Modifiers::default()
            },
            key: key.into(),
            key_char: Some(key_char.into()),
        }
    }

    #[test]
    fn caps_lock_uses_the_physical_letter() {
        assert_eq!(key_from(&stroke("j", "J", false)), Some(KeyId::Char('j')));
        assert_eq!(key_from(&stroke("k", "K", false)), Some(KeyId::Char('k')));
        assert_eq!(key_from(&stroke("g", "G", false)), Some(KeyId::Char('g')));
        assert_eq!(key_from(&stroke("g", "G", true)), Some(KeyId::Char('G')));
        assert_eq!(
            key_from(&stroke("slash", "?", true)),
            Some(KeyId::Char('?'))
        );
    }

    fn controlled(key: &str) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
            key: key.into(),
            key_char: None,
        }
    }

    #[test]
    fn ctrl_letter_is_only_a_single_character_name() {
        assert_eq!(ctrl_letter("d"), Some('d'));
        assert_eq!(ctrl_letter("U"), Some('u'));
        assert_eq!(ctrl_letter("down"), None);
        assert_eq!(ctrl_letter("up"), None);
        assert_eq!(ctrl_letter("delete"), None);
        assert_eq!(key_from(&controlled("d")), Some(KeyId::Ctrl('d')));
        assert_eq!(key_from(&controlled("u")), Some(KeyId::Ctrl('u')));
        assert_eq!(key_from(&controlled("down")), None);
        assert_eq!(key_from(&controlled("up")), None);
        assert_eq!(key_from(&controlled("delete")), None);
    }

    #[test]
    fn visible_message_follow_skips_overlays() {
        assert!(should_follow_visible(
            &Overlay::None,
            true,
            Pane::List,
            KeyId::Char('j')
        ));
        assert!(should_follow_visible(
            &Overlay::None,
            true,
            Pane::Conversation,
            KeyId::Char('q')
        ));
        assert!(should_follow_visible(
            &Overlay::None,
            true,
            Pane::Conversation,
            KeyId::Enter
        ));
        assert!(should_follow_visible(
            &Overlay::None,
            true,
            Pane::Conversation,
            KeyId::Char('o')
        ));
        assert!(!should_follow_visible(
            &Overlay::None,
            true,
            Pane::List,
            KeyId::Enter
        ));
        assert!(!should_follow_visible(
            &Overlay::None,
            true,
            Pane::List,
            KeyId::Char('o')
        ));
        for overlay in [&Overlay::Palette, &Overlay::Folders, &Overlay::Help] {
            assert!(!should_follow_visible(
                overlay,
                true,
                Pane::Conversation,
                KeyId::Char('j')
            ));
            assert!(!should_follow_visible(
                overlay,
                true,
                Pane::Conversation,
                KeyId::Enter
            ));
        }
        assert!(!should_follow_visible(
            &Overlay::None,
            false,
            Pane::Conversation,
            KeyId::Char('k')
        ));
    }
}
