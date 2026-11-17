//! `fell account setup|show|check`: write and test `~/.config/fell/account.toml`.
//!
//! Setup asks for anything not given as a flag, checks the login against the
//! server, and only then writes the file (mode 0600). It reads no mail.

use std::io::{self, BufRead, IsTerminal, Write};

use fell_core::{AccountConfig, BackendKind, ImapSecurity, PasswordSource, classify_sync_error};

use crate::mailbox::{load_account_file, write_account_config};

const USAGE: &str = "\
fell account setup [options]   write ~/.config/fell/account.toml after a login check
fell account show              print the account file without the password
fell account check             log in with the account file and log out

setup options (anything missing is asked for on a terminal):
  --imap                       a plain IMAP account instead of Proton Mail Bridge
  --email ADDRESS              the address you send as
  --name NAME                  display name
  --username USER              IMAP username (Bridge shows it under the account's mailbox details)
  --password-stdin             read the password from the first line of stdin
  --password-command CMD       run CMD (sh -c) for the password at each sync
  --host HOST --port PORT      default 127.0.0.1:1143 for Bridge, PORT 993 for IMAP
  --security starttls|tls|plain
  --no-check                   write the file without logging in first";

/// Run `fell account ...`. Returns the process exit code.
pub fn account_command(args: &[String]) -> i32 {
    let result = match args.first().map(String::as_str) {
        Some("setup") => setup(&args[1..]),
        Some("show") => show(),
        Some("check") => check(),
        Some("--help" | "-h" | "help") | None => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(format!("unknown account command {other:?}\n\n{USAGE}")),
    };
    match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("fell: {err}");
            1
        }
    }
}

#[derive(Debug, Default)]
struct Flags {
    imap: bool,
    email: Option<String>,
    name: Option<String>,
    username: Option<String>,
    password_stdin: bool,
    password_command: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    security: Option<ImapSecurity>,
    no_check: bool,
}

fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--imap" => flags.imap = true,
            "--email" => flags.email = Some(value("--email")?),
            "--name" => flags.name = Some(value("--name")?),
            "--username" => flags.username = Some(value("--username")?),
            "--password-stdin" => flags.password_stdin = true,
            "--password-command" => flags.password_command = Some(value("--password-command")?),
            "--host" => flags.host = Some(value("--host")?),
            "--port" => {
                let raw = value("--port")?;
                flags.port = Some(raw.parse().map_err(|_| format!("bad port {raw:?}"))?);
            }
            "--security" => {
                flags.security = Some(match value("--security")?.as_str() {
                    "starttls" => ImapSecurity::StartTls,
                    "tls" => ImapSecurity::Tls,
                    "plain" => ImapSecurity::Plain,
                    other => return Err(format!("bad --security {other:?}")),
                });
            }
            "--no-check" => flags.no_check = true,
            "--help" | "-h" => return Err(USAGE.into()),
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
    }
    if flags.password_stdin && flags.password_command.is_some() {
        return Err("use --password-stdin or --password-command, not both".into());
    }
    Ok(flags)
}

