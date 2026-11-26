//! GPUI shell for Fell. Linux / Wayland is the first target (Omarchy, Hyprland).
//!
//! This crate renders. It does not speak IMAP or Proton. Compose edits plain
//! markdown and sends only from this window.

use fell::compose_launch::{self, ComposeLaunch};
use fell::render::{self, Reader};
use fell::{ComposeView, ThemeKind, open_demo};
use fell_core::{diagnostic, diagnostics};
use std::ffi::OsString;
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::Instant;

use gpui::{
    App, AppContext, Application, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

const USAGE: &str = "\
Usage:
  fell                         set up a mail account or open the inbox
  fell --demo                  open a sample inbox if no account is configured
  fell --trace                 record inbox startup and frame timing
  fell account setup|list|remove|show|check
                               connect and test a mail account
  fell embeddings status|install|remove
                               the local search model
  fell mcp add|list|revoke     connect an agent (runs fell-mcp)
  fell 'mailto:ADDRESS?subject=SUBJECT&body=TEXT'
                               open a draft from a mailto link
  fell compose [--to ADDRESS] [--cc ADDRESS] [--bcc ADDRESS]
               [--subject TEXT] [--body MARKDOWN] [--attach FILE]…
                               open a draft for editing
  fell --report                write a diagnostic report for review
  fell --version               print the build version
  fell --help                  print this help

`fell account --help` and `fell embeddings --help` list their options.

Development previews:
  fell --fixture NAME [--theme dark|light]
                               one message from crates/fell-render/fixtures
  fell [--theme dark|light] [--narrow] [--width PX] [--height PX] [--play]
                               the compose window on a demo draft";

/// What the command line asks for.
#[derive(Debug, PartialEq)]
enum Command {
    Inbox { demo: bool, trace: bool },
    Help,
    Version,
    Report,
    Account(Vec<String>),
    Embeddings(Vec<String>),
    Mcp(Vec<OsString>),
    Fixture { name: String, light: bool },
    Compose(ComposeLaunch),
    ComposePreview(ComposePreview),
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
fn parse_args(args: &[OsString]) -> Result<Command, String> {
    match args.first().and_then(|arg| arg.to_str()) {
        Some("mcp") => return Ok(Command::Mcp(args[1..].to_vec())),
        Some("compose") => {
            if args.len() == 2 && (args[1] == "--help" || args[1] == "-h") {
                return Ok(Command::Help);
            }
            return compose_launch::cli(&args[1..]).map(Command::Compose);
        }
        Some(uri) if compose_launch::is_mailto(uri) => {
            if args.len() != 1 {
                return Err("expected a single mailto link".into());
            }
            return compose_launch::mailto(uri).map(Command::Compose);
        }
        _ => {}
    }
    let args = args
        .iter()
        .map(|arg| {
            arg.to_str()
                .map(str::to_owned)
                .ok_or("arguments must be UTF-8")
        })
        .collect::<Result<Vec<_>, _>>()?;
    parse_text_args(&args)
}

fn parse_text_args(args: &[String]) -> Result<Command, String> {
    match args.first().map(String::as_str) {
        Some("account") => return Ok(Command::Account(args[1..].to_vec())),
        Some("embeddings") => return Ok(Command::Embeddings(args[1..].to_vec())),
        _ => {}
    }
    if args == ["--report"] {
        return Ok(Command::Report);
    }
    let mut fixture = None;
    let mut trace = false;
    let mut demo = false;
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
            "--version" | "-V" => return Ok(Command::Version),
            "--trace" => trace = true,
            "--demo" => demo = true,
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
    if (trace || demo) && (fixture.is_some() || compose_flag.is_some() || light_set) {
        let flag = if trace { "--trace" } else { "--demo" };
        return Err(format!("{flag} is for the inbox"));
    }
    match (fixture, compose_flag) {
        (Some(_), Some(flag)) => Err(format!("{flag} is for the compose preview, not --fixture")),
        (Some(name), None) => Ok(Command::Fixture { name, light }),
        (None, Some(_)) => Ok(Command::ComposePreview(ComposePreview { light, ..compose })),
        (None, None) if light_set => {
            Ok(Command::ComposePreview(ComposePreview { light, ..compose }))
        }
        (None, None) => Ok(Command::Inbox { demo, trace }),
    }
}

fn size_value(name: &str, raw: &str) -> Result<f32, String> {
    match raw.parse::<f32>() {
        Ok(value) if value.is_finite() && value >= 1.0 => Ok(value),
        _ => Err(format!("bad {name} {raw:?} (a size in pixels)")),
    }
}

fn main() {
    let started = Instant::now();
    if diagnostics::init().is_err() {
        let _ = writeln!(io::stderr(), "Fell could not initialize its diagnostic log");
    }
    let raw: Vec<OsString> = std::env::args_os().skip(1).collect();
    let command = match parse_args(&raw) {
        Ok(command) => command,
        Err(err) => {
            diagnostic!(WARN, "Fell command line rejected");
            let _ = writeln!(io::stderr(), "fell: {err}\n\n{USAGE}");
            std::process::exit(2);
        }
    };
    match command {
        Command::Help => println!("{USAGE}"),
        Command::Version => println!("{}", fell_version::VERSION),
        Command::Report => match diagnostics::write_report() {
            Ok(path) => println!("{}", path.display()),
            Err(err) => {
                diagnostic!(
                    ERROR,
                    "Could not write diagnostic report",
                    error = &err as &dyn std::error::Error
                );
                let _ = writeln!(
                    io::stderr(),
                    "fell: could not write diagnostic report: {err}"
                );
                std::process::exit(1);
            }
        },
        Command::Account(args) => std::process::exit(fell::account_command(&args)),
        Command::Embeddings(args) => std::process::exit(fell::embeddings_command(&args)),
        Command::Mcp(args) => mcp(&args),
        Command::Inbox { demo, trace } => {
            let trace = trace || std::env::var_os("FELL_TRACE").is_some_and(|value| value == "1");
            match (demo, trace) {
                (true, trace) => fell::run_demo(trace.then_some(started)),
                (false, true) => fell::run_traced(started),
                (false, false) => fell::run(),
            }
        }
        Command::Fixture { name, light } => preview(name, light),
        Command::Compose(request) => {
            if let Err(err) = fell::run_compose(request) {
                let _ = writeln!(io::stderr(), "fell: {err}");
                std::process::exit(1);
            }
        }
        Command::ComposePreview(preview) => compose_preview(preview),
    }
}

/// `fell mcp …` runs the `fell-mcp` program installed next to this one. The
/// MCP server stays its own process: it never links the UI's send
/// confirmation. In the AppImage both live in `usr/bin`.
fn mcp(args: &[OsString]) -> ! {
    use std::os::unix::process::CommandExt;
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("fell-mcp")))
        .filter(|path| path.is_file());
    let Some(program) = sibling else {
        diagnostic!(ERROR, "Fell MCP executable is missing");
        let _ = writeln!(io::stderr(), "fell: fell-mcp is not installed next to fell");
        std::process::exit(2);
    };
    // Must match `fell_mcp::FELL_MCP_ARG0`; fell does not link fell-mcp.
    let err = std::process::Command::new(&program)
        .arg0("fell mcp")
        .args(args)
        .exec();
    diagnostic!(
        ERROR,
        "Could not start Fell MCP",
        error = &err as &dyn std::error::Error
    );
    let _ = writeln!(
        io::stderr(),
        "fell: could not start {}: {err}",
        program.display()
    );
    std::process::exit(2);
}

