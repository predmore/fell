//! Blocking IMAP session. Read-only commands only: CAPABILITY, STARTTLS,
//! LOGIN, LIST, EXAMINE, UID FETCH, ENABLE, LOGOUT.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme,
    StreamOwned,
};

use crate::Error;

use super::ImapConfig;
use super::ImapSecurity;
use super::utf7::decode_modified_utf7;

const TIMEOUT: Duration = Duration::from_secs(15);

pub(super) struct Listed {
    pub raw: String,
    pub decoded: String,
}

pub(super) struct Examined {
    pub uidvalidity: u32,
    /// Next UID the server will assign. `None` when EXAMINE omitted `UIDNEXT`.
    pub uidnext: Option<u32>,
    pub exists: u32,
    pub highest_modseq: Option<u64>,
}

pub(super) struct Fetched {
    pub uid: u32,
    pub flags: Vec<String>,
    pub body: Vec<u8>,
}

pub(super) struct Session {
    io: Option<Io>,
    tag: u32,
    caps: Vec<String>,
    host: String,
    pending_buf: Vec<u8>,
}

enum Io {
    Plain(TcpStream),
    Tls(Box<StreamOwned<ClientConnection, TcpStream>>),
}

impl Session {
    pub(super) fn open(config: &ImapConfig) -> Result<Self, Error> {
        let stream = connect_tcp(config)?;
        let mut session = match config.security {
            ImapSecurity::Tls => Session {
                io: Some(Io::Tls(Box::new(wrap_tls(stream, config)?))),
                tag: 0,
                caps: Vec::new(),
                host: config.host.clone(),
                pending_buf: Vec::new(),
            },
            ImapSecurity::Plain | ImapSecurity::StartTls => Session {
                io: Some(Io::Plain(stream)),
                tag: 0,
                caps: Vec::new(),
                host: config.host.clone(),
                pending_buf: Vec::new(),
            },
        };
        session.read_greeting()?;
        if config.security == ImapSecurity::StartTls {
            session.command("STARTTLS", "STARTTLS")?;
            session.upgrade(config)?;
        }
        session.read_capabilities()?;
        if session.has_cap("CONDSTORE") {
            // UID high-water still works when ENABLE is refused.
            let _ = session.command("ENABLE", "ENABLE CONDSTORE");
        }
        let user = quote_imap(&config.username).map_err(imap_err)?;
        let pass = quote_imap(&config.password).map_err(imap_err)?;
        session.command("LOGIN", &format!("LOGIN {user} {pass}"))?;
        Ok(session)
    }

    pub(super) fn list_selectable(&mut self) -> Result<Vec<Listed>, Error> {
        let lines = self.command("LIST", "LIST \"\" \"*\"")?;
        let mut out = Vec::new();
        for line in lines {
            let Some(listed) = parse_list(&line.text, &line.literals) else {
                continue;
            };
            if listed.noselect {
                continue;
            }
            out.push(Listed {
                raw: listed.raw,
                decoded: listed.decoded,
            });
        }
        Ok(out)
    }

    pub(super) fn examine(&mut self, mailbox: &str) -> Result<Examined, Error> {
        let name = quote_imap(mailbox).map_err(imap_err)?;
        let lines = self.command("EXAMINE", &format!("EXAMINE {name}"))?;
        let mut uidvalidity = None;
        let mut uidnext = None;
        let mut exists = 0;
        let mut highest_modseq = None;
        for line in &lines {
            if let Some(count) = exists_count(&line.text) {
                exists = count;
            }
            if let Some(value) = bracket_u32(&line.text, "UIDVALIDITY") {
                uidvalidity = Some(value);
            }
            if let Some(value) = bracket_u32(&line.text, "UIDNEXT") {
                uidnext = Some(value);
            }
            if let Some(value) = bracket_u64(&line.text, "HIGHESTMODSEQ") {
                highest_modseq = Some(value);
            }
        }
        let uidvalidity = uidvalidity
            .ok_or_else(|| imap_err(format!("EXAMINE {mailbox} returned no UIDVALIDITY")))?;
        Ok(Examined {
            uidvalidity,
            uidnext,
            exists,
            highest_modseq,
        })
    }

