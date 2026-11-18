//! Hosts live drafts for `fell-mcp`: the draft socket, the `draft.md`
//! watcher, and the compose window an agent's `request_send` brings up.
//!
//! The inbox and the compose demo both run this. Nothing here transmits:
//! mail goes out only from the compose window's own send action.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fell_core::compose::DraftHub;
use gpui::{
    App, AppContext, AsyncApp, Bounds, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions,
    px, size,
};
use notify::{Event, RecursiveMode, Watcher};

use crate::compose_ui::{ComposeView, ThemeKind};

/// Answer `fell-mcp` draft calls on the user's private socket. Another Fell
/// already serving it keeps it; this one then serves no agents.
pub fn serve_socket(hub: Arc<DraftHub>) {
    let path = fell_core::compose::draft_socket_path();
    let listener = match DraftHub::bind(&path) {
        Ok(listener) => listener,
        Err(_) => return,
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
                let released = match &current {
                    Some((_, folder)) => watcher.unwatch(folder).is_ok(),
                    None => true,
                };
                if released && watcher.watch(&next, RecursiveMode::NonRecursive).is_ok() {
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
                    if fell_core::compose::draft_file_is_symlink(&folder) {
                        continue;
                    }
                    // Sample before reading: the read happens outside the
                    // lock, and a save can land in between.
                    let since = hub
                        .with_session(&id, |session| session.disk_generation())
                        .unwrap_or(0);
                    let content = std::fs::read_to_string(folder.join("draft.md")).ok();
                    hub.with_session(&id, |session| {
                        session.push_disk_seen(content, since, 0);
                        session.poll_disk(150);
                    });
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    })
}

/// Serve agent drafts from the inbox: the socket, the folder watcher, and a
/// compose window that opens, or comes to the front, whenever an agent calls
/// `request_send`. Saves still queued when Fell quits are waited for.
pub fn host_agent_drafts(cx: &mut App, theme: ThemeKind) -> Arc<DraftHub> {
    let hub = Arc::new(DraftHub::new());
    serve_socket(hub.clone());
    // The watcher thread lives as long as the app.
    let _ = watch_folder(hub.clone(), None);
    let quitting = hub.clone();
    cx.on_app_quit(move |_| {
        let hub = quitting.clone();
        async move { hub.settle_all() }
    })
    .detach();
    let events = Arc::new(Mutex::new(hub.subscribe()));
    let host = hub.clone();
    cx.spawn(async move |cx| {
        let mut seen = host.reveal_count();
        let mut window: Option<WindowHandle<ComposeView>> = None;
        loop {
            let events = events.clone();
            let alive = cx
                .background_executor()
                .spawn(async move { events.lock().ok().is_some_and(|rx| rx.recv().is_ok()) })
                .await;
            if !alive {
                break;
            }
            let reveals = host.reveal_count();
            if reveals == seen {
                continue;
            }
            seen = reveals;
            let Some(id) = host.focused_id() else {
                continue;
            };
            // An open compose window follows the hub's focus by itself.
            let raised = window.is_some_and(|handle| {
                handle
                    .update(cx, |_, window, _| window.activate_window())
                    .is_ok()
            });
            if !raised {
                window = open_compose_window(cx, host.clone(), &id, theme);
            }
        }
    })
    .detach();
    hub
}

/// A compose window on draft `id`, focused. `None` when the draft is not in
/// the hub or the window could not open.
fn open_compose_window(
    cx: &mut AsyncApp,
    hub: Arc<DraftHub>,
    id: &str,
    theme: ThemeKind,
) -> Option<WindowHandle<ComposeView>> {
    let folder = hub.with_session(id, |session| {
        session.open_for_edit();
        session.folder().to_path_buf()
    })?;
    let id = id.to_string();
    cx.update(|cx| {
        let bounds = Bounds::centered(None, size(px(900.), px(760.)), cx);
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
                    compose.set_theme(theme);
                    compose.follow_omarchy_theme(cx);
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
