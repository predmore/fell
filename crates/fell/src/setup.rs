//! `fell account setup|show|check`: write and test `~/.config/fell/account.toml`.
//!
//! Setup asks for anything not given as a flag, checks the login against the
//! server, and only then saves the credentials and writes the file (mode 0600).
//! It reads no mail.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::PathBuf;

use fell_core::credentials::{PasswordStore, SecretService};
use fell_core::onboarding::{ConnectionProgress, check_connection, connection_recovery};
use fell_core::{AccountConfig, BackendKind, ImapSecurity, PasswordSource};

use crate::mailbox::{StagedAccountConfig, load_account_file, stage_account_config};

const USAGE: &str = "\
fell account setup [options]   write ~/.config/fell/account.toml after a login check
fell account show              print the account file without the password
fell account check             log in to IMAP and SMTP with the account file, send nothing

setup options (anything missing is asked for on a terminal):
  Entered passwords are saved in the desktop keyring by default.
  --imap                       a plain IMAP account instead of Proton Mail Bridge
  --email ADDRESS              the address you send as
  --name NAME                  display name
  --username USER              IMAP username (Bridge shows it under the account's mailbox details)
  --password-stdin             read the password from the first line of stdin
  --password-inline            store the entered password in account.toml
  --password-command CMD       run CMD (sh -c) for the password at each sync
  --host HOST --port PORT      default 127.0.0.1:1143 for Bridge, PORT 993 for IMAP
  --security starttls|tls|plain
  --smtp-host HOST --smtp-port PORT
                               where mail goes; default the IMAP host, and
                               1025 for Bridge or 465/587 for IMAP
  --smtp-security starttls|tls|plain
                               default tls on port 465, otherwise starttls
  --no-check                   write the file without logging in first";

/// Run `fell account ...`. Returns the process exit code.
pub fn account_command(args: &[String]) -> i32 {
    let result = match args.first().map(String::as_str) {
        Some("setup") => setup(&args[1..]),
        Some(cmd @ ("show" | "check")) => match args.get(1).map(String::as_str) {
            Some("--help" | "-h" | "help") => {
                println!("{USAGE}");
                Ok(())
            }
            Some(other) => Err(format!("unknown option {other:?}\n\n{USAGE}")),
            None if cmd == "show" => show(),
            None => check(),
        },
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
            // Usage errors end with the usage text; they exit 2 like the
            // top-level parser. Anything else (a failed login) exits 1.
            if err.ends_with(USAGE) { 2 } else { 1 }
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
    password_inline: bool,
    password_command: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    security: Option<ImapSecurity>,
    smtp: SmtpFlags,
    no_check: bool,
    help: bool,
}

#[derive(Debug, Default)]
struct SmtpFlags {
    host: Option<String>,
    port: Option<u16>,
    security: Option<ImapSecurity>,
}

fn parse_flags(args: &[String]) -> Result<Flags, String> {
    let mut flags = Flags::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let mut value = |name: &str| {
            args.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value\n\n{USAGE}"))
        };
        match arg.as_str() {
            "--imap" => flags.imap = true,
            "--email" => flags.email = Some(value("--email")?),
            "--name" => flags.name = Some(value("--name")?),
            "--username" => flags.username = Some(value("--username")?),
            "--password-stdin" => flags.password_stdin = true,
            "--password-inline" => flags.password_inline = true,
            "--password-command" => flags.password_command = Some(value("--password-command")?),
            "--host" => flags.host = Some(value("--host")?),
            "--port" => flags.port = Some(port(&value("--port")?)?),
            "--security" => flags.security = Some(security("--security", &value("--security")?)?),
            "--smtp-host" => flags.smtp.host = Some(value("--smtp-host")?),
            "--smtp-port" => flags.smtp.port = Some(port(&value("--smtp-port")?)?),
            "--smtp-security" => {
                flags.smtp.security = Some(security("--smtp-security", &value("--smtp-security")?)?)
            }
            "--no-check" => flags.no_check = true,
            "--help" | "-h" => flags.help = true,
            other => return Err(format!("unknown option {other:?}\n\n{USAGE}")),
        }
    }
    if flags.password_stdin && flags.password_command.is_some() {
        return Err("use --password-stdin or --password-command, not both".into());
    }
    if flags.password_inline && flags.password_command.is_some() {
        return Err("use --password-inline or --password-command, not both".into());
    }
    Ok(flags)
}

fn port(raw: &str) -> Result<u16, String> {
    match raw.parse() {
        Ok(port) if port > 0 => Ok(port),
        _ => Err(format!("bad port {raw:?}")),
    }
}

fn security(flag: &str, raw: &str) -> Result<ImapSecurity, String> {
    match raw {
        "starttls" => Ok(ImapSecurity::StartTls),
        "tls" => Ok(ImapSecurity::Tls),
        "plain" => Ok(ImapSecurity::Plain),
        other => Err(format!("bad {flag} {other:?} (starttls, tls, or plain)")),
    }
}

/// Where the account sends mail. A flag wins; on a terminal an IMAP
/// account is asked for host and port, defaulting to what
/// [`AccountConfig::smtp_config`] picks. Security follows the port (465 is
/// implicit TLS, anything else STARTTLS) unless `--smtp-security` says
/// otherwise; plaintext is never chosen for you. Only keys that differ from
/// those defaults are stored, so the file stays as short as before.
fn choose_smtp(
    config: &mut AccountConfig,
    flags: &SmtpFlags,
    ask: &mut dyn FnMut(&str, &str) -> Result<String, String>,
) -> Result<(), String> {
    let defaults = config.smtp_config();
    let host = match &flags.host {
        Some(host) => host.clone(),
        None => ask("SMTP host", &defaults.host)?,
    };
    let port_for = |config: &AccountConfig, security| {
        let mut probe = config.clone();
        probe.smtp_security = Some(security);
        probe.smtp_port = None;
        probe.smtp_config().port
    };
    let (port, security) = match (flags.port, flags.security) {
        (Some(port), Some(security)) => (port, security),
        (None, Some(security)) => {
            let default = port_for(config, security).to_string();
            (self::port(&ask("SMTP port", &default)?)?, security)
        }
        (port, None) => {
            let port = match port {
                Some(port) => port,
                None => self::port(&ask("SMTP port", &defaults.port.to_string())?)?,
            };
            let security = if port == defaults.port {
                defaults.security
            } else if port == 465 {
                ImapSecurity::Tls
            } else {
                ImapSecurity::StartTls
            };
            (port, security)
        }
    };
    config.smtp_host = (host != config.host).then_some(host);
    config.smtp_security = (security != defaults.security).then_some(security);
    config.smtp_port = (port != port_for(config, security)).then_some(port);
    Ok(())
}

fn setup(args: &[String]) -> Result<(), String> {
    let flags = parse_flags(args)?;
    if flags.help {
        println!("{USAGE}");
        return Ok(());
    }
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
    // Bridge's SMTP is on this machine at a fixed port; only an IMAP
    // account is asked where it sends.
    let ask_smtp = tty && backend == BackendKind::Imap;
    choose_smtp(&mut config, &flags.smtp, &mut |label, default| {
        if ask_smtp {
            prompt(label, Some(default))
        } else {
            Ok(default.to_string())
        }
    })?;
    config.validate().map_err(|err| err.to_string())?;

    if !flags.no_check {
        check_both(&config).map_err(|err| {
            format!(
                "{err}\nFix the settings and run setup again, or add --no-check to save anyway."
            )
        })?;
    }
    let path = save_account(
        config,
        flags.password_inline,
        &SecretService,
        stage_account_config,
    )
    .map_err(|err| match err {
        SaveAccountError::File(detail) => detail,
        SaveAccountError::Keyring { detail, recovery, .. } => {
            let recovery = recovery.unwrap_or_default();
            format!("{detail}\n{recovery}\nUse --password-inline or --password-command to choose a different password source.")
        }
    })?;
    println!("Saved {} (mode 600). Open Fell to sync.", path.display());
    Ok(())
}

#[derive(Debug)]
pub(crate) enum SaveAccountError {
    File(String),
    Keyring {
        detail: String,
        kind: Option<fell_core::credentials::KeyringError>,
        recovery: Option<&'static str>,
    },
}

pub(crate) fn save_account(
    mut config: AccountConfig,
    password_inline: bool,
    keyring: &dyn PasswordStore,
    stage: impl FnOnce(&AccountConfig) -> Result<StagedAccountConfig, String>,
) -> Result<PathBuf, SaveAccountError> {
    let keyring_password = match (password_inline, config.password) {
        (false, PasswordSource::Inline(password)) => {
            config.password = PasswordSource::Keyring;
            Some(password)
        }
        (_, password) => {
            config.password = password;
            None
        }
    };
    let staged = stage(&config).map_err(SaveAccountError::File)?;
    if let Some(password) = keyring_password {
        config
            .store_keyring_password(keyring, &password)
            .map_err(|err| SaveAccountError::Keyring {
                detail: err.to_string(),
                kind: match err {
                    fell_core::Error::Keyring(kind) => Some(kind),
                    _ => None,
                },
                recovery: connection_recovery(&config, &err),
            })?;
    }
    staged.publish().map_err(SaveAccountError::File)
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
    check_both(&config)
}

/// Log in to IMAP, then SMTP, printing one line for each. SMTP is tried
/// even when IMAP fails, so one run shows both, unless the password itself
/// could not be read.
fn check_both(config: &AccountConfig) -> Result<(), String> {
    let report = check_connection(config, |progress| match progress {
        ConnectionProgress::ImapStarted => {
            print!("Logging in to IMAP at {}:{} … ", config.host, config.port);
            let _ = io::stdout().flush();
        }
        ConnectionProgress::ImapFinished(Ok(count)) => println!("ok, {count} folders"),
        ConnectionProgress::SmtpStarted => {
            let smtp = config.smtp_config();
            print!("Logging in to SMTP at {}:{} … ", smtp.host, smtp.port);
            let _ = io::stdout().flush();
        }
        ConnectionProgress::SmtpFinished(Ok(())) => println!("ok"),
        ConnectionProgress::ImapFinished(Err(_)) | ConnectionProgress::SmtpFinished(Err(_)) => {
            println!("failed");
        }
    })
    .map_err(|err| explain(config, &[&err]))?;
    let failures: Vec<_> = report.errors().collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(explain(config, &failures))
    }
}

