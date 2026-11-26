//! SMTP submission for Bridge and ordinary IMAP accounts.
//!
//! [`confirm_send`] checks the editor's confirmation before delivery. The bytes on the wire are
//! [`crate::PendingSend::mime`]. Bcc addresses are envelope recipients only.
//!
//! A failure before the server has the whole message is a plain
//! [`Error::Backend`]: nothing was sent. Once the terminating dot starts going
//! out, a lost connection or a timeout is
//! [`Error::SendUncertain`], because the server may already have queued it.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::imap::{ImapConfig, ImapSecurity, TlsStream, host_is_loopback, wrap_tcp_as_tls};
use crate::send::{Draft, PendingSend, SentMessage, UserSendConfirmation, require_confirmation};
use crate::{AccountId, Error, RemoteId, Result};

const TIMEOUT: Duration = Duration::from_secs(15);

/// Where and how an account submits mail. The login is the account's IMAP
/// username and password.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    /// [`ImapSecurity::StartTls`] refuses a server that does not offer
    /// STARTTLS rather than log in over cleartext. [`ImapSecurity::Plain`] is
    /// only allowed for a literal loopback address.
    pub security: ImapSecurity,
    /// Bridge signs its local listener with its own certificate. Only allowed
    /// for a loopback host.
    pub accept_invalid_certs: bool,
    /// Queue a server Sent copy after successful submission.
    pub append_sent: bool,
    pub senders: SmtpSenders,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmtpSenders {
    /// Recheck the primary address and aliases in account.toml at confirmation.
    AccountFile,
    /// Canonical addresses, including the account address. An empty list permits no sender.
    Fixed(Vec<String>),
}

impl SmtpConfig {
    /// Proton Mail Bridge defaults: local STARTTLS on 1025, certificate not public.
    pub fn bridge_local() -> Self {
        Self {
            host: "127.0.0.1".into(),
            port: 1025,
            security: ImapSecurity::StartTls,
            accept_invalid_certs: true,
            append_sent: false,
            senders: SmtpSenders::Fixed(Vec::new()),
        }
    }
}

pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub security: ImapSecurity,
    pub accept_invalid_certs: bool,
}

impl Endpoint {
    pub(crate) fn new(config: &SmtpConfig, username: &str, password: &str) -> Self {
        Self {
            host: config.host.clone(),
            port: config.port,
            username: username.to_string(),
            password: password.to_string(),
            security: config.security,
            accept_invalid_certs: config.accept_invalid_certs,
        }
    }
}

pub(crate) fn confirm_send(
    account: &AccountId,
    config: &SmtpConfig,
    login: &ImapConfig,
    pending: PendingSend,
    confirmation: UserSendConfirmation,
) -> Result<SentMessage> {
    require_confirmation(&pending, &confirmation)?;
    let from = sender(&pending.mime)?;
    if crate::compose::email_of(&pending.draft.from).as_ref() != Some(&from) {
        return Err(smtp_err("smtp: draft and message From addresses differ"));
    }
    let allowed = match &config.senders {
        SmtpSenders::AccountFile => {
            crate::account_config::configured_sender(account, &from).is_ok()
        }
        SmtpSenders::Fixed(addresses) => addresses.contains(&from),
    };
    if !allowed {
        return Err(smtp_err("smtp: From is not configured in account.toml"));
    }
    let remote = deliver(
        &Endpoint::new(config, &login.username, &login.password),
        &pending,
        &from,
    )?;
    Ok(SentMessage {
        pending_id: pending.id,
        remote_id: RemoteId(remote),
    })
}

fn deliver(endpoint: &Endpoint, pending: &PendingSend, from: &str) -> Result<String> {
    let recipients = envelope(&pending.draft)?;
    let (mut pipe, hello) = login(endpoint)?;
    let mail_from = if offers(&hello, "8BITMIME") {
        format!("MAIL FROM:<{from}> BODY=8BITMIME")
    } else {
        format!("MAIL FROM:<{from}>")
    };
    expect(command(&mut pipe, &mail_from)?, 250, "MAIL FROM")?;
    for recipient in &recipients {
        expect(
            command(&mut pipe, &format!("RCPT TO:<{recipient}>"))?,
            250,
            "RCPT TO",
        )?;
    }
    expect(command(&mut pipe, "DATA")?, 354, "DATA")?;
    transfer(&mut pipe, &pending.mime)
}

/// Log in and quit: greeting, `EHLO`, `STARTTLS` when the account asks for
/// it, `AUTH`, `QUIT`. Never `MAIL FROM`, so nothing can be sent.
pub(crate) fn check_login(endpoint: &Endpoint) -> Result<()> {
    let (mut pipe, _) = login(endpoint)?;
    let _ = command(&mut pipe, "QUIT");
    Ok(())
}