    pub(super) fn uid_fetch(
        &mut self,
        range: &str,
        changed_since: Option<u64>,
    ) -> Result<Vec<Fetched>, Error> {
        let cmd = match changed_since {
            Some(modseq) => {
                format!("UID FETCH {range} (UID FLAGS BODY.PEEK[]) (CHANGEDSINCE {modseq})")
            }
            None => format!("UID FETCH {range} (UID FLAGS BODY.PEEK[])"),
        };
        let lines = self.command("UID FETCH", &cmd)?;
        let mut out = Vec::new();
        for line in lines {
            if let Some(fetched) = parse_fetch(&line.text, &line.literals) {
                out.push(fetched);
            }
        }
        out.sort_by_key(|item| item.uid);
        Ok(out)
    }

    pub(super) fn logout(&mut self) {
        let _ = self.command("LOGOUT", "LOGOUT");
    }

    fn upgrade(&mut self, config: &ImapConfig) -> Result<(), Error> {
        let Some(Io::Plain(tcp)) = self.io.take() else {
            return Err(imap_err("STARTTLS requires a cleartext session"));
        };
        self.io = Some(Io::Tls(Box::new(wrap_tls(tcp, config)?)));
        Ok(())
    }

    fn read_greeting(&mut self) -> Result<(), Error> {
        let (text, _) = self.read_logical()?;
        let upper = text.to_ascii_uppercase();
        if upper.starts_with("* OK") || upper.starts_with("* PREAUTH") {
            Ok(())
        } else {
            Err(imap_err(format!("greeting rejected: {text}")))
        }
    }

    fn read_capabilities(&mut self) -> Result<(), Error> {
        let lines = self.command("CAPABILITY", "CAPABILITY")?;
        self.caps = caps_from(&lines);
        Ok(())
    }

    fn has_cap(&self, name: &str) -> bool {
        self.caps.iter().any(|cap| cap.eq_ignore_ascii_case(name))
    }

    fn command(&mut self, label: &str, cmd: &str) -> Result<Vec<Logical>, Error> {
        self.tag += 1;
        let tag = format!("a{}", self.tag);
        self.write_all(format!("{tag} {cmd}\r\n").as_bytes())?;
        let mut untagged = Vec::new();
        loop {
            let (text, literals) = self.read_logical()?;
            if text_starts_with_tag(&text, &tag) {
                let rest = text[tag.len()..].trim_start();
                let upper = rest.to_ascii_uppercase();
                if upper.starts_with("OK") {
                    return Ok(untagged);
                }
                return Err(imap_err(format!("{label} failed: {rest}")));
            }
            untagged.push(Logical { text, literals });
        }
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let io = self
            .io
            .as_mut()
            .ok_or_else(|| imap_err("connection closed"))?;
        let mut offset = 0;
        while offset < bytes.len() {
            match io.write(&bytes[offset..]) {
                Ok(0) => {
                    return Err(imap_err(format!(
                        "connection to {} closed while writing",
                        self.host
                    )));
                }
                Ok(count) => offset += count,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(imap_err(format!("write to {} failed: {err}", self.host))),
            }
        }
        io.flush()
            .map_err(|err| imap_err(format!("flush to {} failed: {err}", self.host)))?;
        Ok(())
    }

    fn read_logical(&mut self) -> Result<(String, Vec<Vec<u8>>), Error> {
        let mut text = Vec::new();
        let mut literals = Vec::new();
        loop {
            let line = self.read_until_crlf()?;
            if let Some(len) = literal_len(&line) {
                text.extend_from_slice(&line);
                literals.push(self.read_exact(len)?);
                continue;
            }
            text.extend_from_slice(&line);
            break;
        }
        let text = String::from_utf8_lossy(&text).into_owned();
        Ok((text, literals))
    }

