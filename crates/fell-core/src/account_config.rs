//! One configured mail account, read from a small `key = value` file.
//!
//! The app keeps this at `~/.config/fell/account.toml`, mode `0600`. Only
//! flat keys are understood. An unknown key is an error so a typo does not
//! silently fall back to a default.
//!
//! ```toml
//! backend = "bridge"            # "bridge" or "imap"
//! email = "me@proton.me"
//! name = "Me"
//! username = "me@proton.me"     # Bridge shows this in its mailbox settings
//! password = "bridge-password"  # or: password_command = "pass show fell/bridge"
//! # host = "127.0.0.1"
//! # port = 1143
//! # security = "starttls"       # "starttls", "tls", or "plain"
//! ```

use std::fmt;
use std::process::Command;

use crate::account::{Account, AccountId, BackendKind};
use crate::backend::{BridgeBackend, ImapBackend, MailBackend};
use crate::imap::{ImapConfig, ImapSecurity};
use crate::{Error, Result};

/// Ids that [`crate::backend_for`] maps to the in-process demo backend.
const RESERVED_IDS: [&str; 3] = ["local", "fixture", "demo"];

/// Where the IMAP password comes from.
#[derive(Clone, PartialEq, Eq)]
pub enum PasswordSource {
    Inline(String),
    /// Run with `sh -c`; the first line of stdout is the password.
    Command(String),
}

