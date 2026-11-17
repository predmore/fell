//! GPUI shell for Fell. Linux / Wayland is the first target (Omarchy, Hyprland).
//!
//! This crate renders. It does not speak IMAP or Proton. Compose edits plain
//! markdown and sends only from this window.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use fell::{ComposeView, ThemeKind, open_demo};
use fell_core::compose::DraftHub;
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};

fn main() {
    let mut theme = ThemeKind::Dark;
    let mut width = 1100.0;
    let mut height = 760.0;
    let mut play = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--theme" => {
                theme = match args.next().as_deref() {
                    Some("light") => ThemeKind::Light,
                    _ => ThemeKind::Dark,
                };
            }
            "--narrow" => {
                width = 420.0;
                height = 640.0;
            }
            "--width" => {
                if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                    width = value;
                }
            }
            "--height" => {
                if let Some(value) = args.next().and_then(|value| value.parse().ok()) {
                    height = value;
                }
            }
            "--play" => play = true,
            _ => {}
        }
    }
    let root = std::env::temp_dir().join(format!("fell-compose-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (hub, draft_id, folder) = open_demo(root, theme).expect("open demo draft");
    serve_socket(hub.clone());
    let _watcher = watch_folder(hub.clone(), draft_id.clone(), folder.clone());
    let draft_id_for_window = draft_id.clone();
    let folder_for_window = folder.clone();
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(width), px(height)), cx);
        let hub = hub.clone();
        let draft_id = draft_id_for_window.clone();
        let folder = folder_for_window.clone();
        let theme = theme;
        cx.open_window(
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
            move |_window, cx| {
                cx.new(|cx| {
                    let mut compose =
                        ComposeView::new(hub.clone(), draft_id.clone(), folder.clone(), cx);
                    compose.set_theme(theme);
                    compose.watch_live(cx);
                    if play {
                        compose.play_edit(cx);
                    }
                    compose
                })
            },
        )
        .expect("open Fell window");
        cx.activate(true);
    });
}

fn serve_socket(hub: Arc<DraftHub>) {
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

fn watch_folder(
    hub: Arc<DraftHub>,
    draft_id: String,
    folder: PathBuf,
) -> Option<RecommendedWatcher> {
    let (tx, rx) = std::sync::mpsc::channel::<Result<Event, notify::Error>>();
    let mut watcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })
    .ok()?;
    watcher.watch(&folder, RecursiveMode::NonRecursive).ok()?;
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            let start = std::time::Instant::now();
            while start.elapsed() < Duration::from_millis(150) {
                let remaining = Duration::from_millis(150).saturating_sub(start.elapsed());
                if rx.recv_timeout(remaining).is_err() {
                    break;
                }
            }
            let path = folder.join("draft.md");
            let content = std::fs::read_to_string(&path).ok();
            hub.with_session(&draft_id, |session| {
                session.push_disk(content, 0);
                session.poll_disk(150);
            });
        }
    });
    Some(watcher)
}