    fn read_until_crlf(&mut self) -> Result<Vec<u8>, Error> {
        let mut buf = self.take_buf();
        loop {
            if let Some(pos) = buf.iter().position(|byte| *byte == b'\n') {
                let mut line = buf.drain(..=pos).collect::<Vec<_>>();
                self.restore_buf(buf);
                if line.last() == Some(&b'\n') {
                    line.pop();
                }
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(line);
            }
            if !self.fill(&mut buf)? {
                return Err(imap_err(format!("connection to {} closed", self.host)));
            }
        }
    }

    fn read_exact(&mut self, len: usize) -> Result<Vec<u8>, Error> {
        let mut buf = self.take_buf();
        while buf.len() < len {
            if !self.fill(&mut buf)? {
                return Err(imap_err(format!(
                    "connection to {} closed during a literal",
                    self.host
                )));
            }
        }
        let exact = buf.drain(..len).collect();
        self.restore_buf(buf);
        Ok(exact)
    }

    fn take_buf(&mut self) -> Vec<u8> {
        std::mem::take(self.pending())
    }

    fn restore_buf(&mut self, buf: Vec<u8>) {
        *self.pending() = buf;
    }

    fn pending(&mut self) -> &mut Vec<u8> {
        // Leftover bytes live on the session, not the stream.
        &mut self.pending_buf
    }

    fn fill(&mut self, buf: &mut Vec<u8>) -> Result<bool, Error> {
        let mut tmp = [0u8; 4096];
        let io = self
            .io
            .as_mut()
            .ok_or_else(|| imap_err("connection closed"))?;
        match io.read(&mut tmp) {
            Ok(0) => Ok(false),
            Ok(count) => {
                buf.extend_from_slice(&tmp[..count]);
                Ok(true)
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => Ok(true),
            Err(err) => Err(imap_err(format!("read from {} failed: {err}", self.host))),
        }
    }
}

struct Logical {
    text: String,
    literals: Vec<Vec<u8>>,
}

struct ParsedList {
    raw: String,
    decoded: String,
    noselect: bool,
}

fn connect_tcp(config: &ImapConfig) -> Result<TcpStream, Error> {
    let addrs = (config.host.as_str(), config.port)
        .to_socket_addrs()
        .map_err(|err| {
            imap_err(format!(
                "resolve {}:{} failed: {err}",
                config.host, config.port
            ))
        })?;
    connect_addrs(addrs, &config.host, config.port)
}

/// Use the first address that accepts a connection. An earlier unreachable
/// address must not hide a later working one.
fn connect_addrs(
    addrs: impl IntoIterator<Item = std::net::SocketAddr>,
    host: &str,
    port: u16,
) -> Result<TcpStream, Error> {
    let mut last_err = None;
    let mut tried = false;
    for addr in addrs {
        tried = true;
        match TcpStream::connect_timeout(&addr, TIMEOUT) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(TIMEOUT))
                    .map_err(|err| imap_err(err.to_string()))?;
                stream
                    .set_write_timeout(Some(TIMEOUT))
                    .map_err(|err| imap_err(err.to_string()))?;
                let _ = stream.set_nodelay(true);
                return Ok(stream);
            }
            Err(err) => last_err = Some(err),
        }
    }
    if !tried {
        return Err(imap_err(format!(
            "resolve {host}:{port} returned no address"
        )));
    }
    let err = last_err.expect("a failed connect records an error");
    Err(imap_err(format!("connect {host}:{port} failed: {err}")))
}

fn wrap_tls(
    tcp: TcpStream,
    config: &ImapConfig,
) -> Result<StreamOwned<ClientConnection, TcpStream>, Error> {
    let tls = tls_config(config)?;
    let name = ServerName::try_from(config.host.as_str())
        .map_err(|_| {
            imap_err(format!(
                "host {} is not a valid TLS server name",
                config.host
            ))
        })?
        .to_owned();
    let conn = ClientConnection::new(tls, name)
        .map_err(|err| imap_err(format!("tls handshake setup failed: {err}")))?;
    Ok(StreamOwned::new(conn, tcp))
}