fn compose_preview_dir(parent: &Path) -> io::Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix("fell-compose-")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir_in(parent)
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
    let root =
        compose_preview_dir(&std::env::temp_dir()).expect("create compose preview directory");
    let (hub, draft_id, folder) =
        open_demo(root.path().to_path_buf(), theme).expect("open demo draft");
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
            move |window, cx| {
                cx.new(|cx| {
                    let mut compose =
                        ComposeView::new(hub.clone(), draft_id.clone(), folder.clone(), cx);
                    compose.set_theme(theme);
                    compose.watch_live(window, cx);
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

    #[test]
    fn compose_preview_directory_is_cleaned_up_on_exit() {
        let Some(parent) = fell_core::test_support::in_isolated_process() else {
            return;
        };
        let path = {
            let root = compose_preview_dir(&parent).unwrap();
            let path = root.path().to_path_buf();
            let (_hub, _, folder) = open_demo(path.clone(), ThemeKind::Dark).unwrap();
            assert!(folder.join("draft.md").is_file());
            assert!(path.join("mail.db").is_file());
            path
        };
        assert!(!path.exists());
    }

    fn parse(list: &[&str]) -> Result<Command, String> {
        parse_args(&list.iter().map(OsString::from).collect::<Vec<_>>())
    }

    #[test]
    fn the_demo_inbox_is_explicit() {
        assert_eq!(
            parse(&[]),
            Ok(Command::Inbox {
                demo: false,
                trace: false
            })
        );
        assert_eq!(
            parse(&["--demo"]),
            Ok(Command::Inbox {
                demo: true,
                trace: false
            })
        );
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
    fn compose_and_mailto_are_live_drafts() {
        assert!(matches!(parse(&["compose"]), Ok(Command::Compose(_))));
        assert!(matches!(
            parse(&["compose", "--to", "reader@example.org"]),
            Ok(Command::Compose(_))
        ));
        assert!(matches!(
            parse(&["MAILTO:reader@example.org?subject=Hello"]),
            Ok(Command::Compose(_))
        ));
        assert_eq!(parse(&["compose", "--help"]), Ok(Command::Help));
        assert!(parse(&["mailto:reader@example.org", "--demo"]).is_err());
    }

    #[test]
    fn known_forms_still_parse() {
        assert_eq!(
            parse(&[]),
            Ok(Command::Inbox {
                demo: false,
                trace: false
            })
        );
        assert_eq!(parse(&["--report"]), Ok(Command::Report));
        assert!(parse(&["--report", "--trace"]).is_err());
        assert_eq!(
            parse(&["account", "setup", "--bogus"]),
            Ok(Command::Account(vec!["setup".into(), "--bogus".into()]))
        );
        assert_eq!(
            parse(&["embeddings", "--help"]),
            Ok(Command::Embeddings(vec!["--help".into()]))
        );
        assert_eq!(
            parse(&["mcp", "add", "codex", "--help"]),
            Ok(Command::Mcp(vec![
                "add".into(),
                "codex".into(),
                "--help".into()
            ]))
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
            Ok(Command::ComposePreview(ComposePreview {
                light: true,
                width: 1100.0,
                height: 760.0,
                play: false
            }))
        );
        assert_eq!(
            parse(&["--narrow", "--width", "500", "--play"]),
            Ok(Command::ComposePreview(ComposePreview {
                light: false,
                width: 500.0,
                height: 900.0,
                play: true
            }))
        );
    }

    #[test]
    fn trace_opens_only_the_inbox() {
        assert_eq!(
            parse(&["--trace"]),
            Ok(Command::Inbox {
                demo: false,
                trace: true
            })
        );
        assert_eq!(
            parse(&["--trace", "--demo"]),
            Ok(Command::Inbox {
                demo: true,
                trace: true
            })
        );
        assert!(parse(&["--trace", "--fixture", "native-personal-reply"]).is_err());
        assert!(parse(&["--trace", "--play"]).is_err());
    }
}