/// Connect and authenticate. Returns the session and the last `EHLO` reply.
/// Every refusal here comes before any envelope command.
fn login(endpoint: &Endpoint) -> Result<(Pipe, Reply)> {
    if endpoint.username.is_empty() {
        return Err(smtp_err("SMTP username is empty"));
    }
    let loopback = host_is_loopback(&endpoint.host);
    if endpoint.accept_invalid_certs && !loopback {
        return Err(smtp_err(
            "smtp: accept_invalid_certs is only allowed for a loopback host",
        ));
    }
    if endpoint.security == ImapSecurity::Plain && !loopback {
        return Err(smtp_err(
            "smtp: plaintext is only allowed for a loopback address like 127.0.0.1",
        ));
    }
    let tcp = connect(&endpoint.host, endpoint.port)?;
    let mut pipe = match endpoint.security {
        ImapSecurity::Tls => Pipe::tls(wrap_tcp_as_tls(
            tcp,
            &endpoint.host,
            endpoint.accept_invalid_certs,
        )?),
        ImapSecurity::StartTls | ImapSecurity::Plain => Pipe::plain(tcp),
    };
    match handshake(&mut pipe, endpoint) {
        Ok(hello) => Ok((pipe, hello)),
        Err(err) => {
            // Leave politely on every refusal once a session is open.
            if pipe.plain.is_some() || pipe.tls.is_some() {
                let _ = command(&mut pipe, "QUIT");
            }
            Err(err)
        }
    }
}

/// Greeting, `EHLO`, `STARTTLS` when required, and `AUTH` on an open socket.
fn handshake(pipe: &mut Pipe, endpoint: &Endpoint) -> Result<Reply> {
    let pipe = &mut *pipe;
    expect(read_reply(pipe)?, 220, "greeting")?;
    let mut hello = expect(command(pipe, "EHLO localhost")?, 250, "EHLO")?;
    if endpoint.security == ImapSecurity::StartTls {
        if !offers(&hello, "STARTTLS") {
            return Err(Error::Tls(
                "smtp: the server does not offer STARTTLS and this account requires TLS".into(),
            ));
        }
        command(pipe, "STARTTLS")
            .and_then(|reply| expect(reply, 220, "STARTTLS"))
            .map_err(|err| Error::Tls(err.to_string()))?;
        pipe.upgrade(&endpoint.host, endpoint.accept_invalid_certs)?;
        hello = expect(command(pipe, "EHLO localhost")?, 250, "EHLO")?;
    }
    authenticate(pipe, &hello, endpoint)?;
    Ok(hello)
}

/// Write the message after `354` and read the server's verdict.
///
/// The server queues the message when it reads the final `.` line, which can
/// happen before a write error or a lost reply reaches us. So any I/O
/// failure from here on is [`Error::SendUncertain`]. Only an explicit `4xx`
/// or `5xx` reply proves the message was not accepted.
fn transfer(pipe: &mut Pipe, mime: &[u8]) -> Result<String> {
    // A server queues nothing before the terminating dot, so a failure while
    // the body is still going out means it was not sent.
    write_body(pipe, mime)?;
    let reply = write_all(pipe, b".\r\n")
        .and_then(|()| read_reply(pipe))
        .map_err(|err| uncertain(err.to_string()))?;
    let detail = reply.lines.last().cloned().unwrap_or_default();
    match reply.code {
        250 => {
            let _ = command(pipe, "QUIT");
            Ok(reply_text(&reply))
        }
        400..=599 => Err(smtp_err(format!("smtp: message rejected: {detail}"))),
        _ => Err(uncertain(format!(
            "smtp: unexpected reply to the message: {detail}"
        ))),
    }
}

fn uncertain(message: String) -> Error {
    Error::SendUncertain(format!(
        "{message}, after the whole message was handed to the server"
    ))
}

pub(crate) fn sender(mime: &[u8]) -> Result<String> {
    let text = std::str::from_utf8(header_block(mime))
        .map_err(|_| smtp_err("smtp: message headers are not UTF-8"))?;
    let mut from = None;
    let mut name = String::new();
    let mut value = String::new();
    let mut open = false;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line.starts_with([' ', '\t']) {
            if open {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if open {
            absorb(&name, &value, &mut from)?;
            open = false;
        }
        if let Some((raw_name, raw_value)) = line.split_once(':') {
            name = raw_name.trim().to_string();
            value = raw_value.trim().to_string();
            open = true;
        }
    }
    if open {
        absorb(&name, &value, &mut from)?;
    }
    from.ok_or_else(|| smtp_err("smtp: message has no From address"))
}

fn absorb(name: &str, value: &str, from: &mut Option<String>) -> Result<()> {
    if name.eq_ignore_ascii_case("bcc") {
        return Err(smtp_err(
            "smtp: Bcc header is not allowed; Bcc stays on the envelope",
        ));
    }
    if name.eq_ignore_ascii_case("from") && from.is_none() {
        let email = crate::compose::email_of(value.trim())
            .ok_or_else(|| smtp_err("smtp: From is not an address"))?;
        *from = Some(email);
    }
    Ok(())
}

fn envelope(draft: &Draft) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for raw in draft
        .to
        .iter()
        .chain(draft.cc.iter())
        .chain(draft.bcc.iter())
    {
        let email =
            crate::compose::email_of(raw).ok_or_else(|| smtp_err("smtp: invalid recipient"))?;
        if out
            .iter()
            .any(|have: &String| have.eq_ignore_ascii_case(&email))
        {
            continue;
        }
        out.push(email);
    }
    if out.is_empty() {
        return Err(smtp_err("smtp: no recipients"));
    }
    Ok(out)
}