fn setup(args: &[String]) -> Result<(), String> {
    let flags = parse_flags(args)?;
    let tty = io::stdin().is_terminal() && !flags.password_stdin;
    let backend = if flags.imap {
        BackendKind::Imap
    } else {
        BackendKind::Bridge
    };
    if tty && backend == BackendKind::Bridge {
        println!(
            "Proton Mail Bridge must be running and signed in. In Bridge, open your account\n\
             and copy the IMAP username and password it shows (not your Proton password)."
        );
    }

    let email = match flags.email {
        Some(email) => email,
        None if tty => prompt("Email address", None)?,
        None => return Err("--email is required".into()),
    };
    let name = match flags.name {
        Some(name) => name,
        None if tty => prompt("Display name (optional)", Some(""))?,
        None => String::new(),
    };
    let username = match flags.username {
        Some(username) => username,
        None if tty => prompt("IMAP username", Some(&email))?,
        None => email.clone(),
    };
    let host = match (flags.host, backend) {
        (Some(host), _) => host,
        (None, BackendKind::Bridge) => "127.0.0.1".into(),
        (None, _) if tty => prompt("IMAP host", None)?,
        (None, _) => return Err("--host is required with --imap".into()),
    };
    let password = if let Some(command) = flags.password_command {
        PasswordSource::Command(command)
    } else if flags.password_stdin {
        let mut line = String::new();
        io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|err| format!("read password: {err}"))?;
        PasswordSource::Inline(line.trim_end_matches(['\r', '\n']).to_string())
    } else if tty {
        PasswordSource::Inline(prompt_hidden(if backend == BackendKind::Bridge {
            "Bridge IMAP password"
        } else {
            "IMAP password"
        })?)
    } else {
        return Err("--password-stdin or --password-command is required without a terminal".into());
    };

    let mut config = AccountConfig::bridge(email, username, password);
    config.name = name;
    config.backend = backend;
    config.host = host;
    if backend == BackendKind::Imap {
        config.security = ImapSecurity::Tls;
        config.port = 993;
    }
    if let Some(security) = flags.security {
        config.security = security;
        if backend == BackendKind::Imap && flags.port.is_none() {
            config.port = if security == ImapSecurity::Tls {
                993
            } else {
                143
            };
        }
    }
    if let Some(port) = flags.port {
        config.port = port;
    }
    // Same rules the app applies when it reads the file back.
    let config = AccountConfig::parse(&config.render()).map_err(|err| err.to_string())?;

    if !flags.no_check {
        print!("Logging in to {}:{} … ", config.host, config.port);
        let _ = io::stdout().flush();
        match config.check_login() {
            Ok(count) => println!("ok, {count} folders"),
            Err(err) => {
                println!("failed");
                return Err(explain(&config, &err));
            }
        }
    }
    let path = write_account_config(&config)?;
    println!("Saved {} (mode 600). Open Fell to sync.", path.display());
    Ok(())
}

fn show() -> Result<(), String> {
    let (path, config) = load_account_file()?;
    println!("# {}", path.display());
    let mut redacted = config.clone();
    if let PasswordSource::Inline(_) = redacted.password {
        redacted.password = PasswordSource::Inline("<hidden>".into());
    }
    print!("{}", redacted.render());
    Ok(())
}

fn check() -> Result<(), String> {
    let (_, config) = load_account_file()?;
    match config.check_login() {
        Ok(count) => {
            println!("ok: logged in as {}, {count} folders", config.username);
            Ok(())
        }
        Err(err) => Err(explain(&config, &err)),
    }
}

fn explain(config: &AccountConfig, err: &fell_core::Error) -> String {
    let bridge = config.backend == BackendKind::Bridge;
    let hint = match classify_sync_error(err) {
        fell_core::SyncFailureKind::Unreachable if bridge => {
            "Nothing is listening there. Start Proton Mail Bridge and sign in, then try again."
        }
        fell_core::SyncFailureKind::Unreachable => {
            "The server did not answer on that host and port."
        }
        fell_core::SyncFailureKind::LoginRejected if bridge => {
            "Bridge refused the login. Use the IMAP username and password Bridge shows for this \
             account, not your Proton password."
        }
        fell_core::SyncFailureKind::LoginRejected => "The server refused the username or password.",
        fell_core::SyncFailureKind::Password => "The password command did not print a password.",
        fell_core::SyncFailureKind::Other => "",
    };
    if hint.is_empty() {
        err.to_string()
    } else {
        format!("{err}\n{hint}")
    }
}

