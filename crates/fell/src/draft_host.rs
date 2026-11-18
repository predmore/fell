//! Hosts live drafts for `fell-mcp`: the draft socket, the `draft.md`
//! watcher, and the compose window an agent's `request_send` brings up.
//!
//! The inbox and the compose demo both run this. Nothing here transmits:
//! mail goes out only from the compose window's own send action.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use fell_core::compose::DraftHub;
use gpui::{
    App, AppContext, AsyncApp, Bounds, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions,
    px, size,
};
use notify::{Event, RecursiveMode, Watcher};

use crate::compose_ui::ComposeView;

/// Answer `fell-mcp` draft calls on the user's private socket. Another Fell
/// already serving it keeps it; this one then serves no agents.
pub fn serve_socket(hub: Arc<DraftHub>) {
    let path = fell_core::compose::draft_socket_path();
    let listener = match DraftHub::bind(&path) {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("fell: agent drafts are not served here: {err}");
            return;
        }
    };
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            hub.serve_one(stream);
        }
    });
}

/// Merge outside writes to the focused draft's `draft.md`, such as an
/// agent's script, into the live draft. Follows the hub's focus, starting
/// from `initial` when a draft is already open.
pub fn watch_folder(
    hub: Arc<DraftHub>,
    initial: Option<(String, PathBuf)>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let (tx, rx) = std::sync::mpsc::channel::<Result<Event, notify::Error>>();
        let Ok(mut watcher) = notify::recommended_watcher(move |event| {
            let _ = tx.send(event);
        }) else {
            return;
        };
        let mut current: Option<(String, PathBuf)> = None;
        if let Some((id, folder)) = initial {
            if watcher.watch(&folder, RecursiveMode::NonRecursive).is_err() {
                return;
            }
            current = Some((id, folder));
        }
        loop {
            if let Some(id) = hub.focused_id()
                && current
                    .as_ref()
                    .is_none_or(|(current_id, _)| *current_id != id)
                && let Some(next) = hub.with_session(&id, |session| session.folder().to_path_buf())
            {
                // The old watch ends either way: a folder that is gone took
                // its watch with it. A failed watch leaves nothing current,
                // so the next pass tries again.
                if let Some((_, folder)) = current.take() {
                    let _ = watcher.unwatch(&folder);
                }
                if watcher.watch(&next, RecursiveMode::NonRecursive).is_ok() {
                    // A write that landed before the watch began sent no
                    // event; merge whatever the file holds now.
                    merge_from_disk(&hub, &id, &next);
                    current = Some((id, next));
                }
            }
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(_) => {
                    let start = std::time::Instant::now();
                    while start.elapsed() < Duration::from_millis(150) {
                        let remaining = Duration::from_millis(150).saturating_sub(start.elapsed());
                        if rx.recv_timeout(remaining).is_err() {
                            break;
                        }
                    }
                    let Some((id, folder)) = current.clone() else {
                        continue;
                    };
                    merge_from_disk(&hub, &id, &folder);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    })
}

/// Merge `draft.md` in `folder` into live draft `id`, as an outside edit.
fn merge_from_disk(hub: &DraftHub, id: &str, folder: &std::path::Path) {
    if fell_core::compose::draft_file_is_symlink(folder) {
        return;
    }
    // Sample before reading: the read happens outside the lock, and a save
    // can land in between.
    let since = hub
        .with_session(id, |session| session.disk_generation())
        .unwrap_or(0);
    let content = std::fs::read_to_string(folder.join("draft.md")).ok();
    hub.with_session(id, |session| {
        session.push_disk_seen(content, since, 0);
        session.poll_disk(150);
    });
}

/// Serve agent drafts from the inbox: the socket, the folder watcher, and a
/// compose window that opens, or comes to the front, whenever an agent calls
/// `request_send`. Saves still queued when Fell quits are waited for.
/// The app's draft hub, for windows that want to hear from it.
pub(crate) struct AgentDrafts(pub Arc<DraftHub>);

impl gpui::Global for AgentDrafts {}