fn header_block(mime: &[u8]) -> &[u8] {
    if let Some(index) = find_bytes(mime, b"\r\n\r\n") {
        return &mime[..index];
    }
    if let Some(index) = find_bytes(mime, b"\n\n") {
        return &mime[..index];
    }
    mime
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn connect(host: &str, port: u16) -> Result<TcpStream> {
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|err| smtp_err(format!("smtp: resolve {host}:{port} failed: {err}")))?;
    let mut last = None;
    let mut tried = false;
    for addr in addrs {
        tried = true;
        match TcpStream::connect_timeout(&addr, TIMEOUT) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(TIMEOUT))
                    .map_err(|err| smtp_err(format!("smtp: {err}")))?;
                stream
                    .set_write_timeout(Some(TIMEOUT))
                    .map_err(|err| smtp_err(format!("smtp: {err}")))?;
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(err) => last = Some(err),
        }
    }
    if !tried {
        return Err(smtp_err(format!(
            "smtp: resolve {host}:{port} returned no address"
        )));
    }
    Err(smtp_err(format!(
        "smtp: connect {host}:{port} failed: {}",
        last.expect("a failed connect records an error")
    )))
}

trait Io: Read + Write {}
impl<T: Read + Write> Io for T {}

struct Pipe {
    plain: Option<TcpStream>,
    tls: Option<TlsStream>,
}

impl Pipe {
    fn plain(stream: TcpStream) -> Self {
        Self {
            plain: Some(stream),
            tls: None,
        }
    }

    fn tls(stream: TlsStream) -> Self {
        Self {
            plain: None,
            tls: Some(stream),
        }
    }

    fn upgrade(&mut self, host: &str, accept_invalid_certs: bool) -> Result<()> {
        let tcp = self
            .plain
            .take()
            .ok_or_else(|| smtp_err("smtp: STARTTLS requires a cleartext session"))?;
        self.tls = Some(wrap_tcp_as_tls(tcp, host, accept_invalid_certs)?);
        Ok(())
    }

    fn io(&mut self) -> &mut dyn Io {
        if let Some(tls) = self.tls.as_mut() {
            tls
        } else {
            self.plain.as_mut().expect("smtp socket")
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.io().read(buf)
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.io().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.io().flush()
    }
}

struct Reply {
    code: u16,
    lines: Vec<String>,
}

fn command(pipe: &mut Pipe, text: &str) -> Result<Reply> {
    if text
        .bytes()
        .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
    {
        return Err(smtp_err("smtp: command contains a line break"));
    }
    write_all(pipe, format!("{text}\r\n").as_bytes())?;
    read_reply(pipe)
}

/// The dot-stuffed message, ending in CRLF, without the terminating dot.
fn write_body(pipe: &mut Pipe, mime: &[u8]) -> Result<()> {
    let mut stuffed = Vec::with_capacity(mime.len() + 8);
    let mut line_start = true;
    for &byte in mime {
        if line_start && byte == b'.' {
            stuffed.push(b'.');
        }
        stuffed.push(byte);
        line_start = byte == b'\n';
    }
    if !stuffed.ends_with(b"\r\n") {
        stuffed.extend_from_slice(b"\r\n");
    }
    write_all(pipe, &stuffed)
}

fn write_all(pipe: &mut Pipe, bytes: &[u8]) -> Result<()> {
    pipe.write_all(bytes)
        .map_err(|err| smtp_err(format!("smtp: write failed: {err}")))?;
    pipe.flush()
        .map_err(|err| smtp_err(format!("smtp: write failed: {err}")))?;
    Ok(())
}

fn read_reply(pipe: &mut Pipe) -> Result<Reply> {
    let mut lines = Vec::new();
    loop {
        let line = read_line(pipe)?;
        if line.len() < 3 || !line.as_bytes()[..3].iter().all(u8::is_ascii_digit) {
            return Err(smtp_err("smtp: bad reply"));
        }
        let code = line[..3]
            .parse()
            .map_err(|_| smtp_err("smtp: bad reply code"))?;
        let continued = line.as_bytes().get(3).copied() == Some(b'-');
        lines.push(line);
        if !continued {
            return Ok(Reply { code, lines });
        }
        if lines.len() > 64 {
            return Err(smtp_err("smtp: reply too long"));
        }
    }
}

fn read_line(pipe: &mut Pipe) -> Result<String> {
    let mut buf = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        let count = loop {
            match pipe.read(&mut byte) {
                Ok(count) => break count,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(smtp_err(format!("smtp: read failed: {err}"))),
            }
        };
        if count == 0 {
            return Err(smtp_err("smtp: connection closed"));
        }
        buf.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
        if buf.len() > 8192 {
            return Err(smtp_err("smtp: reply line too long"));
        }
    }
    Ok(String::from_utf8_lossy(&buf)
        .trim_end_matches(['\r', '\n'])
        .to_string())
}

fn expect(reply: Reply, code: u16, what: &str) -> Result<Reply> {
    if reply.code == code {
        Ok(reply)
    } else {
        let detail = reply.lines.last().map(String::as_str).unwrap_or("");
        Err(smtp_err(format!("smtp: {what} rejected: {detail}")))
    }
}

fn offers(reply: &Reply, capability: &str) -> bool {
    reply.lines.iter().any(|line| {
        line.get(4..)
            .unwrap_or("")
            .split_whitespace()
            .any(|word| word.eq_ignore_ascii_case(capability))
    })
}