fn tls_config(config: &ImapConfig) -> Result<Arc<ClientConfig>, Error> {
    if config.accept_invalid_certs {
        let provider = scratch_provider();
        let verifier = Arc::new(AcceptInvalidCerts { provider });
        return Ok(Arc::new(
            ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(verifier)
                .with_no_client_auth(),
        ));
    }
    let mut roots = RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        return Err(imap_err(
            "no TLS trust anchors in the system certificate store",
        ));
    }
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

fn scratch_provider() -> Arc<rustls::crypto::CryptoProvider> {
    let scratch = ClientConfig::builder()
        .with_root_certificates(RootCertStore::empty())
        .with_no_client_auth();
    Arc::clone(scratch.crypto_provider())
}

#[derive(Debug)]
struct AcceptInvalidCerts {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for AcceptInvalidCerts {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn quote_imap(value: &str) -> Result<String, String> {
    if value
        .chars()
        .any(|ch| ch == '\r' || ch == '\n' || ch == '\0')
    {
        return Err("value contains a line break".into());
    }
    let mut out = String::from("\"");
    for ch in value.chars() {
        if ch == '\\' || ch == '"' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    Ok(out)
}

fn literal_len(line: &[u8]) -> Option<usize> {
    if !line.ends_with(b"}") {
        return None;
    }
    let start = line.iter().rposition(|byte| *byte == b'{')?;
    let mut number = &line[start + 1..line.len() - 1];
    if let Some(stripped) = number.strip_suffix(b"+") {
        number = stripped;
    }
    if number.is_empty() || !number.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(number).ok()?.parse().ok()
}

fn text_starts_with_tag(text: &str, tag: &str) -> bool {
    let rest = text.trim_start();
    rest.starts_with(tag)
        && rest
            .as_bytes()
            .get(tag.len())
            .is_some_and(|byte| byte.is_ascii_whitespace())
}

fn caps_from(lines: &[Logical]) -> Vec<String> {
    for line in lines {
        let upper = line.text.to_ascii_uppercase();
        let Some(index) = upper.find("CAPABILITY") else {
            continue;
        };
        return line.text[index + "CAPABILITY".len()..]
            .split_whitespace()
            .map(str::to_string)
            .collect();
    }
    Vec::new()
}

fn exists_count(text: &str) -> Option<u32> {
    let rest = text.trim().strip_prefix('*')?.trim_start();
    let (number, rest) = rest.split_once(' ')?;
    if rest.to_ascii_uppercase().starts_with("EXISTS") {
        number.parse().ok()
    } else {
        None
    }
}

fn bracket_u32(text: &str, key: &str) -> Option<u32> {
    bracket_number(text, key)?.parse().ok()
}

fn bracket_u64(text: &str, key: &str) -> Option<u64> {
    bracket_number(text, key)?.parse().ok()
}

fn bracket_number<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let upper = text.to_ascii_uppercase();
    let key_upper = key.to_ascii_uppercase();
    let index = upper.find(&key_upper)?;
    let after = text[index + key.len()..].trim_start();
    let end = after
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(after.len());
    let number = &after[..end];
    if number.is_empty() {
        None
    } else {
        Some(number)
    }
}

fn parse_list(text: &str, literals: &[Vec<u8>]) -> Option<ParsedList> {
    let rest = text.trim().strip_prefix('*')?.trim_start();
    let rest = strip_prefix_ignore_ascii_case(rest, "LIST")?.trim_start();
    if !rest.starts_with('(') {
        return None;
    }
    let end = rest.find(')')?;
    let flags: Vec<String> = rest[1..end]
        .split_whitespace()
        .map(str::to_string)
        .collect();
    let mut scan = Scan {
        text: rest,
        index: end + 1,
        literals,
        literal: 0,
    };
    let _delimiter = scan.astring()?;
    let raw = scan.astring()?;
    let noselect = flags
        .iter()
        .any(|flag| flag.eq_ignore_ascii_case("\\Noselect"));
    let decoded = decode_modified_utf7(&raw);
    Some(ParsedList {
        raw,
        decoded,
        noselect,
    })
}

fn parse_fetch(text: &str, literals: &[Vec<u8>]) -> Option<Fetched> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('*') {
        return None;
    }
    let upper = text.to_ascii_uppercase();
    if !upper.contains(" FETCH ") && !upper.contains(" FETCH(") {
        return None;
    }
    let uid = number_after(&upper, "UID ")?;
    let flags = paren_after(text, "FLAGS").unwrap_or_default();
    let body = literals.first().cloned().unwrap_or_default();
    Some(Fetched { uid, flags, body })
}

fn number_after(upper: &str, key: &str) -> Option<u32> {
    let index = upper.find(key)?;
    let after = upper[index + key.len()..].trim_start();
    let end = after
        .find(|ch: char| !ch.is_ascii_digit())
        .unwrap_or(after.len());
    after[..end].parse().ok()
}

fn paren_after(text: &str, key: &str) -> Option<Vec<String>> {
    let upper = text.to_ascii_uppercase();
    let key_upper = key.to_ascii_uppercase();
    let index = upper.find(&key_upper)?;
    let after = &text[index + key.len()..];
    let start = after.find('(')?;
    let end = after[start + 1..].find(')')?;
    let inner = &after[start + 1..start + 1 + end];
    Some(inner.split_whitespace().map(str::to_string).collect())
}

fn strip_prefix_ignore_ascii_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

struct Scan<'a> {
    text: &'a str,
    index: usize,
    literals: &'a [Vec<u8>],
    literal: usize,
}

