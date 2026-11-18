//! GPUI shell for Fell. Linux / Wayland is the first target (Omarchy, Hyprland).
//!
//! This crate renders. It does not speak IMAP or Proton. Compose edits plain
//! markdown and sends only from this window.

use fell::render::{self, Reader};
use fell::{ComposeView, ThemeKind, open_demo};
use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

const USAGE: &str = "\
Usage:
  fell                         open the inbox
  fell account setup|show|check
                               connect and test a mail account
  fell embeddings status|install|remove
                               the local search model
  fell --help                  print this help

`fell account --help` and `fell embeddings --help` list their options.

Development previews:
  fell --fixture NAME [--theme dark|light]
                               one message from crates/fell-render/fixtures
  fell [--theme dark|light] [--narrow] [--width PX] [--height PX] [--play]
                               the compose window on a demo draft";

/// What the command line asks for.
#[derive(Debug, Clone, PartialEq)]
enum Command {
    Inbox,
    Help,
    Account(Vec<String>),
    Embeddings(Vec<String>),
    Fixture { name: String, light: bool },
    Compose(ComposePreview),
}

#[derive(Debug, Clone, PartialEq)]
struct ComposePreview {
    light: bool,
    width: f32,
    height: f32,
    play: bool,
}

/// Parse everything after the program name. An `Err` is a message for
/// stderr; the caller adds the usage text and exits 2.
fn parse_args(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        Some("account") => return Ok(Command::Account(args[1..].to_vec())),
        Some("embeddings") => return Ok(Command::Embeddings(args[1..].to_vec())),
        _ => {}
    }
    let mut fixture = None;
    let mut light = None;
    let mut compose = ComposePreview {
        light: false,
        width: 1100.0,
        height: 760.0,
        play: false,
    };
    let mut compose_flag = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .map(String::as_str)
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--help" | "-h" | "help" => return Ok(Command::Help),
            "--fixture" => match value("--fixture")? {
                "--help" | "-h" => return Ok(Command::Help),
                name => fixture = Some(name.to_string()),
            },
            "--theme" => {
                light = Some(match value("--theme")? {
                    "dark" => false,
                    "light" => true,
                    other => return Err(format!("bad --theme {other:?} (dark or light)")),
                });
            }
            "--narrow" => {
                compose.width = 420.0;
                compose.height = 900.0;
                compose_flag = Some("--narrow");
            }
            "--width" => {
                compose.width = size_value("--width", value("--width")?)?;
                compose_flag = Some("--width");
            }
            "--height" => {
                compose.height = size_value("--height", value("--height")?)?;
                compose_flag = Some("--height");
            }
            "--play" => {
                compose.play = true;
                compose_flag = Some("--play");
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other:?}")),
            other => return Err(format!("unknown command {other:?}")),
        }
    }
    let light_set = light.is_some();
    let light = light.unwrap_or(false);
    match (fixture, compose_flag) {
        (Some(_), Some(flag)) => Err(format!("{flag} is for the compose preview, not --fixture")),
        (Some(name), None) => Ok(Command::Fixture { name, light }),
        (None, Some(_)) => Ok(Command::Compose(ComposePreview { light, ..compose })),
        (None, None) if light_set => Ok(Command::Compose(ComposePreview { light, ..compose })),
        (None, None) => Ok(Command::Inbox),
    }
}

fn size_value(name: &str, raw: &str) -> Result<f32, String> {
    match raw.parse::<f32>() {
        Ok(value) if value.is_finite() && value >= 1.0 => Ok(value),
        _ => Err(format!("bad {name} {raw:?} (a size in pixels)")),
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match parse_args(&args) {
        Ok(command) => command,
        Err(err) => {
            eprintln!("fell: {err}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    match command {
        Command::Help => println!("{USAGE}"),
        Command::Account(args) => std::process::exit(fell::account_command(&args)),
        Command::Embeddings(args) => std::process::exit(fell::embeddings_command(&args)),
        Command::Inbox => fell::run(),
        Command::Fixture { name, light } => preview(name, light),
        Command::Compose(preview) => compose_preview(preview),
    }
}

fn compose_preview(preview: ComposePreview) {
    let ComposePreview {
        light,
        width,
        height,
        play,
    } = preview;
    let theme = if light {
        ThemeKind::Light
    } else {
        ThemeKind::Dark
    };
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

fn preview(fixture: String, light: bool) {
    let theme = if light {
        render::ThemeKind::Light
    } else {
        render::ThemeKind::Dark
    };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(list: &[&str]) -> Result<Command, String> {
        parse_args(&list.iter().map(|arg| arg.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn help_never_opens_a_window() {
        for args in [
            &["--help"][..],
            &["-h"],
            &["help"],
            &["--fixture", "x", "--help"],
            &["--fixture", "--help"],
            &["--fixture", "-h"],
            &["--narrow", "-h"],
        ] {
            assert_eq!(parse(args), Ok(Command::Help), "{args:?}");
        }
        for line in USAGE.lines() {
            assert!(line.chars().count() <= 80, "{line}");
        }
    }

    #[test]
    fn unknown_options_and_values_are_errors() {
        assert_eq!(
            parse(&["--bogus"]),
            Err("unknown option \"--bogus\"".into())
        );
        assert_eq!(parse(&["inbox"]), Err("unknown command \"inbox\"".into()));
        assert!(parse(&["--theme"]).unwrap_err().contains("needs a value"));
        assert!(
            parse(&["--theme", "blue"])
                .unwrap_err()
                .contains("dark or light")
        );
        assert!(parse(&["--width", "wide"]).unwrap_err().contains("--width"));
        assert!(parse(&["--height", "0"]).unwrap_err().contains("--height"));
        assert!(parse(&["--fixture"]).unwrap_err().contains("needs a value"));
        assert!(
            parse(&["--fixture", "x", "--play"])
                .unwrap_err()
                .contains("compose preview")
        );
    }

    #[test]
    fn known_forms_still_parse() {
        assert_eq!(parse(&[]), Ok(Command::Inbox));
        assert_eq!(
            parse(&["account", "setup", "--bogus"]),
            Ok(Command::Account(vec!["setup".into(), "--bogus".into()]))
        );
        assert_eq!(
            parse(&["embeddings", "--help"]),
            Ok(Command::Embeddings(vec!["--help".into()]))
        );
        assert_eq!(
            parse(&["--fixture", "native-personal-reply", "--theme", "light"]),
            Ok(Command::Fixture {
                name: "native-personal-reply".into(),
                light: true
            })
        );
        assert_eq!(
            parse(&["--theme", "light"]),
            Ok(Command::Compose(ComposePreview {
                light: true,
                width: 1100.0,
                height: 760.0,
                play: false
            }))
        );
        assert_eq!(
            parse(&["--narrow", "--width", "500", "--play"]),
            Ok(Command::Compose(ComposePreview {
                light: false,
                width: 500.0,
                height: 900.0,
                play: true
            }))
        );
    }
}