impl fmt::Debug for PasswordSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline(_) => f.write_str("Inline(<redacted>)"),
            Self::Command(command) => f.debug_tuple("Command").field(command).finish(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountConfig {
    pub id: String,
    pub name: String,
    pub email: String,
    pub backend: BackendKind,
    pub host: String,
    pub port: u16,
    pub security: ImapSecurity,
    pub username: String,
    pub password: PasswordSource,
}

impl AccountConfig {
    /// A Bridge account on this machine with Bridge's usual local IMAP port.
    pub fn bridge(
        email: impl Into<String>,
        username: impl Into<String>,
        password: PasswordSource,
    ) -> Self {
        let email = email.into();
        Self {
            id: "mail".into(),
            name: String::new(),
            email,
            backend: BackendKind::Bridge,
            host: "127.0.0.1".into(),
            port: 1143,
            security: ImapSecurity::StartTls,
            username: username.into(),
            password,
        }
    }

    pub fn parse(text: &str) -> Result<Self> {
        let mut fields: Vec<(String, Value)> = Vec::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let lineno = index + 1;
            let Some((key, value)) = line.split_once('=') else {
                return Err(invalid(format!("line {lineno}: expected key = value")));
            };
            let key = key.trim();
            let value = parse_value(value.trim())
                .map_err(|err| invalid(format!("line {lineno}: {err}")))?;
            if fields.iter().any(|(seen, _)| seen == key) {
                return Err(invalid(format!("line {lineno}: {key} is set twice")));
            }
            fields.push((key.to_string(), value));
        }
        let mut get = |key: &str| -> Option<Value> {
            let index = fields.iter().position(|(seen, _)| seen == key)?;
            Some(fields.remove(index).1)
        };

        let backend = match get("backend") {
            None => BackendKind::Bridge,
            Some(value) => match value.text("backend")?.as_str() {
                "bridge" => BackendKind::Bridge,
                "imap" => BackendKind::Imap,
                other => {
                    return Err(invalid(format!(
                        "backend must be \"bridge\" or \"imap\", not {other:?}"
                    )));
                }
            },
        };
        let email = required(get("email"), "email")?;
        let username = match get("username") {
            Some(value) => value.text("username")?,
            None => email.clone(),
        };
        let name = get("name")
            .map(|v| v.text("name"))
            .transpose()?
            .unwrap_or_default();
        let id = get("id")
            .map(|v| v.text("id"))
            .transpose()?
            .unwrap_or_else(|| "mail".into());
        let (default_host, default_port, default_security) = match backend {
            BackendKind::Bridge => ("127.0.0.1", 1143, ImapSecurity::StartTls),
            _ => ("", 993, ImapSecurity::Tls),
        };
        let host = get("host")
            .map(|v| v.text("host"))
            .transpose()?
            .unwrap_or_else(|| default_host.to_string());
        let security = match get("security") {
            None => default_security,
            Some(value) => match value.text("security")?.as_str() {
                "starttls" => ImapSecurity::StartTls,
                "tls" => ImapSecurity::Tls,
                "plain" => ImapSecurity::Plain,
                other => {
                    return Err(invalid(format!(
                        "security must be \"starttls\", \"tls\", or \"plain\", not {other:?}"
                    )));
                }
            },
        };
        let port = match get("port") {
            Some(value) => value.port()?,
            None if backend == BackendKind::Bridge => default_port,
            None if security == ImapSecurity::Tls => 993,
            None => 143,
        };
        let password = match (get("password"), get("password_command")) {
            (Some(_), Some(_)) => {
                return Err(invalid("set password or password_command, not both"));
            }
            (Some(value), None) => PasswordSource::Inline(value.text("password")?),
            (None, Some(value)) => PasswordSource::Command(value.text("password_command")?),
            (None, None) => return Err(invalid("password or password_command is required")),
        };
        if let Some((key, _)) = fields.first() {
            return Err(invalid(format!("unknown key {key}")));
        }

        let config = Self {
            id,
            name,
            email,
            backend,
            host,
            port,
            security,
            username,
            password,
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.id.is_empty() || RESERVED_IDS.contains(&self.id.as_str()) {
            return Err(invalid(format!(
                "id {:?} is reserved for the demo mailbox",
                self.id
            )));
        }
        if self.email.is_empty() {
            return Err(invalid("email is empty"));
        }
        if self.username.is_empty() {
            return Err(invalid("username is empty"));
        }
        if self.host.is_empty() {
            return Err(invalid("host is required for an imap account"));
        }
        if self.security == ImapSecurity::Plain && !self.is_loopback() {
            return Err(invalid(
                "security = \"plain\" is only allowed for a loopback address like 127.0.0.1",
            ));
        }
        match &self.password {
            PasswordSource::Inline(password) if password.is_empty() => {
                Err(invalid("password is empty"))
            }
            PasswordSource::Command(command) if command.trim().is_empty() => {
                Err(invalid("password_command is empty"))
            }
            _ => Ok(()),
        }
    }

    /// The file text for this config. The inline password, if any, is included.
    pub fn render(&self) -> String {
        let mut out = String::from("# Fell mail account. Keep this file private (mode 0600).\n");
        let backend = match self.backend {
            BackendKind::Imap => "imap",
            _ => "bridge",
        };
        let security = match self.security {
            ImapSecurity::StartTls => "starttls",
            ImapSecurity::Tls => "tls",
            ImapSecurity::Plain => "plain",
        };
        push(&mut out, "backend", backend);
        if self.id != "mail" {
            push(&mut out, "id", &self.id);
        }
        push(&mut out, "email", &self.email);
        if !self.name.is_empty() {
            push(&mut out, "name", &self.name);
        }
        push(&mut out, "username", &self.username);
        match &self.password {
            PasswordSource::Inline(password) => push(&mut out, "password", password),
            PasswordSource::Command(command) => push(&mut out, "password_command", command),
        }
        push(&mut out, "host", &self.host);
        out.push_str(&format!("port = {}\n", self.port));
        push(&mut out, "security", security);
        out
    }

    pub fn account(&self) -> Account {
        Account {
            id: AccountId(self.id.clone()),
            display_name: if self.name.is_empty() {
                self.email.clone()
            } else {
                self.name.clone()
            },
            email: self.email.clone(),
            backend_kind: self.backend,
        }
    }

    /// The host is a literal loopback address. A name such as `localhost`
    /// does not count: it is resolved at connect time and may reach another
    /// machine, which must never get plaintext or an unchecked certificate.
    pub fn is_loopback(&self) -> bool {
        self.host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
    }

    /// Resolve the password (running `password_command` if set) and build the backend.
    pub fn backend(&self) -> Result<Box<dyn MailBackend + Send>> {
        let imap = self.imap_config()?;
        let account_id = AccountId(self.id.clone());
        Ok(match self.backend {
            BackendKind::Imap => Box::new(ImapBackend {
                account_id,
                config: imap,
            }),
            _ => Box::new(BridgeBackend {
                account_id,
                imap,
                ..BridgeBackend::default()
            }),
        })
    }

    /// Log in and log out again without reading mail. Returns the number of
    /// selectable mailboxes the server listed.
    pub fn check_login(&self) -> Result<usize> {
        crate::imap::check_login(&self.imap_config()?)
    }

    fn imap_config(&self) -> Result<ImapConfig> {
        let password = self.resolve_password()?;
        Ok(ImapConfig {
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            password,
            security: self.security,
            // Bridge signs its local listener with its own certificate. Any
            // other host must present a certificate a public CA vouches for.
            accept_invalid_certs: self.backend == BackendKind::Bridge && self.is_loopback(),
        })
    }

    fn resolve_password(&self) -> Result<String> {
        match &self.password {
            PasswordSource::Inline(password) => Ok(password.clone()),
            PasswordSource::Command(command) => {
                let output = Command::new("sh")
                    .arg("-c")
                    .arg(command)
                    .output()
                    .map_err(|err| Error::Backend(format!("password_command: {err}")))?;
                if !output.status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let tail: String = stderr
                        .lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .take(160)
                        .collect();
                    return Err(Error::Backend(if tail.is_empty() {
                        format!("password_command exited with {}", output.status)
                    } else {
                        format!("password_command exited with {}: {tail}", output.status)
                    }));
                }
                let text = String::from_utf8(output.stdout)
                    .map_err(|_| Error::Backend("password_command printed non-UTF-8".into()))?;
                let password = text.lines().next().unwrap_or("").to_string();
                if password.is_empty() {
                    return Err(Error::Backend("password_command printed nothing".into()));
                }
                Ok(password)
            }
        }
    }
}