pub fn host_agent_drafts(cx: &mut App) -> Arc<DraftHub> {
    let hub = Arc::new(DraftHub::new());
    cx.set_global(AgentDrafts(hub.clone()));
    // Subscribe before the socket accepts anything, and count reveals from
    // zero, so a request_send that lands while the inbox is still opening
    // is not lost.
    let (tx, mut events) = futures::channel::mpsc::unbounded::<()>();
    hub.subscribe_with(move || tx.unbounded_send(()).is_ok());
    serve_socket(hub.clone());
    // The watcher thread lives as long as the app.
    let _ = watch_folder(hub.clone(), None);
    let quitting = hub.clone();
    cx.on_app_quit(move |_| {
        let hub = quitting.clone();
        async move { hub.settle_all() }
    })
    .detach();
    let windows: Rc<RefCell<Option<WindowHandle<ComposeView>>>> = Rc::default();
    follow_theme(cx, windows.clone());
    let host = hub.clone();
    cx.spawn(async move |cx| {
        let mut seen = 0;
        loop {
            let reveals = host.reveal_count();
            if reveals > seen {
                seen = reveals;
                if let Some(id) = host.focused_id() {
                    reveal(cx, &host, &id, &windows);
                }
            }
            if futures::StreamExt::next(&mut events).await.is_none() {
                break;
            }
            while events.try_recv().is_ok() {}
        }
    })
    .detach();
    hub
}

/// Bring the compose window to the front on draft `id`, opening one if
/// there is none. An open window follows the hub's focus by itself.
fn reveal(
    cx: &mut AsyncApp,
    hub: &Arc<DraftHub>,
    id: &str,
    window: &Rc<RefCell<Option<WindowHandle<ComposeView>>>>,
) {
    let open = *window.borrow();
    let raised = open.is_some_and(|handle| {
        handle
            .update(cx, |_, window, _| window.activate_window())
            .is_ok()
    });
    if !raised {
        let opened = open_compose_window(cx, hub.clone(), id);
        if opened.is_none() {
            // No window shows it, so it is not open for editing.
            hub.with_session(id, |session| session.close_editor());
        }
        *window.borrow_mut() = opened;
    }
}

/// One Omarchy theme watcher for the compose window, as the inbox has its
/// own. A theme change repaints the window if it still follows the theme.
fn follow_theme(cx: &mut App, window: Rc<RefCell<Option<WindowHandle<ComposeView>>>>) {
    let (changed, mut changes) = futures::channel::mpsc::unbounded();
    let watching = crate::theme::watch(changed);
    cx.spawn(async move |cx| {
        loop {
            if watching {
                if futures::StreamExt::next(&mut changes).await.is_none() {
                    break;
                }
            } else {
                cx.background_executor().timer(Duration::from_secs(1)).await;
            }
            let Some(handle) = *window.borrow() else {
                continue;
            };
            let next = cx
                .background_executor()
                .spawn(async { crate::theme::load() })
                .await;
            let _ = handle.update(cx, |view, _, cx| {
                if view.follows_look() {
                    view.set_look(next);
                    cx.notify();
                }
            });
        }
    })
    .detach();
}

/// A compose window on draft `id`, focused. `None` when the draft is not in
/// the hub or the window could not open.
fn open_compose_window(
    cx: &mut AsyncApp,
    hub: Arc<DraftHub>,
    id: &str,
) -> Option<WindowHandle<ComposeView>> {
    let folder = hub.with_session(id, |session| {
        session.open_for_edit();
        session.folder().to_path_buf()
    })?;
    let id = id.to_string();
    cx.update(|cx| {
        let (width, height) = compose_window_size();
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let handle = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some("Fell compose".into()),
                    ..Default::default()
                }),
                app_id: Some("dev.predmore.fell".into()),
                focus: true,
                show: true,
                ..Default::default()
            },
            move |_window, cx| {
                cx.new(|cx| {
                    let mut compose = ComposeView::new(hub, id, folder, cx);
                    compose.set_look(crate::theme::load());
                    compose.start_on_first_edit();
                    compose.watch_live(cx);
                    compose
                })
            },
        );
        cx.activate(true);
        handle.ok()
    })
    .ok()
    .flatten()
}

/// The compose window's first size. `FELL_COMPOSE_WINDOW=WxH` overrides it,
/// as `FELL_WINDOW` does for the inbox.
fn compose_window_size() -> (f32, f32) {
    std::env::var("FELL_COMPOSE_WINDOW")
        .ok()
        .and_then(|raw| {
            let (width, height) = raw.split_once('x')?;
            Some((width.parse().ok()?, height.parse().ok()?))
        })
        .unwrap_or((900.0, 760.0))
}
