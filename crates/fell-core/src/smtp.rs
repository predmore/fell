//! SMTP submission for Proton Mail Bridge.
//!
//! [`deliver`] is called from [`crate::backend::BridgeBackend::confirm_send`]
//! after the editor's send confirmation matches. The bytes on the wire are
//! [`crate::PendingSend::mime`]. Bcc addresses are envelope recipients only.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::imap::{TlsStream, host_is_loopback, wrap_tcp_as_tls};
use crate::send::{Draft, PendingSend};
use crate::{Error, Result};

const TIMEOUT: Duration = Duration::from_secs(15);

pub(crate) struct Endpoint {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub accept_invalid_certs: bool,
}

pub(crate) fn deliver(endpoint: &Endpoint, pending: &PendingSend) -> Result<String> {
    if endpoint.username.is_empty() {
        return Err(smtp_err("SMTP username is empty"));
    }
    if pending.mime.is_empty() {
        return Err(smtp_err("smtp: message is empty"));
    }
    let from = sender(&pending.mime)?;
    let recipients = envelope(&pending.draft)?;
    let tcp = connect(&endpoint.host, endpoint.port)?;
    let mut pipe = Pipe::plain(tcp);
    expect(read_reply(&mut pipe)?, 220, "greeting")?;
    let mut hello = expect(command(&mut pipe, "EHLO localhost")?, 250, "EHLO")?;
    if offers(&hello, "STARTTLS") {
        expect(command(&mut pipe, "STARTTLS")?, 220, "STARTTLS")?;
        pipe.upgrade(&endpoint.host, endpoint.accept_invalid_certs)?;
        hello = expect(command(&mut pipe, "EHLO localhost")?, 250, "EHLO")?;
    } else if !host_is_loopback(&endpoint.host) {
        return Err(smtp_err("smtp: TLS is required off loopback"));
    }
    authenticate(&mut pipe, &hello, endpoint)?;
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
    write_message(&mut pipe, &pending.mime)?;
    let accepted = expect(read_reply(&mut pipe)?, 250, "message")?;
    let _ = command(&mut pipe, "QUIT");
    Ok(reply_text(&accepted))
}

fn sender(mime: &[u8]) -> Result<String> {
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

fn write_message(pipe: &mut Pipe, mime: &[u8]) -> Result<()> {
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
    stuffed.extend_from_slice(b".\r\n");
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
    expect(
        command(pipe, &format!("AUTH PLAIN {}", b64(&token)))?,
        235,
        "AUTH",
    )?;
    Ok(())
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
mod tests {
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

    fn backend(port: u16) -> BridgeBackend {
        BridgeBackend {
            account_id: AccountId("mailbox".into()),
            imap: ImapConfig {
                host: "127.0.0.1".into(),
                port: 1143,
                username: "bridge-user".into(),
                password: "bridge-secret".into(),
                security: crate::imap::ImapSecurity::Plain,
                accept_invalid_certs: true,
            },
            smtp_host: "127.0.0.1".into(),
            smtp_port: port,
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
            let err = deliver(
                &Endpoint {
                    host: "127.0.0.1".into(),
                    port: 1,
                    username: "bridge-user".into(),
                    password: "bridge-secret".into(),
                    accept_invalid_certs: false,
                },
                &pending,
            )
            .unwrap_err();
            let text = err.to_string();
            assert!(text.contains("Bcc"), "{text}");
            assert!(!text.contains("connect"), "{text}");
        }
    }

    #[test]
    fn empty_username_does_not_open_smtp() {
        let pending = sample_pending();
        let err = BridgeBackend {
            smtp_port: 1,
            ..BridgeBackend::default()
        }
        .confirm_send(pending.clone(), confirm(&pending))
        .unwrap_err();
        let text = err.to_string();
        assert!(text.contains("SMTP"), "{text}");
        assert!(!text.contains("connect"), "{text}");
    }

    #[test]
    fn bridge_delivers_after_confirmation_with_bcc_on_the_envelope() {
        let (sent, got) = exchange(false);
        assert_eq!(sent.remote_id.0, "queued as test-id");
        assert_delivery(&got);
        assert!(got.clear.is_empty());
    }

    #[test]
    fn starttls_hides_the_password_and_still_delivers() {
        let (sent, got) = exchange(true);
        assert_eq!(sent.remote_id.0, "queued as test-id");
        assert_delivery(&got);
        let clear = String::from_utf8_lossy(&got.clear);
        assert!(!clear.to_ascii_uppercase().contains("AUTH"), "{clear}");
        assert!(!clear.contains("bridge-secret"), "{clear}");
    }

    fn assert_delivery(got: &Captured) {
        let mut token = vec![0];
        token.extend_from_slice(b"bridge-user");
        token.push(0);
        token.extend_from_slice(b"bridge-secret");
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

    fn exchange(starttls: bool) -> (crate::SentMessage, Captured) {
        let (port, rx, join) = serve(starttls);
        let backend = backend(port);
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

    struct Captured {
        auth: String,
        from: String,
        rcpt: Vec<String>,
        data: Vec<u8>,
        clear: Vec<u8>,
    }

    fn serve(starttls: bool) -> (u16, mpsc::Receiver<Captured>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let join = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            sock.set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let _ = sock.set_nodelay(true);
            write_reply(&mut sock, "220 localhost ESMTP\r\n");
            if starttls {
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
                dialog(server_tls(sock), tx, clear);
            } else {
                dialog(sock, tx, Vec::new());
            }
        });
        (port, rx, join)
    }

    fn dialog(mut sock: impl Read + Write, tx: mpsc::Sender<Captured>, clear: Vec<u8>) {
        let mut captured = Captured {
            auth: String::new(),
            from: String::new(),
            rcpt: Vec::new(),
            data: Vec::new(),
            clear,
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
                write_reply(&mut sock, "235 ok\r\n");
            } else if upper.starts_with("MAIL FROM:") {
                captured.from = angles(&line);
                write_reply(&mut sock, "250 ok\r\n");
            } else if upper.starts_with("RCPT TO:") {
                captured.rcpt.push(angles(&line));
                write_reply(&mut sock, "250 ok\r\n");
            } else if upper == "DATA" {
                write_reply(&mut sock, "354 go\r\n");
                captured.data = read_data(&mut sock);
                write_reply(&mut sock, "250 queued as test-id\r\n");
            } else if upper.starts_with("QUIT") {
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