/// Why a sync attempt failed, in the terms the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFailureKind {
    /// Nothing answered on the IMAP port (Bridge not running, wrong port, no network).
    Unreachable,
    /// The server refused the username or password.
    LoginRejected,
    /// `password_command` failed.
    Password,
    Other,
}

pub fn classify_sync_error(err: &Error) -> SyncFailureKind {
    let Error::Backend(message) = err else {
        return SyncFailureKind::Other;
    };
    let upper = message.to_ascii_uppercase();
    if message.starts_with("password_command") {
        SyncFailureKind::Password
    } else if upper.contains("AUTHENTICATIONFAILED")
        || ((message.contains("LOGIN failed: NO") || message.contains("AUTHENTICATE failed: NO"))
            && !upper.contains("[UNAVAILABLE]")
            && !upper.contains("[SERVERBUG]"))
    {
        // A NO to LOGIN without a response code is a refused credential on
        // the servers we know; UNAVAILABLE or SERVERBUG is the server's fault.
        SyncFailureKind::LoginRejected
    } else if message.starts_with("imap: connect ") || message.starts_with("imap: resolve ") {
        SyncFailureKind::Unreachable
    } else {
        SyncFailureKind::Other
    }
}

enum Value {
    Text(String),
    Number(u64),
}

impl Value {
    fn text(self, key: &str) -> Result<String> {
        match self {
            Self::Text(text) => Ok(text),
            Self::Number(_) => Err(invalid(format!("{key} must be a quoted string"))),
        }
    }

    fn port(self) -> Result<u16> {
        match self {
            Self::Number(number) => u16::try_from(number)
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| invalid(format!("port {number} is out of range"))),
            Self::Text(_) => Err(invalid("port must be a number")),
        }
    }
}