impl Scan<'_> {
    fn skip_ws(&mut self) {
        while self.text[self.index..].starts_with(' ') {
            self.index += 1;
        }
    }

    fn astring(&mut self) -> Option<String> {
        self.skip_ws();
        let rest = &self.text[self.index..];
        if rest.starts_with('{') {
            let close = rest.find('}')?;
            self.index += close + 1;
            let bytes = self.literals.get(self.literal)?;
            self.literal += 1;
            return Some(String::from_utf8_lossy(bytes).into_owned());
        }
        if rest.starts_with('"') {
            return self.quoted();
        }
        let start = self.index;
        while self.index < self.text.len() {
            let ch = self.text[self.index..].chars().next()?;
            if ch == ' ' || ch == ')' {
                break;
            }
            self.index += ch.len_utf8();
        }
        if start == self.index {
            None
        } else {
            Some(self.text[start..self.index].to_string())
        }
    }

    fn quoted(&mut self) -> Option<String> {
        self.index += 1;
        let bytes = self.text.as_bytes();
        let mut out = Vec::new();
        while self.index < bytes.len() {
            let byte = bytes[self.index];
            self.index += 1;
            if byte == b'\\' {
                if self.index >= bytes.len() {
                    return None;
                }
                out.push(bytes[self.index]);
                self.index += 1;
                continue;
            }
            if byte == b'"' {
                return Some(String::from_utf8_lossy(&out).into_owned());
            }
            out.push(byte);
        }
        None
    }
}

fn imap_err(message: impl Into<String>) -> Error {
    Error::Backend(format!("imap: {}", message.into()))
}

impl Io {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.read(buf),
            Self::Tls(stream) => stream.read(buf),
        }
    }

    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Self::Plain(stream) => stream.write(buf),
            Self::Tls(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Self::Plain(stream) => stream.flush(),
            Self::Tls(stream) => stream.flush(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::{SocketAddr, TcpListener};

    use super::connect_addrs;

    #[test]
    fn connect_uses_a_later_address_when_the_first_refuses() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let good = listener.local_addr().unwrap();
        let closed = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead: SocketAddr = closed.local_addr().unwrap();
        drop(closed);
        let stream = connect_addrs([dead, good], "127.0.0.1", good.port()).unwrap();
        assert_eq!(stream.peer_addr().unwrap(), good);
    }
}