/// Each error on its own line, then what to do about them. A hint two
/// failures share is said once.
fn explain(config: &AccountConfig, errors: &[&fell_core::Error]) -> String {
    let mut lines: Vec<String> = errors.iter().map(ToString::to_string).collect();
    for err in errors {
        if let Some(hint) = connection_recovery(config, err)
            && !lines.iter().any(|line| line == hint)
        {
            lines.push(hint.to_string());
        }
    }
    lines.join("\n")
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
    fn show_and_check_take_help_and_refuse_stray_words() {
        // Help and usage errors return before any account file is read or
        // any server is contacted.
        for cmd in ["show", "check"] {
            assert_eq!(account_command(&args(&[cmd, "--help"])), 0, "{cmd}");
            assert_eq!(account_command(&args(&[cmd, "--bogus"])), 2, "{cmd}");
        }
        assert_eq!(account_command(&args(&["--bogus"])), 2);
        assert_eq!(account_command(&args(&["setup", "--bogus"])), 2);
        assert_eq!(account_command(&args(&["setup", "--email"])), 2);
    }

    #[test]
    fn a_shared_hint_is_said_once() {
        let config = AccountConfig::bridge("a@b", "a@b", PasswordSource::Inline("x".into()));
        let err = |text: &str| fell_core::Error::Backend(text.into());
        let text = explain(
            &config,
            &[
                &err("imap: connect 127.0.0.1:1143 failed: refused"),
                &err("smtp: connect 127.0.0.1:1025 failed: refused"),
            ],
        );
        assert_eq!(text.lines().count(), 3, "{text}");
        assert!(text.ends_with("then try again."), "{text}");
        let text = explain(
            &config,
            &[&fell_core::Error::Tls(
                "smtp: the server does not offer STARTTLS and this account requires TLS".into(),
            )],
        );
        assert_eq!(text.lines().count(), 2, "{text}");
        assert!(text.contains("Check the server host, port, TLS mode, and certificate"));
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
        assert!(parse_flags(&args(&["--password-inline", "--password-command", "x"])).is_err());
        let inline = parse_flags(&args(&["--password-stdin", "--password-inline"])).unwrap();
        assert!(inline.password_inline && inline.password_stdin);
        assert!(parse_flags(&args(&["--port", "nope"])).is_err());
        assert!(parse_flags(&args(&["--smtp-port", "0"])).is_err());
        assert!(parse_flags(&args(&["--smtp-security", "none"])).is_err());
        assert!(parse_flags(&args(&["--bogus"])).is_err());
        assert!(parse_flags(&args(&["--imap", "-h"])).unwrap().help);
        let smtp = parse_flags(&args(&[
            "--smtp-host",
            "smtp.b",
            "--smtp-port",
            "2525",
            "--smtp-security",
            "tls",
        ]))
        .unwrap();
        assert_eq!(
            (
                smtp.smtp.host.as_deref(),
                smtp.smtp.port,
                smtp.smtp.security
            ),
            (Some("smtp.b"), Some(2525), Some(ImapSecurity::Tls))
        );
    }

    #[test]
    fn setup_publishes_keyring_config_only_after_saving_the_password() {
        use std::cell::Cell;

        use fell_core::credentials::KeyringError;

        struct FakeKeyring<'a> {
            path: &'a std::path::Path,
            fail: Cell<bool>,
            writes: Cell<usize>,
        }

        impl PasswordStore for FakeKeyring<'_> {
            fn read(&self, _: &str) -> Result<String, KeyringError> {
                panic!("setup does not need to read a saved password")
            }

            fn store(&self, _: &str, password: &str) -> Result<(), KeyringError> {
                assert!(
                    std::fs::read_to_string(self.path)
                        .unwrap()
                        .contains("password = ")
                );
                assert_eq!(password, "sample-password");
                self.writes.set(self.writes.get() + 1);
                if self.fail.get() {
                    Err(KeyringError::Locked)
                } else {
                    Ok(())
                }
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("account.toml");
        let stage = |config: &AccountConfig| {
            crate::mailbox::stage_account_config_at(path.clone(), config, false)
        };
        let keyring = FakeKeyring {
            path: &path,
            fail: Cell::new(true),
            writes: Cell::new(0),
        };
        let config = AccountConfig::bridge(
            "a@example.test",
            "mailbox-user",
            PasswordSource::Inline("sample-password".into()),
        );
        save_account(config.clone(), true, &keyring, stage).unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        assert_eq!(AccountConfig::parse(&original).unwrap(), config);
        assert_eq!(keyring.writes.get(), 0);

        let err = save_account(config.clone(), false, &keyring, stage).unwrap_err();
        assert!(
            matches!(err, SaveAccountError::Keyring { recovery: Some(hint), .. }
            if hint.contains("Unlock your desktop keyring"))
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

        keyring.fail.set(false);
        save_account(config, false, &keyring, stage).unwrap();
        let published = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            AccountConfig::parse(&published).unwrap().password,
            PasswordSource::Keyring
        );
        assert!(!published.contains("sample-password"));
        assert_eq!(keyring.writes.get(), 2);
    }

    fn imap_account(security: ImapSecurity) -> AccountConfig {
        let mut config = AccountConfig::bridge("a@b", "a@b", PasswordSource::Inline("x".into()));
        config.backend = BackendKind::Imap;
        config.host = "imap.b".into();
        config.security = security;
        config
    }

    /// Run [`choose_smtp`] with these flags and typed answers ("" takes the
    /// default), then read the file back the way the app does.
    fn smtp_after(
        mut config: AccountConfig,
        flag_list: &[&str],
        answers: &[&str],
    ) -> (fell_core::SmtpConfig, AccountConfig, Vec<String>) {
        let flags = parse_flags(&args(flag_list)).unwrap();
        let mut answers = answers.iter();
        let mut asked = Vec::new();
        choose_smtp(&mut config, &flags.smtp, &mut |label, default| {
            asked.push(format!("{label} [{default}]"));
            let answer = answers.next().copied().unwrap_or("");
            Ok(if answer.is_empty() { default } else { answer }.to_string())
        })
        .unwrap();
        let config = AccountConfig::parse(&config.render()).unwrap();
        (config.smtp_config(), config, asked)
    }

    #[test]
    fn smtp_defaults_are_offered_and_not_written() {
        let (smtp, config, asked) = smtp_after(imap_account(ImapSecurity::Tls), &[], &[]);
        assert_eq!(asked, ["SMTP host [imap.b]", "SMTP port [465]"]);
        assert_eq!(
            (smtp.host.as_str(), smtp.port, smtp.security),
            ("imap.b", 465, ImapSecurity::Tls)
        );
        assert_eq!(
            (config.smtp_host, config.smtp_port, config.smtp_security),
            (None, None, None)
        );

        let bridge = AccountConfig::bridge("a@b", "u", PasswordSource::Inline("x".into()));
        let (smtp, config, _) = smtp_after(bridge, &[], &[]);
        assert_eq!(smtp, fell_core::SmtpConfig::bridge_local());
        assert_eq!(config.render().matches("smtp").count(), 0);
    }

    #[test]
    fn smtp_security_follows_the_port_unless_given() {
        // A TLS IMAP account with submission on 587 talks STARTTLS there.
        let (smtp, _, _) = smtp_after(imap_account(ImapSecurity::Tls), &[], &["smtp.b", "587"]);
        assert_eq!(
            (smtp.host.as_str(), smtp.port, smtp.security),
            ("smtp.b", 587, ImapSecurity::StartTls)
        );
        let (smtp, _, _) = smtp_after(
            imap_account(ImapSecurity::StartTls),
            &["--smtp-port", "465"],
            &[],
        );
        assert_eq!((smtp.port, smtp.security), (465, ImapSecurity::Tls));
        // Security alone picks its usual port as the default.
        let (smtp, _, asked) = smtp_after(
            imap_account(ImapSecurity::Tls),
            &["--smtp-security", "starttls"],
            &[],
        );
        assert_eq!(asked[1], "SMTP port [587]");
        assert_eq!((smtp.port, smtp.security), (587, ImapSecurity::StartTls));
        let (smtp, _, asked) = smtp_after(
            imap_account(ImapSecurity::Tls),
            &[
                "--smtp-host",
                "s",
                "--smtp-port",
                "2465",
                "--smtp-security",
                "tls",
            ],
            &[],
        );
        assert!(asked.is_empty());
        assert_eq!(
            (smtp.host.as_str(), smtp.port, smtp.security),
            ("s", 2465, ImapSecurity::Tls)
        );
    }

    #[test]
    fn smtp_plaintext_is_refused_off_loopback() {
        let mut config = imap_account(ImapSecurity::Tls);
        let flags = parse_flags(&args(&["--smtp-security", "plain", "--smtp-port", "25"])).unwrap();
        choose_smtp(&mut config, &flags.smtp, &mut |_, default| {
            Ok(default.into())
        })
        .unwrap();
        let err = AccountConfig::parse(&config.render()).unwrap_err();
        assert!(err.to_string().contains("loopback"), "{err}");
    }
}
