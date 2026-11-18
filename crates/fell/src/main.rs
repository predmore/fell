//! GPUI shell for Fell. Linux / Wayland is the first target (Omarchy, Hyprland).
//!
//! This crate renders. It does not speak IMAP or Proton. Compose edits plain
//! markdown and sends only from this window.

use fell::render::{self, Reader};
use fell::{ComposeView, ThemeKind, open_demo};
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("account") {
        std::process::exit(fell::account_command(&args[1..]));
    }
    if args.first().map(String::as_str) == Some("embeddings") {
        std::process::exit(fell::embeddings_command(&args[1..]));
    }
    if args.iter().any(|arg| arg == "--fixture" || arg == "--help") {
        preview();
        return;
    }
    let compose = args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--theme" | "--narrow" | "--width" | "--height" | "--play"
        )
    });
    if !compose {
        fell::run();
        return;
    }
    let mut theme = ThemeKind::Dark;
    let mut width = 1100.0;
    let mut height = 760.0;
    let mut play = false;
    let mut args = args.into_iter();
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
                height = 900.0;
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
    fell::draft_host::serve_socket(hub.clone());
    let _watcher =
        fell::draft_host::watch_folder(hub.clone(), Some((draft_id.clone(), folder.clone())));
    let draft_id_for_window = draft_id.clone();
    let folder_for_window = folder.clone();
    Application::new().run(move |cx: &mut App| {
        // Saves run on writer threads. Quitting waits for the queued ones.
        let quitting = hub.clone();
        cx.on_app_quit(move |_| {
            let hub = quitting.clone();
            async move { hub.settle_all() }
        })
        .detach();
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

fn preview() {
    let mut fixture = "native-personal-reply".to_string();
    let mut theme = render::ThemeKind::Dark;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--fixture" => {
                if let Some(name) = args.next() {
                    fixture = name;
                }
            }
            "--theme" => {
                theme = if args.next().as_deref() == Some("light") {
                    render::ThemeKind::Light
                } else {
                    render::ThemeKind::Dark
                };
            }
            "--help" => {
                eprintln!(
                    "fell                      open the inbox\n\
                     fell account setup        connect Proton Mail Bridge or an IMAP account\n\
                     fell --fixture <name> --theme dark|light\n\
                     fixtures live in crates/fell-render/fixtures"
                );
            }
            _ => {}
        }
    }
    let opened = render::open_fixture(&fixture, theme);
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(960.), px(720.)), cx);
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
            move |_window, cx| cx.new(|cx| Reader::new(opened, cx)),
        )
        .expect("open Fell window");
        cx.activate(true);
    });
}