fn authenticate(pipe: &mut Pipe, hello: &Reply, endpoint: &Endpoint) -> Result<()> {
    if !offers(hello, "PLAIN") {
        return Err(smtp_err("smtp: server does not offer AUTH PLAIN"));
    }
    let (username, password) = (&endpoint.username, &endpoint.password);
    if forbidden(username) || forbidden(password) {
        return Err(smtp_err("smtp: credentials contain a forbidden octet"));
    }
    let mut token = Vec::with_capacity(username.len() + password.len() + 2);
    token.push(0);
    token.extend(username.as_bytes());
    token.push(0);
    token.extend(password.as_bytes());
    let encoded = b64(&token);
    let reply = command(pipe, &format!("AUTH PLAIN {encoded}"))?;
    if reply.code == 235 {
        return Ok(());
    }
    let detail = auth_rejection(&reply, &encoded, username, password);
    if matches!(reply.code, 534 | 535) {
        Err(Error::SmtpCredentialsRejected(detail))
    } else {
        Err(smtp_err(detail))
    }
}

/// An AUTH reply can echo the exchange. Keep each diagnostic line, with
/// literal credentials and their wire encodings removed from the reply text.
fn auth_rejection(reply: &Reply, token: &str, username: &str, password: &str) -> String {
    let user_b64 = b64(username.as_bytes());
    let pass_b64 = b64(password.as_bytes());
    let mut secrets = [token, &user_b64, &pass_b64, username, password]
        .into_iter()
        .flat_map(|secret| [secret, secret.trim_end_matches('=')])
        .filter(|secret| !secret.is_empty())
        .collect::<Vec<_>>();
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));
    let mut detail = String::from("smtp: AUTH rejected: ");
    for (i, line) in reply.lines.iter().enumerate() {
        if i > 0 {
            detail.push('\n');
        }
        let prefix = if matches!(line.as_bytes().get(3), Some(b' ' | b'-')) {
            4
        } else {
            3
        };
        let mut text = line[prefix..].to_owned();
        for secret in &secrets {
            text = text.replace(secret, "[redacted]");
        }
        detail.push_str(&line[..prefix]);
        detail.push_str(&text);
    }
    detail
}

fn forbidden(value: &str) -> bool {
    value
        .bytes()
        .any(|byte| byte == 0 || byte == b'\r' || byte == b'\n')
}

fn reply_text(reply: &Reply) -> String {
    reply
        .lines
        .last()
        .and_then(|line| line.get(4..))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or("accepted")
        .to_string()
}