fn parse_value(raw: &str) -> std::result::Result<Value, String> {
    if let Some(rest) = raw.strip_prefix('"') {
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(ch) = chars.next() {
            match ch {
                '"' => {
                    let tail = chars.as_str().trim();
                    if !tail.is_empty() && !tail.starts_with('#') {
                        return Err("unexpected text after the closing quote".into());
                    }
                    return Ok(Value::Text(out));
                }
                '\\' => match chars.next() {
                    Some('"') => out.push('"'),
                    Some('\\') => out.push('\\'),
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some(_) => return Err("unsupported escape (use \\\\ \\\" \\n or \\t)".into()),
                    None => return Err("unterminated string".into()),
                },
                other => out.push(other),
            }
        }
        return Err("unterminated string".into());
    }
    if let Some(rest) = raw.strip_prefix('\'') {
        let Some(end) = rest.find('\'') else {
            return Err("unterminated string".into());
        };
        let tail = rest[end + 1..].trim();
        if !tail.is_empty() && !tail.starts_with('#') {
            return Err("unexpected text after the closing quote".into());
        }
        return Ok(Value::Text(rest[..end].to_string()));
    }
    let bare = raw.split('#').next().unwrap_or("").trim();
    bare.parse::<u64>()
        .map(Value::Number)
        // Never repeat the value: it may be a password typed without quotes.
        .map_err(|_| "the value must be a quoted string or a number".into())
}

fn push(out: &mut String, key: &str, value: &str) {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    out.push_str(&format!("{key} = \"{escaped}\"\n"));
}

