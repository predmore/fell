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
//! password_source = "keyring"   # set by account setup
//! # password = "bridge-password"  # or: password_command = "pass show fell/bridge"
//! # host = "127.0.0.1"
//! # port = 1143
//! # security = "starttls"       # "starttls", "tls", or "plain"
//! # smtp_host = "127.0.0.1"     # defaults to host
//! # smtp_port = 1025            # Bridge's SMTP port; 587 or 465 for imap
//! # smtp_security = "starttls"  # "plain" only for a loopback address
//! # sent_folder = "Sent Items"  # only for a server that does not mark its folders
//! # save_sent = "auto"        # "append" or "server" overrides provider detection
//! # write_back = true           # send moves and flag changes to the server (off by default)
//! # notifications = "on"       # "on", "private", or "off"
//! ```
//!
//! Sync learns which folder is Sent, Drafts, Trash and so on from the
//! server's SPECIAL-USE marks, or from common names when it marks none.
//! `sent_folder`, `drafts_folder`, `trash_folder`, `junk_folder`,
//! `archive_folder`, `all_mail_folder`, and `flagged_folder` name the folder
//! for that role and win over both.
//!
//! After SMTP confirms a send, Fell immediately saves a local Sent copy.
//! `save_sent = "auto"` lets Proton Bridge and Gmail file their own copies
//! and selects `"append"` elsewhere. Set `"server"` to let your provider
//! file the copy, or `"append"` to have Fell file it with IMAP.
//!
//! With `write_back = true`, Fell queues Sent appends, moves, and read and star
//! changes for the sync thread (see [`crate::outbox`]). With write-back off,
//! append mode saves the local Sent copy only.
//!
//! Sending uses only this file's username and password source, never anything in a
//! draft folder. `smtp_security = "starttls"` (the default) refuses a server
//! that does not offer STARTTLS rather than log in over cleartext.

use std::fmt;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::account::{Account, AccountId, BackendKind};
use crate::backend::{BridgeBackend, DEMO_ACCOUNT_IDS, ImapBackend, MailBackend};
use crate::credentials::{KeyringError, PasswordStore, SecretService};
use crate::folders::FolderRole;
use crate::imap::{ImapConfig, ImapSecurity};
use crate::smtp::SmtpConfig;
use crate::{Error, Result};

/// Where the shared IMAP and SMTP password comes from.
#[derive(Clone, PartialEq, Eq)]
pub enum PasswordSource {
    Inline(String),
    /// Run with `sh -c`; the first line of stdout is the password.
    Command(String),
    /// Read from the desktop Secret Service for this account's login settings.
    Keyring,
}

impl fmt::Debug for PasswordSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline(_) => f.write_str("Inline(<redacted>)"),
            Self::Command(command) => f.debug_tuple("Command").field(command).finish(),
            Self::Keyring => f.write_str("Keyring"),
        }
    }
}

/// How a confirmed SMTP send is saved to the server's Sent mailbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SaveSent {
    #[default]
    Auto,
    Append,
    Server,
}