fn b64(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let packed =
            ((data[index] as u32) << 16) | ((data[index + 1] as u32) << 8) | data[index + 2] as u32;
        for shift in [18, 12, 6, 0] {
            out.push(ALPHABET[((packed >> shift) & 63) as usize] as char);
        }
        index += 3;
    }
    let rest = data.len() - index;
    if rest == 1 {
        let packed = (data[index] as u32) << 16;
        out.push(ALPHABET[((packed >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((packed >> 12) & 63) as usize] as char);
        out.push('=');
        out.push('=');
    } else if rest == 2 {
        let packed = ((data[index] as u32) << 16) | ((data[index + 1] as u32) << 8);
        out.push(ALPHABET[((packed >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((packed >> 12) & 63) as usize] as char);
        out.push(ALPHABET[((packed >> 6) & 63) as usize] as char);
        out.push('=');
    }
    out
}

fn smtp_err(message: impl Into<String>) -> Error {
    Error::Backend(message.into())
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::*;
    use crate::account::AccountId;
    use crate::backend::{BridgeBackend, MailBackend};
    use crate::imap::ImapConfig;
    use crate::send::{PendingSendId, UserSendConfirmation};

    fn sample_mime() -> Vec<u8> {
        b"From: Jon <jon@example.com>\r\n\
To: Ada <ada@example.com>\r\n\
Cc: be@example.com\r\n\
Subject: Hello\r\n\
MIME-Version: 1.0\r\n\
\r\n\
See you.\r\n\
.hidden\r\n\
Bcc: this is body text\r\n"
            .to_vec()
    }

    fn sample_pending() -> PendingSend {
        PendingSend::new(
            "bridge",
            Draft {
                account_id: AccountId("mailbox".into()),
                from: "jon@example.com".into(),
                to: vec!["Ada <ada@example.com>".into()],
                cc: vec!["be@example.com".into()],
                bcc: vec!["Hidden <hidden@example.com>".into()],
                subject: "Hello".into(),
                body: "See you.\n".into(),
                in_reply_to: None,
                attachments_digest: String::new(),
            },
        )
        .with_message(sample_mime(), Vec::new())
    }

    fn confirm(pending: &PendingSend) -> UserSendConfirmation {
        UserSendConfirmation::from_interactive_ui(
            &fell_confirm::InteractiveUi::for_fell_app(),
            pending,
        )
    }

    pub(crate) fn confirm_fixture_delivery(
        login: &ImapConfig,
        account: &AccountId,
        mime: &[u8],
    ) -> Result<SentMessage> {
        let (port, captured, join) = serve(Wire::Clear, AfterData::Accept);
        let mut pending = sample_pending();
        pending.draft.account_id = account.clone();
        pending.mime = mime.to_vec();
        pending.draft.from = sender(mime)?;
        let confirmation = confirm(&pending);
        let sent = confirm_send(
            account,
            &SmtpConfig {
                senders: SmtpSenders::Fixed(vec![pending.draft.from.clone()]),
                ..backend(port, ImapSecurity::Plain).smtp
            },
            login,
            pending,
            confirmation,
        );
        assert_eq!(
            captured.recv_timeout(Duration::from_secs(5)).unwrap().data,
            mime
        );
        join.join().unwrap();
        sent
    }

    fn backend(port: u16, security: ImapSecurity) -> BridgeBackend {
        BridgeBackend {
            account_id: AccountId("mailbox".into()),
            imap: ImapConfig {
                host: "127.0.0.1".into(),
                port: 1143,
                username: "bridge-user".into(),
                password: "bridge-secret".into(),
                security: crate::imap::ImapSecurity::Plain,
                accept_invalid_certs: true,
                folder_overrides: Vec::new(),
            },
            smtp: SmtpConfig {
                host: "127.0.0.1".into(),
                port,
                security,
                accept_invalid_certs: true,
                append_sent: false,
                senders: SmtpSenders::Fixed(vec!["jon@example.com".into()]),
            },
        }
    }

    #[test]
    fn bcc_header_is_refused_before_connect() {
        for header in [
            "Bcc: hidden@example.com\r\n",
            "Bcc:\r\n hidden@example.com\r\n",
        ] {
            let mime = format!("From: jon@example.com\r\n{header}Subject: x\r\n\r\nHi\r\n");
            let pending = sample_pending().with_message(mime.into_bytes(), Vec::new());
            let backend = backend(1, ImapSecurity::StartTls);
            let err = backend
                .confirm_send(pending.clone(), confirm(&pending))
                .unwrap_err();
            let text = err.to_string();
            assert!(text.contains("Bcc"), "{text}");
            assert!(!text.contains("connect"), "{text}");
        }
    }

    #[test]
    fn empty_username_does_not_open_smtp() {
        let pending = sample_pending();
        let mut bridge = backend(1, ImapSecurity::Plain);
        bridge.imap.username.clear();
        let err = bridge
            .confirm_send(pending.clone(), confirm(&pending))
            .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("SMTP"), "{text}");
        assert!(!text.contains("connect"), "{text}");
    }

    #[test]
    fn a_backend_uses_the_current_account_aliases_at_confirmation() {
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let (port, captured, join) = serve(Wire::Clear, AfterData::Accept);
        let config = format!(
            "id = 'mailbox'\nemail = 'jon@example.com'\nusername = 'bridge-user'\n\
             password = 'bridge-secret'\nsmtp_port = {port}\nsmtp_security = 'plain'\n"
        );
        let path = root.join("config/fell/account.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &config).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let initial = crate::AccountConfig::parse(&config).unwrap();
        let backend = crate::backend_for(&initial.account().id, Some(&initial)).unwrap();
        let mut pending = sample_pending();
        pending.draft.from = "Studio <alias@example.com>".into();
        pending.mime = String::from_utf8(pending.mime)
            .unwrap()
            .replace("jon@example.com", "alias@example.com")
            .into_bytes();
        let send = || backend.confirm_send(pending.clone(), confirm(&pending));
        let assert_refused = |result: Result<SentMessage>| {
            assert!(matches!(result, Err(Error::Backend(message))
                if message == "smtp: From is not configured in account.toml"));
        };
        assert_refused(send());
        fs::write(
            &path,
            format!("{config}[[identities]]\naddress = 'alias@example.com'\nname = 'Studio'\n"),
        )
        .unwrap();
        send().unwrap();
        let captured = captured.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(captured.from, "alias@example.com");
        assert_eq!(sender(&captured.data).unwrap(), "alias@example.com");
        assert_eq!(captured.data, pending.mime);
        join.join().unwrap();
        fs::write(&path, &config).unwrap();
        assert_refused(send());
    }

    #[test]
    fn an_empty_fixed_sender_list_refuses_before_connect() {
        let mut bridge = backend(1, ImapSecurity::Plain);
        bridge.smtp.senders = SmtpSenders::Fixed(Vec::new());
        let pending = sample_pending();
        assert!(matches!(
            bridge.confirm_send(pending.clone(), confirm(&pending)),
            Err(Error::Backend(message)) if message == "smtp: From is not configured in account.toml"
        ));
    }

    #[test]
    fn bridge_delivers_after_confirmation_with_bcc_on_the_envelope() {
        let (sent, got) = exchange(Wire::Clear, ImapSecurity::Plain);
        assert_eq!(sent.remote_id.0, "queued as test-id");
        assert_delivery(&got, "bridge-user", "bridge-secret");
        assert!(got.clear.is_empty());
    }

    #[test]
    fn imap_account_uses_its_smtp_settings_and_classifies_delivery() {
        use crate::{AccountConfig, BackendKind, backend_for};
        use std::fs;
        use std::os::unix::fs::PermissionsExt;

        let Some(root) = crate::test_support::in_isolated_process() else {
            return;
        };
        let path = root.join("config/fell/account.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        for after in [AfterData::Accept, AfterData::Hangup, AfterData::Reject] {
            let (port, rx, join) = serve(Wire::Clear, after);
            let config = AccountConfig::parse(&format!(
                "backend = \"imap\"\nid = \"mailbox\"\nemail = \"jon@example.com\"\n\
                 host = \"127.0.0.2\"\nusername = \"imap-user\"\n\
                 password_command = \"printf imap-secret\"\n\
                 smtp_host = \"127.0.0.1\"\nsmtp_port = {port}\nsmtp_security = \"plain\"\n"
            ))
            .unwrap();
            fs::write(&path, config.render()).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let backend = backend_for(&config.account().id, Some(&config)).unwrap();
            assert_eq!(backend.kind(), BackendKind::Imap);
            let pending = backend
                .submit_composed(sample_pending().draft, sample_mime(), Vec::new())
                .unwrap();
            let mut changed = pending.clone();
            changed.draft.bcc.clear();
            assert!(matches!(
                backend.confirm_send(pending.clone(), confirm(&changed)),
                Err(Error::SendUnconfirmed)
            ));

            let result = backend.confirm_send(pending.clone(), confirm(&pending));
            match after {
                AfterData::Accept => {
                    let sent = result.unwrap();
                    assert_eq!(sent.pending_id, pending.id);
                    assert_eq!(sent.remote_id.0, "queued as test-id");
                }
                AfterData::Hangup => assert!(matches!(result, Err(Error::SendUncertain(_)))),
                AfterData::Reject => assert!(matches!(result, Err(Error::Backend(_)))),
                _ => unreachable!(),
            }
            let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            join.join().unwrap();
            assert_delivery(&got, "imap-user", "imap-secret");
        }
    }

    #[test]
    fn implicit_tls_delivers() {
        let (sent, got) = exchange(Wire::Tls, ImapSecurity::Tls);
        assert_eq!(sent.remote_id.0, "queued as test-id");
        assert_delivery(&got, "bridge-user", "bridge-secret");
    }

    #[test]
    fn required_tls_does_not_log_in_to_a_loopback_server_without_starttls() {
        let (result, got) = attempt(Wire::Clear, ImapSecurity::StartTls, AfterData::Accept);
        let err = result.unwrap_err();
        assert_eq!(
            crate::classify_sync_error(&err),
            crate::SyncFailureKind::Tls,
            "{err}"
        );
        assert!(err.to_string().contains("STARTTLS"), "{err}");
        assert!(got.auth.is_empty(), "the login went out: {}", got.auth);
        assert!(got.data.is_empty());
        assert!(got.quit, "a refused session still says QUIT");
    }

    #[test]
    fn plaintext_and_unchecked_certificates_stay_on_loopback() {
        for (security, accept_invalid_certs) in
            [(ImapSecurity::Plain, false), (ImapSecurity::StartTls, true)]
        {
            let err = deliver(
                &Endpoint {
                    host: "mail.example.com".into(),
                    port: 1,
                    username: "bridge-user".into(),
                    password: "bridge-secret".into(),
                    security,
                    accept_invalid_certs,
                },
                &sample_pending(),
                "jon@example.com",
            )
            .unwrap_err();
            let text = err.to_string();
            assert!(text.contains("loopback"), "{text}");
            assert!(!text.contains("connect"), "{text}");
        }
    }

    #[test]
    fn a_lost_reply_after_the_message_may_have_been_sent() {
        let (result, got) = attempt(Wire::Clear, ImapSecurity::Plain, AfterData::Hangup);
        let err = result.unwrap_err();
        assert!(matches!(err, Error::SendUncertain(_)), "{err:?}");
        assert!(err.to_string().contains("after the whole message"), "{err}");
        assert_eq!(got.data, sample_mime());
    }

    #[test]
    fn a_refusal_before_or_after_the_message_is_not_sent() {
        for after in [
            AfterData::Reject,
            AfterData::RejectRcpt,
            AfterData::HangupAtRcpt,
        ] {
            let (result, _) = attempt(Wire::Clear, ImapSecurity::Plain, after);
            let err = result.unwrap_err();
            assert!(matches!(err, Error::Backend(_)), "{after:?}: {err:?}");
        }
    }

    #[test]
    fn credential_refusal_preserves_diagnostics_and_redacts_authentication_data() {
        let (result, got) = attempt(Wire::Clear, ImapSecurity::Plain, AfterData::RejectAuth);
        let Error::SmtpCredentialsRejected(detail) = result.unwrap_err() else {
            panic!("535 must be a credential rejection");
        };
        assert_eq!(
            detail,
            "smtp: AUTH rejected: 535-5.7.8 Authentication failed for [redacted]\n\
             535 5.7.8 Rejected [redacted] [redacted] [redacted]; check account settings"
        );
        assert!(!got.auth.is_empty());
        assert!(got.quit);
        assert!(got.from.is_empty() && got.rcpt.is_empty() && got.data.is_empty());
    }

    #[test]
    fn starttls_hides_the_password_and_still_delivers() {
        let (sent, got) = exchange(Wire::StartTls, ImapSecurity::StartTls);
        assert_eq!(sent.remote_id.0, "queued as test-id");
        assert_delivery(&got, "bridge-user", "bridge-secret");
        let clear = String::from_utf8_lossy(&got.clear);
        assert!(!clear.to_ascii_uppercase().contains("AUTH"), "{clear}");
        assert!(!clear.contains("bridge-secret"), "{clear}");
    }

    #[test]
    fn the_login_check_authenticates_and_sends_nothing() {
        for (wire, security) in [
            (Wire::Clear, ImapSecurity::Plain),
            (Wire::StartTls, ImapSecurity::StartTls),
            (Wire::Tls, ImapSecurity::Tls),
        ] {
            let (port, rx, join) = serve(wire, AfterData::Accept);
            let backend = backend(port, security);
            let endpoint = Endpoint::new(&backend.smtp, "bridge-user", "bridge-secret");
            check_login(&endpoint).unwrap();
            let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
            join.join().unwrap();
            let mut token = vec![0];
            token.extend_from_slice(b"bridge-user\0bridge-secret");
            assert_eq!(got.auth, b64(&token), "{wire:?}");
            assert!(got.quit, "{wire:?}");
            assert!(got.from.is_empty() && got.rcpt.is_empty() && got.data.is_empty());
        }
    }

    #[test]
    fn the_login_check_through_the_account_file_classifies_failures() {
        use crate::{AccountConfig, SyncFailureKind, classify_sync_error};

        let config = |port: u16| {
            AccountConfig::parse(&format!(
                "email = \"a@b\"\nusername = \"bridge-user\"\npassword = \"bridge-secret\"\n\
                 smtp_port = {port}\nsmtp_security = \"plain\"\n"
            ))
            .unwrap()
        };
        let (port, rx, join) = serve(Wire::Clear, AfterData::Accept);
        config(port).check_smtp().unwrap();
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap().quit);
        join.join().unwrap();

        let (port, rx, join) = serve(Wire::Clear, AfterData::RejectAuth);
        let err = config(port).check_smtp().unwrap_err();
        assert_eq!(
            classify_sync_error(&err),
            SyncFailureKind::LoginRejected,
            "{err}"
        );
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        join.join().unwrap();

        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = closed.local_addr().unwrap().port();
        drop(closed);
        let err = config(port).check_smtp().unwrap_err();
        assert_eq!(
            classify_sync_error(&err),
            SyncFailureKind::Unreachable,
            "{err}"
        );

        // A server without STARTTLS never sees the password.
        let (port, rx, join) = serve(Wire::Clear, AfterData::Accept);
        let mut required = config(port);
        required.smtp_security = Some(ImapSecurity::StartTls);
        let err = required.check_smtp().unwrap_err();
        assert!(err.to_string().contains("STARTTLS"), "{err}");
        assert_eq!(classify_sync_error(&err), SyncFailureKind::Tls, "{err}");
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        join.join().unwrap();
        assert!(got.auth.is_empty());
    }

    #[test]
    fn a_failed_starttls_handshake_closes_without_authenticating() {
        let (port, rx, join) = serve(Wire::StartTls, AfterData::Accept);
        let mut config = backend(port, ImapSecurity::StartTls).smtp;
        config.accept_invalid_certs = false;
        let err =
            check_login(&Endpoint::new(&config, "mailbox-user", "sample-password")).unwrap_err();
        assert_eq!(
            crate::classify_sync_error(&err),
            crate::SyncFailureKind::Tls,
            "{err}"
        );
        let got = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        join.join().unwrap();
        assert!(got.auth.is_empty());
    }

    fn assert_delivery(got: &Captured, username: &str, password: &str) {
        let mut token = vec![0];
        token.extend_from_slice(username.as_bytes());
        token.push(0);
        token.extend_from_slice(password.as_bytes());
        assert_eq!(got.auth, b64(&token));
        assert_eq!(got.from, "jon@example.com");
        assert_eq!(
            got.rcpt,
            ["ada@example.com", "be@example.com", "hidden@example.com"]
        );
        assert_eq!(got.data, sample_mime());
        let headers = String::from_utf8_lossy(header_block(&got.data)).to_ascii_lowercase();
        assert!(!headers.contains("bcc:"), "{headers}");
    }

    fn exchange(wire: Wire, security: ImapSecurity) -> (crate::SentMessage, Captured) {
        let (port, rx, join) = serve(wire, AfterData::Accept);
        let backend = backend(port, security);
        let pending = sample_pending();
        let mut other = pending.clone();
        other.id = PendingSendId("nope".into());
        let err = backend
            .confirm_send(pending.clone(), confirm(&other))
            .unwrap_err();
        assert!(matches!(err, Error::SendUnconfirmed));
        let sent = backend
            .confirm_send(pending.clone(), confirm(&pending))
            .unwrap();
        assert_eq!(sent.pending_id, pending.id);
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("fake smtp finished");
        join.join().expect("fake smtp thread");
        (sent, got)
    }

    /// One confirmed send against a fake server.
    fn attempt(
        wire: Wire,
        security: ImapSecurity,
        after: AfterData,
    ) -> (Result<crate::SentMessage>, Captured) {
        let (port, rx, join) = serve(wire, after);
        let pending = sample_pending();
        let result = backend(port, security).confirm_send(pending.clone(), confirm(&pending));
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("fake smtp finished");
        join.join().expect("fake smtp thread");
        (result, got)
    }

    /// How the fake server protects the session.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Wire {
        /// Cleartext and no STARTTLS offered.
        Clear,
        StartTls,
        /// TLS from the first byte.
        Tls,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum AfterData {
        Accept,
        /// `554` after reading the whole message.
        Reject,
        /// Close the socket after reading the message, without a reply.
        Hangup,
        /// `550` to the first RCPT TO.
        RejectRcpt,
        /// Close the socket at RCPT TO, before DATA.
        HangupAtRcpt,
        /// `535` to AUTH.
        RejectAuth,
    }

    struct Captured {
        auth: String,
        from: String,
        rcpt: Vec<String>,
        data: Vec<u8>,
        clear: Vec<u8>,
        quit: bool,
    }

    fn serve(
        wire: Wire,
        after: AfterData,
    ) -> (u16, mpsc::Receiver<Captured>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let join = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            sock.set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let _ = sock.set_nodelay(true);
            if wire == Wire::Tls {
                let mut tls = server_tls(sock);
                write_reply(&mut tls, "220 localhost ESMTP\r\n");
                dialog(tls, tx, Vec::new(), after);
                return;
            }
            write_reply(&mut sock, "220 localhost ESMTP\r\n");
            if wire == Wire::StartTls {
                let mut clear = Vec::new();
                let ehlo = read_cmd(&mut sock, &mut clear);
                assert!(ehlo.to_ascii_uppercase().starts_with("EHLO"), "{ehlo}");
                write_reply(
                    &mut sock,
                    "250-localhost\r\n250-STARTTLS\r\n250 AUTH PLAIN\r\n",
                );
                let start = read_cmd(&mut sock, &mut clear);
                assert!(
                    start.to_ascii_uppercase().starts_with("STARTTLS"),
                    "{start}"
                );
                write_reply(&mut sock, "220 ready\r\n");
                dialog(server_tls(sock), tx, clear, after);
            } else {
                dialog(sock, tx, Vec::new(), after);
            }
        });
        (port, rx, join)
    }

    fn dialog(
        mut sock: impl Read + Write,
        tx: mpsc::Sender<Captured>,
        clear: Vec<u8>,
        after: AfterData,
    ) {
        let mut captured = Captured {
            auth: String::new(),
            from: String::new(),
            rcpt: Vec::new(),
            data: Vec::new(),
            clear,
            quit: false,
        };
        loop {
            let line = read_cmd(&mut sock, &mut Vec::new());
            if line.is_empty() {
                break;
            }
            let upper = line.to_ascii_uppercase();
            if upper.starts_with("EHLO") {
                write_reply(&mut sock, "250-localhost\r\n250 AUTH PLAIN\r\n");
            } else if let Some(token) = line.strip_prefix("AUTH PLAIN ") {
                captured.auth = token.to_string();
                if after == AfterData::RejectAuth {
                    write_reply(
                        &mut sock,
                        &format!(
                            "535-5.7.8 Authentication failed for bridge-user\r\n\
                             535 5.7.8 Rejected bridge-secret {token} {}; check account settings\r\n",
                            b64(b"bridge-secret").trim_end_matches('=')
                        ),
                    );
                } else {
                    write_reply(&mut sock, "235 ok\r\n");
                }
            } else if upper.starts_with("MAIL FROM:") {
                captured.from = angles(&line);
                write_reply(&mut sock, "250 ok\r\n");
            } else if upper.starts_with("RCPT TO:") {
                captured.rcpt.push(angles(&line));
                match after {
                    AfterData::RejectRcpt => write_reply(&mut sock, "550 no such user\r\n"),
                    AfterData::HangupAtRcpt => break,
                    _ => write_reply(&mut sock, "250 ok\r\n"),
                }
            } else if upper == "DATA" {
                write_reply(&mut sock, "354 go\r\n");
                captured.data = read_data(&mut sock);
                match after {
                    AfterData::Hangup => break,
                    AfterData::Reject => write_reply(&mut sock, "554 refused\r\n"),
                    _ => write_reply(&mut sock, "250 queued as test-id\r\n"),
                }
            } else if upper.starts_with("QUIT") {
                captured.quit = true;
                write_reply(&mut sock, "221 bye\r\n");
                break;
            } else {
                write_reply(&mut sock, "500 unknown\r\n");
            }
        }
        let _ = tx.send(captured);
    }

    fn server_tls(sock: std::net::TcpStream) -> impl Read + Write {
        use std::sync::Arc;

        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        use rustls::{ServerConfig, ServerConnection, StreamOwned};

        let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let certs = vec![CertificateDer::from(certified.cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(certified.key_pair.serialize_der().into());
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .unwrap();
        StreamOwned::new(ServerConnection::new(Arc::new(config)).unwrap(), sock)
    }

    fn write_reply(sock: &mut impl Write, text: &str) {
        sock.write_all(text.as_bytes()).unwrap();
        sock.flush().unwrap();
    }

    fn read_cmd(sock: &mut impl Read, record: &mut Vec<u8>) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let count = match sock.read(&mut byte) {
                Ok(count) => count,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => 0,
            };
            if count == 0 {
                break;
            }
            buf.push(byte[0]);
            record.push(byte[0]);
            if byte[0] == b'\n' || buf.len() > 8192 {
                break;
            }
        }
        String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_string()
    }

    fn read_data(sock: &mut impl Read) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let mut line = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                let count = sock.read(&mut byte).unwrap();
                assert_ne!(count, 0, "smtp data closed early");
                line.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            if line == b".\r\n" || line == b".\n" {
                break;
            }
            if line.first() == Some(&b'.') {
                out.extend_from_slice(&line[1..]);
            } else {
                out.extend_from_slice(&line);
            }
        }
        out
    }

    fn angles(line: &str) -> String {
        let start = line.find('<').unwrap_or(0);
        let end = line[start + 1..].find('>').unwrap_or(0);
        line[start + 1..start + 1 + end].to_ascii_lowercase()
    }
}