fn required(value: Option<Value>, key: &str) -> Result<String> {
    value
        .ok_or_else(|| invalid(format!("{key} is required")))?
        .text(key)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::Invalid(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_defaults() {
        let config =
            AccountConfig::parse("email = \"me@proton.me\"\npassword = \"p\\\"w\"\n").unwrap();
        assert_eq!(config.backend, BackendKind::Bridge);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 1143);
        assert_eq!(config.security, ImapSecurity::StartTls);
        assert_eq!(config.username, "me@proton.me");
        assert_eq!(config.password, PasswordSource::Inline("p\"w".into()));
        assert_eq!(config.account().id.as_str(), "mail");
    }

    #[test]
    fn render_round_trips() {
        let mut config = AccountConfig::bridge(
            "me@proton.me",
            "me@proton.me",
            PasswordSource::Inline("a \\ \"b\" #c".into()),
        );
        config.name = "Me".into();
        let parsed = AccountConfig::parse(&config.render()).unwrap();
        assert_eq!(parsed, config);

        config.backend = BackendKind::Imap;
        config.host = "imap.fastmail.com".into();
        config.port = 993;
        config.security = ImapSecurity::Tls;
        config.password = PasswordSource::Command("pass show mail".into());
        assert_eq!(AccountConfig::parse(&config.render()).unwrap(), config);
    }

    #[test]
    fn imap_needs_a_host_and_defaults_to_tls() {
        let err = AccountConfig::parse("backend = \"imap\"\nemail = \"a@b\"\npassword = \"x\"")
            .unwrap_err();
        assert!(err.to_string().contains("host"), "{err}");
        let config = AccountConfig::parse(
            "backend = \"imap\"\nemail = \"a@b\"\npassword = \"x\"\nhost = \"imap.b\"",
        )
        .unwrap();
        assert_eq!((config.port, config.security), (993, ImapSecurity::Tls));
    }

    #[test]
    fn rejects_typos_reserved_ids_and_remote_plaintext() {
        for (text, needle) in [
            (
                "email = \"a@b\"\npasword = \"x\"",
                "password or password_command",
            ),
            (
                "email = \"a@b\"\npassword = \"x\"\nprot = 1",
                "unknown key prot",
            ),
            (
                "email = \"a@b\"\npassword = \"x\"\nid = \"demo\"",
                "reserved",
            ),
            (
                "email = \"a@b\"\npassword = \"x\"\nport = 70000",
                "out of range",
            ),
            (
                "email = \"a@b\"\npassword = \"x\"\nemail = \"c@d\"",
                "twice",
            ),
            (
                "email = \"a@b\"\npassword = \"x\"\npassword_command = \"y\"",
                "not both",
            ),
            (
                "backend = \"imap\"\nemail = \"a@b\"\npassword = \"x\"\nhost = \"imap.b\"\nsecurity = \"plain\"",
                "only allowed",
            ),
        ] {
            let err = AccountConfig::parse(text).unwrap_err().to_string();
            assert!(err.contains(needle), "{text:?}: {err}");
        }
    }

    #[test]
    fn only_bridge_on_a_loopback_address_skips_certificate_checks() {
        let local = AccountConfig::bridge("a@b", "a@b", PasswordSource::Inline("x".into()));
        assert!(local.imap_config().unwrap().accept_invalid_certs);
        for host in ["::1", "[::1]", "127.0.0.2"] {
            let mut other = local.clone();
            other.host = host.into();
            assert!(other.imap_config().unwrap().accept_invalid_certs, "{host}");
        }
        for host in [
            "localhost",
            "mail.example.com",
            "127.1",
            "2130706433",
            "10.0.0.1",
        ] {
            let mut other = local.clone();
            other.host = host.into();
            assert!(!other.imap_config().unwrap().accept_invalid_certs, "{host}");
        }
        let mut imap = local.clone();
        imap.backend = BackendKind::Imap;
        assert!(!imap.imap_config().unwrap().accept_invalid_certs);
    }

    #[test]
    fn plaintext_needs_a_literal_loopback_address() {
        let text = |host: &str| {
            format!("email = \"a@b\"\npassword = \"x\"\nhost = \"{host}\"\nsecurity = \"plain\"\n")
        };
        assert!(AccountConfig::parse(&text("127.0.0.1")).is_ok());
        assert!(AccountConfig::parse(&text("::1")).is_ok());
        for host in ["localhost", "LOCALHOST", "mail.example.com", "127.1"] {
            assert!(AccountConfig::parse(&text(host)).is_err(), "{host}");
        }
    }

    #[test]
    fn parse_errors_never_repeat_a_value() {
        for text in [
            "email = \"a@b\"\npassword = hunter2\n",
            "email = \"a@b\"\npassword = \"sec\"ret\"\n",
            "email = \"a@b\"\npassword = 'sec'ret'\n",
            "email = \"a@b\"\npassword = \"sec\\qret\"\n",
            "email = \"a@b\"\npassword = \"hunter2\n",
        ] {
            let err = AccountConfig::parse(text).unwrap_err().to_string();
            for secret in ["hunter2", "ret", "qret"] {
                assert!(!err.contains(secret), "{text:?} -> {err}");
            }
        }
    }

    #[test]
    fn password_command_first_line() {
        let mut config = AccountConfig::bridge(
            "a@b",
            "a@b",
            PasswordSource::Command("printf 'pw\\nrest'".into()),
        );
        assert_eq!(config.resolve_password().unwrap(), "pw");
        config.password =
            PasswordSource::Command("echo 'gpg: decryption failed' >&2; exit 3".into());
        let err = config.resolve_password().unwrap_err();
        assert_eq!(classify_sync_error(&err), SyncFailureKind::Password);
        assert!(err.to_string().contains("gpg: decryption failed"), "{err}");
    }

    #[test]
    fn classifies_imap_errors() {
        let cases = [
            (
                "imap: connect 127.0.0.1:1143 failed: Connection refused",
                SyncFailureKind::Unreachable,
            ),
            (
                "imap: LOGIN failed: NO [AUTHENTICATIONFAILED] bad",
                SyncFailureKind::LoginRejected,
            ),
            (
                "imap: tls handshake setup failed: x",
                SyncFailureKind::Other,
            ),
            (
                "imap: resolve nope:993 failed: x",
                SyncFailureKind::Unreachable,
            ),
            (
                "imap: LOGIN failed: NO [UNAVAILABLE] later",
                SyncFailureKind::Other,
            ),
            (
                "imap: LOGIN failed: NO bad password",
                SyncFailureKind::LoginRejected,
            ),
        ];
        for (message, kind) in cases {
            assert_eq!(classify_sync_error(&Error::Backend(message.into())), kind);
        }
    }

    #[test]
    fn debug_hides_inline_password() {
        let config = AccountConfig::bridge("a@b", "a@b", PasswordSource::Inline("hunter2".into()));
        assert!(!format!("{config:?}").contains("hunter2"));
    }
}
