//! GPUI shell for Fell. Linux / Wayland is the first target (Omarchy, Hyprland).
//!
//! With no arguments this opens the inbox. `--fixture` opens one stored
//! message so the HTML renderer can be seen. That preview is not the inbox.

use fell::render::{Reader, ThemeKind, open_fixture};
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--fixture" || arg == "--theme" || arg == "--help") {
        preview();
    } else {
        fell::run();
    }
}

fn preview() {
    let (fixture, theme) = parse_args();
    let opened = open_fixture(&fixture, theme);
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

fn parse_args() -> (String, ThemeKind) {
    let mut fixture = "native-personal-reply".to_string();
    let mut theme = ThemeKind::Dark;
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
                    ThemeKind::Light
                } else {
                    ThemeKind::Dark
                };
            }
            "--help" => {
                eprintln!(
                    "fell --fixture <name> --theme dark|light\n\
                     fixtures live in crates/fell-render/fixtures"
                );
            }
            _ => {}
        }
    }
    (fixture, theme)
}