impl SaveSent {
    fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Append => "append",
            Self::Server => "server",
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
    /// SMTP settings left unset use [`AccountConfig::smtp_config`]'s defaults.
    pub smtp_host: Option<String>,
    pub smtp_port: Option<u16>,
    pub smtp_security: Option<ImapSecurity>,
    /// `sent_folder = "..."` and the like, in [`FolderRole::ALL`] order.
    pub folders: Vec<(FolderRole, String)>,
    pub save_sent: SaveSent,
    /// Allow server moves, flag changes and Sent appends.
    pub write_back: bool,
    pub notifications: NotificationMode,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NotificationMode {
    #[default]
    On,
    Private,
    Off,
}

impl NotificationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Private => "private",
            Self::Off => "off",
        }
    }

    fn parse(value: Option<Value>, content: Option<Value>) -> Result<Self> {
        let show_content = content
            .map(|value| value.flag("notification_content"))
            .transpose()?
            .unwrap_or(true);
        match value {
            Some(Value::Flag(false)) => Ok(Self::Off),
            None | Some(Value::Flag(true)) if show_content => Ok(Self::On),
            None | Some(Value::Flag(true)) => Ok(Self::Private),
            Some(value) => match value.text("notifications")?.as_str() {
                "on" => Ok(Self::On),
                "private" => Ok(Self::Private),
                "off" => Ok(Self::Off),
                _ => Err(invalid(
                    "notifications must be \"on\", \"private\", or \"off\"",
                )),
            },
        }
    }
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
            smtp_host: None,
            smtp_port: None,
            smtp_security: None,
            folders: Vec::new(),
            save_sent: SaveSent::Auto,
            write_back: false,
            notifications: NotificationMode::On,
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
            Some(value) => value.security("security")?,
        };
        let port = match get("port") {
            Some(value) => value.port()?,
            None if backend == BackendKind::Bridge => default_port,
            None if security == ImapSecurity::Tls => 993,
            None => 143,
        };
        let password = match (
            get("password"),
            get("password_command"),
            get("password_source"),
        ) {
            (Some(value), None, None) => PasswordSource::Inline(value.text("password")?),
            (None, Some(value), None) => PasswordSource::Command(value.text("password_command")?),
            (Some(_), Some(_), None) => {
                return Err(invalid("set password or password_command, not both"));
            }
            (None, None, Some(value)) => {
                if value.text("password_source")? != "keyring" {
                    return Err(invalid("password_source must be \"keyring\""));
                }
                PasswordSource::Keyring
            }
            (None, None, None) => {
                return Err(invalid(
                    "password or password_command or password_source = \"keyring\" is required",
                ));
            }
            _ => {
                return Err(invalid(
                    "set only one of password, password_command, and password_source",
                ));
            }
        };
        let smtp_host = get("smtp_host").map(|v| v.text("smtp_host")).transpose()?;
        let smtp_port = get("smtp_port").map(Value::port).transpose()?;
        let smtp_security = get("smtp_security")
            .map(|v| v.security("smtp_security"))
            .transpose()?;
        let mut folders = Vec::new();
        for role in FolderRole::ALL {
            let Some(key) = role.config_key() else {
                continue;
            };
            if let Some(value) = get(key) {
                let name = value.text(key)?;
                if name.trim().is_empty() {
                    return Err(invalid(format!("{key} is empty")));
                }
                if name.chars().any(char::is_control) {
                    return Err(invalid(format!("{key} contains a control character")));
                }
                folders.push((role, name.trim().to_string()));
            }
        }
        let save_sent = match get("save_sent")
            .map(|value| value.text("save_sent"))
            .transpose()?
            .as_deref()
        {
            None | Some("auto") => SaveSent::Auto,
            Some("append") => SaveSent::Append,
            Some("server") => SaveSent::Server,
            Some(_) => return Err(invalid("save_sent must be auto, append or server")),
        };
        let write_back = get("write_back")
            .map(|value| value.flag("write_back"))
            .transpose()?
            .unwrap_or(false);
        let notifications =
            NotificationMode::parse(get("notifications"), get("notification_content"))?;
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
            smtp_host,
            smtp_port,
            smtp_security,
            folders,
            save_sent,
            write_back,
            notifications,
        };
        config.validate()?;
        Ok(config)
    }

    /// Check account settings without opening a connection or reading the password source.
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty() || DEMO_ACCOUNT_IDS.contains(&self.id.as_str()) {
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
        if self.port == 0 || self.smtp_port == Some(0) {
            return Err(invalid("port 0 is out of range"));
        }
        if self.security == ImapSecurity::Plain && !self.is_loopback() {
            return Err(invalid(
                "security = \"plain\" is only allowed for a loopback address like 127.0.0.1",
            ));
        }
        if self.smtp_host.as_ref().is_some_and(|host| host.is_empty()) {
            return Err(invalid("smtp_host is empty"));
        }
        let smtp = self.smtp_config();
        if smtp.security == ImapSecurity::Plain && !literal_loopback(&smtp.host) {
            return Err(invalid(
                "smtp_security = \"plain\" is only allowed for a loopback address like 127.0.0.1",
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
        let security = security_name(self.security);
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
            PasswordSource::Keyring => push(&mut out, "password_source", "keyring"),
        }
        push(&mut out, "host", &self.host);
        out.push_str(&format!("port = {}\n", self.port));
        push(&mut out, "security", security);
        if let Some(host) = &self.smtp_host {
            push(&mut out, "smtp_host", host);
        }
        if let Some(port) = self.smtp_port {
            out.push_str(&format!("smtp_port = {port}\n"));
        }
        if let Some(security) = self.smtp_security {
            push(&mut out, "smtp_security", security_name(security));
        }
        for (role, name) in &self.folders {
            if let Some(key) = role.config_key() {
                push(&mut out, key, name);
            }
        }
        if self.save_sent != SaveSent::Auto {
            push(&mut out, "save_sent", self.save_sent.name());
        }
        if self.write_back {
            out.push_str("write_back = true\n");
        }
        if self.notifications != NotificationMode::On {
            push(&mut out, "notifications", self.notifications.as_str());
        }
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
        literal_loopback(&self.host)
    }

    /// Where this account submits mail. Unset keys default to `host`, then
    /// Bridge's STARTTLS port 1025, or 587 (STARTTLS) / 465 (TLS) for imap.
    /// Plaintext is never a default.
    pub fn smtp_config(&self) -> SmtpConfig {
        let host = self.smtp_host.clone().unwrap_or_else(|| self.host.clone());
        let security = self
            .smtp_security
            .unwrap_or(match (self.backend, self.security) {
                (BackendKind::Imap, ImapSecurity::Tls) => ImapSecurity::Tls,
                _ => ImapSecurity::StartTls,
            });
        let port = self.smtp_port.unwrap_or(match (self.backend, security) {
            (BackendKind::Imap, ImapSecurity::Tls) => 465,
            (BackendKind::Imap, _) => 587,
            _ => 1025,
        });
        SmtpConfig {
            // Same rule as IMAP: only Bridge on this machine skips the
            // certificate check.
            accept_invalid_certs: self.backend == BackendKind::Bridge && literal_loopback(&host),
            append_sent: self.append_sent(),
            host,
            port,
            security,
        }
    }

    /// Resolve the password and build the backend. Call off the UI thread.
    pub fn backend(&self) -> Result<Box<dyn MailBackend + Send>> {
        Ok(match self.backend {
            BackendKind::Imap => Box::new(ImapBackend {
                account_id: AccountId(self.id.clone()),
                config: self.imap_config()?,
                smtp: self.smtp_config(),
            }),
            _ => Box::new(self.bridge_backend()?),
        })
    }

    pub(crate) fn bridge_backend(&self) -> Result<BridgeBackend> {
        Ok(BridgeBackend {
            account_id: AccountId(self.id.clone()),
            imap: self.imap_config()?,
            smtp: self.smtp_config(),
        })
    }

    /// Whether a newly sent message should be queued for IMAP APPEND.
    pub fn append_sent(&self) -> bool {
        self.write_back
            && match self.save_sent {
                SaveSent::Append => true,
                SaveSent::Server => false,
                SaveSent::Auto => {
                    let gmail = |host: &str| {
                        matches!(
                            host.trim_end_matches('.').to_ascii_lowercase().as_str(),
                            "imap.gmail.com"
                                | "imap.googlemail.com"
                                | "smtp.gmail.com"
                                | "smtp.googlemail.com"
                        )
                    };
                    self.backend != BackendKind::Bridge
                        && !gmail(&self.host)
                        && !self.smtp_host.as_deref().is_some_and(gmail)
                }
            }
    }

    /// Send the writes queued for this account on a separate
    /// `SELECT` session. Run it on the sync thread; it connects only when a
    /// row is due. Sends nothing while `write_back` is off, but still
    /// reports, once, a copy an earlier build of write-back left half done.
    pub fn replay_outbox(&self, store: &crate::Store) -> Result<crate::ReplayReport> {
        let account_id = AccountId(self.id.clone());
        let append_sent = self.append_sent();
        let report =
            if !self.write_back || !crate::outbox::any_due(store, &account_id, append_sent)? {
                crate::ReplayReport::default()
            } else {
                crate::imap::replay_outbox(&self.imap_config()?, &account_id, store, append_sent)?
            };
        crate::outbox::report_for_status(store, &account_id, report)
    }

    /// Log in and log out again without reading mail. Returns the number of
    /// selectable mailboxes the server listed.
    pub fn check_login(&self) -> Result<usize> {
        crate::imap::check_login(&self.imap_config()?)
    }

    /// Log in to [`AccountConfig::smtp_config`] with the account's username
    /// and password, then quit. Sends `EHLO`, `STARTTLS` when required, and
    /// `AUTH`, never `MAIL FROM`, so no mail can go out. Errors classify
    /// with [`classify_sync_error`] like an IMAP login.
    pub fn check_smtp(&self) -> Result<()> {
        let password = self.resolve_password()?;
        crate::smtp::check_login(&crate::smtp::Endpoint::new(
            &self.smtp_config(),
            &self.username,
            &password,
        ))
    }

    fn imap_config(&self) -> Result<ImapConfig> {
        Ok(self.imap_config_with_password(self.resolve_password()?))
    }

    pub(crate) fn imap_config_with_password(&self, password: String) -> ImapConfig {
        ImapConfig {
            host: self.host.clone(),
            port: self.port,
            username: self.username.clone(),
            password,
            security: self.security,
            // Bridge signs its local listener with its own certificate. Any
            // other host must present a certificate a public CA vouches for.
            accept_invalid_certs: self.backend == BackendKind::Bridge && self.is_loopback(),
            folder_overrides: self.folders.clone(),
        }
    }

    fn resolve_password(&self) -> Result<String> {
        self.resolve_password_with(&SecretService)
    }

    /// Save a password for this keyring-backed account. Call after a successful
    /// connection check with the entered password, before publishing the file.
    pub fn store_keyring_password(&self, store: &dyn PasswordStore, password: &str) -> Result<()> {
        self.validate()?;
        if self.password != PasswordSource::Keyring {
            return Err(invalid("password_source must be \"keyring\""));
        }
        if password.is_empty() {
            return Err(KeyringError::Invalid.into());
        }
        Ok(store.store(&self.credential_key(), password)?)
    }

    /// Bind the credential to both endpoints as well as the local account
    /// identity (the default id `mail` is shared by every account file).
    /// Changing display name or folder settings keeps the same credential.
    fn credential_key(&self) -> String {
        let smtp = self.smtp_config();
        let mut hash = Sha256::new();
        for field in [
            self.id.as_str(),
            self.email.as_str(),
            self.backend.as_str(),
            self.username.as_str(),
            self.host.as_str(),
            security_name(self.security),
            &smtp.host,
            security_name(smtp.security),
        ] {
            hash.update((field.len() as u64).to_be_bytes());
            hash.update(field);
        }
        hash.update(self.port.to_be_bytes());
        hash.update(smtp.port.to_be_bytes());
        format!("{:x}", hash.finalize())
    }

    pub(crate) fn resolve_password_with(&self, store: &dyn PasswordStore) -> Result<String> {
        match &self.password {
            PasswordSource::Inline(password) => Ok(password.clone()),
            PasswordSource::Keyring => Ok(store.read(&self.credential_key())?),
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

/// Why a sync attempt or a login check failed, in the terms the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncFailureKind {
    /// Nothing answered on the IMAP or SMTP port (Bridge not running, wrong
    /// port, no network).
    Unreachable,
    /// The server refused the username or password.
    LoginRejected,
    /// The configured password source failed.
    Password,
    /// The connection could not negotiate TLS.
    Tls,
    Other,
}

pub fn classify_sync_error(err: &Error) -> SyncFailureKind {
    let message = match err {
        Error::SmtpCredentialsRejected(_) => return SyncFailureKind::LoginRejected,
        Error::Keyring(_) => return SyncFailureKind::Password,
        Error::Tls(_) => return SyncFailureKind::Tls,
        Error::Backend(message) => message,
        _ => return SyncFailureKind::Other,
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
    } else if [
        "imap: connect ",
        "imap: resolve ",
        "smtp: connect ",
        "smtp: resolve ",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix))
    {
        SyncFailureKind::Unreachable
    } else {
        SyncFailureKind::Other
    }
}

enum Value {
    Text(String),
    Number(u64),
    Flag(bool),
}

impl Value {
    fn text(self, key: &str) -> Result<String> {
        match self {
            Self::Text(text) => Ok(text),
            Self::Number(_) | Self::Flag(_) => {
                Err(invalid(format!("{key} must be a quoted string")))
            }
        }
    }

    fn flag(self, key: &str) -> Result<bool> {
        match self {
            Self::Flag(flag) => Ok(flag),
            _ => Err(invalid(format!("{key} must be true or false"))),
        }
    }

    fn security(self, key: &str) -> Result<ImapSecurity> {
        match self.text(key)?.as_str() {
            "starttls" => Ok(ImapSecurity::StartTls),
            "tls" => Ok(ImapSecurity::Tls),
            "plain" => Ok(ImapSecurity::Plain),
            other => Err(invalid(format!(
                "{key} must be \"starttls\", \"tls\", or \"plain\", not {other:?}"
            ))),
        }
    }

    fn port(self) -> Result<u16> {
        match self {
            Self::Number(number) => u16::try_from(number)
                .ok()
                .filter(|port| *port != 0)
                .ok_or_else(|| invalid(format!("port {number} is out of range"))),
            Self::Text(_) | Self::Flag(_) => Err(invalid("port must be a number")),
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
    match bare {
        "true" => return Ok(Value::Flag(true)),
        "false" => return Ok(Value::Flag(false)),
        _ => {}
    }
    bare.parse::<u64>()
        .map(Value::Number)
        // Never repeat the value: it may be a password typed without quotes.
        .map_err(|_| "the value must be a quoted string, a number, or true or false".into())
}

/// A literal loopback address. A name such as `localhost` does not count.
fn literal_loopback(host: &str) -> bool {
    host.trim_start_matches('[')
        .trim_end_matches(']')
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback())
}

fn security_name(security: ImapSecurity) -> &'static str {
    match security {
        ImapSecurity::StartTls => "starttls",
        ImapSecurity::Tls => "tls",
        ImapSecurity::Plain => "plain",
    }
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
    fn folder_overrides_parse_and_render() {
        let text = "backend = \"imap\"\nemail = \"me@example.com\"\nhost = \"imap.example.com\"\npassword = \"pw\"\nsent_folder = \"Sent Items\"\narchive_folder = \" Ablage \"\nall_mail_folder = \"[Gmail]/All Mail\"\n";
        let config = AccountConfig::parse(text).unwrap();
        assert_eq!(
            config.folders,
            vec![
                (FolderRole::Sent, "Sent Items".to_string()),
                (FolderRole::Archive, "Ablage".to_string()),
                (FolderRole::All, "[Gmail]/All Mail".to_string()),
            ]
        );
        assert_eq!(AccountConfig::parse(&config.render()).unwrap(), config);
        assert_eq!(
            config.imap_config().unwrap().folder_overrides,
            config.folders
        );

        let base = "email = \"me@proton.me\"\npassword = \"pw\"\n";
        for (line, message) in [
            ("drafts_folder = \"\"\n", "drafts_folder is empty"),
            ("trash_folder = 3\n", "trash_folder must be a quoted string"),
            (
                "trash_folder = \"a\\tb\"\n",
                "trash_folder contains a control character",
            ),
            ("inbox_folder = \"Inbox\"\n", "unknown key inbox_folder"),
            ("spam_folder = \"Spam\"\n", "unknown key spam_folder"),
        ] {
            let err = AccountConfig::parse(&format!("{base}{line}")).unwrap_err();
            assert!(err.to_string().contains(message), "{line}: {err}");
        }
    }

    #[test]
    fn notifications_default_to_showing_content_and_round_trip() {
        let base = "email = \"a@b\"\npassword = \"x\"\n";
        for (setting, expected) in [
            ("", NotificationMode::On),
            ("notification_content = false", NotificationMode::Private),
            (
                "notifications = true\nnotification_content = false",
                NotificationMode::Private,
            ),
            (
                "notifications = false\nnotification_content = false",
                NotificationMode::Off,
            ),
            (
                "notifications = \"on\"\nnotification_content = false",
                NotificationMode::On,
            ),
            ("notifications = \"private\"", NotificationMode::Private),
            ("notifications = \"off\"", NotificationMode::Off),
        ] {
            let config = AccountConfig::parse(&format!("{base}{setting}\n")).unwrap();
            assert_eq!(config.notifications, expected);
            assert_eq!(AccountConfig::parse(&config.render()).unwrap(), config);
            assert!(!config.render().contains("notification_content"));
        }
        assert!(AccountConfig::parse(&format!("{base}notifications = \"sometimes\"")).is_err());
    }

    #[test]
    fn save_sent_modes_parse_and_choose_by_provider() {
        let base = "backend = \"imap\"\nemail = \"demo@example.com\"\npassword = \"fixture\"\nwrite_back = true\n";
        let parse =
            |host, mode| AccountConfig::parse(&format!("{base}host = \"{host}\"\n{mode}")).unwrap();
        assert!(parse("imap.example.com", "").append_sent());
        assert!(!parse("IMAP.GMAIL.COM.", "save_sent = \"auto\"\n").append_sent());
        assert!(!parse("imap.googlemail.com", "").append_sent());
        assert!(!parse("imap.example.com", "smtp_host = \"smtp.gmail.com\"\n").append_sent());
        let append = parse("imap.gmail.com", "save_sent = \"append\"\n");
        let server = parse("imap.example.com", "save_sent = \"server\"\n");
        assert!(append.append_sent());
        assert!(!server.append_sent());
        assert_eq!(AccountConfig::parse(&append.render()).unwrap(), append);
        assert_eq!(AccountConfig::parse(&server.render()).unwrap(), server);
        assert!(
            AccountConfig::parse(&format!(
                "{base}host = \"imap.example.com\"\nsave_sent = \"yes\""
            ))
            .is_err()
        );
    }

    #[test]
    fn sent_and_notification_settings_round_trip_with_defaults_omitted() {
        let base = "email = \"demo@example.com\"\npassword = \"fixture\"\n";
        for (settings, save_sent, notifications) in [
            (
                "save_sent = \"auto\"\nnotifications = \"on\"",
                SaveSent::Auto,
                NotificationMode::On,
            ),
            (
                "save_sent = \"append\"\nnotifications = true\nnotification_content = false",
                SaveSent::Append,
                NotificationMode::Private,
            ),
            (
                "save_sent = \"server\"\nnotifications = \"off\"",
                SaveSent::Server,
                NotificationMode::Off,
            ),
        ] {
            let config = AccountConfig::parse(&format!("{base}{settings}\n")).unwrap();
            assert_eq!(
                (config.save_sent, config.notifications),
                (save_sent, notifications)
            );
            let rendered = config.render();
            assert_eq!(
                rendered.contains("save_sent ="),
                save_sent != SaveSent::Auto
            );
            assert_eq!(
                rendered.contains("notifications ="),
                notifications != NotificationMode::On
            );
            assert!(!rendered.contains("notification_content"));
            assert_eq!(AccountConfig::parse(&rendered).unwrap(), config);
        }
    }

    #[test]
    fn write_back_is_off_unless_turned_on() {
        let base = "email = \"me@proton.me\"\npassword = \"pw\"\n";
        let off = AccountConfig::parse(base).unwrap();
        assert!(!off.write_back);
        assert!(!off.render().contains("write_back"));
        let on = AccountConfig::parse(&format!("{base}write_back = true # sync triage\n")).unwrap();
        assert!(on.write_back);
        assert_eq!(AccountConfig::parse(&on.render()).unwrap(), on);
        assert!(
            !AccountConfig::parse(&format!("{base}write_back = false\n"))
                .unwrap()
                .write_back
        );
        for line in [
            "write_back = \"true\"\n",
            "write_back = 1\n",
            "write_back = yes\n",
        ] {
            let err = AccountConfig::parse(&format!("{base}{line}")).unwrap_err();
            assert!(
                err.to_string().contains("write_back") || err.to_string().contains("true or false"),
                "{line}: {err}"
            );
        }
        let err = AccountConfig::parse("email = \"me@proton.me\"\npassword = true\n").unwrap_err();
        assert!(
            err.to_string().contains("password must be a quoted string"),
            "{err}"
        );
    }

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
    fn keyring_source_round_trips_and_excludes_inline_and_command() {
        let config =
            AccountConfig::parse("email = \"a@b\"\npassword_source = \"keyring\"").unwrap();
        assert_eq!(config.password, PasswordSource::Keyring);
        let rendered = config.render();
        assert_eq!(AccountConfig::parse(&rendered).unwrap(), config);
        assert!(rendered.contains("password_source = \"keyring\""));
        assert!(!rendered.contains("password =") && !rendered.contains("password_command"));

        for fields in [
            "password_source = \"other\"",
            "password_source = 1",
            "password_source = \"keyring\"\npassword = \"test-only\"",
            "password_source = \"keyring\"\npassword_command = \"printf test-only\"",
        ] {
            assert!(AccountConfig::parse(&format!("email = \"a@b\"\n{fields}")).is_err());
        }
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
    fn classifies_imap_and_smtp_errors() {
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
            (
                "smtp: connect 127.0.0.1:1025 failed: Connection refused",
                SyncFailureKind::Unreachable,
            ),
            (
                "smtp: resolve smtp.nope:465 failed: x",
                SyncFailureKind::Unreachable,
            ),
            (
                "smtp: AUTH rejected: 454 4.7.0 Temporary failure",
                SyncFailureKind::Other,
            ),
            (
                "smtp: the server does not offer STARTTLS and this account requires TLS",
                SyncFailureKind::Other,
            ),
        ];
        for (message, kind) in cases {
            assert_eq!(classify_sync_error(&Error::Backend(message.into())), kind);
        }
        assert_eq!(
            classify_sync_error(&Error::SmtpCredentialsRejected(
                "smtp: AUTH rejected: 535 5.7.8 Authentication failed".into()
            )),
            SyncFailureKind::LoginRejected
        );
    }

    #[test]
    fn smtp_defaults_never_choose_plaintext() {
        let bridge = AccountConfig::parse("email = \"a@b\"\npassword = \"x\"\n").unwrap();
        assert_eq!(bridge.smtp_config(), SmtpConfig::bridge_local());
        let plain_imap = AccountConfig::parse(
            "email = \"a@b\"\npassword = \"x\"\nhost = \"127.0.0.1\"\nsecurity = \"plain\"\n",
        )
        .unwrap();
        assert_eq!(plain_imap.smtp_config().security, ImapSecurity::StartTls);
        let imap = AccountConfig::parse(
            "backend = \"imap\"\nemail = \"a@b\"\npassword = \"x\"\nhost = \"imap.b\"",
        )
        .unwrap();
        let smtp = imap.smtp_config();
        assert_eq!(
            (smtp.host.as_str(), smtp.port, smtp.security),
            ("imap.b", 465, ImapSecurity::Tls)
        );
        assert!(!smtp.accept_invalid_certs);
    }

    #[test]
    fn smtp_keys_parse_round_trip_and_plain_needs_loopback() {
        let config = AccountConfig::parse(
            "email = \"a@b\"\npassword = \"x\"\nsmtp_host = \"::1\"\nsmtp_port = 2525\nsmtp_security = \"plain\"\n",
        )
        .unwrap();
        let smtp = config.smtp_config();
        assert_eq!(
            (smtp.host.as_str(), smtp.port, smtp.security),
            ("::1", 2525, ImapSecurity::Plain)
        );
        assert!(smtp.accept_invalid_certs);
        assert_eq!(AccountConfig::parse(&config.render()).unwrap(), config);

        for (extra, needle) in [
            (
                "smtp_host = \"localhost\"\nsmtp_security = \"plain\"",
                "loopback",
            ),
            (
                "smtp_host = \"smtp.b\"\nsmtp_security = \"plain\"",
                "loopback",
            ),
            ("smtp_security = \"none\"", "smtp_security must be"),
            ("smtp_host = \"\"", "smtp_host is empty"),
            ("smtp_port = 0", "out of range"),
        ] {
            let text = format!("email = \"a@b\"\npassword = \"x\"\n{extra}\n");
            let err = AccountConfig::parse(&text).unwrap_err().to_string();
            assert!(err.contains(needle), "{extra:?}: {err}");
        }
        let mut remote = AccountConfig::bridge("a@b", "a@b", PasswordSource::Inline("x".into()));
        remote.smtp_host = Some("smtp.example.com".into());
        assert!(!remote.smtp_config().accept_invalid_certs);
    }

    #[test]
    fn debug_hides_inline_password() {
        let config = AccountConfig::bridge("a@b", "a@b", PasswordSource::Inline("hunter2".into()));
        assert!(!format!("{config:?}").contains("hunter2"));
    }
}