fn prompt(label: &str, default: Option<&str>) -> Result<String, String> {
    loop {
        match default {
            Some(value) if !value.is_empty() => print!("{label} [{value}]: "),
            _ => print!("{label}: "),
        }
        let _ = io::stdout().flush();
        let mut line = String::new();
        let read = io::stdin()
            .lock()
            .read_line(&mut line)
            .map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("input ended".into());
        }
        let line = line.trim();
        if !line.is_empty() {
            return Ok(line.to_string());
        }
        if let Some(value) = default {
            return Ok(value.to_string());
        }
    }
}

/// Read one line from the terminal with echo off.
///
/// Line editing and keyboard signals are off while it reads, so Ctrl-C
/// cancels the prompt here at once instead of killing the process with echo
/// still off. The terminal settings come back when the guard drops, on every
/// return path.
fn prompt_hidden(label: &str) -> Result<String, String> {
    print!("{label}: ");
    let _ = io::stdout().flush();
    let read = {
        let _quiet = QuietTerminal::new().map_err(|err| format!("could not hide input: {err}"))?;
        read_hidden(&mut io::stdin().lock())
    };
    println!();
    let password = read?;
    if password.is_empty() {
        return Err("no password entered".into());
    }
    Ok(password)
}

/// One line from raw terminal bytes: Enter ends it, Ctrl-C or Ctrl-D
/// cancels, Backspace removes the last character.
fn read_hidden(input: &mut impl io::Read) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Err("input ended".into()),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err.to_string()),
        }
        match byte[0] {
            b'\n' | b'\r' => break,
            0x03 | 0x04 => return Err("cancelled".into()),
            0x7f | 0x08 => {
                // Drop one whole UTF-8 character.
                while let Some(last) = bytes.pop() {
                    if last & 0xC0 != 0x80 {
                        break;
                    }
                }
            }
            other => bytes.push(other),
        }
    }
    String::from_utf8(bytes).map_err(|_| "the password is not valid UTF-8".into())
}

/// Echo and keyboard signals off on stdin until dropped.
struct QuietTerminal {
    saved: libc::termios,
}

impl QuietTerminal {
    fn new() -> io::Result<Self> {
        // SAFETY: tcgetattr and tcsetattr only read and write the termios
        // struct passed by pointer, which lives on this stack frame.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut saved) != 0 {
                return Err(io::Error::last_os_error());
            }
            let mut quiet = saved;
            quiet.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG);
            quiet.c_cc[libc::VMIN] = 1;
            quiet.c_cc[libc::VTIME] = 0;
            if libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &quiet) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { saved })
        }
    }
}

impl Drop for QuietTerminal {
    fn drop(&mut self) {
        // SAFETY: restores the settings read in `new` on the same descriptor.
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &self.saved);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn hidden_input_edits_and_cancels() {
        let read = |bytes: &[u8]| read_hidden(&mut &bytes[..]);
        assert_eq!(read(b"hunter2\n").unwrap(), "hunter2");
        assert_eq!(read(b"huntx\x7fer2\r").unwrap(), "hunter2");
        assert_eq!(read("pä\x7fass\n".as_bytes()).unwrap(), "pass");
        assert_eq!(read(b"hun\x03ter\n").unwrap_err(), "cancelled");
        assert_eq!(read(b"\x04").unwrap_err(), "cancelled");
        assert_eq!(read(b"no newline").unwrap_err(), "input ended");
    }

    #[test]
    fn flags_parse_and_conflict() {
        let flags = parse_flags(&args(&[
            "--imap",
            "--email",
            "a@b",
            "--host",
            "imap.b",
            "--security",
            "starttls",
            "--password-command",
            "pass x",
        ]))
        .unwrap();
        assert!(flags.imap);
        assert_eq!(flags.security, Some(ImapSecurity::StartTls));
        assert_eq!(flags.password_command.as_deref(), Some("pass x"));
        assert!(
            parse_flags(&args(&["--password-stdin", "--password-command", "x"]))
                .unwrap_err()
                .contains("not both")
        );
        assert!(parse_flags(&args(&["--port", "nope"])).is_err());
        assert!(parse_flags(&args(&["--bogus"])).is_err());
    }
}
